//! Merge request discussions against a migrated database (M3 R3): start,
//! reply, resolve / unresolve, the events and audit rows each writes, the
//! position backstop and the guards. `decide` is a plain closure here
//! (forge-review passes `Authorized::reconfirm`, tested there).

use forge_core::{DiscussionId, DiscussionKind, DiscussionSide, ObjectId, RequestId};
use forge_errors::ErrorCode;
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

/// alice, her project `alice/web` and an open merge request feature → main.
async fn seeded(t: &FreshDb) -> (UserId, CreatedProject, StoredMergeRequest) {
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
        title: "Merge feature".to_owned(),
        description: String::new(),
        source_branch: branch("feature"),
        target_branch: branch("main"),
        draft: false,
    };
    let (mr, ()) = open_mr_decided(
        &t.db,
        &as_user(alice.user_id, "req-open"),
        web.project_id,
        alice.user_id,
        &new,
        |_| Ok(()),
    )
    .await
    .expect("open");
    (alice.user_id, web, mr)
}

fn general(body: &str) -> NewDiscussion {
    NewDiscussion {
        body: body.to_owned(),
        position: None,
    }
}

fn inline(version: i64, line: i64) -> NewDiscussion {
    NewDiscussion {
        body: "on a line".to_owned(),
        position: Some(DiscussionPosition {
            version,
            path: "src/lib.rs".to_owned(),
            side: DiscussionSide::New,
            line,
        }),
    }
}

/// (event_type, payload, dedup_key) of a request's events.
async fn events_of(t: &FreshDb, request: &str) -> Vec<(String, serde_json::Value, String)> {
    sqlx::query_as(
        "SELECT event_type, payload, dedup_key FROM forge.events WHERE request_id = $1 \
         ORDER BY id",
    )
    .bind(request)
    .fetch_all(&mut t.superuser().await)
    .await
    .expect("events")
}

/// (action, resource_type, resource_id) of a request's audit rows.
async fn audit_of(t: &FreshDb, request: &str) -> Vec<(String, String, Option<i64>)> {
    sqlx::query_as(
        "SELECT action, resource_type, resource_id FROM forge.audit_log WHERE request_id = $1 \
         ORDER BY id",
    )
    .bind(request)
    .fetch_all(&mut t.superuser().await)
    .await
    .expect("audit")
}

async fn start(
    t: &FreshDb,
    who: UserId,
    p: ProjectId,
    iid: i64,
    request: &str,
    new: &NewDiscussion,
) -> Result<StoredDiscussion, ForgeError> {
    start_discussion_mr_decided(&t.db, &as_user(who, request), p, iid, who, new, |_| Ok(()))
        .await
        .map(|(d, ())| d)
}

#[tokio::test]
async fn starting_a_discussion_stores_its_first_note_and_publishes_commented_once() {
    let t = FreshDb::migrated().await;
    let (alice, web, mr) = seeded(&t).await;
    let d = start(
        &t,
        alice,
        web.project_id,
        mr.iid,
        "req-start",
        &general("Looks **good**"),
    )
    .await
    .expect("start");
    assert_eq!(d.kind, DiscussionKind::General);
    assert_eq!(d.position, None);
    assert_eq!((d.started_by, d.resolved, d.revision), (alice, false, 1));
    assert_eq!(d.notes.len(), 1);
    // Stored as written: Markdown is never rendered or rewritten.
    assert_eq!(d.notes[0].body, "Looks **good**");
    assert_eq!(d.notes[0].author, alice);
    let events = events_of(&t, "req-start").await;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].0, "merge_request.commented");
    assert_eq!(
        events[0].1,
        json!({
            "merge_request_id": mr.id.get(),
            "iid": mr.iid,
            "action": "note_added",
            "discussion_id": d.id.get(),
            "kind": "general",
            "note_id": d.notes[0].id.get(),
            "discussion_started": true,
        })
    );
    assert_eq!(
        events[0].2,
        format!("merge_request_note_added:{}", d.notes[0].id.get())
    );
    assert_eq!(
        audit_of(&t, "req-start").await,
        [(
            "merge_request.commented".to_owned(),
            "project".to_owned(),
            Some(web.project_id.get())
        )]
    );
    t.drop_now().await;
}

#[tokio::test]
async fn a_reply_adds_a_note_and_one_event_and_keeps_the_thread_state() {
    let t = FreshDb::migrated().await;
    let (alice, web, mr) = seeded(&t).await;
    let p = web.project_id;
    let d = start(&t, alice, p, mr.iid, "req-start", &general("first"))
        .await
        .expect("start");
    let (replied, ()) = add_note_mr_decided(
        &t.db,
        &as_user(alice, "req-reply"),
        p,
        mr.iid,
        d.id,
        alice,
        "second",
        |_| Ok(()),
    )
    .await
    .expect("reply");
    let bodies: Vec<&str> = replied.notes.iter().map(|n| n.body.as_str()).collect();
    assert_eq!(bodies, ["first", "second"]);
    assert_eq!((replied.resolved, replied.revision), (false, 1));
    let events = events_of(&t, "req-reply").await;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].1["discussion_started"], json!(false));
    assert_eq!(events[0].1["note_id"], json!(replied.notes[1].id.get()));
    // A discussion of another merge request (or none) is not found, after
    // the decision.
    let missing = add_note_mr_decided(
        &t.db,
        &as_user(alice, "req-reply-missing"),
        p,
        mr.iid,
        DiscussionId::new(d.id.get() + 1000),
        alice,
        "lost",
        |_| Ok(()),
    )
    .await
    .expect_err("absent");
    assert_eq!(missing.code(), ErrorCode::NotFound);
    assert!(events_of(&t, "req-reply-missing").await.is_empty());
    t.drop_now().await;
}

#[tokio::test]
async fn resolving_toggles_once_per_change_with_distinct_dedup_keys() {
    let t = FreshDb::migrated().await;
    let (alice, web, mr) = seeded(&t).await;
    let p = web.project_id;
    let d = start(&t, alice, p, mr.iid, "req-start", &general("fix this"))
        .await
        .expect("start");
    let resolve = |request: &'static str, state: bool| {
        let db = t.db.clone();
        async move {
            resolve_discussion_mr_decided(
                &db,
                &as_user(alice, request),
                p,
                mr.iid,
                d.id,
                alice,
                state,
                |facts: &DiscussionFacts| {
                    // The decision sees who started the thread.
                    let started = facts.discussion.as_ref().map(|d| d.started_by);
                    assert_eq!(started, Some(alice));
                    Ok(())
                },
            )
            .await
            .map(|(outcome, d, ())| (outcome, d))
        }
    };
    let (outcome, done) = resolve("req-resolve", true).await.expect("resolve");
    assert_eq!(outcome, ResolveOutcome::Changed);
    assert_eq!(
        (done.resolved, done.resolved_by, done.revision),
        (true, Some(alice), 2)
    );
    assert!(done.resolved_at.is_some());
    let (again, _) = resolve("req-resolve-again", true).await.expect("again");
    assert_eq!(again, ResolveOutcome::Unchanged);
    assert!(events_of(&t, "req-resolve-again").await.is_empty());
    let (outcome, open) = resolve("req-unresolve", false).await.expect("unresolve");
    assert_eq!(outcome, ResolveOutcome::Changed);
    assert_eq!(
        (open.resolved, open.resolved_by, open.revision),
        (false, None, 3)
    );
    let (_, _) = resolve("req-resolve-2", true).await.expect("resolve again");
    let mut keys = Vec::new();
    for request in ["req-resolve", "req-unresolve", "req-resolve-2"] {
        let events = events_of(&t, request).await;
        assert_eq!(events.len(), 1, "{request}");
        keys.push(events[0].2.clone());
    }
    let id = d.id.get();
    assert_eq!(
        keys,
        [
            format!("merge_request_thread_resolved:{id}:2"),
            format!("merge_request_thread_unresolved:{id}:3"),
            format!("merge_request_thread_resolved:{id}:4"),
        ]
    );
    t.drop_now().await;
}

#[tokio::test]
async fn a_refused_decision_writes_nothing() {
    let t = FreshDb::migrated().await;
    let (alice, web, mr) = seeded(&t).await;
    let refused = start_discussion_mr_decided(
        &t.db,
        &as_user(alice, "req-refused"),
        web.project_id,
        mr.iid,
        alice,
        &general("nope"),
        |_| -> Result<(), ForgeError> { Err(ForgeError::new(ErrorCode::Forbidden)) },
    )
    .await
    .expect_err("refused");
    assert_eq!(refused.code(), ErrorCode::Forbidden);
    let page = discussions(&t.db, mr.id).await.expect("list");
    assert!(page.discussions.is_empty());
    assert!(events_of(&t, "req-refused").await.is_empty());
    t.drop_now().await;
}

#[tokio::test]
async fn an_inline_discussion_needs_a_version_of_its_merge_request() {
    let t = FreshDb::migrated().await;
    let (alice, web, mr) = seeded(&t).await;
    let p = web.project_id;
    // No version yet: the foreign key refuses it as FG-REVIEW-0004.
    let err = start(&t, alice, p, mr.iid, "req-no-version", &inline(1, 3))
        .await
        .expect_err("no version");
    assert_eq!(err.code(), ErrorCode::DiscussionPositionInvalid);
    let head = ObjectId::parse(&"a".repeat(40)).expect("oid");
    record_mr_version(&t.db, &system("req-v1"), mr.id, &head, None)
        .await
        .expect("version");
    let d = start(&t, alice, p, mr.iid, "req-inline", &inline(1, 3))
        .await
        .expect("inline");
    assert_eq!(d.kind, DiscussionKind::Inline);
    assert_eq!(
        d.position,
        Some(DiscussionPosition {
            version: 1,
            path: "src/lib.rs".to_owned(),
            side: DiscussionSide::New,
            line: 3,
        })
    );
    assert_eq!(
        events_of(&t, "req-inline").await[0].1["kind"],
        json!("inline")
    );
    t.drop_now().await;
}

#[tokio::test]
async fn note_bodies_are_capped_by_the_table() {
    let t = FreshDb::migrated().await;
    let (alice, web, mr) = seeded(&t).await;
    for (request, body) in [
        ("req-empty", String::new()),
        ("req-long", "x".repeat(65_537)),
    ] {
        let err = start(&t, alice, web.project_id, mr.iid, request, &general(&body))
            .await
            .expect_err("refused");
        assert_eq!(err.code(), ErrorCode::RequestInvalid, "{request}");
    }
    t.drop_now().await;
}

#[tokio::test]
async fn the_guards_keep_positions_and_notes_fixed() {
    let t = FreshDb::migrated().await;
    let (alice, web, mr) = seeded(&t).await;
    let d = start(
        &t,
        alice,
        web.project_id,
        mr.iid,
        "req-start",
        &general("body"),
    )
    .await
    .expect("start");
    let mut su = t.superuser().await;
    let moved = sqlx::query(
        "UPDATE forge.merge_request_discussions SET started_by = started_by + 1, \
         revision = revision + 1 WHERE id = $1",
    )
    .bind(d.id.get())
    .execute(&mut su)
    .await
    .expect_err("identity");
    assert!(moved.to_string().contains("never change"), "{moved}");
    let skipped = sqlx::query(
        "UPDATE forge.merge_request_discussions SET resolved = true, resolved_at = now(), \
         resolved_by = started_by WHERE id = $1",
    )
    .bind(d.id.get())
    .execute(&mut su)
    .await
    .expect_err("revision");
    assert!(skipped.to_string().contains("revision"), "{skipped}");
    let edited = sqlx::query("UPDATE forge.merge_request_notes SET body = 'changed' WHERE id = $1")
        .bind(d.notes[0].id.get())
        .execute(&mut su)
        .await
        .expect_err("note");
    assert!(edited.to_string().contains("never changed"), "{edited}");
    t.drop_now().await;
}

#[tokio::test]
async fn the_list_returns_discussions_with_their_notes_oldest_first() {
    let t = FreshDb::migrated().await;
    let (alice, web, mr) = seeded(&t).await;
    let p = web.project_id;
    let a = start(&t, alice, p, mr.iid, "req-a", &general("a1"))
        .await
        .expect("a");
    let b = start(&t, alice, p, mr.iid, "req-b", &general("b1"))
        .await
        .expect("b");
    add_note_mr_decided(
        &t.db,
        &as_user(alice, "req-a2"),
        p,
        mr.iid,
        a.id,
        alice,
        "a2",
        |_| Ok(()),
    )
    .await
    .expect("a2");
    let page = discussions(&t.db, mr.id).await.expect("list");
    assert!(!page.truncated);
    let shape: Vec<(i64, Vec<&str>)> = page
        .discussions
        .iter()
        .map(|d| {
            (
                d.id.get(),
                d.notes.iter().map(|n| n.body.as_str()).collect(),
            )
        })
        .collect();
    assert_eq!(
        shape,
        [(a.id.get(), vec!["a1", "a2"]), (b.id.get(), vec!["b1"])]
    );
    let one = discussion(&t.db, mr.id, b.id).await.expect("one");
    assert_eq!(one.map(|d| d.started_by), Some(alice));
    t.drop_now().await;
}
