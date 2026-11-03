//! A project's merge settings (M3 R6, ADR-0024 §4.1, §8.2, §8.3, §9):
//! the methods allowed and the default, the source deletion default,
//! whether new commits clear the approvals, whether committers' approvals
//! are excluded, and the time-boxed CODEOWNERS enforcement switch.
//!
//! A project without a row has the defaults
//! (`forge_core::MergeSettingsValues::default`, CODEOWNERS enforced).
//! Every write is one `ChangeTx` that locks the project row and the
//! caller first (`facts`), decides with `decide` (forge-review:
//! `authorize()` again on what was locked), and writes one audit row on
//! the project: `merge_settings.updated` (audit only) or
//! `merge_settings.codeowners_enforcement_changed` (an event). The worker's
//! [`restore_expired_codeowners`] records an expired suspension once
//! (`merge_settings.codeowners_enforcement_restored`, actor `system`) —
//! for the record only: the evaluation reads an expired suspension as
//! enforced by itself ([`CodeownersEnforcement::enforced`], computed with
//! the database's clock).

use forge_core::ids::{ProjectId, UserId};
use forge_core::{
    AuditDetails, DedupKey, DedupPart, MergeMethod, MergeSettingsChange, MergeSettingsValues, Role,
};
use forge_errors::{ErrorCode, ForgeError};
use sqlx::postgres::PgConnection;

use crate::access::Account;
use crate::change::{
    AuditEntry, ChangeContext, ChangeRecord, ChangeTx, DomainEvent, Outcome, Settle,
};
use crate::projects::{evented_record, lock_access, locked_access_facts, ProjectAccessFacts};
use crate::{query_error, read_query_error, Db};

/// The CODEOWNERS enforcement of a project now.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum CodeownersEnforcement {
    #[default]
    Enforced,
    /// Suspended since `since` until `until` (UTC, RFC 3339).
    Suspended { since: String, until: String },
    /// A suspension whose expiry `until` has passed but that the worker has
    /// not recorded yet: enforced (ADR-0024 §9: the expiry restores it
    /// structurally, never through the job).
    Expired { until: String },
}

impl CodeownersEnforcement {
    /// Whether `evaluate_merge()` enforces CODEOWNERS.
    pub fn enforced(&self) -> bool {
        match self {
            Self::Enforced | Self::Expired { .. } => true,
            Self::Suspended { .. } => false,
        }
    }
}

/// A project's merge settings as stored (or the defaults).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StoredMergeSettings {
    pub values: MergeSettingsValues,
    pub codeowners: CodeownersEnforcement,
    /// The project has a row (`false`: the defaults).
    pub stored: bool,
    /// UTC, RFC 3339 (`None` without a row).
    pub updated_at: Option<String>,
}

/// What a settings write did.
#[must_use = "Unchanged wrote nothing; callers report it differently"]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsOutcome {
    Changed,
    /// The settings were already exactly so: nothing written.
    Unchanged,
}

/// A CODEOWNERS enforcement change as asked for (forge-review decided it
/// with `CodeownersEnforcementToggle`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodeownersChange {
    Enforce,
    /// Suspend for `minutes` (1..=1440, decided by `authorize()`; the
    /// table's CHECK is the backstop).
    Suspend {
        minutes: u32,
    },
}

/// What a settings command's `decide` sees, re-read under the locks: the
/// caller's account and role, the highest minimum push role of the
/// project's protection rules (`above_own_role` of the toggle and of a
/// loosening settings change), and the settings as stored (R6 review:
/// whether a change loosens the approvals is decided against them).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsFacts {
    pub account: Account,
    pub project: ProjectAccessFacts,
    pub highest_protection: Option<Role>,
    pub current: StoredMergeSettings,
}

const TS: &str = "'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'";

fn columns() -> String {
    format!(
        "allowed_methods, default_method, delete_source_default, reset_approvals_on_push, \
         committer_approvals_excluded, codeowners_enforced, \
         to_char(codeowners_suspended_at AT TIME ZONE 'UTC', {TS}) AS suspended_at, \
         to_char(codeowners_enforced_off_until AT TIME ZONE 'UTC', {TS}) AS off_until, \
         COALESCE(codeowners_enforced_off_until <= now(), false) AS expired, \
         to_char(updated_at AT TIME ZONE 'UTC', {TS}) AS updated_at"
    )
}

#[derive(sqlx::FromRow)]
struct SettingsRow {
    allowed_methods: Vec<String>,
    default_method: String,
    delete_source_default: bool,
    reset_approvals_on_push: bool,
    committer_approvals_excluded: bool,
    codeowners_enforced: bool,
    suspended_at: Option<String>,
    off_until: Option<String>,
    expired: bool,
    updated_at: String,
}

fn stored_invalid(project: ProjectId, field: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::Internal)
        .with_context("reason", "stored_merge_settings_invalid")
        .with_context("field", field)
        .with_context("project_id", project.get())
}

impl SettingsRow {
    fn parse(self, project: ProjectId) -> Result<StoredMergeSettings, ForgeError> {
        let method =
            |raw: &str| MergeMethod::parse(raw).ok_or_else(|| stored_invalid(project, "method"));
        let allowed_methods = self
            .allowed_methods
            .iter()
            .map(|m| method(m))
            .collect::<Result<Vec<_>, _>>()?;
        let codeowners = match (self.codeowners_enforced, self.suspended_at, self.off_until) {
            (true, None, None) => CodeownersEnforcement::Enforced,
            (false, Some(_), Some(until)) if self.expired => {
                CodeownersEnforcement::Expired { until }
            }
            (false, Some(since), Some(until)) => CodeownersEnforcement::Suspended { since, until },
            _ => return Err(stored_invalid(project, "codeowners")),
        };
        Ok(StoredMergeSettings {
            values: MergeSettingsValues {
                allowed_methods,
                default_method: method(&self.default_method)?,
                delete_source_default: self.delete_source_default,
                reset_approvals_on_push: self.reset_approvals_on_push,
                committer_approvals_excluded: self.committer_approvals_excluded,
            },
            codeowners,
            stored: true,
            updated_at: Some(self.updated_at),
        })
    }
}

/// The settings of `project` on `conn` (inside a change, or a read
/// transaction): the defaults without a row.
pub(crate) async fn settings_on(
    conn: &mut PgConnection,
    project: ProjectId,
) -> Result<StoredMergeSettings, ForgeError> {
    let row: Option<SettingsRow> = sqlx::query_as(&format!(
        "SELECT {} FROM forge.merge_settings WHERE project_id = $1",
        columns()
    ))
    .bind(project.get())
    .fetch_optional(conn)
    .await
    .map_err(|e| query_error("review.merge_settings", e))?;
    row.map_or_else(|| Ok(StoredMergeSettings::default()), |r| r.parse(project))
}

/// The settings of `project` (read pool, one statement).
pub async fn merge_settings(
    db: &Db,
    project: ProjectId,
) -> Result<StoredMergeSettings, ForgeError> {
    const QUERY: &str = "review.merge_settings_read";
    let row: Option<SettingsRow> = db
        .within(QUERY, async {
            sqlx::query_as(&format!(
                "SELECT {} FROM forge.merge_settings WHERE project_id = $1",
                columns()
            ))
            .bind(project.get())
            .fetch_optional(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    row.map_or_else(|| Ok(StoredMergeSettings::default()), |r| r.parse(project))
}

/// Lock the project and the caller (the lock order of the review
/// commands), then read the protection rules' highest level.
async fn settings_facts(
    tx: &mut ChangeTx,
    project: ProjectId,
    viewer: UserId,
) -> Result<SettingsFacts, ForgeError> {
    let locked = lock_access(tx, project).await?;
    let caller = locked_access_facts(tx, project, viewer, &locked).await?;
    let rules = tx
        .step(|c| async move { crate::protected_refs::rules_on(c, project).await })
        .await?;
    let current = tx
        .step(|c| async move { settings_on(c, project).await })
        .await?;
    Ok(SettingsFacts {
        account: caller.account,
        project: caller.project,
        highest_protection: rules.highest_push_min_role(),
        current,
    })
}

/// Whether `a` and `b` are the same settings (the allowed methods as a
/// set: their order is not a setting, R6 review).
fn same_values(a: &MergeSettingsValues, b: &MergeSettingsValues) -> bool {
    let sorted = |v: &MergeSettingsValues| {
        let mut m: Vec<&str> = v.allowed_methods.iter().map(|m| m.as_str()).collect();
        m.sort_unstable();
        m
    };
    sorted(a) == sorted(b)
        && a.default_method == b.default_method
        && a.delete_source_default == b.delete_source_default
        && a.reset_approvals_on_push == b.reset_approvals_on_push
        && a.committer_approvals_excluded == b.committer_approvals_excluded
}

/// Whether `new` loosens the approvals of `current` (R6 review): it counts
/// committers' approvals or carries approvals on push where `current` did
/// not.
pub fn loosens(current: &MergeSettingsValues, new: &MergeSettingsValues) -> bool {
    (current.committer_approvals_excluded && !new.committer_approvals_excluded)
        || (current.reset_approvals_on_push && !new.reset_approvals_on_push)
}

/// The expiry (UTC text and epoch) of `project`'s suspension when it has
/// passed, the row locked.
async fn expired_on(
    conn: &mut PgConnection,
    project: ProjectId,
) -> Result<Option<(String, i64)>, ForgeError> {
    sqlx::query_as(&format!(
        "SELECT to_char(codeowners_enforced_off_until AT TIME ZONE 'UTC', {TS}), \
         extract(epoch FROM codeowners_enforced_off_until)::bigint \
         FROM forge.merge_settings WHERE project_id = $1 \
         AND codeowners_enforced_off_until <= now() FOR UPDATE"
    ))
    .bind(project.get())
    .fetch_optional(conn)
    .await
    .map_err(|e| query_error("review.codeowners_restore_lock", e))
}

/// The audit row and the deduplicated event of an expired suspension's
/// restore (the worker's, or recorded first by the toggle that replaces it).
fn restored_record(
    project: ProjectId,
    expired_at: String,
    epoch: i64,
) -> Result<ChangeRecord, ForgeError> {
    let details = AuditDetails::MergeSettings {
        project,
        change: MergeSettingsChange::CodeownersEnforcementRestored { expired_at },
    };
    let mut record = evented_record(&details, project)?;
    let dedup = DedupKey::new(
        "codeowners_enforcement_restored",
        &[DedupPart::Id(project.get()), DedupPart::Id(epoch)],
    )?;
    record.events = record
        .events
        .into_iter()
        .map(|e| DomainEvent {
            dedup_key: Some(dedup.clone()), // clone: one key for the one event
            ..e
        })
        .collect();
    Ok(record)
}

fn audit_only(project: ProjectId, details: &AuditDetails) -> ChangeRecord {
    ChangeRecord {
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
    }
}

/// The values with duplicate methods dropped (first occurrence kept).
fn normalized(values: &MergeSettingsValues) -> MergeSettingsValues {
    let mut allowed: Vec<MergeMethod> = Vec::with_capacity(values.allowed_methods.len());
    for m in &values.allowed_methods {
        if !allowed.contains(m) {
            allowed.push(*m);
        }
    }
    MergeSettingsValues {
        allowed_methods: allowed,
        ..values.clone() // clone: the normalized copy owns its values
    }
}

/// Write `values` as the settings of `project` for `viewer` after `decide`
/// (forge-review checked the values: at least one method, the default
/// among them). The CODEOWNERS switch is not touched. `Unchanged` writes
/// nothing.
pub async fn put_merge_settings_mr_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    project: ProjectId,
    viewer: UserId,
    values: &MergeSettingsValues,
    decide: impl FnOnce(&SettingsFacts) -> Result<P, ForgeError>,
) -> Result<(SettingsOutcome, StoredMergeSettings, P), ForgeError> {
    const ACTION: &str = "merge_settings.updated";
    let values = normalized(values);
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let facts = settings_facts(&mut tx, project, viewer).await?;
        let decided = decide(&facts)?;
        let current = facts.current;
        if same_values(&current.values, &values) {
            return Ok(Settle::Discard((
                SettingsOutcome::Unchanged,
                current,
                decided,
            )));
        }
        let methods: Vec<&str> = values.allowed_methods.iter().map(|m| m.as_str()).collect();
        let v = &values;
        tx.step(|c| async move {
            sqlx::query(
                "INSERT INTO forge.merge_settings (project_id, allowed_methods, default_method, \
                 delete_source_default, reset_approvals_on_push, committer_approvals_excluded) \
                 VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT (project_id) DO UPDATE SET \
                 allowed_methods = EXCLUDED.allowed_methods, \
                 default_method = EXCLUDED.default_method, \
                 delete_source_default = EXCLUDED.delete_source_default, \
                 reset_approvals_on_push = EXCLUDED.reset_approvals_on_push, \
                 committer_approvals_excluded = EXCLUDED.committer_approvals_excluded, \
                 updated_at = now()",
            )
            .bind(project.get())
            .bind(&methods)
            .bind(v.default_method.as_str())
            .bind(v.delete_source_default)
            .bind(v.reset_approvals_on_push)
            .bind(v.committer_approvals_excluded)
            .execute(c)
            .await
            .map(drop)
            .map_err(|e| query_error("review.merge_settings_put", e))
        })
        .await?;
        let stored = tx
            .step(|c| async move { settings_on(c, project).await })
            .await?;
        let details = AuditDetails::MergeSettings {
            project,
            change: MergeSettingsChange::Updated {
                values: stored.values.clone(), // clone: the details own their values
            },
        };
        Ok(Settle::Commit(
            audit_only(project, &details),
            (SettingsOutcome::Changed, stored, decided),
        ))
    }
    .await;
    tx.settle(ACTION, outcome).await
}

/// Suspend or enforce CODEOWNERS on `project` for `viewer` after `decide`
/// (forge-review: `CodeownersEnforcementToggle` on the locked facts). A
/// suspension starts now (the database's clock) and ends `minutes` later;
/// suspending again replaces the expiry. Enforcing an enforced project is
/// `Unchanged` (nothing written); enforcing an expired suspension the
/// worker has not recorded yet records it. One audit row and one
/// `merge_settings.codeowners_enforcement_changed` event.
pub async fn toggle_codeowners_mr_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    project: ProjectId,
    viewer: UserId,
    change: CodeownersChange,
    decide: impl FnOnce(&SettingsFacts) -> Result<P, ForgeError>,
) -> Result<(SettingsOutcome, StoredMergeSettings, P), ForgeError> {
    const ACTION: &str = "merge_settings.codeowners_enforcement_changed";
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let facts = settings_facts(&mut tx, project, viewer).await?;
        let decided = decide(&facts)?;
        let current = facts.current;
        // R6 review: a suspension is never extended — suspending while
        // suspended changes nothing (the answer shows the expiry in force).
        if matches!(change, CodeownersChange::Suspend { .. })
            && matches!(current.codeowners, CodeownersEnforcement::Suspended { .. })
        {
            return Ok(Settle::Discard((SettingsOutcome::Unchanged, current, decided)));
        }
        // R6 review: an expired suspension replaced here is recorded as
        // restored first (the worker would find nothing left to record).
        let expired = tx.step(|c| async move { expired_on(c, project).await }).await?;
        let defaults = MergeSettingsValues::default();
        let default_methods: Vec<&str> = defaults
            .allowed_methods
            .iter()
            .map(|m| m.as_str())
            .collect();
        let default_method = defaults.default_method.as_str();
        match change {
            CodeownersChange::Enforce => {
                if current.codeowners == CodeownersEnforcement::Enforced {
                    return Ok(Settle::Discard((
                        SettingsOutcome::Unchanged,
                        current,
                        decided,
                    )));
                }
                tx.step(|c| async move {
                    sqlx::query(
                        "UPDATE forge.merge_settings SET codeowners_enforced = true, \
                         codeowners_suspended_at = NULL, codeowners_enforced_off_until = NULL, \
                         codeowners_restore_seen_at = NULL, updated_at = now() WHERE project_id = $1",
                    )
                    .bind(project.get())
                    .execute(c)
                    .await
                    .map(drop)
                    .map_err(|e| query_error("review.codeowners_enforce", e))
                })
                .await?;
            }
            CodeownersChange::Suspend { minutes } => {
                let minutes = i32::try_from(minutes).map_err(|_| {
                    ForgeError::new(ErrorCode::Internal)
                        .with_context("reason", "codeowners_minutes_out_of_range")
                })?;
                let methods = &default_methods;
                tx.step(|c| async move {
                    sqlx::query(
                        "INSERT INTO forge.merge_settings (project_id, allowed_methods, \
                         default_method, codeowners_enforced, codeowners_suspended_at, \
                         codeowners_enforced_off_until) VALUES ($1, $2, $3, false, now(), \
                         now() + make_interval(mins => $4)) ON CONFLICT (project_id) DO UPDATE \
                         SET codeowners_enforced = false, codeowners_suspended_at = now(), \
                         codeowners_enforced_off_until = now() + make_interval(mins => $4), \
                         codeowners_restore_seen_at = NULL, updated_at = now()",
                    )
                    .bind(project.get())
                    .bind(methods)
                    .bind(default_method)
                    .bind(minutes)
                    .execute(c)
                    .await
                    .map(drop)
                    .map_err(|e| query_error("review.codeowners_suspend", e))
                })
                .await?;
            }
        }
        let stored = tx
            .step(|c| async move { settings_on(c, project).await })
            .await?;
        let until = match &stored.codeowners {
            CodeownersEnforcement::Suspended { until, .. } => Some(until.clone()), // clone: the details own it
            CodeownersEnforcement::Enforced | CodeownersEnforcement::Expired { .. } => None,
        };
        let details = AuditDetails::MergeSettings {
            project,
            change: MergeSettingsChange::CodeownersEnforcementChanged {
                enforced: stored.codeowners.enforced(),
                until,
            },
        };
        let mut record = evented_record(&details, project)?;
        if let Some((expired_at, epoch)) = expired {
            let restored = restored_record(project, expired_at, epoch)?;
            record.affected.push(restored.audit);
            record.events.extend(restored.events);
        }
        Ok(Settle::Commit(
            record,
            (SettingsOutcome::Changed, stored, decided),
        ))
    }
    .await;
    tx.settle(ACTION, outcome).await
}

/// The most expired suspensions one run records.
pub const RESTORE_MAX_BATCH: i64 = 100;

/// What one run of [`restore_expired_codeowners`] did.
#[derive(Debug, Default)]
pub struct RestoreReport {
    /// Suspensions recorded as restored.
    pub restored: u32,
    /// Expired suspensions found (at most [`RESTORE_MAX_BATCH`]).
    pub found: u32,
    /// The projects whose record failed, with the failure (R6 review: one
    /// failing row never stops the batch; the caller logs each once, and the
    /// row moves to the back of the next run).
    pub failed: Vec<(ProjectId, ForgeError)>,
}

/// Record every suspension of CODEOWNERS enforcement past its expiry (at
/// most [`RESTORE_MAX_BATCH`] per run, the oldest first), one change per
/// project as `ctx`'s actor (the worker: `system`): the row back to
/// enforced, one audit row and one
/// `merge_settings.codeowners_enforcement_restored` event deduplicated by
/// `codeowners_enforcement_restored:<project_id>:<expiry epoch>`. A
/// project a person enforced or suspended again in between is skipped.
pub async fn restore_expired_codeowners(
    db: &Db,
    ctx: &ChangeContext,
) -> Result<RestoreReport, ForgeError> {
    const QUERY: &str = "review.codeowners_expired";
    let expired: Vec<i64> = db
        .within(QUERY, async {
            sqlx::query_scalar(
                "SELECT project_id FROM forge.merge_settings \
                 WHERE codeowners_enforced_off_until <= now() \
                 ORDER BY codeowners_restore_seen_at NULLS FIRST, \
                 codeowners_enforced_off_until, project_id LIMIT $1",
            )
            .bind(RESTORE_MAX_BATCH)
            .fetch_all(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    let mut report = RestoreReport {
        found: u32::try_from(expired.len()).unwrap_or(u32::MAX),
        ..RestoreReport::default()
    };
    for id in expired {
        let project = ProjectId::new(id);
        match restore_one(db, ctx, project).await {
            Ok(true) => report.restored += 1,
            Ok(false) => {}
            Err(e) => {
                // To the back of the next run; a failure to mark it is
                // reported with the record's own failure.
                let e = match mark_restore_seen(db, project).await {
                    Ok(()) => e,
                    Err(mark) => e.with_context("mark_error_code", mark.code().id()),
                };
                report.failed.push((project, e));
            }
        }
    }
    Ok(report)
}

/// Move `project`'s expired suspension to the back of the worker's scan
/// (`codeowners_restore_seen_at`, an unaudited bookkeeping column) — only
/// while it is still expired (R6 review round 2: a switch changed since
/// the scan keeps no stale mark).
async fn mark_restore_seen(db: &Db, project: ProjectId) -> Result<(), ForgeError> {
    const QUERY: &str = "review.codeowners_restore_seen";
    db.within(QUERY, async {
        sqlx::query(
            "UPDATE forge.merge_settings SET codeowners_restore_seen_at = now() \
             WHERE project_id = $1 AND codeowners_enforced_off_until <= now()",
        )
        .bind(project.get())
        .execute(db.write_pool())
        .await
        .map_err(|e| query_error(QUERY, e))?;
        Ok(())
    })
    .await
}

/// One project's expired suspension (`false`: no longer expired).
async fn restore_one(db: &Db, ctx: &ChangeContext, project: ProjectId) -> Result<bool, ForgeError> {
    const ACTION: &str = "merge_settings.codeowners_enforcement_restored";
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let row = tx
            .step(|c| async move { expired_on(c, project).await })
            .await?;
        let Some((expired_at, epoch)) = row else {
            return Ok(Settle::Discard(false));
        };
        tx.step(|c| async move {
            sqlx::query(
                "UPDATE forge.merge_settings SET codeowners_enforced = true, \
                 codeowners_suspended_at = NULL, codeowners_enforced_off_until = NULL, \
                 codeowners_restore_seen_at = NULL, updated_at = now() WHERE project_id = $1",
            )
            .bind(project.get())
            .execute(c)
            .await
            .map(drop)
            .map_err(|e| query_error("review.codeowners_restore", e))
        })
        .await?;
        Ok(Settle::Commit(
            restored_record(project, expired_at, epoch)?,
            true,
        ))
    }
    .await;
    tx.settle(ACTION, outcome).await
}

#[cfg(test)]
#[path = "settings_tests.rs"]
mod tests;
