//! Merge requests against a migrated database (M3 R1a). Inspection uses a
//! superuser connection; `decide` is a plain closure here (forge-review
//! passes `Authorized::reconfirm`, tested there).

use std::collections::BTreeSet;

use forge_core::{EventType, MergeRequestTransition, RequestId, Role};
use serde_json::json;

use super::*;
use crate::change::{Actor, ChangeContext};
use crate::projects::{self, CreatedProject, NewProject};
use crate::test_support::*;
use crate::users::{self, NewUser};

fn system(request: &str) -> ChangeContext {
    ChangeContext::new(
        Actor::System,
        RequestId::parse(request).expect("request id"),
    )
}

fn as_user(user: UserId, request: &str) -> ChangeContext {
    ChangeContext::new(
        Actor::User(user),
        RequestId::parse(request).expect("request id"),
    )
}

fn branch(name: &str) -> RefName {
    RefName::parse(&format!("refs/heads/{name}")).expect("branch")
}

fn new_mr(source: &str, target: &str) -> NewMergeRequest {
    NewMergeRequest {
        title: format!("Merge {source}"),
        description: "synthetic".to_owned(),
        source_branch: branch(source),
        target_branch: branch(target),
        draft: false,
    }
}

/// alice and her project `alice/web`.
async fn web(t: &FreshDb) -> (UserId, CreatedProject) {
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
    (alice.user_id, web)
}

async fn open(
    t: &FreshDb,
    who: UserId,
    project: ProjectId,
    request: &str,
    mr: &NewMergeRequest,
) -> Result<StoredMergeRequest, ForgeError> {
    open_mr_decided(&t.db, &as_user(who, request), project, who, mr, |_| Ok(()))
        .await
        .map(|(stored, ())| stored)
}

/// (event_type, aggregate_type, aggregate_id, project_id, payload, dedup_key)
/// of a request's events.
type EventRow = (
    String,
    String,
    i64,
    Option<i64>,
    serde_json::Value,
    Option<String>,
);

async fn events_of(t: &FreshDb, request: &str) -> Vec<EventRow> {
    sqlx::query_as(
        "SELECT event_type, aggregate_type, aggregate_id, project_id, payload, dedup_key \
         FROM forge.events WHERE request_id = $1 ORDER BY id",
    )
    .bind(request)
    .fetch_all(&mut t.superuser().await)
    .await
    .expect("events")
}

/// (action, resource_type, resource_id, project_id) of a request's audit rows.
async fn audit_of(t: &FreshDb, request: &str) -> Vec<(String, String, Option<i64>, Option<i64>)> {
    sqlx::query_as(
        "SELECT action, resource_type, resource_id, project_id FROM forge.audit_log \
         WHERE request_id = $1 ORDER BY id",
    )
    .bind(request)
    .fetch_all(&mut t.superuser().await)
    .await
    .expect("audit")
}

#[tokio::test]
async fn opening_a_merge_request_numbers_it_per_project_and_publishes_opened() {
    let t = FreshDb::migrated().await;
    let (alice, web) = web(&t).await;
    let p = web.project_id;
    let first = open(&t, alice, p, "req-open-1", &new_mr("feature", "main"))
        .await
        .expect("open");
    assert_eq!(
        (first.iid, first.state, first.draft, first.revision),
        (1, MergeRequestState::Open, false, 1)
    );
    assert_eq!(first.author, alice);
    assert_eq!(first.head_sha, None);
    assert_eq!(first.closed_at, None);
    let second = open(&t, alice, p, "req-open-2", &new_mr("fix", "main"))
        .await
        .expect("second");
    assert_eq!(second.iid, 2);
    assert_eq!(
        audit_of(&t, "req-open-1").await,
        [(
            "merge_request.opened".to_owned(),
            "project".to_owned(),
            Some(p.get()),
            Some(p.get())
        )]
    );
    let id = first.id.get();
    assert_eq!(
        events_of(&t, "req-open-1").await,
        [(
            "merge_request.opened".to_owned(),
            "merge_request".to_owned(),
            id,
            Some(p.get()),
            json!({"merge_request_id": id, "iid": 1, "author_id": alice.get(),
                   "source_branch": "refs/heads/feature", "target_branch": "refs/heads/main",
                   "draft": false}),
            Some(format!("merge_request_opened:{id}:1")),
        )]
    );
    // The reads see what was written.
    assert_eq!(get(&t.db, p, 1).await.expect("get"), Some(first.clone()));
    assert_eq!(get(&t.db, p, 3).await.expect("get"), None);
    let all = list(&t.db, p, None, None, 10).await.expect("list");
    assert_eq!(
        all.iter().map(|m| m.iid).collect::<Vec<_>>(),
        [2, 1],
        "newest first"
    );
    let older = list(&t.db, p, Some(MergeRequestState::Open), Some(2), 10)
        .await
        .expect("page");
    assert_eq!(older, [first]);
    t.drop_now().await;
}

#[tokio::test]
async fn a_second_open_merge_request_for_the_same_branches_is_refused() {
    let t = FreshDb::migrated().await;
    let (alice, web) = web(&t).await;
    let p = web.project_id;
    let first = open(&t, alice, p, "req-a", &new_mr("feature", "main"))
        .await
        .expect("open");
    let err = open(&t, alice, p, "req-b", &new_mr("feature", "main"))
        .await
        .expect_err("duplicate");
    assert_reason(&err, ErrorCode::DuplicateOpenMergeRequest, "open_exists");
    assert!(chain(&err).contains("existing_iid"), "{}", chain(&err));
    assert!(audit_of(&t, "req-b").await.is_empty(), "nothing written");
    // The same source into another target is another merge request.
    open(&t, alice, p, "req-c", &new_mr("feature", "release"))
        .await
        .expect("other target");
    // Closed, the branches are free again; reopening the old one then
    // collides with the new one.
    let (closed, _, ()) = close_mr_decided(
        &t.db,
        &as_user(alice, "req-close"),
        p,
        first.iid,
        alice,
        |_| Ok(()),
    )
    .await
    .expect("close");
    assert_eq!(closed, StateOutcome::Changed);
    let again = open(&t, alice, p, "req-d", &new_mr("feature", "main"))
        .await
        .expect("after close");
    // req-b drew no number (refused before the counter): 1, 2, then 3.
    assert_eq!(again.iid, 3, "numbers are never reused");
    let err = reopen_mr_decided(
        &t.db,
        &as_user(alice, "req-reopen"),
        p,
        first.iid,
        alice,
        None,
        |_| Ok(()),
    )
    .await
    .expect_err("collides");
    assert_reason(&err, ErrorCode::DuplicateOpenMergeRequest, "open_exists");
    // Retargeting another one onto the open pair collides too.
    let err = edit_mr_decided(
        &t.db,
        &as_user(alice, "req-retarget"),
        p,
        2,
        alice,
        &MergeRequestPatch {
            target_branch: Some(branch("main")),
            ..MergeRequestPatch::default()
        },
        |_| Ok(()),
    )
    .await
    .expect_err("collides");
    assert_reason(&err, ErrorCode::DuplicateOpenMergeRequest, "open_exists");
    // The partial unique index is the backstop.
    let mut su = t.superuser().await;
    let backstop = sqlx::query(
        "INSERT INTO forge.merge_requests (project_id, iid, title, author_id, source_branch, \
         target_branch) VALUES ($1, 99, 't', $2, 'refs/heads/feature', 'refs/heads/main')",
    )
    .bind(p.get())
    .bind(alice.get())
    .execute(&mut su)
    .await
    .expect_err("one open per pair");
    assert_eq!(
        backstop.as_database_error().and_then(|d| d.constraint()),
        Some("merge_requests_one_open")
    );
    t.drop_now().await;
}

/// N opens at once on one project (each its own branch): the counter row
/// under the project lock gives 1..=N, no gap, no duplicate.
#[tokio::test]
async fn merge_request_iids_are_gapless_under_concurrent_opens() {
    const N: usize = 8;
    let t = FreshDb::migrated().await;
    let (alice, web) = web(&t).await;
    let p = web.project_id;
    let mut tasks = tokio::task::JoinSet::new();
    for i in 0..N {
        let db = t.db.clone();
        tasks.spawn(async move {
            let mr = new_mr(&format!("topic-{i}"), "main");
            let ctx = as_user(alice, &format!("req-race-{i}"));
            open_mr_decided(&db, &ctx, p, alice, &mr, |_| Ok(()))
                .await
                .map(|(stored, ())| stored.iid)
        });
    }
    let mut iids = BTreeSet::new();
    while let Some(done) = tasks.join_next().await {
        let iid = done.expect("task").expect("open");
        assert!(iids.insert(iid), "iid {iid} twice");
    }
    let expected: BTreeSet<i64> = (1..=i64::try_from(N).expect("small")).collect();
    assert_eq!(iids, expected);
    t.drop_now().await;
}

/// ADR-0024 §6: every transition emits exactly one event, of its own type
/// (a table over `MergeRequestTransition::ALL`, so a transition added
/// without its writer fails here).
#[tokio::test]
async fn every_merge_request_transition_emits_exactly_one_event() {
    let t = FreshDb::migrated().await;
    let (alice, web) = web(&t).await;
    let p = web.project_id;
    let ctx = |r: &str| as_user(alice, r);
    let mut drafted = open(&t, alice, p, "req-seed", &new_mr("seed", "main"))
        .await
        .expect("seed");
    for transition in MergeRequestTransition::ALL {
        let request = format!("req-t-{transition:?}").to_lowercase();
        match transition {
            MergeRequestTransition::Open => {
                drafted = open(&t, alice, p, &request, &new_mr("t-open", "main"))
                    .await
                    .expect("open");
            }
            MergeRequestTransition::Close => {
                let (done, _, ()) =
                    close_mr_decided(&t.db, &ctx(&request), p, drafted.iid, alice, |_| Ok(()))
                        .await
                        .expect("close");
                assert_eq!(done, StateOutcome::Changed);
            }
            MergeRequestTransition::Reopen => {
                let (done, _, ()) =
                    reopen_mr_decided(&t.db, &ctx(&request), p, drafted.iid, alice, None, |_| {
                        Ok(())
                    })
                    .await
                    .expect("reopen");
                assert_eq!(done, StateOutcome::Changed);
            }
            MergeRequestTransition::DraftToggle => {
                let patch = MergeRequestPatch {
                    draft: Some(true),
                    ..MergeRequestPatch::default()
                };
                let (done, _, ()) =
                    edit_mr_decided(&t.db, &ctx(&request), p, drafted.iid, alice, &patch, |_| {
                        Ok(())
                    })
                    .await
                    .expect("draft");
                assert_eq!(
                    done,
                    EditOutcome::Changed(vec![EventType::MergeRequestDraftChanged])
                );
            }
            MergeRequestTransition::Edit => {
                let patch = MergeRequestPatch {
                    title: Some("Renamed".to_owned()),
                    ..MergeRequestPatch::default()
                };
                let (done, _, ()) =
                    edit_mr_decided(&t.db, &ctx(&request), p, drafted.iid, alice, &patch, |_| {
                        Ok(())
                    })
                    .await
                    .expect("edit");
                assert_eq!(
                    done,
                    EditOutcome::Changed(vec![EventType::MergeRequestUpdated])
                );
            }
            // M3 R1b: the version job (actor system) adds a version.
            MergeRequestTransition::Push => {
                let head = forge_core::ObjectId::parse(&"a".repeat(40)).expect("oid");
                let done = record_mr_version(&t.db, &system(&request), drafted.id, &head, None)
                    .await
                    .expect("push");
                assert_eq!(done, VersionOutcome::Added { version: 1 });
            }
            // M3 R2: the mergeability job (actor system) records a flip;
            // the first decision (can) is set up under another request.
            MergeRequestTransition::ConflictDetect | MergeRequestTransition::ConflictResolve => {
                let (setup, now) = if *transition == MergeRequestTransition::ConflictDetect {
                    (
                        Some(forge_core::MergeabilityStatus::CanBeMerged),
                        forge_core::MergeabilityStatus::CannotBeMerged,
                    )
                } else {
                    // After the detect arm the last decision is "cannot".
                    (None, forge_core::MergeabilityStatus::CanBeMerged)
                };
                let oid = |c: char| forge_core::ObjectId::parse(&c.to_string().repeat(40));
                let check = |status| CheckResult {
                    status,
                    reason: None,
                    head: oid('a').ok(),
                    target: oid('b').ok(),
                    conflicts: Vec::new(),
                    conflicts_truncated: false,
                };
                let origin = RequestId::parse("req-setup").expect("id");
                if let Some(first) = setup {
                    let seq = request_check(&t.db, drafted.id, origin.clone())
                        .await
                        .expect("request")
                        .expect("open");
                    record_check(&t.db, &system("req-setup"), drafted.id, seq, &check(first))
                        .await
                        .expect("setup");
                }
                let seq = request_check(&t.db, drafted.id, origin)
                    .await
                    .expect("request")
                    .expect("open");
                let done = record_check(&t.db, &system(&request), drafted.id, seq, &check(now))
                    .await
                    .expect("flip");
                assert_eq!(
                    done,
                    Recorded::Stored {
                        flip: Some(transition.event_type())
                    }
                );
            }
            // M3 X6: the conflict scan (actor system) predicts a conflict
            // with another open merge request on the target: one event per
            // side (each its own aggregate), checked below.
            MergeRequestTransition::ConflictPredict => {
                let oid = |c: char| forge_core::ObjectId::parse(&c.to_string().repeat(40));
                let peer = open(&t, alice, p, "req-peer", &new_mr("t-peer", "main"))
                    .await
                    .expect("peer");
                let peer_head = oid('f').expect("oid");
                record_mr_version(&t.db, &system("req-peer-v1"), peer.id, &peer_head, None)
                    .await
                    .expect("peer version");
                let origin = RequestId::parse("req-scan").expect("id");
                let seq = request_scan(&t.db, drafted.id, origin)
                    .await
                    .expect("request")
                    .expect("open");
                assert_eq!(
                    begin_scan(&t.db, drafted.id, seq).await.expect("begin"),
                    ScanStart::Scan
                );
                let result = ScanResult {
                    status: ScanStatus::Scanned,
                    reason: None,
                    head: oid('a').ok(),
                    target_branch: branch("main"),
                    target_sha: oid('9').ok(),
                    peers: 1,
                    pairs_tried: 1,
                    pairs_skipped: 0,
                    pairs: vec![PairResult {
                        other: peer.id,
                        other_iid: peer.iid,
                        other_head: peer_head,
                        outcome: PairOutcome::Conflict,
                        reason: None,
                        conflicts: vec!["README.md".to_owned()],
                        conflicts_truncated: false,
                    }],
                };
                let done = record_scan(&t.db, &system(&request), drafted.id, seq, &result)
                    .await
                    .expect("scan");
                assert_eq!(
                    done,
                    ScanRecorded::Stored {
                        predicted: vec![peer.id],
                        skipped: 0,
                        status: ScanStatus::Scanned,
                        reason: None,
                    }
                );
            }
            // M3 R3: a note (here the first of a general discussion).
            MergeRequestTransition::Comment => {
                let new = NewDiscussion {
                    body: "synthetic".to_owned(),
                    position: None,
                };
                start_discussion_mr_decided(
                    &t.db,
                    &ctx(&request),
                    p,
                    drafted.iid,
                    alice,
                    &new,
                    |_| Ok(()),
                )
                .await
                .expect("comment");
            }
            // M3 R4: the merge job's record (tx2) after the request, the
            // gate and the fence; the target's repository.pushed comes with
            // it in the same transaction (ADR-0024 §4.2 step 9).
            MergeRequestTransition::Merge => {
                let oid =
                    |c: char| forge_core::ObjectId::parse(&c.to_string().repeat(40)).expect("oid");
                let ask = merges::NewMerge {
                    method: forge_core::MergeMethod::MergeCommit,
                    head_sha: oid('a'),
                    message: None,
                    delete_source_branch: true,
                    credential: merges::CredentialRef::Session(1),
                };
                let (attempt, ()) = merges::request_merge_mr_decided(
                    &t.db,
                    &ctx("req-merge-ask"),
                    (p, drafted.iid),
                    alice,
                    &ask,
                    |_| Ok(()),
                    |_| "synthetic".to_owned(),
                )
                .await
                .expect("ask");
                let step = merges::GateStep::Prepare {
                    expected_old: oid('b'),
                    new_sha: oid('c'),
                };
                let prepared = merges::merge_gate_decided(
                    &t.db,
                    &system("req-merge-prepare"),
                    attempt.id,
                    step,
                    |_| Ok(()),
                )
                .await
                .expect("prepare");
                assert!(matches!(prepared, merges::GateOutcome::Decided(())));
                assert!(merges::fence_merge(&t.db, attempt.id).await.expect("fence"));
                let attempt = merges::merge_attempt(&t.db, attempt.id)
                    .await
                    .expect("read")
                    .expect("row");
                let push = forge_core::PushRecord {
                    repository_id: web.repository_id,
                    updates: vec![forge_core::PushedRef {
                        name: branch("main"),
                        old: oid('b'),
                        new: oid('c'),
                        kind: forge_core::PushUpdateKind::FastForward,
                        commits: vec![oid('c')],
                        total_commits: 1,
                        total_commits_partial: false,
                    }],
                };
                let done = merges::finish_merge(&t.db, &ctx(&request), &attempt, &push, Some(&[]))
                    .await
                    .expect("finish");
                assert!(matches!(done, merges::MergeFinished::Recorded(_)));
                let again = merges::finish_merge(
                    &t.db,
                    &ctx("req-merge-again"),
                    &attempt,
                    &push,
                    Some(&[]),
                )
                .await
                .expect("again");
                assert_eq!(again, merges::MergeFinished::AlreadyApplied);
            }
            MergeRequestTransition::SourceBranchDelete => {
                let attempt = merges::latest_merge(&t.db, drafted.id)
                    .await
                    .expect("read")
                    .expect("the merge above");
                let deletion = merges::SourceDeletion {
                    repo: web.repository_id,
                    source_branch: drafted.source_branch.clone(),
                    old: forge_core::ObjectId::parse(&"a".repeat(40)).expect("oid"),
                };
                merges::record_source_deleted(
                    &t.db,
                    &ctx(&request),
                    &attempt,
                    drafted.iid,
                    &deletion,
                )
                .await
                .expect("deleted");
            }
            // M3 R5: an approval and its withdrawal (forge-review decides
            // who may; the command records).
            MergeRequestTransition::Approve => {
                let (done, _, ()) = approve_mr_decided(
                    &t.db,
                    &ctx(&request),
                    (p, drafted.iid),
                    alice,
                    |_| Ok(()),
                    |_, _, _| (true, None),
                )
                .await
                .expect("approve");
                assert!(matches!(done, ApproveOutcome::Approved { .. }));
            }
            MergeRequestTransition::Unapprove => {
                let (done, _, ()) = unapprove_mr_decided(
                    &t.db,
                    &ctx(&request),
                    (p, drafted.iid),
                    alice,
                    |_| Ok(()),
                )
                .await
                .expect("unapprove");
                assert_eq!(done, ApproveOutcome::Withdrawn);
            }
        }
        let events = events_of(&t, &request).await;
        // M3 R4: a merge and a source deletion also record the ref update
        // (`repository.pushed`, its own aggregate); exactly one event of the
        // merge request.
        let pushes = events.iter().filter(|e| e.0 == "repository.pushed").count();
        let carries_push = matches!(
            transition,
            MergeRequestTransition::Merge | MergeRequestTransition::SourceBranchDelete
        );
        assert_eq!(pushes, usize::from(carries_push), "{transition:?}");
        let types: Vec<&str> = events
            .iter()
            .map(|e| e.0.as_str())
            .filter(|t| *t != "repository.pushed")
            .collect();
        // M3 X6: a predicted conflict is about two merge requests: exactly
        // one event (and audit row) for each of them.
        let sides = if *transition == MergeRequestTransition::ConflictPredict {
            let aggregates: BTreeSet<i64> = events.iter().map(|e| e.2).collect();
            assert_eq!(aggregates.len(), 2, "one event per side: {events:?}");
            2
        } else {
            1
        };
        assert_eq!(
            types,
            vec![transition.event_type().as_str(); sides],
            "{transition:?} must emit exactly one event per merge request"
        );
        assert_eq!(
            audit_of(&t, &request).await.len(),
            sides + usize::from(carries_push),
            "{transition:?}"
        );
    }
    t.drop_now().await;
}

#[tokio::test]
async fn an_edit_writes_one_row_and_event_per_transition_or_nothing() {
    let t = FreshDb::migrated().await;
    let (alice, web) = web(&t).await;
    let p = web.project_id;
    let mr = open(&t, alice, p, "req-open", &new_mr("feature", "main"))
        .await
        .expect("open");
    let unchanged = MergeRequestPatch {
        title: Some(mr.title.clone()),
        draft: Some(false),
        target_branch: Some(branch("main")),
        ..MergeRequestPatch::default()
    };
    let (done, _, ()) = edit_mr_decided(
        &t.db,
        &as_user(alice, "req-same"),
        p,
        1,
        alice,
        &unchanged,
        |_| Ok(()),
    )
    .await
    .expect("same");
    assert_eq!(done, EditOutcome::Unchanged);
    assert!(audit_of(&t, "req-same").await.is_empty());
    let both = MergeRequestPatch {
        title: Some("New title".to_owned()),
        draft: Some(true),
        target_branch: Some(branch("release")),
        ..MergeRequestPatch::default()
    };
    let (done, _, ()) = edit_mr_decided(
        &t.db,
        &as_user(alice, "req-both"),
        p,
        1,
        alice,
        &both,
        |_| Ok(()),
    )
    .await
    .expect("both");
    assert_eq!(
        done,
        EditOutcome::Changed(vec![
            EventType::MergeRequestUpdated,
            EventType::MergeRequestDraftChanged
        ])
    );
    let id = mr.id.get();
    let events = events_of(&t, "req-both").await;
    assert_eq!(
        events
            .iter()
            .map(|e| (e.0.as_str(), e.5.as_deref()))
            .collect::<Vec<_>>(),
        [
            (
                "merge_request.updated",
                Some(format!("merge_request_updated:{id}:2").as_str())
            ),
            (
                "merge_request.draft_changed",
                Some(format!("merge_request_draft_changed:{id}:2").as_str())
            ),
        ]
    );
    assert_eq!(
        events[0].4,
        json!({"merge_request_id": id, "iid": 1, "fields": ["title", "target_branch"],
               "target_branch": {"from": "refs/heads/main", "to": "refs/heads/release"}})
    );
    assert_eq!(audit_of(&t, "req-both").await.len(), 2);
    let stored = get(&t.db, p, 1).await.expect("get").expect("there");
    assert_eq!(
        (
            stored.title.as_str(),
            stored.draft,
            stored.target_branch.as_str(),
            stored.revision
        ),
        ("New title", true, "refs/heads/release", 2)
    );
    t.drop_now().await;
}

#[tokio::test]
async fn close_and_reopen_set_their_fields_and_repeat_as_unchanged() {
    let t = FreshDb::migrated().await;
    let (alice, web) = web(&t).await;
    let p = web.project_id;
    open(&t, alice, p, "req-open", &new_mr("feature", "main"))
        .await
        .expect("open");
    let (done, _, ()) =
        close_mr_decided(&t.db, &as_user(alice, "req-close"), p, 1, alice, |_| Ok(()))
            .await
            .expect("close");
    assert_eq!(done, StateOutcome::Changed);
    let closed = get(&t.db, p, 1).await.expect("get").expect("there");
    assert_eq!(closed.state, MergeRequestState::Closed);
    assert_eq!(closed.closed_by, Some(alice));
    assert!(closed.closed_at.is_some());
    let (again, _, ()) =
        close_mr_decided(&t.db, &as_user(alice, "req-close-2"), p, 1, alice, |_| {
            Ok(())
        })
        .await
        .expect("again");
    assert_eq!(again, StateOutcome::Unchanged);
    assert!(audit_of(&t, "req-close-2").await.is_empty());
    let (done, _, ()) = reopen_mr_decided(
        &t.db,
        &as_user(alice, "req-reopen"),
        p,
        1,
        alice,
        None,
        |_| Ok(()),
    )
    .await
    .expect("reopen");
    assert_eq!(done, StateOutcome::Changed);
    let open_again = get(&t.db, p, 1).await.expect("get").expect("there");
    assert_eq!(
        (open_again.state, open_again.closed_at, open_again.closed_by),
        (MergeRequestState::Open, None, None)
    );
    let (same, _, ()) = reopen_mr_decided(
        &t.db,
        &as_user(alice, "req-reopen-2"),
        p,
        1,
        alice,
        None,
        |_| Ok(()),
    )
    .await
    .expect("again");
    assert_eq!(same, StateOutcome::Unchanged);
    // An absent iid is FG-REPO-0005 after decide.
    let err = close_mr_decided(&t.db, &as_user(alice, "req-none"), p, 9, alice, |_| Ok(()))
        .await
        .expect_err("absent");
    assert_reason(&err, ErrorCode::NotFound, "absent");
    t.drop_now().await;
}

#[tokio::test]
async fn decide_sees_the_locked_facts_and_a_denial_writes_nothing() {
    let t = FreshDb::migrated().await;
    let (alice, web) = web(&t).await;
    let p = web.project_id;
    let seen = open_mr_decided(
        &t.db,
        &as_user(alice, "req-deny"),
        p,
        alice,
        &new_mr("feature", "main"),
        |facts| {
            assert_eq!(facts.project.project, p);
            assert_eq!(facts.project.access.role, Some(Role::Owner));
            assert!(facts.account.active);
            assert!(facts.merge_request.is_none());
            Err::<(), _>(ForgeError::new(ErrorCode::Forbidden))
        },
    )
    .await
    .expect_err("denied");
    assert_eq!(seen.code(), ErrorCode::Forbidden);
    assert!(audit_of(&t, "req-deny").await.is_empty());
    let mut su = t.superuser().await;
    let counters: i64 = sqlx::query_scalar("SELECT count(*) FROM forge.merge_request_counters")
        .fetch_one(&mut su)
        .await
        .expect("count");
    assert_eq!(counters, 0, "a denied open draws no number");
    open(&t, alice, p, "req-ok", &new_mr("feature", "main"))
        .await
        .expect("open");
    let (edited, _, ()) = edit_mr_decided(
        &t.db,
        &as_user(alice, "req-edit"),
        p,
        1,
        alice,
        &MergeRequestPatch::default(),
        |facts| {
            let mr = facts.merge_request.as_ref().expect("locked row");
            assert_eq!((mr.iid, mr.author), (1, alice));
            Ok(())
        },
    )
    .await
    .expect("edit");
    assert_eq!(
        edited,
        EditOutcome::Unchanged,
        "an empty patch writes nothing"
    );
    t.drop_now().await;
}

/// ADR-0024 §6: `merged` is final in the database too, and a merge
/// request's identity never changes (guard merge_requests_state_guard,
/// XF001).
#[tokio::test]
async fn the_state_guard_keeps_merged_final_and_the_identity_fixed() {
    let t = FreshDb::migrated().await;
    let (alice, web) = web(&t).await;
    let p = web.project_id;
    let mr = open(&t, alice, p, "req-open", &new_mr("feature", "main"))
        .await
        .expect("open");
    let id = mr.id.get();
    t.commit_audited(
        ("project", p.get()),
        &[format!(
            "UPDATE forge.merge_requests SET state = 'merged', revision = revision + 1              WHERE id = {id}"
        )],
    )
    .await;
    let refused = [
        format!("UPDATE forge.merge_requests SET state = 'open' WHERE id = {id}"),
        format!(
            "UPDATE forge.merge_requests SET state = 'closed', closed_at = now(), \
             closed_by = {} WHERE id = {id}",
            alice.get()
        ),
        format!("UPDATE forge.merge_requests SET iid = 50 WHERE id = {id}"),
        format!("UPDATE forge.merge_requests SET source_branch = 'refs/heads/x' WHERE id = {id}"),
    ];
    for sql in refused {
        let mut su = t.superuser().await;
        let err = sqlx::raw_sql(&sql)
            .execute(&mut su)
            .await
            .expect_err("guarded");
        assert_eq!(
            err.as_database_error().and_then(|d| d.code()).as_deref(),
            Some("XF001"),
            "{sql}"
        );
    }
    // closed never becomes merged directly.
    let other = open(&t, alice, p, "req-open-2", &new_mr("other", "main"))
        .await
        .expect("open");
    let (closed, _, ()) = close_mr_decided(
        &t.db,
        &as_user(alice, "req-close"),
        p,
        other.iid,
        alice,
        |_| Ok(()),
    )
    .await
    .expect("close");
    assert_eq!(closed, StateOutcome::Changed);
    let mut su = t.superuser().await;
    let err = sqlx::raw_sql(&format!(
        "UPDATE forge.merge_requests SET state = 'merged', closed_at = NULL, closed_by = NULL \
         WHERE id = {}",
        other.id.get()
    ))
    .execute(&mut su)
    .await
    .expect_err("reopen first");
    assert_eq!(
        err.as_database_error().and_then(|d| d.code()).as_deref(),
        Some("XF001")
    );
    t.drop_now().await;
}

/// The audit trigger: a merge request change without its audit row cannot
/// commit (XF002), like every audited table.
#[tokio::test]
async fn a_merge_request_write_without_an_audit_row_cannot_commit() {
    let t = FreshDb::migrated().await;
    let (alice, web) = web(&t).await;
    let mut app = t.db.write_pool().acquire().await.expect("conn");
    let err = sqlx::query(
        "INSERT INTO forge.merge_requests (project_id, iid, title, author_id, source_branch, \
         target_branch) VALUES ($1, 1, 't', $2, 'refs/heads/a', 'refs/heads/b')",
    )
    .bind(web.project_id.get())
    .bind(alice.get())
    .execute(&mut *app)
    .await
    .expect_err("no audit row");
    assert_eq!(
        err.as_database_error().and_then(|d| d.code()).as_deref(),
        Some("XF002")
    );
    t.drop_now().await;
}
