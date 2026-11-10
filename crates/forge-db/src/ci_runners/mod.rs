//! CI runners and their API (M4 C3, plan 14 §4.1, ADR-0029 §1; migration
//! 20261205100000): the typed commands and reads of `forge.ci_runners`,
//! `forge.ci_job_tokens` and `forge.ci_log_chunks`, and the runner side of
//! `forge.ci_jobs`.
//!
//! - Administration ([`register`], [`update`], [`list`], [`get`]): an
//!   administrator's change, audited on the runner in the same transaction
//!   (`ci_runner.registered` / `.updated`; the table's audit trigger
//!   refuses a write without it). The runner's token is shown once.
//! - Authentication ([`authenticate`]): a runner token (`fgr_…`: enabled
//!   runners only) or a job token (`fgj_…`: its job, project and pinned SHA
//!   and its state from the database's clock — ADR-0029 obligation 2), on
//!   the write pool (the read pool never sees hashes, ADR-0018).
//! - The lease ([`lease`]): claim with `FOR UPDATE SKIP LOCKED`,
//!   heartbeat, log append, completion, the reaper, and the doctor's
//!   health read.
//!
//! Job tokens are revoked in the transaction that ends the job (completion,
//! cancel, lost lease) and in the project's deletion and move
//! ([`revoke_project_job_tokens`], obligation 1).

pub mod lease;

use forge_core::ci_token::{CiToken, CiTokenKind};
use forge_core::pipeline::RunnerChange;
use forge_core::token::{self, TokenSecret};
use forge_core::{AuditDetails, CiIdentity, CiJobTokenState, ObjectId, ProjectId, RequestId};
use forge_errors::{ErrorCode, ForgeError};
use sqlx::PgConnection;

use crate::change::{AuditEntry, ChangeContext, ChangeRecord, ChangeTx, Outcome, Settle};
use crate::{query_error, read_query_error, Db};

/// The audit resource type of a runner.
pub const RESOURCE: &str = "ci_runner";
/// The most runners [`list`] returns.
pub const MAX_RUNNERS: i64 = 500;
/// The most tags a runner carries (the table's CHECK).
pub const MAX_TAGS: usize = 16;
/// The longest description, in bytes (the table's CHECK).
pub const MAX_DESCRIPTION: usize = 1024;
/// `last_contact_at` is written at most this often by authentication.
pub const CONTACT_INTERVAL_S: i64 = 30;

const TS: &str = "YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"";

/// Compared against when the public id is unknown, so a miss costs the
/// same hash and comparison as a hit.
const DUMMY_HASH: [u8; 32] = [0u8; 32];

/// A runner name or a tag: `[a-z0-9][a-z0-9._-]{0,62}` (the jobs' tag rule).
pub fn valid_name(raw: &str) -> bool {
    let b = raw.as_bytes();
    !b.is_empty()
        && b.len() <= 63
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"._-".contains(c))
}

/// A runner to register (the route validated the shapes; the table's
/// CHECKs are the backstop).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewRunner {
    pub name: String,
    pub description: String,
    pub tags: Vec<String>,
}

/// What may change on a runner (`None`: unchanged).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunnerUpdate {
    pub enabled: Option<bool>,
    pub tags: Option<Vec<String>>,
    pub description: Option<String>,
}

/// A runner as read (never its token or hash).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredRunner {
    pub id: i64,
    pub public_id: String,
    pub name: String,
    pub description: String,
    pub tags: Vec<String>,
    pub enabled: bool,
    pub created_by: i64,
    pub created_at: String,
    pub updated_at: String,
    pub last_contact_at: Option<String>,
}

/// A registered runner and its token (shown once).
#[derive(Debug)]
pub struct Registered {
    pub runner: StoredRunner,
    pub token: CiToken,
}

type RunnerRow = (
    i64,
    String,
    String,
    String,
    Vec<String>,
    bool,
    i64,
    String,
    String,
    Option<String>,
);

fn columns() -> String {
    format!(
        "id, public_id, name, description, tags, enabled, created_by, \
         to_char(created_at AT TIME ZONE 'UTC', '{TS}'), \
         to_char(updated_at AT TIME ZONE 'UTC', '{TS}'), \
         to_char(last_contact_at AT TIME ZONE 'UTC', '{TS}')"
    )
}

fn runner_of(r: RunnerRow) -> StoredRunner {
    StoredRunner {
        id: r.0,
        public_id: r.1,
        name: r.2,
        description: r.3,
        tags: r.4,
        enabled: r.5,
        created_by: r.6,
        created_at: r.7,
        updated_at: r.8,
        last_contact_at: r.9,
    }
}

fn record(change: &RunnerChange) -> ChangeRecord {
    let details = AuditDetails::Runner(change.clone()); // clone: the caller logs it too
    ChangeRecord {
        audit: AuditEntry {
            action: details.action(),
            outcome: Outcome::Succeeded,
            resource_type: RESOURCE,
            resource_id: Some(change.runner()),
            project_id: None,
            error_code: None,
            details: details.to_value(),
        },
        affected: Vec::new(),
        events: Vec::new(),
    }
}

fn token_failed() -> ForgeError {
    ForgeError::new(ErrorCode::Internal).with_context("reason", "ci_token_rng_failed")
}

/// Register a runner (an administrator's change, `ci_runner.registered`).
/// Errors (nothing written): `FG-WEB-0006 reason=runner_name_taken`, the
/// database's.
pub async fn register(
    db: &Db,
    ctx: &ChangeContext,
    new: &NewRunner,
) -> Result<Registered, ForgeError> {
    const QUERY: &str = "ci_runners.register";
    let token = CiToken::generate(CiTokenKind::Runner).map_err(|_| token_failed())?;
    let hash = token.secret_sha256();
    let public_id = token.public_id().as_str().to_owned();
    let created_by = ctx.actor.user().ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "runner_register_without_user")
    })?;
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let sql = format!(
        "INSERT INTO forge.ci_runners (public_id, secret_sha256, name, description, tags, \
           created_by) VALUES ($1, $2, $3, $4, $5, $6) \
         ON CONFLICT (name) DO NOTHING RETURNING {}",
        columns()
    );
    let outcome = async {
        let row: Option<RunnerRow> = tx
            .step(|c| async move {
                sqlx::query_as(&sql)
                    .bind(&public_id)
                    .bind(hash.as_slice())
                    .bind(&new.name)
                    .bind(&new.description)
                    .bind(&new.tags)
                    .bind(created_by)
                    .fetch_optional(c)
                    .await
                    .map_err(|e| query_error(QUERY, e))
            })
            .await?;
        let Some(row) = row else {
            return Err(ForgeError::new(ErrorCode::RequestInvalid)
                .with_context("reason", "runner_name_taken")
                .with_context("param", "name"));
        };
        let runner = runner_of(row);
        let change = RunnerChange::Registered {
            runner: runner.id,
            name: runner.name.clone(), // clone: the audit row owns it
            tags: runner.tags.clone(), // clone: as above
        };
        Ok(Settle::Commit(record(&change), runner))
    }
    .await;
    let runner = tx.settle("ci_runner.registered", outcome).await?;
    Ok(Registered { runner, token })
}

/// Change a runner (`ci_runner.updated`); `None`: no such runner.
/// Disabling it also stops its token and the job tokens it holds at the
/// next request (authentication reads `enabled` for both; the jobs come
/// back through the reaper once their leases end). An update that changes nothing writes nothing.
pub async fn update(
    db: &Db,
    ctx: &ChangeContext,
    id: i64,
    change: &RunnerUpdate,
) -> Result<Option<StoredRunner>, ForgeError> {
    const QUERY: &str = "ci_runners.update";
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let sql = format!(
        "UPDATE forge.ci_runners SET enabled = coalesce($2, enabled), \
           tags = coalesce($3, tags), description = coalesce($4, description), \
           updated_at = now() WHERE id = $1 RETURNING {}",
        columns()
    );
    let outcome = async {
        let current: Option<(bool, Vec<String>, String)> = tx
            .step(|c| async move {
                sqlx::query_as(
                    "SELECT enabled, tags, description FROM forge.ci_runners WHERE id = $1 \
                     FOR UPDATE",
                )
                .bind(id)
                .fetch_optional(c)
                .await
                .map_err(|e| query_error(QUERY, e))
            })
            .await?;
        let Some((enabled, tags, description)) = current else {
            return Ok(Settle::Discard(None));
        };
        let enabled_change = change.enabled.filter(|e| *e != enabled);
        let tags_change = change.tags.clone().filter(|t| *t != tags); // clone: compared and stored
        let description_change = change
            .description
            .clone() // clone: as above
            .filter(|d| *d != description);
        if enabled_change.is_none() && tags_change.is_none() && description_change.is_none() {
            let row: RunnerRow = tx
                .step(|c| async move {
                    sqlx::query_as(&format!(
                        "SELECT {} FROM forge.ci_runners WHERE id = $1",
                        columns()
                    ))
                    .bind(id)
                    .fetch_one(c)
                    .await
                    .map_err(|e| query_error(QUERY, e))
                })
                .await?;
            return Ok(Settle::Discard(Some(runner_of(row))));
        }
        let (t, d) = (tags_change.clone(), description_change.clone()); // clone: the audit row keeps them
        let row: RunnerRow = tx
            .step(|c| async move {
                sqlx::query_as(&sql)
                    .bind(id)
                    .bind(enabled_change)
                    .bind(t)
                    .bind(d)
                    .fetch_one(c)
                    .await
                    .map_err(|e| query_error(QUERY, e))
            })
            .await?;
        let audit = RunnerChange::Updated {
            runner: id,
            enabled: enabled_change,
            tags: tags_change,
            description: description_change.is_some(),
        };
        Ok(Settle::Commit(record(&audit), Some(runner_of(row))))
    }
    .await;
    tx.settle("ci_runner.updated", outcome).await
}

/// Every runner, by id (at most [`MAX_RUNNERS`]; the read pool).
pub async fn list(db: &Db) -> Result<Vec<StoredRunner>, ForgeError> {
    const QUERY: &str = "ci_runners.list";
    let rows: Vec<RunnerRow> = db
        .within(QUERY, async {
            sqlx::query_as(&format!(
                "SELECT {} FROM forge.ci_runners ORDER BY id LIMIT $1",
                columns()
            ))
            .bind(MAX_RUNNERS)
            .fetch_all(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    Ok(rows.into_iter().map(runner_of).collect())
}

/// One runner (the read pool); `None` when absent.
pub async fn get(db: &Db, id: i64) -> Result<Option<StoredRunner>, ForgeError> {
    const QUERY: &str = "ci_runners.get";
    let row: Option<RunnerRow> = db
        .within(QUERY, async {
            sqlx::query_as(&format!(
                "SELECT {} FROM forge.ci_runners WHERE id = $1",
                columns()
            ))
            .bind(id)
            .fetch_optional(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    Ok(row.map(runner_of))
}

fn rejected(kind: CiTokenKind, reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::Unauthenticated)
        .with_context("reason", reason)
        .with_context("credential", kind.as_str())
}

/// Constant-time comparison with a stored hash; a hash that is not 32
/// bytes is `FG-DB-0019 reason=bad_hash` (the CHECK was bypassed).
fn matches(
    kind: CiTokenKind,
    id: i64,
    stored: &[u8],
    secret: &TokenSecret,
) -> Result<bool, ForgeError> {
    let stored: [u8; 32] = stored.try_into().map_err(|_| {
        ForgeError::new(ErrorCode::CredentialDataInvalid)
            .with_context("reason", "bad_hash")
            .with_context("credential", kind.as_str())
            .with_context("credential_row", id)
    })?;
    Ok(token::verify(&stored, secret))
}

/// Resolve a presented CI token (module docs). `FG-AUTH-0001` (`reason`
/// `malformed`, `unknown`, `mismatch`, `runner_disabled` — a runner token,
/// or a job token its disabled runner holds; `credential`);
/// a job token that expired or was revoked is returned with its state
/// (`authorize()` refuses it with the job's id). The runner's
/// `last_contact_at` is written at most every [`CONTACT_INTERVAL_S`]; a
/// failure of that write is logged once (warn) and authentication goes on.
pub async fn authenticate(
    db: &Db,
    request_id: &RequestId,
    presented: &str,
) -> Result<CiIdentity, ForgeError> {
    let kind = CiTokenKind::of(presented).ok_or_else(|| {
        ForgeError::new(ErrorCode::Unauthenticated).with_context("reason", "malformed")
    })?;
    let plain = CiToken::parse(kind, presented).map_err(|_| rejected(kind, "malformed"))?;
    let public_id = plain.public_id().as_str();
    db.within("ci_runners.authenticate", async {
        let mut conn = db
            .write_pool()
            .acquire()
            .await
            .map_err(|e| query_error("ci_runners.authenticate.acquire", e))?;
        match kind {
            CiTokenKind::Runner => runner_identity(&mut conn, request_id, &plain, public_id).await,
            CiTokenKind::Job => job_identity(&mut conn, &plain, public_id).await,
        }
    })
    .await
}

async fn runner_identity(
    conn: &mut PgConnection,
    request_id: &RequestId,
    plain: &CiToken,
    public_id: &str,
) -> Result<CiIdentity, ForgeError> {
    let kind = CiTokenKind::Runner;
    let row: Option<(i64, Vec<u8>, bool, bool)> = sqlx::query_as(
        "SELECT id, secret_sha256, enabled, \
           (last_contact_at IS NULL OR last_contact_at < now() - make_interval(secs => $2)) \
         FROM forge.ci_runners WHERE public_id = $1",
    )
    .bind(public_id)
    .bind(CONTACT_INTERVAL_S as f64)
    .fetch_optional(&mut *conn)
    .await
    .map_err(|e| query_error("ci_runners.authenticate.runner", e))?;
    let Some((id, hash, enabled, stale)) = row else {
        std::hint::black_box(token::verify(&DUMMY_HASH, plain.secret()));
        return Err(rejected(kind, "unknown"));
    };
    if !matches(kind, id, &hash, plain.secret())? {
        return Err(rejected(kind, "mismatch").with_context("public_id", public_id));
    }
    if !enabled {
        return Err(rejected(kind, "runner_disabled").with_context("runner_id", id));
    }
    if stale {
        touch(conn, request_id, id).await;
    }
    Ok(CiIdentity::Runner { runner_id: id })
}

/// Record the runner's contact. Losing it only makes `last_contact_at`
/// older (doctor may then count it as silent), so a failure is logged once
/// here and the request proceeds; never silent.
async fn touch(conn: &mut PgConnection, request_id: &RequestId, runner: i64) {
    let result = sqlx::query(
        "UPDATE forge.ci_runners SET last_contact_at = now() WHERE id = $1 \
         AND (last_contact_at IS NULL OR last_contact_at < now() - make_interval(secs => $2))",
    )
    .bind(runner)
    .bind(CONTACT_INTERVAL_S as f64)
    .execute(conn)
    .await;
    if let Err(e) = result {
        let e = query_error("ci_runners.authenticate.touch", e);
        tracing::warn!(
            target: "forge_db",
            event = "forge.ci.runner_contact_not_recorded",
            request_id = %request_id,
            error_code = e.code().id(),
            runner_id = runner,
            error = %e,
            "runner contact not recorded"
        );
    }
}

type JobTokenRow = (i64, Vec<u8>, i64, i64, i64, String, bool, bool, bool);

async fn job_identity(
    conn: &mut PgConnection,
    plain: &CiToken,
    public_id: &str,
) -> Result<CiIdentity, ForgeError> {
    let kind = CiTokenKind::Job;
    // The expiry is the database's clock (ADR-0029 obligation 2).
    let row: Option<JobTokenRow> = sqlx::query_as(
        "SELECT t.id, t.secret_sha256, t.job_id, t.runner_id, t.project_id, t.sha, \
           t.revoked_at IS NOT NULL, t.expires_at <= now(), r.enabled \
         FROM forge.ci_job_tokens t JOIN forge.ci_runners r ON r.id = t.runner_id \
         WHERE t.public_id = $1",
    )
    .bind(public_id)
    .fetch_optional(&mut *conn)
    .await
    .map_err(|e| query_error("ci_runners.authenticate.job", e))?;
    let Some((id, hash, job_id, runner_id, project, sha, revoked, expired, enabled)) = row else {
        std::hint::black_box(token::verify(&DUMMY_HASH, plain.secret()));
        return Err(rejected(kind, "unknown"));
    };
    if !matches(kind, id, &hash, plain.secret())? {
        return Err(rejected(kind, "mismatch").with_context("public_id", public_id));
    }
    // A disabled runner's job tokens stop with its runner token (M4 C3
    // review): disabling is the administrator's stop for that runner.
    if !enabled {
        return Err(rejected(kind, "runner_disabled")
            .with_context("runner_id", runner_id)
            .with_context("job_id", job_id));
    }
    let sha = ObjectId::parse(&sha).map_err(|_| {
        ForgeError::new(ErrorCode::CredentialDataInvalid)
            .with_context("reason", "bad_sha")
            .with_context("credential", "job")
            .with_context("job_id", job_id)
    })?;
    let state = if revoked {
        CiJobTokenState::Revoked
    } else if expired {
        CiJobTokenState::Expired
    } else {
        CiJobTokenState::Live
    };
    Ok(CiIdentity::Job {
        job_id,
        runner_id,
        project: ProjectId::new(project),
        sha,
        state,
    })
}

/// Why a job token was revoked (the column's CHECK).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Revocation {
    JobFinished,
    JobCanceled,
    LeaseLost,
    ProjectDeleted,
    ProjectMoved,
}

impl Revocation {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::JobFinished => "job_finished",
            Self::JobCanceled => "job_canceled",
            Self::LeaseLost => "lease_lost",
            Self::ProjectDeleted => "project_deleted",
            Self::ProjectMoved => "project_moved",
        }
    }
}

/// Revoke the live job tokens of `projects` on `c` — inside the caller's
/// transaction (the project's deletion or move, ADR-0029 obligation 1).
pub(crate) async fn revoke_project_job_tokens(
    c: &mut PgConnection,
    projects: &[i64],
    why: Revocation,
) -> Result<u64, ForgeError> {
    sqlx::query(
        "UPDATE forge.ci_job_tokens SET revoked_at = now(), revoked_reason = $2 \
         WHERE project_id = ANY($1) AND revoked_at IS NULL",
    )
    .bind(projects)
    .bind(why.as_str())
    .execute(c)
    .await
    .map(|r| r.rows_affected())
    .map_err(|e| query_error("ci_runners.revoke_project_tokens", e))
}

/// Revoke the live token of each job in `jobs` on `c` (inside the caller's
/// transaction: the job's end).
pub(crate) async fn revoke_job_tokens(
    c: &mut PgConnection,
    jobs: &[i64],
    why: Revocation,
) -> Result<u64, ForgeError> {
    sqlx::query(
        "UPDATE forge.ci_job_tokens SET revoked_at = now(), revoked_reason = $2 \
         WHERE job_id = ANY($1) AND revoked_at IS NULL",
    )
    .bind(jobs)
    .bind(why.as_str())
    .execute(c)
    .await
    .map(|r| r.rows_affected())
    .map_err(|e| query_error("ci_runners.revoke_job_tokens", e))
}

/// forge-web's authentication calls this for a `fgr_` / `fgj_` token.
pub(crate) fn authenticate_ci<'a>(
    db: &'a Db,
    request_id: &'a RequestId,
    presented: &'a str,
) -> forge_core::CiAuthFuture<'a> {
    Box::pin(authenticate(db, request_id, presented))
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
