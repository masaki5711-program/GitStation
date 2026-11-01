//! The user commands against a migrated database. Inspection uses a
//! superuser connection (the read pool cannot see email).

use forge_core::RequestId;
use forge_errors::ErrorCode;
use serde_json::json;
use sqlx::postgres::PgConnection;

use super::*;
use crate::audited_tests::{audit, commit};
use crate::change::Actor;
use crate::test_support::*;

mod administration;
mod bootstrap;
mod guard;
mod operator_only;
mod race;
mod request;

const EMAIL: &str = "alice@example.test";

fn ctx(request: &str) -> ChangeContext {
    ChangeContext::new(
        Actor::System,
        RequestId::parse(request).expect("request id"),
    )
}

async fn count(su: &mut PgConnection, sql: &str) -> i64 {
    sqlx::query_scalar(sql)
        .fetch_one(su)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
}

async fn rows_of(su: &mut PgConnection, table: &str) -> i64 {
    count(su, &format!("SELECT count(*) FROM forge.{table}")).await
}

async fn audit_rows(su: &mut PgConnection, request: &str) -> Vec<(String, String, String, i64)> {
    sqlx::query_as(
        "SELECT action, outcome, resource_type, resource_id FROM forge.audit_log \
         WHERE request_id = $1 ORDER BY id",
    )
    .bind(request)
    .fetch_all(su)
    .await
    .expect("audit rows")
}

/// A `deleted` tombstone of `path`, caused by user `cause_id` (commit it
/// together with `audit("user", cause_id, "succeeded")`).
fn tombstone(path: &str, cause_id: i64) -> String {
    format!(
        "INSERT INTO forge.namespace_tombstones (old_path, kind, cause_type, cause_id) \
         VALUES ('{path}', 'deleted', 'user', {cause_id})"
    )
}

/// An active user of `kind` named `name` (bots and service accounts have no
/// email).
async fn account(t: &FreshDb, name: &str, kind: UserKind, is_admin: bool) -> UserId {
    let email = format!("{name}@example.test");
    let new = NewUser {
        kind,
        email: (kind == UserKind::Human).then_some(email.clone()),
        is_admin,
        ..NewUser::human(name, name, &email)
    };
    create(&t.db, &ctx(&format!("req-user-{name}")), &new)
        .await
        .unwrap_or_else(|e| panic!("{name}: {e}"))
        .user_id
}

async fn create_alice(t: &FreshDb, request: &str) -> Created {
    create(
        &t.db,
        &ctx(request),
        &NewUser::human("alice", "Alice A", EMAIL),
    )
    .await
    .expect("create alice")
}

#[tokio::test]
async fn user_create_writes_user_namespace_and_one_audit_row() {
    let t = FreshDb::migrated().await;
    let created = create_alice(&t, "req-create-1").await;
    let mut su = t.superuser().await;

    let user: (String, String, String, String, String, bool) = sqlx::query_as(
        "SELECT kind, username, display_name, email, state, is_admin FROM forge.users WHERE id = $1",
    )
    .bind(created.user_id.get())
    .fetch_one(&mut su)
    .await
    .expect("user row");
    assert_eq!(
        user,
        (
            "human".into(),
            "alice".into(),
            "Alice A".into(),
            EMAIL.into(),
            "active".into(),
            false
        )
    );

    let ns: (String, String, String, i64, i64, bool) = sqlx::query_as(
        "SELECT kind, segment, full_path, depth::bigint, owner_id, parent_id IS NULL \
         FROM forge.namespaces WHERE id = $1",
    )
    .bind(created.namespace_id.get())
    .fetch_one(&mut su)
    .await
    .expect("namespace row");
    assert_eq!(
        ns,
        (
            "user".into(),
            "alice".into(),
            "alice".into(),
            1,
            created.user_id.get(),
            true
        )
    );
    assert_eq!(rows_of(&mut su, "namespaces").await, 1);

    // Exactly one audit row, covering the user; details carry no personal data.
    assert_eq!(
        audit_rows(&mut su, "req-create-1").await,
        vec![(
            "user.created".to_owned(),
            "succeeded".to_owned(),
            "user".to_owned(),
            created.user_id.get()
        )]
    );
    let details: serde_json::Value =
        sqlx::query_scalar("SELECT details FROM forge.audit_log WHERE request_id = 'req-create-1'")
            .fetch_one(&mut su)
            .await
            .expect("details");
    assert_eq!(
        details,
        json!({"kind": "human", "namespace_id": created.namespace_id.get(), "is_admin": false})
    );
    // user.* is audit-only.
    assert_eq!(rows_of(&mut su, "events").await, 0);
    t.drop_now().await;
}

#[tokio::test]
async fn an_invalid_username_is_rejected_before_any_write() {
    let t = FreshDb::migrated().await;
    let err = create(
        &t.db,
        &ctx("req-bad-1"),
        &NewUser::human("Bad Name", "Bad", "bad@example.test"),
    )
    .await
    .expect_err("invalid name");
    assert_eq!(err.code(), ErrorCode::NamespacePathInvalid, "{err}");
    assert_eq!(err.code().http_status(), 400);
    assert!(!chain(&err).contains("Bad Name"), "{}", chain(&err));
    let mut su = t.superuser().await;
    assert_eq!(rows_of(&mut su, "users").await, 0);
    assert_eq!(rows_of(&mut su, "namespaces").await, 0);
    assert!(audit_rows(&mut su, "req-bad-1").await.is_empty());
    t.drop_now().await;
}

#[tokio::test]
async fn a_reserved_or_taken_or_tombstoned_username_is_rejected() {
    let t = FreshDb::migrated().await;
    // A tombstone of a path that was moved away (written with its audit row).
    commit(
        &t,
        &[audit("user", 999, "succeeded"), tombstone("ghost", 999)],
    )
    .await
    .expect("seed tombstone");
    create_alice(&t, "req-first").await;

    let cases = [
        (
            "api",
            ErrorCode::NamespacePathReserved,
            400,
            Some("reserved"),
        ),
        (
            "devices",
            ErrorCode::NamespacePathReserved,
            400,
            Some("reserved"),
        ),
        ("alice", ErrorCode::NamespacePathTaken, 409, Some("sibling")),
        (
            "ghost",
            ErrorCode::NamespacePathTaken,
            409,
            Some("tombstoned"),
        ),
    ];
    for (name, code, status, reason) in cases {
        let request = format!("req-reject-{name}");
        let err = create(
            &t.db,
            &ctx(&request),
            &NewUser::human(name, "Other", &format!("{name}-other@example.test")),
        )
        .await
        .expect_err(name);
        assert_eq!(err.code(), code, "{name}: {err}");
        assert_eq!(err.code().http_status(), status, "{name}");
        if let Some(reason) = reason {
            assert_reason(&err, code, reason);
        }
        // The catalog promises the name is not logged.
        assert!(!chain(&err).contains(name), "{name}: {}", chain(&err));
        let mut su = t.superuser().await;
        assert!(audit_rows(&mut su, &request).await.is_empty(), "{name}");
    }
    let mut su = t.superuser().await;
    assert_eq!(rows_of(&mut su, "users").await, 1, "only alice exists");
    assert_eq!(rows_of(&mut su, "namespaces").await, 1);
    t.drop_now().await;
}

#[tokio::test]
async fn a_second_user_with_the_same_email_is_user_email_taken() {
    let t = FreshDb::migrated().await;
    create_alice(&t, "req-first").await;
    let err = create(
        &t.db,
        &ctx("req-second"),
        &NewUser::human("bob", "Bob", "ALICE@Example.TEST"),
    )
    .await
    .expect_err("same email, other case");
    assert_reason(&err, ErrorCode::UserEmailTaken, "email_taken");
    assert_eq!(err.code().http_status(), 409);
    assert!(
        !err.to_string().to_lowercase().contains("alice@"),
        "the address stays out of the error: {err}"
    );
    let mut su = t.superuser().await;
    assert_eq!(rows_of(&mut su, "users").await, 1);
    assert_eq!(rows_of(&mut su, "namespaces").await, 1);
    assert!(audit_rows(&mut su, "req-second").await.is_empty());
    t.drop_now().await;
}

#[tokio::test]
async fn user_disable_writes_one_audit_row_and_no_event() {
    let t = FreshDb::migrated().await;
    let created = create_alice(&t, "req-create").await;
    let outcome = disable(&t.db, &ctx("req-disable"), created.user_id)
        .await
        .expect("disable");
    assert_eq!(outcome, DisableOutcome::Disabled);
    let mut su = t.superuser().await;
    let state: String = sqlx::query_scalar("SELECT state FROM forge.users WHERE id = $1")
        .bind(created.user_id.get())
        .fetch_one(&mut su)
        .await
        .expect("state");
    assert_eq!(state, "disabled");
    assert_eq!(
        audit_rows(&mut su, "req-disable").await,
        vec![(
            "user.disabled".to_owned(),
            "succeeded".to_owned(),
            "user".to_owned(),
            created.user_id.get()
        )]
    );
    let details: serde_json::Value =
        sqlx::query_scalar("SELECT details FROM forge.audit_log WHERE request_id = 'req-disable'")
            .fetch_one(&mut su)
            .await
            .expect("details");
    assert_eq!(
        details,
        json!({
            "kind": "human",
            "revoked_tokens": 0,
            "revoked_sessions": 0,
            "forced": false,
            "ownerless_groups": 0,
            "ownerless_projects": 0,
            "left_without_admin": false,
        })
    );
    assert_eq!(rows_of(&mut su, "events").await, 0);
    assert_eq!(
        rows_of(&mut su, "namespaces").await,
        1,
        "the namespace stays"
    );
    t.drop_now().await;
}

#[tokio::test]
async fn disabling_an_already_disabled_user_is_a_no_op() {
    let t = FreshDb::migrated().await;
    let created = create_alice(&t, "req-create").await;
    let first = disable(&t.db, &ctx("req-disable-1"), created.user_id)
        .await
        .expect("first disable");
    assert_eq!(first, DisableOutcome::Disabled);
    let again = disable(&t.db, &ctx("req-disable-2"), created.user_id)
        .await
        .expect("second disable is not an error");
    assert_eq!(again, DisableOutcome::AlreadyDisabled);
    let mut su = t.superuser().await;
    assert!(audit_rows(&mut su, "req-disable-2").await.is_empty());
    assert_eq!(
        count(
            &mut su,
            "SELECT count(*) FROM forge.audit_log WHERE action = 'user.disabled'"
        )
        .await,
        1
    );
    assert_eq!(rows_of(&mut su, "events").await, 0);
    t.drop_now().await;
}

#[tokio::test]
async fn user_create_commits_nothing_when_the_namespace_insert_fails() {
    let t = FreshDb::migrated().await;
    t.exec_superuser(
        "CREATE FUNCTION forge.test_boom() RETURNS trigger LANGUAGE plpgsql \
         AS $$ BEGIN RAISE EXCEPTION 'boom'; END $$; \
         CREATE TRIGGER test_boom BEFORE INSERT ON forge.namespaces \
         FOR EACH ROW EXECUTE FUNCTION forge.test_boom()",
    )
    .await;
    let err = create(
        &t.db,
        &ctx("req-boom"),
        &NewUser::human("alice", "Alice A", EMAIL),
    )
    .await
    .expect_err("the namespace insert fails");
    assert_eq!(err.code(), ErrorCode::DbQueryFailed, "{err}");
    let mut su = t.superuser().await;
    assert_eq!(rows_of(&mut su, "users").await, 0, "no half-created user");
    assert_eq!(rows_of(&mut su, "namespaces").await, 0);
    assert!(audit_rows(&mut su, "req-boom").await.is_empty());
    assert_eq!(rows_of(&mut su, "events").await, 0);
    t.drop_now().await;
}

#[tokio::test]
async fn id_by_username_finds_the_user_and_an_unknown_name_is_not_found() {
    let t = FreshDb::migrated().await;
    let created = create(
        &t.db,
        &ctx("req-find-1"),
        &NewUser::human("alice", "Alice", EMAIL),
    )
    .await
    .expect("create");
    assert_eq!(
        id_by_username(&t.db, "alice").await.expect("found"),
        created.user_id
    );
    let err = id_by_username(&t.db, "nobody-here")
        .await
        .expect_err("unknown");
    assert_eq!(err.code(), ErrorCode::NotFound, "{err}");
    assert!(err.to_string().contains("resource=user"), "{err}");
    assert!(
        !err.to_string().contains("nobody-here"),
        "the name stays out: {err}"
    );
    t.drop_now().await;
}
