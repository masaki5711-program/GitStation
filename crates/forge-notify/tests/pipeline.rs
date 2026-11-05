#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! The notify subscriber and the send job against a migrated database (M3
//! N1): events become one notification per recipient, a person who cannot
//! see the project (any more) is never mailed, the person's settings and
//! language apply, a failed send is retried with bounded tries, and a
//! message that may have been handed over is never sent twice. Synthetic
//! data only; the relay is in process.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use forge_core::config::NotifyConfig;
use forge_core::ids::{ProjectId, UserId};
use forge_core::{NotificationKind, RequestId};
use forge_db::change::{Actor, ChangeContext};
use forge_db::jobs::{
    ClaimedJob, JobFailure, JobHandler as _, JobId, JobTimeout, Lease, Retry, WorkerId,
};
use forge_db::notify::{self, Preferences};
use forge_db::projects::{self, CreatedProject, NewProject};
use forge_db::subscribers::{self, Progress, Start};
use forge_db::test_support::FreshDb;
use forge_db::users::{self, NewUser};
use forge_errors::{ErrorCode, Lang};
use forge_notify::test_support::{FakeSmtp, Script};
use forge_notify::{Mail, Notify, SendFailure, SendHandler, SendOutcome, Sender, SmtpSender};
use forge_notify::{Reason, Stage};

// ── fixtures ──────────────────────────────────────────────────────────

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
    cfg: NotifyConfig,
    notify: Notify,
    /// Keeps the relay of `cfg` alive.
    relay: FakeSmtp,
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
    let relay = FakeSmtp::start(Script::default()).await;
    let cfg = relay.config();
    let notify = Notify::new(&t.db, &cfg).expect("notify");
    subscribers::register(&t.db, &notify, Start::Head)
        .await
        .expect("register");
    World {
        t,
        alice: alice.user_id,
        bob: bob.user_id,
        carol: carol.user_id,
        web,
        cfg,
        notify,
        relay,
    }
}

impl World {
    fn project(&self) -> ProjectId {
        self.web.project_id
    }

    /// A merge request of `author` (iid 1, 2, …).
    async fn open_mr(&self, author: UserId, title: &str, branch: &str) -> i64 {
        // Built like the command leaves it (counter + row, audited); the
        // review commands are forge-review's alone (its guard test).
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

    /// Publish a merge request event (the audit row its commit needs, then
    /// the event), as the commands of forge-review would.
    async fn emit(
        &self,
        event_type: &str,
        actor: Option<UserId>,
        payload: serde_json::Value,
        n: u32,
    ) {
        let (kind, actor_id) = match actor {
            Some(a) => ("user", a.get().to_string()),
            None => ("system", "NULL".to_owned()),
        };
        self.t
            .exec_superuser(&format!(
                "INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, \
                 request_id) VALUES ('system', 'merge_request.probe', 'succeeded', \
                 'merge_request', 'req-n1-{n}'); \
                 INSERT INTO forge.events (event_id, event_type, schema_version, aggregate_type, \
                 aggregate_id, project_id, actor_kind, actor_user_id, request_id, payload) \
                 VALUES (gen_random_uuid(), '{event_type}', 1, 'merge_request', 1, {}, '{kind}', \
                 {actor_id}, 'req-n1-{n}', '{payload}'::jsonb)",
                self.project().get()
            ))
            .await;
    }

    async fn approved(&self, iid: i64, by: UserId, n: u32) {
        self.emit(
            "merge_request.approved",
            Some(by),
            serde_json::json!({"merge_request_id": 1, "iid": iid, "approver_id": by.get()}),
            n,
        )
        .await;
    }

    /// Number the events, then run the subscriber until it caught up.
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

    /// (user, kind, state, reason, attempts) of every notification.
    async fn rows(&self) -> Vec<(i64, String, String, Option<String>, i32)> {
        sqlx::query_as(
            "SELECT user_id, kind, state, reason, attempts FROM forge.notifications ORDER BY id",
        )
        .fetch_all(&mut self.t.superuser().await)
        .await
        .expect("rows")
    }

    async fn ids(&self) -> Vec<i64> {
        sqlx::query_scalar("SELECT id FROM forge.notifications ORDER BY id")
            .fetch_all(&mut self.t.superuser().await)
            .await
            .expect("ids")
    }

    async fn send_jobs(&self) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM forge.jobs WHERE kind = 'notification.send'")
            .fetch_one(&mut self.t.superuser().await)
            .await
            .expect("jobs")
    }

    fn handler(&self, sender: Arc<dyn Sender>) -> SendHandler {
        SendHandler::new(
            &self.t.db,
            sender,
            &self.cfg,
            Some("https://git.example.test"),
        )
        .expect("handler")
    }

    async fn set_prefs(&self, user: UserId, prefs: Preferences) {
        notify::set_preferences(&self.t.db, user, &prefs)
            .await
            .expect("prefs");
    }
}

/// A claimed `notification.send` job, run number `attempt` of `max`.
fn claimed(notification: i64, attempt: i32, max: i32) -> ClaimedJob {
    ClaimedJob {
        lease: Lease {
            job_id: JobId::new(1),
            worker_id: WorkerId::generate(),
            attempt,
        },
        kind: forge_db::jobs::JobKind::parse("notification.send").expect("kind"),
        payload: serde_json::json!({ "notification_id": notification }),
        max_attempts: max,
        timeout: JobTimeout::new(Duration::from_secs(40)).expect("timeout"),
        origin_request_id: "req-n1-send".to_owned(),
        origin_event_seq: Some(1),
    }
}

/// A sender that answers from a script and keeps what it was given.
struct Scripted {
    outcomes: Mutex<VecDeque<SendOutcome>>,
    sent: Mutex<Vec<Mail>>,
}

impl Scripted {
    fn new(outcomes: Vec<SendOutcome>) -> Arc<Self> {
        Arc::new(Self {
            outcomes: Mutex::new(outcomes.into()),
            sent: Mutex::new(Vec::new()),
        })
    }

    fn sent(&self) -> Vec<Mail> {
        self.sent.lock().expect("sent").clone()
    }
}

impl Sender for Scripted {
    fn send<'a>(
        &'a self,
        mail: &'a Mail,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = SendOutcome> + Send + 'a>> {
        Box::pin(async move {
            self.sent.lock().expect("sent").push(mail.clone());
            self.outcomes
                .lock()
                .expect("outcomes")
                .pop_front()
                .unwrap_or(SendOutcome::Delivered)
        })
    }
}

fn not_sent(reason: Reason, stage: Stage, code: Option<u16>) -> SendOutcome {
    SendOutcome::NotSent(SendFailure {
        reason,
        stage,
        smtp_code: code,
    })
}

/// Log lines of the current thread, for the assertions on `error_code`.
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("log").extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
    type Writer = Capture;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

impl Capture {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().expect("log")).into_owned()
    }

    fn install(&self) -> tracing::subscriber::DefaultGuard {
        tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .with_writer(self.clone())
                .with_ansi(false)
                .with_max_level(tracing::Level::DEBUG)
                .finish(),
        )
    }
}

// ── the subscriber ────────────────────────────────────────────────────

#[tokio::test]
async fn an_approval_tells_the_author_once_and_the_approver_never() {
    let w = world().await;
    let iid = w.open_mr(w.alice, "Fix rounding", "fix-rounding").await;
    w.approved(iid, w.bob, 1).await;
    // Alice approving her own merge request tells nobody.
    w.approved(iid, w.alice, 2).await;
    w.drain().await;
    assert_eq!(
        w.rows().await,
        [(
            w.alice.get(),
            "mr_approved".to_owned(),
            "pending".to_owned(),
            None,
            0
        )]
    );
    assert_eq!(w.send_jobs().await, 1);
    // The subscriber is at the head; handling the same events again (a
    // repeated step) adds nothing.
    w.drain().await;
    assert_eq!(w.rows().await.len(), 1);
    assert_eq!(w.send_jobs().await, 1);
    w.t.drop_now().await;
}

#[tokio::test]
async fn merged_and_commented_tell_the_author_and_thread_changes_tell_nobody() {
    let w = world().await;
    let iid = w.open_mr(w.alice, "Fix rounding", "fix-rounding").await;
    w.emit(
        "merge_request.merged",
        Some(w.bob),
        serde_json::json!({"merge_request_id": 1, "iid": iid, "merged_by": w.bob.get()}),
        1,
    )
    .await;
    w.emit(
        "merge_request.commented",
        Some(w.bob),
        serde_json::json!({"merge_request_id": 1, "iid": iid, "action": "note_added"}),
        2,
    )
    .await;
    for (n, action) in [(3, "thread_resolved"), (4, "thread_unresolved")] {
        w.emit(
            "merge_request.commented",
            Some(w.bob),
            serde_json::json!({"merge_request_id": 1, "iid": iid, "action": action}),
            n,
        )
        .await;
    }
    // Events nobody is told about.
    w.emit(
        "merge_request.closed",
        Some(w.bob),
        serde_json::json!({"merge_request_id": 1, "iid": iid}),
        5,
    )
    .await;
    w.emit(
        "merge_request.unapproved",
        Some(w.bob),
        serde_json::json!({"merge_request_id": 1, "iid": iid}),
        6,
    )
    .await;
    // Merged by the author herself: nothing.
    w.emit(
        "merge_request.merged",
        Some(w.alice),
        serde_json::json!({"merge_request_id": 1, "iid": iid, "merged_by": w.alice.get()}),
        7,
    )
    .await;
    w.drain().await;
    let kinds: Vec<String> = w.rows().await.into_iter().map(|r| r.1).collect();
    assert_eq!(kinds, ["mr_merged", "mr_commented"]);
    w.t.drop_now().await;
}

#[tokio::test]
async fn an_event_of_a_type_this_build_does_not_know_is_passed_over_with_a_warning() {
    let w = world().await;
    let log = Capture::default();
    let _guard = log.install();
    w.emit(
        "merge_request.frobnicated",
        Some(w.bob),
        serde_json::json!({"iid": 1}),
        1,
    )
    .await;
    w.drain().await;
    assert!(w.rows().await.is_empty());
    let text = log.text();
    assert!(
        text.contains("forge.subscribers.unknown_event_type"),
        "{text}"
    );
    assert!(text.contains("merge_request.frobnicated"), "{text}");
    w.t.drop_now().await;
}

#[tokio::test]
async fn the_decision_is_recorded_with_its_reason_when_nothing_is_to_be_sent() {
    let w = world().await;
    let iid = w.open_mr(w.alice, "Fix rounding", "fix-rounding").await;
    // Mail off.
    w.set_prefs(
        w.alice,
        Preferences {
            email_enabled: false,
            ..Preferences::default()
        },
    )
    .await;
    w.approved(iid, w.bob, 1).await;
    w.drain().await;
    // One kind muted, mail on again.
    w.set_prefs(
        w.alice,
        Preferences {
            muted: vec![NotificationKind::MergeRequestApproved],
            ..Preferences::default()
        },
    )
    .await;
    w.approved(iid, w.carol, 2).await;
    w.drain().await;
    let reasons: Vec<(String, Option<String>)> =
        w.rows().await.into_iter().map(|r| (r.2, r.3)).collect();
    assert_eq!(
        reasons,
        [
            ("skipped".to_owned(), Some("opted_out".to_owned())),
            ("skipped".to_owned(), Some("muted".to_owned())),
        ]
    );
    assert_eq!(
        w.send_jobs().await,
        0,
        "no job for a notification not to be sent"
    );
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_person_who_cannot_see_the_project_is_suppressed_at_enqueue_and_at_send() {
    let w = world().await;
    // Carol is a developer of alice's private project, so she sees it.
    w.t.commit_audited(
        ("project", w.project().get()),
        &[format!(
            "INSERT INTO forge.project_members (project_id, user_id, role) VALUES ({}, {}, 'developer')",
            w.project().get(),
            w.carol.get()
        )],
    )
    .await;
    let iid = w.open_mr(w.carol, "Carol's change", "carol-change").await;
    w.approved(iid, w.bob, 1).await;
    w.drain().await;
    let id = w.ids().await[0];
    assert_eq!(w.rows().await[0].2, "pending");

    // She is removed before the job runs: nothing is mailed.
    w.t.commit_audited(
        ("project", w.project().get()),
        &[format!(
            "DELETE FROM forge.project_members WHERE project_id = {} AND user_id = {}",
            w.project().get(),
            w.carol.get()
        )],
    )
    .await;
    let sender = Scripted::new(vec![]);
    let handler = w.handler(sender.clone());
    handler.run(claimed(id, 1, 3)).await.expect("job");
    assert!(sender.sent().is_empty());
    assert_eq!(
        w.rows().await[0],
        (
            w.carol.get(),
            "mr_approved".to_owned(),
            "suppressed".to_owned(),
            Some("not_visible".to_owned()),
            1
        )
    );

    // And a notification recorded for someone who never could see it.
    let iid2 = w.open_mr(w.alice, "Alice's change", "alice-change").await;
    w.t.exec_superuser(&format!(
        "INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id) \
         VALUES ('system', 'merge_request.probe', 'succeeded', 'merge_request', 'req-n1-x'); \
         INSERT INTO forge.events (event_id, event_type, schema_version, aggregate_type, \
         aggregate_id, project_id, actor_kind, actor_user_id, request_id, payload) VALUES \
         (gen_random_uuid(), 'merge_request.approved', 1, 'merge_request', 2, {}, 'user', {}, \
         'req-n1-x', '{{\"merge_request_id\": 2, \"iid\": {iid2}, \"approver_id\": {}}}'::jsonb)",
        w.project().get(),
        w.bob.get(),
        w.bob.get()
    ))
    .await;
    w.drain().await;
    // Alice owns it: pending. (Bob, an outsider, is never a recipient here.)
    assert_eq!(w.rows().await.last().expect("row").2, "pending");
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_disabled_recipient_and_a_service_account_are_not_mailed() {
    let w = world().await;
    let iid = w.open_mr(w.alice, "Fix rounding", "fix-rounding").await;
    w.approved(iid, w.bob, 1).await;
    w.drain().await;
    let id = w.ids().await[0];
    w.t.commit_audited(
        ("user", w.alice.get()),
        &[format!(
            "UPDATE forge.users SET state = 'disabled' WHERE id = {}",
            w.alice.get()
        )],
    )
    .await;
    let sender = Scripted::new(vec![]);
    w.handler(sender.clone())
        .run(claimed(id, 1, 3))
        .await
        .expect("job");
    assert!(sender.sent().is_empty());
    assert_eq!(w.rows().await[0].2, "suppressed", "{:?}", w.rows().await);
    assert_eq!(w.rows().await[0].3.as_deref(), Some("recipient_inactive"));
    w.t.drop_now().await;
}

// ── the send job ──────────────────────────────────────────────────────

#[tokio::test]
async fn the_mail_is_in_the_recipients_language_and_says_only_what_they_may_see() {
    let w = world().await;
    let iid = w
        .open_mr(w.alice, "Fix rounding", "secret-branch-name")
        .await;
    w.set_prefs(
        w.alice,
        Preferences {
            language: Some(Lang::Ja),
            ..Preferences::default()
        },
    )
    .await;
    w.approved(iid, w.bob, 1).await;
    w.drain().await;
    let id = w.ids().await[0];
    let sender = Scripted::new(vec![]);
    w.handler(sender.clone())
        .run(claimed(id, 1, 3))
        .await
        .expect("job");
    let mails = sender.sent();
    assert_eq!(mails.len(), 1);
    let mail = &mails[0];
    assert_eq!(mail.to_address(), "alice@example.test");
    assert_eq!(mail.from_address(), "forge@git.example.test");
    assert_eq!(mail.subject(), "[GitStation] alice/web!1 が承認されました");
    let body = mail.body();
    assert!(
        body.contains("Bob さんが、あなたのマージリクエスト「Fix rounding」を承認しました。"),
        "{body}"
    );
    assert!(
        body.contains("https://git.example.test/alice/web/-/pull/1"),
        "{body}"
    );
    // Nothing internal: not the branch, not an id, not a code.
    for forbidden in [
        "secret-branch-name",
        "refs/heads",
        "req-n1",
        "FG-",
        "notification_id",
    ] {
        assert!(
            !body.contains(forbidden) && !mail.subject().contains(forbidden),
            "{forbidden} in {body}"
        );
    }
    assert_eq!(w.rows().await[0].2, "sent");
    // The default language applies to a person who chose none.
    let iid2 = w.open_mr(w.bob, "Bob's change", "bob-change").await;
    w.t.commit_audited(
        ("project", w.project().get()),
        &[format!(
            "INSERT INTO forge.project_members (project_id, user_id, role) VALUES ({}, {}, 'developer')",
            w.project().get(),
            w.bob.get()
        )],
    )
    .await;
    w.approved(iid2, w.alice, 2).await;
    w.drain().await;
    let id2 = w.ids().await[1];
    w.handler(sender.clone())
        .run(claimed(id2, 1, 3))
        .await
        .expect("job");
    assert_eq!(
        sender.sent()[1].subject(),
        "[GitStation] alice/web!2 was approved"
    );
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_failed_try_is_retried_with_bounded_tries_and_the_last_one_is_failed_not_dead() {
    let w = world().await;
    let iid = w.open_mr(w.alice, "Fix rounding", "fix-rounding").await;
    w.approved(iid, w.bob, 1).await;
    w.drain().await;
    let id = w.ids().await[0];
    let log = Capture::default();
    let _guard = log.install();
    let sender = Scripted::new(vec![
        not_sent(Reason::ConnectFailed, Stage::Connect, None),
        not_sent(Reason::RejectedTemporary, Stage::RcptTo, Some(451)),
        not_sent(Reason::Timeout, Stage::Data, None),
    ]);
    let handler = w.handler(sender.clone());

    // Run 1: a retryable failure; the queue backs off and calls again.
    let Err(JobFailure { error, retry }) = handler.run(claimed(id, 1, 3)).await else {
        panic!("expected a retryable failure");
    };
    assert_eq!(retry, Retry::Backoff);
    assert_eq!(error.code(), ErrorCode::SmtpUnavailable);
    let text = error.to_string();
    assert!(
        text.contains("reason=connect_failed") && text.contains("stage=connect"),
        "{text}"
    );
    assert_eq!(
        (
            w.rows().await[0].2.as_str(),
            w.rows().await[0].3.as_deref(),
            w.rows().await[0].4
        ),
        ("retrying", Some("connect_failed"), 1)
    );
    // Run 2.
    let Err(JobFailure { error, .. }) = handler.run(claimed(id, 2, 3)).await else {
        panic!("expected a retryable failure");
    };
    assert!(error.to_string().contains("smtp_code=451"));
    // Run 3 is the last: failed, not dead (the job succeeds), and logged
    // once with FG-INT-0015 at warn.
    handler
        .run(claimed(id, 3, 3))
        .await
        .expect("the last run ends the job");
    assert_eq!(
        (
            w.rows().await[0].2.as_str(),
            w.rows().await[0].3.as_deref(),
            w.rows().await[0].4
        ),
        ("failed", Some("tries_used_up"), 3)
    );
    assert_eq!(sender.sent().len(), 3);
    let text = log.text();
    assert_eq!(text.matches("FG-INT-0015").count(), 1, "{text}");
    assert!(
        text.contains("tries_used_up") && text.contains("WARN"),
        "{text}"
    );
    assert!(
        !text.contains("alice@example.test"),
        "no address in a log: {text}"
    );
    // A stray fourth run changes nothing.
    handler.run(claimed(id, 4, 3)).await.expect("settled");
    assert_eq!(sender.sent().len(), 3);
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_permanent_refusal_is_failed_at_once_without_a_retry() {
    let w = world().await;
    let iid = w.open_mr(w.alice, "Fix rounding", "fix-rounding").await;
    w.approved(iid, w.bob, 1).await;
    w.drain().await;
    let id = w.ids().await[0];
    let log = Capture::default();
    let _guard = log.install();
    let sender = Scripted::new(vec![not_sent(
        Reason::RejectedPermanent,
        Stage::RcptTo,
        Some(550),
    )]);
    w.handler(sender.clone())
        .run(claimed(id, 1, 5))
        .await
        .expect("no retry, no dead job");
    assert_eq!(
        (w.rows().await[0].2.as_str(), w.rows().await[0].3.as_deref()),
        ("failed", Some("rejected_permanent"))
    );
    let text = log.text();
    assert_eq!(text.matches("FG-INT-0015").count(), 1, "{text}");
    assert!(
        text.contains("smtp_code=550") && text.contains("stage=rcpt_to"),
        "{text}"
    );
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_message_that_may_have_been_handed_over_is_never_sent_again() {
    let w = world().await;
    let iid = w.open_mr(w.alice, "Fix rounding", "fix-rounding").await;
    w.approved(iid, w.bob, 1).await;
    w.drain().await;
    let id = w.ids().await[0];
    let log = Capture::default();
    let _guard = log.install();
    let sender = Scripted::new(vec![SendOutcome::Unknown(SendFailure {
        reason: Reason::OutcomeUnknown,
        stage: Stage::EndOfData,
        smtp_code: None,
    })]);
    let handler = w.handler(sender.clone());
    handler.run(claimed(id, 1, 5)).await.expect("ends the job");
    assert_eq!(
        (w.rows().await[0].2.as_str(), w.rows().await[0].3.as_deref()),
        ("unknown", Some("outcome_unknown"))
    );
    // The queue calls again (a reaped lease, a duplicate): nothing is sent.
    handler.run(claimed(id, 2, 5)).await.expect("settled");
    assert_eq!(sender.sent().len(), 1);
    assert!(
        log.text().contains("reason=outcome_unknown"),
        "{}",
        log.text()
    );
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_run_that_died_mid_send_is_recorded_unknown_and_the_retry_sends_nothing() {
    let w = world().await;
    let iid = w.open_mr(w.alice, "Fix rounding", "fix-rounding").await;
    w.approved(iid, w.bob, 1).await;
    w.drain().await;
    let id = w.ids().await[0];
    // Run 1 took the send and the worker died before it could record.
    assert!(matches!(
        notify::begin_send(&w.t.db, id, 1)
            .await
            .expect("take the send"),
        notify::Begin::Send(_)
    ));
    let log = Capture::default();
    let _guard = log.install();
    let sender = Scripted::new(vec![]);
    w.handler(sender.clone())
        .run(claimed(id, 2, 5))
        .await
        .expect("job");
    assert!(sender.sent().is_empty());
    assert_eq!(
        (w.rows().await[0].2.as_str(), w.rows().await[0].3.as_deref()),
        ("unknown", Some("interrupted"))
    );
    let text = log.text();
    assert!(
        text.contains("FG-INT-0015") && text.contains("outcome_unknown"),
        "{text}"
    );
    w.t.drop_now().await;
}

#[tokio::test]
async fn running_the_same_job_twice_sends_one_mail() {
    let w = world().await;
    let iid = w.open_mr(w.alice, "Fix rounding", "fix-rounding").await;
    w.approved(iid, w.bob, 1).await;
    w.drain().await;
    let id = w.ids().await[0];
    let sender = Scripted::new(vec![]);
    let handler = w.handler(sender.clone());
    handler.run(claimed(id, 1, 5)).await.expect("first");
    handler.run(claimed(id, 1, 5)).await.expect("again");
    assert_eq!(sender.sent().len(), 1);
    assert_eq!(w.rows().await[0].2, "sent");
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_mail_is_delivered_through_the_real_smtp_sender_end_to_end() {
    let w = world().await;
    let iid = w.open_mr(w.alice, "Fix rounding", "fix-rounding").await;
    w.approved(iid, w.bob, 1).await;
    w.drain().await;
    let id = w.ids().await[0];
    let smtp = Arc::new(SmtpSender::new(&w.cfg).expect("sender"));
    w.handler(smtp).run(claimed(id, 1, 3)).await.expect("job");
    let got = w.relay.messages();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].rcpt_to, ["alice@example.test"]);
    assert!(got[0].data.contains("Message-ID: <notification-"));
    assert_eq!(w.rows().await[0].2, "sent");
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_job_for_no_notification_is_a_permanent_failure() {
    let w = world().await;
    let handler = w.handler(Scripted::new(vec![]));
    let Err(JobFailure { error, retry }) = handler.run(claimed(987_654, 1, 3)).await else {
        panic!("expected a failure");
    };
    assert_eq!(retry, Retry::Never);
    assert!(
        error.to_string().contains("notification_missing"),
        "{error}"
    );
    let Err(JobFailure { retry, .. }) = handler
        .run({
            let mut job = claimed(1, 1, 3);
            job.payload = serde_json::json!({"nope": 1});
            job
        })
        .await
    else {
        panic!("expected a failure");
    };
    assert_eq!(retry, Retry::Never);
    w.t.drop_now().await;
}

// ── review follow-ups ─────────────────────────────────────────────────

#[tokio::test]
async fn many_notes_on_one_merge_request_make_at_most_one_mail_per_window() {
    let w = world().await;
    let iid = w.open_mr(w.alice, "Fix rounding", "fix-rounding").await;
    for n in 1..=20 {
        w.emit(
            "merge_request.commented",
            Some(w.bob),
            serde_json::json!({"merge_request_id": 1, "iid": iid, "action": "note_added"}),
            n,
        )
        .await;
    }
    w.drain().await;
    let rows = w.rows().await;
    assert_eq!(rows.len(), 20);
    let pending = rows.iter().filter(|r| r.2 == "pending").count();
    assert_eq!(pending, 1, "{rows:?}");
    assert!(rows
        .iter()
        .filter(|r| r.2 != "pending")
        .all(|r| r.2 == "skipped" && r.3.as_deref() == Some("coalesced")));
    assert_eq!(w.send_jobs().await, 1);
    // A later window mails again; another merge request is its own.
    w.t.exec_superuser("UPDATE forge.notifications SET created_at = now() - interval '11 minutes'")
        .await;
    w.emit(
        "merge_request.commented",
        Some(w.bob),
        serde_json::json!({"merge_request_id": 1, "iid": iid, "action": "note_added"}),
        100,
    )
    .await;
    w.drain().await;
    assert_eq!(w.send_jobs().await, 2);
    w.t.drop_now().await;
}

#[tokio::test]
async fn turning_mail_off_and_on_again_does_not_mail_the_disabled_period() {
    let w = world().await;
    let iid = w.open_mr(w.alice, "Fix rounding", "fix-rounding").await;
    // Enabled: told.
    w.approved(iid, w.bob, 1).await;
    w.drain().await;
    assert_eq!(w.rows().await.len(), 1);
    // Switched off: the same subscriber row keeps reading, records nothing,
    // and is not behind afterwards (no lag, no retention held back).
    let paused = Notify::paused(&w.t.db).expect("paused");
    w.approved(iid, w.carol, 2).await;
    w.approved(iid, w.bob, 3).await;
    forge_db::sequencer::run_once(
        &w.t.db,
        forge_db::sequencer::BatchSize::MAX,
        forge_db::sequencer::AuditSequencing::Skip,
    )
    .await
    .expect("sequencer");
    while matches!(
        subscribers::run_once(&w.t.db, &paused).await.expect("step"),
        Progress::Handled { .. }
    ) {}
    assert_eq!(w.rows().await.len(), 1, "nothing recorded while off");
    let state = subscribers::states(&w.t.db)
        .await
        .expect("states")
        .expect("migrated");
    let notify = state.iter().find(|s| s.name == "notify").expect("notify");
    assert_eq!(notify.lag_events, 0, "{notify:?}");
    // Switched on again: only what happens now is told.
    w.approved(iid, w.bob, 4).await;
    w.drain().await;
    assert_eq!(w.rows().await.len(), 2, "{:?}", w.rows().await);
    w.t.drop_now().await;
}

#[tokio::test]
async fn when_recording_a_retry_fails_the_smtp_cause_is_still_logged() {
    let w = world().await;
    let iid = w.open_mr(w.alice, "Fix rounding", "fix-rounding").await;
    w.approved(iid, w.bob, 1).await;
    w.drain().await;
    let id = w.ids().await[0];
    // The state write of a retry fails (a stand-in for the database going
    // away between the SMTP failure and its record).
    w.t.exec_superuser(
        "CREATE FUNCTION forge.n1_fail_retry() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN IF NEW.state = 'retrying' THEN RAISE EXCEPTION 'n1 test'; END IF; RETURN NEW; END $$; \
         CREATE TRIGGER n1_fail BEFORE UPDATE ON forge.notifications FOR EACH ROW \
         EXECUTE FUNCTION forge.n1_fail_retry()",
    )
    .await;
    let log = Capture::default();
    let _guard = log.install();
    let sender = Scripted::new(vec![not_sent(Reason::ConnectFailed, Stage::Connect, None)]);
    let Err(JobFailure { retry, .. }) = w.handler(sender.clone()).run(claimed(id, 1, 3)).await
    else {
        panic!("the job must be retried");
    };
    assert_eq!(retry, Retry::Backoff);
    let text = log.text();
    assert!(
        text.contains("FG-INT-0015") && text.contains("reason=connect_failed"),
        "the SMTP cause is logged: {text}"
    );
    w.t.drop_now().await;
}
