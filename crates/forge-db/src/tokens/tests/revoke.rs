//! tokens::revoke and tokens::list, and the bulk revoke of users::disable.

use super::*;

#[tokio::test]
async fn revoke_writes_one_audit_row_one_token_revoked_event_and_the_token_stops_authenticating() {
    let t = FreshDb::migrated().await;
    let alice = human(&t, "alice", false).await;
    let issued = issue_ok(&t, "req-rev-1", alice, &[Scope::ReadApi]).await;
    let presented = issued.token.reveal();
    authenticate(&t.db, &rid("req-rev-2"), &presented)
        .await
        .expect("valid before");
    assert_eq!(
        revoke(&t.db, &ctx("req-rev-3"), issued.token_id)
            .await
            .expect("revoke"),
        RevokeOutcome::Revoked
    );
    let mut su = t.superuser().await;
    assert_eq!(
        audit_rows(&mut su, "req-rev-3").await,
        vec![(
            "token.revoked".to_owned(),
            "succeeded".to_owned(),
            "user".to_owned(),
            alice.get()
        )]
    );
    let reason: String = sqlx::query_scalar("SELECT revoked_reason FROM forge.tokens")
        .fetch_one(&mut su)
        .await
        .expect("reason");
    assert_eq!(reason, "revoked");
    let events = events_of(&mut su, "token.revoked").await;
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0].0, "token");
    assert_eq!(events[0].1, issued.token_id);
    assert_eq!(events[0].3["token_id"], issued.token_id);
    assert_eq!(events[0].3["user_id"], alice.get());
    assert_eq!(events[0].3["public_id"], issued.token.public_id().as_str());
    let err = authenticate(&t.db, &rid("req-rev-4"), &presented)
        .await
        .expect_err("revoked");
    assert_reason(&err, ErrorCode::Unauthenticated, "revoked");
    t.drop_now().await;
}

#[tokio::test]
async fn revoking_twice_writes_nothing() {
    let t = FreshDb::migrated().await;
    let alice = human(&t, "alice", false).await;
    let issued = issue_ok(&t, "req-twice-1", alice, &[Scope::ReadApi]).await;
    assert_eq!(
        revoke(&t.db, &ctx("req-twice-2"), issued.token_id)
            .await
            .expect("first"),
        RevokeOutcome::Revoked
    );
    let mut su = t.superuser().await;
    let first: String = sqlx::query_scalar("SELECT revoked_at::text FROM forge.tokens")
        .fetch_one(&mut su)
        .await
        .expect("revoked_at");
    assert_eq!(
        revoke(&t.db, &ctx("req-twice-3"), issued.token_id)
            .await
            .expect("second"),
        RevokeOutcome::AlreadyRevoked
    );
    assert!(audit_rows(&mut su, "req-twice-3").await.is_empty());
    assert_eq!(
        events_of(&mut su, "token.revoked").await.len(),
        1,
        "no second event"
    );
    let again: String = sqlx::query_scalar("SELECT revoked_at::text FROM forge.tokens")
        .fetch_one(&mut su)
        .await
        .expect("revoked_at");
    assert_eq!(first, again);
    t.drop_now().await;
}

#[tokio::test]
async fn revoking_an_unknown_token_is_not_found() {
    let t = FreshDb::migrated().await;
    let err = revoke(&t.db, &ctx("req-unknown"), 424242)
        .await
        .expect_err("unknown");
    assert_eq!(err.code(), ErrorCode::NotFound, "{err}");
    assert!(err.to_string().contains("resource=token"), "{err}");
    assert!(err.to_string().contains("reason=absent"), "{err}");
    t.drop_now().await;
}

#[tokio::test]
async fn list_shows_the_owners_tokens_without_secrets() {
    let t = FreshDb::migrated().await;
    let alice = human(&t, "alice", false).await;
    let bob = human(&t, "bob", false).await;
    let a = issue_ok(&t, "req-list-1", alice, &[Scope::ReadApi]).await;
    issue_ok(&t, "req-list-2", bob, &[Scope::Api]).await;
    let listed = list(&t.db, alice).await.expect("list");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].token_id, a.token_id);
    assert_eq!(listed[0].public_id, a.token.public_id().as_str());
    assert_eq!(listed[0].scopes, vec![Scope::ReadApi]);
    assert!(!format!("{listed:?}").contains(&a.token.reveal()));
    t.drop_now().await;
}

#[tokio::test]
async fn list_with_an_unknown_stored_scope_is_credential_data_invalid() {
    let t = FreshDb::migrated().await;
    let alice = human(&t, "alice", false).await;
    let issued = issue_ok(&t, "req-list-bad-1", alice, &[Scope::ReadApi]).await;
    let mut su = t.superuser().await;
    sqlx::query("ALTER TABLE forge.tokens DROP CONSTRAINT tokens_scopes_known")
        .execute(&mut su)
        .await
        .expect("drop constraint");
    su_change(
        &mut su,
        alice,
        "UPDATE forge.tokens SET scopes = ARRAY['teleport']",
    )
    .await;
    let err = list(&t.db, alice).await.expect_err("unknown scope");
    assert_reason(&err, ErrorCode::CredentialDataInvalid, "unknown_scope");
    assert!(
        chain(&err).contains(issued.token.public_id().as_str()),
        "the row is named by its public id: {err}"
    );
    t.drop_now().await;
}

#[tokio::test]
async fn disable_revokes_every_active_token_in_the_same_transaction() {
    let t = FreshDb::migrated().await;
    let alice = human(&t, "alice", false).await;
    let bob = human(&t, "bob", false).await;
    let a1 = issue_ok(&t, "req-dis-1", alice, &[Scope::ReadApi]).await;
    let a2 = issue_ok(&t, "req-dis-2", alice, &[Scope::Api]).await;
    let a3 = issue_ok(&t, "req-dis-3", alice, &[Scope::ReadApi]).await;
    assert_eq!(
        revoke(&t.db, &ctx("req-dis-4"), a3.token_id)
            .await
            .expect("revoke"),
        RevokeOutcome::Revoked
    );
    let b1 = issue_ok(&t, "req-dis-5", bob, &[Scope::ReadApi]).await;
    let _ = users::disable(&t.db, &ctx("req-dis-6"), alice)
        .await
        .expect("disable");
    let mut su = t.superuser().await;
    let reasons: Vec<(i64, Option<String>)> =
        sqlx::query_as("SELECT id, revoked_reason FROM forge.tokens ORDER BY id")
            .fetch_all(&mut su)
            .await
            .expect("reasons");
    assert_eq!(
        reasons,
        vec![
            (a1.token_id, Some("user_disabled".to_owned())),
            (a2.token_id, Some("user_disabled".to_owned())),
            (a3.token_id, Some("revoked".to_owned())),
            (b1.token_id, None),
        ],
        "only the disabled user's active tokens"
    );
    // One audit row for the whole change, counting the revoked tokens.
    assert_eq!(
        audit_rows(&mut su, "req-dis-6").await,
        vec![(
            "user.disabled".to_owned(),
            "succeeded".to_owned(),
            "user".to_owned(),
            alice.get()
        )]
    );
    let details: serde_json::Value =
        sqlx::query_scalar("SELECT details FROM forge.audit_log WHERE request_id = 'req-dis-6'")
            .fetch_one(&mut su)
            .await
            .expect("details");
    assert_eq!(details["revoked_tokens"], 2);
    // One token.revoked event per revoked token, in the same request.
    let revoked: Vec<i64> = sqlx::query_scalar(
        "SELECT aggregate_id FROM forge.events WHERE event_type = 'token.revoked' \
         AND request_id = 'req-dis-6' ORDER BY aggregate_id",
    )
    .fetch_all(&mut su)
    .await
    .expect("events");
    assert_eq!(revoked, vec![a1.token_id, a2.token_id]);
    // The revoked tokens are rejected as revoked; bob's still works.
    let err = authenticate(&t.db, &rid("req-dis-7"), &a1.token.reveal())
        .await
        .expect_err("revoked");
    assert_reason(&err, ErrorCode::Unauthenticated, "revoked");
    authenticate(&t.db, &rid("req-dis-8"), &b1.token.reveal())
        .await
        .expect("bob unaffected");
    t.drop_now().await;
}

#[tokio::test]
async fn a_disable_whose_token_revoke_fails_changes_nothing() {
    let t = FreshDb::migrated().await;
    let alice = human(&t, "alice", false).await;
    let issued = issue_ok(&t, "req-roll-1", alice, &[Scope::ReadApi]).await;
    t.exec_superuser("REVOKE UPDATE (revoked_at, revoked_reason) ON forge.tokens FROM forge_app")
        .await;
    let err = users::disable(&t.db, &ctx("req-roll-2"), alice)
        .await
        .expect_err("the token revoke fails");
    assert_eq!(err.code(), ErrorCode::DbQueryFailed, "{err}");
    let mut su = t.superuser().await;
    let state: String = sqlx::query_scalar("SELECT state FROM forge.users WHERE id = $1")
        .bind(alice.get())
        .fetch_one(&mut su)
        .await
        .expect("state");
    assert_eq!(
        state, "active",
        "the user is not disabled without its tokens"
    );
    let revoked: Option<String> = sqlx::query_scalar("SELECT revoked_reason FROM forge.tokens")
        .fetch_one(&mut su)
        .await
        .expect("token");
    assert!(revoked.is_none());
    assert!(audit_rows(&mut su, "req-roll-2").await.is_empty());
    assert_eq!(
        count(
            &mut su,
            "SELECT count(*) FROM forge.events WHERE request_id = 'req-roll-2'"
        )
        .await,
        0
    );
    // Still usable: nothing was half-applied.
    authenticate(&t.db, &rid("req-roll-3"), &issued.token.reveal())
        .await
        .expect("still valid");
    t.drop_now().await;
}
