//! The autofix rows against a migrated database (M4 C14): the default-off
//! switch and its audited change, the trigger subscriber (only for an
//! enabled project, once per head), an attempt's start, end and event. The
//! whole path with real git is forge-review's `tests/autofix_git.rs`.

use forge_core::autofix::{AttemptState, AutofixTrigger};
use forge_core::{ObjectId, RefName, RequestId, UserId};

use super::*;
use crate::change::{Actor, ChangeContext};
use crate::pipelines::{CopyMark, NewCiJob, PipelineRequest, Planned, Source};
use crate::projects::{self, CreatedProject, NewProject};
use crate::review::{open_mr_decided, NewMergeRequest, StoredMergeRequest};
use crate::subscribers::{register, run_once, Progress, Start};
use crate::test_support::*;
use crate::users::{self, NewUser};

fn system(request: &str) -> ChangeContext {
    ChangeContext::new(
        Actor::System,
        RequestId::parse(request).expect("request id"),
    )
}

fn person(user: UserId, request: &str) -> ChangeContext {
    ChangeContext::new(
        Actor::User(user),
        RequestId::parse(request).expect("request id"),
    )
}

fn sha(c: char) -> ObjectId {
    ObjectId::parse(&c.to_string().repeat(40)).expect("sha")
}

fn branch(name: &str) -> RefName {
    RefName::parse(&format!("refs/heads/{name}")).expect("branch")
}

async fn web(t: &FreshDb) -> (UserId, CreatedProject, StoredMergeRequest) {
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
    let new = NewMergeRequest {
        title: "Format".to_owned(),
        description: "synthetic".to_owned(),
        source_branch: branch("feature"),
        target_branch: branch("main"),
        draft: false,
    };
    let (mr, ()) = open_mr_decided(
        &t.db,
        &system("req-open"),
        web.project_id,
        alice.user_id,
        &new,
        |_| Ok(()),
    )
    .await
    .expect("open");
    (alice.user_id, web, mr)
}

async fn enable(t: &FreshDb, project: ProjectId, user: UserId) {
    let (outcome, s, ()) =
        set_enabled_decided(&t.db, &person(user, "req-on"), project, user, true, |_| {
            Ok(())
        })
        .await
        .expect("on");
    assert_eq!(outcome, ChangeOutcome::Changed);
    assert!(s.enabled);
}

/// One published event of `event_type` with `payload` (and its audit row).
async fn publish(t: &FreshDb, project: ProjectId, event_type: &str, payload: serde_json::Value) {
    t.exec_superuser(&format!(
        "INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id) \
         VALUES ('system', '{event_type}', 'succeeded', 'project', 'req-ev'); \
         INSERT INTO forge.events (event_id, event_type, schema_version, aggregate_type, \
         aggregate_id, project_id, actor_kind, request_id, payload) VALUES (gen_random_uuid(), \
         '{event_type}', 1, 'merge_request', 1, {}, 'system', 'req-ev', '{payload}')",
        project.get()
    ))
    .await;
    crate::sequencer::run_once(
        &t.db,
        crate::sequencer::BatchSize::MAX,
        crate::sequencer::AuditSequencing::Skip,
    )
    .await
    .expect("sequence");
}

async fn drain(t: &FreshDb) {
    loop {
        match run_once(&t.db, &AutofixTriggers).await.expect("step") {
            Progress::Handled { .. } => {}
            Progress::CaughtUp => break,
            other @ (Progress::Busy
            | Progress::Disabled
            | Progress::Failed(_)
            | Progress::Stuck(_)
            | Progress::Quarantined(_)) => panic!("{other:?}"),
        }
    }
}

async fn attempts(t: &FreshDb) -> Vec<serde_json::Value> {
    t.json_rows(
        "SELECT trigger, head_sha, target_sha, state, fixer FROM forge.autofix_attempts ORDER BY id",
    )
    .await
}

/// The task: autofix is off by default; a Maintainer's change is audited.
#[tokio::test]
async fn autofix_is_off_by_default_and_turning_it_on_is_audited() {
    let t = FreshDb::migrated().await;
    let (alice, web, _) = web(&t).await;
    let s = settings(&t.db, web.project_id).await.expect("read");
    assert!(!s.enabled);
    assert_eq!(s.changed_by, None);
    enable(&t, web.project_id, alice).await;
    let s = settings(&t.db, web.project_id).await.expect("read");
    assert_eq!((s.enabled, s.changed_by), (true, Some(alice)));
    let (again, _, ()) = set_enabled_decided(
        &t.db,
        &person(alice, "req-on-again"),
        web.project_id,
        alice,
        true,
        |_| Ok(()),
    )
    .await
    .expect("again");
    assert_eq!(again, ChangeOutcome::Unchanged);
    let rows = t
        .json_rows(
            "SELECT action, details FROM forge.audit_log WHERE action = 'autofix.settings_changed'",
        )
        .await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["details"]["enabled"], true);
    // A refused decision writes nothing.
    let err = set_enabled_decided(
        &t.db,
        &person(alice, "req-off-refused"),
        web.project_id,
        alice,
        false,
        |_| -> Result<(), ForgeError> { Err(ForgeError::new(ErrorCode::Forbidden)) },
    )
    .await
    .expect_err("refused");
    assert_eq!(err.code(), ErrorCode::Forbidden);
    assert!(settings(&t.db, web.project_id).await.expect("read").enabled);
    t.drop_now().await;
}

/// A conflict queues one attempt, once per head and target, and only for
/// a project whose autofix is on (default off: nothing).
#[tokio::test]
async fn a_conflict_queues_one_attempt_only_when_autofix_is_on() {
    let t = FreshDb::migrated().await;
    let (alice, web, mr) = web(&t).await;
    let p = web.project_id;
    register(&t.db, &AutofixTriggers, Start::Beginning)
        .await
        .expect("register");
    let conflict = |head: char| {
        serde_json::json!({
            "merge_request_id": mr.id.get(), "iid": mr.iid, "check_seq": 1,
            "head_sha": sha(head).as_str(), "target_sha": sha('b').as_str(),
            "conflict_files": 1, "files_truncated": false,
        })
    };
    publish(&t, p, "merge_request.conflict_detected", conflict('a')).await;
    drain(&t).await;
    assert!(attempts(&t).await.is_empty(), "off by default");
    enable(&t, p, alice).await;
    publish(&t, p, "merge_request.conflict_detected", conflict('a')).await;
    publish(&t, p, "merge_request.conflict_detected", conflict('a')).await;
    drain(&t).await;
    let rows = attempts(&t).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["trigger"], "conflict");
    assert_eq!(rows[0]["state"], "queued");
    assert_eq!(rows[0]["fixer"], serde_json::Value::Null);
    t.drop_now().await;
}

/// A failed `ci/pipeline` on the merge request's current merge result
/// queues a pipeline attempt; one on another commit does not.
#[tokio::test]
async fn a_failed_merge_result_pipeline_queues_a_pipeline_attempt() {
    let t = FreshDb::migrated().await;
    let (alice, web, mr) = web(&t).await;
    let p = web.project_id;
    enable(&t, p, alice).await;
    register(&t.db, &AutofixTriggers, Start::Beginning)
        .await
        .expect("register");
    t.commit_audited(
        ("project", p.get()),
        &[
            format!(
                "UPDATE forge.merge_requests SET head_sha = '{}', revision = revision + 1 \
                 WHERE id = {}",
                sha('a').as_str(),
                mr.id.get()
            ),
            format!(
                "INSERT INTO forge.merge_result_refs (merge_request_id, project_id, head_sha, \
                   target_sha, result_sha) VALUES ({}, {}, '{}', '{}', '{}')",
                mr.id.get(),
                p.get(),
                sha('a').as_str(),
                sha('b').as_str(),
                sha('c').as_str()
            ),
        ],
    )
    .await;
    let check = |head: char, conclusion: &str| {
        serde_json::json!({
            "name": "ci/pipeline", "conclusion": conclusion, "head_sha": sha(head).as_str(),
        })
    };
    publish(&t, p, "check.completed", check('d', "failure")).await;
    publish(&t, p, "check.completed", check('c', "success")).await;
    publish(&t, p, "check.completed", check('c', "failure")).await;
    drain(&t).await;
    let rows = attempts(&t).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["trigger"], "pipeline_failed");
    assert_eq!(rows[0]["head_sha"], sha('a').as_str());
    assert_eq!(rows[0]["target_sha"], sha('b').as_str());
    t.drop_now().await;
}

/// An attempt starts its own pipeline (source `autofix`, no merge request
/// link, never protected), ends once with the merge request's event, and
/// the next fixer can be queued on the same head.
#[tokio::test]
async fn an_attempt_runs_its_own_pipeline_and_ends_once_with_its_event() {
    let t = FreshDb::migrated().await;
    let (alice, web, mr) = web(&t).await;
    let p = web.project_id;
    enable(&t, p, alice).await;
    t.exec_superuser(&format!(
        "INSERT INTO forge.autofix_attempts (project_id, merge_request_id, trigger, head_sha, \
           target_sha, state, request_id) VALUES ({}, {}, 'pipeline_failed', '{}', '{}', \
           'queued', 'req-q')",
        p.get(),
        mr.id.get(),
        sha('a').as_str(),
        sha('b').as_str(),
    ))
    .await;
    let queued_now = queued(&t.db, 10).await.expect("queued");
    assert_eq!(queued_now.len(), 1);
    let a = &queued_now[0];
    assert_eq!(
        (a.trigger, a.state, a.iid),
        (AutofixTrigger::PipelineFailed, AttemptState::Queued, mr.iid)
    );
    let new = NewPipeline {
        request: PipelineRequest {
            project: p,
            git_ref: branch("feature"),
            sha: sha('e'),
            source: Source::Autofix,
            merge_request: None,
            triggered_by: None,
        },
        request_id: "req-start".to_owned(),
        definition_hash: Some("0".repeat(64)),
        protected: false,
        copy_mark: CopyMark::Undecided,
        planned: Planned::Jobs(vec![NewCiJob {
            name: "fmt".to_owned(),
            stage: "autofix".to_owned(),
            wave: 0,
            needs: Vec::new(),
            image: format!("registry.internal/rust@sha256:{}", "a".repeat(64)),
            timeout_s: 600,
            tags: Vec::new(),
            secrets: Vec::new(),
            manual: false,
        }]),
        ai_eval_job: None,
    };
    let (pipeline, job) = start(&t.db, a.id, "fmt", &sha('e'), &new)
        .await
        .expect("start")
        .expect("started");
    assert!(start(&t.db, a.id, "fmt", &sha('e'), &new)
        .await
        .expect("again")
        .is_none());
    let rows = t
        .json_rows(&format!(
            "SELECT source, merge_request_id, protected, status FROM forge.ci_pipelines WHERE id = {pipeline}"
        ))
        .await;
    assert_eq!(rows[0]["source"], "autofix");
    assert_eq!(rows[0]["merge_request_id"], serde_json::Value::Null);
    assert_eq!(rows[0]["protected"], false);
    // Not reported as `ci/pipeline` of the input commit.
    let due = crate::ci_outputs::pipelines_due_for_check(&t.db, 100)
        .await
        .expect("due");
    assert!(!due.contains(&pipeline));
    assert!(tried_fixers(&t.db, a)
        .await
        .expect("tried")
        .contains(&"fmt".to_owned()));
    assert_eq!(fixer_runs_today(&t.db, mr.id).await.expect("count"), 1);
    // The job ends: the attempt is among the running ones whose job ended.
    t.exec_superuser(&format!(
        "UPDATE forge.ci_jobs SET status = 'succeeded' WHERE id = {job}"
    ))
    .await;
    let ended = running(&t.db, 10).await.expect("running");
    assert_eq!(ended.len(), 1);
    assert_eq!(ended[0].job_status, "succeeded");
    assert_eq!(ended[0].bundle, None);
    let a = ended[0].attempt.clone();
    let done = Finish {
        state: AttemptState::NoChange,
        reason: Some("no_change"),
        detail: Vec::new(),
        error_code: None,
        commit: None,
        fixer: None,
    };
    assert!(finish(&t.db, &system("req-end"), &a, &done)
        .await
        .expect("finish"));
    assert!(!finish(&t.db, &system("req-end-2"), &a, &done)
        .await
        .expect("again"));
    let events = t
        .json_rows(
            "SELECT dedup_key, payload FROM forge.events \
             WHERE event_type = 'merge_request.autofix_finished'",
        )
        .await;
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["payload"]["state"], "no_change");
    assert_eq!(events[0]["payload"]["fixer"], "fmt");
    let latest_now = latest(&t.db, mr.id).await.expect("latest").expect("one");
    assert_eq!(latest_now.state, AttemptState::NoChange);
    // The next fixer on the same head.
    assert!(requeue(&t.db, &a, &RequestId::generate())
        .await
        .expect("requeue"));
    assert!(!requeue(&t.db, &a, &RequestId::generate())
        .await
        .expect("once"));
    assert!(!is_bot_commit(&t.db, p, &sha('e')).await.expect("bot"));
    t.drop_now().await;
}
