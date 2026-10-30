//! `bootstrap_admin` (D3 MR2, ADR-0019 Amendment): the first-install and
//! recovery path. Refused while an active human administrator exists.

use super::*;

async fn is_admin(su: &mut PgConnection, user: UserId) -> bool {
    sqlx::query_scalar("SELECT is_admin FROM forge.users WHERE id = $1")
        .bind(user.get())
        .fetch_one(su)
        .await
        .expect("is_admin")
}

async fn details(su: &mut PgConnection, request: &str) -> serde_json::Value {
    sqlx::query_scalar("SELECT details FROM forge.audit_log WHERE request_id = $1")
        .bind(request)
        .fetch_one(su)
        .await
        .expect("details")
}

#[tokio::test]
async fn bootstrap_admin_promotes_an_active_human_with_one_audit_row() {
    let t = FreshDb::migrated().await;
    let alice = account(&t, "alice", UserKind::Human, false).await;
    let done = bootstrap_admin(&t.db, &ctx("req-boot"), &Bootstrap::Promote(alice))
        .await
        .expect("promote");
    assert_eq!(
        done,
        Bootstrapped {
            user_id: alice,
            namespace_id: None
        }
    );
    let mut su = t.superuser().await;
    assert!(is_admin(&mut su, alice).await);
    assert_eq!(
        audit_rows(&mut su, "req-boot").await,
        vec![(
            "user.admin_bootstrapped".to_owned(),
            "succeeded".to_owned(),
            "user".to_owned(),
            alice.get()
        )]
    );
    assert_eq!(
        details(&mut su, "req-boot").await,
        json!({"created": false, "namespace_id": null})
    );
    // user.* is audit-only.
    assert_eq!(rows_of(&mut su, "events").await, 0);
    t.drop_now().await;
}

#[tokio::test]
async fn bootstrap_admin_creates_a_new_human_admin_with_its_namespace() {
    let t = FreshDb::migrated().await;
    let done = bootstrap_admin(
        &t.db,
        &ctx("req-boot-create"),
        &Bootstrap::Create(NewUser::human("root-admin", "Root", "root@example.test")),
    )
    .await
    .expect("create");
    let namespace_id = done.namespace_id.expect("a new user has a namespace");
    let mut su = t.superuser().await;
    assert!(is_admin(&mut su, done.user_id).await);
    let (kind, full_path): (String, String) =
        sqlx::query_as("SELECT kind, full_path FROM forge.namespaces WHERE id = $1")
            .bind(namespace_id.get())
            .fetch_one(&mut su)
            .await
            .expect("namespace");
    assert_eq!((kind.as_str(), full_path.as_str()), ("user", "root-admin"));
    // One audit row: the bootstrap, not a separate user.created.
    assert_eq!(
        audit_rows(&mut su, "req-boot-create").await,
        vec![(
            "user.admin_bootstrapped".to_owned(),
            "succeeded".to_owned(),
            "user".to_owned(),
            done.user_id.get()
        )]
    );
    assert_eq!(
        details(&mut su, "req-boot-create").await,
        json!({"created": true, "namespace_id": namespace_id.get()})
    );
    t.drop_now().await;
}

#[tokio::test]
async fn bootstrap_admin_is_refused_while_an_active_human_admin_exists() {
    let t = FreshDb::migrated().await;
    // Neither a bot admin nor a disabled admin is an administrator to recover with.
    let _bot = account(&t, "ci-bot", UserKind::Bot, true).await;
    let gone = account(&t, "gone", UserKind::Human, true).await;
    let _ = disable_with(&t.db, &ctx("req-disable-gone"), gone, Guard::Force)
        .await
        .expect("forced");
    let alice = account(&t, "alice", UserKind::Human, false).await;
    bootstrap_admin(&t.db, &ctx("req-boot-1"), &Bootstrap::Promote(alice))
        .await
        .expect("no active human admin: allowed");

    let bob = account(&t, "bob", UserKind::Human, false).await;
    let err = bootstrap_admin(&t.db, &ctx("req-boot-2"), &Bootstrap::Promote(bob))
        .await
        .expect_err("alice is an admin now");
    assert_reason(&err, ErrorCode::AdministrationConflict, "admin_exists");
    assert_eq!(err.code().http_status(), 409);
    let err = bootstrap_admin(
        &t.db,
        &ctx("req-boot-3"),
        &Bootstrap::Create(NewUser::human("carol", "Carol", "carol@example.test")),
    )
    .await
    .expect_err("alice is an admin now");
    assert_reason(&err, ErrorCode::AdministrationConflict, "admin_exists");
    let mut su = t.superuser().await;
    assert!(!is_admin(&mut su, bob).await);
    assert_eq!(
        count(
            &mut su,
            "SELECT count(*) FROM forge.users WHERE username = 'carol'"
        )
        .await,
        0,
        "nothing created"
    );
    for request in ["req-boot-2", "req-boot-3"] {
        assert!(audit_rows(&mut su, request).await.is_empty(), "{request}");
    }
    t.drop_now().await;
}

#[tokio::test]
async fn bots_service_accounts_and_disabled_users_cannot_become_bootstrap_admins() {
    let t = FreshDb::migrated().await;
    let bot = account(&t, "ci-bot", UserKind::Bot, false).await;
    let service = account(&t, "backup", UserKind::ServiceAccount, false).await;
    let carol = account(&t, "carol", UserKind::Human, false).await;
    let _ = disable(&t.db, &ctx("req-disable-carol"), carol)
        .await
        .expect("disable carol");
    for (user, reason) in [
        (bot, "not_human"),
        (service, "not_human"),
        (carol, "user_disabled"),
    ] {
        let request = format!("req-boot-{}", user.get());
        let err = bootstrap_admin(&t.db, &ctx(&request), &Bootstrap::Promote(user))
            .await
            .expect_err(reason);
        assert_reason(&err, ErrorCode::AdministrationConflict, reason);
        let mut su = t.superuser().await;
        assert!(!is_admin(&mut su, user).await, "{reason}");
        assert!(audit_rows(&mut su, &request).await.is_empty(), "{reason}");
    }
    // `Create` makes humans only.
    let err = bootstrap_admin(
        &t.db,
        &ctx("req-boot-bot"),
        &Bootstrap::Create(NewUser {
            kind: UserKind::Bot,
            email: None,
            ..NewUser::human("new-bot", "Bot", "unused@example.test")
        }),
    )
    .await
    .expect_err("a bot");
    assert_reason(&err, ErrorCode::AdministrationConflict, "not_human");
    // An unknown user is not found.
    let err = bootstrap_admin(
        &t.db,
        &ctx("req-boot-none"),
        &Bootstrap::Promote(UserId::new(9_999)),
    )
    .await
    .expect_err("unknown");
    assert_eq!(err.code(), ErrorCode::NotFound, "{err}");
    let mut su = t.superuser().await;
    assert_eq!(
        count(&mut su, "SELECT count(*) FROM forge.users WHERE is_admin").await,
        0
    );
    t.drop_now().await;
}

#[tokio::test]
async fn two_concurrent_bootstraps_make_one_admin() {
    let t = FreshDb::migrated().await;
    let alice = account(&t, "alice", UserKind::Human, false).await;
    let bob = account(&t, "bob", UserKind::Human, false).await;
    let (ctx_a, ctx_b) = (ctx("req-boot-alice"), ctx("req-boot-bob"));
    let (promote_a, promote_b) = (Bootstrap::Promote(alice), Bootstrap::Promote(bob));
    let (a, b) = tokio::join!(
        bootstrap_admin(&t.db, &ctx_a, &promote_a),
        bootstrap_admin(&t.db, &ctx_b, &promote_b),
    );
    let refused: Vec<_> = [a, b].into_iter().filter_map(Result::err).collect();
    assert_eq!(refused.len(), 1, "exactly one is refused: {refused:?}");
    assert_reason(
        &refused[0],
        ErrorCode::AdministrationConflict,
        "admin_exists",
    );
    let mut su = t.superuser().await;
    assert_eq!(
        count(&mut su, "SELECT count(*) FROM forge.users WHERE is_admin").await,
        1
    );
    t.drop_now().await;
}
