//! tokens::issue request rules (FG-AUTH-0008): nothing is written when one
//! is broken.

use super::*;

async fn nothing_written(t: &FreshDb, request: &str) {
    let mut su = t.superuser().await;
    assert_eq!(count(&mut su, "SELECT count(*) FROM forge.tokens").await, 0);
    assert!(audit_rows(&mut su, request).await.is_empty());
}

#[tokio::test]
async fn expiry_over_365_or_under_1_day_is_token_request_invalid() {
    let t = FreshDb::migrated().await;
    let alice = human(&t, "alice", false).await;
    let day = 24 * 3600;
    for (i, secs) in [0, day - 1, 365 * day + 1].into_iter().enumerate() {
        let mut new = request(alice, &[Scope::ReadApi]);
        new.expires_in = Some(Duration::from_secs(secs));
        let req = format!("req-range-{i}");
        let err = issue(&t.db, &ctx(&req), &new).await.expect_err("range");
        assert_reason(&err, ErrorCode::TokenRequestInvalid, "expires_out_of_range");
        assert_eq!(err.code().http_status(), 400);
        nothing_written(&t, &req).await;
    }
    // The boundaries themselves are valid.
    for (i, secs) in [day, 365 * day].into_iter().enumerate() {
        let mut new = request(alice, &[Scope::ReadApi]);
        new.expires_in = Some(Duration::from_secs(secs));
        issue(&t.db, &ctx(&format!("req-edge-{i}")), &new)
            .await
            .expect("boundary expiry");
    }
    t.drop_now().await;
}

#[tokio::test]
async fn an_invalid_name_or_empty_scopes_is_token_request_invalid() {
    let t = FreshDb::migrated().await;
    let alice = human(&t, "alice", false).await;
    let cases: Vec<(NewToken, &str)> = vec![
        (
            NewToken {
                name: String::new(),
                ..request(alice, &[Scope::ReadApi])
            },
            "name_length",
        ),
        (
            NewToken {
                name: "x".repeat(101),
                ..request(alice, &[Scope::ReadApi])
            },
            "name_length",
        ),
        (
            NewToken {
                name: "a\0b".to_owned(),
                ..request(alice, &[Scope::ReadApi])
            },
            "nul_character",
        ),
        (request(alice, &[]), "scopes_empty"),
        (
            NewToken {
                name: "ci\tfake\nrow".to_owned(),
                ..request(alice, &[Scope::ReadApi])
            },
            "control_character",
        ),
        (
            NewToken {
                name: "x\u{1b}[31m".to_owned(),
                ..request(alice, &[Scope::ReadApi])
            },
            "control_character",
        ),
        (
            NewToken {
                name: "abc\u{202E}txt.exe".to_owned(),
                ..request(alice, &[Scope::ReadApi])
            },
            "control_character",
        ),
    ];
    for (i, (new, reason)) in cases.into_iter().enumerate() {
        let req = format!("req-bad-{i}");
        let err = issue(&t.db, &ctx(&req), &new).await.expect_err(reason);
        assert_reason(&err, ErrorCode::TokenRequestInvalid, reason);
        nothing_written(&t, &req).await;
    }
    // 100 characters (multi-byte) is the longest valid name.
    let new = NewToken {
        name: "あ".repeat(100),
        ..request(alice, &[Scope::ReadApi])
    };
    issue(&t.db, &ctx("req-bad-long-ok"), &new)
        .await
        .expect("100 chars");
    t.drop_now().await;
}

#[tokio::test]
async fn admin_scope_for_a_non_admin_owner_is_refused() {
    let t = FreshDb::migrated().await;
    let alice = human(&t, "alice", false).await;
    let root = human(&t, "root", true).await;
    let err = issue(
        &t.db,
        &ctx("req-admin-1"),
        &request(alice, &[Scope::ReadApi, Scope::Admin]),
    )
    .await
    .expect_err("non-admin");
    assert_reason(
        &err,
        ErrorCode::TokenRequestInvalid,
        "admin_scope_requires_admin",
    );
    nothing_written(&t, "req-admin-1").await;
    let ok = issue_ok(&t, "req-admin-2", root, &[Scope::Admin]).await;
    let id = authenticate(&t.db, &rid("req-admin-3"), &ok.token.reveal())
        .await
        .expect("admin token");
    assert!(id.is_admin && id.scopes == vec![Scope::Admin]);
    t.drop_now().await;
}

#[tokio::test]
async fn a_bot_cannot_hold_a_token() {
    let t = FreshDb::migrated().await;
    let robot = bot(&t, "robot").await;
    let err = issue(&t.db, &ctx("req-bot"), &request(robot, &[Scope::ReadApi]))
        .await
        .expect_err("bot");
    assert_reason(&err, ErrorCode::TokenRequestInvalid, "owner_bot");
    nothing_written(&t, "req-bot").await;
    t.drop_now().await;
}

#[tokio::test]
async fn a_disabled_user_cannot_get_a_token() {
    let t = FreshDb::migrated().await;
    let alice = human(&t, "alice", false).await;
    let _ = users::disable(&t.db, &ctx("req-disable"), alice)
        .await
        .expect("disable");
    let err = issue(
        &t.db,
        &ctx("req-disabled"),
        &request(alice, &[Scope::ReadApi]),
    )
    .await
    .expect_err("disabled");
    assert_reason(&err, ErrorCode::TokenRequestInvalid, "owner_disabled");
    nothing_written(&t, "req-disabled").await;
    t.drop_now().await;
}

#[tokio::test]
async fn a_refused_issue_names_the_owner_in_its_context() {
    let t = FreshDb::migrated().await;
    let alice = human(&t, "alice", false).await;
    let mut su = t.superuser().await;
    seed_active(&mut su, alice, 50).await;
    let err = issue(&t.db, &ctx("req-ctx-1"), &request(alice, &[Scope::ReadApi]))
        .await
        .expect_err("limit");
    assert_reason(&err, ErrorCode::TokenRequestInvalid, "active_limit");
    assert!(
        chain(&err).contains(&format!("user={}", alice.get())),
        "{err}"
    );
    let err = issue(&t.db, &ctx("req-ctx-2"), &request(alice, &[]))
        .await
        .expect_err("empty");
    assert!(
        chain(&err).contains(&format!("user={}", alice.get())),
        "{err}"
    );
    t.drop_now().await;
}

#[tokio::test]
async fn issuing_for_an_unknown_user_id_is_not_found() {
    let t = FreshDb::migrated().await;
    let err = issue(
        &t.db,
        &ctx("req-nouser"),
        &request(UserId::new(987_654), &[Scope::ReadApi]),
    )
    .await
    .expect_err("unknown owner");
    assert_eq!(err.code(), ErrorCode::NotFound, "{err}");
    assert!(chain(&err).contains("resource=user"), "{err}");
    nothing_written(&t, "req-nouser").await;
    t.drop_now().await;
}

#[tokio::test]
async fn repeated_scopes_are_stored_once_in_first_order() {
    let t = FreshDb::migrated().await;
    let alice = human(&t, "alice", false).await;
    let issued = issue_ok(
        &t,
        "req-dup",
        alice,
        &[Scope::ReadApi, Scope::Api, Scope::ReadApi],
    )
    .await;
    let mut su = t.superuser().await;
    let stored: Vec<String> = sqlx::query_scalar("SELECT scopes FROM forge.tokens")
        .fetch_one(&mut su)
        .await
        .expect("scopes");
    assert_eq!(stored, vec!["read_api", "api"]);
    let details: serde_json::Value =
        sqlx::query_scalar("SELECT details FROM forge.audit_log WHERE action = 'token.created'")
            .fetch_one(&mut su)
            .await
            .expect("details");
    assert_eq!(details["scopes"], serde_json::json!(["read_api", "api"]));
    let id = authenticate(&t.db, &rid("req-dup-2"), &issued.token.reveal())
        .await
        .expect("auth");
    assert_eq!(id.scopes, vec![Scope::ReadApi, Scope::Api]);
    t.drop_now().await;
}
