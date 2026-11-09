#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! In-app notifications against a migrated database (M3 U4, ADR-0016
//! Amendment U4): the notify subscriber records one item for the same
//! recipient as the mail (the merge request's author, never the actor),
//! none when the person switched it off, also while mail is off; the list
//! decides visibility again on every read, pages by id with an unread
//! filter, marks only the caller's own items read, and the retention keeps
//! the table bounded. Synthetic data only.

use forge_auth::{prove, Action, Credential, NotificationPreferencesCtx, Principal};
use forge_core::config::NotifyConfig;
use forge_core::ids::{ProjectId, UserId};
use forge_core::{NotificationKind, RequestId, UserKind};
use forge_db::change::{Actor, ChangeContext};
use forge_db::notify::in_app::{self, PageQuery};
use forge_db::notify::{self, Preferences};
use forge_db::projects::{self, CreatedProject, NewProject};
use forge_db::subscribers::{self, Progress, Start};
use forge_db::test_support::FreshDb;
use forge_db::users::{self, NewUser};
use forge_errors::ErrorCode;
use forge_notify::{Inbox, Notify};

fn system(request: &str) -> ChangeContext {
    ChangeContext::new(
        Actor::System,
        RequestId::parse(request).expect("request id"),
    )
}

struct World {
    t: FreshDb,
    alice: UserId,
    bob: UserId,
    carol: UserId,
    web: CreatedProject,
    notify: Notify,
    inbox: Inbox,
}

async fn world() -> World {
    let t = FreshDb::migrated().await;
    let human =
        |name: &str, display: &str| NewUser::human(name, display, &format!("{name}@example.test"));
    let alice = users::create(&t.db, &system("req-alice"), &human("alice", "Alice"))
        .await
        .expect("alice");
    let bob = users::create(&t.db, &system("req-bob"), &human("bob", "Bob"))
        .await
        .expect("bob");
    let carol = users::create(&t.db, &system("req-carol"), &human("carol", "Carol"))
        .await
        .expect("carol");
    let web = projects::create(
        &t.db,
        &system("req-web"),
        &NewProject::new("web", alice.namespace_id, "Web", alice.user_id),
    )
    .await
    .expect("web");
    // Mail is not configured: the paused subscriber still records in-app
    // items (they need no relay).
    let notify = Notify::paused(&t.db).expect("notify");
    subscribers::register(&t.db, &notify, Start::Head)
        .await
        .expect("register");
    let inbox = Inbox::new(t.db.clone());
    World {
        t,
        alice: alice.user_id,
        bob: bob.user_id,
        carol: carol.user_id,
        web,
        notify,
        inbox,
    }
}

impl World {
    fn project(&self) -> ProjectId {
        self.web.project_id
    }

    async fn open_mr(&self, author: UserId, title: &str, branch: &str) -> i64 {
        let p = self.project().get();
        let title = title.replace('\'', "''");
        let rows = self
            .t
            .json_rows(&format!(
                "SELECT count(*) AS n FROM forge.merge_requests WHERE project_id = {p}"
            ))
            .await;
        let iid = rows[0]["n"].as_i64().expect("count") + 1;
        let counter = if iid == 1 {
            format!(
                "INSERT INTO forge.merge_request_counters (project_id, last_iid) VALUES ({p}, 1)"
            )
        } else {
            format!(
                "UPDATE forge.merge_request_counters SET last_iid = {iid} WHERE project_id = {p}"
            )
        };
        self.t
            .commit_audited(
                ("project", p),
                &[
                    counter,
                    format!(
                        "INSERT INTO forge.merge_requests (project_id, iid, title, author_id, \
                         source_branch, target_branch) VALUES ({p}, {iid}, '{title}', {}, \
                         'refs/heads/{branch}', 'refs/heads/main')",
                        author.get()
                    ),
                ],
            )
            .await;
        iid
    }

    async fn approved(&self, iid: i64, by: UserId, n: u32) {
        let payload =
            serde_json::json!({"merge_request_id": 1, "iid": iid, "approver_id": by.get()});
        self.t
            .exec_superuser(&format!(
                "INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, \
                 request_id) VALUES ('system', 'merge_request.probe', 'succeeded', \
                 'merge_request', 'req-u4-{n}'); \
                 INSERT INTO forge.events (event_id, event_type, schema_version, aggregate_type, \
                 aggregate_id, project_id, actor_kind, actor_user_id, request_id, payload) \
                 VALUES (gen_random_uuid(), 'merge_request.approved', 1, 'merge_request', 1, {}, \
                 'user', {}, 'req-u4-{n}', '{payload}'::jsonb)",
                self.project().get(),
                by.get()
            ))
            .await;
    }

    async fn drain(&self) {
        forge_db::sequencer::run_once(
            &self.t.db,
            forge_db::sequencer::BatchSize::MAX,
            forge_db::sequencer::AuditSequencing::Skip,
        )
        .await
        .expect("sequencer");
        loop {
            match subscribers::run_once(&self.t.db, &self.notify)
                .await
                .expect("step")
            {
                Progress::Handled { .. } => {}
                Progress::CaughtUp => return,
                other @ (Progress::Busy
                | Progress::Disabled
                | Progress::Failed(_)
                | Progress::Stuck(_)
                | Progress::Quarantined(_)) => panic!("unexpected {other:?}"),
            }
        }
    }

    /// (user, kind, read) of every in-app item, oldest first.
    async fn items(&self) -> Vec<(i64, String, bool)> {
        sqlx::query_as(
            "SELECT user_id, kind, read_at IS NOT NULL FROM forge.in_app_notifications ORDER BY id",
        )
        .fetch_all(&mut self.t.superuser().await)
        .await
        .expect("items")
    }

    async fn mail_rows(&self) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM forge.notifications")
            .fetch_one(&mut self.t.superuser().await)
            .await
            .expect("mail rows")
    }

    async fn set_prefs(&self, user: UserId, prefs: Preferences) {
        notify::set_preferences(&self.t.db, user, &prefs)
            .await
            .expect("prefs");
    }

    async fn add_member(&self, user: UserId, role: &str) {
        self.t
            .commit_audited(
                ("project", self.project().get()),
                &[format!(
                    "INSERT INTO forge.project_members (project_id, user_id, role) \
                     VALUES ({}, {}, '{role}')",
                    self.project().get(),
                    user.get()
                )],
            )
            .await;
    }

    async fn remove_member(&self, user: UserId) {
        self.t
            .commit_audited(
                ("project", self.project().get()),
                &[format!(
                    "DELETE FROM forge.project_members WHERE project_id = {} AND user_id = {}",
                    self.project().get(),
                    user.get()
                )],
            )
            .await;
    }
}

fn person(user: UserId) -> Principal {
    Principal::authenticated(user, false, true, Credential::Session, UserKind::Human)
}

fn read_proof(user: UserId) -> forge_auth::Authorized<NotificationPreferencesCtx> {
    prove(
        &person(user),
        Action::NotificationInboxRead,
        NotificationPreferencesCtx::new(user),
    )
    .expect("own inbox")
}

fn update_proof(user: UserId) -> forge_auth::Authorized<NotificationPreferencesCtx> {
    prove(
        &person(user),
        Action::NotificationInboxUpdate,
        NotificationPreferencesCtx::new(user),
    )
    .expect("own inbox")
}

fn all(limit: i64) -> PageQuery {
    PageQuery {
        before: None,
        unread_only: false,
        limit,
    }
}

// ── recording ─────────────────────────────────────────────────────────

#[tokio::test]
async fn an_event_gives_the_author_one_item_and_the_actor_none() {
    let w = world().await;
    let iid = w.open_mr(w.alice, "Fix rounding", "fix-rounding").await;
    w.approved(iid, w.bob, 1).await;
    // Alice approving her own merge request tells nobody.
    w.approved(iid, w.alice, 2).await;
    w.drain().await;
    assert_eq!(
        w.items().await,
        [(w.alice.get(), "mr_approved".to_owned(), false)]
    );
    // Mail is off: no mail row at all.
    assert_eq!(w.mail_rows().await, 0);
    // A repeated step adds nothing.
    w.drain().await;
    assert_eq!(w.items().await.len(), 1);
    // The list shows it with what Alice may see.
    let page = w
        .inbox
        .list(&read_proof(w.alice), all(20))
        .await
        .expect("list");
    assert_eq!(page.items.len(), 1);
    let listed = &page.items[0];
    assert_eq!(listed.item.kind, NotificationKind::MergeRequestApproved);
    assert_eq!(listed.item.iid, iid);
    assert_eq!(listed.item.actor, Some(w.bob));
    assert_eq!(listed.item.actor_name.as_deref(), Some("Bob"));
    assert_eq!(listed.view.title, "Fix rounding");
    assert_eq!(listed.view.project_path, "alice/web");
    assert_eq!(page.next_before, None);
    // Bob has nothing.
    let bob = w
        .inbox
        .list(&read_proof(w.bob), all(20))
        .await
        .expect("bob");
    assert!(bob.items.is_empty());
    w.t.drop_now().await;
}

#[tokio::test]
async fn no_item_when_the_person_switched_in_app_off_or_muted_the_kind() {
    let w = world().await;
    let iid = w.open_mr(w.alice, "Fix rounding", "fix-rounding").await;
    w.set_prefs(
        w.alice,
        Preferences {
            in_app_enabled: false,
            ..Preferences::default()
        },
    )
    .await;
    w.approved(iid, w.bob, 1).await;
    w.drain().await;
    assert!(w.items().await.is_empty(), "in-app off");
    w.set_prefs(
        w.alice,
        Preferences {
            in_app_muted: vec![NotificationKind::MergeRequestApproved],
            // Mail settings do not apply to the app.
            email_enabled: false,
            ..Preferences::default()
        },
    )
    .await;
    w.approved(iid, w.carol, 2).await;
    w.drain().await;
    assert!(w.items().await.is_empty(), "kind muted in the app");
    w.set_prefs(
        w.alice,
        Preferences {
            email_enabled: false,
            muted: vec![NotificationKind::MergeRequestApproved],
            ..Preferences::default()
        },
    )
    .await;
    w.approved(iid, w.bob, 3).await;
    w.drain().await;
    assert_eq!(w.items().await.len(), 1, "mail off, the app on");
    w.t.drop_now().await;
}

#[tokio::test]
async fn mail_on_records_the_mail_and_the_item_for_the_same_person() {
    let w = world().await;
    let notify = Notify::new(&w.t.db, &NotifyConfig::default()).expect("notify");
    let iid = w.open_mr(w.alice, "Fix rounding", "fix-rounding").await;
    w.approved(iid, w.bob, 1).await;
    forge_db::sequencer::run_once(
        &w.t.db,
        forge_db::sequencer::BatchSize::MAX,
        forge_db::sequencer::AuditSequencing::Skip,
    )
    .await
    .expect("sequencer");
    while matches!(
        subscribers::run_once(&w.t.db, &notify).await.expect("step"),
        Progress::Handled { .. }
    ) {}
    assert_eq!(w.mail_rows().await, 1);
    assert_eq!(
        w.items().await,
        [(w.alice.get(), "mr_approved".to_owned(), false)]
    );
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_disabled_person_and_one_who_cannot_see_the_project_get_no_item() {
    let w = world().await;
    // Carol's merge request in Alice's private project, Carol not (yet) a
    // member: she could never see it.
    let iid = w.open_mr(w.carol, "Carol's change", "carol-change").await;
    w.approved(iid, w.bob, 1).await;
    w.drain().await;
    assert!(w.items().await.is_empty(), "not visible: no item");
    // Disabled.
    let iid2 = w.open_mr(w.alice, "Alice's change", "alice-change").await;
    w.t.commit_audited(
        ("user", w.alice.get()),
        &[format!(
            "UPDATE forge.users SET state = 'disabled' WHERE id = {}",
            w.alice.get()
        )],
    )
    .await;
    w.approved(iid2, w.bob, 2).await;
    w.drain().await;
    assert!(w.items().await.is_empty(), "disabled: no item");
    w.t.drop_now().await;
}

// ── reading ───────────────────────────────────────────────────────────

#[tokio::test]
async fn an_item_is_hidden_once_its_person_lost_access_and_shown_when_it_returns() {
    let w = world().await;
    w.add_member(w.carol, "developer").await;
    let iid = w.open_mr(w.carol, "Carol's change", "carol-change").await;
    w.approved(iid, w.bob, 1).await;
    w.drain().await;
    assert_eq!(w.items().await.len(), 1);
    let count = w
        .inbox
        .unread_count(&read_proof(w.carol))
        .await
        .expect("count");
    assert_eq!((count.count, count.capped), (1, false));

    w.remove_member(w.carol).await;
    let page = w
        .inbox
        .list(&read_proof(w.carol), all(20))
        .await
        .expect("list");
    assert!(page.items.is_empty(), "{page:?}");
    let count = w
        .inbox
        .unread_count(&read_proof(w.carol))
        .await
        .expect("count");
    assert_eq!(count.count, 0);
    // The row is kept (access may come back) and is shown again then.
    assert_eq!(w.items().await.len(), 1);
    w.add_member(w.carol, "reporter").await;
    let page = w
        .inbox
        .list(&read_proof(w.carol), all(20))
        .await
        .expect("list");
    assert_eq!(page.items.len(), 1);
    // A merge request that no longer exists is not listed either.
    w.t.exec_superuser(&format!(
        "ALTER TABLE forge.merge_requests DISABLE TRIGGER ALL; \
         DELETE FROM forge.merge_requests WHERE project_id = {} AND iid = {iid}; \
         ALTER TABLE forge.merge_requests ENABLE TRIGGER ALL",
        w.project().get()
    ))
    .await;
    let page = w
        .inbox
        .list(&read_proof(w.carol), all(20))
        .await
        .expect("list");
    assert!(page.items.is_empty());
    w.t.drop_now().await;
}

#[tokio::test]
async fn the_list_pages_newest_first_with_an_unread_filter() {
    let w = world().await;
    let iid = w.open_mr(w.alice, "Fix rounding", "fix-rounding").await;
    for n in 1..=3 {
        w.approved(iid, w.bob, n).await;
    }
    w.drain().await;
    let first = w
        .inbox
        .list(&read_proof(w.alice), all(2))
        .await
        .expect("first");
    assert_eq!(first.items.len(), 2);
    assert!(
        first.items[0].item.id > first.items[1].item.id,
        "newest first"
    );
    let before = first.next_before.expect("more");
    assert_eq!(before, first.items[1].item.id);
    let second = w
        .inbox
        .list(
            &read_proof(w.alice),
            PageQuery {
                before: Some(before),
                unread_only: false,
                limit: 2,
            },
        )
        .await
        .expect("second");
    assert_eq!(second.items.len(), 1);
    assert_eq!(second.next_before, None);
    let oldest = second.items[0].item.id;

    // Mark the oldest read: it stays in the list, not in the unread one.
    let marked = w
        .inbox
        .mark_read(&update_proof(w.alice), oldest)
        .await
        .expect("mark");
    assert!(marked.read_at.is_some());
    let unread = w
        .inbox
        .list(
            &read_proof(w.alice),
            PageQuery {
                before: None,
                unread_only: true,
                limit: 20,
            },
        )
        .await
        .expect("unread");
    assert_eq!(unread.items.len(), 2);
    assert!(unread.items.iter().all(|l| l.item.id != oldest));
    assert_eq!(
        w.inbox
            .unread_count(&read_proof(w.alice))
            .await
            .expect("count")
            .count,
        2
    );
    // Marking again keeps the first time.
    let again = w
        .inbox
        .mark_read(&update_proof(w.alice), oldest)
        .await
        .expect("again");
    assert_eq!(again.read_at, marked.read_at);

    // Read all up to the newest Alice saw: one that arrives later stays unread.
    let newest = first.items[0].item.id;
    w.approved(iid, w.carol, 4).await;
    w.drain().await;
    let n = w
        .inbox
        .mark_all_read(&update_proof(w.alice), Some(newest))
        .await
        .expect("all");
    assert_eq!(n, 2);
    assert_eq!(
        w.inbox
            .unread_count(&read_proof(w.alice))
            .await
            .expect("count")
            .count,
        1
    );
    assert_eq!(
        w.inbox
            .mark_all_read(&update_proof(w.alice), None)
            .await
            .expect("rest"),
        1
    );
    w.t.drop_now().await;
}

#[tokio::test]
async fn only_the_callers_own_items_are_marked_read() {
    let w = world().await;
    w.add_member(w.carol, "developer").await;
    let mine = w.open_mr(w.alice, "Alice's", "alice").await;
    let theirs = w.open_mr(w.carol, "Carol's", "carol").await;
    w.approved(mine, w.bob, 1).await;
    w.approved(theirs, w.bob, 2).await;
    w.drain().await;
    let carol_item = w
        .inbox
        .list(&read_proof(w.carol), all(20))
        .await
        .expect("carol")
        .items[0]
        .item
        .id;
    // Alice's proof on Carol's item: not found, and nothing changes.
    let err = w
        .inbox
        .mark_read(&update_proof(w.alice), carol_item)
        .await
        .expect_err("foreign");
    assert_eq!(err.code(), ErrorCode::NotFound);
    let err = w
        .inbox
        .mark_read(&update_proof(w.alice), 999_999)
        .await
        .expect_err("absent");
    assert_eq!(err.code(), ErrorCode::NotFound);
    // Alice's read-all leaves Carol's alone.
    w.inbox
        .mark_all_read(&update_proof(w.alice), None)
        .await
        .expect("all");
    assert_eq!(
        w.inbox
            .unread_count(&read_proof(w.carol))
            .await
            .expect("count")
            .count,
        1
    );
    // A read proof does not mark (the commands check the proof's action).
    let err = w
        .inbox
        .mark_all_read(&read_proof(w.alice), None)
        .await
        .expect_err("wrong proof");
    assert_eq!(err.code(), ErrorCode::Internal);
    // The owner lookup the route's loader uses.
    assert_eq!(
        in_app::owner(&w.t.db, carol_item).await.expect("owner"),
        Some(w.carol)
    );
    assert_eq!(in_app::owner(&w.t.db, 999_999).await.expect("none"), None);
    w.t.drop_now().await;
}

// ── retention ─────────────────────────────────────────────────────────

#[tokio::test]
async fn the_retention_deletes_old_read_items_and_very_old_unread_ones() {
    let w = world().await;
    let iid = w.open_mr(w.alice, "Fix rounding", "fix-rounding").await;
    for n in 1..=4 {
        w.approved(iid, w.bob, n).await;
    }
    w.drain().await;
    let ids: Vec<i64> = sqlx::query_scalar("SELECT id FROM forge.in_app_notifications ORDER BY id")
        .fetch_all(&mut w.t.superuser().await)
        .await
        .expect("ids");
    // 1: read 31 days ago (goes); 2: read 29 days ago (stays);
    // 3: unread, 181 days old (goes); 4: unread, new (stays).
    w.t.exec_superuser(&format!(
        "UPDATE forge.in_app_notifications SET created_at = now() - interval '40 days', \
           read_at = now() - interval '31 days' WHERE id = {}; \
         UPDATE forge.in_app_notifications SET created_at = now() - interval '40 days', \
           read_at = now() - interval '29 days' WHERE id = {}; \
         UPDATE forge.in_app_notifications SET created_at = now() - interval '181 days' \
           WHERE id = {}",
        ids[0], ids[1], ids[2]
    ))
    .await;
    assert_eq!(in_app::expire(&w.t.db, 100).await.expect("expire"), 2);
    let left: Vec<i64> =
        sqlx::query_scalar("SELECT id FROM forge.in_app_notifications ORDER BY id")
            .fetch_all(&mut w.t.superuser().await)
            .await
            .expect("left");
    assert_eq!(left, [ids[1], ids[3]]);
    assert_eq!(in_app::expire(&w.t.db, 100).await.expect("again"), 0);
    w.t.drop_now().await;
}

#[tokio::test]
async fn the_schema_refuses_unknown_kinds_and_a_read_before_creation() {
    let w = world().await;
    let mut su = w.t.superuser().await;
    let uid = w.alice.get();
    for (sql, constraint) in [
        (
            format!(
                "INSERT INTO forge.in_app_notifications (user_id, kind, event_id, event_seq, \
                 project_id, merge_request_iid) VALUES ({uid}, 'mr_unknown', gen_random_uuid(), \
                 1, 1, 1)"
            ),
            "in_app_notifications_kind",
        ),
        (
            format!(
                "INSERT INTO forge.in_app_notifications (user_id, kind, event_id, event_seq, \
                 project_id, merge_request_iid, read_at) VALUES ({uid}, 'mr_merged', \
                 gen_random_uuid(), 1, 1, 1, now() - interval '1 day')"
            ),
            "in_app_notifications_read_after_created",
        ),
        (
            format!(
                "INSERT INTO forge.notification_preferences (user_id, in_app_muted_kinds) \
                 VALUES ({uid}, ARRAY['mr_unknown'])"
            ),
            "notification_preferences_in_app_muted_kinds",
        ),
    ] {
        let err = sqlx::query(&sql)
            .execute(&mut su)
            .await
            .expect_err("refused");
        let text = err.to_string();
        let db = err.as_database_error().expect("database error");
        assert_eq!(db.constraint(), Some(constraint), "{text}");
    }
    w.t.drop_now().await;
}
