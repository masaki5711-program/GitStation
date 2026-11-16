//! CI artifacts against a migrated database (M4 C6): an upload only while
//! the job runs, one row per path, the attempt's bounds, a read decided
//! before any row, what a later job may read of its needs, and the expiry
//! that never takes a kept row.

use forge_core::{ObjectId, RequestId, UserId};
use forge_errors::ErrorCode;

use super::*;
use crate::change::{Actor, ChangeContext};
use crate::ci_runners::lease;
use crate::ci_runners::{register, NewRunner};
use crate::pipelines::{self, NewCiJob, NewPipeline, PipelineRequest, Planned, Source};
use crate::projects::{self, CreatedProject, NewProject};
use crate::test_support::*;
use crate::users::{self, NewUser};

/// The default project quota (5 GiB).
const QUOTA: u64 = 5 * 1024 * 1024 * 1024;

fn ctx(actor: Actor, request: &str) -> ChangeContext {
    ChangeContext::new(actor, RequestId::parse(request).expect("request id"))
}

struct World {
    t: FreshDb,
    alice: UserId,
    web: CreatedProject,
}

async fn world() -> World {
    let t = FreshDb::migrated().await;
    let alice = users::create(
        &t.db,
        &ctx(Actor::System, "req-user-alice"),
        &NewUser::human("alice", "alice", "alice@example.test"),
    )
    .await
    .expect("alice");
    let web = projects::create(
        &t.db,
        &ctx(Actor::System, "req-project-web"),
        &NewProject::new("web", alice.namespace_id, "Web", alice.user_id),
    )
    .await
    .expect("web");
    World {
        t,
        alice: alice.user_id,
        web,
    }
}

fn job(name: &str, needs: &[&str]) -> NewCiJob {
    NewCiJob {
        name: name.to_owned(),
        stage: "test".to_owned(),
        wave: i32::from(!needs.is_empty()),
        needs: needs.iter().map(|n| (*n).to_owned()).collect(),
        image: format!("registry.internal/rust@sha256:{}", "a".repeat(64)),
        timeout_s: 600,
        tags: Vec::new(),
        secrets: Vec::new(),
    }
}

impl World {
    async fn pipeline(&self, sha: char, mark: CopyMark, jobs: Vec<NewCiJob>) -> i64 {
        let new = NewPipeline {
            ai_eval_job: None,
            request: PipelineRequest {
                project: self.web.project_id,
                git_ref: forge_core::RefName::parse("refs/heads/main").expect("ref"),
                sha: ObjectId::parse(&sha.to_string().repeat(40)).expect("sha"),
                source: Source::Push,
                merge_request: None,
                triggered_by: None,
            },
            request_id: "req-plan".to_owned(),
            definition_hash: Some(pipelines::definition_hash(b"version: 1")),
            protected: false,
            copy_mark: mark,
            planned: Planned::Jobs(jobs),
        };
        pipelines::create(&self.t.db, &new)
            .await
            .expect("pipeline")
            .pipeline_id
    }

    async fn runner(&self) -> i64 {
        register(
            &self.t.db,
            &ctx(Actor::User(self.alice), "req-reg-r1"),
            &NewRunner {
                name: "r1".to_owned(),
                description: String::new(),
                tags: Vec::new(),
            },
        )
        .await
        .expect("runner")
        .runner
        .id
    }

    async fn store(&self, job: i64, path: &str, size: u64) -> i64 {
        let target = upload_target(&self.t.db, job, path, QUOTA)
            .await
            .expect("target")
            .expect("running");
        record(&self.t.db, &target, (path, size, &[7u8; 32]), QUOTA)
            .await
            .expect("record")
    }
}

#[tokio::test]
async fn an_upload_is_taken_only_while_the_job_runs_and_once_per_path() {
    let w = world().await;
    let pipeline = w
        .pipeline('a', CopyMark::Open, vec![job("build", &[])])
        .await;
    let runner = w.runner().await;
    let claimed = lease::claim(&w.t.db, runner)
        .await
        .expect("claim")
        .expect("job");
    assert!(upload_target(&w.t.db, 999_999, "x", QUOTA)
        .await
        .expect("q")
        .is_none());
    let id = w.store(claimed.job_id, "out/app", 12).await;
    let e = upload_target(&w.t.db, claimed.job_id, "out/app", QUOTA)
        .await
        .expect_err("twice");
    assert_eq!(e.code(), ErrorCode::RequestInvalid);
    assert!(e.to_string().contains("artifact_exists"), "{e}");
    lease::complete(
        &w.t.db,
        runner,
        claimed.job_id,
        claimed.attempt,
        lease::JobResult::Succeeded,
    )
    .await
    .expect("complete");
    assert!(upload_target(&w.t.db, claimed.job_id, "out/other", QUOTA)
        .await
        .expect("q")
        .is_none());
    let (rows, ()) = list_decided(
        &w.t.db,
        w.web.project_id,
        (pipeline, claimed.job_id),
        |_| Ok(()),
    )
    .await
    .expect("list")
    .expect("job");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, id);
    assert_eq!(rows[0].sha256, "07".repeat(32));
}

#[tokio::test]
async fn an_artifact_past_the_limit_is_refused() {
    let w = world().await;
    w.pipeline('a', CopyMark::Open, vec![job("build", &[])])
        .await;
    let runner = w.runner().await;
    let claimed = lease::claim(&w.t.db, runner)
        .await
        .expect("claim")
        .expect("job");
    w.store(
        claimed.job_id,
        "big",
        forge_core::ci_artifact::MAX_FILE_BYTES,
    )
    .await;
    // Five files of 100 MiB: 500 MiB; a sixth would pass the attempt's
    // 512 MiB and is refused when it is recorded.
    for i in 0..4 {
        w.store(
            claimed.job_id,
            &format!("big{i}"),
            forge_core::ci_artifact::MAX_FILE_BYTES,
        )
        .await;
    }
    let target = upload_target(&w.t.db, claimed.job_id, "one-more", QUOTA)
        .await
        .expect("12 MiB still free")
        .expect("running");
    assert_eq!(target.bytes_left, 12 * 1024 * 1024);
    let e = record(
        &w.t.db,
        &target,
        (
            "one-more",
            forge_core::ci_artifact::MAX_FILE_BYTES,
            &[3u8; 32],
        ),
        QUOTA,
    )
    .await
    .expect_err("past the attempt");
    assert_eq!(e.code(), ErrorCode::ArtifactTooLarge);
}

#[tokio::test]
async fn a_read_is_decided_before_any_row_and_a_job_reads_only_its_needs() {
    let w = world().await;
    let pipeline = w
        .pipeline(
            'a',
            CopyMark::Confidential,
            vec![job("build", &[]), job("test", &["build"]), job("lint", &[])],
        )
        .await;
    let runner = w.runner().await;
    let mut claimed = Vec::new();
    for _ in 0..2 {
        claimed.push(
            lease::claim(&w.t.db, runner)
                .await
                .expect("claim")
                .expect("job"),
        );
    }
    let build = claimed.iter().find(|c| c.name == "build").expect("build");
    let lint = claimed.iter().find(|c| c.name == "lint").expect("lint");
    let artifact = w.store(build.job_id, "out/app", 3).await;
    w.store(lint.job_id, "lint.txt", 3).await;
    // The decision sees the pipeline's mark; a refusal reads nothing.
    let refused = list_decided(&w.t.db, w.web.project_id, (pipeline, build.job_id), |t| {
        assert_eq!(t.copy_mark, CopyMark::Confidential);
        Err::<(), _>(ForgeError::new(ErrorCode::RepositoryCopyWithheld))
    })
    .await
    .expect_err("refused");
    assert_eq!(refused.code(), ErrorCode::RepositoryCopyWithheld);
    // Another project's id finds nothing.
    assert!(one_decided(
        &w.t.db,
        ProjectId::new(999_999),
        (pipeline, build.job_id),
        artifact,
        |_| Ok(())
    )
    .await
    .expect("q")
    .is_none());
    lease::complete(
        &w.t.db,
        runner,
        build.job_id,
        build.attempt,
        lease::JobResult::Succeeded,
    )
    .await
    .expect("build done");
    let test = lease::claim(&w.t.db, runner)
        .await
        .expect("claim")
        .expect("test");
    assert_eq!(test.name, "test");
    let needed = needs_of(&w.t.db, test.job_id).await.expect("needs");
    let paths: Vec<&str> = needed.iter().map(|a| a.path.as_str()).collect();
    assert_eq!(paths, ["out/app"], "lint is not a need of test");
    assert!(need_one(&w.t.db, test.job_id, artifact)
        .await
        .expect("q")
        .is_some());
    // The build job itself has no needs.
    assert!(needs_of(&w.t.db, lint.job_id).await.expect("q").is_empty());
}

#[tokio::test]
async fn the_expiry_takes_old_rows_but_never_a_kept_one() {
    let w = world().await;
    w.pipeline('a', CopyMark::Open, vec![job("build", &[])])
        .await;
    let runner = w.runner().await;
    let claimed = lease::claim(&w.t.db, runner)
        .await
        .expect("claim")
        .expect("job");
    let old = w.store(claimed.job_id, "old", 1).await;
    let kept = w.store(claimed.job_id, "kept", 1).await;
    let fresh = w.store(claimed.job_id, "fresh", 1).await;
    w.t.exec_superuser(&format!(
        "UPDATE forge.ci_artifacts SET created_at = now() - interval '40 days', \
         expires_at = now() - interval '10 days' WHERE id IN ({old}, {kept})"
    ))
    .await;
    w.t.exec_superuser(&format!(
        "UPDATE forge.ci_artifacts SET keep = true WHERE id = {kept}"
    ))
    .await;
    let due = due(&w.t.db, 10).await.expect("due");
    assert_eq!(due, vec![(old, w.web.project_id.get())]);
    assert!(forget(&w.t.db, old).await.expect("forget"));
    assert!(!forget(&w.t.db, kept).await.expect("kept"));
    assert!(!forget(&w.t.db, fresh).await.expect("fresh"));
}

#[tokio::test]
async fn an_upload_past_the_project_quota_is_refused() {
    let w = world().await;
    w.pipeline('a', CopyMark::Open, vec![job("build", &[])])
        .await;
    let runner = w.runner().await;
    let claimed = lease::claim(&w.t.db, runner)
        .await
        .expect("claim")
        .expect("job");
    let quota = 1000;
    let first = upload_target(&w.t.db, claimed.job_id, "a", quota)
        .await
        .expect("target")
        .expect("running");
    assert_eq!(first.bytes_left, 1000);
    // Two uploads checked against the same free space: the record of the
    // second sees the first and refuses (no parallel bypass).
    let second = upload_target(&w.t.db, claimed.job_id, "b", quota)
        .await
        .expect("target")
        .expect("running");
    record(&w.t.db, &first, ("a", 600, &[1u8; 32]), quota)
        .await
        .expect("first fits");
    let e = record(&w.t.db, &second, ("b", 600, &[2u8; 32]), quota)
        .await
        .expect_err("past the quota");
    assert_eq!(e.code(), ErrorCode::ArtifactQuotaExceeded);
    let e = upload_target(&w.t.db, claimed.job_id, "c", 500)
        .await
        .expect_err("already full");
    assert_eq!(e.code(), ErrorCode::ArtifactQuotaExceeded);
}

#[tokio::test]
async fn a_secret_jobs_artifacts_reach_needs_only_in_a_protected_push() {
    let w = world().await;
    let pipeline = w
        .pipeline(
            'a',
            CopyMark::Open,
            vec![job("build", &[]), job("test", &["build"])],
        )
        .await;
    let runner = w.runner().await;
    let build = lease::claim(&w.t.db, runner)
        .await
        .expect("claim")
        .expect("build");
    w.store(build.job_id, "out/app", 3).await;
    // The build was given secrets (as the claim records it).
    w.t.exec_superuser(&format!(
        "UPDATE forge.ci_jobs SET secrets_released = true WHERE id = {}",
        build.job_id
    ))
    .await;
    lease::complete(
        &w.t.db,
        runner,
        build.job_id,
        build.attempt,
        lease::JobResult::Succeeded,
    )
    .await
    .expect("done");
    let test = lease::claim(&w.t.db, runner)
        .await
        .expect("claim")
        .expect("test");
    // An unprotected pipeline: not given to the consumer.
    assert!(needs_of(&w.t.db, test.job_id).await.expect("q").is_empty());
    // The target says so, for the read decision.
    let (_, target) = list_decided(&w.t.db, w.web.project_id, (pipeline, build.job_id), |t| {
        Ok::<_, ForgeError>(t.clone())
    })
    .await
    .expect("list")
    .expect("job");
    assert!(target.secrets_released);
    // In a protected push it is.
    w.t.exec_superuser(&format!(
        "UPDATE forge.ci_pipelines SET protected = true WHERE id = {pipeline}"
    ))
    .await;
    assert_eq!(needs_of(&w.t.db, test.job_id).await.expect("q").len(), 1);
}

#[tokio::test]
async fn more_needed_files_than_the_bound_is_an_error_not_a_short_list() {
    let w = world().await;
    w.pipeline(
        'a',
        CopyMark::Open,
        vec![job("build", &[]), job("test", &["build"])],
    )
    .await;
    let runner = w.runner().await;
    let build = lease::claim(&w.t.db, runner)
        .await
        .expect("claim")
        .expect("build");
    w.t.exec_superuser(&format!(
        "INSERT INTO forge.ci_artifacts (project_id, pipeline_id, job_id, attempt, path, \
           size_bytes, sha256, expires_at) \
         SELECT project_id, pipeline_id, id, attempt, 'f' || g, 1, decode(repeat('00', 32), 'hex'), \
           now() + interval '1 day' FROM forge.ci_jobs, generate_series(1, {}) g WHERE id = {}",
        MAX_LISTED + 1,
        build.job_id
    ))
    .await;
    lease::complete(
        &w.t.db,
        runner,
        build.job_id,
        build.attempt,
        lease::JobResult::Succeeded,
    )
    .await
    .expect("done");
    let test = lease::claim(&w.t.db, runner)
        .await
        .expect("claim")
        .expect("test");
    let e = needs_of(&w.t.db, test.job_id).await.expect_err("too many");
    assert_eq!(e.code(), ErrorCode::ArtifactTooLarge);
    assert!(e.to_string().contains("too_many_needed_files"), "{e}");
}
