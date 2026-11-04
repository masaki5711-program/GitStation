#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! The integration feed's cursors against the development database
//! (ADR-0011; M3 I1c, plan 02 §11.2, ADR-0016 §7): the four 410 reasons
//! (retention, epoch_mismatch, ahead_of_head, visible_set_changed — a
//! rebind of the same project included), another integration's cursor
//! (400), the same cursor answering the same events, the next cursor
//! advancing to the head when nothing is visible, the lag warning and the
//! personal facts behind their scopes. Bindings are written as fixtures
//! (the binding commands are forge-hosting's); events come from pushes.
//! Synthetic data only.

use std::io::Write;
use std::sync::{Arc, Mutex};

use forge_auth::{
    prove, AccountKind, Action, Authorized, Credential, IntegrationFeedCtx, Principal, Scope,
};
use forge_core::{
    CursorKeys, FeedPosition, IntegrationId, IntegrationKind, ObjectId, ProjectId, PushRecord,
    PushRecorded, PushRecorder, PushUpdateKind, PushedRef, Pusher, RefName, RequestId, UserId,
    UserKind,
};
use forge_db::change::{Actor, ChangeContext};
use forge_db::integrations::{self, NewIntegration};
use forge_db::projects::{self, CreatedProject, NewProject};
use forge_db::service_accounts::{self, NewServiceAccount};
use forge_db::subscribers::{self, Progress, Start};
use forge_db::test_support::*;
use forge_db::users::{self, NewUser};
use forge_db::work_items::WorkItemSync;
use forge_errors::{ErrorCode, ForgeError};
use forge_integ::{EventsPage, Feed, FeedQuery};

fn system(request: &str) -> ChangeContext {
    ChangeContext::new(Actor::System, RequestId::parse(request).expect("id"))
}

fn keys() -> Arc<CursorKeys> {
    Arc::new(CursorKeys::parse(&format!("5 {}\n", "3c".repeat(32))).expect("keys"))
}

struct World {
    t: FreshDb,
    alice: UserId,
    namespace: forge_core::NamespaceId,
    web: CreatedProject,
    sa: UserId,
    integration: IntegrationId,
    feed: Feed,
}

async fn account(t: &FreshDb, owner: UserId, name: &str) -> UserId {
    service_accounts::create(
        &t.db,
        &system(&format!("req-sa-{name}")),
        &NewServiceAccount {
            username: name.to_owned(),
            display_name: name.to_owned(),
            owner,
            purpose: "Kanflow reads the event feed".to_owned(),
        },
        owner,
    )
    .await
    .expect("service account")
    .user_id
}

async fn integration_for(t: &FreshDb, sa: UserId, by: UserId) -> IntegrationId {
    integrations::create(
        &t.db,
        &system(&format!("req-int-{}", sa.get())),
        &NewIntegration {
            kind: IntegrationKind::Kanflow,
            service_account: sa,
            event_types: None,
        },
        by,
    )
    .await
    .expect("integration")
}

/// alice's project `web` bound to kanflow project `kf-1`; work-item-sync
/// registered at the head and caught up.
async fn world() -> World {
    let t = FreshDb::migrated().await;
    let alice = users::create(
        &t.db,
        &system("req-user-alice"),
        &NewUser::human("alice", "Alice", "alice@example.test"),
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
    let sa = account(&t, alice.user_id, "kanflow-sync").await;
    let integration = integration_for(&t, sa, alice.user_id).await;
    let feed = Feed::new(t.db.clone(), keys());
    let w = World {
        t,
        alice: alice.user_id,
        namespace: alice.namespace_id,
        web,
        sa,
        integration,
        feed,
    };
    bind(&w, w.integration, w.sa, w.web.project_id, "kf-1").await;
    subscribers::register(&w.t.db, &WorkItemSync, Start::Head)
        .await
        .expect("register");
    drain(&w.t).await;
    w
}

/// A binding as the bind command writes it: the row and the service
/// account's Reporter membership, with the project's audit row.
async fn bind(
    w: &World,
    integration: IntegrationId,
    sa: UserId,
    project: ProjectId,
    external: &str,
) {
    w.t.commit_audited(
        ("project", project.get()),
        &[
            format!(
                "INSERT INTO forge.integration_bindings (integration_id, project_id, \
                 external_project_id, granted_membership, created_by) \
                 VALUES ({}, {}, '{external}', true, {})",
                integration.get(),
                project.get(),
                w.alice.get()
            ),
            format!(
                "INSERT INTO forge.project_members (project_id, user_id, role) \
                 VALUES ({}, {}, 'reporter') ON CONFLICT DO NOTHING",
                project.get(),
                sa.get()
            ),
        ],
    )
    .await;
}

async fn unbind(w: &World, project: ProjectId) {
    w.t.commit_audited(
        ("project", project.get()),
        &[
            format!(
                "DELETE FROM forge.integration_bindings WHERE project_id = {}",
                project.get()
            ),
            format!(
                "DELETE FROM forge.project_members WHERE project_id = {} AND user_id = {}",
                project.get(),
                w.sa.get()
            ),
        ],
    )
    .await;
}

async fn sequence(t: &FreshDb) {
    forge_db::sequencer::run_once(
        &t.db,
        forge_db::sequencer::BatchSize::MAX,
        forge_db::sequencer::AuditSequencing::Skip,
    )
    .await
    .expect("sequencer");
}

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

/// A push creating `branch` in `project` (one `repository.pushed`).
async fn push(w: &World, project: &CreatedProject, branch: &str) {
    let who = Pusher {
        request_id: RequestId::parse(&format!("req-push-{}", branch.replace('/', "-")))
            .expect("id"),
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
                updates: vec![PushedRef {
                    name: RefName::parse(&format!("refs/heads/{branch}")).expect("ref"),
                    old: ObjectId::parse(&"0".repeat(40)).expect("oid"),
                    new: ObjectId::parse(&"b".repeat(40)).expect("oid"),
                    kind: PushUpdateKind::Create,
                    commits: Vec::new(),
                    total_commits: 0,
                    total_commits_partial: false,
                }],
            },
        )
        .await
        .expect("push");
    assert_eq!(done, PushRecorded::Recorded);
}

fn token(sa: UserId, scopes: &[Scope]) -> Principal {
    Principal::authenticated(
        sa,
        false,
        true,
        Credential::Token(scopes.to_vec()),
        AccountKind::ServiceAccount,
    )
}

async fn proof_as(
    w: &World,
    integration: IntegrationId,
    who: &Principal,
    action: Action,
) -> Authorized<IntegrationFeedCtx> {
    let loaded = w
        .feed
        .target(integration)
        .await
        .expect("target")
        .expect("integration exists");
    prove(
        who,
        action,
        IntegrationFeedCtx::new(loaded, who.user(), false),
    )
    .expect("proof")
}

async fn head(w: &World) -> String {
    let proof = proof_as(
        w,
        w.integration,
        &token(w.sa, &[Scope::ReadApi]),
        Action::IntegrationEventCursorRead,
    )
    .await;
    w.feed.head_cursor(&proof).await.expect("head")
}

async fn read(w: &World, cursor: Option<&str>) -> Result<EventsPage, ForgeError> {
    read_as(w, &token(w.sa, &[Scope::ReadApi]), cursor).await
}

async fn read_as(
    w: &World,
    who: &Principal,
    cursor: Option<&str>,
) -> Result<EventsPage, ForgeError> {
    let proof = proof_as(w, w.integration, who, Action::IntegrationEventRead).await;
    w.feed
        .read_events(
            &proof,
            &RequestId::parse("req-feed-read").expect("id"),
            &FeedQuery {
                cursor: cursor.map(str::to_owned),
                limit: 100,
            },
        )
        .await
}

fn reason_of(err: &ForgeError) -> String {
    err.context()
        .iter()
        .find(|(k, _)| *k == "reason")
        .map(|(_, v)| v.clone())
        .unwrap_or_default()
}

fn assert_gone(result: Result<EventsPage, ForgeError>, reason: &str) {
    let err = result.expect_err("410");
    assert_eq!(err.code(), ErrorCode::FeedCursorExpired, "{err:?}");
    assert_eq!(err.code().http_status(), 410);
    assert_eq!(reason_of(&err), reason);
}

fn age_all_events(days: i64) -> String {
    format!(
        "SET CONSTRAINTS ALL IMMEDIATE; \
         ALTER TABLE forge.events DISABLE TRIGGER events_append_only; \
         UPDATE forge.events SET occurred_at = now() - interval '{days} days'; \
         ALTER TABLE forge.events ENABLE ALWAYS TRIGGER events_append_only"
    )
}

// ── the four 410 reasons ───────────────────────────────────────────────

/// 02 §8.2 / §11.2: a cursor older than what retention kept is 410
/// `retention` — the feed never continues silently after a gap.
#[tokio::test]
async fn expired_cursor_is_410_retention() {
    let w = world().await;
    let before = head(&w).await;
    push(&w, &w.web, "feature/PBI-1-a").await;
    push(&w, &w.web, "feature/PBI-2-b").await;
    drain(&w.t).await;
    assert_eq!(
        read(&w, Some(&before))
            .await
            .expect("readable")
            .events
            .len(),
        2
    );
    w.t.exec_superuser(&age_all_events(100)).await;
    let batch = forge_db::feed::run_retention(
        &w.t.db,
        &RequestId::parse("req-retention").expect("id"),
        forge_db::feed::RETENTION_BATCH,
    )
    .await
    .expect("retention");
    assert!(batch.deleted > 0);
    assert_gone(read(&w, Some(&before)).await, "retention");
    // The head is still a good position (recovery: take it, resync, read).
    let fresh = head(&w).await;
    assert!(read(&w, Some(&fresh))
        .await
        .expect("from the head")
        .events
        .is_empty());
    w.t.drop_now().await;
}

#[tokio::test]
async fn epoch_mismatch_is_410() {
    let w = world().await;
    let before = head(&w).await;
    let pw = PasswordFile::new(MAINT_PASSWORD);
    let (url, path) = (url_as("forge_maint", &w.t.name), pw.path());
    let cfg = w.t.app_config(&[
        ("FORGE_DATABASE_MAINT_URL", url.as_str()),
        ("FORGE_DATABASE_MAINT_PASSWORD_FILE", path.as_str()),
    ]);
    let mut maint = forge_db::Db::connect_maint(&cfg.db)
        .await
        .expect("forge_maint");
    forge_db::feed::new_epoch(&mut maint, &RequestId::parse("req-epoch").expect("id"))
        .await
        .expect("new epoch");
    maint.close().await;
    assert_gone(read(&w, Some(&before)).await, "epoch_mismatch");
    let fresh = head(&w).await;
    read(&w, Some(&fresh))
        .await
        .expect("the new epoch's cursor");
    w.t.drop_now().await;
}

/// A cursor past the last numbered event (the database went back to an
/// earlier point) is 410 `ahead_of_head`.
#[tokio::test]
async fn ahead_of_head_is_410() {
    let w = world().await;
    let at_head = head(&w).await;
    let k = keys();
    let position = FeedPosition::open(
        &k,
        w.integration,
        &at_head,
        std::time::Duration::from_secs(3600),
    )
    .expect("own cursor");
    let ahead = FeedPosition {
        seq: position.seq + 10,
        ..position
    }
    .seal(&k)
    .expect("seal");
    assert_gone(read(&w, Some(&ahead)).await, "ahead_of_head");
    w.t.drop_now().await;
}

/// Binding another project changes what the integration sees: the old
/// cursor is 410 `visible_set_changed` (the gap is never skipped).
#[tokio::test]
async fn visible_set_change_returns_410() {
    let w = world().await;
    let before = head(&w).await;
    let api = projects::create(
        &w.t.db,
        &system("req-project-api"),
        &NewProject::new("api", w.namespace, "API", w.alice),
    )
    .await
    .expect("api");
    bind(&w, w.integration, w.sa, api.project_id, "kf-2").await;
    drain(&w.t).await;
    assert_gone(read(&w, Some(&before)).await, "visible_set_changed");
    w.t.drop_now().await;
}

/// ADR-0016 Amendment I1c (the I1b dependency): unbinding and binding the
/// same project to the same kanflow project again is a new generation —
/// the cursor of the earlier binding is 410, never continued.
#[tokio::test]
async fn rebind_to_same_project_returns_410() {
    let w = world().await;
    let before = head(&w).await;
    unbind(&w, w.web.project_id).await;
    bind(&w, w.integration, w.sa, w.web.project_id, "kf-1").await;
    drain(&w.t).await;
    assert_gone(read(&w, Some(&before)).await, "visible_set_changed");
    w.t.drop_now().await;
}

// ── another integration's cursor, determinism, the head ────────────────

#[tokio::test]
async fn other_integrations_cursor_is_400_int_0008() {
    let w = world().await;
    let other_sa = account(&w.t, w.alice, "kanflow-other").await;
    let other = integration_for(&w.t, other_sa, w.alice).await;
    let mine = head(&w).await;
    let proof = proof_as(
        &w,
        other,
        &token(other_sa, &[Scope::ReadApi]),
        Action::IntegrationEventRead,
    )
    .await;
    let err = w
        .feed
        .read_events(
            &proof,
            &RequestId::parse("req-other").expect("id"),
            &FeedQuery {
                cursor: Some(mine.clone()),
                limit: 10,
            },
        )
        .await
        .expect_err("another integration's cursor");
    assert_eq!(err.code(), ErrorCode::FeedCursorInvalid);
    assert_eq!(err.code().http_status(), 400);
    assert!(err
        .context()
        .iter()
        .any(|(k, v)| *k == "rule" && v == "other_integration"));
    assert!(err.context().iter().all(|(_, v)| !v.contains(&mine)));
    w.t.drop_now().await;
}

#[tokio::test]
async fn same_cursor_same_events() {
    let w = world().await;
    let before = head(&w).await;
    for n in 1..=3 {
        push(&w, &w.web, &format!("feature/PBI-{n}-x")).await;
    }
    drain(&w.t).await;
    let ids = |p: &EventsPage| p.events.iter().map(|e| e.id.clone()).collect::<Vec<_>>();
    let first = read(&w, Some(&before)).await.expect("first");
    let again = read(&w, Some(&before)).await.expect("again");
    assert_eq!(ids(&first).len(), 3);
    assert_eq!(ids(&first), ids(&again));
    assert_eq!(first.has_more, again.has_more);
    let refs: Vec<String> = first
        .events
        .iter()
        .flat_map(|e| e.work_items.iter().map(|i| i.reference.clone()))
        .collect();
    assert_eq!(refs, ["PBI-1", "PBI-2", "PBI-3"]);
    // Resuming from the second event's own cursor returns the third.
    let rest = read(&w, Some(&first.events[1].cursor)).await.expect("rest");
    assert_eq!(ids(&rest), [ids(&first)[2].clone()]);
    w.t.drop_now().await;
}

/// With nothing visible left the next cursor is the head (it advances past
/// invisible events, sealed: the consumer learns nothing about them).
#[tokio::test]
async fn next_cursor_advances_to_head_when_nothing_visible() {
    let w = world().await;
    let before = head(&w).await;
    let hidden = projects::create(
        &w.t.db,
        &system("req-project-hidden"),
        &NewProject::new("hidden", w.namespace, "Hidden", w.alice),
    )
    .await
    .expect("hidden");
    push(&w, &hidden, "feature/PBI-4-hidden").await;
    drain(&w.t).await;
    let page = read(&w, Some(&before)).await.expect("page");
    assert!(page.events.is_empty());
    assert!(!page.has_more);
    let k = keys();
    let age = std::time::Duration::from_secs(3600);
    let next = FeedPosition::open(&k, w.integration, &page.next_cursor, age).expect("next");
    let at_head = FeedPosition::open(&k, w.integration, &head(&w).await, age).expect("head");
    let started = FeedPosition::open(&k, w.integration, &before, age).expect("before");
    assert!(next.seq > started.seq, "advanced past the invisible events");
    assert_eq!(next.seq, at_head.seq);
    assert!(read(&w, Some(&page.next_cursor))
        .await
        .expect("again")
        .events
        .is_empty());
    w.t.drop_now().await;
}

// ── the lag warning ────────────────────────────────────────────────────

#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

thread_local! {
    static SINK: std::cell::RefCell<Option<Captured>> = const { std::cell::RefCell::new(None) };
}

struct ThreadSink;

impl Write for ThreadSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        SINK.with(|s| {
            if let Some(c) = s.borrow().as_ref() {
                c.0.lock().expect("log buffer").extend_from_slice(buf);
            }
        });
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct CaptureGuard;

impl Drop for CaptureGuard {
    fn drop(&mut self) {
        SINK.with(|s| s.borrow_mut().take());
    }
}

impl Captured {
    fn install(&self) -> CaptureGuard {
        static GLOBAL: std::sync::Once = std::sync::Once::new();
        GLOBAL.call_once(|| {
            let subscriber = tracing_subscriber::fmt()
                .json()
                .flatten_event(true)
                .with_max_level(tracing::Level::DEBUG)
                .with_writer(|| ThreadSink)
                .finish();
            tracing::subscriber::set_global_default(subscriber).expect("one global subscriber");
        });
        SINK.with(|s| *s.borrow_mut() = Some(self.clone()));
        CaptureGuard
    }

    fn lines(&self) -> Vec<serde_json::Value> {
        let bytes = self.0.lock().expect("log buffer").clone();
        String::from_utf8_lossy(&bytes)
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }
}

/// A read that finds the oldest unread event older than half the retention
/// period logs FG-INT-0009 (warn, `reason=behind`) with the request id —
/// once per minute per integration (with the read record).
#[tokio::test]
async fn lagging_integration_logs_int_0009() {
    let w = world().await;
    let before = head(&w).await;
    for n in 1..=3 {
        push(&w, &w.web, &format!("feature/PBI-{n}-lag")).await;
    }
    drain(&w.t).await;
    w.t.exec_superuser(&age_all_events(50)).await;
    let logs = Captured::default();
    let guard = logs.install();
    let proof = proof_as(
        &w,
        w.integration,
        &token(w.sa, &[Scope::ReadApi]),
        Action::IntegrationEventRead,
    )
    .await;
    let page = w
        .feed
        .read_events(
            &proof,
            &RequestId::parse("req-lagging").expect("id"),
            &FeedQuery {
                cursor: Some(before.clone()),
                limit: 1,
            },
        )
        .await
        .expect("page");
    assert!(page.has_more);
    let second = w
        .feed
        .read_events(
            &proof,
            &RequestId::parse("req-lagging-2").expect("id"),
            &FeedQuery {
                cursor: Some(page.next_cursor.clone()),
                limit: 1,
            },
        )
        .await
        .expect("second page");
    assert_eq!(second.events.len(), 1);
    drop(guard);
    let warned: Vec<serde_json::Value> = logs
        .lines()
        .into_iter()
        .filter(|l| l["error_code"] == "FG-INT-0009")
        .collect();
    assert_eq!(warned.len(), 1, "once per minute: {warned:?}");
    assert_eq!(warned[0]["level"], "WARN");
    assert_eq!(warned[0]["request_id"], "req-lagging");
    let context = warned[0]["context"].as_str().unwrap_or_default();
    assert!(context.contains("reason=behind"), "{context}");
    assert!(context.contains(&format!("integration_id={}", w.integration.get())));
    let raw = String::from_utf8_lossy(&logs.0.lock().expect("buffer")).into_owned();
    assert!(!raw.contains(&before), "a cursor never reaches the log");
    w.t.drop_now().await;
}

// ── the actor's personal facts ─────────────────────────────────────────

/// `actor.email` only with `user_email`, `actor.employee_number` only with
/// `user_employee_number` (plan 11 Q7); neither without.
#[tokio::test]
async fn personal_facts_need_their_scopes() {
    let w = world().await;
    w.t.commit_audited(
        ("user", w.alice.get()),
        &[format!(
            "UPDATE forge.users SET employee_number = 'E-1001' WHERE id = {}",
            w.alice.get()
        )],
    )
    .await;
    let before = head(&w).await;
    push(&w, &w.web, "feature/PBI-6-facts").await;
    drain(&w.t).await;
    let actor = |page: EventsPage| page.events[0].actor.user.clone().expect("the pusher");
    let plain = actor(read(&w, Some(&before)).await.expect("plain"));
    assert_eq!(plain.username.as_deref(), Some("alice"));
    assert_eq!((plain.email, plain.employee_number), (None, None));
    let mail = actor(
        read_as(
            &w,
            &token(w.sa, &[Scope::ReadApi, Scope::UserEmail]),
            Some(&before),
        )
        .await
        .expect("email"),
    );
    assert_eq!(mail.email.as_deref(), Some("alice@example.test"));
    assert_eq!(mail.employee_number, None);
    let number = actor(
        read_as(
            &w,
            &token(w.sa, &[Scope::ReadApi, Scope::UserEmployeeNumber]),
            Some(&before),
        )
        .await
        .expect("employee number"),
    );
    assert_eq!(number.email, None);
    assert_eq!(number.employee_number.as_deref(), Some("E-1001"));
    w.t.drop_now().await;
}

// ── review of I1c: one read in flight per integration ──────────────────

/// Review I1c (security MEDIUM): a second read of the same integration
/// while one is in flight is refused at once with 429 FG-WEB-0010
/// (`reason=feed_read_in_flight`, `Retry-After` 1 s); the first completes,
/// and the slot is free again afterwards.
#[tokio::test]
async fn a_second_concurrent_read_of_the_same_integration_is_429() {
    let w = world().await;
    let before = head(&w).await;
    let who = token(w.sa, &[Scope::ReadApi]);
    let one = proof_as(&w, w.integration, &who, Action::IntegrationEventRead).await;
    let two = proof_as(&w, w.integration, &who, Action::IntegrationEventRead).await;
    let query = FeedQuery {
        cursor: Some(before.clone()),
        limit: 10,
    };
    let request = RequestId::parse("req-concurrent").expect("id");
    // The first enters its slot before its first database round trip; the
    // second is polled while the first waits on it.
    let (first, second) = tokio::join!(
        w.feed.read_events(&one, &request, &query),
        w.feed.read_events(&two, &request, &query)
    );
    first.expect("the first read completes");
    let err = second.expect_err("one in flight per integration");
    assert_eq!(err.code(), ErrorCode::RateLimited);
    assert_eq!(err.code().http_status(), 429);
    assert_eq!(reason_of(&err), "feed_read_in_flight");
    assert!(err
        .context()
        .iter()
        .any(|(k, v)| *k == "retry_after_s" && v == "1"));
    read(&w, Some(&before)).await.expect("free again");
    w.t.drop_now().await;
}
