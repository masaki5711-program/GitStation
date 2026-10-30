//! Protected ref rules (step D4 MR5, ADR-0008 and its D4 Amendments): create,
//! change and delete the rules of one project, and read the levels that
//! apply to one ref ([`levels_for`]) — the input of forge-auth's
//! `RefProtection::effective` and of the pre-receive policy snapshot.
//!
//! Each command is one `ChangeTx` that locks the project row `FOR UPDATE`
//! first (the lock of `projects::members`), so the rule count and the
//! duplicate check run against the committed rules of the project, and
//! writes one `protected_ref.*` audit row on (`project`, id) with
//! `project_id` set plus one `protected_ref.*` event of the maintainer class
//! (ADR-0016 §5), without a dedup key.
//!
//! Two rules the database does not hold are kept here (ADR-0008 Amendment
//! D4 MR1): at most [`MAX_RULES`] rules per project (`FG-REPO-0013`
//! `too_many_rules`), and no two patterns of one project equal ignoring case
//! by [`RefPattern::key`] (`duplicate_pattern`; the unique index
//! `protected_refs_project_pattern` folds ASCII letters only). Rules are not
//! authorized here: a route decides with `authorize()`
//! (`Action::ProtectedRefManage`) before calling.

use forge_core::ids::{ProjectId, ProtectedRefId};
use forge_core::{AuditDetails, RefName, RefPattern, Role};
use forge_errors::{ErrorCode, ForgeError};
use sqlx::postgres::PgConnection;

use crate::change::{ChangeContext, ChangeTx};
use crate::projects::{evented_record, lock_project};
use crate::{query_error, read_query_error, Db};

/// The most rules one project may have (ADR-0008 Amendment D4 MR1).
pub const MAX_RULES: usize = 100;

/// What `change` did.
#[must_use = "Unchanged wrote nothing; callers report it differently"]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleChange {
    /// The minimum push role changed from `from`: one `protected_ref.updated`
    /// audit row and event.
    Changed { from: Role },
    /// The rule already had that role: nothing was written.
    Unchanged,
}

/// One stored rule, parsed.
#[derive(Debug, Clone)]
struct Rule {
    pattern: RefPattern,
    push_min_role: Role,
}

/// `FG-REPO-0013` with its `reason`.
fn rule_invalid(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::ProtectedRefRuleInvalid).with_context("reason", reason)
}

/// A rule's minimum push role is Developer, Maintainer or Owner (the CHECK
/// `protected_refs_push_min_role`): Guest and Reporter cannot push anyway,
/// so a rule naming them would protect nothing (`FG-REPO-0013`
/// `push_min_role`).
fn check_push_min_role(role: Role) -> Result<(), ForgeError> {
    match role {
        Role::Developer | Role::Maintainer | Role::Owner => Ok(()),
        Role::Guest | Role::Reporter => Err(rule_invalid("push_min_role")),
    }
}

/// A stored row this build cannot read (written outside forge-db,
/// `FG-DB-0020`): never skipped, since skipping a rule would loosen
/// protection.
fn stored_invalid(reason: &'static str, id: i64) -> ForgeError {
    ForgeError::new(ErrorCode::ProtectedRefDataInvalid)
        .with_context("reason", reason)
        .with_context("protected_ref_id", id)
}

fn parse_row((id, pattern, role): (i64, String, String)) -> Result<Rule, ForgeError> {
    let pattern =
        RefPattern::parse(&pattern).map_err(|_| stored_invalid("stored_pattern_invalid", id))?;
    let push_min_role = Role::parse(&role)
        .filter(|r| check_push_min_role(*r).is_ok())
        .ok_or_else(|| stored_invalid("stored_push_min_role_invalid", id))?;
    Ok(Rule {
        pattern,
        push_min_role,
    })
}

/// The rows of a project's rules, at most `MAX_RULES + 1` (a bounded query:
/// one row past the cap is enough to know the cap is reached or broken).
const RULES_SQL: &str = "SELECT id, pattern, push_min_role FROM forge.protected_refs \
     WHERE project_id = $1 ORDER BY id LIMIT $2";

/// The bind of `LIMIT`: one past the cap.
fn row_limit() -> i64 {
    i64::try_from(MAX_RULES).map_or(i64::MAX, |n| n.saturating_add(1))
}

/// More stored rows than `MAX_RULES` (written outside forge-db) is
/// `FG-DB-0020` `too_many_stored_rules`, never a partial rule set.
fn parse_rows(
    project: ProjectId,
    rows: Vec<(i64, String, String)>,
) -> Result<Vec<Rule>, ForgeError> {
    if rows.len() > MAX_RULES {
        return Err(ForgeError::new(ErrorCode::ProtectedRefDataInvalid)
            .with_context("reason", "too_many_stored_rules")
            .with_context("project_id", project.get()));
    }
    rows.into_iter().map(parse_row).collect()
}

/// The project's rules, read inside a change after the project lock.
async fn rules_locked(
    conn: &mut PgConnection,
    project: ProjectId,
) -> Result<Vec<Rule>, ForgeError> {
    let rows: Vec<(i64, String, String)> = sqlx::query_as(RULES_SQL)
        .bind(project.get())
        .bind(row_limit())
        .fetch_all(conn)
        .await
        .map_err(|e| query_error("protected_refs.rules", e))?;
    parse_rows(project, rows)
}

/// The stored row of rule `rule` of `project`, locked `FOR UPDATE` and not
/// parsed (so `delete` can remove a row this build cannot read).
/// `FG-REPO-0005` (`resource=protected_ref`) when the project has no such
/// rule.
async fn lock_rule_row(
    conn: &mut PgConnection,
    project: ProjectId,
    rule: ProtectedRefId,
) -> Result<(i64, String, String), ForgeError> {
    let row: Option<(i64, String, String)> = sqlx::query_as(
        "SELECT id, pattern, push_min_role FROM forge.protected_refs \
         WHERE id = $1 AND project_id = $2 FOR UPDATE",
    )
    .bind(rule.get())
    .bind(project.get())
    .fetch_optional(conn)
    .await
    .map_err(|e| query_error("protected_refs.lock", e))?;
    row.ok_or_else(|| {
        ForgeError::new(ErrorCode::NotFound)
            .with_context("resource", "protected_ref")
            .with_context("reason", "absent")
    })
}

/// The rule `rule` of `project`, locked `FOR UPDATE` and parsed
/// (`FG-DB-0020` for a row this build cannot read).
async fn lock_rule(
    conn: &mut PgConnection,
    project: ProjectId,
    rule: ProtectedRefId,
) -> Result<Rule, ForgeError> {
    parse_row(lock_rule_row(conn, project, rule).await?)
}

/// `s` cut to at most `max` bytes, on a character boundary.
fn bounded(s: &str, max: usize) -> String {
    let mut end = s.len().min(max);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s.get(..end).unwrap_or_default().to_owned()
}

/// A unique violation on the pattern index is a duplicate (the ASCII
/// backstop of the `key()` check); anything else stays a query error.
fn insert_conflict(e: sqlx::Error) -> ForgeError {
    match e.as_database_error().and_then(|d| d.constraint()) {
        Some("protected_refs_project_pattern") => rule_invalid("duplicate_pattern"),
        _ => query_error("protected_refs.create", e),
    }
}

async fn commit(
    tx: ChangeTx,
    project: ProjectId,
    details: &AuditDetails,
) -> Result<(), ForgeError> {
    tx.commit(evented_record(details, project)?)
        .await?
        .require_recorded(details.action())
}

/// Add a rule to `project`: one `protected_ref.created` audit row and event.
///
/// Errors (nothing is written): `FG-REPO-0013` `push_min_role` (Guest or
/// Reporter), `duplicate_pattern` (another rule of the project has the same
/// `RefPattern::key`), `too_many_rules` (the project has `MAX_RULES`);
/// `FG-REPO-0005` (`resource=project`) for an unknown project.
pub async fn create(
    db: &Db,
    ctx: &ChangeContext,
    project: ProjectId,
    pattern: &RefPattern,
    push_min_role: Role,
) -> Result<ProtectedRefId, ForgeError> {
    check_push_min_role(push_min_role)?;
    let mut tx = ChangeTx::begin(db, ctx).await?;
    tx.step(|c| lock_project(c, project)).await?;
    // Counted under the project lock: concurrent creates of one project run
    // one at a time, so the cap and the duplicate check cannot be raced.
    let rules = tx.step(|c| rules_locked(c, project)).await?;
    if rules.len() >= MAX_RULES {
        return Err(rule_invalid("too_many_rules").with_context("rules", rules.len()));
    }
    if rules.iter().any(|r| r.pattern.key() == pattern.key()) {
        return Err(rule_invalid("duplicate_pattern"));
    }
    let id: i64 = tx
        .step(|c| async move {
            sqlx::query_scalar(
                "INSERT INTO forge.protected_refs (project_id, pattern, push_min_role) \
                 VALUES ($1, $2, $3) RETURNING id",
            )
            .bind(project.get())
            .bind(pattern.as_str())
            .bind(push_min_role.as_str())
            .fetch_one(c)
            .await
            .map_err(insert_conflict)
        })
        .await?;
    let rule = ProtectedRefId::new(id);
    let details = AuditDetails::ProtectedRefCreated {
        rule,
        pattern: pattern.as_str().to_owned(),
        push_min_role,
    };
    commit(tx, project, &details).await?;
    Ok(rule)
}

/// Change a rule's minimum push role: one `protected_ref.updated` audit row
/// and event, or `Unchanged` (nothing written) when it is the same. The
/// pattern is not changed (delete the rule and create another).
/// `FG-REPO-0013` `push_min_role`; `FG-REPO-0005` (`resource=project` /
/// `protected_ref`) when the project or its rule does not exist.
pub async fn change(
    db: &Db,
    ctx: &ChangeContext,
    project: ProjectId,
    rule: ProtectedRefId,
    push_min_role: Role,
) -> Result<RuleChange, ForgeError> {
    check_push_min_role(push_min_role)?;
    let mut tx = ChangeTx::begin(db, ctx).await?;
    tx.step(|c| lock_project(c, project)).await?;
    let current = tx.step(|c| lock_rule(c, project, rule)).await?;
    let from = current.push_min_role;
    if from == push_min_role {
        // Dropping the transaction releases the locks; nothing was written.
        return Ok(RuleChange::Unchanged);
    }
    tx.step(|c| async move {
        sqlx::query(
            "UPDATE forge.protected_refs SET push_min_role = $2, updated_at = now() \
             WHERE id = $1",
        )
        .bind(rule.get())
        .bind(push_min_role.as_str())
        .execute(c)
        .await
        .map_err(|e| query_error("protected_refs.change", e))
    })
    .await?;
    let details = AuditDetails::ProtectedRefUpdated {
        rule,
        pattern: current.pattern.as_str().to_owned(),
        from,
        to: push_min_role,
    };
    commit(tx, project, &details).await?;
    Ok(RuleChange::Changed { from })
}

/// Delete a rule: one `protected_ref.deleted` audit row and event.
/// `FG-REPO-0005` (`resource=project` / `protected_ref`) when the project or
/// its rule does not exist. The row is not parsed, so a rule this build
/// cannot read (`FG-DB-0020`, written outside forge-db) can be deleted — the
/// runbook `protected-ref-data` fix; its details then carry
/// `stored_invalid: true` with the stored text bounded.
pub async fn delete(
    db: &Db,
    ctx: &ChangeContext,
    project: ProjectId,
    rule: ProtectedRefId,
) -> Result<(), ForgeError> {
    let mut tx = ChangeTx::begin(db, ctx).await?;
    tx.step(|c| lock_project(c, project)).await?;
    let (id, pattern, role) = tx.step(|c| lock_rule_row(c, project, rule)).await?;
    let stored_invalid = parse_row((id, pattern.clone(), role.clone())).is_err(); // clone: the raw text goes into the details
    tx.step(|c| async move {
        sqlx::query("DELETE FROM forge.protected_refs WHERE id = $1")
            .bind(rule.get())
            .execute(c)
            .await
            .map_err(|e| query_error("protected_refs.delete", e))
    })
    .await?;
    let details = AuditDetails::ProtectedRefDeleted {
        rule,
        pattern: bounded(&pattern, forge_core::ref_pattern::MAX_PATTERN_LEN),
        push_min_role: role.chars().take(32).collect(),
        stored_invalid,
    };
    commit(tx, project, &details).await
}

/// The minimum push roles of **every** rule of `project` whose pattern
/// matches `name`, ignoring case (`RefPattern::matches`), in rule id order;
/// empty = the ref is not protected. Feed it to
/// `forge_auth::RefProtection::effective`, which takes the strictest, so a
/// matching rule can never be dropped in between (ADR-0008 Amendment
/// D4 MR5: the policy snapshot is built from this).
///
/// One bounded read on the read pool (at most `MAX_RULES + 1` rows). More
/// rules than `MAX_RULES`, or a stored pattern or role this build cannot
/// parse (written outside forge-db), is `FG-DB-0020` ProtectedRefDataInvalid
/// (`too_many_stored_rules` / `stored_pattern_invalid` /
/// `stored_push_min_role_invalid`, with `project_id` / `protected_ref_id`):
/// the push is refused rather than decided on a partial rule set. An unknown
/// project has no rules (the caller has already loaded it).
pub async fn levels_for(
    db: &Db,
    project: ProjectId,
    name: &RefName,
) -> Result<Vec<Role>, ForgeError> {
    const QUERY: &str = "protected_refs.levels_for";
    let rows: Vec<(i64, String, String)> = db
        .within(QUERY, async {
            sqlx::query_as(RULES_SQL)
                .bind(project.get())
                .bind(row_limit())
                .fetch_all(db.pool().executor())
                .await
                .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    Ok(parse_rows(project, rows)?
        .into_iter()
        .filter(|rule| rule.pattern.matches(name))
        .map(|rule| rule.push_min_role)
        .collect())
}

#[cfg(test)]
mod tests;
