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
        let target = upload_target(&self.t.db, job, path)
            .await
            .expect("target")
            .expect("running");
        record(&self.t.db, &target, path, size, &[7u8; 32])
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
    assert!(upload_target(&w.t.db, 999_999, "x")
        .await
        .expect("q")
        .is_none());
    let id = w.store(claimed.job_id, "out/app", 12).await;
    let e = upload_target(&w.t.db, claimed.job_id, "out/app")
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
    assert!(upload_target(&w.t.db, claimed.job_id, "out/other")
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
    // Six files of 100 MiB: 600 MiB, past the attempt's 512 MiB.
    for i in 0..5 {
        w.store(
            claimed.job_id,
            &format!("big{i}"),
            forge_core::ci_artifact::MAX_FILE_BYTES,
        )
        .await;
    }
    let e = upload_target(&w.t.db, claimed.job_id, "one-more")
        .await
        .expect_err("full");
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
