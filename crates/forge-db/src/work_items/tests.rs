//! Work items and links against a migrated database (M3 I1b): the
//! `work-item-sync` subscriber end to end (merge request events and pushes
//! through the sequencer), a person's links, the stop on a failure, the
//! schema's backstops by constraint name, and the purge. Synthetic data only.

use forge_core::ids::{IntegrationId, ProjectId, UserId};
use forge_core::{
    EventType, IntegrationKind, ObjectId, PushRecord, PushRecorder, PushUpdateKind, PushedRef,
    Pusher, RefName, RequestId, UserKind, WorkItemRef,
};
use forge_errors::ErrorCode;
use sqlx::Connection as _;

use super::*;
use crate::change::{
    Actor, AuditEntry, ChangeContext, ChangeRecord, ChangeTx, DomainEvent, Outcome,
};
use crate::integrations::{self, NewIntegration};
use crate::projects::{self, CreatedProject, NewProject};
use crate::review::{
    self, link_work_item_mr_decided, unlink_work_item_mr_decided, LinkOutcome, MergeRequestPatch,
    NewMergeRequest, StoredMergeRequest,
};
use crate::service_accounts::{self, NewServiceAccount};
use crate::subscribers::{self, Progress, Start};
use crate::test_support::*;
use crate::users::{self, NewUser};

fn ctx(actor: Actor, request: &str) -> ChangeContext {
    ChangeContext::new(actor, RequestId::parse(request).expect("request id"))
}

fn system(request: &str) -> ChangeContext {
    ctx(Actor::System, request)
}

struct World {
    t: FreshDb,
    alice: UserId,
    namespace: forge_core::NamespaceId,
    web: CreatedProject,
    integration: IntegrationId,
}

/// alice, her project `web` bound to kanflow project `kf-1`, and the
/// subscriber registered at the head.
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
    let sa = service_accounts::create(
        &t.db,
        &system("req-sa"),
        &NewServiceAccount {
            username: "kanflow-sync".to_owned(),
            display_name: "kanflow".to_owned(),
            owner: alice.user_id,
            purpose: "Kanflow reads the event feed".to_owned(),
        },
        alice.user_id,
    )
    .await
    .expect("service account")
    .user_id;
    let integration = integrations::create(
        &t.db,
        &system("req-int"),
        &NewIntegration {
            kind: IntegrationKind::Kanflow,
            service_account: sa,
            event_types: None,
        },
        alice.user_id,
    )
    .await
    .expect("integration");
    let world = World {
        t,
        alice: alice.user_id,
        namespace: alice.namespace_id,
        web,
        integration,
    };
    bind(&world, world.web.project_id).await;
    subscribers::register(&world.t.db, &WorkItemSync, Start::Head)
        .await
        .expect("register");
    world
}

async fn bind(w: &World, project: ProjectId) {
    let (_bound, _, ()) = integrations::bind_decided(
        &w.t.db,
        &system(&format!("req-bind-{}", project.get())),
        project,
        w.alice,
        w.integration,
        "kf-1",
        |_| Ok(()),
    )
    .await
    .expect("bind");
}

async fn open(w: &World, title: &str, source: &str) -> StoredMergeRequest {
    review::open_mr_decided(
        &w.t.db,
        &ctx(
            Actor::User(w.alice),
            &format!("req-open-{}", source.replace('/', "-")),
        ),
        w.web.project_id,
        w.alice,
        &NewMergeRequest {
            title: title.to_owned(),
            description: String::new(),
            source_branch: RefName::parse(&format!("refs/heads/{source}")).expect("branch"),
            target_branch: RefName::parse("refs/heads/main").expect("branch"),
            draft: false,
        },
        |_| Ok(()),
    )
    .await
    .map(|(stored, ())| stored)
    .expect("open")
}

async fn retitle(w: &World, mr: &StoredMergeRequest, title: &str, request: &str) {
    let (_, _, ()) = review::edit_mr_decided(
        &w.t.db,
        &ctx(Actor::User(w.alice), request),
        w.web.project_id,
        mr.iid,
        w.alice,
        &MergeRequestPatch {
            title: Some(title.to_owned()),
            description: None,
            draft: None,
            target_branch: None,
        },
        |_| Ok(()),
    )
    .await
    .expect("edit");
}

fn reference(raw: &str) -> WorkItemRef {
    WorkItemRef::parse(raw).expect("ref")
}

async fn link(w: &World, mr: &StoredMergeRequest, raw: &str, request: &str) -> LinkOutcome {
    link_work_item_mr_decided(
        &w.t.db,
        &ctx(Actor::User(w.alice), request),
        (w.web.project_id, mr.iid),
        &reference(raw),
        w.alice,
        |_| Ok(()),
    )
    .await
    .map(|(outcome, _, ())| outcome)
    .expect("link")
}

/// Number the events, then run the subscriber until it caught up.
async fn drain(t: &FreshDb) {
    crate::sequencer::run_once(
        &t.db,
        crate::sequencer::BatchSize::MAX,
        crate::sequencer::AuditSequencing::Skip,
    )
    .await
    .expect("sequencer");
    loop {
        match subscribers::run_once(&t.db, &WorkItemSync)
            .await
            .expect("step")
        {
            Progress::Handled { .. } => {}
            Progress::CaughtUp => break,
            other @ (Progress::Busy
            | Progress::Disabled
            | Progress::Failed(_)
            | Progress::Stuck(_)
            | Progress::Quarantined(_)) => panic!("unexpected {other:?}"),
        }
        // Derived events wait for the sequencer too.
        crate::sequencer::run_once(
            &t.db,
            crate::sequencer::BatchSize::MAX,
            crate::sequencer::AuditSequencing::Skip,
        )
        .await
        .expect("sequencer");
    }
}

/// (ref, source, open, unlink_source) of the merge request's link rows.
async fn link_rows(
    t: &FreshDb,
    mr: &StoredMergeRequest,
) -> Vec<(String, String, bool, Option<String>)> {
    sqlx::query_as(
        "SELECT w.ref, l.source, l.unlinked_at IS NULL, l.unlink_source \
         FROM forge.merge_request_work_items AS l JOIN forge.work_items AS w \
         ON w.id = l.work_item_id WHERE l.merge_request_id = $1 ORDER BY l.id",
    )
    .bind(mr.id.get())
    .fetch_all(&mut t.superuser().await)
    .await
    .expect("links")
}

/// (event_type, actor_kind, payload) of the work_item.* events, in order.
async fn link_events(t: &FreshDb) -> Vec<(String, String, serde_json::Value)> {
    sqlx::query_as(
        "SELECT event_type, actor_kind, payload FROM forge.events \
         WHERE event_type LIKE 'work_item.%' ORDER BY id",
    )
    .fetch_all(&mut t.superuser().await)
    .await
    .expect("events")
}

/// The refs recorded for the newest event of `event_type`.
async fn refs_of_last(t: &FreshDb, event_type: &str) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT w.ref FROM forge.event_work_items AS e JOIN forge.work_items AS w \
         ON w.id = e.work_item_id WHERE e.seq = (SELECT max(seq) FROM forge.events \
         WHERE event_type = $1) ORDER BY w.ref",
    )
    .bind(event_type)
    .fetch_all(&mut t.superuser().await)
    .await
    .expect("event work items")
}

#[tokio::test]
async fn title_edit_adding_ref_emits_work_item_linked() {
    let w = world().await;
    let mr = open(&w, "Fix rounding", "fix-rounding").await;
    drain(&w.t).await;
    assert!(link_rows(&w.t, &mr).await.is_empty());
    assert!(refs_of_last(&w.t, "merge_request.opened").await.is_empty());

    retitle(&w, &mr, "Fix rounding (PBI #43)", "req-edit-1").await;
    drain(&w.t).await;
    assert_eq!(
        link_rows(&w.t, &mr).await,
        [("PBI-43".to_owned(), "auto".to_owned(), true, None)]
    );
    let events = link_events(&w.t).await;
    assert_eq!(events.len(), 1, "{events:?}");
    let (kind, actor, payload) = &events[0];
    assert_eq!(kind, EventType::WorkItemLinked.as_str());
    assert_eq!(actor, "system", "a derived event");
    assert_eq!(payload["merge_request_id"], mr.id.get());
    assert_eq!(payload["source"], "auto");
    assert_eq!(
        payload["work_item"],
        serde_json::json!({"provider": "kanflow", "project_id": "kf-1", "ref": "PBI-43"})
    );
    // The edit and the link event both concern the work item.
    assert_eq!(
        refs_of_last(&w.t, "merge_request.updated").await,
        ["PBI-43"]
    );
    assert_eq!(refs_of_last(&w.t, "work_item.linked").await, ["PBI-43"]);
    // The derived event came from the edit, in its request.
    let lineage: (Option<String>, String) = sqlx::query_as(
        "SELECT (SELECT c.event_type FROM forge.events AS c WHERE c.seq = e.caused_by_seq), \
         e.request_id FROM forge.events AS e WHERE e.event_type = 'work_item.linked'",
    )
    .fetch_one(&mut w.t.superuser().await)
    .await
    .expect("lineage");
    assert_eq!(
        lineage,
        (
            Some("merge_request.updated".to_owned()),
            "req-edit-1".to_owned()
        )
    );
    // Handling the edit again (a retried step) links nothing twice.
    retitle(&w, &mr, "Fix rounding (PBI #43) now", "req-edit-2").await;
    drain(&w.t).await;
    assert_eq!(link_rows(&w.t, &mr).await.len(), 1);
    assert_eq!(link_events(&w.t).await.len(), 1);
    w.t.drop_now().await;
}

#[tokio::test]
async fn unlink_keeps_history() {
    let w = world().await;
    let mr = open(&w, "PBI-1 and PBI-2", "two").await;
    drain(&w.t).await;
    retitle(&w, &mr, "PBI-2 only", "req-edit-drop").await;
    drain(&w.t).await;
    // PBI-1's row stays, ended by detection; PBI-2 is still open.
    assert_eq!(
        link_rows(&w.t, &mr).await,
        [
            (
                "PBI-1".to_owned(),
                "auto".to_owned(),
                false,
                Some("auto".to_owned())
            ),
            ("PBI-2".to_owned(), "auto".to_owned(), true, None),
        ]
    );
    let kinds: Vec<String> = link_events(&w.t).await.into_iter().map(|e| e.0).collect();
    assert_eq!(
        kinds,
        ["work_item.linked", "work_item.linked", "work_item.unlinked"]
    );
    assert_eq!(refs_of_last(&w.t, "work_item.unlinked").await, ["PBI-1"]);
    // The opening event still names both: history is never rewritten.
    assert_eq!(
        refs_of_last(&w.t, "merge_request.opened").await,
        ["PBI-1", "PBI-2"]
    );
    // Named again: a new interval, the old row untouched.
    retitle(&w, &mr, "PBI-2 and PBI-1 again", "req-edit-back").await;
    drain(&w.t).await;
    let rows = link_rows(&w.t, &mr).await;
    assert_eq!(rows.len(), 3, "{rows:?}");
    assert_eq!(rows[2], ("PBI-1".to_owned(), "auto".to_owned(), true, None));
    // The API's read: every interval, oldest first.
    let listed = links(&w.t.db, mr.id).await.expect("links");
    assert_eq!(listed.links.len(), 3);
    assert!(!listed.truncated);
    assert!(listed.links[0].unlinked_at.is_some());
    assert_eq!(
        listed.links[0].unlink_source,
        Some(forge_core::LinkSource::Auto)
    );
    // forge_app may not delete a link of a live project.
    let err = sqlx::query("DELETE FROM forge.merge_request_work_items")
        .execute(w.t.db.write_pool())
        .await
        .expect_err("guarded");
    assert!(
        err.to_string().contains("deleted only by the purge"),
        "{err}"
    );
    w.t.drop_now().await;
}

fn deleted_branch(name: &str) -> PushedRef {
    PushedRef {
        name: RefName::parse(name).expect("ref"),
        old: ObjectId::parse(&"a".repeat(40)).expect("oid"),
        new: ObjectId::parse(&"0".repeat(40)).expect("oid"),
        kind: PushUpdateKind::Delete,
        commits: Vec::new(),
        total_commits: 0,
        total_commits_partial: false,
    }
}

fn created_branch(name: &str) -> PushedRef {
    PushedRef {
        name: RefName::parse(name).expect("ref"),
        old: ObjectId::parse(&"0".repeat(40)).expect("oid"),
        new: ObjectId::parse(&"b".repeat(40)).expect("oid"),
        kind: PushUpdateKind::Create,
        commits: Vec::new(),
        total_commits: 0,
        total_commits_partial: false,
    }
}

async fn push(w: &World, project: &CreatedProject, request: &str, updates: Vec<PushedRef>) {
    let who = Pusher {
        request_id: RequestId::parse(request).expect("request id"),
        user: w.alice,
        kind: UserKind::Human,
        credential_id: None,
        source_ip: None,
        user_agent: None,
    };
    let recorder: &dyn PushRecorder = &w.t.db;
    let done = recorder
        .record_push(
            &who,
            &PushRecord {
                repository_id: project.repository_id,
                updates,
            },
        )
        .await
        .expect("push");
    assert_eq!(done, forge_core::PushRecorded::Recorded);
}

#[tokio::test]
async fn link_survives_source_branch_deletion() {
    let w = world().await;
    let mr = open(&w, "Login form", "feature/PBI-7-login-form").await;
    drain(&w.t).await;
    assert_eq!(
        link_rows(&w.t, &mr).await,
        [("PBI-7".to_owned(), "auto".to_owned(), true, None)]
    );
    // The branch goes (02 §8.2: "the merge that deletes the branch"), then
    // the merge request is closed.
    push(
        &w,
        &w.web,
        "req-push-delete",
        vec![deleted_branch("refs/heads/feature/PBI-7-login-form")],
    )
    .await;
    let _closed = review::close_mr_decided(
        &w.t.db,
        &ctx(Actor::User(w.alice), "req-close"),
        w.web.project_id,
        mr.iid,
        w.alice,
        |_| Ok(()),
    )
    .await
    .expect("close");
    drain(&w.t).await;
    assert_eq!(
        link_rows(&w.t, &mr).await,
        [("PBI-7".to_owned(), "auto".to_owned(), true, None)],
        "the link stays"
    );
    assert_eq!(refs_of_last(&w.t, "repository.pushed").await, ["PBI-7"]);
    assert_eq!(refs_of_last(&w.t, "merge_request.closed").await, ["PBI-7"]);
    let kinds: Vec<String> = link_events(&w.t).await.into_iter().map(|e| e.0).collect();
    assert_eq!(kinds, ["work_item.linked"], "never unlinked");
    w.t.drop_now().await;
}

#[tokio::test]
async fn branch_name_in_push_links_work_item() {
    let w = world().await;
    push(
        &w,
        &w.web,
        "req-push-branch",
        vec![
            created_branch("refs/heads/fix/L2-pbi-43-e1-until-precision"),
            created_branch("refs/heads/feature/PBI-430"),
            created_branch("refs/tags/PBI-9"),
        ],
    )
    .await;
    drain(&w.t).await;
    // Branches only (a tag names no work item); PBI-430 is not PBI-43.
    assert_eq!(
        refs_of_last(&w.t, "repository.pushed").await,
        ["PBI-43", "PBI-430"]
    );
    let items: Vec<(i64, String, String)> = sqlx::query_as(
        "SELECT integration_id, external_project_id, ref FROM forge.work_items ORDER BY ref",
    )
    .fetch_all(&mut w.t.superuser().await)
    .await
    .expect("items");
    assert_eq!(
        items,
        [
            (w.integration.get(), "kf-1".to_owned(), "PBI-43".to_owned()),
            (w.integration.get(), "kf-1".to_owned(), "PBI-430".to_owned()),
        ]
    );
    // An unbound project's push names no work item (PBI numbers are per
    // kanflow project, 02 §2).
    let api = projects::create(
        &w.t.db,
        &system("req-project-api"),
        &NewProject::new("api", w.namespace, "API", w.alice),
    )
    .await
    .expect("api");
    push(
        &w,
        &api,
        "req-push-unbound",
        vec![created_branch("refs/heads/PBI-5")],
    )
    .await;
    drain(&w.t).await;
    assert!(refs_of_last(&w.t, "repository.pushed").await.is_empty());
    w.t.drop_now().await;
}

/// A published `merge_request.opened` whose payload lacks its id (a writer
/// bug): the handler fails on it.
async fn publish_broken_event(w: &World) {
    let tx = ChangeTx::begin(&w.t.db, &system("req-broken"))
        .await
        .expect("begin");
    let record = ChangeRecord {
        audit: AuditEntry {
            action: "merge_request.opened",
            outcome: Outcome::Succeeded,
            resource_type: "project",
            resource_id: Some(w.web.project_id.get()),
            project_id: Some(w.web.project_id),
            error_code: None,
            details: serde_json::json!({}),
        },
        affected: Vec::new(),
        events: vec![DomainEvent {
            event_type: EventType::MergeRequestOpened,
            schema_version: 1,
            aggregate_type: "merge_request",
            aggregate_id: 999,
            project_id: Some(w.web.project_id),
            payload: serde_json::json!({ "iid": 1 }),
            dedup_key: None,
        }],
    };
    tx.commit(record).await.expect("committed");
    crate::sequencer::run_once(
        &w.t.db,
        crate::sequencer::BatchSize::MAX,
        crate::sequencer::AuditSequencing::Skip,
    )
    .await
    .expect("sequencer");
}

#[tokio::test]
async fn work_item_sync_failure_stops_and_alerts() {
    let w = world().await;
    drain(&w.t).await;
    publish_broken_event(&w).await;
    let before: i64 = sqlx::query_scalar(
        "SELECT last_seq FROM forge.event_subscribers WHERE name = 'work-item-sync'",
    )
    .fetch_one(&mut w.t.superuser().await)
    .await
    .expect("cursor");
    for attempt in 1..=4 {
        let Progress::Failed(incident) = subscribers::run_once(&w.t.db, &WorkItemSync)
            .await
            .expect("step")
        else {
            panic!("attempt {attempt} is a counted failure");
        };
        assert_reason(
            &incident.error,
            ErrorCode::Internal,
            "invalid_event_payload",
        );
        assert_eq!(
            incident.request_id.as_ref().map(|r| r.as_str()),
            Some("req-broken")
        );
    }
    let Progress::Stuck(incident) = subscribers::run_once(&w.t.db, &WorkItemSync)
        .await
        .expect("step")
    else {
        panic!("the fifth failure stops it");
    };
    assert_reason(&incident.error, ErrorCode::SubscriberLag, "stuck");
    // It stays on the event: never skipped, and doctor reports it.
    let (cursor, failures, code): (i64, i32, Option<String>) = sqlx::query_as(
        "SELECT last_seq, failures, last_error_code FROM forge.event_subscribers \
         WHERE name = 'work-item-sync'",
    )
    .fetch_one(&mut w.t.superuser().await)
    .await
    .expect("state");
    assert_eq!((cursor, failures), (before, 5));
    assert_eq!(code.as_deref(), Some("FG-SYS-0001"));
    let states = subscribers::states(&w.t.db)
        .await
        .expect("states")
        .expect("migrated");
    let lag = subscribers::check_lag(&states).expect_err("doctor alerts");
    assert_eq!(lag.code(), ErrorCode::SubscriberLag);
    // Still stuck on the next try (it never moves on by itself).
    assert!(matches!(
        subscribers::run_once(&w.t.db, &WorkItemSync)
            .await
            .expect("step"),
        Progress::Stuck(_)
    ));
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_persons_link_and_unlink_are_audited_and_win_over_detection() {
    let w = world().await;
    let mr = open(&w, "Fix (PBI-43)", "fix").await;
    drain(&w.t).await;
    // A person links PBI-9; again is unchanged; PBI-43 is linked already.
    assert_eq!(
        link(&w, &mr, "PBI-9", "req-link-9").await,
        LinkOutcome::Changed
    );
    assert_eq!(
        link(&w, &mr, "PBI-9", "req-link-9b").await,
        LinkOutcome::Unchanged
    );
    assert_eq!(
        link(&w, &mr, "PBI-43", "req-link-43").await,
        LinkOutcome::Unchanged
    );
    let audit: Vec<(String, String, Option<i64>)> = sqlx::query_as(
        "SELECT action, actor_kind, actor_user_id FROM forge.audit_log \
         WHERE action LIKE 'work_item.%' ORDER BY id",
    )
    .fetch_all(&mut w.t.superuser().await)
    .await
    .expect("audit");
    assert_eq!(
        audit,
        [(
            "work_item.linked".to_owned(),
            "user".to_owned(),
            Some(w.alice.get())
        )]
    );
    // A person unlinks the detected PBI-43: detection does not bring it back.
    let (answer, ()) = unlink_work_item_mr_decided(
        &w.t.db,
        &ctx(Actor::User(w.alice), "req-unlink-43"),
        (w.web.project_id, mr.iid),
        &reference("PBI-43"),
        w.alice,
        |_| Ok(()),
    )
    .await
    .expect("unlink");
    assert_eq!(answer.source, forge_core::LinkSource::Auto);
    retitle(&w, &mr, "Fix (PBI-43) again", "req-edit").await;
    drain(&w.t).await;
    assert_eq!(
        link_rows(&w.t, &mr).await,
        [
            (
                "PBI-43".to_owned(),
                "auto".to_owned(),
                false,
                Some("manual".to_owned())
            ),
            ("PBI-9".to_owned(), "manual".to_owned(), true, None),
        ]
    );
    // Detection never ends a person's link.
    retitle(&w, &mr, "Fix", "req-edit-none").await;
    drain(&w.t).await;
    assert!(link_rows(&w.t, &mr).await[1].2, "PBI-9 stays");
    // Until a person links it again.
    assert_eq!(
        link(&w, &mr, "PBI-43", "req-relink-43").await,
        LinkOutcome::Changed
    );
    let kinds: Vec<(String, String)> = link_events(&w.t)
        .await
        .into_iter()
        .map(|(k, a, _)| (k, a))
        .collect();
    assert_eq!(
        kinds,
        [
            ("work_item.linked".to_owned(), "system".to_owned()),
            ("work_item.linked".to_owned(), "user".to_owned()),
            ("work_item.unlinked".to_owned(), "user".to_owned()),
            ("work_item.linked".to_owned(), "user".to_owned()),
        ]
    );
    // Unlinking what is not linked is not found.
    let err = unlink_work_item_mr_decided(
        &w.t.db,
        &ctx(Actor::User(w.alice), "req-unlink-none"),
        (w.web.project_id, mr.iid),
        &reference("PBI-1"),
        w.alice,
        |_| Ok(()),
    )
    .await
    .expect_err("not linked");
    assert_reason(&err, ErrorCode::NotFound, "not_linked");
    w.t.drop_now().await;
}

#[tokio::test]
async fn manual_link_on_an_unbound_project_is_int_0018() {
    let w = world().await;
    let api = projects::create(
        &w.t.db,
        &system("req-project-api"),
        &NewProject::new("api", w.namespace, "API", w.alice),
    )
    .await
    .expect("api");
    let mr = review::open_mr_decided(
        &w.t.db,
        &ctx(Actor::User(w.alice), "req-open-api"),
        api.project_id,
        w.alice,
        &NewMergeRequest {
            title: "PBI-3".to_owned(),
            description: String::new(),
            source_branch: RefName::parse("refs/heads/x").expect("branch"),
            target_branch: RefName::parse("refs/heads/main").expect("branch"),
            draft: false,
        },
        |_| Ok(()),
    )
    .await
    .map(|(stored, ())| stored)
    .expect("open");
    let err = link_work_item_mr_decided(
        &w.t.db,
        &ctx(Actor::User(w.alice), "req-link-api"),
        (api.project_id, mr.iid),
        &reference("PBI-3"),
        w.alice,
        |_| Ok(()),
    )
    .await
    .expect_err("not bound");
    assert_reason(
        &err,
        ErrorCode::WorkItemProjectNotBound,
        "project_not_bound",
    );
    // Detection links nothing there either.
    drain(&w.t).await;
    assert!(link_rows(&w.t, &mr).await.is_empty());
    w.t.drop_now().await;
}

#[tokio::test]
async fn manual_links_past_the_limit_are_int_0019() {
    let w = world().await;
    let mr = open(&w, "Many", "many").await;
    for n in 1..=MAX_MANUAL_LINKS {
        assert_eq!(
            link(&w, &mr, &format!("PBI-{n}"), &format!("req-link-{n}")).await,
            LinkOutcome::Changed
        );
    }
    let err = link_work_item_mr_decided(
        &w.t.db,
        &ctx(Actor::User(w.alice), "req-link-over"),
        (w.web.project_id, mr.iid),
        &reference("PBI-999"),
        w.alice,
        |_| Ok(()),
    )
    .await
    .expect_err("limit");
    assert_reason(
        &err,
        ErrorCode::WorkItemLinkLimitReached,
        "manual_link_limit",
    );
    w.t.drop_now().await;
}

/// A rejected statement, rolled back, and the constraint it named.
async fn violated(t: &FreshDb, sql: &str) -> String {
    let mut su = t.superuser().await;
    let mut tx = su.begin().await.expect("begin");
    let err = sqlx::query(sql)
        .execute(&mut *tx)
        .await
        .expect_err("refused");
    let name = err
        .as_database_error()
        .and_then(|d| d.constraint().map(str::to_owned))
        .unwrap_or_else(|| err.to_string());
    tx.rollback().await.expect("rollback");
    name
}

#[tokio::test]
async fn work_item_checks_refuse_raw_rows_by_constraint_name() {
    let w = world().await;
    let mr = open(&w, "PBI-1", "one").await;
    drain(&w.t).await;
    let i = w.integration.get();
    let (m, p) = (mr.id.get(), w.web.project_id.get());
    let item: i64 = sqlx::query_scalar("SELECT id FROM forge.work_items WHERE ref = 'PBI-1'")
        .fetch_one(&mut w.t.superuser().await)
        .await
        .expect("item");
    let a = w.alice.get();
    for (sql, constraint) in [
        (format!("INSERT INTO forge.work_items (integration_id, external_project_id, ref) VALUES ({i}, 'kf-1', 'pbi-1')"), "work_items_ref_format"),
        (format!("INSERT INTO forge.work_items (integration_id, external_project_id, ref) VALUES ({i}, 'kf-1', 'PBI-01')"), "work_items_ref_format"),
        (format!("INSERT INTO forge.work_items (integration_id, external_project_id, ref) VALUES ({i}, 'kf 1', 'PBI-1')"), "work_items_external_project_id_format"),
        (format!("INSERT INTO forge.work_items (integration_id, external_project_id, ref) VALUES ({i}, 'kf-1', 'PBI-1')"), "work_items_identity"),
        (format!("INSERT INTO forge.merge_request_work_items (merge_request_id, project_id, work_item_id, source, linked_seq) VALUES ({m}, {p}, {item}, 'detected', 1)"), "merge_request_work_items_source"),
        (format!("INSERT INTO forge.merge_request_work_items (merge_request_id, project_id, work_item_id, source) VALUES ({m}, {p}, {item}, 'manual')"), "merge_request_work_items_source_fields"),
        (format!("INSERT INTO forge.merge_request_work_items (merge_request_id, project_id, work_item_id, source, linked_seq) VALUES ({m}, {p}, {item}, 'auto', 0)"), "merge_request_work_items_linked_seq"),
        (format!("INSERT INTO forge.merge_request_work_items (merge_request_id, project_id, work_item_id, source, linked_by, unlinked_at, unlink_source) VALUES ({m}, {p}, {item}, 'manual', {a}, now(), 'manual')"), "merge_request_work_items_unlink_source_fields"),
        (format!("INSERT INTO forge.merge_request_work_items (merge_request_id, project_id, work_item_id, source, linked_by, unlinked_at, unlink_source) VALUES ({m}, {p}, {item}, 'manual', {a}, now(), 'bot')"), "merge_request_work_items_unlink_source"),
        (format!("INSERT INTO forge.merge_request_work_items (merge_request_id, project_id, work_item_id, source, linked_by) VALUES ({m}, {p}, {item}, 'manual', {a})"), "merge_request_work_items_one_active"),
        ("INSERT INTO forge.event_work_items (seq, work_item_id) VALUES (0, 1)".to_owned(), "event_work_items_seq_positive"),
    ] {
        assert_eq!(violated(&w.t, &sql).await, constraint, "{sql}");
    }
    // The guard: the link of a row never changes, an ended row is final.
    let changed = violated(
        &w.t,
        "UPDATE forge.merge_request_work_items SET source = 'manual', linked_by = 1, linked_seq = NULL",
    )
    .await;
    assert!(changed.contains("never changes"), "{changed}");
    // Ended once (in a transaction that is rolled back: the deferred
    // trigger would refuse a detected end outside the subscriber), then
    // final.
    let ended = {
        let mut su = w.t.superuser().await;
        let mut tx = su.begin().await.expect("begin");
        sqlx::query(
            "UPDATE forge.merge_request_work_items SET unlinked_at = now(), unlink_source = 'auto'",
        )
        .execute(&mut *tx)
        .await
        .expect("end once");
        let err = sqlx::query(
            "UPDATE forge.merge_request_work_items SET unlinked_at = now() - interval '1 day'",
        )
        .execute(&mut *tx)
        .await
        .expect_err("final");
        tx.rollback().await.expect("rollback");
        err.to_string()
    };
    assert!(ended.contains("ended link"), "{ended}");
    // forge_app: no UPDATE of a work item or an event's work items.
    for sql in [
        "UPDATE forge.work_items SET ref = 'PBI-2'",
        "UPDATE forge.event_work_items SET seq = seq",
        "UPDATE forge.merge_request_work_items SET work_item_id = work_item_id",
    ] {
        let err = sqlx::query(sql)
            .execute(w.t.db.write_pool())
            .await
            .expect_err(sql);
        assert!(
            err.to_string().contains("permission denied"),
            "{sql}: {err}"
        );
    }
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_purge_removes_the_merge_requests_work_item_links() {
    let w = world().await;
    let mr = open(&w, "PBI-1", "one").await;
    drain(&w.t).await;
    assert_eq!(link_rows(&w.t, &mr).await.len(), 1);
    let p = w.web.project_id;
    projects::delete_decided(
        &w.t.db,
        &system("req-delete"),
        p,
        w.alice,
        std::time::Duration::from_secs(3 * 86_400),
        |_| Ok(()),
    )
    .await
    .expect("delete");
    w.t.commit_audited(
        ("project", p.get()),
        &[format!(
            "UPDATE forge.projects SET deleted_at = now() - interval '2 days', \
             purge_after = now() - interval '1 second' WHERE id = {}",
            p.get()
        )],
    )
    .await;
    let _started = projects::purge::start(&w.t.db, &system("req-start"), p)
        .await
        .expect("start");
    let finished = projects::purge::finish(&w.t.db, &system("req-finish"), p, true)
        .await
        .expect("finish");
    assert!(
        matches!(finished, projects::purge::Finished::Purged(_)),
        "{finished:?}"
    );
    let left: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM forge.merge_request_work_items WHERE project_id = $1",
    )
    .bind(p.get())
    .fetch_one(&mut w.t.superuser().await)
    .await
    .expect("count");
    assert_eq!(left, 0);
    // The work item and the events' work items stay (no project row).
    let items: i64 = sqlx::query_scalar("SELECT count(*) FROM forge.work_items")
        .fetch_one(&mut w.t.superuser().await)
        .await
        .expect("items");
    assert_eq!(items, 1);
    w.t.drop_now().await;
}

/// The constraint a statement in forge_app's own transaction is refused by
/// (at the statement or, for a deferred trigger, at COMMIT).
async fn refused_as_app(t: &FreshDb, sql: &str) -> String {
    let mut tx = t.db.write_pool().begin().await.expect("begin");
    let err = match sqlx::query(sql).execute(&mut *tx).await {
        Err(e) => e,
        Ok(_) => tx.commit().await.expect_err("refused at commit"),
    };
    err.as_database_error()
        .and_then(|d| d.constraint().map(str::to_owned))
        .unwrap_or_else(|| err.to_string())
}

/// I1b review (database HIGH): a person's link and its end need their
/// `work_item.*` audit row, a detected one the subscriber's progress, in
/// the same transaction; a person's link ends only as a person's; the
/// rows go only with the purge (the repository `purging`); the link's
/// project is its merge request's; `linked_at` is the database's.
#[tokio::test]
async fn link_rows_are_backed_by_audit_rows_and_the_purge() {
    let w = world().await;
    let mr = open(&w, "Fix", "fix").await;
    drain(&w.t).await;
    assert_eq!(
        link(&w, &mr, "PBI-9", "req-link-9").await,
        LinkOutcome::Changed
    );
    let (m, p, a) = (mr.id.get(), w.web.project_id.get(), w.alice.get());
    let manual: i64 =
        sqlx::query_scalar("SELECT id FROM forge.merge_request_work_items WHERE source = 'manual'")
            .fetch_one(&mut w.t.superuser().await)
            .await
            .expect("manual row");
    let other: i64 = sqlx::query_scalar(
        "INSERT INTO forge.work_items (integration_id, external_project_id, ref) \
         VALUES ($1, 'kf-1', 'PBI-10') RETURNING id",
    )
    .bind(w.integration.get())
    .fetch_one(w.t.db.write_pool())
    .await
    .expect("work item");
    // A person's link without its audit row.
    assert_eq!(
        refused_as_app(
            &w.t,
            &format!(
                "INSERT INTO forge.merge_request_work_items (merge_request_id, project_id, \
             work_item_id, source, linked_by) VALUES ({m}, {p}, {other}, 'manual', {a})"
            )
        )
        .await,
        "merge_request_work_items_audited"
    );
    // A person's link ended without its audit row.
    assert_eq!(
        refused_as_app(
            &w.t,
            &format!(
                "UPDATE forge.merge_request_work_items SET unlinked_at = now(), \
             unlink_source = 'manual', unlinked_by = {a} WHERE id = {manual}"
            )
        )
        .await,
        "merge_request_work_items_audited"
    );
    // A detected link outside the subscriber's step.
    assert_eq!(
        refused_as_app(
            &w.t,
            &format!(
                "INSERT INTO forge.merge_request_work_items (merge_request_id, project_id, \
             work_item_id, source, linked_seq) VALUES ({m}, {p}, {other}, 'auto', 1)"
            )
        )
        .await,
        "merge_request_work_items_derived"
    );
    // A person's link never ends as detection's.
    let ended = violated(
        &w.t,
        &format!(
            "UPDATE forge.merge_request_work_items SET unlinked_at = now(), \
         unlink_source = 'auto' WHERE id = {manual}"
        ),
    )
    .await;
    assert!(ended.contains("a person's link"), "{ended}");
    // The link's project is its merge request's.
    let api = projects::create(
        &w.t.db,
        &system("req-project-api"),
        &NewProject::new("api", w.namespace, "API", w.alice),
    )
    .await
    .expect("api");
    assert_eq!(
        violated(
            &w.t,
            &format!(
                "INSERT INTO forge.merge_request_work_items (merge_request_id, project_id, \
             work_item_id, source, linked_by) VALUES ({m}, {}, {other}, 'manual', {a})",
                api.project_id.get()
            )
        )
        .await,
        "merge_request_work_items_merge_request_project"
    );
    // linked_at is not forge_app's to write.
    let err = sqlx::query(&format!(
        "INSERT INTO forge.merge_request_work_items (merge_request_id, project_id, \
         work_item_id, source, linked_seq, linked_at) VALUES ({m}, {p}, {other}, 'auto', 1, now())"
    ))
    .execute(w.t.db.write_pool())
    .await
    .expect_err("column grant");
    assert!(err.to_string().contains("permission denied"), "{err}");
    // Pending deletion is not the purge: the rows stay until it runs.
    projects::delete_decided(
        &w.t.db,
        &system("req-delete"),
        w.web.project_id,
        w.alice,
        std::time::Duration::from_secs(3 * 86_400),
        |_| Ok(()),
    )
    .await
    .expect("delete");
    let err = sqlx::query("DELETE FROM forge.merge_request_work_items")
        .execute(w.t.db.write_pool())
        .await
        .expect_err("not purging");
    assert!(
        err.to_string().contains("deleted only by the purge"),
        "{err}"
    );
    w.t.drop_now().await;
}

/// I1b review (security / rust MEDIUM): an ordered subscriber that meets a
/// type it does not know stops and alerts instead of skipping it (a newer
/// writer's event may carry links this build would lose).
#[tokio::test]
async fn an_unknown_event_type_stops_work_item_sync() {
    let w = world().await;
    drain(&w.t).await;
    w.t.exec_superuser(
        "INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id) \
         VALUES ('system', 'future.happened', 'succeeded', 'repository', 'r-future'); \
         INSERT INTO forge.events (event_id, event_type, schema_version, aggregate_type, \
         aggregate_id, actor_kind, request_id, payload) VALUES (gen_random_uuid(), \
         'future.happened', 1, 'repository', 1, 'system', 'r-future', '{}')",
    )
    .await;
    crate::sequencer::run_once(
        &w.t.db,
        crate::sequencer::BatchSize::MAX,
        crate::sequencer::AuditSequencing::Skip,
    )
    .await
    .expect("sequencer");
    for attempt in 1..=4 {
        let Progress::Failed(incident) = subscribers::run_once(&w.t.db, &WorkItemSync)
            .await
            .expect("step")
        else {
            panic!("attempt {attempt} is a counted failure");
        };
        assert_reason(&incident.error, ErrorCode::Internal, "unknown_event_type");
        let text = incident.error.to_string();
        assert!(text.contains("future.happened"), "{text}");
    }
    let Progress::Stuck(incident) = subscribers::run_once(&w.t.db, &WorkItemSync)
        .await
        .expect("step")
    else {
        panic!("the fifth failure stops it");
    };
    assert_reason(&incident.error, ErrorCode::SubscriberLag, "stuck");
    w.t.drop_now().await;
}

/// (external project, ref) of the newest event of `event_type`.
async fn items_of_last(t: &FreshDb, event_type: &str) -> Vec<(String, String)> {
    sqlx::query_as(
        "SELECT w.external_project_id, w.ref FROM forge.event_work_items AS e \
         JOIN forge.work_items AS w ON w.id = e.work_item_id \
         WHERE e.seq = (SELECT max(seq) FROM forge.events WHERE event_type = $1) \
         ORDER BY 1, 2",
    )
    .bind(event_type)
    .fetch_all(&mut t.superuser().await)
    .await
    .expect("event work items")
}

/// I1b review (security MEDIUM): after the project is bound to another
/// kanflow project, its events name only the current binding's work items
/// (never the old project's PBI of the same number).
#[tokio::test]
async fn events_record_only_the_current_bindings_work_items() {
    let w = world().await;
    let mr = open(&w, "Fix", "fix").await;
    drain(&w.t).await;
    // A person's link to the old binding's PBI-1 (detection never ends it).
    assert_eq!(
        link(&w, &mr, "PBI-1", "req-link-old").await,
        LinkOutcome::Changed
    );
    integrations::unbind_decided(
        &w.t.db,
        &system("req-unbind"),
        w.web.project_id,
        w.alice,
        w.integration,
        |_| Ok(()),
    )
    .await
    .expect("unbind");
    let (_bound, _, ()) = integrations::bind_decided(
        &w.t.db,
        &system("req-rebind"),
        w.web.project_id,
        w.alice,
        w.integration,
        "kf-2",
        |_| Ok(()),
    )
    .await
    .expect("rebind");
    retitle(&w, &mr, "Fix (PBI-1) again", "req-edit-rebound").await;
    push(
        &w,
        &w.web,
        "req-push-rebound",
        vec![created_branch("refs/heads/fix")],
    )
    .await;
    drain(&w.t).await;
    let current = [("kf-2".to_owned(), "PBI-1".to_owned())];
    assert_eq!(items_of_last(&w.t, "merge_request.updated").await, current);
    assert_eq!(items_of_last(&w.t, "repository.pushed").await, current);
    w.t.drop_now().await;
}
