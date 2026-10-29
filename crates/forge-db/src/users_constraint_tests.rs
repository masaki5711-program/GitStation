//! The D1 tables' constraints, privileges and audit triggers, one by name
//! (migration 20261028100100). Probes run as superuser in transactions that
//! are rolled back; the audit probes commit as forge_app.

use sqlx::postgres::PgConnection;
use sqlx::Connection as _;

use crate::audited_tests::{assert_refused, audit, commit, sql};
use crate::test_support::*;

const GROUP: &str = "INSERT INTO forge.namespaces (kind, owner_id, segment, full_path, depth) \
                     VALUES ('group', 7, 'acme', 'acme', 1)";
/// User 1, whatever the identity sequence says (a refused insert uses a value).
const USER: &str = "INSERT INTO forge.users (id, kind, username, display_name, email) \
                    OVERRIDING SYSTEM VALUE VALUES (1, 'human', 'alice', 'A', 'alice@example.test')";

/// Run `setup`, then `probe`, in a transaction that is rolled back: the
/// SQLSTATE and constraint name of the probe's failure (None = it worked).
async fn refusal(su: &mut PgConnection, setup: &str, probe: &str) -> Option<(String, String)> {
    let mut tx = su.begin().await.expect("begin");
    if !setup.is_empty() {
        sqlx::raw_sql(setup).execute(&mut *tx).await.expect(setup);
    }
    let result = sqlx::query(probe).execute(&mut *tx).await;
    tx.rollback().await.expect("rollback");
    let e = result.err()?;
    let db = e.as_database_error().expect("a database error");
    Some((
        db.code().map(|c| c.into_owned()).unwrap_or_default(),
        db.constraint().unwrap_or_default().to_owned(),
    ))
}

#[tokio::test]
async fn each_d1_constraint_refuses_its_rows() {
    let t = FreshDb::migrated().await;
    let mut su = t.superuser().await;
    let user = |cols: &str, vals: &str| {
        format!("INSERT INTO forge.users (kind, username, display_name{cols}) VALUES ('service_account', 'svc', 'S'{vals})")
    };
    let tombstone = |cols: &str, vals: &str| {
        format!("INSERT INTO forge.namespace_tombstones (old_path, kind, cause_type, cause_id{cols}) VALUES ('old', 'deleted', 'user', 1{vals})")
    };
    let cases: Vec<(&str, String, String, &str, &str)> = vec![
        (
            "",
            user("", "").replace("'svc'", "'Svc'"),
            "23514".into(),
            "users_username",
            "username",
        ),
        (
            "",
            user("", "").replace("'svc'", "'x.git'"),
            "23514".into(),
            "users_username",
            "suffix",
        ),
        (
            "",
            user("", "").replace("'S'", "''"),
            "23514".into(),
            "users_display_name_length",
            "empty name",
        ),
        (
            "",
            user(", email", ", ''"),
            "23514".into(),
            "users_email_length",
            "empty email",
        ),
        (
            "",
            user(", employee_number", ", ''"),
            "23514".into(),
            "users_employee_number_length",
            "empty number",
        ),
        (
            "",
            user(", external_id", ", ''"),
            "23514".into(),
            "users_external_id_length",
            "empty external id",
        ),
        (
            "",
            user("", "").replace("'service_account'", "'human'"),
            "23514".into(),
            "users_human_email",
            "human without email",
        ),
        (
            "INSERT INTO forge.users (kind, username, display_name, auth_source, external_id) \
             VALUES ('bot', 'b1', 'B', 'ldap', 'uid-1')",
            user(", auth_source, external_id", ", 'ldap', 'uid-1'"),
            "23505".into(),
            "users_external_id",
            "external id per source",
        ),
        (
            "",
            "INSERT INTO forge.namespaces (kind, owner_id, segment, full_path, depth) \
             VALUES ('group', 7, 'acme', 'other', 1)"
                .into(),
            "23514".into(),
            "namespaces_top_level_path",
            "top-level full_path is the segment",
        ),
        (
            "",
            tombstone("", "").replace("'deleted'", "'renamed'"),
            "23514".into(),
            "namespace_tombstones_kind",
            "kind",
        ),
        (
            "",
            tombstone(", namespace_id", ", 999"),
            "23503".into(),
            "namespace_tombstones_namespace_id_fkey",
            "target fk",
        ),
        (
            "",
            tombstone(", parent_namespace_id", ", 999"),
            "23503".into(),
            "namespace_tombstones_parent_namespace_id_fkey",
            "parent fk",
        ),
        (
            "",
            tombstone(", owner_at_write", ", 999"),
            "23503".into(),
            "namespace_tombstones_owner_at_write_fkey",
            "owner fk",
        ),
    ];
    for (setup, probe, state, constraint, what) in cases {
        assert_eq!(
            refusal(&mut su, setup, &probe).await,
            Some((state, constraint.to_owned())),
            "{what}"
        );
    }
    // The same rows with valid values are accepted.
    for ok in [
        user("", ""),
        tombstone("", ""),
        GROUP.to_owned(),
        USER.to_owned(),
    ] {
        assert_eq!(refusal(&mut su, "", &ok).await, None, "{ok}");
    }
    drop(su);
    t.drop_now().await;
}

/// The real tables refuse a change without its audit row (FG-DB-0013).
#[tokio::test]
async fn the_d1_tables_refuse_a_change_without_its_audit_row() {
    let t = FreshDb::migrated().await;
    let refused = [
        ("users", vec![sql(USER)]),
        ("namespaces", vec![sql(GROUP)]),
        (
            "namespace_tombstones",
            vec![sql(
                "INSERT INTO forge.namespace_tombstones (old_path, kind, cause_type, cause_id) \
                      VALUES ('old', 'deleted', 'group', 7)",
            )],
        ),
        // An audit row for another resource does not cover the namespace.
        (
            "namespaces",
            vec![audit("group", 8, "succeeded"), sql(GROUP)],
        ),
    ];
    for (table, stmts) in refused {
        let err = commit(&t, &stmts).await.expect_err(table);
        assert_refused(&err, table);
    }
    // With the audit row the same writes commit.
    commit(&t, &[audit("user", 1, "succeeded"), sql(USER)])
        .await
        .expect("user with its audit row");
    // A state change of the user needs its row; updated_at alone does not.
    let err = commit(&t, &[sql("UPDATE forge.users SET state = 'disabled'")])
        .await
        .expect_err("state");
    assert_refused(&err, "users");
    commit(&t, &[sql("UPDATE forge.users SET updated_at = now()")])
        .await
        .expect("updated_at is unaudited");
    t.drop_now().await;
}

/// Purging a namespace clears the tombstones' targets (ON DELETE SET NULL)
/// without an audit row of the tombstone's cause: those columns are
/// unaudited, and forge_app cannot set them.
#[tokio::test]
async fn purging_a_namespace_clears_tombstone_targets() {
    let t = FreshDb::migrated().await;
    commit(
        &t,
        &[
            audit("group", 7, "succeeded"),
            sql(GROUP),
            audit("user", 1, "succeeded"),
            sql(
                "INSERT INTO forge.namespace_tombstones (old_path, kind, namespace_id, \
                 parent_namespace_id, cause_type, cause_id) SELECT 'old', 'moved', id, id, \
                 'user', 1 FROM forge.namespaces",
            ),
        ],
    )
    .await
    .expect("seed");
    t.exec_superuser(&format!(
        "BEGIN; {}; DELETE FROM forge.namespaces; COMMIT",
        audit("group", 7, "succeeded")
    ))
    .await;
    let mut su = t.superuser().await;
    let targets: (Option<i64>, Option<i64>) =
        sqlx::query_as("SELECT namespace_id, parent_namespace_id FROM forge.namespace_tombstones")
            .fetch_one(&mut su)
            .await
            .expect("the tombstone stays");
    assert_eq!(targets, (None, None));
    drop(su);
    t.drop_now().await;
}

#[tokio::test]
async fn the_application_can_only_insert_and_delete_tombstones() {
    let t = FreshDb::migrated().await;
    let mut su = t.superuser().await;
    let mut tx = su.begin().await.expect("begin");
    sqlx::raw_sql(
        "SET LOCAL ROLE forge_app; \
         SELECT old_path FROM forge.namespace_tombstones; \
         INSERT INTO forge.namespace_tombstones (old_path, kind, cause_type, cause_id) \
         VALUES ('old', 'deleted', 'user', 1); \
         DELETE FROM forge.namespace_tombstones WHERE old_path = 'old'",
    )
    .execute(&mut *tx)
    .await
    .expect("forge_app may read, insert and delete tombstones");
    tx.rollback().await.expect("rollback");
    for probe in [
        "UPDATE forge.namespace_tombstones SET old_path = 'x'",
        "UPDATE forge.namespace_tombstones SET namespace_id = NULL",
        "TRUNCATE forge.namespace_tombstones",
    ] {
        let mut tx = su.begin().await.expect("begin");
        sqlx::raw_sql("SET LOCAL ROLE forge_app")
            .execute(&mut *tx)
            .await
            .expect("role");
        let state = sqlx::raw_sql(probe)
            .execute(&mut *tx)
            .await
            .err()
            .and_then(|e| {
                e.as_database_error()
                    .and_then(|d| d.code())
                    .map(|c| c.into_owned())
            });
        tx.rollback().await.expect("rollback");
        assert_eq!(state.as_deref(), Some("42501"), "{probe}");
    }
    // The role was transaction-local: the connection is the superuser again.
    let back: bool = sqlx::query_scalar("SELECT current_user = session_user")
        .fetch_one(&mut su)
        .await
        .expect("current_user");
    assert!(back, "SET LOCAL ROLE ended with the transaction");
    drop(su);
    t.drop_now().await;
}
