//! Org bindings (ADR-0038 §2, step B; migration 20270101100000): "the
//! people of this unit (and the units below it) are Reporter / Developer
//! here", on a group or a project. `forge.org_bindings` holds them; the
//! role loaders read them through `forge.org_binding_role` inside
//! `roles::chain_role_columns!`, so the effective role is the higher of the
//! hand-made membership (direct or inherited) and the matching bindings —
//! never above Developer, and nothing at all when the person's snapshot or
//! the master's copy cannot be trusted (§4). `authorize()` decides with the
//! role as before.
//!
//! - **A person's commands** (audited, `ChangeTx`): [`create_decided`]
//!   (`org_binding.created`) and [`delete_decided`] (`org_binding.deleted`),
//!   on the group or project, decided again on the locked target by
//!   `decide` (forge-hosting: `OrgBindingManage`, the Owner).
//! - **Reads** (read pool): [`list`] (the bindings with their unit's names),
//!   [`members`] (the people a binding gives a role to now: the members
//!   screen's 「組織から」 rows, as [`MemberDisclosure`] allows; on the
//!   write pool), [`units`] (the master's units for the
//!   picker).
//! - [`publish_ages`]: `forge serve` / `forge worker` copy the configured
//!   `org.*_age_hours` into `forge.org_access_policy`, which the SQL
//!   function reads (the read pool has no configuration).
//!
//! [`check`] holds the field rules (FG-AUTH-0029, FG-AUTH-0031); the unit
//! must be in the master's copy (FG-AUTH-0030).

use forge_core::ids::{GroupId, NamespaceId, ProjectId, UserId};
use forge_core::Role;
use forge_errors::{ErrorCode, ForgeError};
use sqlx::postgres::PgConnection;

use crate::access::lock_account;
use crate::change::{AuditEntry, ChangeContext, ChangeRecord, ChangeTx, Outcome, Settle};
use crate::groups::{GroupAccess, GroupUpdateFacts};
use crate::org_master::{LocalizedName, UnitLevel};
use crate::projects::{lock_shared, locked_access_facts, ProjectUpdateFacts};
use crate::roles::{lock_roles, RoleAnchor};
use crate::{query_error, read_query_error, Db};

/// The audit action of an added binding.
pub const CREATED_ACTION: &str = "org_binding.created";
/// The audit action of a removed binding.
pub const DELETED_ACTION: &str = "org_binding.deleted";
/// The most bindings one group or project holds.
pub const MAX_BINDINGS: i64 = 100;
/// The most people [`members`] answers (the members screen's list).
pub const MAX_MEMBERS: usize = 200;
/// The most units [`units`] answers.
pub const MAX_UNITS: usize = 50;
/// The longest reason (characters).
pub const MAX_REASON_CHARS: usize = 500;
/// The highest role a binding gives (ADR-0038 §2.1).
pub const ROLE_CAP: Role = Role::Developer;

/// Where a binding applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BindingTarget {
    Group(GroupId),
    Project(ProjectId),
}

impl BindingTarget {
    /// `group` / `project` (the table's `target_kind`, the audit resource).
    pub fn kind(self) -> &'static str {
        match self {
            Self::Group(_) => "group",
            Self::Project(_) => "project",
        }
    }

    pub fn id(self) -> i64 {
        match self {
            Self::Group(g) => g.get(),
            Self::Project(p) => p.get(),
        }
    }

    fn project(self) -> Option<ProjectId> {
        match self {
            Self::Group(_) => None,
            Self::Project(p) => Some(p),
        }
    }
}

/// Which of a person's units a binding looks at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Membership {
    /// The primary unit only.
    Primary,
    /// The primary unit and the concurrent ones.
    Any,
}

impl Membership {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::Any => "any",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "primary" => Some(Self::Primary),
            "any" => Some(Self::Any),
            _ => None,
        }
    }
}

/// A binding as asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewBinding {
    pub plant_code: String,
    pub org_code: String,
    pub include_descendants: bool,
    pub membership: Membership,
    pub min_rank: Option<i32>,
    pub role: Role,
    pub reason: Option<String>,
}

/// A unit's level and names (from the master's copy).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitName {
    pub level: UnitLevel,
    pub name: LocalizedName,
}

/// A stored binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredBinding {
    pub id: i64,
    pub target: BindingTarget,
    pub plant_code: String,
    pub org_code: String,
    pub include_descendants: bool,
    pub membership: Membership,
    pub min_rank: Option<i32>,
    pub role: Role,
    pub reason: Option<String>,
    pub created_by: UserId,
    pub created_by_username: String,
    /// UTC, RFC 3339.
    pub created_at: String,
    /// The unit in the master's copy (`None`: no longer there — the
    /// binding then gives nothing).
    pub unit: Option<UnitName>,
}

/// A person a binding gives a role to now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundMember {
    pub user_id: UserId,
    pub username: String,
    pub display_name: String,
    /// The highest role the bindings of the target give the person.
    pub role: Role,
    /// The binding that gives it (the first by id among the highest).
    pub binding_id: i64,
    pub org_code: String,
    pub unit: Option<UnitName>,
}

/// One unit of the master's copy, for the picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickUnit {
    pub plant_code: String,
    pub code: String,
    pub level: UnitLevel,
    pub parent_code: Option<String>,
    pub name: LocalizedName,
}

/// What `decide` sees: the locked target, the caller's roles and account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetFacts {
    Group(GroupUpdateFacts),
    Project(ProjectUpdateFacts),
}

// ── the field rules ──

fn invalid(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::OrgBindingInvalid).with_context("reason", reason)
}

fn is_plant(s: &str) -> bool {
    (1..=4).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
}

fn is_org_code(s: &str) -> bool {
    let bytes = s.as_bytes();
    let Some((first, rest)) = bytes.split_first() else {
        return false;
    };
    (first.is_ascii_uppercase() || first.is_ascii_digit())
        && rest.len() <= 19
        && rest
            .iter()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || *b == b'_' || *b == b'-')
}

/// The field rules of a binding (the table's CHECKs, checked before
/// anything is read): `FG-AUTH-0031 role_above_cap` for Maintainer or
/// Owner, `FG-AUTH-0029` with `reason` otherwise.
pub fn check(new: &NewBinding) -> Result<(), ForgeError> {
    if new.role > ROLE_CAP {
        return Err(ForgeError::new(ErrorCode::OrgBindingRoleAboveCap)
            .with_context("reason", "role_above_cap")
            .with_context("role", new.role.as_str())
            .with_context("cap", ROLE_CAP.as_str()));
    }
    if new.role < Role::Reporter {
        return Err(invalid("role_not_allowed").with_context("role", new.role.as_str()));
    }
    if !is_plant(&new.plant_code) {
        return Err(invalid("plant_code_invalid"));
    }
    if !is_org_code(&new.org_code) {
        return Err(invalid("org_code_invalid"));
    }
    if new.min_rank.is_some_and(|r| !(0..=999).contains(&r)) {
        return Err(invalid("min_rank_out_of_range"));
    }
    if let Some(reason) = &new.reason {
        let chars = reason.chars().count();
        if reason.trim().is_empty() || chars > MAX_REASON_CHARS {
            return Err(invalid("reason_invalid").with_context("chars", chars));
        }
    }
    Ok(())
}

// ── reads ──

type BindingRow = (
    i64,
    String,
    String,
    bool,
    String,
    Option<i32>,
    String,
    Option<String>,
    i64,
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

const LIST_SQL: &str = "SELECT b.id, b.plant_code, b.org_code, b.include_descendants, \
     b.membership, b.min_rank, b.role, b.reason, b.created_by, u.username, \
     to_char(b.created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'), \
     m.level, m.name_ja, m.name_en, m.name_id \
     FROM forge.org_bindings b JOIN forge.users u ON u.id = b.created_by \
     LEFT JOIN forge.org_units_master m ON m.plant_code = b.plant_code AND m.code = b.org_code \
     WHERE b.target_kind = $1 AND b.target_id = $2 ORDER BY b.id";

fn unit_name(
    level: Option<String>,
    ja: Option<String>,
    en: Option<String>,
    id: Option<String>,
) -> Option<UnitName> {
    Some(UnitName {
        level: UnitLevel::parse(&level?)?,
        name: LocalizedName { ja: ja?, en, id },
    })
}

/// `FG-DB-0018`-like refusal of a stored value the CHECKs should have
/// refused (bounded).
fn stored_invalid(field: &'static str, raw: &str) -> ForgeError {
    ForgeError::new(ErrorCode::Internal)
        .with_context("reason", "stored_org_binding_invalid")
        .with_context("field", field)
        .with_context("raw", raw.chars().take(32).collect::<String>())
}

fn binding(target: BindingTarget, row: BindingRow) -> Result<StoredBinding, ForgeError> {
    let (
        id,
        plant,
        code,
        desc,
        membership,
        min_rank,
        role,
        reason,
        by,
        by_name,
        at,
        lv,
        ja,
        en,
        nid,
    ) = row;
    Ok(StoredBinding {
        id,
        target,
        plant_code: plant,
        org_code: code,
        include_descendants: desc,
        membership: Membership::parse(&membership)
            .ok_or_else(|| stored_invalid("membership", &membership))?,
        min_rank,
        role: Role::parse(&role).ok_or_else(|| stored_invalid("role", &role))?,
        reason,
        created_by: UserId::new(by),
        created_by_username: by_name,
        created_at: at,
        unit: unit_name(lv, ja, en, nid),
    })
}

/// The bindings of `target`, oldest first (at most [`MAX_BINDINGS`]).
pub async fn list(db: &Db, target: BindingTarget) -> Result<Vec<StoredBinding>, ForgeError> {
    const QUERY: &str = "org_bindings.list";
    let rows: Vec<BindingRow> = db
        .within(QUERY, async {
            sqlx::query_as(LIST_SQL)
                .bind(target.kind())
                .bind(target.id())
                .fetch_all(db.pool().executor())
                .await
                .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    rows.into_iter().map(|r| binding(target, r)).collect()
}

type MemberSql = (
    i64,
    String,
    String,
    String,
    i64,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

/// How much of the people a binding reaches a reader may see (ADR-0038
/// §6: rank and concurrent units are the person's and the
/// administrators').
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberDisclosure {
    /// Administrators: every match, the highest role.
    Full,
    /// Everyone else (the Owner included): only the matches through the
    /// person's primary unit's chain of bindings without `min_rank`, and
    /// the highest role among those. Varying `min_rank` or binding a
    /// concurrent unit then tells the reader nothing; the primary unit is
    /// already shown to every signed-in user (§6, step A).
    PrimaryUnit,
}

impl MemberDisclosure {
    fn full(self) -> bool {
        matches!(self, Self::Full)
    }
}

/// The people the bindings of `target` give a role to now, by user id (at
/// most [`MAX_MEMBERS`]; `true` = there are more), as `disclosure` allows.
/// The same function as the role loaders (`forge.org_binding_matches`), so
/// the list and the roles agree; the people without a valid, recent
/// snapshot are not listed. Bindings of the groups above are not listed
/// here (as inherited members are not).
///
/// The people are narrowed first to the snapshots holding a bound unit or
/// one below it (the GIN probe on `org_units`; the primary unit only for
/// [`MemberDisclosure::PrimaryUnit`]), so the matching runs for them only.
/// It runs on the write pool: the read pool has no EXECUTE on the matching
/// function (migration 20270101100000).
pub async fn members(
    db: &Db,
    target: BindingTarget,
    disclosure: MemberDisclosure,
) -> Result<(Vec<BoundMember>, bool), ForgeError> {
    const QUERY: &str = "org_bindings.members";
    let limit = i64::try_from(MAX_MEMBERS + 1).unwrap_or(i64::MAX);
    let rows: Vec<MemberSql> = db
        .within(QUERY, async {
            sqlx::query_as(
                "WITH RECURSIVE bound AS ( \
                   SELECT DISTINCT b.plant_code, b.org_code, b.include_descendants \
                   FROM forge.org_bindings b \
                   WHERE b.target_kind = $1 AND b.target_id = $2 \
                     AND ($4 OR b.min_rank IS NULL)), \
                 reach (plant_code, code, depth, descend) AS ( \
                   SELECT plant_code, org_code, 0, include_descendants FROM bound \
                   UNION \
                   SELECT m.plant_code, m.code, r.depth + 1, true FROM reach r \
                   JOIN forge.org_units_master m \
                     ON m.plant_code = r.plant_code AND m.parent_code = r.code \
                   WHERE r.descend AND r.depth < 8), \
                 codes AS (SELECT DISTINCT code FROM reach), \
                 candidates AS ( \
                   SELECT DISTINCT p.user_id FROM codes c \
                   JOIN forge.user_org_profiles p ON p.org_units @> CASE WHEN $4 \
                     THEN jsonb_build_array(jsonb_build_object('code', c.code)) \
                     ELSE jsonb_build_array(jsonb_build_object('code', c.code, 'primary', true)) \
                     END \
                   WHERE p.invalid_since IS NULL) \
                 SELECT u.id, u.username, u.display_name, best.binding_role, b.id, b.org_code, \
                 m.level, m.name_ja, m.name_en, m.name_id \
                 FROM candidates cd JOIN forge.users u ON u.id = cd.user_id \
                 CROSS JOIN LATERAL (SELECT x.binding_id, x.binding_role \
                   FROM forge.org_binding_matches(u.id, $1, $2) x \
                   JOIN forge.org_bindings xb ON xb.id = x.binding_id \
                   WHERE $4 OR (x.via_primary AND xb.min_rank IS NULL) \
                   ORDER BY (x.binding_role = 'developer') DESC, x.binding_id LIMIT 1) best \
                 JOIN forge.org_bindings b ON b.id = best.binding_id \
                 LEFT JOIN forge.org_units_master m \
                   ON m.plant_code = b.plant_code AND m.code = b.org_code \
                 WHERE u.state = 'active' AND u.kind = 'human' \
                 ORDER BY u.id LIMIT $3",
            )
            .bind(target.kind())
            .bind(target.id())
            .bind(limit)
            .bind(disclosure.full())
            .fetch_all(db.write_pool())
            .await
            .map_err(|e| query_error(QUERY, e))
        })
        .await?;
    let truncated = rows.len() > MAX_MEMBERS;
    let mut out = Vec::with_capacity(rows.len().min(MAX_MEMBERS));
    for (id, username, display_name, role, binding_id, org_code, lv, ja, en, nid) in
        rows.into_iter().take(MAX_MEMBERS)
    {
        out.push(BoundMember {
            user_id: UserId::new(id),
            username,
            display_name,
            role: Role::parse(&role).ok_or_else(|| stored_invalid("role", &role))?,
            binding_id,
            org_code,
            unit: unit_name(lv, ja, en, nid),
        });
    }
    Ok((out, truncated))
}

/// The units of the master's copy for the picker (valid now): matching
/// `search` (a code prefix, or a part of a name in any language; ASCII case
/// folded) and `plant` when given, by plant and code, at most `limit`
/// (1..=[`MAX_UNITS`]).
pub async fn units(
    db: &Db,
    search: Option<&str>,
    plant: Option<&str>,
    limit: usize,
) -> Result<Vec<PickUnit>, ForgeError> {
    const QUERY: &str = "org_bindings.units";
    let limit = i64::try_from(limit.clamp(1, MAX_UNITS)).unwrap_or(1);
    let pattern = search.map(|s| {
        let escaped: String = s
            .to_lowercase()
            .chars()
            .flat_map(|c| match c {
                '%' | '_' | '\\' => vec!['\\', c],
                c => vec![c],
            })
            .collect();
        escaped
    });
    type Row = (
        String,
        String,
        String,
        Option<String>,
        String,
        Option<String>,
        Option<String>,
    );
    let rows: Vec<Row> = db
        .within(QUERY, async {
            sqlx::query_as(
                "SELECT plant_code, code, level, parent_code, name_ja, name_en, name_id \
                 FROM forge.org_units_master \
                 WHERE (valid_from IS NULL OR valid_from <= current_date) \
                   AND ($1::text IS NULL OR plant_code = $1) \
                   AND ($2::text IS NULL OR lower(code) LIKE $2 || '%' \
                     OR lower(name_ja) LIKE '%' || $2 || '%' \
                     OR lower(coalesce(name_en, '')) LIKE '%' || $2 || '%' \
                     OR lower(coalesce(name_id, '')) LIKE '%' || $2 || '%') \
                 ORDER BY plant_code, code LIMIT $3",
            )
            .bind(plant)
            .bind(pattern.as_deref())
            .bind(limit)
            .fetch_all(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    rows.into_iter()
        .map(|(plant_code, code, level, parent_code, ja, en, id)| {
            Ok(PickUnit {
                plant_code,
                code,
                level: UnitLevel::parse(&level).ok_or_else(|| stored_invalid("level", &level))?,
                parent_code,
                name: LocalizedName { ja, en, id },
            })
        })
        .collect()
}

/// Copy the configured maximum ages ([`Db::org_ages`]) into
/// `forge.org_access_policy` (written only when they differ). `forge
/// serve` and `forge worker` call it at their start.
pub async fn publish_ages(db: &Db) -> Result<(), ForgeError> {
    const QUERY: &str = "org_bindings.publish_ages";
    let ages = db.org_ages();
    let secs = |d: std::time::Duration| i32::try_from(d.as_secs().max(1)).unwrap_or(i32::MAX);
    let (profile, master) = (secs(ages.profile), secs(ages.master));
    db.within(QUERY, async {
        sqlx::query(
            "UPDATE forge.org_access_policy SET max_profile_age_secs = $1, \
             master_max_age_secs = $2, updated_at = now() \
             WHERE id AND (max_profile_age_secs, master_max_age_secs) IS DISTINCT FROM ($1, $2)",
        )
        .bind(profile)
        .bind(master)
        .execute(db.write_pool())
        .await
        .map(drop)
        .map_err(|e| query_error(QUERY, e))
    })
    .await
}

// ── the commands ──

async fn lock_group(conn: &mut PgConnection, group: GroupId) -> Result<NamespaceId, ForgeError> {
    let row: Option<i64> = sqlx::query_scalar(
        "SELECT n.id FROM forge.groups g \
         JOIN forge.namespaces n ON n.kind = 'group' AND n.owner_id = g.id \
         WHERE g.id = $1 FOR SHARE OF g",
    )
    .bind(group.get())
    .fetch_optional(conn)
    .await
    .map_err(|e| query_error("org_bindings.lock_group", e))?;
    row.map(NamespaceId::new).ok_or_else(|| not_found("group"))
}

fn not_found(resource: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::NotFound)
        .with_context("resource", resource)
        .with_context("reason", "absent")
}

/// Lock the target (`FOR SHARE`), then the caller's roles and account (the
/// lock order of the member commands) and return what `decide` sees.
async fn target_facts(
    tx: &mut ChangeTx,
    target: BindingTarget,
    caller: UserId,
) -> Result<TargetFacts, ForgeError> {
    match target {
        BindingTarget::Group(group) => {
            let namespace = tx.step(|c| lock_group(c, group)).await?;
            let roles = tx
                .step(|c| lock_roles(c, RoleAnchor::Group(group), caller))
                .await?;
            let account = tx.step(|c| lock_account(c, caller)).await?;
            Ok(TargetFacts::Group(GroupUpdateFacts {
                account,
                group: GroupAccess {
                    group,
                    namespace,
                    role: roles.native,
                },
            }))
        }
        BindingTarget::Project(project) => {
            let locked = lock_shared(tx, project).await?;
            Ok(TargetFacts::Project(
                locked_access_facts(tx, project, caller, &locked).await?,
            ))
        }
    }
}

/// Whether (plant, code) is in the master's copy and valid now:
/// `FG-AUTH-0030` otherwise.
async fn in_master(conn: &mut PgConnection, plant: &str, code: &str) -> Result<(), ForgeError> {
    let row: (bool, Option<bool>) = sqlx::query_as(
        "SELECT EXISTS (SELECT 1 FROM forge.org_master_state WHERE id), \
         (SELECT valid_from IS NULL OR valid_from <= current_date \
            FROM forge.org_units_master WHERE plant_code = $1 AND code = $2)",
    )
    .bind(plant)
    .bind(code)
    .fetch_one(conn)
    .await
    .map_err(|e| query_error("org_bindings.in_master", e))?;
    let reason = match row {
        (false, _) => "master_missing",
        (true, None) => "not_in_master",
        (true, Some(false)) => "not_yet_valid",
        (true, Some(true)) => return Ok(()),
    };
    Err(ForgeError::new(ErrorCode::OrgUnitNotInMaster)
        .with_context("reason", reason)
        .with_context("plant_code", plant.to_owned())
        .with_context("org_code", code.to_owned()))
}

fn audit(action: &'static str, b: &StoredBinding) -> ChangeRecord {
    ChangeRecord {
        audit: AuditEntry {
            action,
            outcome: Outcome::Succeeded,
            resource_type: b.target.kind(),
            resource_id: Some(b.target.id()),
            project_id: b.target.project(),
            error_code: None,
            // The reason is free text: its length only.
            details: serde_json::json!({
                "binding_id": b.id,
                "plant_code": b.plant_code,
                "org_code": b.org_code,
                "include_descendants": b.include_descendants,
                "membership": b.membership.as_str(),
                "min_rank": b.min_rank,
                "role": b.role.as_str(),
                "reason_chars": b.reason.as_ref().map(|r| r.chars().count()),
            }),
        },
        affected: Vec::new(),
        events: Vec::new(),
    }
}

async fn stored_on(
    conn: &mut PgConnection,
    target: BindingTarget,
    id: i64,
) -> Result<Option<StoredBinding>, ForgeError> {
    let sql = LIST_SQL.replace("ORDER BY b.id", "AND b.id = $3");
    let row: Option<BindingRow> = sqlx::query_as(&sql)
        .bind(target.kind())
        .bind(target.id())
        .bind(id)
        .fetch_optional(conn)
        .await
        .map_err(|e| query_error("org_bindings.read", e))?;
    row.map(|r| binding(target, r)).transpose()
}

/// Add `new` to `target` for `caller`: the field rules first ([`check`]),
/// then the target locked and `decide` (the caller's reconfirmed
/// `OrgBindingManage`), then the unit must be in the master's copy
/// (`FG-AUTH-0030`), the target below [`MAX_BINDINGS`] and the binding
/// new (`FG-AUTH-0029 too_many` / `duplicate`). One `org_binding.created`
/// audit row on the group or project.
pub async fn create_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    target: BindingTarget,
    caller: UserId,
    new: &NewBinding,
    decide: impl FnOnce(&TargetFacts) -> Result<P, ForgeError>,
) -> Result<(StoredBinding, P), ForgeError> {
    check(new)?;
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let facts = target_facts(&mut tx, target, caller).await?;
        let decided = decide(&facts)?;
        tx.step(|c| in_master(c, &new.plant_code, &new.org_code))
            .await?;
        // The target is locked FOR SHARE only (as the member commands lock
        // it), so two adds could both count 99: the count and the insert
        // are serialized per target by a transaction advisory lock.
        tx.step(|c| async move {
            sqlx::query(
                "SELECT pg_advisory_xact_lock(hashtextextended( \
                 'org_bindings:' || $1 || ':' || $2::text, 0))",
            )
            .bind(target.kind())
            .bind(target.id())
            .execute(c)
            .await
            .map(drop)
            .map_err(|e| query_error("org_bindings.cap_lock", e))
        })
        .await?;
        let count: i64 = tx
            .step(|c| async move {
                sqlx::query_scalar(
                    "SELECT count(*) FROM forge.org_bindings \
                     WHERE target_kind = $1 AND target_id = $2",
                )
                .bind(target.kind())
                .bind(target.id())
                .fetch_one(c)
                .await
                .map_err(|e| query_error("org_bindings.count", e))
            })
            .await?;
        if count >= MAX_BINDINGS {
            return Err(invalid("too_many").with_context("count", count));
        }
        let (group, project) = match target {
            BindingTarget::Group(g) => (Some(g.get()), None),
            BindingTarget::Project(p) => (None, Some(p.get())),
        };
        let id: Option<i64> = tx
            .step(|c| async move {
                sqlx::query_scalar(
                    "INSERT INTO forge.org_bindings (group_id, project_id, plant_code, org_code, \
                     include_descendants, membership, min_rank, role, reason, created_by) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) \
                     ON CONFLICT DO NOTHING RETURNING id",
                )
                .bind(group)
                .bind(project)
                .bind(&new.plant_code)
                .bind(&new.org_code)
                .bind(new.include_descendants)
                .bind(new.membership.as_str())
                .bind(new.min_rank)
                .bind(new.role.as_str())
                .bind(new.reason.as_deref())
                .bind(caller.get())
                .fetch_optional(c)
                .await
                .map_err(|e| query_error("org_bindings.insert", e))
            })
            .await?;
        let Some(id) = id else {
            return Err(invalid("duplicate"));
        };
        let stored = tx
            .step(|c| stored_on(c, target, id))
            .await?
            .ok_or_else(|| {
                ForgeError::new(ErrorCode::Internal)
                    .with_context("reason", "written_org_binding_missing")
                    .with_context("binding_id", id)
            })?;
        Ok(Settle::Commit(
            audit(CREATED_ACTION, &stored),
            (stored, decided),
        ))
    }
    .await;
    tx.settle(CREATED_ACTION, outcome).await
}

/// Remove binding `id` of `target` for `caller`, decided again on the
/// locked target (`decide`). `FG-REPO-0005 resource=org_binding` when the
/// target has no such binding. One `org_binding.deleted` audit row.
pub async fn delete_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    target: BindingTarget,
    id: i64,
    caller: UserId,
    decide: impl FnOnce(&TargetFacts) -> Result<P, ForgeError>,
) -> Result<(StoredBinding, P), ForgeError> {
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let facts = target_facts(&mut tx, target, caller).await?;
        let decided = decide(&facts)?;
        let stored = tx
            .step(|c| stored_on(c, target, id))
            .await?
            .ok_or_else(|| not_found("org_binding"))?;
        tx.step(|c| async move {
            sqlx::query(
                "DELETE FROM forge.org_bindings \
                 WHERE id = $1 AND target_kind = $2 AND target_id = $3",
            )
            .bind(id)
            .bind(target.kind())
            .bind(target.id())
            .execute(c)
            .await
            .map(drop)
            .map_err(|e| query_error("org_bindings.delete", e))
        })
        .await?;
        Ok(Settle::Commit(
            audit(DELETED_ACTION, &stored),
            (stored, decided),
        ))
    }
    .await;
    tx.settle(DELETED_ACTION, outcome).await
}

#[cfg(test)]
mod tests;
