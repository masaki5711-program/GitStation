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

// ── review of I1c (2026-11-04) ─────────────────────────────────────────

/// A SQLSTATE of a failed statement.
fn sqlstate(e: &sqlx::Error) -> String {
    e.as_database_error()
        .and_then(|d| d.code())
        .map(|c| c.into_owned())
        .unwrap_or_default()
}

/// The constraint name of a failed statement.
fn constraint(e: &sqlx::Error) -> String {
    e.as_database_error()
        .and_then(|d| d.constraint())
        .map(str::to_owned)
        .unwrap_or_default()
}

/// forge_app cannot write a binding's generation, `OVERRIDING SYSTEM VALUE`
/// included (it could reuse an old one and continue a cursor across a
/// rebind): 42501, nothing written.
#[tokio::test]
async fn forge_app_cannot_choose_a_binding_generation() {
    let w = world().await;
    let api = second_project(&w, "api").await;
    let mut tx = w.t.db.write_pool().begin().await.expect("begin");
    sqlx::query(
        "INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, resource_id, \
         request_id) VALUES ('system', 'probe.changed', 'succeeded', 'project', $1, 'r-probe')",
    )
    .bind(api.project_id.get())
    .execute(&mut *tx)
    .await
    .expect("audit");
    let err = sqlx::query(
        "INSERT INTO forge.integration_bindings (integration_id, project_id, external_project_id, \
         granted_membership, generation) OVERRIDING SYSTEM VALUE VALUES ($1, $2, 'kf-9', false, 1)",
    )
    .bind(w.integration.get())
    .bind(api.project_id.get())
    .execute(&mut *tx)
    .await
    .expect_err("generation is not forge_app's");
    assert_eq!(sqlstate(&err), "42501", "{err}");
    drop(tx);
    w.t.drop_now().await;
}

/// `count` numbered `project.created` events of `project` written in one
/// superuser transaction (with its audit row), then numbered.
async fn bulk_events(w: &World, project: ProjectId, count: i64) {
    w.t.exec_superuser(&format!(
        "BEGIN; \
         INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, resource_id, \
           request_id) VALUES ('system', 'probe.bulk', 'succeeded', 'project', {p}, 'r-bulk'); \
         INSERT INTO forge.events (event_id, event_type, schema_version, aggregate_type, \
           aggregate_id, project_id, actor_kind, request_id, payload) \
         SELECT gen_random_uuid(), 'project.created', 1, 'project', {p}, {p}, 'system', 'r-bulk', \
           '{{}}'::jsonb FROM generate_series(1, {count}); \
         COMMIT",
        p = project.get()
    ))
    .await;
    for _ in 0..=(count / 1000) {
        sequence(&w.t).await;
    }
}

/// The table and index rows a plan read: what its scan nodes produced plus
/// what their filters removed, over every loop (EXPLAIN ANALYZE). Joins,
/// sorts and limits above them are not counted again.
fn touched(plan: &serde_json::Value) -> f64 {
    let loops = plan["Actual Loops"].as_f64().unwrap_or(1.0);
    let scan = plan["Node Type"]
        .as_str()
        .is_some_and(|t| t.contains("Scan") && t != "Function Scan");
    let own = if scan {
        (plan["Actual Rows"].as_f64().unwrap_or(0.0)
            + plan["Rows Removed by Filter"].as_f64().unwrap_or(0.0))
            * loops
    } else {
        0.0
    };
    own + plan["Plans"]
        .as_array()
        .map(|c| c.iter().map(touched).sum())
        .unwrap_or(0.0)
}

/// The rows a plan of `sql` touched, with `$1`..`$5` written in as
/// literals (EXPLAIN ANALYZE through a temporary function: no json decoding
/// is needed on this side).
async fn explain(
    w: &World,
    sql: &str,
    after: i64,
    bound: i64,
    visible: &VisibleSet,
    limit: Option<i64>,
) -> f64 {
    let projects = format!(
        "ARRAY[{}]::bigint[]",
        visible
            .project_ids()
            .iter()
            .map(i64::to_string)
            .collect::<Vec<_>>()
            .join(",")
    );
    let types = format!(
        "ARRAY[{}]::text[]",
        visible
            .event_type_names()
            .iter()
            .map(|t| format!("'{t}'"))
            .collect::<Vec<_>>()
            .join(",")
    );
    // The planner decides on statistics, as autovacuum keeps them in
    // production (a bulk insert right before would otherwise look empty).
    w.t.exec_superuser("ANALYZE forge.events").await;
    let literal = sql
        .replace("$5", &limit.unwrap_or(1).to_string())
        .replace("$4", &types)
        .replace("$3", &projects)
        .replace("$2", &bound.to_string())
        .replace("$1", &after.to_string());
    let mut su = w.t.superuser().await;
    sqlx::raw_sql(
        "CREATE FUNCTION pg_temp.plan_of(q text) RETURNS text LANGUAGE plpgsql AS $$ \
         DECLARE r json; BEGIN EXECUTE 'EXPLAIN (ANALYZE, FORMAT JSON) ' || q INTO r; \
         RETURN r::text; END $$",
    )
    .execute(&mut su)
    .await
    .expect("plan function");
    let text: String = sqlx::query_scalar("SELECT pg_temp.plan_of($1)")
        .bind(&literal)
        .fetch_one(&mut su)
        .await
        .expect("explain");
    let plan: serde_json::Value = serde_json::from_str(&text).expect("plan json");
    touched(&plan[0]["Plan"])
}

/// Review I1c (database MEDIUM): a page and the oldest-unread probe touch a
/// bounded number of rows however long the backlog is — 3 000 visible
/// events in each of two bound projects (the page must not sort the whole
/// visible backlog per chunk) and 5 000 of an unbound project in between.
#[tokio::test]
async fn the_page_reads_a_bounded_number_of_rows_over_a_large_backlog() {
    let w = world().await;
    let api = second_project(&w, "api").await;
    bind(&w, api.project_id, "kf-2").await;
    let hidden = second_project(&w, "hidden").await;
    drain(&w.t).await;
    let start = head(&w).await;
    bulk_events(&w, w.web.project_id, 3_000).await;
    bulk_events(&w, hidden.project_id, 5_000).await;
    bulk_events(&w, api.project_id, 3_000).await;
    let (mut snap, state, visible) = decided(&w).await;
    let page = snap
        .read_feed_page(&visible, start, state.high_seq, MAX_LIMIT, MAX_PAGE_BYTES)
        .await
        .expect("page");
    snap.close().await;
    assert_eq!(page.events.len(), MAX_LIMIT);
    assert!(page.has_more);
    let seqs: Vec<i64> = page.events.iter().map(|e| e.seq).collect();
    assert!(seqs.windows(2).all(|p| p[0] < p[1]));
    let read = explain(
        &w,
        PAGE_SEQS_SQL,
        start,
        state.high_seq,
        &visible,
        Some(501),
    )
    .await;
    // Two projects × (limit + 1), whatever the backlog (6 000 visible).
    assert!(read <= 1_100.0, "the page touched {read} rows");
    let oldest = explain(&w, OLDEST_SQL, start, state.high_seq, &visible, None).await;
    assert!(
        oldest <= 10.0,
        "the oldest-unread probe touched {oldest} rows"
    );
    w.t.drop_now().await;
}

/// Review I1c: the new tables' CHECKs refuse raw rows by constraint name.
#[tokio::test]
async fn feed_schema_checks_refuse_raw_rows_by_constraint_name() {
    let w = world().await;
    let mut su = w.t.superuser().await;
    for (sql, name) in [
        (
            "UPDATE forge.retention_policy SET events_days = 6".to_owned(),
            "retention_policy_events_days",
        ),
        (
            "INSERT INTO forge.retention_policy (singleton, events_days) VALUES (false, 90)"
                .to_owned(),
            "retention_policy_singleton",
        ),
        (
            format!(
                "INSERT INTO forge.integration_feed_reads (integration_id, last_read_at, \
                 cursor_seq, read_bound_seq) VALUES ({}, now(), -1, 0)",
                w.integration.get()
            ),
            "integration_feed_reads_cursor_seq",
        ),
        (
            format!(
                "INSERT INTO forge.integration_feed_reads (integration_id, last_read_at, \
                 cursor_seq, read_bound_seq) VALUES ({}, now(), 5, 4)",
                w.integration.get()
            ),
            "integration_feed_reads_bound",
        ),
    ] {
        let err = sqlx::query(&sql).execute(&mut su).await.expect_err(name);
        assert_eq!(constraint(&err), name, "{err}");
    }
    w.t.drop_now().await;
}

/// Review I1c: the definer functions check their arguments (22023), only
/// forge_maint executes `feed_new_epoch` (42501 for forge_app, the read
/// pool and forge_purge), the 7-day floor of `events_guard` holds for
/// forge_purge itself, and the token scope list knows
/// `user_employee_number`.
#[tokio::test]
async fn feed_functions_and_guards_refuse_what_they_must() {
    let w = world().await;
    let mut app = w.t.db.write_pool().acquire().await.expect("app");
    for sql in [
        "SELECT * FROM forge.events_retention(0, 'req-x')",
        "SELECT * FROM forge.events_retention(10001, 'req-x')",
        "SELECT * FROM forge.events_retention(10, '')",
    ] {
        let err = sqlx::query(sql).execute(&mut *app).await.expect_err(sql);
        assert_eq!(sqlstate(&err), "22023", "{sql}: {err}");
    }
    let mut su = w.t.superuser().await;
    for sql in [
        "SELECT * FROM forge.feed_new_epoch('restore', 'req-x')",
        "SELECT * FROM forge.feed_new_epoch('manual', '')",
    ] {
        let err = sqlx::query(sql).execute(&mut su).await.expect_err(sql);
        assert_eq!(sqlstate(&err), "22023", "{sql}: {err}");
    }
    for role in ["forge_app", "forge_ro", "forge_purge"] {
        let err = sqlx::raw_sql(&format!(
            "BEGIN; SET LOCAL ROLE {role}; \
             SELECT * FROM forge.feed_new_epoch('manual', 'req-x'); COMMIT"
        ))
        .execute(&mut su)
        .await
        .expect_err(role);
        assert_eq!(sqlstate(&err), "42501", "{role}: {err}");
        sqlx::raw_sql("ROLLBACK")
            .execute(&mut su)
            .await
            .expect("rollback");
    }
    // A 3-day-old published event is not forge_purge's to remove.
    w.t.exec_superuser(
        "SET CONSTRAINTS ALL IMMEDIATE; \
         ALTER TABLE forge.events DISABLE TRIGGER events_append_only; \
         UPDATE forge.events SET occurred_at = now() - interval '3 days' WHERE seq = 1; \
         ALTER TABLE forge.events ENABLE ALWAYS TRIGGER events_append_only",
    )
    .await;
    let err = sqlx::raw_sql(&format!(
        "BEGIN; SET LOCAL ROLE forge_purge; {} FROM forge.events WHERE seq = 1; COMMIT",
        "DELETE"
    ))
    .execute(&mut su)
    .await
    .expect_err("the 7-day floor");
    assert_eq!(sqlstate(&err), "XF001", "{err}");
    sqlx::raw_sql("ROLLBACK")
        .execute(&mut su)
        .await
        .expect("rollback");
    // The scope list: the new scope is stored, an unknown one refused.
    let token = |scope: &str| {
        format!(
            "INSERT INTO forge.tokens (user_id, public_id, name, secret_sha256, scopes, \
             expires_at) VALUES ({}, left(md5('{scope}'), 16), 'probe-{scope}', \
             decode(md5('{scope}'), 'hex') || decode(md5('{scope}x'), 'hex'), \
             ARRAY['read_api', '{scope}'], now() + interval '1 day')",
            w.sa.get()
        )
    };
    w.t.commit_audited(("user", w.sa.get()), &[token("user_employee_number")])
        .await;
    let mut tx = w.t.db.write_pool().begin().await.expect("begin");
    let err = sqlx::query(&token("user_shoe_size"))
        .execute(&mut *tx)
        .await
        .expect_err("unknown scope");
    assert_eq!(constraint(&err), "tokens_scopes_known", "{err}");
    drop(tx);
    w.t.drop_now().await;
}

/// Review I1c (LOW): when the 1.5 × limit removes events past a lagging
/// subscriber, the audit row names it and its position.
#[tokio::test]
async fn retention_records_the_subscribers_it_overrode() {
    let w = world().await;
    w.t.exec_superuser(
        "SET CONSTRAINTS ALL IMMEDIATE; \
         ALTER TABLE forge.events DISABLE TRIGGER events_append_only; \
         UPDATE forge.events SET occurred_at = now() - interval '140 days'; \
         ALTER TABLE forge.events ENABLE ALWAYS TRIGGER events_append_only; \
         INSERT INTO forge.event_subscribers (name, ordering, last_seq, updated_at) \
         VALUES ('probe-lagging', 'ordered', 1, now())",
    )
    .await;
    let batch = run_retention(
        &w.t.db,
        &RequestId::parse("req-override").expect("id"),
        RETENTION_BATCH,
    )
    .await
    .expect("retention");
    assert!(batch.deleted > 1);
    let details: Vec<serde_json::Value> =
        w.t.json_rows("SELECT details FROM forge.audit_log WHERE action = 'events.retention'")
            .await;
    assert_eq!(
        details[0]["details"]["overridden"],
        serde_json::json!([{"subscriber": "probe-lagging", "last_seq": 1}]),
        "{details:?}"
    );
    w.t.drop_now().await;
}

/// Review I1c (LOW): a snapshot dropped without `close` (a request cut by
/// its timeout) leaves no transaction open once its connection is reused.
#[tokio::test]
async fn a_dropped_snapshot_leaves_no_open_transaction() {
    let w = world().await;
    for _ in 0..3 {
        let mut snap = open(&w.t.db).await.expect("snapshot");
        snap.state().await.expect("state");
        drop(snap);
    }
    // Reuse the read pool: every connection's queued rollback runs first.
    for _ in 0..3 {
        let mut snap = open(&w.t.db).await.expect("snapshot again");
        snap.state().await.expect("state");
        snap.close().await;
    }
    let stuck: Vec<serde_json::Value> =
        w.t.json_rows(&format!(
            "SELECT pid FROM pg_stat_activity WHERE datname = '{}' \
             AND state = 'idle in transaction'",
            w.t.name
        ))
        .await;
    assert!(stuck.is_empty(), "{stuck:?}");
    w.t.drop_now().await;
}

// ── re-review of I1c (2026-11-04) ──────────────────────────────────────

/// `count` numbered events of `event_type` for `project` (one superuser
/// transaction with its audit row), then numbered.
async fn bulk_typed(w: &World, project: ProjectId, event_type: &str, count: i64) {
    w.t.exec_superuser(&format!(
        "BEGIN; \
         INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, resource_id, \
           request_id) VALUES ('system', 'probe.bulk', 'succeeded', 'project', {p}, 'r-bulk'); \
         INSERT INTO forge.events (event_id, event_type, schema_version, aggregate_type, \
           aggregate_id, project_id, actor_kind, request_id, payload) \
         SELECT gen_random_uuid(), '{event_type}', 1, 'project', {p}, {p}, 'system', 'r-bulk', \
           '{{}}'::jsonb FROM generate_series(1, {count}); \
         COMMIT",
        p = project.get()
    ))
    .await;
    for _ in 0..=(count / 1000) {
        sequence(&w.t).await;
    }
}

/// Re-review (database MEDIUM): types the feed does not show (3 000
/// maintainer-class `member.added` of a bound project) are not walked
/// before the one visible event — neither by the page nor by the
/// oldest-unread probe.
#[tokio::test]
async fn per_project_reads_skip_types_the_feed_does_not_show() {
    let w = world().await;
    let start = head(&w).await;
    bulk_typed(&w, w.web.project_id, "member.added", 3_000).await;
    bulk_typed(&w, w.web.project_id, "project.created", 1).await;
    let (mut snap, state, visible) = decided(&w).await;
    let page = snap
        .read_feed_page(&visible, start, state.high_seq, MAX_LIMIT, MAX_PAGE_BYTES)
        .await
        .expect("page");
    snap.close().await;
    assert_eq!(types(&page.events), ["project.created"]);
    let read = explain(
        &w,
        PAGE_SEQS_SQL,
        start,
        state.high_seq,
        &visible,
        Some(501),
    )
    .await;
    assert!(read <= 50.0, "the page touched {read} rows");
    let oldest = explain(&w, OLDEST_SQL, start, state.high_seq, &visible, None).await;
    assert!(
        oldest <= 50.0,
        "the oldest-unread probe touched {oldest} rows"
    );
    w.t.drop_now().await;
}

/// Re-review (database MEDIUM): 200 bound projects whose events interleave
/// in `seq` with a mix of shown and hidden types — paging returns exactly
/// the visible events of a full scan, strictly increasing, without gaps or
/// duplicates, and `has_more` is exact at `limit` and `limit + 1`.
#[tokio::test]
async fn two_hundred_bound_projects_page_exactly() {
    let w = world().await;
    let mut ids = vec![w.web.project_id.get()];
    for n in 1..200 {
        let p = second_project(&w, &format!("p{n}")).await;
        bind(&w, p.project_id, &format!("kf-{n}")).await;
        ids.push(p.project_id.get());
    }
    drain(&w.t).await;
    let start = head(&w).await;
    let list = ids.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
    // 600 events round-robin over the 200 projects: project.created and
    // repository.pushed shown, member.added hidden.
    w.t.exec_superuser(&format!(
        "BEGIN; \
         INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, resource_id, \
           request_id) VALUES ('system', 'probe.bulk', 'succeeded', 'project', {first}, 'r-mix'); \
         INSERT INTO forge.events (event_id, event_type, schema_version, aggregate_type, \
           aggregate_id, project_id, actor_kind, request_id, payload) \
         SELECT gen_random_uuid(), \
           (ARRAY['project.created', 'member.added', 'repository.pushed'])[1 + g % 3], 1, \
           'project', p, p, 'system', 'r-mix', '{{}}'::jsonb \
         FROM generate_series(0, 599) AS g, \
           LATERAL (SELECT (ARRAY[{list}])[1 + (g * 7) % 200] AS p) AS x; \
         COMMIT",
        first = ids[0]
    ))
    .await;
    sequence(&w.t).await;
    let (mut snap, state, visible) = decided(&w).await;
    assert_eq!(visible.project_ids().len(), 200);
    let expected: Vec<i64> = sqlx::query_scalar(
        "SELECT seq FROM forge.events WHERE seq > $1 AND seq <= $2 \
         AND project_id = ANY($3) AND event_type = ANY($4) ORDER BY seq",
    )
    .bind(start)
    .bind(state.high_seq)
    .bind(visible.project_ids())
    .bind(
        visible
            .event_type_names()
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<String>>(),
    )
    .fetch_all(&mut w.t.superuser().await)
    .await
    .expect("full scan");
    assert_eq!(expected.len(), 400);
    let mut got = Vec::new();
    let mut after = start;
    loop {
        let page = snap
            .read_feed_page(&visible, after, state.high_seq, 37, MAX_PAGE_BYTES)
            .await
            .expect("page");
        got.extend(page.events.iter().map(|e| e.seq));
        match page.events.last() {
            Some(last) if page.has_more => after = last.seq,
            _ => break,
        }
    }
    assert!(got.windows(2).all(|p| p[0] < p[1]), "strictly increasing");
    assert_eq!(got, expected, "no gap, no duplicate");
    let exact = snap
        .read_feed_page(&visible, start, state.high_seq, 400, MAX_PAGE_BYTES)
        .await
        .expect("exactly limit");
    assert_eq!((exact.events.len(), exact.has_more), (400, false));
    let one_less = snap
        .read_feed_page(&visible, start, state.high_seq, 399, MAX_PAGE_BYTES)
        .await
        .expect("limit + 1 exist");
    assert_eq!((one_less.events.len(), one_less.has_more), (399, true));
    snap.close().await;
    w.t.drop_now().await;
}

/// Re-review (database MEDIUM): a retention batch takes no lock on the feed
/// state row until its final UPDATE. While the row is held (as the
/// sequencer holds it), a batch has already removed its events — it holds
/// the row-exclusive lock on forge.events — and waits only at the end.
#[tokio::test]
async fn a_retention_batch_locks_the_feed_state_only_at_its_end() {
    let w = world().await;
    w.t.exec_superuser(
        "SET CONSTRAINTS ALL IMMEDIATE; \
         ALTER TABLE forge.events DISABLE TRIGGER events_append_only; \
         UPDATE forge.events SET occurred_at = now() - interval '100 days'; \
         ALTER TABLE forge.events ENABLE ALWAYS TRIGGER events_append_only",
    )
    .await;
    let mut holder = w.t.superuser().await;
    sqlx::raw_sql("BEGIN; SELECT 1 FROM forge.event_feed_state FOR UPDATE")
        .execute(&mut holder)
        .await
        .expect("hold the state row");
    let db = w.t.db.clone();
    let batch = tokio::spawn(async move {
        run_retention(
            &db,
            &RequestId::parse("req-lock-order").expect("id"),
            RETENTION_BATCH,
        )
        .await
    });
    w.t.until_waiting("events_retention").await;
    let deleting: Vec<serde_json::Value> =
        w.t.json_rows(
            "SELECT l.mode FROM pg_locks l JOIN pg_stat_activity a ON a.pid = l.pid \
             WHERE a.query LIKE '%events_retention%' AND a.pid <> pg_backend_pid() \
             AND l.relation = 'forge.events'::regclass AND l.mode = 'RowExclusiveLock' \
             AND l.granted",
        )
        .await;
    sqlx::raw_sql("COMMIT")
        .execute(&mut holder)
        .await
        .expect("release");
    let done = batch.await.expect("task").expect("retention");
    assert!(
        !deleting.is_empty(),
        "the batch waited for the state row before deleting anything"
    );
    assert!(done.deleted > 0);
    w.t.drop_now().await;
}

/// Re-review (LOW): a batch whose range holds no rows any more writes no
/// audit row (it still moves retained_from past the range).
#[tokio::test]
async fn an_empty_retention_batch_writes_no_audit_row() {
    let w = world().await;
    w.t.exec_superuser(&format!(
        "SET CONSTRAINTS ALL IMMEDIATE; \
         ALTER TABLE forge.events DISABLE TRIGGER events_append_only; \
         ALTER TABLE forge.event_work_items DISABLE TRIGGER ALL; \
         {} FROM forge.event_work_items; {} FROM forge.events; \
         ALTER TABLE forge.event_work_items ENABLE TRIGGER ALL; \
         ALTER TABLE forge.events ENABLE ALWAYS TRIGGER events_append_only",
        "DELETE", "DELETE"
    ))
    .await;
    let batch = run_retention(
        &w.t.db,
        &RequestId::parse("req-empty").expect("id"),
        RETENTION_BATCH,
    )
    .await
    .expect("retention");
    assert_eq!(batch.deleted, 0);
    let audits: Vec<serde_json::Value> =
        w.t.json_rows("SELECT id FROM forge.audit_log WHERE action = 'events.retention'")
            .await;
    assert!(audits.is_empty(), "{audits:?}");
    w.t.drop_now().await;
}

/// Re-review (LOW): the batched access read gives each bound project its
/// own facts — a Reporter, a Guest, a project the account left, and one
/// pending deletion.
#[tokio::test]
async fn the_batched_access_read_gives_each_binding_its_own_facts() {
    let w = world().await;
    let guest = second_project(&w, "guest").await;
    let left = second_project(&w, "left").await;
    let deleted = second_project(&w, "deleted").await;
    for (p, ext) in [(&guest, "kf-g"), (&left, "kf-l"), (&deleted, "kf-d")] {
        bind(&w, p.project_id, ext).await;
    }
    w.t.commit_audited(
        ("project", guest.project_id.get()),
        &[format!(
            "UPDATE forge.project_members SET role = 'guest' \
             WHERE project_id = {} AND user_id = {}",
            guest.project_id.get(),
            w.sa.get()
        )],
    )
    .await;
    w.t.commit_audited(
        ("project", left.project_id.get()),
        &[format!(
            "{} FROM forge.project_members WHERE project_id = {} AND user_id = {}",
            "DELETE",
            left.project_id.get(),
            w.sa.get()
        )],
    )
    .await;
    w.t.commit_audited(
        ("project", deleted.project_id.get()),
        &[format!(
            "UPDATE forge.projects SET deleted_at = now(), purge_after = now() + interval '30 days' WHERE id = {}",
            deleted.project_id.get()
        )],
    )
    .await;
    let mut snap = open(&w.t.db).await.expect("snapshot");
    let material = snap
        .material(w.integration)
        .await
        .expect("material")
        .expect("integration");
    snap.close().await;
    let facts = |p: ProjectId| {
        material
            .bindings
            .iter()
            .find(|b| b.project == p)
            .and_then(|b| b.access)
            .map(|a| (a.role.map(|r| r.as_str()), a.pending_deletion))
    };
    assert_eq!(facts(w.web.project_id), Some((Some("reporter"), false)));
    assert_eq!(facts(guest.project_id), Some((Some("guest"), false)));
    assert_eq!(facts(left.project_id), Some((None, false)));
    assert_eq!(facts(deleted.project_id), Some((Some("reporter"), true)));
    w.t.drop_now().await;
}
