//! The event feed against a migrated database (M3 I1c): plan 02 §8.2's
//! regressions on the page read (closed without merge, close → reopen →
//! merge, the branch-deleting merge, open → close between polls), what is
//! never shown (unbound projects old and new, instance-wide and
//! maintainer-class events), the work-item-sync watermark, the page caps,
//! the read records, the lag, retention and the epoch. The visible set is
//! decided by forge-auth's `prove_visible` (dev-dependency), as in the
//! product. Synthetic data only.

use forge_auth::{
    prove, prove_visible, AccountKind, Action, Credential, IntegrationFeedCtx,
    LoadedIntegrationFeed, Principal, Scope,
};
use forge_core::ids::{IntegrationId, ProjectId, UserId};
use forge_core::{
    EventType, IntegrationKind, ObjectId, PushRecord, PushRecorder, PushUpdateKind, PushedRef,
    Pusher, RefName, RequestId, UserKind, VisibleSet,
};
use forge_errors::ErrorCode;

use super::*;
use crate::change::{
    Actor, AuditEntry, ChangeContext, ChangeRecord, ChangeTx, DomainEvent, Outcome,
};
use crate::integrations::{self, NewIntegration};
use crate::projects::{self, CreatedProject, NewProject};
use crate::review::{self, NewMergeRequest, StoredMergeRequest};
use crate::service_accounts::{self, NewServiceAccount};
use crate::subscribers::{self, Progress, Start};
use crate::test_support::*;
use crate::users::{self, NewUser};
use crate::work_items::WorkItemSync;

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
    sa: UserId,
    integration: IntegrationId,
}

/// alice, her project `web` bound to kanflow project `kf-1` through the
/// integration acting as `kanflow-sync`, and work-item-sync at the head.
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
    let w = World {
        t,
        alice: alice.user_id,
        namespace: alice.namespace_id,
        web,
        sa,
        integration,
    };
    bind(&w, w.web.project_id, "kf-1").await;
    subscribers::register(&w.t.db, &WorkItemSync, Start::Head)
        .await
        .expect("register");
    drain(&w.t).await;
    w
}

async fn bind(w: &World, project: ProjectId, external: &str) {
    let (_bound, _, ()) = integrations::bind_decided(
        &w.t.db,
        &system(&format!("req-bind-{}-{external}", project.get())),
        project,
        w.alice,
        w.integration,
        external,
        |_| Ok(()),
    )
    .await
    .expect("bind");
}

async fn unbind(w: &World, project: ProjectId) {
    integrations::unbind_decided(
        &w.t.db,
        &system(&format!("req-unbind-{}", project.get())),
        project,
        w.alice,
        w.integration,
        |_| Ok(()),
    )
    .await
    .expect("unbind");
}

async fn second_project(w: &World, name: &str) -> CreatedProject {
    projects::create(
        &w.t.db,
        &system(&format!("req-project-{name}")),
        &NewProject::new(name, w.namespace, name, w.alice),
    )
    .await
    .expect("project")
}

/// Number the events, then run work-item-sync until it caught up.
async fn drain(t: &FreshDb) {
    sequence(t).await;
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
        sequence(t).await;
    }
}

async fn sequence(t: &FreshDb) {
    crate::sequencer::run_once(
        &t.db,
        crate::sequencer::BatchSize::MAX,
        crate::sequencer::AuditSequencing::Skip,
    )
    .await
    .expect("sequencer");
}

async fn open_mr(w: &World, project: ProjectId, title: &str, source: &str) -> StoredMergeRequest {
    review::open_mr_decided(
        &w.t.db,
        &ctx(
            Actor::User(w.alice),
            &format!("req-open-{}", source.replace('/', "-")),
        ),
        project,
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

async fn close_mr(w: &World, mr: &StoredMergeRequest, request: &str) {
    let (_, _, ()) = review::close_mr_decided(
        &w.t.db,
        &ctx(Actor::User(w.alice), request),
        mr.project_id,
        mr.iid,
        w.alice,
        |_| Ok(()),
    )
    .await
    .expect("close");
}

async fn reopen_mr(w: &World, mr: &StoredMergeRequest, request: &str) {
    let (_, _, ()) = review::reopen_mr_decided(
        &w.t.db,
        &ctx(Actor::User(w.alice), request),
        mr.project_id,
        mr.iid,
        w.alice,
        None,
        |_| Ok(()),
    )
    .await
    .expect("reopen");
}

/// The merge job's two events (R4 needs git; the feed reads only the
/// rows): `merge_request.merged` and `merge_request.source_branch_deleted`.
async fn merged_with_branch_deleted(w: &World, mr: &StoredMergeRequest) {
    let tx = ChangeTx::begin(&w.t.db, &system("req-merge"))
        .await
        .expect("begin");
    let event = |event_type| DomainEvent {
        event_type,
        schema_version: 1,
        aggregate_type: "merge_request",
        aggregate_id: mr.id.get(),
        project_id: Some(mr.project_id),
        payload: serde_json::json!({
            "merge_request_id": mr.id.get(),
            "project_id": mr.project_id.get(),
            "iid": mr.iid,
        }),
        dedup_key: None,
    };
    tx.commit(ChangeRecord {
        audit: AuditEntry {
            action: "merge_request.merged",
            outcome: Outcome::Succeeded,
            resource_type: "project",
            resource_id: Some(mr.project_id.get()),
            project_id: Some(mr.project_id),
            error_code: None,
            details: serde_json::json!({}),
        },
        affected: Vec::new(),
        events: vec![
            event(EventType::MergeRequestMerged),
            event(EventType::MergeRequestSourceBranchDeleted),
        ],
    })
    .await
    .expect("merged");
}

/// Events of `event_type` with a `pad` of `bytes` in their payload.
async fn padded_events(w: &World, event_type: EventType, count: usize, bytes: usize) {
    let mut left = count;
    while left > 0 {
        let batch = left.min(100);
        let tx = ChangeTx::begin(&w.t.db, &system("req-padded"))
            .await
            .expect("begin");
        tx.commit(ChangeRecord {
            audit: AuditEntry {
                action: "probe.padded",
                outcome: Outcome::Succeeded,
                resource_type: "project",
                resource_id: Some(w.web.project_id.get()),
                project_id: Some(w.web.project_id),
                error_code: None,
                details: serde_json::json!({}),
            },
            affected: Vec::new(),
            events: (0..batch)
                .map(|_| DomainEvent {
                    event_type,
                    schema_version: 1,
                    aggregate_type: "project",
                    aggregate_id: w.web.project_id.get(),
                    project_id: Some(w.web.project_id),
                    payload: serde_json::json!({ "pad": "x".repeat(bytes) }),
                    dedup_key: None,
                })
                .collect(),
        })
        .await
        .expect("padded");
        left -= batch;
    }
}

fn principal(sa: UserId) -> Principal {
    Principal::authenticated(
        sa,
        false,
        true,
        Credential::Token(vec![Scope::ReadApi]),
        AccountKind::ServiceAccount,
    )
}

/// The feed's decision in one snapshot (the product's path).
async fn decided(w: &World) -> (FeedSnapshot, FeedState, VisibleSet) {
    let mut snap = open(&w.t.db).await.expect("snapshot");
    let state = snap.state().await.expect("state").expect("feed state");
    let material = snap
        .material(w.integration)
        .await
        .expect("material")
        .expect("integration");
    let proof = prove(
        &principal(w.sa),
        Action::IntegrationEventRead,
        IntegrationFeedCtx::new(
            LoadedIntegrationFeed {
                integration: w.integration,
                service_account: w.sa,
                enabled: true,
            },
            Some(w.sa),
            false,
        ),
    )
    .expect("feed proof");
    let visible = prove_visible(&proof, &material).expect("visible");
    (snap, state, visible)
}

/// Every visible event after `after` up to the bound.
async fn read_all(w: &World, after: i64) -> Vec<FeedEvent> {
    let (mut snap, state, visible) = decided(w).await;
    let page = snap
        .read_feed_page(&visible, after, state.bound(), MAX_LIMIT, MAX_PAGE_BYTES)
        .await
        .expect("page");
    snap.close().await;
    assert!(!page.has_more, "one page holds the test's events");
    page.events
}

fn types(events: &[FeedEvent]) -> Vec<&str> {
    events.iter().map(|e| e.event_type.as_str()).collect()
}

fn refs(event: &FeedEvent) -> Vec<(&str, &str, &str)> {
    event
        .work_items
        .iter()
        .map(|i| {
            (
                i.provider.as_str(),
                i.project_id.as_str(),
                i.reference.as_str(),
            )
        })
        .collect()
}

async fn head(w: &World) -> i64 {
    let (snap, state, _) = decided(w).await;
    snap.close().await;
    state.bound()
}

// ── plan 02 §8.2: the regressions ──────────────────────────────────────

/// A merge request closed without merging is in the feed with its work
/// item (the dev_status part is I1d's).
#[tokio::test]
async fn closed_without_merge_is_returned_and_dev_status_closed() {
    let w = world().await;
    let start = head(&w).await;
    let mr = open_mr(&w, w.web.project_id, "Login form", "feature/PBI-12-login").await;
    close_mr(&w, &mr, "req-close").await;
    drain(&w.t).await;
    let events = read_all(&w, start).await;
    let mr_events: Vec<&FeedEvent> = events
        .iter()
        .filter(|e| e.event_type.starts_with("merge_request."))
        .collect();
    assert_eq!(
        mr_events
            .iter()
            .map(|e| e.event_type.as_str())
            .collect::<Vec<_>>(),
        ["merge_request.opened", "merge_request.closed"]
    );
    let closed = mr_events[1];
    assert_eq!(refs(closed), [("kanflow", "kf-1", "PBI-12")]);
    assert_eq!(closed.project_id, w.web.project_id);
    assert_eq!(closed.actor.kind, "user");
    assert_eq!(
        closed
            .actor
            .user
            .as_ref()
            .and_then(|u| u.username.as_deref()),
        Some("alice")
    );
    w.t.drop_now().await;
}

#[tokio::test]
async fn close_reopen_merge_in_order() {
    let w = world().await;
    let start = head(&w).await;
    let mr = open_mr(&w, w.web.project_id, "PBI-3 export", "export").await;
    close_mr(&w, &mr, "req-close").await;
    reopen_mr(&w, &mr, "req-reopen").await;
    merged_with_branch_deleted(&w, &mr).await;
    drain(&w.t).await;
    let events = read_all(&w, start).await;
    let mr_types: Vec<&str> = types(&events)
        .into_iter()
        .filter(|t| t.starts_with("merge_request."))
        .collect();
    assert_eq!(
        mr_types,
        [
            "merge_request.opened",
            "merge_request.closed",
            "merge_request.reopened",
            "merge_request.merged",
            "merge_request.source_branch_deleted",
        ]
    );
    let seqs: Vec<i64> = events.iter().map(|e| e.seq).collect();
    assert!(seqs.windows(2).all(|p| p[0] < p[1]), "{seqs:?}");
    w.t.drop_now().await;
}

/// 02 §8.2: after "the merge that deletes the branch" the link and every
/// event (with its work item) are still read.
#[tokio::test]
async fn events_survive_branch_deleting_merge() {
    let w = world().await;
    let start = head(&w).await;
    let mr = open_mr(
        &w,
        w.web.project_id,
        "Login form",
        "feature/PBI-7-login-form",
    )
    .await;
    merged_with_branch_deleted(&w, &mr).await;
    let who = Pusher {
        request_id: RequestId::parse("req-push-delete").expect("id"),
        user: w.alice,
        kind: UserKind::Human,
        credential_id: None,
        source_ip: None,
        user_agent: None,
    };
    let recorder: &dyn PushRecorder = &w.t.db;
    let recorded = recorder
        .record_push(
            &who,
            &PushRecord {
                repository_id: w.web.repository_id,
                updates: vec![PushedRef {
                    name: RefName::parse("refs/heads/feature/PBI-7-login-form").expect("ref"),
                    old: ObjectId::parse(&"a".repeat(40)).expect("oid"),
                    new: ObjectId::parse(&"0".repeat(40)).expect("oid"),
                    kind: PushUpdateKind::Delete,
                    commits: Vec::new(),
                    total_commits: 0,
                    total_commits_partial: false,
                }],
            },
        )
        .await
        .expect("push");
    assert_eq!(recorded, forge_core::PushRecorded::Recorded);
    drain(&w.t).await;
    let events = read_all(&w, start).await;
    for wanted in [
        "merge_request.opened",
        "work_item.linked",
        "merge_request.merged",
        "merge_request.source_branch_deleted",
        "repository.pushed",
    ] {
        let event = events
            .iter()
            .find(|e| e.event_type == wanted)
            .unwrap_or_else(|| panic!("{wanted} in {:?}", types(&events)));
        assert_eq!(refs(event), [("kanflow", "kf-1", "PBI-7")], "{wanted}");
    }
    w.t.drop_now().await;
}

/// 02 §8.2: polling misses nothing that happened between two polls.
#[tokio::test]
async fn open_then_close_between_polls_returns_both() {
    let w = world().await;
    // The first poll reads up to the head and keeps that position.
    let _first_poll = read_all(&w, 0).await;
    let after = head(&w).await;
    let mr = open_mr(&w, w.web.project_id, "Quick fix PBI-9", "quick-fix").await;
    close_mr(&w, &mr, "req-close").await;
    drain(&w.t).await;
    let second_poll = read_all(&w, after).await;
    assert_eq!(
        types(&second_poll)
            .into_iter()
            .filter(|t| t.starts_with("merge_request."))
            .collect::<Vec<_>>(),
        ["merge_request.opened", "merge_request.closed"]
    );
    w.t.drop_now().await;
}

// ── what the feed never shows ──────────────────────────────────────────

/// ADR-0016 §7: after an unbind neither the project's old events nor its
/// new ones are returned; the still-bound project's are.
#[tokio::test]
async fn unbound_project_old_and_new_events_hidden() {
    let w = world().await;
    let api = second_project(&w, "api").await;
    bind(&w, api.project_id, "kf-2").await;
    drain(&w.t).await;
    let start = head(&w).await;
    let _old = open_mr(&w, api.project_id, "PBI-1 api", "api-old").await;
    let _kept = open_mr(&w, w.web.project_id, "PBI-2 web", "web-kept").await;
    drain(&w.t).await;
    let both: Vec<ProjectId> = read_all(&w, start)
        .await
        .iter()
        .map(|e| e.project_id)
        .collect();
    assert!(both.contains(&api.project_id) && both.contains(&w.web.project_id));
    unbind(&w, api.project_id).await;
    let _new = open_mr(&w, api.project_id, "PBI-4 api", "api-new").await;
    drain(&w.t).await;
    let after: Vec<FeedEvent> = read_all(&w, start).await;
    assert!(
        after.iter().all(|e| e.project_id == w.web.project_id),
        "{:?}",
        after
            .iter()
            .map(|e| (e.project_id, &e.event_type))
            .collect::<Vec<_>>()
    );
    assert!(types(&after).contains(&"merge_request.opened"));
    w.t.drop_now().await;
}

/// Instance-wide events (no project) and maintainer-class events of a
/// bound project (the binding's own `member.added`) never appear, even
/// with the allowlist `*`.
#[tokio::test]
async fn project_null_and_maintainer_class_never_in_feed() {
    let w = world().await;
    let all = read_all(&w, 0).await;
    assert!(!all.is_empty(), "project.created is integration class");
    let counted: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM forge.events WHERE project_id IS NULL OR event_type = 'member.added'",
    )
    .fetch_one(&mut w.t.superuser().await)
    .await
    .expect("count");
    assert!(
        counted > 0,
        "the fixture wrote instance-wide and member events"
    );
    for event in &all {
        assert!(
            EventType::parse(&event.event_type)
                .is_some_and(|t| t.visibility_class() == forge_core::VisibilityClass::Integration),
            "{}",
            event.event_type
        );
    }
    w.t.drop_now().await;
}

// ── the watermark, the caps ────────────────────────────────────────────

/// The bound is min(high_seq, work-item-sync): a numbered event whose work
/// items are not derived yet is withheld, then returned with them.
#[tokio::test]
async fn events_beyond_work_item_sync_watermark_withheld() {
    let w = world().await;
    let start = head(&w).await;
    let _mr = open_mr(&w, w.web.project_id, "PBI-5 search", "search").await;
    sequence(&w.t).await;
    let (snap, state, _) = decided(&w).await;
    snap.close().await;
    assert!(state.high_seq > state.work_items_seq, "{state:?}");
    assert_eq!(state.bound(), state.work_items_seq);
    assert!(read_all(&w, start).await.is_empty(), "withheld");
    drain(&w.t).await;
    let events = read_all(&w, start).await;
    let opened = events
        .iter()
        .find(|e| e.event_type == "merge_request.opened")
        .expect("opened");
    assert_eq!(refs(opened), [("kanflow", "kf-1", "PBI-5")]);
    w.t.drop_now().await;
}

/// At most `limit` (500) events and about 2 MiB per page; `has_more` says
/// when more remain.
#[tokio::test]
async fn response_capped_2mib_limit_500() {
    let w = world().await;
    let start = head(&w).await;
    padded_events(&w, EventType::ProjectCreated, 600, 10).await;
    drain(&w.t).await;
    let (mut snap, state, visible) = decided(&w).await;
    let page = snap
        .read_feed_page(&visible, start, state.bound(), MAX_LIMIT, MAX_PAGE_BYTES)
        .await
        .expect("page");
    assert_eq!(page.events.len(), 500);
    assert!(page.has_more);
    let rest = snap
        .read_feed_page(
            &visible,
            page.events.last().expect("last").seq,
            state.bound(),
            MAX_LIMIT,
            MAX_PAGE_BYTES,
        )
        .await
        .expect("rest");
    assert_eq!(rest.events.len(), 100);
    assert!(!rest.has_more);
    snap.close().await;

    let big_start = head(&w).await;
    padded_events(&w, EventType::ProjectCreated, 60, 60_000).await;
    drain(&w.t).await;
    let (mut snap, state, visible) = decided(&w).await;
    let page = snap
        .read_feed_page(
            &visible,
            big_start,
            state.bound(),
            MAX_LIMIT,
            MAX_PAGE_BYTES,
        )
        .await
        .expect("page");
    let bytes: usize = page
        .events
        .iter()
        .map(|e| e.payload.to_string().len())
        .sum();
    assert!(page.has_more);
    assert!(page.events.len() < 60 && !page.events.is_empty());
    assert!(bytes <= MAX_PAGE_BYTES, "{bytes}");
    snap.close().await;
    w.t.drop_now().await;
}

// ── the read records and the lag ───────────────────────────────────────

#[tokio::test]
async fn feed_reads_written_once_per_minute() {
    let w = world().await;
    let read = FeedRead {
        integration: w.integration,
        cursor_seq: 3,
        bound: 5,
        oldest_unread_at: None,
    };
    assert!(record_read(&w.t.db, &read).await.expect("first"));
    let again = FeedRead {
        cursor_seq: 4,
        ..read.clone()
    };
    assert!(!record_read(&w.t.db, &again).await.expect("within a minute"));
    let stored: i64 = sqlx::query_scalar(
        "SELECT cursor_seq FROM forge.integration_feed_reads WHERE integration_id = $1",
    )
    .bind(w.integration.get())
    .fetch_one(&mut w.t.superuser().await)
    .await
    .expect("row");
    assert_eq!(stored, 3);
    w.t.exec_superuser(
        "UPDATE forge.integration_feed_reads SET last_read_at = now() - interval '61 seconds'",
    )
    .await;
    assert!(record_read(&w.t.db, &again).await.expect("a minute later"));
    w.t.drop_now().await;
}

#[tokio::test]
async fn feed_lag_names_not_reading_and_behind_integrations() {
    let w = world().await;
    // Bound just now and never read: not lagging yet.
    let fresh = lag(&w.t.db).await.expect("lag").expect("migrated");
    assert_eq!(fresh.integrations, 1);
    fresh.check().expect("fresh binding");
    // The binding is two days old and nobody read.
    w.t.exec_superuser(
        "ALTER TABLE forge.integration_bindings DISABLE TRIGGER audited_change_present; \
         UPDATE forge.integration_bindings SET created_at = now() - interval '2 days'; \
         ALTER TABLE forge.integration_bindings ENABLE ALWAYS TRIGGER audited_change_present",
    )
    .await;
    let err = lag(&w.t.db)
        .await
        .expect("lag")
        .expect("migrated")
        .check()
        .expect_err("not reading");
    assert_reason(&err, ErrorCode::FeedConsumerLag, "not_reading");
    // A recent read whose oldest unread event is 50 days old (> 90 / 2).
    let read = FeedRead {
        integration: w.integration,
        cursor_seq: 0,
        bound: 1,
        oldest_unread_at: Some("2000-01-01T00:00:00Z".to_owned()),
    };
    assert!(record_read(&w.t.db, &read).await.expect("record"));
    w.t.exec_superuser(
        "UPDATE forge.integration_feed_reads SET oldest_unread_at = now() - interval '50 days'",
    )
    .await;
    let err = lag(&w.t.db)
        .await
        .expect("lag")
        .expect("migrated")
        .check()
        .expect_err("behind");
    assert_reason(&err, ErrorCode::FeedConsumerLag, "behind");
    assert!(err
        .context()
        .iter()
        .any(|(k, v)| *k == "integration_id" && *v == w.integration.get().to_string()));
    w.t.drop_now().await;
}

// ── generations, retention, the epoch ──────────────────────────────────

#[tokio::test]
async fn rebinding_a_project_gives_a_new_generation() {
    let w = world().await;
    let generation = || async {
        sqlx::query_scalar::<_, i64>(
            "SELECT generation FROM forge.integration_bindings WHERE project_id = $1",
        )
        .bind(w.web.project_id.get())
        .fetch_one(&mut w.t.superuser().await)
        .await
        .expect("generation")
    };
    let first = generation().await;
    let (snap, _, before) = decided(&w).await;
    snap.close().await;
    unbind(&w, w.web.project_id).await;
    bind(&w, w.web.project_id, "kf-1").await;
    let second = generation().await;
    assert_ne!(first, second);
    let (snap, _, after) = decided(&w).await;
    snap.close().await;
    assert_eq!(before.project_ids(), after.project_ids());
    assert_ne!(before.digest(), after.digest());
    // forge_app cannot choose a generation.
    let mut app = w.t.db.write_pool().acquire().await.expect("app");
    let err =
        sqlx::query("UPDATE forge.integration_bindings SET generation = 1 WHERE project_id = $1")
            .bind(w.web.project_id.get())
            .execute(&mut *app)
            .await
            .expect_err("no update of generation");
    assert!(err.to_string().contains("permission denied") || err.to_string().contains("generat"));
    w.t.drop_now().await;
}

/// Events past the retention period go with their work items, one bounded
/// batch at a time; retained_from moves past them; a lagging subscriber
/// holds them back until 1.5 × the period.
#[tokio::test]
async fn retention_deletes_old_events_and_their_work_items() {
    let w = world().await;
    let _mr = open_mr(&w, w.web.project_id, "PBI-8 old", "old").await;
    drain(&w.t).await;
    let request = RequestId::parse("req-retention").expect("id");
    // Nothing is due yet.
    let none = run_retention(&w.t.db, &request, RETENTION_BATCH)
        .await
        .expect("run");
    assert_eq!(
        none,
        RetentionBatch {
            deleted: 0,
            range: None
        }
    );
    let high: i64 = sqlx::query_scalar("SELECT high_seq FROM forge.event_feed_state")
        .fetch_one(&mut w.t.superuser().await)
        .await
        .expect("high");
    // Every event is 100 days old; a subscriber that stopped at seq 2
    // holds back the rest until 135 days.
    w.t.exec_superuser(
        "SET CONSTRAINTS ALL IMMEDIATE; \
         ALTER TABLE forge.events DISABLE TRIGGER events_append_only; \
         UPDATE forge.events SET occurred_at = now() - interval '100 days'; \
         ALTER TABLE forge.events ENABLE ALWAYS TRIGGER events_append_only; \
         INSERT INTO forge.event_subscribers (name, ordering, last_seq, updated_at) \
         VALUES ('probe-lagging', 'ordered', 2, now())",
    )
    .await;
    let held = run_retention(&w.t.db, &request, RETENTION_BATCH)
        .await
        .expect("held");
    assert_eq!(held.range, Some((1, 2)));
    let retained_from: i64 = sqlx::query_scalar("SELECT retained_from FROM forge.event_feed_state")
        .fetch_one(&mut w.t.superuser().await)
        .await
        .expect("retained_from");
    assert_eq!(retained_from, 3);
    // Past the hard limit the lagging subscriber no longer holds them.
    w.t.exec_superuser(
        "SET CONSTRAINTS ALL IMMEDIATE; \
         ALTER TABLE forge.events DISABLE TRIGGER events_append_only; \
         UPDATE forge.events SET occurred_at = now() - interval '140 days'; \
         ALTER TABLE forge.events ENABLE ALWAYS TRIGGER events_append_only",
    )
    .await;
    let rest = run_retention(&w.t.db, &request, RETENTION_BATCH)
        .await
        .expect("rest");
    assert_eq!(rest.range, Some((3, high)));
    let (events, items, audits): (i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM forge.events WHERE seq IS NOT NULL), \
                (SELECT count(*) FROM forge.event_work_items), \
                (SELECT count(*) FROM forge.audit_log WHERE action = 'events.retention')",
    )
    .fetch_one(&mut w.t.superuser().await)
    .await
    .expect("counts");
    assert_eq!((events, items, audits), (0, 0, 2));
    // forge_app cannot write the policy.
    let mut app = w.t.db.write_pool().acquire().await.expect("app");
    sqlx::query("UPDATE forge.retention_policy SET events_days = 7")
        .execute(&mut *app)
        .await
        .expect_err("forge_app cannot shorten retention");
    w.t.drop_now().await;
}

#[tokio::test]
async fn new_epoch_rotates_the_feed_epoch() {
    let w = world().await;
    let before = head_epoch(&w).await;
    let (cfg, _pw) = maint_config(&w.t);
    let mut maint = Db::connect_maint(&cfg.db).await.expect("forge_maint");
    let rotated = new_epoch(&mut maint, &RequestId::parse("req-new-epoch").expect("id"))
        .await
        .expect("rotated");
    maint.close().await;
    assert_eq!(rotated.old, before);
    assert_ne!(rotated.new, before);
    assert_eq!(head_epoch(&w).await, rotated.new);
    let logged = epoch_changed(&rotated);
    assert_reason(&logged, ErrorCode::FeedEpochChanged, "manual");
    let (count, label): (i64, Option<String>) = sqlx::query_as(
        "SELECT count(*), max(actor_label) FROM forge.audit_log WHERE action = 'feed.epoch_rotated'",
    )
    .fetch_one(&mut w.t.superuser().await)
    .await
    .expect("audit");
    assert_eq!((count, label.as_deref()), (1, Some("forge_maint")));
    // The application cannot rotate it.
    let mut app = w.t.db.write_pool().acquire().await.expect("app");
    sqlx::query("SELECT * FROM forge.feed_new_epoch('manual', 'req-x')")
        .execute(&mut *app)
        .await
        .expect_err("forge_app has no EXECUTE");
    w.t.drop_now().await;
}

async fn head_epoch(w: &World) -> [u8; 16] {
    let (snap, state, _) = decided(w).await;
    snap.close().await;
    state.epoch
}

/// A config whose maintenance login is forge_maint (the password file
/// lives as long as the second value).
fn maint_config(t: &FreshDb) -> (TestConfig, PasswordFile) {
    let pw = PasswordFile::new(MAINT_PASSWORD);
    let (url, path) = (url_as("forge_maint", &t.name), pw.path());
    let pairs = [
        ("FORGE_DATABASE_MAINT_URL", url.as_str()),
        ("FORGE_DATABASE_MAINT_PASSWORD_FILE", path.as_str()),
    ];
    (t.app_config(&pairs), pw)
}
