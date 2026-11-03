//! Integrations and their project bindings (M3 I1a, plan 11 §5 I1, 02 §2,
//! ADR-0016 §1 / §7, ADR-0016 Amendment I1a).
//!
//! - [`create`] / [`update`]: an integration (`forge.integrations`) — its
//!   kind, the service account it acts as (fixed for its life), whether it
//!   is enabled and its event type allowlist; one `integration.created` /
//!   `integration.updated` audit row on (`integration`, id), no event.
//! - [`bind_decided`] / [`unbind_decided`]: bind a project to an external
//!   (kanflow) project of an integration, or unbind it, decided inside the
//!   transaction (forge-hosting: `Authorized::reconfirm`). Binding makes
//!   the service account a Reporter of the project when it is no member
//!   yet (`granted_membership`); unbinding removes that membership only if
//!   the binding granted it and it is still Reporter (a membership someone
//!   gave or changed by hand is kept). One `integration.bound` /
//!   `integration.unbound` audit row on the project, plus the membership's
//!   own `member.added` / `member.removed` audit row and event.
//! - [`get`], [`list`], [`bindings`], [`project_bindings`]: reads on the
//!   read pool.
//!
//! Lock order of a bind or unbind (that of `projects::member_writes`): the
//! project row `FOR UPDATE` (with its deletion), the caller's membership
//! rows and account row `FOR SHARE`, `decide`, then the integration row
//! `FOR UPDATE` (so the 200-binding count of [`MAX_BINDINGS`] holds
//! against concurrent binds of other projects), the project's binding, the
//! service account's membership row `FOR UPDATE` and its user row `FOR
//! SHARE`. An integration update locks only the integration row; neither
//! ever locks a project row after a user row.
//!
//! These commands do not decide who may call them beyond `decide`: the
//! CHECKs, the unique indexes, the guard `integrations_service_account_guard`
//! and the deferred audit triggers are the backstop.

use forge_core::ids::{IntegrationId, ProjectId, UserId};
use forge_core::integration::{
    default_event_types, event_type_entry_ok, external_project_id_ok, MAX_EVENT_TYPE_ENTRIES,
};
use forge_core::{AuditDetails, IntegrationChange, IntegrationField, IntegrationKind, Role};
use forge_errors::{ErrorCode, ForgeError};

use crate::access::Account;
use crate::change::{AuditEntry, ChangeContext, ChangeRecord, ChangeTx, Outcome, Settle};
use crate::projects::{
    direct_role_for_update, evented_record, lock_access, locked_access_facts, member_candidate,
    ProjectAccessFacts,
};
use crate::{query_error, read_query_error, Db};

#[cfg(test)]
mod tests;

/// The most projects one integration may be bound to (ADR-0016 §1: the
/// feed decides every bound project on every read). `FG-INT-0012` beyond.
pub const MAX_BINDINGS: i64 = 200;
/// The most integrations [`list`] returns (it says when it cut).
pub const MAX_LISTED: i64 = 500;

/// The audit resource of an integration's own changes (tables.toml).
const RESOURCE: &str = "integration";

/// The request to create an integration.
#[derive(Debug, Clone)]
pub struct NewIntegration {
    pub kind: IntegrationKind,
    /// The service account it acts as (one integration per account).
    pub service_account: UserId,
    /// The event type allowlist; `None`: every integration-class event
    /// (`["*"]`).
    pub event_types: Option<Vec<String>>,
}

/// The fields an update may change (`None`: unchanged).
#[derive(Debug, Clone, Default)]
pub struct Changes {
    pub enabled: Option<bool>,
    pub event_types: Option<Vec<String>>,
}

/// What [`update`] did.
#[must_use = "Unchanged wrote nothing; callers report it differently"]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateOutcome {
    /// One `integration.updated` row naming these fields.
    Updated(Vec<IntegrationField>),
    /// Every given value equals the stored one: nothing was written.
    Unchanged,
}

/// One integration as the administration sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Integration {
    pub id: IntegrationId,
    pub kind: IntegrationKind,
    pub service_account: UserId,
    pub service_account_username: String,
    pub enabled: bool,
    /// Sorted, without duplicates.
    pub event_types: Vec<String>,
    /// How many projects are bound to it.
    pub bindings: i64,
    pub created_by: Option<UserId>,
    /// RFC 3339, UTC.
    pub created_at: String,
    pub updated_at: String,
}

/// What [`list`] read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    pub items: Vec<Integration>,
    /// More than [`MAX_LISTED`] exist.
    pub truncated: bool,
}

/// One binding of a project to an external project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    pub integration: IntegrationId,
    pub kind: IntegrationKind,
    pub project: ProjectId,
    pub external_project_id: String,
    /// The binding made the service account a Reporter of the project.
    pub granted_membership: bool,
    pub created_by: Option<UserId>,
    /// RFC 3339, UTC.
    pub created_at: String,
}

/// What a bind did.
#[must_use = "Unchanged wrote nothing; callers report it differently"]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindOutcome {
    /// Written: one `integration.bound` row, and when `granted_membership`
    /// the service account's `member.added` row and event.
    Bound { granted_membership: bool },
    /// The project was bound to that external project already: nothing
    /// written.
    Unchanged,
}

/// What an unbind did: one `integration.unbound` row, and when
/// `membership_removed` the service account's `member.removed` row and
/// event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnbindOutcome {
    pub membership_removed: bool,
}

/// What a bind's or unbind's `decide` sees (the facts of
/// `projects::update_decided`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BindingFacts {
    pub account: Account,
    pub project: ProjectAccessFacts,
}

/// `FG-WEB-0006` (`field_invalid`) for `field` with `rule`.
fn field_invalid(field: &'static str, rule: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::RequestInvalid)
        .with_context("reason", "field_invalid")
        .with_context("field", field)
        .with_context("rule", rule)
}

fn not_found(resource: &'static str, reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::NotFound)
        .with_context("resource", resource)
        .with_context("reason", reason)
}

/// The allowlist as stored: every entry checked (`FG-WEB-0006
/// field=event_types`), sorted, without duplicates.
fn event_types(given: Option<&[String]>) -> Result<Vec<String>, ForgeError> {
    let mut types = match given {
        Some(types) => types.to_vec(),
        None => default_event_types(),
    };
    if types.is_empty() || types.len() > MAX_EVENT_TYPE_ENTRIES {
        return Err(field_invalid("event_types", "count").with_context("count", types.len()));
    }
    if types.iter().any(|t| !event_type_entry_ok(t)) {
        return Err(field_invalid("event_types", "unknown_event_type"));
    }
    types.sort();
    types.dedup();
    Ok(types)
}

/// Create an integration acting as `new.service_account`, with one
/// `integration.created` audit row; `created_by` is the administrator.
///
/// Errors (nothing written): `FG-WEB-0006 field=service_account_id`
/// `rule=not_service_account` (absent, a person or a bot) or
/// `service_account_in_use` (it has an integration already);
/// `field=event_types` (`count`, `unknown_event_type`).
pub async fn create(
    db: &Db,
    ctx: &ChangeContext,
    new: &NewIntegration,
    created_by: UserId,
) -> Result<IntegrationId, ForgeError> {
    let types = event_types(new.event_types.as_deref())?;
    let account = new.service_account;
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        // The account row FOR SHARE: its kind never changes, and a
        // disabled account may still get an integration (it reads nothing
        // until enabled again).
        let kind: Option<String> = tx
            .step(|c| async move {
                sqlx::query_scalar("SELECT kind FROM forge.users WHERE id = $1 FOR SHARE")
                    .bind(account.get())
                    .fetch_optional(c)
                    .await
                    .map_err(|e| query_error("integrations.create.account", e))
            })
            .await?;
        if kind.as_deref() != Some("service_account") {
            return Err(field_invalid("service_account_id", "not_service_account"));
        }
        let taken: bool = tx
            .step(|c| async move {
                sqlx::query_scalar(
                    "SELECT EXISTS (SELECT 1 FROM forge.integrations WHERE service_account_id = $1)",
                )
                .bind(account.get())
                .fetch_one(c)
                .await
                .map_err(|e| query_error("integrations.create.taken", e))
            })
            .await?;
        if taken {
            return Err(field_invalid("service_account_id", "service_account_in_use"));
        }
        let kind_name = new.kind.as_str();
        let stored = &types;
        let id: i64 = tx
            .step(|c| async move {
                sqlx::query_scalar(
                    "INSERT INTO forge.integrations (kind, service_account_id, event_types, \
                     created_by) VALUES ($1, $2, $3, $4) RETURNING id",
                )
                .bind(kind_name)
                .bind(account.get())
                .bind(stored)
                .bind(created_by.get())
                .fetch_one(c)
                .await
                .map_err(|e| query_error("integrations.create", e))
            })
            .await?;
        let integration = IntegrationId::new(id);
        let details = AuditDetails::Integration(IntegrationChange::Created {
            integration,
            kind: new.kind,
            service_account: account,
            event_types: types.clone(), // clone: the audit row keeps them
        });
        let record = ChangeRecord::audit_only(&details, RESOURCE, id);
        Ok(Settle::Commit(record, integration))
    }
    .await;
    tx.settle("integration.created", outcome).await
}

/// Enable or disable an integration, or replace its event type allowlist.
/// The row is locked `FOR UPDATE`; nothing is written when every given
/// value equals the stored one.
///
/// Errors (nothing written): `FG-REPO-0005 resource=integration`,
/// `FG-WEB-0006 field=event_types`.
pub async fn update(
    db: &Db,
    ctx: &ChangeContext,
    id: IntegrationId,
    changes: &Changes,
) -> Result<UpdateOutcome, ForgeError> {
    let new_types = changes
        .event_types
        .as_deref()
        .map(|t| event_types(Some(t)))
        .transpose()?;
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let row: Option<(bool, Vec<String>)> = tx
            .step(|c| async move {
                sqlx::query_as(
                    "SELECT enabled, event_types FROM forge.integrations WHERE id = $1 FOR UPDATE",
                )
                .bind(id.get())
                .fetch_optional(c)
                .await
                .map_err(|e| query_error("integrations.update.lock", e))
            })
            .await?;
        let Some((enabled, stored_types)) = row else {
            return Err(not_found("integration", "absent"));
        };
        let mut stored_sorted = stored_types;
        stored_sorted.sort();
        stored_sorted.dedup();
        let mut fields = Vec::new();
        let to_enabled = changes.enabled.unwrap_or(enabled);
        if to_enabled != enabled {
            fields.push(IntegrationField::Enabled);
        }
        let to_types = match new_types {
            Some(types) if types != stored_sorted => {
                fields.push(IntegrationField::EventTypes);
                types
            }
            Some(_) | None => stored_sorted,
        };
        if fields.is_empty() {
            return Ok(Settle::Discard(UpdateOutcome::Unchanged));
        }
        let stored = &to_types;
        tx.step(|c| async move {
            sqlx::query(
                "UPDATE forge.integrations SET enabled = $2, event_types = $3, \
                 updated_at = now() WHERE id = $1",
            )
            .bind(id.get())
            .bind(to_enabled)
            .bind(stored)
            .execute(c)
            .await
            .map_err(|e| query_error("integrations.update", e))
        })
        .await?;
        let details = AuditDetails::Integration(IntegrationChange::Updated {
            integration: id,
            fields: fields.clone(), // clone: the audit row and the answer both keep them
            enabled: to_enabled,
            event_types: to_types.clone(), // clone: as above
        });
        let record = ChangeRecord::audit_only(&details, RESOURCE, id.get());
        Ok(Settle::Commit(record, UpdateOutcome::Updated(fields)))
    }
    .await;
    tx.settle("integration.updated", outcome).await
}

/// Lock and read what `decide` sees (`projects::update_decided`'s facts).
async fn binding_facts(
    tx: &mut ChangeTx,
    project: ProjectId,
    viewer: UserId,
) -> Result<BindingFacts, ForgeError> {
    let locked = lock_access(tx, project).await?;
    let facts = locked_access_facts(tx, project, viewer, &locked).await?;
    Ok(BindingFacts {
        account: facts.account,
        project: facts.project,
    })
}

/// Lock the integration row: its service account.
async fn lock_integration(
    tx: &mut ChangeTx,
    integration: IntegrationId,
) -> Result<UserId, ForgeError> {
    let account: Option<i64> = tx
        .step(|c| async move {
            sqlx::query_scalar(
                "SELECT service_account_id FROM forge.integrations WHERE id = $1 FOR UPDATE",
            )
            .bind(integration.get())
            .fetch_optional(c)
            .await
            .map_err(|e| query_error("integrations.lock", e))
        })
        .await?;
    account
        .map(UserId::new)
        .ok_or_else(|| not_found("integration", "absent"))
}

/// The project's binding, if any (the project row is locked, so no other
/// bind or unbind of it runs meanwhile).
async fn binding_of(
    tx: &mut ChangeTx,
    project: ProjectId,
) -> Result<Option<(IntegrationId, String, bool)>, ForgeError> {
    let row: Option<(i64, String, bool)> = tx
        .step(|c| async move {
            sqlx::query_as(
                "SELECT integration_id, external_project_id, granted_membership \
                 FROM forge.integration_bindings WHERE project_id = $1",
            )
            .bind(project.get())
            .fetch_optional(c)
            .await
            .map_err(|e| query_error("integrations.binding_of", e))
        })
        .await?;
    Ok(row.map(|(i, e, g)| (IntegrationId::new(i), e, g)))
}

/// The record of a bind or unbind: its own row on the project, plus the
/// membership's row and event when the membership changed.
fn binding_record(
    details: &AuditDetails,
    project: ProjectId,
    membership: Option<&AuditDetails>,
) -> Result<ChangeRecord, ForgeError> {
    let mut record = ChangeRecord {
        audit: AuditEntry {
            action: details.action(),
            outcome: Outcome::Succeeded,
            resource_type: "project",
            resource_id: Some(project.get()),
            project_id: Some(project),
            error_code: None,
            details: details.to_value(),
        },
        affected: Vec::new(),
        events: Vec::new(),
    };
    if let Some(member) = membership {
        let member = evented_record(member, project)?;
        record.affected.push(member.audit);
        record.events.extend(member.events);
    }
    Ok(record)
}

/// Bind `project` to `external_project_id` of `integration`, after
/// `decide` on the locked project facts. A project pending deletion is
/// refused here too (`FG-REPO-0006`), whatever `decide` returned.
///
/// Errors (nothing written): `FG-WEB-0006 field=external_project_id`;
/// `FG-REPO-0005` `resource=project` / `resource=integration`;
/// `FG-REPO-0006`; `FG-INT-0016` (`bound_to_other_integration`,
/// `bound_to_other_external_project`); `FG-INT-0012 reason=binding_limit`;
/// whatever `decide` returns.
pub async fn bind_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    project: ProjectId,
    viewer: UserId,
    integration: IntegrationId,
    external_project_id: &str,
    decide: impl FnOnce(&BindingFacts) -> Result<P, ForgeError>,
) -> Result<(BindOutcome, P), ForgeError> {
    if !external_project_id_ok(external_project_id) {
        return Err(field_invalid("external_project_id", "format"));
    }
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let facts = binding_facts(&mut tx, project, viewer).await?;
        let decided = decide(&facts)?;
        if facts.project.access.pending_deletion {
            return Err(ForgeError::new(ErrorCode::PendingDeletion)
                .with_context("action", "IntegrationBindingManage")
                .with_context("project_id", project.get()));
        }
        let account = lock_integration(&mut tx, integration).await?;
        let conflict = |reason: &'static str, bound: IntegrationId| {
            ForgeError::new(ErrorCode::BindingConflict)
                .with_context("reason", reason)
                .with_context("project_id", project.get())
                .with_context("integration_id", integration.get())
                .with_context("bound_integration_id", bound.get())
        };
        match binding_of(&mut tx, project).await? {
            Some((bound, _, _)) if bound != integration => {
                return Err(conflict("bound_to_other_integration", bound));
            }
            Some((bound, external, _)) if external != external_project_id => {
                return Err(conflict("bound_to_other_external_project", bound));
            }
            Some(_) => return Ok(Settle::Discard((BindOutcome::Unchanged, decided))),
            None => {}
        }
        let bound: i64 = tx
            .step(|c| async move {
                sqlx::query_scalar(
                    "SELECT count(*) FROM forge.integration_bindings WHERE integration_id = $1",
                )
                .bind(integration.get())
                .fetch_one(c)
                .await
                .map_err(|e| query_error("integrations.bind.count", e))
            })
            .await?;
        if bound >= MAX_BINDINGS {
            return Err(ForgeError::new(ErrorCode::IntegrationLimitReached)
                .with_context("reason", "binding_limit")
                .with_context("limit", MAX_BINDINGS)
                .with_context("integration_id", integration.get())
                .with_context("project_id", project.get()));
        }
        let member = tx
            .step(|c| direct_role_for_update(c, project, account))
            .await?;
        let granted = member.is_none();
        if granted {
            tx.step(|c| member_candidate(c, account)).await?;
            tx.step(|c| async move {
                sqlx::query(
                    "INSERT INTO forge.project_members (project_id, user_id, role) \
                     VALUES ($1, $2, $3)",
                )
                .bind(project.get())
                .bind(account.get())
                .bind(Role::Reporter.as_str())
                .execute(c)
                .await
                .map_err(|e| query_error("integrations.bind.member", e))
            })
            .await?;
        }
        tx.step(|c| async move {
            sqlx::query(
                "INSERT INTO forge.integration_bindings (integration_id, project_id, \
                 external_project_id, granted_membership, created_by) VALUES ($1, $2, $3, $4, $5)",
            )
            .bind(integration.get())
            .bind(project.get())
            .bind(external_project_id)
            .bind(granted)
            .bind(viewer.get())
            .execute(c)
            .await
            .map_err(|e| query_error("integrations.bind", e))
        })
        .await?;
        let details = AuditDetails::Integration(IntegrationChange::Bound {
            integration,
            project,
            external_project_id: external_project_id.to_owned(),
            service_account: account,
            granted_membership: granted,
        });
        let added = AuditDetails::ProjectMemberAdded {
            user: account,
            role: Role::Reporter,
        };
        let record = binding_record(&details, project, granted.then_some(&added))?;
        Ok(Settle::Commit(
            record,
            (
                BindOutcome::Bound {
                    granted_membership: granted,
                },
                decided,
            ),
        ))
    }
    .await;
    tx.settle("integration.bound", outcome).await
}

/// Unbind `project` from `integration`, after `decide` on the locked
/// project facts (a project pending deletion may be unbound). The
/// service account's membership is removed when the binding granted it
/// and it is still a direct Reporter.
///
/// Errors (nothing written): `FG-REPO-0005` `resource=project`, or
/// `resource=integration_binding reason=not_bound`; whatever `decide`
/// returns.
pub async fn unbind_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    project: ProjectId,
    viewer: UserId,
    integration: IntegrationId,
    decide: impl FnOnce(&BindingFacts) -> Result<P, ForgeError>,
) -> Result<(UnbindOutcome, P), ForgeError> {
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let facts = binding_facts(&mut tx, project, viewer).await?;
        let decided = decide(&facts)?;
        let (external, granted) = match binding_of(&mut tx, project).await? {
            Some((bound, external, granted)) if bound == integration => (external, granted),
            Some(_) | None => return Err(not_found("integration_binding", "not_bound")),
        };
        let account = lock_integration(&mut tx, integration).await?;
        let removed = if granted {
            let role = tx
                .step(|c| direct_role_for_update(c, project, account))
                .await?;
            if role == Some(Role::Reporter) {
                tx.step(|c| async move {
                    sqlx::query(
                        "DELETE FROM forge.project_members WHERE project_id = $1 AND user_id = $2",
                    )
                    .bind(project.get())
                    .bind(account.get())
                    .execute(c)
                    .await
                    .map_err(|e| query_error("integrations.unbind.member", e))
                })
                .await?;
                true
            } else {
                false
            }
        } else {
            false
        };
        tx.step(|c| async move {
            sqlx::query(
                "DELETE FROM forge.integration_bindings \
                 WHERE integration_id = $1 AND project_id = $2",
            )
            .bind(integration.get())
            .bind(project.get())
            .execute(c)
            .await
            .map_err(|e| query_error("integrations.unbind", e))
        })
        .await?;
        let details = AuditDetails::Integration(IntegrationChange::Unbound {
            integration,
            project,
            external_project_id: external,
            service_account: account,
            membership_removed: removed,
        });
        let gone = AuditDetails::ProjectMemberRemoved {
            user: account,
            role: Role::Reporter,
        };
        let record = binding_record(&details, project, removed.then_some(&gone))?;
        Ok(Settle::Commit(
            record,
            (
                UnbindOutcome {
                    membership_removed: removed,
                },
                decided,
            ),
        ))
    }
    .await;
    tx.settle("integration.unbound", outcome).await
}

/// The columns of [`get`] and [`list`].
const SELECT: &str = concat!(
    "SELECT i.id, i.kind, i.service_account_id, u.username, i.enabled, i.event_types, ",
    "(SELECT count(*) FROM forge.integration_bindings b WHERE b.integration_id = i.id), ",
    "i.created_by, ",
    "to_char(i.created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'), ",
    "to_char(i.updated_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') ",
    "FROM forge.integrations i JOIN forge.users u ON u.id = i.service_account_id "
);

type Row = (
    i64,
    String,
    i64,
    String,
    bool,
    Vec<String>,
    i64,
    Option<i64>,
    String,
    String,
);

/// A stored kind this build does not know: `FG-SYS-0001` (the CHECK
/// `integrations_kind_known` lists the same names).
fn kind_of(id: i64, raw: &str) -> Result<IntegrationKind, ForgeError> {
    IntegrationKind::parse(raw).ok_or_else(|| {
        let raw: String = raw.chars().take(32).collect();
        ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "unknown_integration_kind")
            .with_context("integration_id", id)
            .with_context("raw", raw)
    })
}

fn integration(row: Row) -> Result<Integration, ForgeError> {
    let (
        id,
        kind,
        service_account,
        username,
        enabled,
        mut event_types,
        bindings,
        created_by,
        created_at,
        updated_at,
    ) = row;
    event_types.sort();
    Ok(Integration {
        id: IntegrationId::new(id),
        kind: kind_of(id, &kind)?,
        service_account: UserId::new(service_account),
        service_account_username: username,
        enabled,
        event_types,
        bindings,
        created_by: created_by.map(UserId::new),
        created_at,
        updated_at,
    })
}

/// Integration `id` (`None`: absent).
pub async fn get(db: &Db, id: IntegrationId) -> Result<Option<Integration>, ForgeError> {
    const QUERY: &str = "integrations.get";
    let row: Option<Row> = db
        .within(QUERY, async {
            let mut conn = db.acquire().await?;
            sqlx::query_as(&format!("{SELECT} WHERE i.id = $1"))
                .bind(id.get())
                .fetch_optional(&mut *conn)
                .await
                .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    row.map(integration).transpose()
}

/// Every integration by id (at most [`MAX_LISTED`]; `truncated` says
/// there are more).
pub async fn list(db: &Db) -> Result<Listed, ForgeError> {
    const QUERY: &str = "integrations.list";
    let rows: Vec<Row> = db
        .within(QUERY, async {
            let mut conn = db.acquire().await?;
            sqlx::query_as(&format!("{SELECT} ORDER BY i.id LIMIT $1"))
                .bind(MAX_LISTED + 1)
                .fetch_all(&mut *conn)
                .await
                .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    let truncated = i64::try_from(rows.len()).unwrap_or(i64::MAX) > MAX_LISTED;
    let mut items = rows
        .into_iter()
        .map(integration)
        .collect::<Result<Vec<_>, _>>()?;
    items.truncate(usize::try_from(MAX_LISTED).unwrap_or(usize::MAX));
    Ok(Listed { items, truncated })
}

/// The columns of [`bindings`] and [`project_bindings`].
const BINDING_SELECT: &str = concat!(
    "SELECT b.integration_id, i.kind, b.project_id, b.external_project_id, ",
    "b.granted_membership, b.created_by, ",
    "to_char(b.created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') ",
    "FROM forge.integration_bindings b JOIN forge.integrations i ON i.id = b.integration_id "
);

type BindingRow = (i64, String, i64, String, bool, Option<i64>, String);

fn binding(row: BindingRow) -> Result<Binding, ForgeError> {
    let (integration, kind, project, external_project_id, granted, created_by, created_at) = row;
    Ok(Binding {
        integration: IntegrationId::new(integration),
        kind: kind_of(integration, &kind)?,
        project: ProjectId::new(project),
        external_project_id,
        granted_membership: granted,
        created_by: created_by.map(UserId::new),
        created_at,
    })
}

async fn binding_rows(
    db: &Db,
    query: &'static str,
    filter: &str,
    id: i64,
) -> Result<Vec<Binding>, ForgeError> {
    let sql = format!("{BINDING_SELECT} WHERE {filter} = $1 ORDER BY b.project_id LIMIT $2");
    let rows: Vec<BindingRow> = db
        .within(query, async {
            let mut conn = db.acquire().await?;
            sqlx::query_as(&sql)
                .bind(id)
                .bind(MAX_BINDINGS)
                .fetch_all(&mut *conn)
                .await
                .map_err(|e| read_query_error(query, e))
        })
        .await?;
    rows.into_iter().map(binding).collect()
}

/// The projects bound to `integration` (at most [`MAX_BINDINGS`]), by
/// project id.
pub async fn bindings(db: &Db, integration: IntegrationId) -> Result<Vec<Binding>, ForgeError> {
    binding_rows(
        db,
        "integrations.bindings",
        "b.integration_id",
        integration.get(),
    )
    .await
}

/// The binding of `project` (none or one).
pub async fn project_bindings(db: &Db, project: ProjectId) -> Result<Vec<Binding>, ForgeError> {
    binding_rows(
        db,
        "integrations.project_bindings",
        "b.project_id",
        project.get(),
    )
    .await
}
