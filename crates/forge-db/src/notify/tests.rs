//! Notifications against a migrated database (M3 N1): the settings, the
//! subscriber-side record (one row per event and person, a job only for a
//! queued one), the send state machine (at most one send, ever), the
//! schema's backstops by constraint name and the doctor's backlog.
//! Synthetic data only.

#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly

use std::time::Duration;

use forge_core::ids::{ProjectId, UserId};
use forge_core::{EventType, NotificationKind, NotificationState, RefName, RequestId, UserKind};
use forge_errors::{ErrorCode, ForgeError, Lang};

use super::*;
use crate::change::{Actor, ChangeContext};
use crate::projects::{self, CreatedProject, NewProject};
use crate::review::{self, NewMergeRequest, StoredMergeRequest};
use crate::service_accounts::{self, NewServiceAccount};
use crate::subscribers::{self, HandleFuture, Ordering, Progress, Start, Subscriber};
use crate::test_support::*;
use crate::users::{self, NewUser};

fn system(request: &str) -> ChangeContext {
    ChangeContext::new(
        Actor::System,
        RequestId::parse(request).expect("request id"),
    )
}

/// What the probe does for every `merge_request.opened` event.
#[derive(Clone, Copy)]
struct Probe {
    initial: Initial,
    /// Handle the event twice in one step (a repeated step).
    twice: bool,
}

impl Subscriber for Probe {
    fn name(&self) -> &'static str {
        "notify-probe"
    }

    fn ordering(&self) -> Ordering {
        Ordering::Independent
    }

    fn handle<'a>(&'a self, event: &'a SourceEvent, tx: &'a mut DerivedTx<'_>) -> HandleFuture<'a> {
        Box::pin(async move {
            if event.event_type != EventType::MergeRequestOpened.as_str() {
                return Ok(());
            }
            let project = event.project_id.expect("project");
            let iid = event.payload["iid"].as_i64().expect("iid");
            let author = merge_request_author(tx, project, iid)
                .await?
                .expect("author");
            let new = NewNotification {
                user: author,
                kind: NotificationKind::MergeRequestApproved,
                project,
                iid,
                actor: event.actor_user_id.map(UserId::new),
                initial: self.initial,
            };
            let delivery = Delivery {
                max_attempts: MaxAttempts::new(5).expect("attempts"),
                timeout: JobTimeout::new(Duration::from_secs(40)).expect("timeout"),
            };
            let first = enqueue(tx, event, &new, &delivery).await?;
            assert!(matches!(first, Recorded::Created(_)), "{first:?}");
            if self.twice {
                let second = enqueue(tx, event, &new, &delivery).await?;
                assert_eq!(second, Recorded::Duplicate);
            }
            Ok(())
        })
    }
}

struct World {
    t: FreshDb,
    alice: UserId,
    bob: UserId,
    web: CreatedProject,
}

async fn world(probe: Probe) -> World {
    let t = FreshDb::migrated().await;
    let alice = users::create(
        &t.db,
        &system("req-user-alice"),
        &NewUser::human("alice", "Alice", "alice@example.test"),
    )
    .await
    .expect("alice");
    let bob = users::create(
        &t.db,
        &system("req-user-bob"),
        &NewUser::human("bob", "Bob", "bob@example.test"),
    )
    .await
    .expect("bob");
    let web = projects::create(
        &t.db,
        &system("req-project-web"),
        &NewProject::new("web", alice.namespace_id, "Web", alice.user_id),
    )
    .await
    .expect("web");
    subscribers::register(&t.db, &probe, Start::Head)
        .await
        .expect("register");
    World {
        t,
        alice: alice.user_id,
        bob: bob.user_id,
        web,
    }
}

async fn open_mr(w: &World) -> StoredMergeRequest {
    review::open_mr_decided(
        &w.t.db,
        &ChangeContext::new(
            Actor::User(w.bob),
            RequestId::parse("req-open-mr").expect("request id"),
        ),
        w.web.project_id,
        w.alice,
        &NewMergeRequest {
            title: "Fix rounding".to_owned(),
            description: String::new(),
            source_branch: RefName::parse("refs/heads/fix-rounding").expect("branch"),
            target_branch: RefName::parse("refs/heads/main").expect("branch"),
            draft: false,
        },
        |_| Ok(()),
    )
    .await
    .map(|(stored, ())| stored)
    .expect("open")
}

/// Number the events, then run the probe until it caught up.
async fn drain(w: &World, probe: &Probe) -> Result<(), ForgeError> {
    crate::sequencer::run_once(
        &w.t.db,
        crate::sequencer::BatchSize::MAX,
        crate::sequencer::AuditSequencing::Skip,
    )
    .await?;
    loop {
        match subscribers::run_once(&w.t.db, probe).await? {
            Progress::Handled { .. } => {}
            Progress::CaughtUp => return Ok(()),
            other @ (Progress::Busy
            | Progress::Disabled
            | Progress::Failed(_)
            | Progress::Stuck(_)
            | Progress::Quarantined(_)) => panic!("unexpected {other:?}"),
        }
    }
}

const QUEUE: Probe = Probe {
    initial: Initial::Queue,
    twice: true,
};

/// (state, reason, attempts) of every notification, oldest first.
async fn rows(w: &World) -> Vec<(String, Option<String>, i32)> {
    sqlx::query_as("SELECT state, reason, attempts FROM forge.notifications ORDER BY id")
        .fetch_all(&mut w.t.superuser().await)
        .await
        .expect("rows")
}

async fn first_id(w: &World) -> i64 {
    sqlx::query_scalar("SELECT min(id) FROM forge.notifications")
        .fetch_one(&mut w.t.superuser().await)
        .await
        .expect("id")
}

#[tokio::test]
async fn preferences_default_without_a_row_and_replace_as_a_whole() {
    let w = world(QUEUE).await;
    assert_eq!(
        preferences(&w.t.db, w.alice).await.expect("defaults"),
        Preferences::default()
    );
    assert!(Preferences::default().email_enabled);

    let saved = Preferences {
        email_enabled: false,
        language: Some(Lang::Id),
        muted: vec![
            NotificationKind::MergeRequestApproved,
            NotificationKind::MergeRequestCommented,
        ],
    };
    assert_eq!(
        set_preferences(&w.t.db, w.alice, &saved)
            .await
            .expect("set"),
        saved
    );
    assert_eq!(preferences(&w.t.db, w.alice).await.expect("read"), saved);
    // Bob's are his own.
    assert_eq!(
        preferences(&w.t.db, w.bob).await.expect("bob"),
        Preferences::default()
    );
    // A second save replaces, it does not merge; duplicates collapse.
    let again = Preferences {
        email_enabled: true,
        language: None,
        muted: vec![
            NotificationKind::MergeRequestMerged,
            NotificationKind::MergeRequestMerged,
        ],
    };
    let stored = set_preferences(&w.t.db, w.alice, &again)
        .await
        .expect("replace");
    assert_eq!(stored.muted, [NotificationKind::MergeRequestMerged]);
    assert!(stored.email_enabled && stored.language.is_none());
    w.t.drop_now().await;
}

#[tokio::test]
async fn the_schema_refuses_unknown_kinds_languages_and_half_states() {
    let w = world(QUEUE).await;
    let mut su = w.t.superuser().await;
    let uid = w.alice.get();
    for (sql, constraint) in [
        (
            format!("INSERT INTO forge.notification_preferences (user_id, muted_kinds) VALUES ({uid}, ARRAY['mr_unknown'])"),
            "notification_preferences_muted_kinds",
        ),
        (
            format!("INSERT INTO forge.notification_preferences (user_id, language) VALUES ({uid}, 'fr')"),
            "notification_preferences_language",
        ),
        (
            format!("INSERT INTO forge.notifications (user_id, kind, event_id, event_seq, project_id, merge_request_iid, request_id, state) VALUES ({uid}, 'mr_merged', gen_random_uuid(), 1, 1, 1, 'r', 'sent')"),
            "notifications_sent_at",
        ),
        (
            format!("INSERT INTO forge.notifications (user_id, kind, event_id, event_seq, project_id, merge_request_iid, request_id, state) VALUES ({uid}, 'mr_merged', gen_random_uuid(), 1, 1, 1, 'r', 'failed')"),
            "notifications_terminal_reason",
        ),
        (
            format!("INSERT INTO forge.notifications (user_id, kind, event_id, event_seq, project_id, merge_request_iid, request_id) VALUES ({uid}, 'mr_closed', gen_random_uuid(), 1, 1, 1, 'r')"),
            "notifications_kind",
        ),
        (
            format!("INSERT INTO forge.notifications (user_id, kind, event_id, event_seq, project_id, merge_request_iid, request_id, state, reason) VALUES ({uid}, 'mr_merged', gen_random_uuid(), 1, 1, 1, 'r', 'skipped', 'Not A Code')"),
            "notifications_reason_shape",
        ),
    ] {
        let err = sqlx::query(&sql).execute(&mut su).await.expect_err(constraint);
        assert!(err.to_string().contains(constraint), "{constraint}: {err}");
    }
    drop(su);
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_queued_notification_has_one_row_and_one_job_even_when_the_step_repeats() {
    let w = world(QUEUE).await;
    let mr = open_mr(&w).await;
    drain(&w, &QUEUE).await.expect("drain");

    assert_eq!(rows(&w).await, [("pending".to_owned(), None, 0)]);
    let id = first_id(&w).await;
    let jobs: Vec<(String, String, Option<String>, i32, i32)> = sqlx::query_as(
        "SELECT kind, payload::text, dedup_key, max_attempts, timeout_ms FROM forge.jobs \
         WHERE kind = 'notification.send'",
    )
    .fetch_all(&mut w.t.superuser().await)
    .await
    .expect("jobs");
    assert_eq!(
        jobs,
        [(
            SEND_JOB.to_owned(),
            format!("{{\"notification_id\": {id}}}"),
            Some(format!("notification:{id}")),
            5,
            40_000
        )]
    );
    // The row names what the mail is about, nothing more.
    let row: (i64, String, i64, i64, Option<i64>) = sqlx::query_as(
        "SELECT user_id, kind, project_id, merge_request_iid, actor_user_id FROM forge.notifications",
    )
    .fetch_one(&mut w.t.superuser().await)
    .await
    .expect("row");
    assert_eq!(
        row,
        (
            w.alice.get(),
            "mr_approved".to_owned(),
            w.web.project_id.get(),
            mr.iid,
            Some(w.bob.get())
        )
    );
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_skipped_or_suppressed_notification_is_a_record_without_a_job() {
    for (initial, state, reason) in [
        (Initial::Skip("opted_out"), "skipped", "opted_out"),
        (Initial::Suppress("no_address"), "suppressed", "no_address"),
    ] {
        let probe = Probe {
            initial,
            twice: false,
        };
        let w = world(probe).await;
        open_mr(&w).await;
        drain(&w, &probe).await.expect("drain");
        assert_eq!(
            rows(&w).await,
            [(state.to_owned(), Some(reason.to_owned()), 0)]
        );
        let jobs: i64 = sqlx::query_scalar("SELECT count(*) FROM forge.jobs WHERE kind = $1")
            .bind(SEND_JOB)
            .fetch_one(&mut w.t.superuser().await)
            .await
            .expect("jobs");
        assert_eq!(jobs, 0);
        w.t.drop_now().await;
    }
}

#[tokio::test]
async fn a_send_is_taken_once_and_a_dead_run_is_never_sent_again() {
    let w = world(QUEUE).await;
    open_mr(&w).await;
    drain(&w, &QUEUE).await.expect("drain");
    let id = first_id(&w).await;

    // Run 1 takes it.
    let Begin::Send(stored) = begin_send(&w.t.db, id, 1).await.expect("begin") else {
        panic!("expected the send");
    };
    assert_eq!(
        (stored.id, stored.user, stored.kind, stored.iid),
        (id, w.alice, NotificationKind::MergeRequestApproved, 1)
    );
    assert_eq!(stored.project, w.web.project_id);
    assert_eq!(stored.actor, Some(w.bob));
    assert_eq!(rows(&w).await, [("sending".to_owned(), None, 1)]);

    // Run 2 finds it still `sending`: run 1 died or timed out mid-send, so
    // the outcome is unknown and nothing is sent again.
    assert_eq!(
        begin_send(&w.t.db, id, 2).await.expect("second"),
        Begin::Interrupted
    );
    assert_eq!(
        rows(&w).await,
        [("unknown".to_owned(), Some("interrupted".to_owned()), 1)]
    );
    // Run 1 comes back late: it no longer owns the send.
    assert!(!finish(&w.t.db, id, 1, Finish::Sent).await.expect("late"));
    assert_eq!(
        begin_send(&w.t.db, id, 3).await.expect("third"),
        Begin::Settled(NotificationState::Unknown)
    );
    assert_eq!(
        begin_send(&w.t.db, id + 1000, 1).await.expect("missing"),
        Begin::Missing
    );
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_failed_try_is_retried_and_a_sent_one_is_final() {
    let w = world(QUEUE).await;
    open_mr(&w).await;
    drain(&w, &QUEUE).await.expect("drain");
    let id = first_id(&w).await;

    assert!(matches!(
        begin_send(&w.t.db, id, 1).await.expect("begin"),
        Begin::Send(_)
    ));
    // A fenced finish: another run number changes nothing.
    assert!(!finish(&w.t.db, id, 2, Finish::Sent).await.expect("fenced"));
    assert!(finish(&w.t.db, id, 1, Finish::Retry("connect_failed"))
        .await
        .expect("retry"));
    assert_eq!(
        rows(&w).await,
        [("retrying".to_owned(), Some("connect_failed".to_owned()), 1)]
    );
    // The retry may be taken again, now as run 2.
    assert!(matches!(
        begin_send(&w.t.db, id, 2).await.expect("again"),
        Begin::Send(_)
    ));
    assert!(finish(&w.t.db, id, 2, Finish::Sent).await.expect("sent"));
    let sent: (String, bool, Option<String>) =
        sqlx::query_as("SELECT state, sent_at IS NOT NULL, reason FROM forge.notifications")
            .fetch_one(&mut w.t.superuser().await)
            .await
            .expect("row");
    assert_eq!(sent, ("sent".to_owned(), true, None));
    // Final: a stray job run does nothing.
    assert_eq!(
        begin_send(&w.t.db, id, 3).await.expect("settled"),
        Begin::Settled(NotificationState::Sent)
    );
    assert!(!finish(&w.t.db, id, 2, Finish::Failed("x"))
        .await
        .expect("final"));
    w.t.drop_now().await;
}

#[tokio::test]
async fn every_ending_is_recorded_with_its_reason() {
    for (how, state, reason) in [
        (
            Finish::Failed("rejected_permanent"),
            "failed",
            Some("rejected_permanent"),
        ),
        (
            Finish::Unknown("outcome_unknown"),
            "unknown",
            Some("outcome_unknown"),
        ),
        (Finish::Skipped("opted_out"), "skipped", Some("opted_out")),
        (
            Finish::Suppressed("not_visible"),
            "suppressed",
            Some("not_visible"),
        ),
    ] {
        let w = world(QUEUE).await;
        open_mr(&w).await;
        drain(&w, &QUEUE).await.expect("drain");
        let id = first_id(&w).await;
        assert!(matches!(
            begin_send(&w.t.db, id, 1).await.expect("begin"),
            Begin::Send(_)
        ));
        assert!(finish(&w.t.db, id, 1, how).await.expect("finish"));
        assert_eq!(
            rows(&w).await,
            [(state.to_owned(), reason.map(str::to_owned), 1)]
        );
        w.t.drop_now().await;
    }
}

#[tokio::test]
async fn the_recipient_is_read_with_the_facts_the_mail_needs() {
    let w = world(QUEUE).await;
    let alice = recipient(&w.t.db, w.alice)
        .await
        .expect("read")
        .expect("alice");
    assert_eq!(
        alice,
        Recipient {
            email: Some("alice@example.test".to_owned()),
            display_name: "Alice".to_owned(),
            active: true,
            kind: UserKind::Human,
            is_admin: false,
        }
    );
    let sa = service_accounts::create(
        &w.t.db,
        &system("req-sa"),
        &NewServiceAccount {
            username: "ci-bot".to_owned(),
            display_name: "CI".to_owned(),
            owner: w.alice,
            purpose: "pipelines".to_owned(),
        },
        w.alice,
    )
    .await
    .expect("service account")
    .user_id;
    let sa = recipient(&w.t.db, sa).await.expect("read").expect("sa");
    assert_eq!((sa.kind, sa.email), (UserKind::ServiceAccount, None));
    assert!(recipient(&w.t.db, UserId::new(9_999_999))
        .await
        .expect("read")
        .is_none());
    w.t.drop_now().await;
}

#[tokio::test]
async fn the_backlog_counts_waiting_and_recent_problems() {
    let w = world(QUEUE).await;
    assert_eq!(
        backlog(&w.t.db).await.expect("empty"),
        Backlog {
            waiting: 0,
            oldest_waiting_secs: None,
            failed_24h: 0,
            unknown_24h: 0
        }
    );
    open_mr(&w).await;
    drain(&w, &QUEUE).await.expect("drain");
    let waiting = backlog(&w.t.db).await.expect("one waiting");
    assert_eq!(waiting.waiting, 1);
    assert!(waiting.oldest_waiting_secs.is_some_and(|s| s >= 0));
    let id = first_id(&w).await;
    assert!(matches!(
        begin_send(&w.t.db, id, 1).await.expect("begin"),
        Begin::Send(_)
    ));
    assert!(finish(&w.t.db, id, 1, Finish::Failed("rejected_permanent"))
        .await
        .expect("failed"));
    let after = backlog(&w.t.db).await.expect("after");
    assert_eq!(
        (after.waiting, after.failed_24h, after.unknown_24h),
        (0, 1, 0)
    );
    assert_eq!(after.oldest_waiting_secs, None);
    // An old failure is not "recent".
    sqlx::query("UPDATE forge.notifications SET updated_at = now() - interval '2 days'")
        .execute(&mut w.t.superuser().await)
        .await
        .expect("age");
    assert_eq!(backlog(&w.t.db).await.expect("old").failed_24h, 0);
    w.t.drop_now().await;
}

#[test]
fn the_project_and_error_types_are_the_shared_ones() {
    // The send job's payload names a notification, never a person or text.
    let _: ProjectId = ProjectId::new(1);
    assert_eq!(SEND_JOB, "notification.send");
    assert_eq!(ErrorCode::SmtpUnavailable.id(), "FG-INT-0015");
}
