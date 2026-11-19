//! Plan 18 S5 review: the merge request pipeline's rules against a migrated
//! database — the default (`OnNewHead`) gives a head one pipeline across
//! target moves, a head whose first pipeline's create failed for good gets
//! one on the next merge result, a person's ask is deduplicated against the
//! pipeline itself, and a consumed ask is taken off the live jobs.
#![allow(clippy::expect_used)] // tests may fail loudly

use forge_core::ids::{MergeRequestId, ProjectId, UserId};
use forge_core::{MergeabilityStatus, ObjectId, RefName, RequestId};

use crate::change::{Actor, ChangeContext};
use crate::projects::{self, NewProject};
use crate::review::{
    open_mr_decided, record_check, record_merge_result, record_mr_version, request_check,
    run_merge_request_pipeline_decided, CheckResult, MergePipelineOutcome, MergeRequestPipeline,
    NewMergeRequest, Recorded,
};
use crate::test_support::FreshDb;
use crate::users::{self, NewUser};

fn system(request: &str) -> ChangeContext {
    ChangeContext::new(
        Actor::System,
        RequestId::parse(request).expect("request id"),
    )
}

fn oid(c: char) -> ObjectId {
    ObjectId::parse(&c.to_string().repeat(40)).expect("oid")
}

fn branch(name: &str) -> RefName {
    RefName::parse(&format!("refs/heads/{name}")).expect("branch")
}

struct World {
    t: FreshDb,
    alice: UserId,
    project: ProjectId,
    mr: MergeRequestId,
    iid: i64,
}

/// alice's project and her merge request `feature` → `main` at head `e…`.
async fn world() -> World {
    let t = FreshDb::migrated().await;
    let alice = users::create(
        &t.db,
        &system("req-user-alice"),
        &NewUser::human("alice", "alice", "alice@example.test"),
    )
    .await
    .expect("alice");
    let web = projects::create(
        &t.db,
        &system("req-project-web"),
        &NewProject::new("web", alice.namespace_id, "Web", alice.user_id),
    )
    .await
    .expect("web");
    let (mr, ()) = open_mr_decided(
        &t.db,
        &ChangeContext::new(
            Actor::User(alice.user_id),
            RequestId::parse("req-open").expect("id"),
        ),
        web.project_id,
        alice.user_id,
        &NewMergeRequest {
            title: "Feature".to_owned(),
            description: String::new(),
            source_branch: branch("feature"),
            target_branch: branch("main"),
            draft: false,
        },
        |_| Ok(()),
    )
    .await
    .expect("open");
    record_mr_version(
        &t.db,
        &system("req-v1"),
        mr.id,
        &oid('e'),
        Some(alice.user_id),
    )
    .await
    .expect("version");
    World {
        t,
        alice: alice.user_id,
        project: web.project_id,
        mr: mr.id,
        iid: mr.iid,
    }
}

impl World {
    /// The head `e…` merged into `target` as `result`, by `pipeline`.
    async fn record(&self, (target, result): (char, char), pipeline: MergeRequestPipeline) -> bool {
        record_merge_result(
            &self.t.db,
            &RequestId::parse("req-mr").expect("id"),
            self.mr,
            (&oid('e'), &oid(target)),
            &oid(result),
            pipeline,
        )
        .await
        .expect("record")
        .expect("open")
        .pipeline_enqueued
    }

    /// The commits of the merge request's `ci.pipeline_create` jobs that are
    /// not dead, oldest first.
    async fn pipeline_jobs(&self) -> Vec<String> {
        self.t
            .json_rows(
                "SELECT payload FROM forge.jobs WHERE kind = 'ci.pipeline_create' \
                 AND state <> 'dead' ORDER BY id",
            )
            .await
            .iter()
            .map(|r| r["payload"]["sha"].as_str().expect("sha").to_owned())
            .collect()
    }

    async fn count(&self, sql: &str) -> i64 {
        self.t.json_rows(sql).await[0]["n"].as_i64().expect("count")
    }

    /// The create job of `result` ends as `state` (as the runner leaves it).
    async fn create_job_ends(&self, result: char, state: &str) {
        self.t
            .exec_superuser(&format!(
                "UPDATE forge.jobs SET state = '{state}', finished_at = now(), \
                 last_error_code = CASE WHEN '{state}' = 'dead' THEN 'FG-CI-0002' END \
                 WHERE kind = 'ci.pipeline_create' AND payload ->> 'sha' = '{}'",
                oid(result).as_str()
            ))
            .await;
    }

    /// The pipeline of `result` as the create job writes it.
    async fn pipeline_row(&self, result: char, status: &str) {
        self.t
            .exec_superuser(&format!(
                "INSERT INTO forge.ci_pipelines (project_id, ref, sha, source, merge_request_id, \
                 copy_mark, status, error_code, request_id) VALUES ({}, 'refs/heads/feature', \
                 '{}', 'merge_request', {}, 'open', '{status}', \
                 CASE WHEN '{status}' = 'error' THEN 'FG-CI-0002' END, 'req-test-pipeline')",
                self.project.get(),
                oid(result).as_str(),
                self.mr.get()
            ))
            .await;
    }

    /// alice's ask for the pipeline on the current target.
    async fn ask(&self, request: &str) -> MergePipelineOutcome {
        let (run, ()) = run_merge_request_pipeline_decided(
            &self.t.db,
            &ChangeContext::new(
                Actor::User(self.alice),
                RequestId::parse(request).expect("id"),
            ),
            (self.project, self.iid),
            self.alice,
            |_| Ok(()),
        )
        .await
        .expect("ask");
        run.outcome
    }

    /// A computed `can_be_merged` of the head at `target`.
    async fn checked_at(&self, target: char) {
        let seq = request_check(&self.t.db, self.mr, RequestId::parse("req-q").expect("id"))
            .await
            .expect("request")
            .expect("open");
        let stored = record_check(
            &self.t.db,
            &system("req-check"),
            self.mr,
            seq,
            &CheckResult {
                status: MergeabilityStatus::CanBeMerged,
                reason: None,
                head: Some(oid('e')),
                target: Some(oid(target)),
                conflicts: Vec::new(),
                conflicts_truncated: false,
                paths: None,
            },
        )
        .await
        .expect("check");
        assert!(matches!(stored, Recorded::Stored { .. }));
    }
}

/// Item 4: by default a head gets one merge request pipeline — its first
/// merge result's — and a target move's merge result gets none.
#[tokio::test]
async fn the_default_gives_a_head_one_pipeline_across_target_moves() {
    let w = world().await;
    assert!(w.record(('1', 'a'), MergeRequestPipeline::OnNewHead).await);
    assert!(!w.record(('2', 'c'), MergeRequestPipeline::OnNewHead).await);
    assert_eq!(w.pipeline_jobs().await, ["a".repeat(40)]);
    w.t.drop_now().await;
}

/// Item 2: the head's first pipeline's create failed for good (its job
/// dead, an `error` row): the next merge result gets one. Once a pipeline
/// exists (its job purged since), the next target move gets none.
#[tokio::test]
async fn a_head_whose_first_pipeline_failed_gets_one_on_the_next_target_move() {
    let w = world().await;
    assert!(w.record(('1', 'a'), MergeRequestPipeline::OnNewHead).await);
    w.create_job_ends('a', "dead").await;
    w.pipeline_row('a', "error").await;
    assert!(
        w.record(('2', 'c'), MergeRequestPipeline::OnNewHead).await,
        "the failed head gets its pipeline"
    );
    assert_eq!(w.pipeline_jobs().await, ["c".repeat(40)]);
    w.create_job_ends('c', "succeeded").await;
    w.pipeline_row('c', "succeeded").await;
    w.t.exec_superuser("DELETE FROM forge.jobs WHERE kind = 'ci.pipeline_create'")
        .await;
    assert!(
        !w.record(('3', 'd'), MergeRequestPipeline::OnNewHead).await,
        "its pipeline row answers once the job is purged"
    );
    w.t.drop_now().await;
}

/// Item 1: an ask is deduplicated against the pipeline, not only its job —
/// with the pipeline written and its job gone (purged, or dead), the next
/// press is `exists` and writes no job and no audit row.
#[tokio::test]
async fn a_press_after_the_pipeline_exists_writes_nothing() {
    let w = world().await;
    assert!(w.record(('1', 'a'), MergeRequestPipeline::OnNewHead).await);
    assert!(!w.record(('2', 'c'), MergeRequestPipeline::OnNewHead).await);
    w.checked_at('2').await;
    assert_eq!(w.ask("req-ask-1").await, MergePipelineOutcome::Queued);
    w.create_job_ends('c', "succeeded").await;
    w.pipeline_row('c', "succeeded").await;
    w.t.exec_superuser(&format!(
        "DELETE FROM forge.jobs WHERE kind = 'ci.pipeline_create' AND payload ->> 'sha' = '{}'",
        oid('c').as_str()
    ))
    .await;
    let jobs = "SELECT count(*) AS n FROM forge.jobs WHERE kind = 'ci.pipeline_create'";
    let audit =
        "SELECT count(*) AS n FROM forge.audit_log WHERE action = 'pipeline.run_merge_request'";
    let (jobs_before, audit_before) = (w.count(jobs).await, w.count(audit).await);
    assert_eq!(audit_before, 1);
    assert_eq!(w.ask("req-ask-2").await, MergePipelineOutcome::Exists);
    assert_eq!(w.count(jobs).await, jobs_before, "no new job");
    assert_eq!(w.count(audit).await, audit_before, "no new audit row");
    w.t.drop_now().await;
}

/// Item 3: the ask a check job carries is consumed by the merge result
/// written for it — taken off the merge request's live asked jobs, so a
/// later request does not inherit it.
#[tokio::test]
async fn a_consumed_ask_is_taken_off_the_live_jobs() {
    let w = world().await;
    assert_eq!(w.ask("req-ask-1").await, MergePipelineOutcome::Computing);
    let asked = "SELECT count(*) AS n FROM forge.jobs WHERE kind = 'merge_request.mergeability' \
                 AND state = 'queued' AND payload ->> 'pipeline' = 'true'";
    assert_eq!(w.count(asked).await, 1);
    assert!(w.record(('1', 'a'), MergeRequestPipeline::Requested).await);
    assert_eq!(w.count(asked).await, 0, "the ask is consumed");
    let later = request_check(&w.t.db, w.mr, RequestId::parse("req-later").expect("id"))
        .await
        .expect("request")
        .expect("open");
    let rows =
        w.t.json_rows(&format!(
            "SELECT payload FROM forge.jobs WHERE kind = 'merge_request.mergeability' \
             AND state = 'queued' AND (payload ->> 'check_seq')::bigint = {later}"
        ))
        .await;
    assert_eq!(rows.len(), 1);
    assert!(rows[0]["payload"].get("pipeline").is_none(), "{rows:?}");
    w.t.drop_now().await;
}
