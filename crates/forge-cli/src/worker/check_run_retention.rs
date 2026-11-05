//! The `check_run_retention` task of `forge worker` (feat/db-check-run-
//! retention, ADR-0024 O3 note): every [`PERIOD`] it deletes one batch of
//! check runs (with their annotations) last updated more than
//! `review.check_run_retention_days` (default 180) days ago, except the
//! current head of an open merge request (forge-db
//! `checks::expire_old_runs`: one short transaction per batch, one
//! `check_run.retention` audit row per project and batch). A full batch
//! runs again after [`BATCH_PAUSE`], so a backlog drains without waiting a
//! period per batch.
//!
//! A failed run (the database: `FG-DB-0001` / `FG-DB-0002`) is returned and
//! logged once per failure streak by the supervisor with `error_code` and
//! `task=check_run_retention`, then retried after the backoff; a batch is
//! one transaction, so nothing is half done.

use std::time::Duration;

use forge_core::RequestId;
use forge_db::change::{Actor, ChangeContext};
use forge_db::checks::{expire_old_runs, RETENTION_BATCH};
use forge_db::Db;
use forge_errors::ForgeError;

use super::supervise::Next;

/// The task's name in the supervisor's logs.
pub(crate) const NAME: &str = "check_run_retention";
/// How often a batch runs when nothing is left.
pub(crate) const PERIOD: Duration = Duration::from_secs(3600);
/// The pause after a full batch before the next one.
pub(crate) const BATCH_PAUSE: Duration = Duration::from_millis(500);
/// The audit label of the worker's changes.
const ACTOR_LABEL: &str = "forge-worker";

/// One run: one batch; a full one asks to run again after a short pause.
pub(crate) async fn run_check_run_retention(db: &Db, days: u32) -> Result<Next, ForgeError> {
    let request_id = RequestId::generate();
    let mut ctx = ChangeContext::new(Actor::System, request_id.clone()); // clone: logged below
    ctx.actor_label = Some(ACTOR_LABEL.to_owned());
    let expired = expire_old_runs(db, &ctx, days, RETENTION_BATCH).await?;
    if expired.runs > 0 || expired.skipped_projects > 0 {
        tracing::info!(
            target: "forge_cli",
            task = NAME,
            request_id = %request_id,
            runs = expired.runs,
            annotations = expired.annotations,
            projects = expired.projects,
            skipped_locked_projects = expired.skipped_projects,
            older_than_days = days,
            "forge.worker.check_runs_expired"
        );
    }
    // More is due when the batch filled, or when projects were skipped
    // while others made progress (a run with no progress waits the period).
    if expired.runs >= i64::from(RETENTION_BATCH)
        || (expired.runs > 0 && expired.skipped_projects > 0)
    {
        // A short pause between full batches: reports and the merge gate
        // get the database between them.
        tokio::time::sleep(BATCH_PAUSE).await;
        return Ok(Next::Immediately);
    }
    Ok(Next::AfterPeriod)
}

#[cfg(test)]
mod tests {
    use forge_db::test_support::FreshDb;

    use super::*;

    /// A run on a database with nothing due waits for the period.
    #[tokio::test]
    async fn a_run_with_nothing_due_waits_for_the_period() {
        let t = FreshDb::migrated().await;
        assert_eq!(
            run_check_run_retention(&t.db, 180).await.expect("run"),
            Next::AfterPeriod
        );
        t.drop_now().await;
    }

    /// A partial batch deletes the old runs, writes the audit row as the
    /// worker (actor `system`, label `forge-worker`) and waits.
    #[tokio::test]
    async fn a_run_deletes_old_runs_and_audits_it() {
        let t = FreshDb::migrated().await;
        let user = forge_db::users::create(
            &t.db,
            &ChangeContext::new(Actor::System, RequestId::parse("req-user").expect("id")),
            &forge_db::users::NewUser::human("alice", "Alice", "alice@example.test"),
        )
        .await
        .expect("user");
        let project = forge_db::projects::create(
            &t.db,
            &ChangeContext::new(Actor::System, RequestId::parse("req-project").expect("id")),
            &forge_db::projects::NewProject::new("web", user.namespace_id, "Web", user.user_id),
        )
        .await
        .expect("project")
        .project_id;
        let p = project.get();
        t.commit_audited(
            ("project", p),
            &[format!(
                "INSERT INTO forge.check_runs (project_id, head_sha, name, status, reported_by, \
                 request_id, created_at, updated_at) VALUES ({p}, repeat('a', 40), 'ci/old', \
                 'queued', {}, 'req-old', now() - interval '400 days', now() - interval '300 days')",
                user.user_id.get()
            )],
        )
        .await;
        let next = run_check_run_retention(&t.db, 180).await.expect("run");
        assert_eq!(next, Next::AfterPeriod);
        let left = t.json_rows("SELECT id FROM forge.check_runs").await;
        assert!(left.is_empty(), "{left:?}");
        let rows = t
            .json_rows(
                "SELECT actor_kind, actor_label FROM forge.audit_log \
                 WHERE action = 'check_run.retention'",
            )
            .await;
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0]["actor_kind"], "system");
        assert_eq!(rows[0]["actor_label"], ACTOR_LABEL);
        t.drop_now().await;
    }
}
