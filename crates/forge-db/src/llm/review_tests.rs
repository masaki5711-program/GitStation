//! The first pass's candidates against a migrated database (M5 L5-5).

use forge_core::checks::{CheckConclusion, CheckStatus, AI_REVIEW_CHECK};
use forge_core::{CheckName, ObjectId, RequestId};

use super::*;
use crate::change::{Actor, ChangeContext};
use crate::checks::NewCheckRun;
use crate::projects::{self, NewProject};
use crate::test_support::*;
use crate::users::{self, NewUser};

fn system(request: &str) -> ChangeContext {
    ChangeContext::new(Actor::System, RequestId::parse(request).expect("id"))
}

struct World {
    t: FreshDb,
    alice: i64,
    ns: forge_core::NamespaceId,
}

async fn world() -> World {
    let t = FreshDb::migrated().await;
    let alice = users::create(
        &t.db,
        &system("req-user-alice"),
        &NewUser::human("alice", "alice", "alice@example.test"),
    )
    .await
    .expect("alice");
    World {
        t,
        alice: alice.user_id.get(),
        ns: alice.namespace_id,
    }
}

/// A project `name` with `llm.review` = `mode` (`None`: no row) and one
/// merge request per `(head, draft, state)`.
async fn project(w: &World, name: &str, mode: Option<&str>, mrs: &[(char, bool, &str)]) -> i64 {
    let created = projects::create(
        &w.t.db,
        &system(&format!("req-p-{name}")),
        &NewProject::new(name, w.ns, name, forge_core::UserId::new(w.alice)),
    )
    .await
    .expect("project");
    let p = created.project_id.get();
    if let Some(mode) = mode {
        w.t.exec_superuser(&format!(
            "INSERT INTO forge.project_llm (project_id, review, changed_by) VALUES ({p}, '{mode}', {})",
            w.alice
        ))
        .await;
    }
    let mut statements = vec![format!(
        "INSERT INTO forge.merge_request_counters (project_id, last_iid) VALUES ({p}, {})",
        mrs.len()
    )];
    for (i, (head, draft, state)) in mrs.iter().enumerate() {
        let iid = i + 1;
        statements.push(format!(
            "INSERT INTO forge.merge_requests (project_id, iid, title, author_id, source_branch, \
               target_branch, head_sha, draft, state, closed_at, closed_by) VALUES ({p}, {iid}, \
               'synthetic', {a}, 'refs/heads/f{iid}', 'refs/heads/main', '{}', {draft}, \
               '{state}', {closed}, {closed_by})",
            head.to_string().repeat(40),
            a = w.alice,
            closed = if *state == "closed" { "now()" } else { "NULL" },
            closed_by = if *state == "closed" {
                w.alice.to_string()
            } else {
                "NULL".to_owned()
            },
        ));
        // The version job's record: the target tip and the merge base.
        statements.push(format!(
            "INSERT INTO forge.merge_request_versions (merge_request_id, project_id, version_no, \
               head_sha, base_sha, merge_base_sha) SELECT id, {p}, 1, head_sha, '{base}', \
               '{base}' FROM forge.merge_requests WHERE project_id = {p} AND iid = {iid}",
            base = "e".repeat(40)
        ));
    }
    w.t.commit_audited(("project", p), &statements).await;
    p
}

fn run(status: CheckStatus, conclusion: Option<CheckConclusion>) -> NewCheckRun {
    NewCheckRun {
        name: CheckName::parse(AI_REVIEW_CHECK).expect("name"),
        status,
        conclusion,
        summary: "synthetic".to_owned(),
        details_url: None,
        annotations: Vec::new(),
    }
}

fn heads(c: &[ReviewCandidate]) -> Vec<String> {
    let mut out: Vec<String> = c.iter().map(|c| c.head_sha.clone()).collect();
    out.sort();
    out
}

#[tokio::test]
async fn only_open_ready_heads_of_reviewing_projects_are_candidates() {
    let w = world().await;
    let off = project(&w, "off", Some("off"), &[('1', false, "open")]).await;
    let _none = project(&w, "none", None, &[('2', false, "open")]).await;
    let on = project(
        &w,
        "on",
        Some("advisory"),
        &[
            ('a', false, "open"),
            ('b', true, "open"),
            ('c', false, "closed"),
        ],
    )
    .await;
    let req = project(&w, "req", Some("required"), &[('d', false, "open")]).await;
    let found = review_candidates(&w.t.db, 20, 2400).await.expect("read");
    assert_eq!(heads(&found), vec!["a".repeat(40), "d".repeat(40)]);
    assert!(found.iter().all(|c| c.project.get() != off));
    let d = found
        .iter()
        .find(|c| c.project.get() == req)
        .expect("required");
    assert_eq!(d.mode, ReviewMode::Required);
    assert_eq!(d.iid, 1);
    let a = found.iter().find(|c| c.project.get() == on).expect("on");
    assert_eq!(a.mode, ReviewMode::Advisory);
    assert_eq!(
        current_head(&w.t.db, a.mr).await.expect("read"),
        Some((Some("a".repeat(40)), true))
    );
}

#[tokio::test]
async fn a_head_with_a_run_is_asked_once_unless_its_wait_went_stale() {
    let w = world().await;
    let on = project(
        &w,
        "on",
        Some("advisory"),
        &[('a', false, "open"), ('b', false, "open")],
    )
    .await;
    let pid = forge_core::ProjectId::new(on);
    let a = ObjectId::parse(&"a".repeat(40)).expect("sha");
    let b = ObjectId::parse(&"b".repeat(40)).expect("sha");
    record_forge_check(
        &w.t.db,
        pid,
        &a,
        &run(CheckStatus::Completed, Some(CheckConclusion::Error)),
    )
    .await;
    record_forge_check(&w.t.db, pid, &b, &run(CheckStatus::Queued, None)).await;
    // A completed run (even `error`) and a fresh wait: nothing to ask.
    assert!(review_candidates(&w.t.db, 20, 2400)
        .await
        .expect("read")
        .is_empty());
    // The wait of a stopped worker goes stale: asked again.
    tokio::time::sleep(std::time::Duration::from_millis(1_500)).await;
    let found = review_candidates(&w.t.db, 20, 1).await.expect("read");
    assert_eq!(heads(&found), vec!["b".repeat(40)]);
}

#[tokio::test]
async fn a_rerun_asks_a_completed_head_again_once() {
    let w = world().await;
    let on = project(&w, "on", Some("advisory"), &[('a', false, "open")]).await;
    let pid = forge_core::ProjectId::new(on);
    let a = ObjectId::parse(&"a".repeat(40)).expect("sha");
    record_forge_check(
        &w.t.db,
        pid,
        &a,
        &run(CheckStatus::Completed, Some(CheckConclusion::Neutral)),
    )
    .await;
    assert!(review_candidates(&w.t.db, 20, 2400)
        .await
        .expect("read")
        .is_empty());
    let alice = forge_core::UserId::new(w.alice);
    let queued = super::super::ops::request_rerun(
        &w.t.db,
        &ChangeContext::new(
            Actor::User(alice),
            RequestId::parse("req-rerun").expect("id"),
        ),
        pid,
        1,
        alice,
        |_| Ok(()),
    )
    .await
    .expect("rerun")
    .0;
    assert!(matches!(queued, super::super::ops::Rerun::Queued { .. }));
    let found = review_candidates(&w.t.db, 20, 2400).await.expect("read");
    assert_eq!(heads(&found), vec!["a".repeat(40)]);
    // The worker's queued write makes the run newer than the rerun.
    record_forge_check(&w.t.db, pid, &a, &run(CheckStatus::Queued, None)).await;
    assert!(review_candidates(&w.t.db, 20, 2400)
        .await
        .expect("read")
        .is_empty());
    record_forge_check(
        &w.t.db,
        pid,
        &a,
        &run(CheckStatus::Completed, Some(CheckConclusion::Success)),
    )
    .await;
    assert!(review_candidates(&w.t.db, 20, 2400)
        .await
        .expect("read")
        .is_empty());
}
