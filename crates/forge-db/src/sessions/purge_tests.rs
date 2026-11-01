//! M2 MR6 against a migrated database: the session list and owner reads,
//! and the purge of sessions 30 days past their end — one `session.purged`
//! audit row per owner, the guard that refuses a younger row for every
//! role, the batch bound.

use forge_core::ids::UserId;
use forge_core::{RequestId, SessionRevokeReason};
use forge_errors::ErrorCode;

use super::*;
use crate::audited_tests::{audit, commit, sql};
use crate::change::{Actor, ChangeContext};
use crate::sessions::{issue, list, owner, revoke, NewSession, RevokeOutcome};
use crate::test_support::*;
use crate::users::{self, NewUser};

fn ctx(request: &str) -> ChangeContext {
    ChangeContext::new(
        Actor::System,
        RequestId::parse(request).expect("request id"),
    )
}

async fn human(t: &FreshDb, name: &str) -> UserId {
    users::create(
        &t.db,
        &ctx(&format!("req-user-{name}")),
        &NewUser::human(name, "Someone", &format!("{name}@example.test")),
    )
    .await
    .expect("create user")
    .user_id
}

fn new_session(user: UserId) -> NewSession {
    NewSession {
        user,
        idle: std::time::Duration::from_secs(4 * 3600),
        absolute: std::time::Duration::from_secs(12 * 3600),
        oidc: None,
    }
}

/// A synthetic session of `user` that began `days_ago` days ago (12 h
/// absolute lifetime), revoked `revoked_days_ago` days ago when given.
fn old_session(public_id: &str, user: UserId, days_ago: i32, revoked: Option<i32>) -> String {
    let revoked = match revoked {
        Some(d) => format!("now() - interval '{d} days', 'logout'"),
        None => "NULL, NULL".to_owned(),
    };
    format!(
        "INSERT INTO forge.web_sessions (public_id, secret_sha256, user_id, created_at, \
         last_seen_at, idle_expires_at, absolute_expires_at, revoked_at, revoked_reason) \
         VALUES ('{public_id}', decode(repeat('ab', 32), 'hex'), {user}, \
         now() - interval '{days_ago} days', now() - interval '{days_ago} days', \
         now() - interval '{days_ago} days' + interval '4 hours', \
         now() - interval '{days_ago} days' + interval '12 hours', {revoked})",
        user = user.get()
    )
}

async fn seed_old(t: &FreshDb, user: UserId, rows: &[String]) {
    let mut stmts = vec![audit("user", user.get(), "succeeded")];
    stmts.extend(rows.iter().cloned());
    commit(t, &stmts).await.expect("seed old sessions");
}

async fn remaining(t: &FreshDb) -> Vec<String> {
    t.json_rows("SELECT public_id FROM forge.web_sessions ORDER BY public_id")
        .await
        .into_iter()
        .filter_map(|v| v["public_id"].as_str().map(str::to_owned))
        .collect()
}

#[tokio::test]
async fn the_purge_deletes_sessions_30_days_after_they_ended_with_one_audit_row_per_owner() {
    let t = FreshDb::migrated().await;
    let alice = human(&t, "alice").await;
    let bob = human(&t, "bob").await;
    seed_old(
        &t,
        alice,
        &[
            // expired 40 days ago, revoked 31 days ago: both old
            old_session("000000000000a001", alice, 40, None),
            old_session("000000000000a002", alice, 35, Some(31)),
            // expired 20 days ago: kept
            old_session("000000000000a003", alice, 20, None),
        ],
    )
    .await;
    seed_old(&t, bob, &[old_session("000000000000b001", bob, 45, None)]).await;
    // A live session and one revoked just now are kept.
    let live = issue(&t.db, &ctx("req-live"), &new_session(bob))
        .await
        .expect("issue");
    let fresh = issue(&t.db, &ctx("req-fresh"), &new_session(alice))
        .await
        .expect("issue");
    let ended = revoke(
        &t.db,
        &ctx("req-revoke"),
        fresh.session_id,
        SessionRevokeReason::Logout,
    )
    .await
    .expect("revoke");
    assert_eq!(ended, RevokeOutcome::Revoked);

    let purged = purge_expired(&t.db, &ctx("req-purge-1"), MAX_BATCH)
        .await
        .expect("purge");
    assert_eq!(
        purged,
        Purged {
            sessions: 3,
            users: 2
        }
    );
    let left = remaining(&t).await;
    assert_eq!(left.len(), 3, "{left:?}");
    assert!(left.contains(&"000000000000a003".to_owned()));
    assert!(left.contains(&live.token.public_id().as_str().to_owned()));
    assert!(left.contains(&fresh.token.public_id().as_str().to_owned()));
    let rows = t
        .json_rows(
            "SELECT resource_type, resource_id, actor_kind, details FROM forge.audit_log \
             WHERE action = 'session.purged' AND request_id = 'req-purge-1' \
             ORDER BY resource_id",
        )
        .await;
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(rows[0]["resource_type"], "user");
    assert_eq!(rows[0]["resource_id"], alice.get());
    assert_eq!(rows[0]["actor_kind"], "system");
    assert_eq!(
        rows[0]["details"],
        serde_json::json!({"user_id": alice.get(), "purged_sessions": 2})
    );
    assert_eq!(
        rows[1]["details"],
        serde_json::json!({"user_id": bob.get(), "purged_sessions": 1})
    );
    // Nothing left to purge: nothing is written.
    let again = purge_expired(&t.db, &ctx("req-purge-2"), MAX_BATCH)
        .await
        .expect("purge again");
    assert_eq!(again, Purged::default());
    let none = t
        .json_rows("SELECT id FROM forge.audit_log WHERE request_id = 'req-purge-2'")
        .await;
    assert!(none.is_empty(), "{none:?}");
    t.drop_now().await;
}

#[tokio::test]
async fn the_purge_deletes_at_most_one_batch_oldest_first() {
    let t = FreshDb::migrated().await;
    let alice = human(&t, "alice").await;
    seed_old(
        &t,
        alice,
        &[
            old_session("000000000000c001", alice, 50, None),
            old_session("000000000000c002", alice, 50, None),
            old_session("000000000000c003", alice, 50, None),
        ],
    )
    .await;
    let first = purge_expired(&t.db, &ctx("req-batch-1"), 2)
        .await
        .expect("purge");
    assert_eq!(first.sessions, 2);
    assert_eq!(remaining(&t).await, ["000000000000c003"]);
    let second = purge_expired(&t.db, &ctx("req-batch-2"), 2)
        .await
        .expect("purge");
    assert_eq!(second.sessions, 1);
    for batch in [0, MAX_BATCH + 1] {
        let err = purge_expired(&t.db, &ctx("req-batch-3"), batch)
            .await
            .expect_err("out of range");
        assert_reason(&err, ErrorCode::Internal, "purge_batch_out_of_range");
    }
    t.drop_now().await;
}

/// Migration 20261104100000: the DELETE grant is for old rows only, for
/// every role (the guard is enabled ALWAYS).
#[tokio::test]
async fn a_session_that_ended_less_than_30_days_ago_cannot_be_deleted() {
    let t = FreshDb::migrated().await;
    let alice = human(&t, "alice").await;
    seed_old(
        &t,
        alice,
        &[
            old_session("000000000000d001", alice, 10, None),
            old_session("000000000000d002", alice, 20, Some(10)),
        ],
    )
    .await;
    issue(&t.db, &ctx("req-live"), &new_session(alice))
        .await
        .expect("issue");
    for public_id in ["000000000000d001", "000000000000d002"] {
        let err = commit(
            &t,
            &[
                audit("user", alice.get(), "succeeded"),
                sql(&format!(
                    "DELETE FROM forge.web_sessions WHERE public_id = '{public_id}'"
                )),
            ],
        )
        .await
        .expect_err(public_id);
        assert!(chain(&err).contains("30 days after it ended"), "{err}");
    }
    let err = commit(
        &t,
        &[
            audit("user", alice.get(), "succeeded"),
            sql("DELETE FROM forge.web_sessions WHERE revoked_at IS NULL"),
        ],
    )
    .await
    .expect_err("live session");
    assert!(chain(&err).contains("30 days after it ended"), "{err}");
    // Not even a superuser deletes a live session.
    let mut su = t.superuser().await;
    let err = sqlx::raw_sql(&format!(
        "{}; DELETE FROM forge.web_sessions",
        audit("user", alice.get(), "succeeded")
    ))
    .execute(&mut su)
    .await
    .expect_err("superuser");
    assert!(err.to_string().contains("30 days after it ended"), "{err}");
    assert_eq!(remaining(&t).await.len(), 3);
    // An old row without its owner's audit row is refused by the audit trigger.
    seed_old(
        &t,
        alice,
        &[old_session("000000000000d003", alice, 90, None)],
    )
    .await;
    let err = commit(
        &t,
        &[sql(
            "DELETE FROM forge.web_sessions WHERE public_id = '000000000000d003'",
        )],
    )
    .await
    .expect_err("no audit row");
    assert_reason(&err, ErrorCode::AuditMissing, "audited_change");
    t.drop_now().await;
}

#[tokio::test]
async fn the_list_shows_only_active_sessions_and_the_owner_read_names_the_user() {
    let t = FreshDb::migrated().await;
    let alice = human(&t, "alice").await;
    let bob = human(&t, "bob").await;
    let a1 = issue(&t.db, &ctx("req-a1"), &new_session(alice))
        .await
        .expect("issue");
    let a2 = issue(&t.db, &ctx("req-a2"), &new_session(alice))
        .await
        .expect("issue");
    let b1 = issue(&t.db, &ctx("req-b1"), &new_session(bob))
        .await
        .expect("issue");
    let _ = revoke(
        &t.db,
        &ctx("req-r"),
        a2.session_id,
        SessionRevokeReason::Logout,
    )
    .await
    .expect("revoke");
    seed_old(
        &t,
        alice,
        &[old_session("000000000000e001", alice, 3, None)],
    )
    .await;
    let listed = list(&t.db, alice).await.expect("list");
    assert!(!listed.truncated);
    let ids: Vec<&str> = listed
        .sessions
        .iter()
        .map(|s| s.public_id.as_str())
        .collect();
    assert_eq!(ids, [a1.token.public_id().as_str()]);
    assert!(!listed.sessions[0].oidc);
    assert!(listed.sessions[0].created_at.ends_with('Z'));
    let found = owner(&t.db, b1.token.public_id().as_str())
        .await
        .expect("owner");
    assert_eq!(found, Some((b1.session_id, bob)));
    assert_eq!(
        owner(&t.db, "0000000000000000").await.expect("absent"),
        None
    );
    let err = owner(&t.db, "not-a-public-id")
        .await
        .expect_err("unchecked");
    assert_reason(&err, ErrorCode::Internal, "session_public_id_unchecked");
    t.drop_now().await;
}
