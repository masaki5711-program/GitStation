//! Webhooks and deliveries against a migrated database (M3 I2; migration
//! 20261120200000): the decided commands, the 20-webhook limit, the
//! secret rotation's overlap, the attempt bookkeeping (backoff, the last
//! attempt, the automatic disable at 50 failures in a row or 24 hours of
//! failures), redelivery, retention, the constraints and the read pool.

use forge_core::ids::{ProjectId, UserId, WebhookDeliveryId, WebhookId};
use forge_core::webhook::{WebhookSecret, WebhookSecretKeys};
use forge_core::{DisableReason, RequestId, WebhookField};
use forge_errors::ErrorCode;

use super::*;
use crate::change::{Actor, ChangeContext};
use crate::projects::{self, CreatedProject, NewProject};
use crate::test_support::*;
use crate::users::{self, NewUser};

const KEYS: &str = "1 0000000000000000000000000000000000000000000000000000000000000001\n";

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

fn keys() -> WebhookSecretKeys {
    WebhookSecretKeys::parse(KEYS).expect("keys")
}

fn new_hook(url: &str, types: &[&str]) -> NewWebhook {
    let uid = uuid::Uuid::new_v4();
    let secret = WebhookSecret::generate().expect("secret");
    NewWebhook {
        uid,
        url: url.to_owned(),
        origin: "https://kanflow.internal:443".to_owned(),
        event_types: types.iter().map(|t| (*t).to_owned()).collect(),
        secret: keys().seal(&uid, &secret).expect("seal"),
    }
}

struct World {
    t: FreshDb,
    alice: UserId,
    web: CreatedProject,
    other: CreatedProject,
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
    let web = projects::create(
        &t.db,
        &system("req-project-web"),
        &NewProject::new("web", alice.namespace_id, "Web", alice.user_id),
    )
    .await
    .expect("web");
    let other = projects::create(
        &t.db,
        &system("req-project-other"),
        &NewProject::new("other", alice.namespace_id, "Other", alice.user_id),
    )
    .await
    .expect("other");
    World {
        t,
        alice: alice.user_id,
        web,
        other,
    }
}

impl World {
    fn project(&self) -> ProjectId {
        self.web.project_id
    }

    async fn create(&self, request: &str) -> WebhookId {
        create_decided(
            &self.t.db,
            &as_user(self.alice, request),
            self.project(),
            self.alice,
            &new_hook("https://kanflow.internal/hook", &["*"]),
            |_| Ok(()),
        )
        .await
        .map(|(id, ())| id)
        .expect("create")
    }

    async fn scalar_i64(&self, sql: &str) -> i64 {
        sqlx::query_scalar(sql)
            .fetch_one(&mut self.t.superuser().await)
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e}"))
    }

    async fn text(&self, sql: &str) -> String {
        sqlx::query_scalar(sql)
            .fetch_one(&mut self.t.superuser().await)
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e}"))
    }

    /// A pending delivery row of `hook` (the subscriber's write, here by
    /// the superuser: derived rows need no audit).
    async fn delivery(&self, hook: WebhookId, seq: i64) -> WebhookDeliveryId {
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO forge.webhook_deliveries (delivery_uid, webhook_id, project_id, \
             event_seq, event_id, event_type, request_id) VALUES (gen_random_uuid(), $1, $2, $3, \
             gen_random_uuid(), 'merge_request.opened', 'req-delivery') RETURNING id",
        )
        .bind(hook.get())
        .bind(self.project().get())
        .bind(seq)
        .fetch_one(&mut self.t.superuser().await)
        .await
        .expect("delivery");
        WebhookDeliveryId::new(id)
    }

    async fn attempt(
        &self,
        delivery: WebhookDeliveryId,
        round: i32,
        attempt: i32,
        outcome: AttemptOutcome,
    ) -> RecordedAttempt {
        record_attempt(
            &self.t.db,
            &RequestId::parse("req-attempt").expect("request id"),
            &Attempt {
                delivery,
                round,
                attempt,
                outcome,
            },
        )
        .await
        .expect("record attempt")
    }
}

fn failed(status: u16) -> AttemptOutcome {
    AttemptOutcome::Failed {
        status: Some(status),
        reason: "status",
        error_code: ErrorCode::WebhookAttemptFailed,
        duration_ms: 12,
    }
}

const OK: AttemptOutcome = AttemptOutcome::Succeeded {
    status: 204,
    duration_ms: 5,
};

#[tokio::test]
async fn create_list_and_get_a_webhook_with_its_audit_row() {
    let w = world().await;
    let id = w.create("req-hook-create").await;
    let listed = list(&w.t.db, w.project()).await.expect("list");
    assert_eq!(listed.len(), 1);
    let hook = &listed[0];
    assert_eq!(hook.id, id);
    assert_eq!(hook.owner, w.alice);
    assert_eq!(hook.origin, "https://kanflow.internal:443");
    assert_eq!(hook.event_types, vec!["*"]);
    assert!(hook.enabled);
    assert_eq!(hook.secret_version, 1);
    assert_eq!(hook.previous_secret_until, None);
    assert_eq!(
        get(&w.t.db, w.project(), id).await.expect("get").as_ref(),
        Some(hook)
    );
    // Another project's id is not found under this one.
    assert_eq!(
        get(&w.t.db, w.other.project_id, id).await.expect("get"),
        None
    );
    let audit = w
        .text(
            "SELECT action || ':' || resource_type || ':' || resource_id || ':' || \
             (details->>'origin') FROM forge.audit_log WHERE request_id = 'req-hook-create'",
        )
        .await;
    assert_eq!(
        audit,
        format!(
            "webhook.created:project:{}:https://kanflow.internal:443",
            w.project().get()
        )
    );
    // No event for a create (audit-only).
    assert_eq!(
        w.scalar_i64("SELECT count(*) FROM forge.events WHERE request_id = 'req-hook-create'")
            .await,
        0
    );
    w.t.drop_now().await;
}

#[tokio::test]
async fn the_twenty_first_webhook_is_int_0012() {
    let w = world().await;
    for i in 0..MAX_WEBHOOKS {
        w.create(&format!("req-hook-{i}")).await;
    }
    let e = create_decided(
        &w.t.db,
        &as_user(w.alice, "req-hook-21"),
        w.project(),
        w.alice,
        &new_hook("https://kanflow.internal/hook", &["*"]),
        |_| Ok(()),
    )
    .await
    .expect_err("limit");
    assert_reason(&e, ErrorCode::IntegrationLimitReached, "webhook_limit");
    // Another project has its own limit.
    create_decided(
        &w.t.db,
        &as_user(w.alice, "req-hook-other"),
        w.other.project_id,
        w.alice,
        &new_hook("https://kanflow.internal/hook", &["*"]),
        |_| Ok(()),
    )
    .await
    .expect("other project");
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_refused_decision_writes_nothing() {
    let w = world().await;
    let e = create_decided(
        &w.t.db,
        &as_user(w.alice, "req-hook-denied"),
        w.project(),
        w.alice,
        &new_hook("https://kanflow.internal/hook", &["*"]),
        |facts| {
            assert!(facts.account.active);
            Err::<(), _>(forge_errors::ForgeError::new(ErrorCode::Forbidden))
        },
    )
    .await
    .expect_err("denied");
    assert_eq!(e.code(), ErrorCode::Forbidden);
    assert_eq!(w.scalar_i64("SELECT count(*) FROM forge.webhooks").await, 0);
    w.t.drop_now().await;
}

#[tokio::test]
async fn update_records_its_fields_and_enabling_again_resets_the_failures() {
    let w = world().await;
    let id = w.create("req-hook").await;
    let (outcome, ()) = update_decided(
        &w.t.db,
        &as_user(w.alice, "req-hook-off"),
        (w.project(), id),
        w.alice,
        &WebhookPatch {
            enabled: Some(false),
            event_types: Some(vec!["merge_request.*".to_owned()]),
            ..WebhookPatch::default()
        },
        |_| Ok(()),
    )
    .await
    .expect("update");
    assert_eq!(
        outcome,
        UpdateOutcome::Updated(vec![WebhookField::EventTypes, WebhookField::Enabled])
    );
    let hook = get(&w.t.db, w.project(), id)
        .await
        .expect("get")
        .expect("hook");
    assert!(!hook.enabled);
    assert_eq!(hook.disabled_reason, Some(DisableReason::Manual));
    // The same values again: nothing written.
    let (again, ()) = update_decided(
        &w.t.db,
        &as_user(w.alice, "req-hook-same"),
        (w.project(), id),
        w.alice,
        &WebhookPatch {
            enabled: Some(false),
            ..WebhookPatch::default()
        },
        |_| Ok(()),
    )
    .await
    .expect("same");
    assert_eq!(again, UpdateOutcome::Unchanged);
    assert_eq!(
        w.scalar_i64("SELECT count(*) FROM forge.audit_log WHERE request_id = 'req-hook-same'")
            .await,
        0
    );
    // Failures counted while disabled are cleared by enabling it.
    w.t.exec_superuser(&format!(
        "UPDATE forge.webhooks SET consecutive_failures = 7, failing_since = now() WHERE id = {}",
        id.get()
    ))
    .await;
    let _ = update_decided(
        &w.t.db,
        &as_user(w.alice, "req-hook-on"),
        (w.project(), id),
        w.alice,
        &WebhookPatch {
            enabled: Some(true),
            ..WebhookPatch::default()
        },
        |_| Ok(()),
    )
    .await
    .expect("enable");
    let hook = get(&w.t.db, w.project(), id)
        .await
        .expect("get")
        .expect("hook");
    assert!(hook.enabled);
    assert_eq!(hook.disabled_reason, None);
    assert_eq!(hook.consecutive_failures, 0);
    assert_eq!(hook.failing_since, None);
    // Another project's webhook id is not found after the decision.
    let e = update_decided(
        &w.t.db,
        &as_user(w.alice, "req-hook-idor"),
        (w.other.project_id, id),
        w.alice,
        &WebhookPatch::default(),
        |_| Ok(()),
    )
    .await
    .expect_err("other project");
    assert_eq!(e.code(), ErrorCode::NotFound);
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_rotation_keeps_the_previous_secret_signing_for_24_hours() {
    let w = world().await;
    let id = w.create("req-hook").await;
    let before = delivery_task_of(&w, id).await;
    let (rotated, ()) = rotate_secret_decided(
        &w.t.db,
        &as_user(w.alice, "req-hook-rotate"),
        (w.project(), id),
        w.alice,
        |uid| keys().seal(uid, &WebhookSecret::generate().expect("secret")),
        |_| Ok(()),
    )
    .await
    .expect("rotate");
    assert_eq!(rotated.secret_version, 2);
    let hours = w
        .scalar_i64(&format!(
            "SELECT round(extract(epoch FROM secret_prev_until - now()) / 3600)::bigint \
             FROM forge.webhooks WHERE id = {}",
            id.get()
        ))
        .await;
    assert_eq!(hours, 24);
    let after = delivery_task_of(&w, id).await;
    assert_eq!(after.secret_version, 2);
    // The previous secret is the one before the rotation, and opens.
    let previous = after.previous_secret.as_ref().expect("previous signs");
    assert_eq!(previous, &before.secret);
    assert_ne!(after.secret, before.secret);
    let keys = keys();
    keys.open(&after.uid, previous.key_id, &previous.ciphertext)
        .expect("previous opens");
    keys.open(&after.uid, after.secret.key_id, &after.secret.ciphertext)
        .expect("current opens");
    // After the overlap it no longer signs.
    w.t.commit_audited(
        ("project", w.project().get()),
        &[format!(
            "UPDATE forge.webhooks SET secret_prev_until = now() - interval '1 second'              WHERE id = {}",
            id.get()
        )],
    )
    .await;
    assert_eq!(delivery_task_of(&w, id).await.previous_secret, None);
    let hook = get(&w.t.db, w.project(), id)
        .await
        .expect("get")
        .expect("hook");
    assert_eq!(hook.previous_secret_until, None);
    w.t.drop_now().await;
}

async fn delivery_task_of(w: &World, hook: WebhookId) -> DeliveryTask {
    let seq = w
        .scalar_i64("SELECT coalesce(max(event_seq), 0) + 1 FROM forge.webhook_deliveries")
        .await;
    let d = w.delivery(hook, seq).await;
    delivery_task(&w.t.db, d)
        .await
        .expect("task")
        .expect("present")
}

#[tokio::test]
async fn deleting_a_webhook_deletes_its_deliveries() {
    let w = world().await;
    let id = w.create("req-hook").await;
    let d = w.delivery(id, 1).await;
    delete_decided(
        &w.t.db,
        &as_user(w.alice, "req-hook-delete"),
        (w.project(), id),
        w.alice,
        |_| Ok(()),
    )
    .await
    .expect("delete");
    assert_eq!(list(&w.t.db, w.project()).await.expect("list"), Vec::new());
    assert!(delivery_task(&w.t.db, d).await.expect("task").is_none());
    assert_eq!(
        w.text("SELECT action FROM forge.audit_log WHERE request_id = 'req-hook-delete'")
            .await,
        "webhook.deleted"
    );
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_failed_attempt_queues_the_next_after_its_backoff() {
    let w = world().await;
    let id = w.create("req-hook").await;
    let d = w.delivery(id, 1).await;
    let r = w.attempt(d, 0, 1, failed(500)).await;
    assert_eq!(
        r.recorded,
        Recorded::Retrying {
            next_attempt: 2,
            retry_in: std::time::Duration::from_secs(10)
        }
    );
    assert_eq!(r.consecutive_failures, 1);
    assert_eq!(r.disabled, None);
    let (state, attempts, status): (String, i32, Option<i32>) = sqlx::query_as(
        "SELECT state, attempts, last_status FROM forge.webhook_deliveries WHERE id = $1",
    )
    .bind(d.get())
    .fetch_one(&mut w.t.superuser().await)
    .await
    .expect("row");
    assert_eq!(
        (state.as_str(), attempts, status),
        ("pending", 1, Some(500))
    );
    let wait = w
        .scalar_i64(&format!(
            "SELECT round(extract(epoch FROM run_after - now()))::bigint FROM forge.jobs \
             WHERE kind = 'webhook.deliver' AND dedup_key = 'delivery:{}:0:2'",
            d.get()
        ))
        .await;
    assert!((8..=10).contains(&wait), "{wait}");
    // A rerun of the same attempt's job adds no second job.
    assert_eq!(
        w.scalar_i64("SELECT count(*) FROM forge.jobs WHERE kind = 'webhook.deliver'")
            .await,
        1
    );
    w.t.drop_now().await;
}

#[tokio::test]
async fn the_last_attempt_fails_the_delivery() {
    let w = world().await;
    let id = w.create("req-hook").await;
    let d = w.delivery(id, 1).await;
    let r = w.attempt(d, 0, MAX_ATTEMPTS, failed(502)).await;
    assert_eq!(
        r.recorded,
        Recorded::Failed {
            attempts: MAX_ATTEMPTS
        }
    );
    assert_eq!(
        w.text(&format!(
            "SELECT state || ':' || last_reason || ':' || last_error_code \
             FROM forge.webhook_deliveries WHERE id = {}",
            d.get()
        ))
        .await,
        "failed:status:FG-INT-0020"
    );
    assert_eq!(
        w.scalar_i64("SELECT count(*) FROM forge.jobs WHERE kind = 'webhook.deliver'")
            .await,
        0
    );
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_success_resets_the_failure_count() {
    let w = world().await;
    let id = w.create("req-hook").await;
    for seq in 1..=3 {
        let d = w.delivery(id, seq).await;
        w.attempt(d, 0, MAX_ATTEMPTS, failed(500)).await;
    }
    let hook = get(&w.t.db, w.project(), id)
        .await
        .expect("get")
        .expect("hook");
    assert_eq!(hook.consecutive_failures, 3);
    assert!(hook.failing_since.is_some());
    let d = w.delivery(id, 4).await;
    let r = w.attempt(d, 0, 1, OK).await;
    assert_eq!(r.recorded, Recorded::Succeeded);
    let hook = get(&w.t.db, w.project(), id)
        .await
        .expect("get")
        .expect("hook");
    assert_eq!(hook.consecutive_failures, 0);
    assert_eq!(hook.failing_since, None);
    assert!(hook.last_delivery_at.is_some());
    w.t.drop_now().await;
}

#[tokio::test]
async fn fifty_consecutive_failures_disable_the_webhook() {
    let w = world().await;
    let id = w.create("req-hook").await;
    for seq in 1..i64::from(DISABLE_AFTER_FAILURES) {
        let d = w.delivery(id, seq).await;
        let r = w.attempt(d, 0, MAX_ATTEMPTS, failed(500)).await;
        assert_eq!(r.disabled, None, "{seq}");
    }
    assert!(
        get(&w.t.db, w.project(), id)
            .await
            .expect("get")
            .expect("hook")
            .enabled
    );
    let d = w.delivery(id, 50).await;
    let r = w.attempt(d, 0, 1, failed(500)).await;
    // The disabling attempt ends its delivery (no further attempt).
    assert_eq!(r.recorded, Recorded::Failed { attempts: 1 });
    let disabled = r.disabled.expect("disabled");
    assert_eq!(disabled.reason, DisableReason::Failures);
    assert_eq!(disabled.failures, 50);
    assert_eq!(disabled.project, w.project());
    let hook = get(&w.t.db, w.project(), id)
        .await
        .expect("get")
        .expect("hook");
    assert!(!hook.enabled);
    assert_eq!(hook.disabled_reason, Some(DisableReason::Failures));
    // The system's audit row and the event (the notification hook point).
    assert_eq!(
        w.text(
            "SELECT actor_kind || ':' || action || ':' || (details->>'consecutive_failures') \
             FROM forge.audit_log WHERE action = 'webhook.disabled'"
        )
        .await,
        "system:webhook.disabled:50"
    );
    assert_eq!(
        w.text(
            "SELECT event_type || ':' || actor_kind || ':' || (payload->>'reason') \
             FROM forge.events WHERE event_type = 'webhook.disabled'"
        )
        .await,
        "webhook.disabled:system:failures"
    );
    // A later failure of a delivery still pending ends it without a retry.
    let late = w.delivery(id, 51).await;
    let r = w.attempt(late, 0, 1, failed(500)).await;
    assert_eq!(r.recorded, Recorded::Failed { attempts: 1 });
    assert_eq!(r.disabled, None, "disabled once");
    w.t.drop_now().await;
}

#[tokio::test]
async fn failing_for_24_hours_disables_the_webhook() {
    let w = world().await;
    let id = w.create("req-hook").await;
    w.t.exec_superuser(&format!(
        "UPDATE forge.webhooks SET consecutive_failures = 3, \
         failing_since = now() - interval '25 hours' WHERE id = {}",
        id.get()
    ))
    .await;
    let d = w.delivery(id, 1).await;
    let r = w.attempt(d, 0, 1, failed(503)).await;
    let disabled = r.disabled.expect("disabled");
    assert_eq!(disabled.reason, DisableReason::Failing24h);
    assert_eq!(disabled.failures, 4);
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_stale_attempt_writes_nothing() {
    let w = world().await;
    let id = w.create("req-hook").await;
    let d = w.delivery(id, 1).await;
    // Another round than the row's.
    let r = w.attempt(d, 1, 1, failed(500)).await;
    assert_eq!(r.recorded, Recorded::Stale);
    // A finished delivery.
    w.attempt(d, 0, 1, OK).await;
    let r = w.attempt(d, 0, 2, failed(500)).await;
    assert_eq!(r.recorded, Recorded::Stale);
    let hook = get(&w.t.db, w.project(), id)
        .await
        .expect("get")
        .expect("hook");
    assert_eq!(hook.consecutive_failures, 0);
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_skipped_delivery_counts_no_failure() {
    let w = world().await;
    let id = w.create("req-hook").await;
    let d = w.delivery(id, 1).await;
    let r = w
        .attempt(
            d,
            0,
            1,
            AttemptOutcome::Skipped {
                reason: "owner_lost",
            },
        )
        .await;
    assert_eq!(r.recorded, Recorded::Skipped);
    assert_eq!(
        w.text(&format!(
            "SELECT state || ':' || last_reason FROM forge.webhook_deliveries WHERE id = {}",
            d.get()
        ))
        .await,
        "skipped:owner_lost"
    );
    let hook = get(&w.t.db, w.project(), id)
        .await
        .expect("get")
        .expect("hook");
    assert_eq!(hook.consecutive_failures, 0);
    w.t.drop_now().await;
}

#[tokio::test]
async fn redelivery_starts_a_new_round_and_is_rate_limited() {
    let w = world().await;
    let id = w.create("req-hook").await;
    let d = w.delivery(id, 1).await;
    let redeliver = |request: &'static str, delivery: WebhookDeliveryId, project: ProjectId| {
        let ctx = as_user(w.alice, request);
        let db = w.t.db.clone();
        let alice = w.alice;
        async move { redeliver_decided(&db, &ctx, (project, id, delivery), alice, |_| Ok(())).await }
    };
    // Still pending: refused.
    let e = redeliver("req-redeliver-1", d, w.project())
        .await
        .expect_err("pending");
    assert_reason(&e, ErrorCode::RateLimited, "delivery_pending");
    w.attempt(d, 0, MAX_ATTEMPTS, failed(500)).await;
    // Right after an attempt: refused.
    let e = redeliver("req-redeliver-2", d, w.project())
        .await
        .expect_err("interval");
    assert_reason(&e, ErrorCode::RateLimited, "redelivery_interval");
    w.t.exec_superuser(&format!(
        "UPDATE forge.webhook_deliveries SET last_attempt_at = now() - interval '2 minutes' \
         WHERE id = {}",
        d.get()
    ))
    .await;
    redeliver("req-redeliver-3", d, w.project())
        .await
        .expect("redeliver");
    assert_eq!(
        w.text(&format!(
            "SELECT state || ':' || redeliveries FROM forge.webhook_deliveries WHERE id = {}",
            d.get()
        ))
        .await,
        "pending:1"
    );
    assert_eq!(
        w.scalar_i64(&format!(
            "SELECT count(*) FROM forge.jobs WHERE dedup_key = 'delivery:{}:1:1'",
            d.get()
        ))
        .await,
        1
    );
    assert_eq!(
        w.text("SELECT action FROM forge.audit_log WHERE request_id = 'req-redeliver-3'")
            .await,
        "webhook.redelivered"
    );
    // The old round's late attempt is stale now.
    let r = w.attempt(d, 0, 2, failed(500)).await;
    assert_eq!(r.recorded, Recorded::Stale);
    // A delivery of another webhook, or under another project, is not found.
    let other = w.create("req-hook-2").await;
    let foreign = w.delivery(other, 9).await;
    let e = redeliver("req-redeliver-4", foreign, w.project())
        .await
        .expect_err("idor");
    assert_eq!(e.code(), ErrorCode::NotFound);
    let e = redeliver("req-redeliver-5", d, w.other.project_id)
        .await
        .expect_err("project");
    assert_eq!(e.code(), ErrorCode::NotFound);
    w.t.drop_now().await;
}

#[tokio::test]
async fn delivery_retention_deletes_finished_rows_only() {
    let w = world().await;
    let id = w.create("req-hook").await;
    let old = w.delivery(id, 1).await;
    let pending = w.delivery(id, 2).await;
    let recent = w.delivery(id, 3).await;
    w.attempt(old, 0, 1, OK).await;
    w.attempt(recent, 0, 1, OK).await;
    w.t.exec_superuser(&format!(
        "UPDATE forge.webhook_deliveries SET finished_at = now() - interval '31 days', \
         created_at = now() - interval '31 days' WHERE id = {}; \
         UPDATE forge.webhook_deliveries SET created_at = now() - interval '40 days' WHERE id = {}",
        old.get(),
        pending.get()
    ))
    .await;
    assert_eq!(
        purge_deliveries(&w.t.db, 30, RETENTION_BATCH)
            .await
            .expect("purge"),
        1
    );
    let left: Vec<i64> = sqlx::query_scalar("SELECT id FROM forge.webhook_deliveries ORDER BY id")
        .fetch_all(&mut w.t.superuser().await)
        .await
        .expect("left");
    assert_eq!(left, vec![pending.get(), recent.get()]);
    w.t.drop_now().await;
}

#[tokio::test]
async fn webhook_checks_refuse_raw_rows_by_constraint_name() {
    let w = world().await;
    let id = w.create("req-hook").await;
    let mut su = w.t.superuser().await;
    let insert = |url: &str, types: &str| {
        format!(
            "INSERT INTO forge.webhooks (uid, project_id, owner_user_id, url, origin, event_types, \
             secret_key_id, secret_ciphertext) VALUES (gen_random_uuid(), {}, {}, '{url}', \
             'https://x.example:443', {types}, 1, decode(repeat('00', 40), 'hex'))",
            w.project().get(),
            w.alice.get()
        )
    };
    for (sql, constraint) in [
        (
            insert("ftp://x.example/", "ARRAY['*']"),
            "webhooks_url_shape",
        ),
        (
            insert("https://u@x.example/", "ARRAY['*']"),
            "webhooks_url_shape",
        ),
        (
            insert("https://x.example/", "ARRAY['Bad Type']"),
            "webhooks_event_types_entries",
        ),
        (
            insert("https://x.example/", "ARRAY[]::text[]"),
            "webhooks_event_types_entries",
        ),
        (
            format!(
                "UPDATE forge.webhooks SET enabled = false WHERE id = {}",
                id.get()
            ),
            "webhooks_disabled_consistent",
        ),
        (
            format!(
                "UPDATE forge.webhooks SET consecutive_failures = 2 WHERE id = {}",
                id.get()
            ),
            "webhooks_failing_since",
        ),
        (
            format!(
                "UPDATE forge.webhooks SET secret_prev_key_id = 1 WHERE id = {}",
                id.get()
            ),
            "webhooks_previous_secret_complete",
        ),
    ] {
        let got = match sqlx::raw_sql(&sql).execute(&mut su).await {
            Err(sqlx::Error::Database(e)) => e.constraint().map(str::to_owned),
            other => panic!("{sql}: {other:?}"),
        };
        assert_eq!(got.as_deref(), Some(constraint), "{sql}");
    }
    let d = w.delivery(id, 1).await;
    for (sql, constraint) in [
        (
            format!(
                "UPDATE forge.webhook_deliveries SET state = 'failed' WHERE id = {}",
                d.get()
            ),
            "webhook_deliveries_finished",
        ),
        (
            format!(
                "UPDATE forge.webhook_deliveries SET state = 'skipped', finished_at = now() \
                 WHERE id = {}",
                d.get()
            ),
            "webhook_deliveries_terminal_reason",
        ),
        (
            format!(
                "UPDATE forge.webhook_deliveries SET last_reason = 'Free text' WHERE id = {}",
                d.get()
            ),
            "webhook_deliveries_last_reason",
        ),
        (
            format!(
                "INSERT INTO forge.webhook_deliveries (delivery_uid, webhook_id, project_id, \
                 event_seq, event_id, event_type, request_id) VALUES (gen_random_uuid(), {}, {}, \
                 1, gen_random_uuid(), 'merge_request.opened', 'r')",
                id.get(),
                w.project().get()
            ),
            "webhook_deliveries_once",
        ),
    ] {
        let got = match sqlx::raw_sql(&sql).execute(&mut su).await {
            Err(sqlx::Error::Database(e)) => e.constraint().map(str::to_owned),
            other => panic!("{sql}: {other:?}"),
        };
        assert_eq!(got.as_deref(), Some(constraint), "{sql}");
    }
    drop(su);
    w.t.drop_now().await;
}

#[tokio::test]
async fn the_read_pool_never_reads_a_sealed_secret_and_app_writes_are_audited() {
    let w = world().await;
    let id = w.create("req-hook").await;
    let mut su = w.t.superuser().await;
    sqlx::raw_sql("SET ROLE forge_ro")
        .execute(&mut su)
        .await
        .expect("role");
    for sql in [
        "SELECT secret_ciphertext FROM forge.webhooks",
        "SELECT secret_prev_ciphertext FROM forge.webhooks",
    ] {
        let state = match sqlx::raw_sql(sql).execute(&mut su).await {
            Err(sqlx::Error::Database(e)) => e.code().map(|c| c.into_owned()),
            other => panic!("{sql}: {other:?}"),
        };
        assert_eq!(state.as_deref(), Some("42501"), "{sql}");
    }
    sqlx::raw_sql("SELECT origin, event_types, secret_version FROM forge.webhooks")
        .execute(&mut su)
        .await
        .expect("the listed columns are readable");
    drop(su);
    // forge_app changing an audited column without an audit row fails at
    // COMMIT (FG-DB-0013's XF002).
    let mut tx = w.t.db.write_pool().begin().await.expect("begin");
    sqlx::query("UPDATE forge.webhooks SET url = 'https://evil.example/' WHERE id = $1")
        .bind(id.get())
        .execute(&mut *tx)
        .await
        .expect("update");
    let e = tx.commit().await.expect_err("unaudited");
    assert!(e.to_string().contains("audit"), "{e}");
    w.t.drop_now().await;
}

// ── review fixes (I2 review) ──

#[tokio::test]
async fn editing_the_target_or_rotating_makes_the_editor_the_owner() {
    let w = world().await;
    let id = w.create("req-hook").await;
    let bob = users::create(
        &w.t.db,
        &system("req-user-bob"),
        &NewUser::human("bob", "bob", "bob@example.test"),
    )
    .await
    .expect("bob")
    .user_id;
    // Enabling / disabling alone keeps the owner.
    let _ = update_decided(
        &w.t.db,
        &as_user(bob, "req-hook-off"),
        (w.project(), id),
        bob,
        &WebhookPatch {
            enabled: Some(false),
            ..WebhookPatch::default()
        },
        |_| Ok(()),
    )
    .await
    .expect("disable");
    let hook = get(&w.t.db, w.project(), id)
        .await
        .expect("get")
        .expect("hook");
    assert_eq!(hook.owner, w.alice);
    // A new event allowlist: bob answers for it now, audited.
    let (outcome, ()) = update_decided(
        &w.t.db,
        &as_user(bob, "req-hook-types"),
        (w.project(), id),
        bob,
        &WebhookPatch {
            event_types: Some(vec!["merge_request.*".to_owned()]),
            ..WebhookPatch::default()
        },
        |_| Ok(()),
    )
    .await
    .expect("types");
    assert_eq!(
        outcome,
        UpdateOutcome::Updated(vec![WebhookField::EventTypes, WebhookField::Owner])
    );
    let hook = get(&w.t.db, w.project(), id)
        .await
        .expect("get")
        .expect("hook");
    assert_eq!(hook.owner, bob);
    assert_eq!(
        w.text(
            "SELECT (details->>'owner_from') || ':' || (details->>'owner_id') \
             FROM forge.audit_log WHERE request_id = 'req-hook-types'"
        )
        .await,
        format!("{}:{}", w.alice.get(), bob.get())
    );
    // A rotation by alice makes her the owner again.
    let _ = rotate_secret_decided(
        &w.t.db,
        &as_user(w.alice, "req-hook-rotate"),
        (w.project(), id),
        w.alice,
        |uid| keys().seal(uid, &WebhookSecret::generate().expect("secret")),
        |_| Ok(()),
    )
    .await
    .expect("rotate");
    let hook = get(&w.t.db, w.project(), id)
        .await
        .expect("get")
        .expect("hook");
    assert_eq!(hook.owner, w.alice);
    assert_eq!(
        w.text(
            "SELECT details->>'owner_id' FROM forge.audit_log WHERE request_id = 'req-hook-rotate'"
        )
        .await,
        w.alice.get().to_string()
    );
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_pending_delivery_without_a_live_job_is_swept_failed() {
    let w = world().await;
    let id = w.create("req-hook").await;
    let orphan = w.delivery(id, 1).await;
    let queued = w.delivery(id, 2).await;
    let recent = w.delivery(id, 3).await;
    w.t.exec_superuser(&format!(
        "UPDATE forge.webhook_deliveries SET created_at = now() - interval '1 hour' \
         WHERE id IN ({}, {}); \
         INSERT INTO forge.jobs (kind, payload, state, max_attempts, timeout_ms, dedup_key, \
         origin_request_id) VALUES ('webhook.deliver', '{{}}', 'queued', 3, 30000, \
         'delivery:{}:0:1', 'req-j')",
        orphan.get(),
        queued.get(),
        queued.get()
    ))
    .await;
    let h = health(&w.t.db).await.expect("health");
    assert_eq!(h.stuck_pending, 1, "{h:?}");
    assert_eq!(
        sweep_stuck(&w.t.db, 100).await.expect("sweep"),
        vec![orphan]
    );
    assert_eq!(
        w.text(&format!(
            "SELECT state || ':' || last_reason || ':' || last_error_code \
             FROM forge.webhook_deliveries WHERE id = {}",
            orphan.get()
        ))
        .await,
        "failed:stuck:FG-INT-0004"
    );
    for live in [queued, recent] {
        assert_eq!(
            w.text(&format!(
                "SELECT state FROM forge.webhook_deliveries WHERE id = {}",
                live.get()
            ))
            .await,
            "pending"
        );
    }
    assert_eq!(health(&w.t.db).await.expect("health").stuck_pending, 0);
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_failed_job_fails_its_pending_delivery() {
    let w = world().await;
    let id = w.create("req-hook").await;
    let d = w.delivery(id, 1).await;
    assert!(fail_delivery(&w.t.db, d, Some(0), ErrorCode::DbQueryFailed)
        .await
        .expect("fail"));
    assert_eq!(
        w.text(&format!(
            "SELECT state || ':' || last_reason || ':' || last_error_code \
             FROM forge.webhook_deliveries WHERE id = {}",
            d.get()
        ))
        .await,
        "failed:internal:FG-DB-0002"
    );
    // Already finished: nothing changes.
    assert!(!fail_delivery(&w.t.db, d, None, ErrorCode::DbQueryFailed)
        .await
        .expect("again"));
    w.t.drop_now().await;
}

#[tokio::test]
async fn the_read_pool_never_reads_the_full_url() {
    let w = world().await;
    w.create("req-hook").await;
    let mut su = w.t.superuser().await;
    sqlx::raw_sql("SET ROLE forge_ro")
        .execute(&mut su)
        .await
        .expect("role");
    let state = match sqlx::raw_sql("SELECT url FROM forge.webhooks")
        .execute(&mut su)
        .await
    {
        Err(sqlx::Error::Database(e)) => e.code().map(|c| c.into_owned()),
        other => panic!("{other:?}"),
    };
    assert_eq!(state.as_deref(), Some("42501"));
    sqlx::raw_sql("SELECT origin FROM forge.webhooks")
        .execute(&mut su)
        .await
        .expect("origin is readable");
    drop(su);
    w.t.drop_now().await;
}
