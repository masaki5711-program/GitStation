#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)

//! What `forge doctor` reads about the AI evaluation bench (plan 17 P17-8)
//! against a migrated database: only `gpu-eval` runners and jobs count for
//! the runner, only scheduled declaring pipelines of a project with an
//! active schedule count as the nightly bench, and only open merge requests
//! from a baseline candidate branch count as candidates.

use forge_core::{ObjectId, RefName, RequestId};

use super::*;
use crate::change::{Actor, ChangeContext};
use crate::pipelines::{self, CopyMark, NewCiJob, NewPipeline, PipelineRequest, Planned, Source};
use crate::projects::{self, CreatedProject, NewProject};
use crate::test_support::*;
use crate::users::{self, Created as CreatedUser, NewUser};

fn ctx(request: &str) -> ChangeContext {
    ChangeContext::new(
        Actor::System,
        RequestId::parse(request).expect("request id"),
    )
}

struct World {
    t: FreshDb,
    alice: CreatedUser,
    web: CreatedProject,
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
    let web = projects::create(
        &t.db,
        &ctx("req-project-web"),
        &NewProject::new("web", alice.namespace_id, "Web", alice.user_id),
    )
    .await
    .expect("web");
    World { t, alice, web }
}

fn job(name: &str, tags: &[&str]) -> NewCiJob {
    NewCiJob {
        name: name.to_owned(),
        stage: "test".to_owned(),
        wave: 0,
        needs: Vec::new(),
        image: format!("registry.internal/bench@sha256:{}", "a".repeat(64)),
        timeout_s: 600,
        tags: tags.iter().map(|t| (*t).to_owned()).collect(),
        secrets: Vec::new(),
        manual: false,
        artifacts_keep: false,
    }
}

impl World {
    fn p(&self) -> i64 {
        self.web.project_id.get()
    }

    /// A pipeline at commit `c` whose `bench` job asks for `gpu-eval` and
    /// declares the report.
    async fn bench(&self, c: char, source: Source) -> i64 {
        let new = NewPipeline {
            ai_eval_job: Some("bench".to_owned()),
            request: PipelineRequest {
                project: self.web.project_id,
                git_ref: RefName::parse("refs/heads/main").expect("ref"),
                sha: ObjectId::parse(&c.to_string().repeat(40)).expect("sha"),
                source,
                merge_request: None,
                triggered_by: None,
            },
            request_id: "req-plan".to_owned(),
            definition_hash: Some(pipelines::definition_hash(b"version: 1")),
            protected: false,
            copy_mark: CopyMark::Open,
            planned: Planned::Jobs(vec![job("bench", &[GPU_EVAL_TAG])]),
        };
        pipelines::create(&self.t.db, &new)
            .await
            .expect("pipeline")
            .pipeline_id
    }

    /// `pipeline` ends in `status`, created `hours_ago`.
    async fn age(&self, pipeline: i64, status: &str, hours_ago: i64) {
        self.t
            .exec_superuser_tx(&[format!(
                "UPDATE forge.ci_pipelines SET status = '{status}', \
                 created_at = now() - interval '{hours_ago} hours' WHERE id = {pipeline}"
            )])
            .await;
    }

    async fn runner(&self, id: i64, tags: &str, enabled: bool, contact: &str) {
        self.t
            .commit_audited(
                ("ci_runner", id),
                &[format!(
                    "INSERT INTO forge.ci_runners (id, public_id, secret_sha256, name, tags, \
                       enabled, created_by, last_contact_at) OVERRIDING SYSTEM VALUE \
                     VALUES ({id}, '{:016x}', decode(repeat('cd', 32), 'hex'), 'runner-{id}', \
                       '{tags}', {enabled}, {}, {contact})",
                    id,
                    self.alice.user_id.get()
                )],
            )
            .await;
    }

    async fn schedule(&self, active: bool) {
        let next = if active { "now()" } else { "NULL" };
        self.t
            .commit_audited(
                ("project", self.p()),
                &[format!(
                    "INSERT INTO forge.ci_pipeline_schedules (project_id, description, ref, \
                       cron, utc_offset_minutes, active, owner_id, created_by, next_run_at) \
                     VALUES ({}, 'nightly bench', 'refs/heads/main', '0 2 * * *', 420, \
                       {active}, {u}, {u}, {next})",
                    self.p(),
                    u = self.alice.user_id.get()
                )],
            )
            .await;
    }

    async fn merge_request(&self, iid: i64, branch: &str, state: &str, days_ago: i64) {
        let p = self.p();
        let (closed_at, closed_by) = if state == "closed" {
            ("now()".to_owned(), self.alice.user_id.get().to_string())
        } else {
            ("NULL".to_owned(), "NULL".to_owned())
        };
        self.t
            .commit_audited(
                ("project", p),
                &[
                    format!(
                        "INSERT INTO forge.merge_request_counters (project_id, last_iid) \
                         VALUES ({p}, {iid}) ON CONFLICT (project_id) DO UPDATE SET last_iid = {iid}"
                    ),
                    format!(
                        "INSERT INTO forge.merge_requests (project_id, iid, title, author_id, \
                         source_branch, target_branch, state, head_sha, closed_at, closed_by, \
                         created_at) VALUES ({p}, {iid}, 'synthetic', {}, '{branch}', \
                         'refs/heads/main', '{state}', '{}', {closed_at}, {closed_by}, \
                         now() - interval '{days_ago} days')",
                        self.alice.user_id.get(),
                        "e".repeat(40)
                    ),
                ],
            )
            .await;
    }

    async fn enable_ai_assets(&self) {
        self.t
            .commit_audited(
                ("project", self.p()),
                &[format!(
                    "INSERT INTO forge.project_ai_assets (project_id, enabled, changed_by) \
                     VALUES ({}, true, {})",
                    self.p(),
                    self.alice.user_id.get()
                )],
            )
            .await;
    }
}

#[tokio::test]
async fn an_unused_bench_reads_as_nothing() {
    let w = world().await;
    assert_eq!(
        runner_health(&w.t.db).await.expect("runner"),
        RunnerHealth::default()
    );
    assert_eq!(
        baseline_health(&w.t.db).await.expect("baseline"),
        BaselineHealth::default()
    );
    w.t.drop_now().await;
}

/// Only enabled `gpu-eval` runners count; a `gpu-eval` job pending past
/// the bound is waiting, a fresh one is not.
#[tokio::test]
async fn only_gpu_eval_runners_and_jobs_count() {
    let w = world().await;
    w.runner(9001, "{gpu-eval}", true, "now() - interval '1 hour'")
        .await;
    w.runner(9002, "{gpu-eval}", false, "now()").await;
    w.runner(9003, "{linux}", true, "now()").await;
    let old = w.bench('a', Source::Push).await;
    let fresh = w.bench('b', Source::Manual).await;
    w.t.exec_superuser_tx(&[
        format!(
            "UPDATE forge.ci_jobs SET status = 'pending', \
             updated_at = now() - interval '20 minutes' WHERE pipeline_id = {old}"
        ),
        format!("UPDATE forge.ci_jobs SET status = 'pending' WHERE pipeline_id = {fresh}"),
    ])
    .await;
    let h = runner_health(&w.t.db).await.expect("runner");
    assert_eq!(h.declaring_pipelines, 2);
    assert_eq!((h.gpu_runners, h.live_gpu_runners), (1, 0));
    assert_eq!(h.waiting, 1);
    let job: i64 =
        w.t.json_rows(&format!(
            "SELECT id FROM forge.ci_jobs WHERE pipeline_id = {old}"
        ))
        .await[0]["id"]
            .as_i64()
            .expect("job id");
    assert_eq!(h.oldest_job_id, Some(job));
    assert!(h.oldest_wait_s.expect("wait") >= 20 * 60);
    w.t.exec_superuser_tx(&[
        "INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, \
         resource_id, request_id) VALUES ('system', 'probe.changed', 'succeeded', \
         'ci_runner', 9001, 'r-fixture')"
            .to_owned(),
        "UPDATE forge.ci_runners SET last_contact_at = now() WHERE id = 9001".to_owned(),
    ])
    .await;
    assert_eq!(
        runner_health(&w.t.db)
            .await
            .expect("runner")
            .live_gpu_runners,
        1
    );
    w.t.drop_now().await;
}

/// The nightly bench counts only with an active schedule; a last success
/// older than two nights is stale.
#[tokio::test]
async fn a_nightly_without_a_recent_success_is_stale() {
    let w = world().await;
    let night = w.bench('a', Source::Schedule).await;
    w.age(night, "succeeded", 72).await;
    // No active schedule: not a nightly project.
    w.schedule(false).await;
    assert_eq!(
        baseline_health(&w.t.db).await.expect("baseline"),
        BaselineHealth::default()
    );
    w.schedule(true).await;
    let h = baseline_health(&w.t.db).await.expect("baseline");
    assert_eq!(h.nightly_projects, 1);
    assert_eq!((h.stale_count, h.stale_projects.clone()), (1, vec![w.p()]));
    // A failed night does not refresh it; a succeeded one does.
    let failed = w.bench('b', Source::Schedule).await;
    w.age(failed, "failed", 2).await;
    assert_eq!(
        baseline_health(&w.t.db)
            .await
            .expect("baseline")
            .stale_count,
        1
    );
    let passed = w.bench('c', Source::Schedule).await;
    w.age(passed, "succeeded", 20).await;
    let h = baseline_health(&w.t.db).await.expect("baseline");
    assert_eq!((h.stale_count, h.stale_projects.len()), (0, 0));
    assert!(h.fresh_oldest_s.expect("age") >= 20 * 3600);
    w.t.drop_now().await;
}

/// Only open merge requests from a candidate branch count; the oldest is
/// reported, and is stale past two weekly rounds.
#[tokio::test]
async fn the_oldest_open_baseline_candidate_is_reported() {
    let w = world().await;
    w.merge_request(1, "refs/heads/feature", "open", 40).await;
    w.merge_request(2, "refs/heads/ai-eval/baseline-old", "closed", 30)
        .await;
    w.merge_request(3, "refs/heads/ai-eval/baseline-qwen3-14b", "open", 20)
        .await;
    w.merge_request(4, "refs/heads/ai-eval/baseline-qwen3-32b", "open", 1)
        .await;
    let h = baseline_health(&w.t.db).await.expect("baseline");
    assert_eq!(h.open_candidates, 2);
    let (project, iid, age) = h.oldest_candidate.expect("candidate");
    assert_eq!((project, iid), (w.p(), 3));
    assert!(age >= 20 * 24 * 3600);
    assert_eq!(h.stale_candidate().map(|c| c.1), Some(3));
    w.t.drop_now().await;
}

/// A project with `ai_assets.enabled` and no nightly bench is listed.
#[tokio::test]
async fn an_ai_asset_project_without_a_nightly_is_listed() {
    let w = world().await;
    w.enable_ai_assets().await;
    assert_eq!(
        baseline_health(&w.t.db)
            .await
            .expect("baseline")
            .enabled_without_nightly,
        vec![w.p()]
    );
    let night = w.bench('a', Source::Schedule).await;
    w.age(night, "succeeded", 1).await;
    assert!(baseline_health(&w.t.db)
        .await
        .expect("baseline")
        .enabled_without_nightly
        .is_empty());
    w.t.drop_now().await;
}

#[test]
fn a_candidate_is_stale_only_past_the_bound() {
    let at = |age| BaselineHealth {
        oldest_candidate: Some((1, 2, age)),
        ..BaselineHealth::default()
    };
    assert_eq!(at(CANDIDATE_STALE_AFTER_S).stale_candidate(), None);
    assert_eq!(
        at(CANDIDATE_STALE_AFTER_S + 1).stale_candidate(),
        Some((1, 2, CANDIDATE_STALE_AFTER_S + 1))
    );
}

/// A bench first scheduled less than two nights ago is neither stale nor
/// fresh until it passes once.
#[test]
fn a_first_night_is_not_stale_yet() {
    let h = summarize(
        vec![
            (1, None, NIGHTLY_STALE_AFTER_S - 60),
            (2, None, NIGHTLY_STALE_AFTER_S + 60),
            (3, Some(3600), 5 * 24 * 3600),
        ],
        Vec::new(),
        Vec::new(),
    );
    assert_eq!(h.nightly_projects, 3);
    assert_eq!((h.stale_count, h.stale_projects), (1, vec![2]));
    assert_eq!(h.fresh_oldest_s, Some(3600));
}
