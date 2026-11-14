#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)

//! The `forge/ai-eval` sweep against a migrated database (plan 17 P17-2,
//! ADR-0034): no check without a declared report, `error` (never success)
//! without a usable report or baseline, the comparison table otherwise.
//! The report file lives in a temporary artifact store; the baseline comes
//! from a fixed [`BaselineSource`]. All data is synthetic.

use std::collections::BTreeMap;

use forge_core::ai_eval::SCHEMA_V1;
use forge_core::{ProjectId, RefName};
use forge_db::change::{Actor, ChangeContext};
use forge_db::pipelines::{
    self as pl, CopyMark, NewCiJob, NewPipeline, PipelineRequest, Planned, Source,
};
use forge_db::projects::{self, NewProject};
use forge_db::test_support::FreshDb;
use forge_db::users::{self, NewUser};

use super::*;

/// The baselines by path; every other path is absent.
struct Baselines(BTreeMap<String, Vec<u8>>);

impl BaselineSource for Baselines {
    fn baseline<'a>(
        &'a self,
        _repo: RepoId,
        _commit: &'a ObjectId,
        path: &'a str,
    ) -> BaselineFuture<'a> {
        let found = self.0.get(path).cloned(); // clone: the answer owns its bytes
        Box::pin(async move { Ok(found.map_or(SmallFile::Absent, SmallFile::Found)) })
    }
}

fn ctx(request: &str) -> ChangeContext {
    ChangeContext::new(
        Actor::System,
        RequestId::parse(request).expect("request id"),
    )
}

fn report_json(cases: &[(&str, &str, u32, u32)], hit: f64) -> Vec<u8> {
    let cases: Vec<String> = cases
        .iter()
        .map(|(id, kind, passes, runs)| {
            format!(r#"{{"id":"{id}","kind":"{kind}","runs":{runs},"passes":{passes}}}"#)
        })
        .collect();
    format!(
        r#"{{"schema":"{SCHEMA_V1}","models":"qwen3-14b","cases":[{}],"metrics":{{
        "cause_hit_rate":{hit},"attach_precision":0.9,"false_alarm_rate":0.05,
        "abstain_accuracy":0.8,"p95_latency_ms":40000}}}}"#,
        cases.join(",")
    )
    .into_bytes()
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

struct World {
    t: FreshDb,
    project: ProjectId,
    store: ArtifactStore,
    _dir: tempfile::TempDir,
}

async fn world() -> World {
    let t = FreshDb::migrated().await;
    let alice = users::create(
        &t.db,
        &ctx("req-user-alice"),
        &NewUser::human("alice", "alice", "alice@example.test"),
    )
    .await
    .expect("alice");
    let created = projects::create(
        &t.db,
        &ctx("req-project-assets"),
        &NewProject::new("assets", alice.namespace_id, "Assets", alice.user_id),
    )
    .await
    .expect("project");
    let _ready =
        forge_db::repositories::mark_ready(&t.db, &ctx("req-ready"), created.repository_id)
            .await
            .expect("ready");
    let dir = tempfile::tempdir().expect("dir");
    World {
        t,
        project: created.project_id,
        store: ArtifactStore::new(dir.path()),
        _dir: dir,
    }
}

impl World {
    /// A finished pipeline on commit `c`; `declared`: its `bench` job
    /// declares the report.
    async fn pipeline(&self, c: char, declared: bool, job_status: &str) -> i64 {
        let new = NewPipeline {
            ai_eval_job: declared.then(|| "bench".to_owned()),
            request: PipelineRequest {
                project: self.project,
                git_ref: RefName::parse("refs/heads/improve/x").expect("ref"),
                sha: ObjectId::parse(&c.to_string().repeat(40)).expect("sha"),
                source: Source::Push,
                merge_request: None,
                triggered_by: None,
            },
            request_id: "req-plan".to_owned(),
            definition_hash: Some(pl::definition_hash(b"version: 1")),
            protected: false,
            copy_mark: CopyMark::Open,
            planned: Planned::Jobs(vec![job("bench")]),
        };
        let id = pl::create(&self.t.db, &new)
            .await
            .expect("pipeline")
            .pipeline_id;
        let status = if job_status == "succeeded" {
            "succeeded"
        } else {
            "failed"
        };
        self.t
            .exec_superuser_tx(&[
                format!("UPDATE forge.ci_pipelines SET status = '{status}' WHERE id = {id}"),
                format!(
                    "UPDATE forge.ci_jobs SET status = '{job_status}' WHERE pipeline_id = {id}"
                ),
            ])
            .await;
        id
    }

    /// Store `bytes` as the bench job's `eval-report.json` (row and file).
    async fn upload(&self, pipeline: i64, bytes: &[u8]) {
        let digest: String = Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        self.t
            .exec_superuser(&format!(
                "INSERT INTO forge.ci_artifacts (project_id, pipeline_id, job_id, attempt, path, \
                   size_bytes, sha256, expires_at) \
                 SELECT project_id, pipeline_id, id, attempt, 'eval-report.json', {}, \
                   decode('{digest}', 'hex'), now() + interval '30 days' \
                 FROM forge.ci_jobs WHERE pipeline_id = {pipeline}",
                bytes.len()
            ))
            .await;
        let id: i64 = self
            .t
            .json_rows(&format!(
                "SELECT id FROM forge.ci_artifacts WHERE pipeline_id = {pipeline}"
            ))
            .await[0]["id"]
            .as_i64()
            .expect("id");
        let file = self.store.file(self.project.get(), id);
        std::fs::create_dir_all(file.parent().expect("dir")).expect("mkdir");
        std::fs::write(&file, bytes).expect("write");
    }

    async fn sweep(&self, baselines: &Baselines) -> AiEvalPass {
        report_ai_eval_checks(
            &self.t.db,
            &self.store,
            baselines,
            &RequestId::parse("req-sweep").expect("id"),
        )
        .await
        .expect("pass")
    }

    /// `(conclusion, summary)` of `forge/ai-eval` on commit `c`, if any.
    async fn check(&self, c: char) -> Option<(String, String)> {
        let rows = self
            .t
            .json_rows(&format!(
                "SELECT conclusion, summary FROM forge.check_runs WHERE project_id = {} \
                 AND head_sha = '{}' AND name = 'forge/ai-eval'",
                self.project.get(),
                c.to_string().repeat(40)
            ))
            .await;
        rows.first().map(|r| {
            (
                r["conclusion"].as_str().unwrap_or("-").to_owned(),
                r["summary"].as_str().unwrap_or("").to_owned(),
            )
        })
    }
}

fn baselines(bytes: Vec<u8>) -> Baselines {
    Baselines(BTreeMap::from([(
        "evals/baselines/qwen3-14b.json".to_owned(),
        bytes,
    )]))
}

/// Plan 17 §1.6, §9: a pipeline whose `.forge-ci.yml` declares no
/// `eval-report.json` never gets `forge/ai-eval` — even when one of its
/// jobs happens to upload a file of that name.
#[tokio::test]
async fn no_ai_eval_check_without_declared_report() {
    let w = world().await;
    let plain = w.pipeline('a', false, "succeeded").await;
    w.upload(plain, &report_json(&[("a/1", "regular", 3, 3)], 0.8))
        .await;
    let pass = w
        .sweep(&baselines(report_json(&[("a/1", "regular", 3, 3)], 0.8)))
        .await;
    assert_eq!(pass, AiEvalPass::default());
    assert_eq!(w.check('a').await, None);
    w.t.drop_now().await;
}

/// Gate 5: a declared report that is missing — the job failed, or it
/// succeeded without uploading the file — is `error`, never success.
#[tokio::test]
async fn missing_report_is_error_not_success() {
    let w = world().await;
    let base = baselines(report_json(&[("a/1", "regular", 3, 3)], 0.8));
    w.pipeline('b', true, "succeeded").await;
    w.pipeline('c', true, "failed").await;
    let pass = w.sweep(&base).await;
    assert_eq!(pass.written, 2);
    let (conclusion, summary) = w.check('b').await.expect("run");
    assert_eq!(conclusion, "error");
    assert!(
        summary.contains("[FG-AIASSET-0002] report_missing"),
        "{summary}"
    );
    assert!(summary.contains("Runbook: ai-eval-bench"), "{summary}");
    let (conclusion, summary) = w.check('c').await.expect("run");
    assert_eq!(conclusion, "error");
    assert!(summary.contains("job_failed"), "{summary}");
    w.t.drop_now().await;
}

#[tokio::test]
async fn baseline_absent_is_error() {
    let w = world().await;
    let p = w.pipeline('d', true, "succeeded").await;
    w.upload(p, &report_json(&[("a/1", "regular", 3, 3)], 0.8))
        .await;
    w.sweep(&Baselines(BTreeMap::new())).await;
    let (conclusion, summary) = w.check('d').await.expect("run");
    assert_eq!(conclusion, "error");
    assert!(
        summary.contains("[FG-AIASSET-0003] baseline_absent")
            && summary.contains("evals/baselines/qwen3-14b.json"),
        "{summary}"
    );
    w.t.drop_now().await;
}

/// A report that is not v1, or whose stored bytes changed after upload, is
/// `error` FG-AIASSET-0007.
#[tokio::test]
async fn an_invalid_or_altered_report_is_error() {
    let w = world().await;
    let base = baselines(report_json(&[("a/1", "regular", 3, 3)], 0.8));
    let p = w.pipeline('e', true, "succeeded").await;
    w.upload(p, b"{\"schema\":\"other\"}").await;
    let q = w.pipeline('f', true, "succeeded").await;
    w.upload(q, &report_json(&[("a/1", "regular", 3, 3)], 0.8))
        .await;
    let id =
        w.t.json_rows(&format!(
            "SELECT id FROM forge.ci_artifacts WHERE pipeline_id = {q}"
        ))
        .await[0]["id"]
            .as_i64()
            .expect("id");
    std::fs::write(w.store.file(w.project.get(), id), b"{}").expect("alter");
    w.sweep(&base).await;
    let (conclusion, summary) = w.check('e').await.expect("run");
    assert_eq!(conclusion, "error");
    assert!(
        summary.contains("[FG-AIASSET-0007] json_invalid"),
        "{summary}"
    );
    let (conclusion, summary) = w.check('f').await.expect("run");
    assert_eq!(conclusion, "error");
    assert!(summary.contains("stored_digest_mismatch"), "{summary}");
    w.t.drop_now().await;
}

/// The comparison: success with the table, failure on a regression.
#[tokio::test]
async fn the_report_is_compared_with_the_baseline_and_tabled() {
    let w = world().await;
    let base = baselines(report_json(
        &[("a/1", "regular", 3, 3), ("a/2", "regular", 1, 3)],
        0.8,
    ));
    let good = w.pipeline('1', true, "succeeded").await;
    w.upload(
        good,
        &report_json(&[("a/1", "regular", 3, 3), ("a/2", "regular", 0, 3)], 0.8),
    )
    .await;
    let bad = w.pipeline('2', true, "succeeded").await;
    w.upload(bad, &report_json(&[("a/1", "regular", 1, 3)], 0.8))
        .await;
    let pass = w.sweep(&base).await;
    assert_eq!(pass.written, 2);
    let (conclusion, summary) = w.check('1').await.expect("run");
    assert_eq!(conclusion, "success", "{summary}");
    assert!(
        summary.contains("| `a/2` | regular | 1/3 | 0/3 | flaky |"),
        "{summary}"
    );
    let (conclusion, summary) = w.check('2').await.expect("run");
    assert_eq!(conclusion, "failure");
    assert!(summary.contains("stable_case_regressed `a/1`"), "{summary}");
    // Reported: the next pass has nothing to do.
    assert_eq!(w.sweep(&base).await, AiEvalPass::default());
    w.t.drop_now().await;
}

/// forge-db keeps its own copy of the path (it does not depend on
/// forge-ci): both say the same.
#[test]
fn the_report_path_is_forge_cis() {
    assert_eq!(
        forge_db::ci_ai_eval::REPORT_PATH,
        forge_ci::EVAL_REPORT_PATH
    );
}
