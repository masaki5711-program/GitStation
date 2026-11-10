//! Check runs (M3 C1, ADR-0024 §10, §11; migration 20261115100000): one
//! row per (project, commit, name), its annotations, and the namespace
//! grants of service accounts.
//!
//! - [`report_check_run_decided`]: a report, decided inside its own
//!   transaction (forge-checks reconfirms `authorize()` on the locked
//!   facts). Reporting a name again updates its row; a new name beyond
//!   [`MAX_CHECK_RUNS_PER_COMMIT`] is `FG-REVIEW-0021`. All reports of one
//!   commit serialize on a transaction-scoped advisory lock, so two
//!   concurrent reports of one name leave one row and the cap holds.
//!   `check.completed` is published only when the conclusion changed
//!   (dedup key `check_completed:<id>:<revision>`); every other report is
//!   one `check_run.reported` audit row.
//! - [`runs_on`] (inside a transaction: the merge gate) and the
//!   [`forge_core::CheckResultsSource`] impl on [`Db`] (the read pool: the
//!   merge status) run the same SQL ([`RUNS_SQL`]). A row this build cannot
//!   read, or more than the cap, is `FG-DB-0022` CheckRunDataInvalid —
//!   never a shorter snapshot.
//! - [`check_runs`]: the runs with their annotations (`GET …/checks`).
//! - [`grant_check_namespace_decided`] / [`revoke_check_namespace_decided`]:
//!   an administrator's grant of a namespace on one project (audit only, on
//!   the project); [`grants_of`], [`granted`], [`reported_namespaces`] (the
//!   namespaces some active service account may report on a project),
//!   [`reporters`] and [`unreported_requirements`] (doctor
//!   `checks.reporters`).

use std::collections::BTreeMap;

use forge_core::checks::{
    CheckAnnotationLevel, CheckConclusion, CheckRun, CheckRunChange, CheckRunsOnHead, CheckStatus,
    MAX_CHECK_RUNS_PER_COMMIT, MAX_STORED_CHECK_RUNS_PER_COMMIT,
};
use forge_core::{
    AuditDetails, CheckName, CheckNamespace, DedupKey, DedupPart, ObjectId, ProjectId, UserId,
};
use forge_errors::{ErrorCode, ForgeError};
use sqlx::postgres::PgConnection;

use crate::access::{lock_account, Account};
use crate::change::{ChangeContext, ChangeRecord, ChangeTx, Settle};
use crate::projects::{evented_record, lock_shared, locked_access_facts, ProjectAccessFacts};
use crate::{query_error, read_query_error, Db};

const TS: &str = "'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"'";

/// One annotation of a report (validated by forge-checks; the table's
/// CHECKs are the backstop).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckAnnotation {
    pub path: String,
    pub start_line: u32,
    pub end_line: u32,
    pub level: CheckAnnotationLevel,
    pub message: String,
    pub title: Option<String>,
}

/// A report of one check run on one commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewCheckRun {
    pub name: CheckName,
    pub status: CheckStatus,
    /// `Some` exactly when `completed` (forge-checks checks it).
    pub conclusion: Option<CheckConclusion>,
    pub summary: String,
    pub details_url: Option<String>,
    pub annotations: Vec<CheckAnnotation>,
}

/// A check run as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredCheckRun {
    pub id: i64,
    pub head_sha: ObjectId,
    pub name: CheckName,
    pub status: CheckStatus,
    pub conclusion: Option<CheckConclusion>,
    pub summary: String,
    pub details_url: Option<String>,
    /// The account that reported it last; `None` exactly for a `forge/`
    /// run (forge's own worker, M3 X2: `check_runs_reporter`).
    pub reported_by: Option<UserId>,
    /// The reporter's username (`None` for a `forge/` run).
    pub reported_by_username: Option<String>,
    pub revision: i64,
    /// RFC 3339 UTC with microseconds.
    pub created_at: String,
    pub updated_at: String,
    pub completed_at: Option<String>,
    pub annotations: Vec<CheckAnnotation>,
}

/// What `decide` sees for a report: the reporter's account, the project's
/// access facts and roles (all locked), and whether the reporter holds a
/// grant of the name's namespace (read `FOR SHARE`, so a concurrent revoke
/// waits; always `false` for `forge/` and `ext/`, which no grant names).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckReportFacts {
    pub account: Account,
    pub project: ProjectAccessFacts,
    pub granted: bool,
}

/// What a report did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reported {
    pub run: StoredCheckRun,
    /// The report created the run (else it updated it).
    pub created: bool,
    /// `check.completed` was published (the conclusion changed).
    pub completed_event: bool,
}

/// The token or session a report was made with (its id only).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReportCredential(pub Option<i64>);

fn data_invalid(reason: &'static str, project: ProjectId, head: &str) -> ForgeError {
    ForgeError::new(ErrorCode::CheckRunDataInvalid)
        .with_context("reason", reason)
        .with_context("project_id", project.get())
        .with_context("head_sha", head.to_owned())
}

/// The grantable namespaces (`check_namespace_grants_namespace`).
pub fn grantable(namespace: CheckNamespace) -> bool {
    match namespace {
        CheckNamespace::Ci | CheckNamespace::Kanflow => true,
        CheckNamespace::Forge | CheckNamespace::Ext => false,
    }
}

/// The same statement for the merge gate (inside its transaction) and the
/// merge status (the read pool): every run of `$2` in project `$1`, one
/// more than the cap to see a breach.
pub(crate) const RUNS_SQL: &str = "SELECT id, name, status, conclusion FROM forge.check_runs \
     WHERE project_id = $1 AND head_sha = $2 ORDER BY name LIMIT $3";

fn row_limit() -> i64 {
    i64::try_from(MAX_STORED_CHECK_RUNS_PER_COMMIT)
        .unwrap_or(i64::MAX)
        .saturating_add(1)
}

/// A stored status / conclusion pair, `FG-DB-0022 stored_status_invalid`
/// unless it is one the CHECKs allow.
fn parse_run(
    project: ProjectId,
    head: &ObjectId,
    id: i64,
    status: &str,
    conclusion: Option<&str>,
) -> Result<CheckRun, ForgeError> {
    let bad = || {
        data_invalid("stored_status_invalid", project, head.as_str())
            .with_context("check_run_id", id)
    };
    let status = CheckStatus::parse(status).ok_or_else(bad)?;
    let conclusion = conclusion
        .map(|c| CheckConclusion::parse(c).ok_or_else(bad))
        .transpose()?;
    CheckRun::new(status, conclusion).ok_or_else(bad)
}

fn parse_name(
    project: ProjectId,
    head: &ObjectId,
    id: i64,
    raw: &str,
) -> Result<CheckName, ForgeError> {
    CheckName::parse(raw).map_err(|_| {
        data_invalid("stored_name_invalid", project, head.as_str()).with_context("check_run_id", id)
    })
}

type RunRow = (i64, String, String, Option<String>);

fn snapshot(
    project: ProjectId,
    head: &ObjectId,
    rows: Vec<RunRow>,
) -> Result<CheckRunsOnHead, ForgeError> {
    if rows.len() > MAX_STORED_CHECK_RUNS_PER_COMMIT {
        return Err(data_invalid("too_many_stored_runs", project, head.as_str())
            .with_context("limit", MAX_STORED_CHECK_RUNS_PER_COMMIT));
    }
    let mut runs = Vec::with_capacity(rows.len());
    for (id, name, status, conclusion) in rows {
        let name = parse_name(project, head, id, &name)?;
        let run = parse_run(project, head, id, &status, conclusion.as_deref())?;
        runs.push((name, run));
    }
    Ok(CheckRunsOnHead::new(head.clone(), runs)) // clone: the snapshot owns its head
}

/// Every run of `head` in `project`, inside a transaction (the merge gate,
/// under the merge request's locks).
pub(crate) async fn runs_on(
    conn: &mut PgConnection,
    project: ProjectId,
    head: &ObjectId,
) -> Result<CheckRunsOnHead, ForgeError> {
    let rows: Vec<RunRow> = sqlx::query_as(RUNS_SQL)
        .bind(project.get())
        .bind(head.as_str())
        .bind(row_limit())
        .fetch_all(conn)
        .await
        .map_err(|e| query_error("checks.runs_on", e))?;
    snapshot(project, head, rows)
}

/// [`runs_on`] on the read pool (the merge status).
pub async fn runs_for(
    db: &Db,
    project: ProjectId,
    head: &ObjectId,
) -> Result<CheckRunsOnHead, ForgeError> {
    const QUERY: &str = "checks.runs_read";
    let rows: Vec<RunRow> = db
        .within(QUERY, async {
            sqlx::query_as(RUNS_SQL)
                .bind(project.get())
                .bind(head.as_str())
                .bind(row_limit())
                .fetch_all(db.pool().executor())
                .await
                .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    snapshot(project, head, rows)
}

/// The merge status's source (only `forge serve` injects the
/// `Db`): exactly [`runs_for`].
impl forge_core::CheckResultsSource for Db {
    fn runs_on<'a>(
        &'a self,
        project: ProjectId,
        head: &'a ObjectId,
    ) -> forge_core::namespace_resolver::LookupFuture<'a, CheckRunsOnHead> {
        Box::pin(runs_for(self, project, head))
    }
}

type FullRow = (
    i64,
    String,
    String,
    Option<String>,
    String,
    Option<String>,
    Option<i64>,
    Option<String>,
    i64,
    String,
    String,
    Option<String>,
);

fn full_columns() -> String {
    format!(
        "r.id, r.name, r.status, r.conclusion, r.summary, r.details_url, r.reported_by, \
         u.username, r.revision, to_char(r.created_at AT TIME ZONE 'UTC', {TS}), \
         to_char(r.updated_at AT TIME ZONE 'UTC', {TS}), \
         to_char(r.completed_at AT TIME ZONE 'UTC', {TS})"
    )
}

type AnnotationRow = (i64, String, i32, i32, String, String, Option<String>);

const ANNOTATIONS_SQL: &str = "SELECT check_run_id, path, start_line, end_line, level, message, \
     title FROM forge.check_annotations WHERE check_run_id = ANY($1) \
     ORDER BY check_run_id, position";

fn parse_annotations(
    project: ProjectId,
    head: &ObjectId,
    rows: Vec<AnnotationRow>,
) -> Result<BTreeMap<i64, Vec<CheckAnnotation>>, ForgeError> {
    let mut out: BTreeMap<i64, Vec<CheckAnnotation>> = BTreeMap::new();
    for (run, path, start, end, level, message, title) in rows {
        let bad = || {
            data_invalid("stored_annotation_invalid", project, head.as_str())
                .with_context("check_run_id", run)
        };
        let level = CheckAnnotationLevel::parse(&level).ok_or_else(bad)?;
        let start_line = u32::try_from(start).map_err(|_| bad())?;
        let end_line = u32::try_from(end).map_err(|_| bad())?;
        if start_line == 0 || end_line < start_line {
            return Err(bad());
        }
        out.entry(run).or_default().push(CheckAnnotation {
            path,
            start_line,
            end_line,
            level,
            message,
            title,
        });
    }
    Ok(out)
}

fn parse_full(
    project: ProjectId,
    head: &ObjectId,
    row: FullRow,
    annotations: &mut BTreeMap<i64, Vec<CheckAnnotation>>,
) -> Result<StoredCheckRun, ForgeError> {
    let (
        id,
        name,
        status,
        conclusion,
        summary,
        details_url,
        reported_by,
        username,
        revision,
        created_at,
        updated_at,
        completed_at,
    ) = row;
    let name = parse_name(project, head, id, &name)?;
    let run = parse_run(project, head, id, &status, conclusion.as_deref())?;
    Ok(StoredCheckRun {
        id,
        head_sha: head.clone(), // clone: each run names its commit
        name,
        status: run.status,
        conclusion: run.conclusion,
        summary,
        details_url,
        reported_by: reported_by.map(UserId::new),
        reported_by_username: username,
        revision,
        created_at,
        updated_at,
        completed_at,
        annotations: annotations.remove(&id).unwrap_or_default(),
    })
}

/// Every run of `head` in `project` with its annotations, by name (the
/// read pool; `GET …/commits/{sha}/checks`). `FG-DB-0022` for a row this
/// build cannot read or more than the cap.
pub async fn check_runs(
    db: &Db,
    project: ProjectId,
    head: &ObjectId,
) -> Result<Vec<StoredCheckRun>, ForgeError> {
    const QUERY: &str = "checks.list";
    let sql = format!(
        "SELECT {} FROM forge.check_runs r LEFT JOIN forge.users u ON u.id = r.reported_by \
         WHERE r.project_id = $1 AND r.head_sha = $2 ORDER BY r.name LIMIT $3",
        full_columns()
    );
    let (rows, notes): (Vec<FullRow>, Vec<AnnotationRow>) = db
        .within(QUERY, async {
            let rows: Vec<FullRow> = sqlx::query_as(&sql)
                .bind(project.get())
                .bind(head.as_str())
                .bind(row_limit())
                .fetch_all(db.pool().executor())
                .await
                .map_err(|e| read_query_error(QUERY, e))?;
            let ids: Vec<i64> = rows.iter().map(|r| r.0).collect();
            let notes: Vec<AnnotationRow> = sqlx::query_as(ANNOTATIONS_SQL)
                .bind(&ids)
                .fetch_all(db.pool().executor())
                .await
                .map_err(|e| read_query_error(QUERY, e))?;
            Ok((rows, notes))
        })
        .await?;
    if rows.len() > MAX_STORED_CHECK_RUNS_PER_COMMIT {
        return Err(data_invalid("too_many_stored_runs", project, head.as_str())
            .with_context("limit", MAX_STORED_CHECK_RUNS_PER_COMMIT));
    }
    let mut annotations = parse_annotations(project, head, notes)?;
    rows.into_iter()
        .map(|row| parse_full(project, head, row, &mut annotations))
        .collect()
}

/// Whether `user` holds a grant of `namespace` for `project`, `FOR SHARE`
/// (a grant is for one project: review fix 1).
async fn granted_on(
    conn: &mut PgConnection,
    user: UserId,
    project: ProjectId,
    namespace: CheckNamespace,
) -> Result<bool, ForgeError> {
    if !grantable(namespace) {
        return Ok(false);
    }
    let row: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM forge.check_namespace_grants WHERE user_id = $1 AND project_id = $2 \
         AND namespace = $3 FOR SHARE",
    )
    .bind(user.get())
    .bind(project.get())
    .bind(namespace.as_str())
    .fetch_optional(conn)
    .await
    .map_err(|e| query_error("checks.grant_lock", e))?;
    Ok(row.is_some())
}

fn limit_reached(project: ProjectId, head: &ObjectId) -> ForgeError {
    ForgeError::new(ErrorCode::CheckRunLimitReached)
        .with_context("reason", "too_many_runs")
        .with_context("limit", MAX_CHECK_RUNS_PER_COMMIT)
        .with_context("project_id", project.get())
        .with_context("head_sha", head.as_str().to_owned())
        .with_context("action", "CheckWrite")
}

/// Record a report of `new` on `head` of `project` by `reporter`, after
/// `decide` (forge-checks: `Authorized::reconfirm` on the locked facts)
/// allowed it.
///
/// Lock order (review fix 3): the project row `FOR SHARE` (it blocks a
/// purge, a deletion or a visibility change until the report commits, but
/// not other reports — reports of other commits run in parallel), then the
/// reporter's roles and account (`FOR SHARE`), then the grant (`FOR SHARE`,
/// a revoke waits), then the commit's transaction-scoped advisory lock
/// (all reports of one commit serialize on it: the name's row and the
/// 100-names cap are decided under it), then the run's row `FOR UPDATE`.
/// The merge gate takes the project row `FOR UPDATE` first: a report in
/// flight finishes before a gate reads the runs, and a report that starts
/// during a gate waits for the gate to commit, so the gate always reads
/// committed runs and the two never deadlock (both take the project row
/// first). Annotations are
/// replaced as a whole. Errors (nothing written): `decide`'s, `FG-REPO-0005
/// resource=project`, `FG-REVIEW-0021` for a new name beyond the cap,
/// `FG-DB-*`.
pub async fn report_check_run_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    (project, head): (ProjectId, &ObjectId),
    reporter: UserId,
    credential: ReportCredential,
    new: &NewCheckRun,
    decide: impl FnOnce(&CheckReportFacts) -> Result<P, ForgeError>,
) -> Result<(Reported, P), ForgeError> {
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let locked = lock_shared(&mut tx, project).await?;
        let caller = locked_access_facts(&mut tx, project, reporter, &locked).await?;
        let namespace = new.name.namespace();
        let granted = tx
            .step(|c| async move { granted_on(c, reporter, project, namespace).await })
            .await?;
        let decided = decide(&CheckReportFacts {
            account: caller.account,
            project: caller.project,
            granted,
        })?;
        let by = Some((reporter, credential));
        let (reported, record) = write_run(&mut tx, ctx, (project, head), by, new).await?;
        Ok(Settle::Commit(record, (reported, decided)))
    }
    .await;
    tx.settle(REPORTED_ACTION, outcome).await
}

/// The audit action of a report that left the conclusion as it was.
const REPORTED_ACTION: &str = "check_run.reported";

/// Record forge's own run `new` on `head` of `project` (M3 X2: the
/// `check.spec_drift` job, through forge-checks; M4 C5: `ci/pipeline`,
/// through forge-pipelines). No decision: forge's worker is no account and
/// these names are never written over HTTP; the run has no reporter
/// (`check_runs_reporter`). Only a name of
/// [`forge_core::checks::SYSTEM_REPORTED_CHECKS`] is accepted
/// (`FG-SYS-0001 reason=not_forge_check` otherwise: a caller bug). The
/// project row is taken `FOR SHARE` and the rest is a report's (lock order,
/// the cap, `check.completed` only when the conclusion changed). Errors
/// (nothing written): `FG-REPO-0005 resource=project` (a purged project),
/// `FG-REVIEW-0021`, `FG-DB-*`.
pub async fn record_forge_check_run(
    db: &Db,
    ctx: &ChangeContext,
    (project, head): (ProjectId, &ObjectId),
    new: &NewCheckRun,
) -> Result<Reported, ForgeError> {
    if !forge_core::checks::system_reports(&new.name) {
        return Err(ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "not_forge_check")
            .with_context("name", new.name.as_str().to_owned()));
    }
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let _locked = lock_shared(&mut tx, project).await?;
        let (reported, record) = write_run(&mut tx, ctx, (project, head), None, new).await?;
        Ok(Settle::Commit(record, reported))
    }
    .await;
    tx.settle(REPORTED_ACTION, outcome).await
}

/// The write shared by a report and forge's own run, under the project
/// row lock its caller took: the commit's advisory lock, the run's row
/// (the cap for a new name), its annotations, the audit record and the
/// row as read back. `by`: the reporting account and its credential
/// (`None`: forge's worker).
async fn write_run(
    tx: &mut ChangeTx,
    ctx: &ChangeContext,
    (project, head): (ProjectId, &ObjectId),
    by: Option<(UserId, ReportCredential)>,
    new: &NewCheckRun,
) -> Result<(Reported, ChangeRecord), ForgeError> {
    let reporter = by.map(|(u, _)| u.get());
    let credential = by.and_then(|(_, c)| c.0);
    let sha = head.as_str().to_owned();
    let name = new.name.as_str().to_owned();
    let key = format!("forge.check_runs:{}:{}", project.get(), sha);
    tx.step(|c| async move {
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(key)
            .execute(c)
            .await
            .map_err(|e| query_error("checks.commit_lock", e))
    })
    .await?;
    let (s, n) = (sha.clone(), name.clone()); // clone: the step owns its binds
    let existing: Option<(i64, Option<String>)> = tx
        .step(|c| async move {
            sqlx::query_as(
                "SELECT id, conclusion FROM forge.check_runs WHERE project_id = $1 \
             AND head_sha = $2 AND name = $3 FOR UPDATE",
            )
            .bind(project.get())
            .bind(s)
            .bind(n)
            .fetch_optional(c)
            .await
            .map_err(|e| query_error("checks.run_lock", e))
        })
        .await?;
    let status = new.status.as_str();
    let conclusion = new.conclusion.map(CheckConclusion::as_str);
    let summary = new.summary.clone(); // clone: the step owns its binds
    let url = new.details_url.clone(); // clone: as above
    let request_id = ctx.request_id.as_str().to_owned();
    let (id, revision, previous, created) = match existing {
        None => {
            let s = sha.clone(); // clone: the step owns its binds
            let count: i64 = tx
                .step(|c| async move {
                    sqlx::query_scalar(
                        "SELECT count(*) FROM forge.check_runs WHERE project_id = $1 \
                         AND head_sha = $2 AND name <> ALL($3)",
                    )
                    .bind(project.get())
                    .bind(s)
                    .bind(forge_core::checks::SYSTEM_REPORTED_CHECKS)
                    .fetch_one(c)
                    .await
                    .map_err(|e| query_error("checks.run_count", e))
                })
                .await?;
            // Review X2: forge's own names (one run each, the list is fixed;
            // M4 C5 `ci/pipeline` too) are outside the reporters' cap and
            // never counted in it.
            let forge_name = forge_core::checks::system_reports(&new.name);
            if !forge_name
                && usize::try_from(count).unwrap_or(usize::MAX) >= MAX_CHECK_RUNS_PER_COMMIT
            {
                return Err(limit_reached(project, head));
            }
            let (s, n) = (sha.clone(), name.clone()); // clone: the step owns its binds
            let id: i64 = tx
                .step(|c| async move {
                    sqlx::query_scalar(
                        "INSERT INTO forge.check_runs (project_id, head_sha, name, status, \
                 conclusion, summary, details_url, reported_by, credential_id, \
                 request_id, completed_at) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, \
                 $9, $10, CASE WHEN $4 = 'completed' THEN now() END) RETURNING id",
                    )
                    .bind(project.get())
                    .bind(s)
                    .bind(n)
                    .bind(status)
                    .bind(conclusion)
                    .bind(summary)
                    .bind(url)
                    .bind(reporter)
                    .bind(credential)
                    .bind(request_id)
                    .fetch_one(c)
                    .await
                    .map_err(|e| query_error("checks.run_insert", e))
                })
                .await?;
            (id, 1_i64, None, true)
        }
        Some((id, previous)) => {
            let previous = previous
                .as_deref()
                .map(|c| {
                    CheckConclusion::parse(c).ok_or_else(|| {
                        data_invalid("stored_status_invalid", project, head.as_str())
                            .with_context("check_run_id", id)
                    })
                })
                .transpose()?;
            let revision: i64 = tx
                .step(|c| async move {
                    sqlx::query_scalar(
                        "UPDATE forge.check_runs SET status = $2, conclusion = $3, \
                 summary = $4, details_url = $5, reported_by = $6, \
                 credential_id = $7, request_id = $8, revision = revision + 1, \
                 updated_at = now(), \
                 completed_at = CASE WHEN $2 = 'completed' THEN \
                 CASE WHEN status = 'completed' AND conclusion IS NOT DISTINCT FROM $3 \
                 THEN completed_at ELSE now() END END \
                 WHERE id = $1 RETURNING revision",
                    )
                    .bind(id)
                    .bind(status)
                    .bind(conclusion)
                    .bind(summary)
                    .bind(url)
                    .bind(reporter)
                    .bind(credential)
                    .bind(request_id)
                    .fetch_one(c)
                    .await
                    .map_err(|e| query_error("checks.run_update", e))
                })
                .await?;
            tx.step(|c| async move {
                sqlx::query("DELETE FROM forge.check_annotations WHERE check_run_id = $1")
                    .bind(id)
                    .execute(c)
                    .await
                    .map_err(|e| query_error("checks.annotations_clear", e))
            })
            .await?;
            (id, revision, previous, false)
        }
    };
    if !new.annotations.is_empty() {
        let mut positions = Vec::with_capacity(new.annotations.len());
        let mut paths = Vec::with_capacity(new.annotations.len());
        let mut starts = Vec::with_capacity(new.annotations.len());
        let mut ends = Vec::with_capacity(new.annotations.len());
        let mut levels = Vec::with_capacity(new.annotations.len());
        let mut messages = Vec::with_capacity(new.annotations.len());
        let mut titles = Vec::with_capacity(new.annotations.len());
        // forge-checks keeps 50 annotations at most and lines within
        // i32 (FG-REVIEW-0020); a value beyond is a broken precondition,
        // never written saturated (review fix 8).
        let out_of_range = |field: &'static str| {
            ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "annotation_out_of_range")
                .with_context("field", field)
        };
        for (i, a) in new.annotations.iter().enumerate() {
            positions.push(i16::try_from(i).map_err(|_| out_of_range("position"))?);
            paths.push(a.path.clone()); // clone: the arrays own their values
            starts.push(i32::try_from(a.start_line).map_err(|_| out_of_range("start_line"))?);
            ends.push(i32::try_from(a.end_line).map_err(|_| out_of_range("end_line"))?);
            levels.push(a.level.as_str());
            messages.push(a.message.clone()); // clone: as above
            titles.push(a.title.clone()); // clone: as above
        }
        tx.step(|c| async move {
            sqlx::query(
                "INSERT INTO forge.check_annotations (check_run_id, project_id, position, \
             path, start_line, end_line, level, message, title) \
             SELECT $1, $2, * FROM unnest($3::smallint[], $4::text[], $5::int[], \
             $6::int[], $7::text[], $8::text[], $9::text[])",
            )
            .bind(id)
            .bind(project.get())
            .bind(positions)
            .bind(paths)
            .bind(starts)
            .bind(ends)
            .bind(levels)
            .bind(messages)
            .bind(titles)
            .execute(c)
            .await
            .map_err(|e| query_error("checks.annotations_insert", e))
        })
        .await?;
    }
    let change = CheckRunChange {
        project,
        check_run_id: id,
        head_sha: head.clone(), // clone: the audit details own their commit
        name: new.name.clone(), // clone: as above
        status: new.status,
        conclusion: new.conclusion,
        previous_conclusion: previous,
        created,
        revision,
    };
    let completed_event = change.completed();
    let details = AuditDetails::CheckRun(change);
    let record = if completed_event {
        let mut record = evented_record(&details, project)?;
        let dedup = DedupKey::new(
            "check_completed",
            &[DedupPart::Id(id), DedupPart::Id(revision)],
        )?;
        for event in &mut record.events {
            event.dedup_key = Some(dedup.clone()); // clone: one key for the one event
        }
        record
    } else {
        project_audit_only(project, &details)
    };
    let sql = format!(
        "SELECT {} FROM forge.check_runs r LEFT JOIN forge.users u ON u.id = r.reported_by \
         WHERE r.id = $1",
        full_columns()
    );
    let row: FullRow = tx
        .step(|c| async move {
            sqlx::query_as(&sql)
                .bind(id)
                .fetch_one(c)
                .await
                .map_err(|e| query_error("checks.run_read", e))
        })
        .await?;
    let notes: Vec<AnnotationRow> = tx
        .step(|c| async move {
            sqlx::query_as(ANNOTATIONS_SQL)
                .bind(vec![id])
                .fetch_all(c)
                .await
                .map_err(|e| query_error("checks.annotations_read", e))
        })
        .await?;
    let mut annotations = parse_annotations(project, head, notes)?;
    let run = parse_full(project, head, row, &mut annotations)?;
    Ok((
        Reported {
            run,
            created,
            completed_event,
        },
        record,
    ))
}

fn project_audit_only(project: ProjectId, details: &AuditDetails) -> ChangeRecord {
    let mut record = ChangeRecord::audit_only(details, "project", project.get());
    record.audit.project_id = Some(project);
    record
}

/// What `decide` sees for a grant change: the administrator's account
/// (locked) and the target's kind and state (locked `FOR SHARE`; `None`
/// when no such user exists). The project is locked `FOR SHARE` before
/// (absent: `FG-REPO-0005 resource=project`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GrantFacts {
    pub account: Account,
    pub target: Option<crate::service_accounts::Target>,
}

/// What a grant change did.
#[must_use = "Unchanged wrote nothing; callers report it differently"]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantOutcome {
    /// One `check_namespace.granted` / `.revoked` audit row.
    Changed,
    /// The grant already was (or was not) there: nothing written.
    Unchanged,
}

/// One grant: a namespace a service account reports on one project.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Grant {
    pub project: ProjectId,
    pub namespace: CheckNamespace,
}

fn not_grantable(namespace: CheckNamespace) -> ForgeError {
    ForgeError::new(ErrorCode::CheckPayloadInvalid)
        .with_context("reason", "namespace_not_grantable")
        .with_context("namespace", namespace.as_str())
}

async fn lock_target(
    tx: &mut ChangeTx,
    user: UserId,
) -> Result<Option<crate::service_accounts::Target>, ForgeError> {
    let row: Option<(String, String)> = tx
        .step(|c| async move {
            sqlx::query_as("SELECT kind, state FROM forge.users WHERE id = $1 FOR SHARE")
                .bind(user.get())
                .fetch_optional(c)
                .await
                .map_err(|e| query_error("checks.grant_target", e))
        })
        .await?;
    row.map(|(kind, state)| {
        let kind = forge_core::UserKind::parse(&kind).ok_or_else(|| {
            ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "unknown_user_kind")
                .with_context("user_id", user.get())
        })?;
        Ok(crate::service_accounts::Target {
            kind,
            active: state == "active",
        })
    })
    .transpose()
}

/// `FG-REPO-0005 resource=project` unless `project` exists, the row locked
/// `FOR SHARE` (a purge waits for the grant). `true` when the project is
/// pending deletion (`deleted_at` set).
async fn lock_project_shared(tx: &mut ChangeTx, project: ProjectId) -> Result<bool, ForgeError> {
    let row: Option<bool> = tx
        .step(|c| async move {
            sqlx::query_scalar(
                "SELECT deleted_at IS NOT NULL FROM forge.projects WHERE id = $1 FOR SHARE",
            )
            .bind(project.get())
            .fetch_optional(c)
            .await
            .map_err(|e| query_error("checks.grant_project", e))
        })
        .await?;
    match row {
        Some(deleted) => Ok(deleted),
        None => Err(ForgeError::new(ErrorCode::NotFound)
            .with_context("resource", "project")
            .with_context("reason", "absent")),
    }
}

/// Grant (`grant = true`) or revoke `namespace` on `project` to service
/// account `user` for administrator `admin`, after `decide` (forge-checks:
/// `Authorized::reconfirm`) allowed it on the locked facts. A grant is for
/// one project only (review fix 1: a grant for every project let any
/// project's Maintainer borrow another team's reporter). `ci` and `kanflow`
/// only (`FG-REVIEW-0020 reason=namespace_not_grantable` otherwise, before
/// anything is locked). The table's guard refuses a grant to any account
/// that is not a service account. Audited on the project.
pub async fn change_check_namespace_grant_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    (user, project, namespace, grant): (UserId, ProjectId, CheckNamespace, bool),
    admin: UserId,
    decide: impl FnOnce(&GrantFacts) -> Result<P, ForgeError>,
) -> Result<(GrantOutcome, P), ForgeError> {
    if !grantable(namespace) {
        return Err(not_grantable(namespace));
    }
    let action = if grant {
        "check_namespace.granted"
    } else {
        "check_namespace.revoked"
    };
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let pending_deletion = lock_project_shared(&mut tx, project).await?;
        let target = lock_target(&mut tx, user).await?;
        let account = tx.step(|c| lock_account(c, admin)).await?;
        let decided = decide(&GrantFacts { account, target })?;
        // Re-review (LOW): no new reporter for a project pending deletion
        // (a revoke still removes one).
        if grant && pending_deletion {
            return Err(ForgeError::new(ErrorCode::PendingDeletion)
                .with_context("reason", "project_pending_deletion")
                .with_context("project_id", project.get())
                .with_context("action", "CheckNamespaceGrantManage"));
        }
        let changed: u64 = if grant {
            tx.step(|c| async move {
                sqlx::query(
                    "INSERT INTO forge.check_namespace_grants (user_id, project_id, namespace, \
                     granted_by) VALUES ($1, $2, $3, $4) ON CONFLICT DO NOTHING",
                )
                .bind(user.get())
                .bind(project.get())
                .bind(namespace.as_str())
                .bind(admin.get())
                .execute(c)
                .await
                .map(|r| r.rows_affected())
                .map_err(|e| query_error("checks.grant", e))
            })
            .await?
        } else {
            tx.step(|c| async move {
                sqlx::query(
                    "DELETE FROM forge.check_namespace_grants WHERE user_id = $1 \
                     AND project_id = $2 AND namespace = $3",
                )
                .bind(user.get())
                .bind(project.get())
                .bind(namespace.as_str())
                .execute(c)
                .await
                .map(|r| r.rows_affected())
                .map_err(|e| query_error("checks.revoke", e))
            })
            .await?
        };
        if changed == 0 {
            return Ok(Settle::Discard((GrantOutcome::Unchanged, decided)));
        }
        let details = AuditDetails::CheckNamespaceGrant {
            user,
            project,
            namespace,
            granted: grant,
        };
        Ok(Settle::Commit(
            project_audit_only(project, &details),
            (GrantOutcome::Changed, decided),
        ))
    }
    .await;
    tx.settle(action, outcome).await
}

/// The grants `user` holds (the read pool), by project then namespace.
pub async fn grants_of(db: &Db, user: UserId) -> Result<Vec<Grant>, ForgeError> {
    const QUERY: &str = "checks.grants_of";
    let rows: Vec<(i64, String)> = db
        .within(QUERY, async {
            sqlx::query_as(
                "SELECT project_id, namespace FROM forge.check_namespace_grants \
                 WHERE user_id = $1 ORDER BY project_id, namespace",
            )
            .bind(user.get())
            .fetch_all(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    rows.into_iter()
        .map(|(project, raw)| {
            namespace_of(&raw)
                .map(|namespace| Grant {
                    project: ProjectId::new(project),
                    namespace,
                })
                .ok_or_else(|| unknown_grant(&raw))
        })
        .collect()
}

/// Whether `user` holds a grant of `namespace` on `project` now (the read
/// pool; the report route's loader — the write reads it again under the
/// lock).
pub async fn granted(
    db: &Db,
    user: UserId,
    project: ProjectId,
    namespace: CheckNamespace,
) -> Result<bool, ForgeError> {
    if !grantable(namespace) {
        return Ok(false);
    }
    const QUERY: &str = "checks.granted";
    let row: Option<i32> = db
        .within(QUERY, async {
            sqlx::query_scalar(
                "SELECT 1 FROM forge.check_namespace_grants WHERE user_id = $1 \
                 AND project_id = $2 AND namespace = $3",
            )
            .bind(user.get())
            .bind(project.get())
            .bind(namespace.as_str())
            .fetch_optional(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    Ok(row.is_some())
}

fn namespace_of(raw: &str) -> Option<CheckNamespace> {
    CheckNamespace::ALL.into_iter().find(|n| n.as_str() == raw)
}

fn unknown_grant(raw: &str) -> ForgeError {
    ForgeError::new(ErrorCode::Internal)
        .with_context("reason", "unknown_grant_namespace")
        .with_context("raw", raw.chars().take(32).collect::<String>())
}

fn by_namespace(rows: Vec<(String, i64)>) -> Result<BTreeMap<CheckNamespace, i64>, ForgeError> {
    rows.into_iter()
        .map(|(raw, n)| {
            namespace_of(&raw)
                .map(|ns| (ns, n))
                .ok_or_else(|| unknown_grant(&raw))
        })
        .collect()
}

/// How many grants of each namespace active service accounts hold, over
/// all projects (doctor `checks.reporters`; a namespace without one is
/// absent).
pub async fn reporters(db: &Db) -> Result<BTreeMap<CheckNamespace, i64>, ForgeError> {
    const QUERY: &str = "checks.reporters";
    let rows: Vec<(String, i64)> = db
        .within(QUERY, async {
            sqlx::query_as(
                "SELECT g.namespace, count(*) FROM forge.check_namespace_grants g \
                 JOIN forge.users u ON u.id = g.user_id \
                 WHERE u.state = 'active' AND u.kind = 'service_account' \
                 GROUP BY g.namespace ORDER BY g.namespace",
            )
            .fetch_all(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    by_namespace(rows)
}

/// How many protected ref merge rules require a check of each namespace
/// that no active service account may report on the rule's own project
/// (doctor `checks.reporters`; empty: every required check has a reporter).
/// The names forge's own worker reports (M3 X2: `forge/spec-drift`) have
/// a reporter.
pub async fn unreported_requirements(db: &Db) -> Result<BTreeMap<CheckNamespace, i64>, ForgeError> {
    const QUERY: &str = "checks.unreported_requirements";
    let rows: Vec<(String, i64)> = db
        .within(QUERY, async {
            sqlx::query_as(
                "SELECT split_part(c.name, '/', 1), count(DISTINCT r.protected_ref_id) \
                 FROM forge.protected_ref_merge_rules r, unnest(r.required_checks) AS c(name) \
                 WHERE c.name <> ALL($1) \
                 AND NOT EXISTS (SELECT 1 FROM forge.check_namespace_grants g \
                 JOIN forge.users u ON u.id = g.user_id \
                 WHERE g.project_id = r.project_id \
                 AND g.namespace = split_part(c.name, '/', 1) \
                 AND u.state = 'active' AND u.kind = 'service_account') \
                 GROUP BY 1 ORDER BY 1",
            )
            .bind(forge_core::checks::SYSTEM_REPORTED_CHECKS)
            .fetch_all(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    by_namespace(rows)
}

/// The namespaces some active service account may report on `project`
/// (the merge rule's `unreportable_checks`: a required name outside them
/// has no granted reporter there).
pub async fn reported_namespaces(
    db: &Db,
    project: ProjectId,
) -> Result<Vec<CheckNamespace>, ForgeError> {
    const QUERY: &str = "checks.reported_namespaces";
    let rows: Vec<String> = db
        .within(QUERY, async {
            sqlx::query_scalar(
                "SELECT DISTINCT g.namespace FROM forge.check_namespace_grants g \
                 JOIN forge.users u ON u.id = g.user_id \
                 WHERE g.project_id = $1 AND u.state = 'active' \
                 AND u.kind = 'service_account' ORDER BY 1",
            )
            .bind(project.get())
            .fetch_all(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    rows.iter()
        .map(|raw| namespace_of(raw).ok_or_else(|| unknown_grant(raw)))
        .collect()
}

mod retention;
pub use retention::{
    expire_old_runs, retention_backlog, Expired, RetentionBacklog, BACKLOG_COUNT_LIMIT,
    RETENTION_BATCH,
};

pub mod doc_lint_migration;
pub mod risk;
pub mod spec_drift;
pub mod spec_index;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod forge_tests;

#[cfg(test)]
mod spec_index_tests;
