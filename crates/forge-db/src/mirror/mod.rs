//! The push mirror to GitLab (M3 O2, plan 11 §5 O2, plan 06 D-2 / D-4,
//! ADR-0024 Amendment O2): its setting, its progress and its job. The git
//! push is forge-git's (`mirror`); the job handler and the lag sweep are
//! `forge worker`'s (`worker/mirror.rs`).
//!
//! - [`set_decided`] / [`remove_decided`]: an administrator's change
//!   (`push_mirror.set` / `push_mirror.removed`, audited on the project).
//!   Setting a mirror (or enabling it again) marks it waiting and enqueues
//!   one push, so the existing history reaches GitLab at once.
//! - [`PushMirrorSubscriber`] (`push-mirror`, independent): each
//!   `repository.pushed` of a repository with an enabled mirror marks it
//!   waiting (`pending_seq`, `pending_since`) and enqueues one
//!   `repository.push_mirror` job keyed `push_mirror:<repo_id>:<seq>`. The
//!   job pushes every branch and tag as they are when it runs, so a job
//!   that runs late or twice is harmless and the newest one catches up.
//! - [`start`] / [`succeeded`] / [`failed`]: the job's reads and writes.
//! - [`sync_decided`]: an administrator's "push now" (decided too).
//! - [`lagging`]: the worker's sweep (FG-INT-0014 once an hour per mirror).
//! - [`report`]: `forge doctor` (`mirror.status`, `mirror.lag`).
//!
//! The status is operational state (tables.toml `derived`): no audit row.
//! The project purge deletes a project's rows.

use std::time::Duration;

use forge_core::ids::{ProjectId, RepoId, UserId};
use forge_core::mirror::PushMirrorChange;
use forge_core::{AuditDetails, EventType, RequestId};
use forge_errors::{ErrorCode, ForgeError};

use crate::change::{AuditEntry, ChangeContext, ChangeRecord, ChangeTx, Outcome, Settle};
use crate::jobs::{enqueue_on, DedupKey, Enqueued, JobKind, JobTimeout, MaxAttempts, NewJob};
use crate::projects::{lock_shared, locked_access_facts, ProjectUpdateFacts};
use crate::subscribers::{DerivedTx, HandleFuture, Ordering, SourceEvent, Subscriber};
use crate::{query_error, read_query_error, Db};

/// The kind of the mirror job.
pub const PUSH_MIRROR_JOB: &str = "repository.push_mirror";
/// The prefix of its dedup key.
pub const DEDUP_PREFIX: &str = "push_mirror";
/// The subscriber's name (its row in forge.event_subscribers).
pub const SUBSCRIBER: &str = "push-mirror";
/// Attempts of one job (backoff 10 s × 2^(n−1)); FG-INT-0013 on the last.
pub const JOB_ATTEMPTS: u8 = 5;
/// One run of the job (the push's own timeout is `mirror.push_timeout_secs`,
/// at most [`MAX_PUSH_TIMEOUT`]).
pub const JOB_TIMEOUT: Duration = Duration::from_secs(1800);
/// The longest push the job allows (leaves room for its database writes).
pub const MAX_PUSH_TIMEOUT: Duration = Duration::from_secs(1500);
/// FG-INT-0014 is logged at most this often per mirror.
pub const LAG_ALERT_EVERY: Duration = Duration::from_secs(3600);
/// The most rows of each kind [`report`] and [`lagging`] return.
pub const LIST_MAX: i64 = 50;
/// The audit resource type (the project).
const RESOURCE: &str = "project";

fn internal(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::Internal)
        .with_context("reason", reason)
        .with_context("kind", PUSH_MIRROR_JOB)
}

/// A mirror's setting as an administrator chose it (forge-core checked the
/// URL against the policy and the credential name's shape; the route
/// checked that the credential file exists).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MirrorSetting {
    /// As `MirrorUrl::as_str`.
    pub url: String,
    /// As `MirrorUrl::origin`.
    pub origin: String,
    pub credential_name: String,
    pub enabled: bool,
}

/// A mirror and its progress, as the administrator's read shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushMirror {
    pub repo: RepoId,
    pub project: ProjectId,
    pub url: String,
    pub origin: String,
    pub credential_name: String,
    pub enabled: bool,
    pub created_by: UserId,
    /// RFC 3339, UTC.
    pub created_at: String,
    pub updated_at: String,
    pub status: MirrorStatus,
}

/// A mirror's progress (`push_mirror_status`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MirrorStatus {
    pub pending_seq: i64,
    /// RFC 3339; `None` when up to date.
    pub pending_since: Option<String>,
    pub mirrored_seq: i64,
    pub last_attempt_at: Option<String>,
    pub last_success_at: Option<String>,
    pub failures: i32,
    pub last_error_code: Option<String>,
    pub last_reason: Option<String>,
}

/// What [`set_decided`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetOutcome {
    Created,
    Updated,
    /// Every value equals the stored one: nothing written.
    Unchanged,
}

/// The change's audit row on the project (no event).
fn record(change: PushMirrorChange, project: ProjectId) -> ChangeRecord {
    let details = AuditDetails::PushMirror(change);
    ChangeRecord {
        audit: AuditEntry {
            action: details.action(),
            outcome: Outcome::Succeeded,
            resource_type: RESOURCE,
            resource_id: Some(project.get()),
            project_id: Some(project),
            error_code: None,
            details: details.to_value(),
        },
        affected: Vec::new(),
        events: Vec::new(),
    }
}

async fn decided_facts(
    tx: &mut ChangeTx,
    project: ProjectId,
    caller: UserId,
) -> Result<ProjectUpdateFacts, ForgeError> {
    let locked = lock_shared(tx, project).await?;
    locked_access_facts(tx, project, caller, &locked).await
}

/// The project's ready repository, `FOR SHARE` (a purge waits). None:
/// `FG-INT-0021 reason=no_repository`.
async fn ready_repository(tx: &mut ChangeTx, project: ProjectId) -> Result<RepoId, ForgeError> {
    let id: Option<i64> = tx
        .step(|c| async move {
            sqlx::query_scalar(
                "SELECT id FROM forge.repositories WHERE project_id = $1 AND state = 'ready' \
                 FOR SHARE",
            )
            .bind(project.get())
            .fetch_optional(c)
            .await
            .map_err(|e| query_error("mirror.repository", e))
        })
        .await?;
    id.map(RepoId::new).ok_or_else(|| {
        ForgeError::new(ErrorCode::PushMirrorTargetInvalid)
            .with_context("reason", "no_repository")
            .with_context("project_id", project.get())
    })
}

/// The job that pushes `repo` now (keyed by `what`).
pub fn mirror_job(
    repo: RepoId,
    what: &str,
    origin: RequestId,
    event_seq: Option<i64>,
) -> Result<NewJob, ForgeError> {
    let key = format!("{DEDUP_PREFIX}:{}:{what}", repo.get());
    Ok(NewJob {
        kind: JobKind::parse(PUSH_MIRROR_JOB).ok_or_else(|| internal("invalid_job_kind"))?,
        payload: serde_json::json!({ "repo_id": repo.get() }),
        max_attempts: MaxAttempts::new(JOB_ATTEMPTS)
            .ok_or_else(|| internal("invalid_max_attempts"))?,
        timeout: JobTimeout::new(JOB_TIMEOUT).ok_or_else(|| internal("invalid_job_timeout"))?,
        dedup_key: Some(DedupKey::parse(&key).ok_or_else(|| internal("invalid_dedup_key"))?),
        origin_request_id: origin,
        origin_event_seq: event_seq,
    })
}

type StoredSetting = (String, String, String, bool);

/// Create or change the mirror of `project`'s repository. A new mirror, or
/// one enabled again, is marked waiting and gets one job. Errors (nothing
/// written): `decide`'s, `FG-REPO-0005 resource=project`, `FG-INT-0021
/// reason=no_repository`.
pub async fn set_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    project: ProjectId,
    caller: UserId,
    setting: &MirrorSetting,
    decide: impl FnOnce(&ProjectUpdateFacts) -> Result<P, ForgeError>,
) -> Result<(SetOutcome, P), ForgeError> {
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let decided = decide(&decided_facts(&mut tx, project, caller).await?)?;
        let repo = ready_repository(&mut tx, project).await?;
        let stored: Option<StoredSetting> = tx
            .step(|c| async move {
                sqlx::query_as(
                    "SELECT url, origin, credential_name, enabled FROM forge.push_mirrors \
                     WHERE repo_id = $1 FOR UPDATE",
                )
                .bind(repo.get())
                .fetch_optional(c)
                .await
                .map_err(|e| query_error("mirror.lock", e))
            })
            .await?;
        let same = stored.as_ref().is_some_and(|(url, _, name, enabled)| {
            *url == setting.url && *name == setting.credential_name && *enabled == setting.enabled
        });
        if same {
            return Ok(Settle::Discard((SetOutcome::Unchanged, decided)));
        }
        let created = stored.is_none();
        let was_enabled = stored.as_ref().is_some_and(|(_, _, _, e)| *e);
        let (url, origin, name, enabled) = (
            setting.url.clone(),             // clone: bound into the statement
            setting.origin.clone(),          // clone: as above
            setting.credential_name.clone(), // clone: as above
            setting.enabled,
        );
        tx.step(|c| async move {
            sqlx::query(
                "INSERT INTO forge.push_mirrors (repo_id, project_id, url, origin, \
                 credential_name, enabled, created_by) VALUES ($1, $2, $3, $4, $5, $6, $7) \
                 ON CONFLICT (repo_id) DO UPDATE SET url = EXCLUDED.url, \
                 origin = EXCLUDED.origin, credential_name = EXCLUDED.credential_name, \
                 enabled = EXCLUDED.enabled, updated_at = now()",
            )
            .bind(repo.get())
            .bind(project.get())
            .bind(url)
            .bind(origin)
            .bind(name)
            .bind(enabled)
            .bind(caller.get())
            .execute(c)
            .await
            .map_err(|e| query_error("mirror.set", e))
        })
        .await?;
        // A new destination (or credential) is behind until pushed once.
        if setting.enabled {
            tx.step(|c| async move {
                sqlx::query(
                    "INSERT INTO forge.push_mirror_status (repo_id, project_id, pending_since) \
                     VALUES ($1, $2, now()) ON CONFLICT (repo_id) DO UPDATE SET \
                     pending_since = coalesce(forge.push_mirror_status.pending_since, now()), \
                     updated_at = now()",
                )
                .bind(repo.get())
                .bind(project.get())
                .execute(c)
                .await
                .map_err(|e| query_error("mirror.status_waiting", e))
            })
            .await?;
            let what = format!("set:{}", ctx.request_id.as_str());
            let job = mirror_job(repo, &what, ctx.request_id.clone(), None)?; // clone: the job keeps it
            tx.enqueue_job(&job).await?;
        } else if was_enabled {
            // Disabled: nothing is waiting any more (no lag alert).
            tx.step(|c| async move {
                sqlx::query(
                    "UPDATE forge.push_mirror_status SET pending_since = NULL, updated_at = now() \
                     WHERE repo_id = $1",
                )
                .bind(repo.get())
                .execute(c)
                .await
                .map_err(|e| query_error("mirror.status_disabled", e))
            })
            .await?;
        }
        let change = PushMirrorChange::Set {
            project,
            repo,
            origin: setting.origin.clone(), // clone: the audit row owns it
            credential_name: setting.credential_name.clone(), // clone: as above
            enabled: setting.enabled,
            created,
        };
        let outcome = if created {
            SetOutcome::Created
        } else {
            SetOutcome::Updated
        };
        Ok(Settle::Commit(record(change, project), (outcome, decided)))
    }
    .await;
    tx.settle("push_mirror.set", outcome).await
}

/// Remove the mirror of `project`'s repository and its status (a queued
/// job finds nothing and ends). Errors: `decide`'s, `FG-REPO-0005`
/// (`resource=project` / `push_mirror`), `FG-INT-0021 reason=no_repository`.
pub async fn remove_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    project: ProjectId,
    caller: UserId,
    decide: impl FnOnce(&ProjectUpdateFacts) -> Result<P, ForgeError>,
) -> Result<P, ForgeError> {
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let decided = decide(&decided_facts(&mut tx, project, caller).await?)?;
        let repo = ready_repository(&mut tx, project).await?;
        let origin: Option<String> = tx
            .step(|c| async move {
                sqlx::query_scalar(
                    "SELECT origin FROM forge.push_mirrors WHERE repo_id = $1 FOR UPDATE",
                )
                .bind(repo.get())
                .fetch_optional(c)
                .await
                .map_err(|e| query_error("mirror.lock", e))
            })
            .await?;
        let origin = origin.ok_or_else(|| {
            ForgeError::new(ErrorCode::NotFound).with_context("resource", "push_mirror")
        })?;
        tx.step(|c| async move {
            sqlx::query("DELETE FROM forge.push_mirror_status WHERE repo_id = $1")
                .bind(repo.get())
                .execute(c)
                .await
                .map_err(|e| query_error("mirror.delete_status", e))
        })
        .await?;
        tx.step(|c| async move {
            sqlx::query("DELETE FROM forge.push_mirrors WHERE repo_id = $1")
                .bind(repo.get())
                .execute(c)
                .await
                .map_err(|e| query_error("mirror.delete", e))
        })
        .await?;
        let change = PushMirrorChange::Removed {
            project,
            repo,
            origin,
        };
        Ok(Settle::Commit(record(change, project), decided))
    }
    .await;
    tx.settle("push_mirror.removed", outcome).await
}

/// repo_id, project_id, url, origin, credential_name, enabled, created_by,
/// created_at, updated_at.
type MirrorRow = (i64, i64, String, String, String, bool, i64, String, String);
/// pending_seq, pending_since, mirrored_seq, last_attempt_at,
/// last_success_at, failures, last_error_code, last_reason.
type StatusRow = (
    i64,
    Option<String>,
    i64,
    Option<String>,
    Option<String>,
    i32,
    Option<String>,
    Option<String>,
);

const TS: &str = "YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"";

/// The mirror of `project` (any repository of it), with its status, for an
/// administrator (the full URL: the write pool, the read pool never sees
/// it). `None` when there is none.
pub async fn get(db: &Db, project: ProjectId) -> Result<Option<PushMirror>, ForgeError> {
    const QUERY: &str = "mirror.get";
    let mirror_sql = format!(
        "SELECT repo_id, project_id, url, origin, credential_name, enabled, created_by, \
         to_char(created_at AT TIME ZONE 'UTC', '{TS}'), \
         to_char(updated_at AT TIME ZONE 'UTC', '{TS}') \
         FROM forge.push_mirrors WHERE project_id = $1"
    );
    let status_sql = format!(
        "SELECT pending_seq, to_char(pending_since AT TIME ZONE 'UTC', '{TS}'), mirrored_seq, \
         to_char(last_attempt_at AT TIME ZONE 'UTC', '{TS}'), \
         to_char(last_success_at AT TIME ZONE 'UTC', '{TS}'), failures, last_error_code, \
         last_reason FROM forge.push_mirror_status WHERE repo_id = $1"
    );
    let row: Option<MirrorRow> = db
        .within(QUERY, async {
            sqlx::query_as(&mirror_sql)
                .bind(project.get())
                .fetch_optional(db.write_pool())
                .await
                .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    let Some(r) = row else {
        return Ok(None);
    };
    let status: Option<StatusRow> = db
        .within(QUERY, async {
            sqlx::query_as(&status_sql)
                .bind(r.0)
                .fetch_optional(db.write_pool())
                .await
                .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    let status = status.map_or_else(MirrorStatus::default, |s| MirrorStatus {
        pending_seq: s.0,
        pending_since: s.1,
        mirrored_seq: s.2,
        last_attempt_at: s.3,
        last_success_at: s.4,
        failures: s.5,
        last_error_code: s.6,
        last_reason: s.7,
    });
    Ok(Some(PushMirror {
        repo: RepoId::new(r.0),
        project: ProjectId::new(r.1),
        url: r.2,
        origin: r.3,
        credential_name: r.4,
        enabled: r.5,
        created_by: UserId::new(r.6),
        created_at: r.7,
        updated_at: r.8,
        status,
    }))
}

/// What the job pushes: the destination as stored now and the newest push
/// it covers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobTarget {
    pub project: ProjectId,
    pub url: String,
    pub origin: String,
    pub credential_name: String,
    /// The `pending_seq` read at the start: a success covers up to it.
    pub covers_seq: i64,
}

/// The job's start: the enabled mirror of a ready repository, its attempt
/// time recorded. `None` (the job does nothing): no mirror, disabled, or the
/// repository is not ready (purging, gone).
pub async fn start(db: &Db, repo: RepoId) -> Result<Option<JobTarget>, ForgeError> {
    const QUERY: &str = "mirror.start";
    let row: Option<(i64, String, String, String, i64)> = db
        .within(QUERY, async {
            sqlx::query_as(
                "WITH m AS (SELECT m.repo_id, m.project_id, m.url, m.origin, m.credential_name \
                     FROM forge.push_mirrors m JOIN forge.repositories r ON r.id = m.repo_id \
                     WHERE m.repo_id = $1 AND m.enabled AND r.state = 'ready') \
                 INSERT INTO forge.push_mirror_status AS s (repo_id, project_id, last_attempt_at) \
                 SELECT repo_id, project_id, now() FROM m \
                 ON CONFLICT (repo_id) DO UPDATE SET last_attempt_at = now(), updated_at = now() \
                 RETURNING s.project_id, (SELECT url FROM m), (SELECT origin FROM m), \
                     (SELECT credential_name FROM m), s.pending_seq",
            )
            .bind(repo.get())
            .fetch_optional(db.write_pool())
            .await
            .map_err(|e| query_error(QUERY, e).with_context("repo_id", repo.get()))
        })
        .await?;
    Ok(row.map(|(project, url, origin, name, seq)| JobTarget {
        project: ProjectId::new(project),
        url,
        origin,
        credential_name: name,
        covers_seq: seq,
    }))
}

/// A successful push covering `covers_seq`: failures cleared, and the mirror
/// is up to date unless a newer push arrived meanwhile (its own job follows).
pub async fn succeeded(db: &Db, repo: RepoId, covers_seq: i64) -> Result<(), ForgeError> {
    const QUERY: &str = "mirror.succeeded";
    db.within(QUERY, async {
        sqlx::query(
            "UPDATE forge.push_mirror_status SET last_success_at = now(), failures = 0, \
             last_error_code = NULL, last_reason = NULL, lag_alerted_at = NULL, \
             mirrored_seq = greatest(mirrored_seq, least($2, pending_seq)), \
             pending_since = CASE WHEN pending_seq <= $2 THEN NULL ELSE pending_since END, \
             updated_at = now() WHERE repo_id = $1",
        )
        .bind(repo.get())
        .bind(covers_seq)
        .execute(db.write_pool())
        .await
        .map_err(|e| query_error(QUERY, e).with_context("repo_id", repo.get()))
    })
    .await?;
    Ok(())
}

/// A failed attempt: `code` and `reason` kept, failures counted. Returns
/// the consecutive failures after this one.
pub async fn failed(
    db: &Db,
    repo: RepoId,
    code: ErrorCode,
    reason: &str,
) -> Result<i32, ForgeError> {
    const QUERY: &str = "mirror.failed";
    let reason = short_reason(reason);
    let failures: Option<i32> = db
        .within(QUERY, async {
            sqlx::query_scalar(
                "UPDATE forge.push_mirror_status SET failures = failures + 1, \
                 last_error_code = $2, last_reason = $3, updated_at = now() \
                 WHERE repo_id = $1 RETURNING failures",
            )
            .bind(repo.get())
            .bind(code.id())
            .bind(reason)
            .fetch_optional(db.write_pool())
            .await
            .map_err(|e| query_error(QUERY, e).with_context("repo_id", repo.get()))
        })
        .await?;
    Ok(failures.unwrap_or(0))
}

/// A reason fit for the CHECK (`[a-z][a-z0-9_]{0,39}`), `git` otherwise.
fn short_reason(reason: &str) -> &str {
    let ok = reason.len() <= 40
        && reason
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_lowercase())
        && reason
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
    if ok {
        reason
    } else {
        "git"
    }
}

/// Enqueue one push of `project`'s mirror now (`POST …/push-mirror/sync`,
/// `gstn mirror sync`, an administrator; key `…:admin:<request_id>`),
/// decided in the transaction like every `PushMirrorManage` command (a
/// caller who lost the rights after the route's decision is refused
/// here). Operational: no audit row (the route logs
/// `forge.mirror.sync_requested`). Errors: `decide`'s, `FG-REPO-0005`
/// (`resource=project`; `resource=push_mirror` with `state` when there is
/// no enabled mirror), `FG-INT-0021 reason=no_repository`.
pub async fn sync_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    project: ProjectId,
    caller: UserId,
    decide: impl FnOnce(&ProjectUpdateFacts) -> Result<P, ForgeError>,
) -> Result<(Enqueued, P), ForgeError> {
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let decided = decide(&decided_facts(&mut tx, project, caller).await?)?;
        let repo = ready_repository(&mut tx, project).await?;
        let enabled: Option<bool> = tx
            .step(|c| async move {
                sqlx::query_scalar(
                    "SELECT enabled FROM forge.push_mirrors WHERE repo_id = $1 FOR SHARE",
                )
                .bind(repo.get())
                .fetch_optional(c)
                .await
                .map_err(|e| query_error("mirror.sync_lock", e))
            })
            .await?;
        if enabled != Some(true) {
            return Err(ForgeError::new(ErrorCode::NotFound)
                .with_context("resource", "push_mirror")
                .with_context("repo_id", repo.get())
                .with_context(
                    "state",
                    if enabled.is_some() {
                        "disabled"
                    } else {
                        "missing"
                    },
                ));
        }
        let what = format!("admin:{}", ctx.request_id.as_str());
        let job = mirror_job(repo, &what, ctx.request_id.clone(), None)?; // clone: the job keeps it
        let enqueued = tx.enqueue_job(&job).await?;
        Ok(Settle::CommitDerived((enqueued, decided)))
    }
    .await;
    tx.settle("push_mirror.sync", outcome).await
}

/// A mirror waiting longer than the threshold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lagging {
    pub repo: RepoId,
    pub project: ProjectId,
    /// RFC 3339.
    pub pending_since: String,
    pub lag_secs: i64,
    pub last_success_at: Option<String>,
    pub last_error_code: Option<String>,
}

type LagRow = (i64, i64, String, i64, Option<String>, Option<String>);

/// The enabled mirrors waiting longer than `threshold` that were not
/// alerted within [`LAG_ALERT_EVERY`], marked alerted now (the worker logs
/// FG-INT-0014 for each). Oldest first, at most [`LIST_MAX`].
pub async fn lagging(db: &Db, threshold: Duration) -> Result<Vec<Lagging>, ForgeError> {
    const QUERY: &str = "mirror.lagging";
    let sql = format!(
        "UPDATE forge.push_mirror_status s SET lag_alerted_at = now() \
         FROM (SELECT s.repo_id FROM forge.push_mirror_status s \
               JOIN forge.push_mirrors m USING (repo_id) \
               WHERE m.enabled AND s.pending_since < now() - make_interval(secs => $1) \
               AND (s.lag_alerted_at IS NULL \
                    OR s.lag_alerted_at < now() - make_interval(secs => $2)) \
               ORDER BY s.pending_since LIMIT $3 FOR UPDATE OF s SKIP LOCKED) due \
         WHERE s.repo_id = due.repo_id \
         RETURNING s.repo_id, s.project_id, \
             to_char(s.pending_since AT TIME ZONE 'UTC', '{TS}'), \
             floor(extract(epoch FROM now() - s.pending_since))::bigint, \
             to_char(s.last_success_at AT TIME ZONE 'UTC', '{TS}'), s.last_error_code"
    );
    let rows: Vec<LagRow> = db
        .within(QUERY, async {
            sqlx::query_as(&sql)
                .bind(threshold.as_secs_f64())
                .bind(LAG_ALERT_EVERY.as_secs_f64())
                .bind(LIST_MAX)
                .fetch_all(db.write_pool())
                .await
                .map_err(|e| query_error(QUERY, e))
        })
        .await?;
    Ok(rows.into_iter().map(lag_from).collect())
}

fn lag_from((repo, project, since, lag, success, code): LagRow) -> Lagging {
    Lagging {
        repo: RepoId::new(repo),
        project: ProjectId::new(project),
        pending_since: since,
        lag_secs: lag,
        last_success_at: success,
        last_error_code: code,
    }
}

/// What `forge doctor` reads.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MirrorReport {
    pub mirrors: i64,
    pub enabled: i64,
    /// Enabled mirrors whose last attempt failed (`failures > 0`).
    pub failing: Vec<Lagging>,
    /// Enabled mirrors waiting longer than the threshold.
    pub lagging: Vec<Lagging>,
}

/// The mirrors' health for `forge doctor` (the read pool: no URL).
pub async fn report(db: &Db, threshold: Duration) -> Result<MirrorReport, ForgeError> {
    const QUERY: &str = "mirror.report";
    let (mirrors, enabled): (i64, i64) = db
        .within(QUERY, async {
            sqlx::query_as(
                "SELECT count(*), count(*) FILTER (WHERE enabled) FROM forge.push_mirrors",
            )
            .fetch_one(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    let list = |filter: &'static str| {
        format!(
            "SELECT s.repo_id, s.project_id, \
             coalesce(to_char(s.pending_since AT TIME ZONE 'UTC', '{TS}'), ''), \
             coalesce(floor(extract(epoch FROM now() - s.pending_since))::bigint, 0), \
             to_char(s.last_success_at AT TIME ZONE 'UTC', '{TS}'), s.last_error_code \
             FROM forge.push_mirror_status s JOIN forge.push_mirrors m USING (repo_id) \
             WHERE m.enabled AND {filter} ORDER BY s.repo_id LIMIT $2"
        )
    };
    let failing_sql = list("s.failures > 0 AND $1 >= 0");
    let lagging_sql = list("s.pending_since < now() - make_interval(secs => $1)");
    let mut out = MirrorReport {
        mirrors,
        enabled,
        ..MirrorReport::default()
    };
    for (sql, into) in [
        (failing_sql, &mut out.failing),
        (lagging_sql, &mut out.lagging),
    ] {
        let rows: Vec<LagRow> = db
            .within(QUERY, async {
                sqlx::query_as(&sql)
                    .bind(threshold.as_secs_f64())
                    .bind(LIST_MAX)
                    .fetch_all(db.pool().executor())
                    .await
                    .map_err(|e| read_query_error(QUERY, e))
            })
            .await?;
        *into = rows.into_iter().map(lag_from).collect();
    }
    Ok(out)
}

/// The `push-mirror` subscriber (module docs).
#[derive(Debug, Clone, Copy, Default)]
pub struct PushMirrorSubscriber;

impl PushMirrorSubscriber {
    async fn on_push(event: &SourceEvent, tx: &mut DerivedTx<'_>) -> Result<(), ForgeError> {
        let repo = RepoId::new(event.aggregate_id);
        let mirrored: Option<i64> = sqlx::query_scalar(
            "SELECT project_id FROM forge.push_mirrors WHERE repo_id = $1 AND enabled",
        )
        .bind(repo.get())
        .fetch_optional(tx.conn())
        .await
        .map_err(|e| query_error("mirror.on_push", e))?;
        let Some(project) = mirrored else {
            return Ok(());
        };
        sqlx::query(
            "INSERT INTO forge.push_mirror_status AS s (repo_id, project_id, pending_seq, \
             pending_since) VALUES ($1, $2, $3, now()) ON CONFLICT (repo_id) DO UPDATE SET \
             pending_seq = greatest(s.pending_seq, EXCLUDED.pending_seq), \
             pending_since = coalesce(s.pending_since, now()), updated_at = now()",
        )
        .bind(repo.get())
        .bind(project)
        .bind(event.seq)
        .execute(tx.conn())
        .await
        .map_err(|e| query_error("mirror.on_push_status", e))?;
        let origin = event.request_id().unwrap_or_else(RequestId::generate);
        let job = mirror_job(
            repo,
            &format!("push:{}", event.seq),
            origin,
            Some(event.seq),
        )?;
        enqueue_on(tx.conn(), &job).await?;
        Ok(())
    }
}

impl Subscriber for PushMirrorSubscriber {
    fn name(&self) -> &'static str {
        SUBSCRIBER
    }

    fn ordering(&self) -> Ordering {
        Ordering::Independent
    }

    fn handle<'a>(&'a self, event: &'a SourceEvent, tx: &'a mut DerivedTx<'_>) -> HandleFuture<'a> {
        Box::pin(async move {
            match EventType::parse(&event.event_type) {
                Some(EventType::RepositoryPushed) => Self::on_push(event, tx).await,
                Some(_) => Ok(()),
                None => {
                    tracing::warn!(
                        target: "forge_db",
                        event = "forge.subscribers.unknown_event_type",
                        subscriber = SUBSCRIBER,
                        request_id = %event.request_id,
                        source_seq = event.seq,
                        event_type = %event.event_type,
                        "an event type this build does not know was passed over"
                    );
                    Ok(())
                }
            }
        })
    }
}

#[cfg(test)]
mod tests;
