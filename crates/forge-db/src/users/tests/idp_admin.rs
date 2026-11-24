//! The administrator flag from the identity provider's role (ADR-0021
//! amendment 2026-11-24): grant and revoke with one audit row, a CLI
//! administrator kept, bots and disabled users untouched, and the CLI's
//! switch of who manages the flag.

use forge_core::AdminSource;

use super::*;

async fn flags(su: &mut PgConnection, user: UserId) -> (bool, String) {
    sqlx::query_as("SELECT is_admin, admin_source FROM forge.users WHERE id = $1")
        .bind(user.get())
        .fetch_one(su)
        .await
        .expect("flags")
}

async fn details(su: &mut PgConnection, request: &str) -> serde_json::Value {
    sqlx::query_scalar("SELECT details FROM forge.audit_log WHERE request_id = $1")
        .bind(request)
        .fetch_one(su)
        .await
        .expect("details")
}

#[test]
fn the_rule_grants_revokes_and_keeps_the_cli_admin() {
    let person = |is_admin, source| AdminFacts {
        kind: UserKind::Human,
        active: true,
        is_admin,
        source,
    };
    assert_eq!(
        idp_admin_change(person(false, AdminSource::Cli), true),
        Some(true)
    );
    assert_eq!(
        idp_admin_change(person(true, AdminSource::Oidc), false),
        Some(false)
    );
    // Break-glass: the CLI's administrator keeps the flag without the role.
    assert_eq!(
        idp_admin_change(person(true, AdminSource::Cli), false),
        None
    );
    assert_eq!(
        idp_admin_change(person(true, AdminSource::Oidc), true),
        None
    );
    assert_eq!(
        idp_admin_change(person(false, AdminSource::Cli), false),
        None
    );
    for kind in [UserKind::Bot, UserKind::ServiceAccount] {
        let facts = AdminFacts {
            kind,
            ..person(false, AdminSource::Cli)
        };
        assert_eq!(idp_admin_change(facts, true), None, "{kind:?}");
    }
    let disabled = AdminFacts {
        active: false,
        ..person(true, AdminSource::Oidc)
    };
    assert_eq!(idp_admin_change(disabled, false), None);
}

#[tokio::test]
async fn the_role_grants_and_revokes_with_one_audit_row_each() {
    let t = FreshDb::migrated().await;
    let alice = account(&t, "alice", UserKind::Human, false).await;
    let done = sync_idp_admin(&t.db, &ctx("req-idp-grant"), alice, true)
        .await
        .expect("grant");
    assert_eq!(done, IdpAdmin::Granted);
    let mut su = t.superuser().await;
    assert_eq!(flags(&mut su, alice).await, (true, "oidc".to_owned()));
    assert_eq!(
        audit_rows(&mut su, "req-idp-grant").await,
        vec![(
            "user.admin_changed".to_owned(),
            "succeeded".to_owned(),
            "user".to_owned(),
            alice.get()
        )]
    );
    assert_eq!(
        details(&mut su, "req-idp-grant").await,
        json!({"from": false, "to": true, "source": "oidc_role"})
    );
    // The same answer again writes nothing.
    let again = sync_idp_admin(&t.db, &ctx("req-idp-again"), alice, true)
        .await
        .expect("again");
    assert_eq!(again, IdpAdmin::Unchanged);
    assert!(audit_rows(&mut su, "req-idp-again").await.is_empty());
    // The role is gone: revoked, back to the CLI's default.
    let done = sync_idp_admin(&t.db, &ctx("req-idp-revoke"), alice, false)
        .await
        .expect("revoke");
    assert_eq!(done, IdpAdmin::Revoked);
    assert_eq!(flags(&mut su, alice).await, (false, "cli".to_owned()));
    assert_eq!(
        details(&mut su, "req-idp-revoke").await,
        json!({"from": true, "to": false, "source": "oidc_role"})
    );
    // user.* is audit-only.
    assert_eq!(rows_of(&mut su, "events").await, 0);
    t.drop_now().await;
}

#[tokio::test]
async fn a_cli_admin_keeps_the_flag_and_bots_never_gain_it() {
    let t = FreshDb::migrated().await;
    let root = account(&t, "root", UserKind::Human, true).await;
    let bot = account(&t, "robot", UserKind::Bot, false).await;
    let carol = account(&t, "carol", UserKind::Human, false).await;
    assert_eq!(
        sync_idp_admin(&t.db, &ctx("req-idp-root"), root, false)
            .await
            .expect("root"),
        IdpAdmin::Unchanged
    );
    assert_eq!(
        sync_idp_admin(&t.db, &ctx("req-idp-bot"), bot, true)
            .await
            .expect("bot"),
        IdpAdmin::Unchanged
    );
    let _ = disable(&t.db, &ctx("req-disable-carol"), carol)
        .await
        .expect("disabled");
    assert_eq!(
        sync_idp_admin(&t.db, &ctx("req-idp-carol"), carol, true)
            .await
            .expect("carol"),
        IdpAdmin::Unchanged
    );
    let mut su = t.superuser().await;
    assert_eq!(flags(&mut su, root).await, (true, "cli".to_owned()));
    assert_eq!(flags(&mut su, bot).await, (false, "cli".to_owned()));
    assert_eq!(flags(&mut su, carol).await, (false, "cli".to_owned()));
    for request in ["req-idp-root", "req-idp-bot", "req-idp-carol"] {
        assert!(audit_rows(&mut su, request).await.is_empty(), "{request}");
    }
    let err = sync_idp_admin(&t.db, &ctx("req-idp-none"), UserId::new(999_999), true)
        .await
        .expect_err("unknown");
    assert_eq!(err.code(), ErrorCode::NotFound);
    // The counts doctor shows.
    let alice = account(&t, "alice", UserKind::Human, false).await;
    sync_idp_admin(&t.db, &ctx("req-idp-alice"), alice, true)
        .await
        .expect("grant");
    assert_eq!(
        admin_sources(&t.db).await.expect("counts"),
        AdminSources { cli: 1, oidc: 1 }
    );
    t.drop_now().await;
}

#[tokio::test]
async fn the_cli_switches_who_manages_an_administrator() {
    let t = FreshDb::migrated().await;
    let root = account(&t, "root", UserKind::Human, true).await;
    let alice = account(&t, "alice", UserKind::Human, false).await;
    let done = set_admin_source(&t.db, &ctx("req-src-oidc"), root, AdminSource::Oidc)
        .await
        .expect("to oidc");
    assert_eq!(
        done,
        AdminSourceSet::Changed {
            from: AdminSource::Cli
        }
    );
    let mut su = t.superuser().await;
    assert_eq!(flags(&mut su, root).await, (true, "oidc".to_owned()));
    assert_eq!(
        audit_rows(&mut su, "req-src-oidc").await,
        vec![(
            "user.admin_source_changed".to_owned(),
            "succeeded".to_owned(),
            "user".to_owned(),
            root.get()
        )]
    );
    assert_eq!(
        details(&mut su, "req-src-oidc").await,
        json!({"from": "cli", "to": "oidc"})
    );
    // Now the role manages it: a sign-in without the role revokes it.
    assert_eq!(
        sync_idp_admin(&t.db, &ctx("req-src-revoke"), root, false)
            .await
            .expect("revoke"),
        IdpAdmin::Revoked
    );
    // Not an administrator (any more): refused, nothing written.
    for (user, request) in [(root, "req-src-root"), (alice, "req-src-alice")] {
        let err = set_admin_source(&t.db, &ctx(request), user, AdminSource::Cli)
            .await
            .expect_err("not an administrator");
        assert_eq!(err.code(), ErrorCode::AdministrationConflict);
        assert!(err.to_string().contains("reason=not_admin"), "{err}");
        assert!(audit_rows(&mut su, request).await.is_empty());
    }
    // The same source again changes nothing.
    sync_idp_admin(&t.db, &ctx("req-src-grant"), alice, true)
        .await
        .expect("grant");
    assert_eq!(
        set_admin_source(&t.db, &ctx("req-src-same"), alice, AdminSource::Oidc)
            .await
            .expect("same"),
        AdminSourceSet::Unchanged
    );
    assert!(audit_rows(&mut su, "req-src-same").await.is_empty());
    t.drop_now().await;
}

/// The database keeps the rule's backstop: an `oidc` source only on an
/// administrator.
#[tokio::test]
async fn the_oidc_source_needs_the_flag_in_the_database() {
    let t = FreshDb::migrated().await;
    let alice = account(&t, "alice", UserKind::Human, false).await;
    let err = commit(
        &t,
        &[
            format!(
                "UPDATE forge.users SET admin_source = 'oidc' WHERE id = {}",
                alice.get()
            ),
            audit("user", alice.get(), "succeeded"),
        ],
    )
    .await
    .expect_err("the CHECK refuses it");
    assert!(
        err.to_string().contains("users_admin_source_only_admin"),
        "{err}"
    );
    t.drop_now().await;
}
