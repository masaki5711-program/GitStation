//! Token commands against a migrated database. Inspection uses a superuser
//! connection (the read pool cannot see token hashes).

use std::time::Duration;

use forge_core::ids::UserId;
use forge_core::{RequestId, Scope, UserKind};
use forge_errors::ErrorCode;
use sqlx::postgres::PgConnection;

use super::*;
use crate::audited_tests::audit;
use crate::change::Actor;
use crate::test_support::*;
use crate::users::{self, AuthSource, NewUser};

mod auth;
mod request;
mod revoke;
mod schema;

pub(super) fn ctx(request: &str) -> ChangeContext {
    ChangeContext::new(
        Actor::System,
        RequestId::parse(request).expect("request id"),
    )
}

pub(super) fn rid(request: &str) -> RequestId {
    RequestId::parse(request).expect("request id")
}

pub(super) async fn human(t: &FreshDb, name: &str, is_admin: bool) -> UserId {
    let new = NewUser {
        is_admin,
        ..NewUser::human(name, "Someone", &format!("{name}@example.test"))
    };
    users::create(&t.db, &ctx(&format!("req-user-{name}")), &new)
        .await
        .expect("create user")
        .user_id
}

pub(super) async fn bot(t: &FreshDb, name: &str) -> UserId {
    let new = NewUser {
        kind: UserKind::Bot,
        username: name.to_owned(),
        display_name: "Bot".to_owned(),
        email: None,
        employee_number: None,
        external_id: None,
        auth_source: AuthSource::Local,
        is_admin: false,
    };
    users::create(&t.db, &ctx(&format!("req-user-{name}")), &new)
        .await
        .expect("create bot")
        .user_id
}

pub(super) fn request(user: UserId, scopes: &[Scope]) -> NewToken {
    NewToken {
        user,
        name: "ci".to_owned(),
        scopes: scopes.to_vec(),
        expires_in: None,
        is_agent: false,
    }
}

/// The events of `event_type`: (aggregate_type, aggregate_id, actor_kind, payload).
pub(super) async fn events_of(
    su: &mut PgConnection,
    event_type: &str,
) -> Vec<(String, i64, String, serde_json::Value)> {
    sqlx::query_as(
        "SELECT aggregate_type, aggregate_id, actor_kind, payload FROM forge.events \n         WHERE event_type = $1 ORDER BY id",
    )
    .bind(event_type)
    .fetch_all(su)
    .await
    .expect("events")
}

pub(super) async fn issue_ok(t: &FreshDb, req: &str, user: UserId, scopes: &[Scope]) -> Issued {
    issue(&t.db, &ctx(req), &request(user, scopes))
        .await
        .expect("issue")
}

pub(super) async fn count(su: &mut PgConnection, sql: &str) -> i64 {
    sqlx::query_scalar(sql)
        .fetch_one(su)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
}

pub(super) async fn audit_rows(
    su: &mut PgConnection,
    request: &str,
) -> Vec<(String, String, String, i64)> {
    sqlx::query_as(
        "SELECT action, outcome, resource_type, resource_id FROM forge.audit_log \
         WHERE request_id = $1 ORDER BY id",
    )
    .bind(request)
    .fetch_all(su)
    .await
    .expect("audit rows")
}

/// A superuser change of audited columns, with its audit row in the same
/// transaction (a simple-protocol batch is one transaction).
pub(super) async fn su_change(su: &mut PgConnection, user: UserId, change: &str) {
    let batch = format!("{}; {change}", audit("user", user.get(), "succeeded"));
    sqlx::raw_sql(&batch)
        .execute(su)
        .await
        .unwrap_or_else(|e| panic!("{change}: {e}"));
}

/// `n` active tokens for `user`, inserted directly (fast path for the limit
/// tests), with the audit row they need.
pub(super) async fn seed_active(su: &mut PgConnection, user: UserId, n: i64) {
    su_change(
        su,
        user,
        &format!(
            "INSERT INTO forge.tokens (user_id, public_id, secret_sha256, name, scopes, expires_at) \
             SELECT {}, lpad(to_hex(i), 16, '0'), sha256(i::text::bytea), 'seed', \
             ARRAY['read_api'], now() + interval '30 days' FROM generate_series(1, {n}) AS i",
            user.get()
        ),
    )
    .await;
}

pub(super) async fn active_count(su: &mut PgConnection, user: UserId) -> i64 {
    count(
        su,
        &format!(
            "SELECT count(*) FROM forge.tokens WHERE user_id = {} AND revoked_at IS NULL \
             AND expires_at > now()",
            user.get()
        ),
    )
    .await
}

fn secret_hex(issued: &Issued) -> String {
    issued
        .token
        .reveal()
        .rsplit('_')
        .next()
        .expect("secret part")
        .to_owned()
}

#[tokio::test]
async fn issue_writes_one_token_one_audit_row_and_one_token_created_event() {
    let t = FreshDb::migrated().await;
    let alice = human(&t, "alice", false).await;
    let issued = issue_ok(
        &t,
        "req-issue-1",
        alice,
        &[Scope::ReadApi, Scope::ReadRepository],
    )
    .await;
    let mut su = t.superuser().await;

    assert_eq!(count(&mut su, "SELECT count(*) FROM forge.tokens").await, 1);
    assert_eq!(
        audit_rows(&mut su, "req-issue-1").await,
        vec![(
            "token.created".to_owned(),
            "succeeded".to_owned(),
            "user".to_owned(),
            alice.get()
        )]
    );
    assert_eq!(count(&mut su, "SELECT count(*) FROM forge.events").await, 1);
    let events = events_of(&mut su, "token.created").await;
    assert_eq!(events.len(), 1, "{events:?}");
    let (aggregate, aggregate_id, actor, payload) = &events[0];
    assert_eq!(
        (aggregate.as_str(), *aggregate_id),
        ("token", issued.token_id)
    );
    assert_eq!(actor, "system");
    assert_eq!(payload["token_id"], issued.token_id);
    assert_eq!(payload["user_id"], alice.get());
    assert_eq!(payload["public_id"], issued.token.public_id().as_str());
    assert_eq!(
        payload["scopes"],
        serde_json::json!(["read_api", "read_repository"])
    );
    assert_eq!(payload["is_agent"], false);
    assert!(payload["expires_at"].is_string(), "{payload}");
    assert_eq!(payload.as_object().expect("object").len(), 6, "{payload}");
    let details: serde_json::Value =
        sqlx::query_scalar("SELECT details FROM forge.audit_log WHERE action = 'token.created'")
            .fetch_one(&mut su)
            .await
            .expect("details");
    assert_eq!(details["token_id"], issued.token_id);
    assert_eq!(details["user_id"], alice.get());
    assert_eq!(details["public_id"], issued.token.public_id().as_str());
    assert_eq!(
        details["scopes"],
        serde_json::json!(["read_api", "read_repository"])
    );
    assert!(details["expires_at"].is_string(), "{details}");
    assert_eq!(
        details.as_object().expect("object").len(),
        6,
        "ids, public_id, scopes, expires_at, is_agent only: {details}"
    );
    t.drop_now().await;
}

#[tokio::test]
async fn only_the_sha256_of_the_secret_is_stored() {
    let t = FreshDb::migrated().await;
    let alice = human(&t, "alice", false).await;
    let issued = issue_ok(&t, "req-hash", alice, &[Scope::ReadApi]).await;
    let mut su = t.superuser().await;
    let stored: Vec<u8> = sqlx::query_scalar("SELECT secret_sha256 FROM forge.tokens")
        .fetch_one(&mut su)
        .await
        .expect("hash");
    assert_eq!(
        stored,
        forge_core::token::sha256(issued.token.secret()).to_vec()
    );
    let row: String = sqlx::query_scalar("SELECT to_jsonb(t)::text FROM forge.tokens t")
        .fetch_one(&mut su)
        .await
        .expect("row");
    assert!(
        !row.contains(&secret_hex(&issued)),
        "the secret is stored nowhere"
    );
    t.drop_now().await;
}

#[tokio::test]
async fn no_token_like_string_reaches_audit_events_or_jobs() {
    let t = FreshDb::migrated().await;
    let alice = human(&t, "alice", false).await;
    let issued = issue_ok(&t, "req-leak-1", alice, &[Scope::ReadApi]).await;
    let revealed = issued.token.reveal();
    let secret = secret_hex(&issued);
    let hash: String = forge_core::token::sha256(issued.token.secret())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    authenticate(&t.db, &rid("req-leak-2"), &revealed)
        .await
        .expect("authenticate");
    assert_eq!(
        revoke(&t.db, &ctx("req-leak-3"), issued.token_id)
            .await
            .expect("revoke"),
        RevokeOutcome::Revoked
    );
    let mut su = t.superuser().await;
    for table in ["audit_log", "events", "jobs"] {
        let all: String = sqlx::query_scalar(&format!(
            "SELECT coalesce(string_agg(to_jsonb(x)::text, ' '), '') FROM forge.{table} x"
        ))
        .fetch_one(&mut su)
        .await
        .expect("table text");
        assert!(!all.contains("fgp_"), "{table} holds a token-like string");
        assert!(!all.contains(&secret), "{table} holds the secret");
        assert!(!all.contains(&hash), "{table} holds the hash");
    }
    t.drop_now().await;
}

#[tokio::test]
async fn expiry_defaults_to_90_days() {
    let t = FreshDb::migrated().await;
    let alice = human(&t, "alice", false).await;
    let default = issue_ok(&t, "req-exp-1", alice, &[Scope::ReadApi]).await;
    let mut named = request(alice, &[Scope::ReadApi]);
    named.expires_in = Some(Duration::from_secs(30 * 24 * 3600));
    let named = issue(&t.db, &ctx("req-exp-2"), &named)
        .await
        .expect("30 days");
    let mut su = t.superuser().await;
    let sql = "SELECT extract(epoch FROM expires_at - created_at)::float8 \
               FROM forge.tokens WHERE id = $1";
    let a: f64 = sqlx::query_scalar(sql)
        .bind(default.token_id)
        .fetch_one(&mut su)
        .await
        .expect("default expiry");
    let b: f64 = sqlx::query_scalar(sql)
        .bind(named.token_id)
        .fetch_one(&mut su)
        .await
        .expect("named expiry");
    assert_eq!(a, 90.0 * 86400.0);
    assert_eq!(b, 30.0 * 86400.0);
    t.drop_now().await;
}

#[tokio::test]
async fn the_51st_active_token_is_active_limit() {
    let t = FreshDb::migrated().await;
    let alice = human(&t, "alice", false).await;
    let mut su = t.superuser().await;
    seed_active(&mut su, alice, 49).await;
    issue_ok(&t, "req-lim-50", alice, &[Scope::ReadApi]).await;
    let err = issue(
        &t.db,
        &ctx("req-lim-51"),
        &request(alice, &[Scope::ReadApi]),
    )
    .await
    .expect_err("51st");
    assert_reason(&err, ErrorCode::TokenRequestInvalid, "active_limit");
    assert_eq!(active_count(&mut su, alice).await, 50);
    assert!(audit_rows(&mut su, "req-lim-51").await.is_empty());
    // Another user is not affected.
    let bob = human(&t, "bob", false).await;
    issue_ok(&t, "req-lim-bob", bob, &[Scope::ReadApi]).await;
    t.drop_now().await;
}

#[tokio::test]
async fn expired_and_revoked_tokens_do_not_count_toward_the_limit() {
    let t = FreshDb::migrated().await;
    let alice = human(&t, "alice", false).await;
    let mut su = t.superuser().await;
    seed_active(&mut su, alice, 50).await;
    su_change(
        &mut su,
        alice,
        "UPDATE forge.tokens SET created_at = now() - interval '10 days', \
         expires_at = now() - interval '1 day' WHERE public_id = lpad(to_hex(1), 16, '0')",
    )
    .await;
    su_change(
        &mut su,
        alice,
        "UPDATE forge.tokens SET revoked_at = now(), revoked_reason = 'revoked' \
         WHERE public_id = lpad(to_hex(2), 16, '0')",
    )
    .await;
    assert_eq!(active_count(&mut su, alice).await, 48);
    issue_ok(&t, "req-free-1", alice, &[Scope::ReadApi]).await;
    issue_ok(&t, "req-free-2", alice, &[Scope::ReadApi]).await;
    let err = issue(
        &t.db,
        &ctx("req-free-3"),
        &request(alice, &[Scope::ReadApi]),
    )
    .await
    .expect_err("limit again");
    assert_reason(&err, ErrorCode::TokenRequestInvalid, "active_limit");
    t.drop_now().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_issues_never_exceed_50_active_tokens() {
    let t = FreshDb::migrated().await;
    let alice = human(&t, "alice", false).await;
    let mut su = t.superuser().await;
    seed_active(&mut su, alice, 40).await;
    let mut tasks = Vec::new();
    for i in 0..20 {
        let db = t.db.clone();
        tasks.push(tokio::spawn(async move {
            issue(
                &db,
                &ctx(&format!("req-conc-{i}")),
                &request(alice, &[Scope::ReadApi]),
            )
            .await
        }));
    }
    let (mut ok, mut limited) = (0, 0);
    for task in tasks {
        match task.await.expect("task") {
            Ok(_) => ok += 1,
            Err(e) => {
                assert_reason(&e, ErrorCode::TokenRequestInvalid, "active_limit");
                limited += 1;
            }
        }
    }
    assert_eq!((ok, limited), (10, 10));
    assert_eq!(active_count(&mut su, alice).await, 50);
    t.drop_now().await;
}
