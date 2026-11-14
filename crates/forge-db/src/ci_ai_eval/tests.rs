#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)

//! The `forge/ai-eval` store against a migrated database (plan 17 P17-2):
//! only declaring pipelines are due, the target reads the declaring job's
//! current attempt and its report, one write per pipeline status, the
//! newest declaring pipeline of a commit owns the check, and a pipeline
//! that changed since its target was read is left for the next pass.

use forge_core::checks::{CheckConclusion, CheckStatus};
use forge_core::{CheckName, RefName, RequestId};

use super::*;
use crate::change::{Actor, ChangeContext};
use crate::pipelines::{self, CopyMark, NewCiJob, NewPipeline, PipelineRequest, Planned, Source};
use crate::projects::{self, CreatedProject, NewProject};
use crate::test_support::*;
use crate::users::{self, NewUser};

fn ctx(actor: Actor, request: &str) -> ChangeContext {
    ChangeContext::new(actor, RequestId::parse(request).expect("request id"))
}

fn system() -> ChangeContext {
    let mut c = ctx(Actor::System, "req-ai-eval");
    c.actor_label = Some("forge-ci".to_owned());
    c
}

struct World {
    t: FreshDb,
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
    World { t, web }
}

fn job(name: &str) -> NewCiJob {
    NewCiJob {
        name: name.to_owned(),
        stage: "test".to_owned(),
        wave: 0,
        needs: Vec::new(),
        image: format!("registry.internal/bench@sha256:{}", "a".repeat(64)),
        timeout_s: 600,
        tags: Vec::new(),
        secrets: Vec::new(),
    }
}

fn sha(c: char) -> ObjectId {
    ObjectId::parse(&c.to_string().repeat(40)).expect("sha")
}

fn run(conclusion: CheckConclusion) -> NewCheckRun {
    NewCheckRun {
        name: CheckName::parse(AI_EVAL_CHECK).expect("name"),
        status: CheckStatus::Completed,
        conclusion: Some(conclusion),
        summary: "forge/ai-eval".to_owned(),
        details_url: None,
        annotations: Vec::new(),
    }
}

impl World {
    async fn pipeline(&self, c: char, source: Source, ai_eval_job: Option<&str>) -> i64 {
        let new = NewPipeline {
            ai_eval_job: ai_eval_job.map(str::to_owned),
            request: PipelineRequest {
                project: self.web.project_id,
                git_ref: RefName::parse("refs/heads/main").expect("ref"),
                sha: sha(c),
                source,
                merge_request: None,
                triggered_by: None,
            },
            request_id: "req-plan".to_owned(),
            definition_hash: Some(pipelines::definition_hash(b"version: 1")),
            protected: false,
            copy_mark: CopyMark::Open,
            planned: Planned::Jobs(vec![job("build"), job("bench")]),
        };
        pipelines::create(&self.t.db, &new)
            .await
            .expect("pipeline")
            .pipeline_id
    }

    /// The pipeline and its bench job end in `status` / `job_status`; with
    /// `report`, the bench job's attempt holds `eval-report.json`.
    async fn finish(&self, pipeline: i64, status: &str, job_status: &str, report: bool) {
        let mut sql = vec![
            format!("UPDATE forge.ci_pipelines SET status = '{status}' WHERE id = {pipeline}"),
            format!(
                "UPDATE forge.ci_jobs SET status = '{job_status}' \
                 WHERE pipeline_id = {pipeline} AND name = 'bench'"
            ),
        ];
        if report {
            sql.push(format!(
                "INSERT INTO forge.ci_artifacts (project_id, pipeline_id, job_id, attempt, path, \
                   size_bytes, sha256, expires_at) \
                 SELECT project_id, pipeline_id, id, attempt, 'eval-report.json', 12, \
                   decode(repeat('ab', 32), 'hex'), now() + interval '30 days' \
                 FROM forge.ci_jobs WHERE pipeline_id = {pipeline} AND name = 'bench'"
            ));
        }
        self.t.exec_superuser_tx(&sql).await;
    }

    async fn check_of(&self, c: char) -> Vec<serde_json::Value> {
        self.t
            .json_rows(&format!(
                "SELECT status, conclusion, reported_by FROM forge.check_runs \
                 WHERE project_id = {} AND head_sha = '{}' AND name = 'forge/ai-eval'",
                self.web.project_id.get(),
                sha(c).as_str()
            ))
            .await
    }
}

/// Plan 17 §1.6: a pipeline that declares no report is never due and has
/// no target; nothing is ever written for it.
#[tokio::test]
async fn a_pipeline_without_a_declared_report_is_never_due() {
    let w = world().await;
    let plain = w.pipeline('a', Source::Push, None).await;
    w.finish(plain, "succeeded", "succeeded", true).await;
    assert!(pipelines_due(&w.t.db, 100).await.expect("due").is_empty());
    assert_eq!(target(&w.t.db, plain).await.expect("target"), None);
    assert!(w.check_of('a').await.is_empty());
    w.t.drop_now().await;
}

#[tokio::test]
async fn the_declared_job_must_be_planned() {
    let w = world().await;
    let new = NewPipeline {
        ai_eval_job: Some("missing".to_owned()),
        request: PipelineRequest {
            project: w.web.project_id,
            git_ref: RefName::parse("refs/heads/main").expect("ref"),
            sha: sha('b'),
            source: Source::Push,
            merge_request: None,
            triggered_by: None,
        },
        request_id: "req-plan".to_owned(),
        definition_hash: None,
        protected: false,
        copy_mark: CopyMark::Open,
        planned: Planned::Jobs(vec![job("build")]),
    };
    let e = pipelines::create(&w.t.db, &new)
        .await
        .expect_err("not planned");
    assert_reason(&e, ErrorCode::Internal, "ai_eval_job_not_planned");
    w.t.drop_now().await;
}

/// The target follows the declaring job's current attempt; the check is
/// written once per pipeline status, as the system (no reporter).
#[tokio::test]
async fn the_check_follows_the_declaring_pipeline_once_per_status() {
    let w = world().await;
    let p = w.pipeline('c', Source::Manual, Some("bench")).await;
    assert_eq!(pipelines_due(&w.t.db, 100).await.expect("due"), [p]);
    let t = target(&w.t.db, p).await.expect("target").expect("declared");
    assert_eq!(
        (t.status, t.job.as_str()),
        (PipelineStatus::Pending, "bench")
    );
    assert_eq!((t.job_status, t.job_attempt), (Some(JobStatus::Pending), 1));
    assert_eq!(t.report, None);
    assert_eq!(t.repo, None, "the repository is still provisioning");

    w.finish(p, "succeeded", "succeeded", true).await;
    let t = target(&w.t.db, p).await.expect("target").expect("declared");
    let report = t.report.clone().expect("report");
    assert_eq!((report.size_bytes, report.sha256.len()), (12, 32));
    let written = record(&w.t.db, &system(), &t, &run(CheckConclusion::Success))
        .await
        .expect("record");
    assert!(matches!(written, AiEvalCheck::Written(_)), "{written:?}");
    let rows = w.check_of('c').await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["conclusion"], "success");
    assert!(rows[0]["reported_by"].is_null());
    // Reported at this status: no longer due, a second write is a no-op.
    assert!(pipelines_due(&w.t.db, 100).await.expect("due").is_empty());
    let again = record(&w.t.db, &system(), &t, &run(CheckConclusion::Failure))
        .await
        .expect("record");
    assert_eq!(again, AiEvalCheck::Skipped);
    assert_eq!(w.check_of('c').await[0]["conclusion"], "success");
    w.t.drop_now().await;
}

/// A retry between the read and the write (the attempt moved) or a status
/// change leaves the pipeline for the next pass.
#[tokio::test]
async fn a_pipeline_changed_since_its_target_is_skipped() {
    let w = world().await;
    let p = w.pipeline('d', Source::Push, Some("bench")).await;
    w.finish(p, "failed", "failed", false).await;
    let t = target(&w.t.db, p).await.expect("target").expect("declared");
    w.t.exec_superuser(&format!(
        "UPDATE forge.ci_jobs SET attempt = 2, status = 'pending' \
         WHERE pipeline_id = {p} AND name = 'bench'"
    ))
    .await;
    let skipped = record(&w.t.db, &system(), &t, &run(CheckConclusion::Error))
        .await
        .expect("record");
    assert_eq!(skipped, AiEvalCheck::Skipped);
    assert!(w.check_of('d').await.is_empty());
    assert_eq!(pipelines_due(&w.t.db, 100).await.expect("due"), [p]);
    w.t.drop_now().await;
}

/// Only the newest declaring pipeline of a commit writes; a newer
/// pipeline that declares no report does not take the check away.
#[tokio::test]
async fn the_newest_declaring_pipeline_of_a_commit_owns_the_check() {
    let w = world().await;
    let older = w.pipeline('e', Source::Manual, Some("bench")).await;
    let newer = w.pipeline('e', Source::Manual, Some("bench")).await;
    let plain = w.pipeline('e', Source::Push, None).await;
    assert!(plain > newer);
    for p in [older, newer] {
        w.finish(p, "succeeded", "succeeded", true).await;
    }
    let t_old = target(&w.t.db, older).await.expect("t").expect("declared");
    let superseded = record(&w.t.db, &system(), &t_old, &run(CheckConclusion::Failure))
        .await
        .expect("record");
    assert_eq!(superseded, AiEvalCheck::Superseded);
    assert!(w.check_of('e').await.is_empty());
    let t_new = target(&w.t.db, newer).await.expect("t").expect("declared");
    let written = record(&w.t.db, &system(), &t_new, &run(CheckConclusion::Success))
        .await
        .expect("record");
    assert!(matches!(written, AiEvalCheck::Written(_)));
    assert_eq!(w.check_of('e').await[0]["conclusion"], "success");
    w.t.drop_now().await;
}

#[tokio::test]
async fn only_the_ai_eval_check_is_written_here() {
    let w = world().await;
    let p = w.pipeline('f', Source::Push, Some("bench")).await;
    let t = target(&w.t.db, p).await.expect("t").expect("declared");
    let mut other = run(CheckConclusion::Success);
    other.name = CheckName::parse("forge/risk").expect("name");
    let e = record(&w.t.db, &system(), &t, &other)
        .await
        .expect_err("other name");
    assert_reason(&e, ErrorCode::Internal, "not_ai_eval_check");
    w.t.drop_now().await;
}
