//! Service accounts against a migrated database (M3 S0): the schema's
//! backstops by constraint name (raw statements as superuser, rolled back:
//! the deferred audit trigger would refuse any raw INSERT at COMMIT, so the
//! name of the violated constraint is what proves the rule), the commands,
//! a service account's tokens and the doctor's reads. Synthetic data only.

use forge_core::ids::UserId;
use forge_core::{RequestId, Scope, ServiceAccountField, UserKind};
use forge_errors::ErrorCode;
use sqlx::postgres::PgConnection;
use sqlx::Connection as _;

use super::*;
use crate::change::Actor;
use crate::identities::{self, LinkPolicy, LoginIdentity};
use crate::test_support::*;
use crate::tokens::{self, NewToken};
use crate::users::{self, NewUser};

const ISSUER: &str = "http://127.0.0.1:58080/realms/gitstation-dev";

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
        &NewUser::human(name, name, &format!("{name}@example.test")),
    )
    .await
    .expect("create user")
    .user_id
}

fn new_sa(username: &str, owner: UserId) -> NewServiceAccount {
    NewServiceAccount {
        username: username.to_owned(),
        display_name: format!("{username} bot"),
        owner,
        purpose: "Reports CI results for the synthetic project".to_owned(),
    }
}

async fn sa(t: &FreshDb, username: &str, owner: UserId, admin: UserId) -> UserId {
    create(
        &t.db,
        &ctx(&format!("req-sa-{username}")),
        &new_sa(username, owner),
        admin,
    )
    .await
    .expect("create service account")
    .user_id
}

fn token_for(user: UserId, days: u64) -> NewToken {
    NewToken {
        user,
        name: "kanflow".to_owned(),
        scopes: vec![Scope::ReadApi],
        expires_in: Some(Duration::from_secs(days * 24 * 3600)),
        is_agent: false,
    }
}

/// Run `setup` then `probe` as superuser in a transaction that is rolled
/// back: the SQLSTATE and constraint name of the probe's failure.
async fn refusal(su: &mut PgConnection, setup: &str, probe: &str) -> Option<(String, String)> {
    let mut tx = su.begin().await.expect("begin");
    if !setup.is_empty() {
        sqlx::raw_sql(setup).execute(&mut *tx).await.expect(setup);
    }
    let result = sqlx::raw_sql(probe).execute(&mut *tx).await;
    tx.rollback().await.expect("rollback");
    let e = result.err()?;
    let db = e.as_database_error().expect("a database error");
    Some((
        db.code().map(|c| c.into_owned()).unwrap_or_default(),
        db.constraint().unwrap_or_default().to_owned(),
    ))
}

async fn scalar(su: &mut PgConnection, sql: &str) -> i64 {
    sqlx::query_scalar(sql)
        .fetch_one(su)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
}

const PEOPLE: &str = "\
    INSERT INTO forge.users (id, kind, username, display_name, email) OVERRIDING SYSTEM VALUE \
    VALUES (901, 'human', 'p901', 'P', 'p901@example.test'); \
    INSERT INTO forge.users (id, kind, username, display_name) OVERRIDING SYSTEM VALUE \
    VALUES (902, 'service_account', 's902', 'S'), (903, 'bot', 'b903', 'B'), \
    (904, 'service_account', 's904', 'S');";

fn profile(user: i64, owner: i64) -> String {
    format!(
        "INSERT INTO forge.service_account_profiles (user_id, owner_user_id, purpose) \
         VALUES ({user}, {owner}, 'ci')"
    )
}

#[tokio::test]
async fn service_account_profile_requires_kind_service_account() {
    let t = FreshDb::migrated().await;
    let mut su = t.superuser().await;
    for user in [901, 903] {
        assert_eq!(
            refusal(&mut su, PEOPLE, &profile(user, 901)).await,
            Some(("23514".into(), "service_account_profiles_kind".into())),
            "user {user}"
        );
    }
    // Moving a profile to a person is refused the same way.
    let moved = format!(
        "{}; UPDATE forge.service_account_profiles SET user_id = 901",
        profile(902, 901)
    );
    assert_eq!(
        refusal(&mut su, PEOPLE, &moved).await,
        Some(("23514".into(), "service_account_profiles_kind".into()))
    );
    // A service account owned by a person passes (only the deferred audit
    // trigger, at COMMIT, is left).
    assert_eq!(refusal(&mut su, PEOPLE, &profile(902, 901)).await, None);
    t.drop_now().await;
}

#[tokio::test]
async fn profile_owner_must_be_human() {
    let t = FreshDb::migrated().await;
    let mut su = t.superuser().await;
    for owner in [904, 903] {
        assert_eq!(
            refusal(&mut su, PEOPLE, &profile(902, owner)).await,
            Some((
                "23514".into(),
                "service_account_profiles_owner_human".into()
            )),
            "owner {owner}"
        );
    }
    let handed_on = format!(
        "{}; UPDATE forge.service_account_profiles SET owner_user_id = 903",
        profile(902, 901)
    );
    assert_eq!(
        refusal(&mut su, PEOPLE, &handed_on).await,
        Some((
            "23514".into(),
            "service_account_profiles_owner_human".into()
        ))
    );
    drop(su);

    // The command refuses first, naming the reason and writing nothing.
    let admin = human(&t, "ada").await;
    let alice = human(&t, "alice").await;
    let ci = sa(&t, "svc-ci", alice, admin).await;
    for (owner, reason) in [
        (ci, "owner_not_human"),
        (UserId::new(99_999), "owner_absent"),
    ] {
        let err = create(&t.db, &ctx("req-refused"), &new_sa("svc-two", owner), admin)
            .await
            .expect_err("refused");
        assert_reason(&err, ErrorCode::UserRequestInvalid, reason);
    }
    let _ = users::disable(&t.db, &ctx("req-disable"), alice)
        .await
        .expect("disable alice");
    let err = create(&t.db, &ctx("req-refused"), &new_sa("svc-two", alice), admin)
        .await
        .expect_err("a disabled owner");
    assert_reason(&err, ErrorCode::UserRequestInvalid, "owner_disabled");
    let err = update(
        &t.db,
        &ctx("req-refused"),
        ci,
        &Changes {
            owner: Some(ci),
            ..Changes::default()
        },
    )
    .await
    .expect_err("itself as owner");
    assert_reason(&err, ErrorCode::UserRequestInvalid, "owner_not_human");
    let mut su = t.superuser().await;
    assert_eq!(
        scalar(
            &mut su,
            "SELECT count(*) FROM forge.users WHERE username = 'svc-two'"
        )
        .await,
        0
    );
    t.drop_now().await;
}

#[tokio::test]
async fn service_account_has_no_email_and_no_identity_link() {
    let t = FreshDb::migrated().await;
    let mut su = t.superuser().await;
    let sa_with = |cols: &str, vals: &str| {
        format!(
            "INSERT INTO forge.users (kind, username, display_name{cols}) \
             VALUES ('service_account', 'svc', 'S'{vals})"
        )
    };
    for (cols, vals) in [
        (", email", ", 'svc@example.test'"),
        (", employee_number", ", 'SYN0900'"),
        (", auth_source, external_id", ", 'ldap', 'uid-svc'"),
    ] {
        assert_eq!(
            refusal(&mut su, "", &sa_with(cols, vals)).await,
            Some((
                "23514".into(),
                "users_service_account_no_personal_data".into()
            )),
            "{cols}"
        );
    }
    assert_eq!(
        refusal(&mut su, "", &sa_with(", is_admin", ", true")).await,
        Some(("23514".into(), "users_service_account_not_admin".into()))
    );
    let link = |user: i64| {
        format!(
            "INSERT INTO forge.user_identities (user_id, issuer, subject) \
             VALUES ({user}, '{ISSUER}', 'sub-{user}')"
        )
    };
    for user in [902, 903] {
        assert_eq!(
            refusal(&mut su, PEOPLE, &link(user)).await,
            Some(("23514".into(), "user_identities_human_only".into())),
            "user {user}"
        );
    }
    assert_eq!(refusal(&mut su, PEOPLE, &link(901)).await, None);
    drop(su);

    // The command never writes personal data: the created row has none.
    let admin = human(&t, "ada").await;
    let alice = human(&t, "alice").await;
    let ci = sa(&t, "svc-ci", alice, admin).await;
    let mut su = t.superuser().await;
    assert_eq!(
        scalar(
            &mut su,
            &format!(
                "SELECT count(*) FROM forge.users WHERE id = {} AND kind = 'service_account' \
                 AND email IS NULL AND employee_number IS NULL AND external_id IS NULL \
                 AND NOT is_admin",
                ci.get()
            )
        )
        .await,
        1
    );
    t.drop_now().await;
}

#[tokio::test]
async fn service_account_cannot_sign_in_via_oidc() {
    let t = FreshDb::migrated().await;
    let admin = human(&t, "ada").await;
    let alice = human(&t, "alice").await;
    let ci = sa(&t, "svc-ci", alice, admin).await;
    // The operator cannot make it a sign-in target.
    let err = identities::link_by_operator(&t.db, &ctx("req-link"), ci, ISSUER, "sub-svc")
        .await
        .expect_err("not a person");
    assert_reason(&err, ErrorCode::IdentityLinkConflict, "not_human");
    // An IdP person named like it never takes it over: the username is
    // taken, and no identity is linked to the service account.
    let who = LoginIdentity {
        issuer: ISSUER,
        subject: "sub-impostor",
        employee_number: Some("SYN0901"),
        email: Some("impostor@example.test"),
        email_verified: true,
        preferred_username: Some("svc-ci"),
        name: Some("Impostor"),
    };
    let err = identities::login_decided(
        &t.db,
        &ChangeContext::new(
            Actor::Anonymous,
            RequestId::parse("req-login").expect("request id"),
        ),
        &who,
        LinkPolicy { auto_link: true },
        |_| Ok(()),
    )
    .await
    .expect_err("refused");
    assert_reason(
        &err,
        ErrorCode::IdentityLinkConflict,
        "username_unavailable",
    );
    let mut su = t.superuser().await;
    assert_eq!(
        scalar(&mut su, "SELECT count(*) FROM forge.user_identities").await,
        0
    );
    t.drop_now().await;
}

#[tokio::test]
async fn create_writes_the_account_its_namespace_and_profile_under_one_audit_row() {
    let t = FreshDb::migrated().await;
    let admin = human(&t, "ada").await;
    let alice = human(&t, "alice").await;
    let created = create(
        &t.db,
        &ctx("req-sa-create"),
        &new_sa("svc-ci", alice),
        admin,
    )
    .await
    .expect("create");
    let mut su = t.superuser().await;
    let audit: Vec<(String, String, i64, serde_json::Value)> = sqlx::query_as(
        "SELECT action, resource_type, resource_id, details FROM forge.audit_log \
         WHERE request_id = 'req-sa-create'",
    )
    .fetch_all(&mut su)
    .await
    .expect("audit");
    assert_eq!(audit.len(), 1, "{audit:?}");
    assert_eq!(audit[0].0, "service_account.created");
    assert_eq!(audit[0].2, created.user_id.get());
    assert_eq!(audit[0].3["owner_id"], alice.get());
    assert!(audit[0].3.get("purpose").is_none(), "never the free text");
    let got = get(&t.db, created.user_id)
        .await
        .expect("get")
        .expect("found");
    assert_eq!(got.username, "svc-ci");
    assert_eq!(got.owner.as_ref().map(|o| o.id), Some(alice));
    assert_eq!(got.created_by, Some(admin));
    assert_eq!(got.active_tokens, 0);
    assert_eq!(
        get(&t.db, alice).await.expect("get"),
        None,
        "a person is no service account"
    );
    let target_of = target(&t.db, created.user_id).await.expect("target");
    assert_eq!(
        target_of,
        Some(Target {
            kind: UserKind::ServiceAccount,
            active: true
        })
    );
    assert_eq!(
        target(&t.db, UserId::new(99_999)).await.expect("target"),
        None
    );

    // Field rules before anything is written.
    for (purpose, reason) in [
        ("", "purpose_length"),
        ("   ", "purpose_length"),
        ("two\nlines", "purpose_control_character"),
    ] {
        let mut new = new_sa("svc-bad", alice);
        new.purpose = purpose.to_owned();
        let err = create(&t.db, &ctx("req-bad"), &new, admin)
            .await
            .expect_err("refused");
        assert_reason(&err, ErrorCode::UserRequestInvalid, reason);
    }
    let mut long = new_sa("svc-bad", alice);
    long.purpose = "x".repeat(MAX_PURPOSE_CHARS + 1);
    let err = create(&t.db, &ctx("req-bad"), &long, admin)
        .await
        .expect_err("long");
    assert_reason(&err, ErrorCode::UserRequestInvalid, "purpose_length");
    // The username is a user's: taken by alice.
    let err = create(&t.db, &ctx("req-bad"), &new_sa("alice", alice), admin)
        .await
        .expect_err("taken");
    assert_eq!(err.code(), ErrorCode::NamespacePathTaken);
    let listed = list(&t.db).await.expect("list");
    assert_eq!(listed.items.len(), 1);
    assert!(!listed.truncated);
    t.drop_now().await;
}

#[tokio::test]
async fn update_changes_the_named_fields_and_an_equal_update_writes_nothing() {
    let t = FreshDb::migrated().await;
    let admin = human(&t, "ada").await;
    let alice = human(&t, "alice").await;
    let bob = human(&t, "bob").await;
    let ci = sa(&t, "svc-ci", alice, admin).await;
    let outcome = update(
        &t.db,
        &ctx("req-sa-update"),
        ci,
        &Changes {
            display_name: Some("CI reporter".to_owned()),
            purpose: Some("Reports CI results".to_owned()),
            owner: Some(bob),
        },
    )
    .await
    .expect("update");
    assert_eq!(
        outcome,
        UpdateOutcome::Updated(vec![
            ServiceAccountField::DisplayName,
            ServiceAccountField::Purpose,
            ServiceAccountField::Owner,
        ])
    );
    let got = get(&t.db, ci).await.expect("get").expect("found");
    assert_eq!(got.display_name, "CI reporter");
    assert_eq!(got.purpose.as_deref(), Some("Reports CI results"));
    assert_eq!(got.owner.map(|o| o.id), Some(bob));
    let same = update(
        &t.db,
        &ctx("req-sa-same"),
        ci,
        &Changes {
            owner: Some(bob),
            ..Changes::default()
        },
    )
    .await
    .expect("same");
    assert_eq!(same, UpdateOutcome::Unchanged);
    let mut su = t.superuser().await;
    assert_eq!(
        scalar(
            &mut su,
            "SELECT count(*) FROM forge.audit_log WHERE request_id = 'req-sa-same'"
        )
        .await,
        0
    );
    let err = update(&t.db, &ctx("req-x"), alice, &Changes::default())
        .await
        .expect_err("a person");
    assert_reason(&err, ErrorCode::NotFound, "absent");
    t.drop_now().await;
}

#[tokio::test]
async fn a_service_account_token_names_its_issuer_and_a_person_gets_none_this_way() {
    let t = FreshDb::migrated().await;
    let admin = human(&t, "ada").await;
    let alice = human(&t, "alice").await;
    let ci = sa(&t, "svc-ci", alice, admin).await;
    let issued =
        tokens::issue_for_service_account(&t.db, &ctx("req-issue"), &token_for(ci, 90), admin)
            .await
            .expect("issue");
    let listed = tokens::list(&t.db, ci).await.expect("list");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].issued_by, Some(admin));
    let mut su = t.superuser().await;
    let details: serde_json::Value =
        sqlx::query_scalar("SELECT details FROM forge.audit_log WHERE request_id = 'req-issue'")
            .fetch_one(&mut su)
            .await
            .expect("audit");
    assert_eq!(details["issued_by_user_id"], admin.get());
    assert_eq!(details["token_id"], issued.token_id);
    let err =
        tokens::issue_for_service_account(&t.db, &ctx("req-human"), &token_for(alice, 90), admin)
            .await
            .expect_err("a person");
    assert_reason(
        &err,
        ErrorCode::TokenRequestInvalid,
        "owner_not_service_account",
    );
    let mut admin_scope = token_for(ci, 90);
    admin_scope.scopes = vec![Scope::Api, Scope::Admin];
    let err = tokens::issue_for_service_account(&t.db, &ctx("req-admin"), &admin_scope, admin)
        .await
        .expect_err("never admin");
    assert_reason(
        &err,
        ErrorCode::TokenRequestInvalid,
        "admin_scope_requires_admin",
    );
    // A person's own token names no issuer.
    let _ = tokens::issue(&t.db, &ctx("req-own"), &token_for(alice, 90))
        .await
        .expect("own");
    assert_eq!(
        tokens::list(&t.db, alice).await.expect("list")[0].issued_by,
        None
    );
    t.drop_now().await;
}

#[tokio::test]
async fn disabling_service_account_revokes_tokens() {
    let t = FreshDb::migrated().await;
    let admin = human(&t, "ada").await;
    let alice = human(&t, "alice").await;
    let ci = sa(&t, "svc-ci", alice, admin).await;
    for _ in 0..2 {
        let _ = tokens::issue_for_service_account(&t.db, &ctx("req-i"), &token_for(ci, 90), admin)
            .await
            .expect("issue");
    }
    let outcome = users::disable(&t.db, &ctx("req-disable-sa"), ci)
        .await
        .expect("disable");
    assert_eq!(outcome, users::DisableOutcome::Disabled);
    let mut su = t.superuser().await;
    assert_eq!(
        scalar(
            &mut su,
            &format!(
                "SELECT count(*) FROM forge.tokens WHERE user_id = {} \
                 AND revoked_reason = 'user_disabled'",
                ci.get()
            )
        )
        .await,
        2
    );
    let err = tokens::issue_for_service_account(&t.db, &ctx("req-late"), &token_for(ci, 90), admin)
        .await
        .expect_err("disabled");
    assert_reason(&err, ErrorCode::TokenRequestInvalid, "owner_disabled");
    t.drop_now().await;
}

#[tokio::test]
async fn doctor_warns_token_expiring_within_14_days() {
    let t = FreshDb::migrated().await;
    let admin = human(&t, "ada").await;
    let alice = human(&t, "alice").await;
    let soon = sa(&t, "svc-soon", alice, admin).await;
    let rotated = sa(&t, "svc-rotated", alice, admin).await;
    let fine = sa(&t, "svc-fine", alice, admin).await;
    let gone = sa(&t, "svc-gone", alice, admin).await;
    let issue = |user, days| {
        let db = &t.db;
        async move {
            tokens::issue_for_service_account(db, &ctx("req-t"), &token_for(user, days), admin)
                .await
                .expect("issue")
        }
    };
    let _ = issue(soon, 3).await;
    // Rotated: the old token expires soon, the new one later — no warning.
    let _ = issue(rotated, 3).await;
    let _ = issue(rotated, 90).await;
    let _ = issue(fine, 30).await;
    // A disabled account is no longer a concern of this check.
    let _ = issue(gone, 3).await;
    let _ = users::disable(&t.db, &ctx("req-gone"), gone)
        .await
        .expect("disable");
    // A person's expiring token is not a service account's.
    let _ = tokens::issue(&t.db, &ctx("req-own"), &token_for(alice, 3))
        .await
        .expect("own");

    let h = health(&t.db, EXPIRY_WARNING)
        .await
        .expect("health")
        .expect("migrated");
    assert_eq!(h.expiring, 1, "{h:?}");
    assert_eq!(h.expiring_ids, vec![soon]);
    assert!(h.soonest_expiry.is_some());
    let err = h.check_expiring().expect_err("warns");
    assert_eq!(err.code(), ErrorCode::ServiceAccountTokenExpiring);
    assert_eq!(err.code().severity(), forge_errors::Severity::Warn);
    let ids = err
        .context()
        .iter()
        .find(|(k, _)| *k == "user_ids")
        .map(|(_, v)| v.clone());
    assert_eq!(ids, Some(soon.get().to_string()));
    assert_eq!(h.ownerless, 0);
    assert!(h.check_ownerless().is_ok());
    // A narrower window does not see it.
    let narrow = health(&t.db, Duration::from_secs(24 * 3600))
        .await
        .expect("health")
        .expect("migrated");
    assert_eq!(narrow.expiring, 0);
    assert!(narrow.check_expiring().is_ok());
    t.drop_now().await;
}

#[tokio::test]
async fn doctor_reports_a_service_account_whose_owner_left() {
    let t = FreshDb::migrated().await;
    let admin = human(&t, "ada").await;
    let alice = human(&t, "alice").await;
    let bob = human(&t, "bob").await;
    let kept = sa(&t, "svc-kept", bob, admin).await;
    let orphan = sa(&t, "svc-orphan", alice, admin).await;
    let _ = kept;
    let _ = users::disable(&t.db, &ctx("req-off"), alice)
        .await
        .expect("disable");
    let h = health(&t.db, EXPIRY_WARNING)
        .await
        .expect("health")
        .expect("migrated");
    assert_eq!(h.ownerless, 1, "{h:?}");
    assert_eq!(h.ownerless_ids, vec![orphan]);
    let err = h.check_ownerless().expect_err("warns");
    assert_reason(
        &err,
        ErrorCode::AdministrationOrphaned,
        "ownerless_service_account",
    );
    // Handing it on clears the finding.
    let _ = update(
        &t.db,
        &ctx("req-hand-on"),
        orphan,
        &Changes {
            owner: Some(bob),
            ..Changes::default()
        },
    )
    .await
    .expect("hand on");
    let h = health(&t.db, EXPIRY_WARNING)
        .await
        .expect("health")
        .expect("migrated");
    assert_eq!(h.ownerless, 0);
    t.drop_now().await;
}

/// S0 review: a user's kind never changes (`users_kind_immutable`), so the
/// kind checks of a profile and an identity stay true after their rows were
/// written; an UPDATE of another column passes.
#[tokio::test]
async fn a_users_kind_never_changes() {
    let t = FreshDb::migrated().await;
    let mut su = t.superuser().await;
    for (from, to) in [
        (902, "human"),
        (901, "service_account"),
        (903, "service_account"),
    ] {
        assert_eq!(
            refusal(
                &mut su,
                PEOPLE,
                &format!("UPDATE forge.users SET kind = '{to}' WHERE id = {from}")
            )
            .await,
            Some(("23514".into(), "users_kind_immutable".into())),
            "{from} -> {to}"
        );
    }
    assert_eq!(
        refusal(
            &mut su,
            PEOPLE,
            "UPDATE forge.users SET display_name = 'Renamed', kind = kind WHERE id = 902"
        )
        .await,
        None
    );
    t.drop_now().await;
}

/// S0 review: the kind checks are guards (20261113100200): the triggers
/// under their guard names, ENABLE ALWAYS, registered as guarded objects;
/// the names of 20261113100000 are gone.
#[tokio::test]
async fn the_kind_checks_are_registered_guards() {
    let t = FreshDb::migrated().await;
    let mut su = t.superuser().await;
    let enabled = |table: &str, name: &str| {
        format!(
            "SELECT count(*) FROM pg_catalog.pg_trigger \
             WHERE tgrelid = 'forge.{table}'::regclass AND tgname = '{name}' AND tgenabled = 'A'"
        )
    };
    for (table, name, old) in [
        (
            "service_account_profiles",
            "service_account_profiles_kind_guard",
            "service_account_profiles_kinds",
        ),
        (
            "user_identities",
            "user_identities_human_guard",
            "user_identities_human_only",
        ),
        ("users", "users_kind_immutable", "users_kind_immutable"),
    ] {
        assert_eq!(scalar(&mut su, &enabled(table, name)).await, 1, "{name}");
        if old != name {
            assert_eq!(
                scalar(&mut su, &enabled(table, old)).await,
                0,
                "{old} dropped"
            );
        }
        assert_eq!(
            scalar(
                &mut su,
                &format!(
                    "SELECT count(*) FROM forge_audit.guarded_objects WHERE \
                     (object_type = 'trigger' AND object_identity = '{name} on forge.{table}') \
                     OR (object_type = 'function' AND object_identity = 'forge.{name}()')"
                )
            )
            .await,
            2,
            "{name} is a guarded trigger and function"
        );
    }
    t.drop_now().await;
}
