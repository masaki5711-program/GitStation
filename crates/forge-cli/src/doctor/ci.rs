//! The CI pipelines check of `forge doctor` (M4 C2, plan 14, runbook
//! ci-pipelines), from `forge_db::pipelines::health` on the read pool:
//!
//! - `ci.pipelines` — `FG-CI-0002` when a pipeline was recorded `error` in
//!   the last 24 hours or a `ci.pipeline_create` job died in the last 7
//!   days (a push may be without its pipeline: run it by hand); a warning
//!   when a `pending` / `running` pipeline has not moved for
//!   [`STALE_AFTER`] (until the runner API exists, M4 C3, nothing runs
//!   jobs, so pending pipelines only wait); otherwise ok. Ids only.

use std::time::Duration;

use forge_db::pipelines::{health, PipelineHealth};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};

use super::{fail, ok, warn, Check};

pub(super) const NAME: &str = "ci.pipelines";

/// A `pending` / `running` pipeline that has not changed this long is stale.
pub(super) const STALE_AFTER: Duration = Duration::from_secs(24 * 60 * 60);

fn ids(list: &[i64]) -> String {
    list.iter()
        .map(i64::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

/// `ci.pipelines` for `h` (pure: the tests feed reports).
fn line(h: &PipelineHealth) -> Check {
    if h.errors > 0 || h.dead_jobs > 0 {
        return fail(
            NAME,
            ForgeError::new(ErrorCode::PipelineCreateFailed)
                .with_context("reason", "unanswered_pushes")
                .with_context("error_pipelines_24h", h.errors)
                .with_context("pipeline_ids", ids(&h.error_ids))
                .with_context("dead_jobs_7d", h.dead_jobs)
                .with_context("job_ids", ids(&h.dead_job_ids))
                .with_context("runbook", "ci-pipelines"),
        );
    }
    if h.stale > 0 {
        return warn(
            NAME,
            &format!(
                "{} pending or running pipeline(s) unchanged for over {} h (ids {}); no runner takes jobs before M4 C3 (runbook ci-pipelines)",
                h.stale,
                STALE_AFTER.as_secs() / 3600,
                ids(&h.stale_ids)
            ),
        );
    }
    ok(
        NAME,
        "no error pipeline in 24 h, no dead ci.pipeline_create job in 7 days, none stale",
    )
}

/// The check.
pub(super) async fn pipelines_check(db: &Db) -> Check {
    match health(db, STALE_AFTER).await {
        Ok(h) => line(&h),
        Err(e) => fail(NAME, e),
    }
}

#[cfg(test)]
mod tests {
    use forge_db::pipelines::{
        create, definition_hash, CopyMark, NewPipeline, PipelineRequest, Planned, Source,
    };
    use forge_db::test_support::FreshDb;

    use super::super::Outcome;
    use super::*;

    fn describe(c: &Check) -> String {
        match &c.outcome {
            Outcome::Ok(s) | Outcome::Warn(s) => s.clone(), // clone: the message for the assertion
            Outcome::Fail(e) => e.code().id().to_owned(),
            Outcome::Skip(s) => (*s).to_owned(),
        }
    }

    #[test]
    fn errors_and_dead_jobs_fail_and_stale_pipelines_warn() {
        let quiet = PipelineHealth::default();
        assert!(matches!(line(&quiet).outcome, Outcome::Ok(_)));
        let errors = PipelineHealth {
            errors: 2,
            error_ids: vec![9, 8],
            ..PipelineHealth::default()
        };
        assert_eq!(describe(&line(&errors)), "FG-CI-0002");
        let dead = PipelineHealth {
            dead_jobs: 1,
            dead_job_ids: vec![5],
            ..PipelineHealth::default()
        };
        let Outcome::Fail(e) = line(&dead).outcome else {
            panic!("dead jobs fail");
        };
        assert!(e.context().iter().any(|(k, v)| *k == "job_ids" && v == "5"));
        let stale = PipelineHealth {
            stale: 1,
            stale_ids: vec![3],
            ..PipelineHealth::default()
        };
        let c = line(&stale);
        assert!(matches!(c.outcome, Outcome::Warn(_)));
        assert!(describe(&c).contains("ids 3"), "{}", describe(&c));
    }

    /// Against the database: an `error` pipeline fails the check.
    #[tokio::test]
    async fn an_error_pipeline_fails_the_check() {
        let t = FreshDb::migrated().await;
        assert!(matches!(
            pipelines_check(&t.db).await.outcome,
            Outcome::Ok(_)
        ));
        let ctx = forge_db::change::ChangeContext::new(
            forge_db::change::Actor::System,
            forge_core::RequestId::parse("req-doctor").expect("id"),
        );
        let alice = forge_db::users::create(
            &t.db,
            &ctx,
            &forge_db::users::NewUser::human("alice", "Alice", "alice@example.test"),
        )
        .await
        .expect("alice");
        let web = forge_db::projects::create(
            &t.db,
            &ctx,
            &forge_db::projects::NewProject::new("web", alice.namespace_id, "Web", alice.user_id),
        )
        .await
        .expect("web");
        let mut new = NewPipeline {
            request: PipelineRequest {
                project: web.project_id,
                git_ref: forge_core::RefName::parse("refs/heads/main").expect("ref"),
                sha: forge_core::ObjectId::parse(&"b".repeat(40)).expect("sha"),
                source: Source::Push,
                merge_request: None,
                triggered_by: None,
            },
            request_id: "req-doctor".to_owned(),
            definition_hash: Some(definition_hash(b"x")),
            protected: false,
            copy_mark: CopyMark::Open,
            planned: Planned::Jobs(Vec::new()),
        };
        let _ = create(&t.db, &new).await.expect("skipped");
        assert!(matches!(
            pipelines_check(&t.db).await.outcome,
            Outcome::Ok(_)
        ));
        new.request.git_ref = forge_core::RefName::parse("refs/heads/other").expect("ref");
        new.definition_hash = None;
        new.planned = Planned::Error;
        let _ = create(&t.db, &new).await.expect("error");
        assert_eq!(describe(&pipelines_check(&t.db).await), "FG-CI-0002");
        t.drop_now().await;
    }
}
