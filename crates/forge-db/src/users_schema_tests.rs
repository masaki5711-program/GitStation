//! The D1 schema (users, namespaces, namespace_tombstones; migration
//! 20261028100100) against the ledger and the approved column rules.
//! Inspection and probes run as superuser in transactions that are never
//! committed, so the deferred audit trigger does not fire and the
//! constraints under test are the only thing that can refuse a row.

use forge_core::path_vectors::{accepted, rejected};
use forge_errors::ErrorCode;
use sqlx::postgres::PgConnection;
use sqlx::Connection as _;

use crate::bootstrap::audited::{check_audited_triggers, AuditResource, AuditedTable, Ledger};
use crate::test_support::*;

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| (*x).to_owned()).collect()
}

/// The SQLSTATE of a failed statement (None = it succeeded).
async fn state_of(conn: &mut PgConnection, sql: &str, binds: &[&str]) -> Option<String> {
    let mut q = sqlx::query(sql);
    for b in binds {
        q = q.bind(*b);
    }
    match q.execute(conn).await {
        Ok(_) => None,
        Err(e) => Some(
            e.as_database_error()
                .and_then(|d| d.code())
                .map_or_else(|| format!("not a database error: {e}"), |c| c.into_owned()),
        ),
    }
}

const INSERT_USER: &str =
    "INSERT INTO forge.users (kind, username, display_name, email) VALUES ($1, $2, 'X', NULLIF($3, ''))";

#[tokio::test]
async fn every_audited_table_has_its_audit_trigger() {
    let _lock = ClusterLock::shared().await;
    let t = FreshDb::migrated().await;
    let ledger = Ledger::embedded().expect("tables.toml parses");
    let mut audited = ledger.audited.clone();
    audited.sort_by(|a, b| a.name.cmp(&b.name));
    assert_eq!(
        audited,
        vec![
            AuditedTable {
                name: "group_members".into(),
                audit_resource: AuditResource::Literal("group".into()),
                audit_id_column: "group_id".into(),
                unaudited_columns: s(&["updated_at"]),
                txn_audited_columns: vec![],
            },
            AuditedTable {
                name: "groups".into(),
                audit_resource: AuditResource::Literal("group".into()),
                audit_id_column: "id".into(),
                unaudited_columns: s(&["updated_at"]),
                txn_audited_columns: vec![],
            },
            AuditedTable {
                name: "namespace_tombstones".into(),
                audit_resource: AuditResource::Column("cause_type".into()),
                audit_id_column: "cause_id".into(),
                unaudited_columns: s(&["namespace_id", "parent_namespace_id"]),
                txn_audited_columns: vec![],
            },
            AuditedTable {
                name: "namespaces".into(),
                audit_resource: AuditResource::Column("kind".into()),
                audit_id_column: "owner_id".into(),
                unaudited_columns: vec![],
                txn_audited_columns: s(&["full_path", "depth"]),
            },
            AuditedTable {
                name: "tokens".into(),
                audit_resource: AuditResource::Literal("user".into()),
                audit_id_column: "user_id".into(),
                unaudited_columns: s(&["last_used_at"]),
                txn_audited_columns: vec![],
            },
            AuditedTable {
                name: "users".into(),
                audit_resource: AuditResource::Literal("user".into()),
                audit_id_column: "id".into(),
                unaudited_columns: s(&["updated_at"]),
                txn_audited_columns: vec![],
            },
        ]
    );
    // The triggers exist as the ledger describes them, and doctor accepts the
    // tables' owner, rules and exact ACL (they are in LATER_TABLES and
    // expectations(); `audited_table_unlisted` otherwise).
    let mut su = t.superuser().await;
    check_audited_triggers(&mut su, &ledger)
        .await
        .expect("triggers match the ledger");
    crate::bootstrap::check(&t.db, &[])
        .await
        .expect("doctor accepts the D1 tables");
    t.drop_now().await;
}

#[tokio::test]
async fn the_db_check_rejects_what_the_parser_rejects() {
    let t = FreshDb::migrated().await;
    let mut su = t.superuser().await;
    let insert = "INSERT INTO forge.namespaces (kind, segment, full_path, depth, owner_id) \
                  VALUES ('group', $1, $1, 1, 1)";
    for good in accepted() {
        let mut tx = su.begin().await.expect("begin");
        let state = state_of(&mut tx, insert, &[good.as_str()]).await;
        assert_eq!(state, None, "the database must accept {good:?}");
        tx.rollback().await.expect("rollback");
    }
    for bad in rejected() {
        let mut tx = su.begin().await.expect("begin");
        let state = state_of(&mut tx, insert, &[bad.as_str()]).await;
        // 23514 check_violation; 22021 for a NUL the server refuses earlier.
        assert!(
            matches!(state.as_deref(), Some("23514" | "22021")),
            "the database must refuse {bad:?}: {state:?}"
        );
        tx.rollback().await.expect("rollback");
    }
    // The other approved limits on the same table.
    for (what, sql) in [
        (
            "depth 21",
            "INSERT INTO forge.namespaces (kind, segment, full_path, depth, owner_id) \
             VALUES ('group', 'a', 'a', 21, 1)",
        ),
        (
            "full_path over 255",
            "INSERT INTO forge.namespaces (kind, segment, full_path, depth, owner_id) \
             VALUES ('group', 'a', repeat('a', 256), 1, 1)",
        ),
        (
            "unknown kind",
            "INSERT INTO forge.namespaces (kind, segment, full_path, depth, owner_id) \
             VALUES ('device', 'a', 'a', 1, 1)",
        ),
    ] {
        let mut tx = su.begin().await.expect("begin");
        let state = state_of(&mut tx, sql, &[]).await;
        assert_eq!(state.as_deref(), Some("23514"), "{what}");
        tx.rollback().await.expect("rollback");
    }
    // Siblings: UNIQUE NULLS NOT DISTINCT (parent_id, segment) at the top level.
    let mut tx = su.begin().await.expect("begin");
    assert_eq!(state_of(&mut tx, insert, &["acme"]).await, None);
    let again = "INSERT INTO forge.namespaces (kind, segment, full_path, depth, owner_id) \
                 VALUES ('user', 'acme', 'acme', 1, 2)";
    assert_eq!(
        state_of(&mut tx, again, &[]).await.as_deref(),
        Some("23505"),
        "two top-level namespaces with one segment"
    );
    tx.rollback().await.expect("rollback");
    drop(su);
    t.drop_now().await;
}
/// Run one user insert in a transaction that is rolled back; the SQLSTATE.
async fn user_insert(
    su: &mut PgConnection,
    kind: &str,
    username: &str,
    email: &str,
    steps: &[(&str, &str, &str, Option<&str>)],
) -> Option<String> {
    let mut tx = su.begin().await.expect("begin");
    for (k, u, e, expected) in steps {
        let state = state_of(&mut tx, INSERT_USER, &[k, u, e]).await;
        assert_eq!(state.as_deref(), *expected, "{k} {u}");
    }
    let state = state_of(&mut tx, INSERT_USER, &[kind, username, email]).await;
    tx.rollback().await.expect("rollback");
    state
}

#[tokio::test]
async fn human_email_is_unique_ignoring_case() {
    let t = FreshDb::migrated().await;
    let mut su = t.superuser().await;
    let first = [("human", "u1", "a@example.test", None)];
    assert_eq!(
        user_insert(&mut su, "human", "u2", "A@Example.TEST", &first)
            .await
            .as_deref(),
        Some("23505")
    );
    // A different address is fine.
    assert_eq!(
        user_insert(&mut su, "human", "u2", "b@example.test", &first).await,
        None
    );
    // A human needs an email; accounts that are not people do not.
    assert_eq!(
        user_insert(&mut su, "human", "u3", "", &[])
            .await
            .as_deref(),
        Some("23514")
    );
    let svc = [
        ("service_account", "svc1", "", None),
        ("service_account", "svc2", "", None),
    ];
    assert_eq!(user_insert(&mut su, "bot", "bot1", "", &svc).await, None);
    drop(su);
    t.drop_now().await;
}

#[tokio::test]
async fn employee_number_is_unique_when_present() {
    let t = FreshDb::migrated().await;
    let mut su = t.superuser().await;
    let insert = "INSERT INTO forge.users (kind, username, display_name, email, employee_number) \
                  VALUES ('human', $1, 'X', $2, NULLIF($3, ''))";
    let mut tx = su.begin().await.expect("begin");
    for (user, number, expected) in [
        ("e1", "N-100", None),
        ("e2", "", None),
        ("e3", "", None),
        ("e4", "N-100", Some("23505")),
    ] {
        let email = format!("{user}@example.test");
        let state = state_of(&mut tx, insert, &[user, &email, number]).await;
        assert_eq!(state.as_deref(), expected, "{user}");
        if state.is_some() {
            break;
        }
    }
    tx.rollback().await.expect("rollback");
    drop(su);
    t.drop_now().await;
}

#[tokio::test]
async fn the_database_rejects_a_device_user_kind() {
    let t = FreshDb::migrated().await;
    let mut su = t.superuser().await;
    for kind in ["device", "Human", ""] {
        let mut tx = su.begin().await.expect("begin");
        let state = state_of(&mut tx, INSERT_USER, &[kind, "d1", "d1@example.test"]).await;
        assert_eq!(state.as_deref(), Some("23514"), "{kind:?}");
        tx.rollback().await.expect("rollback");
    }
    drop(su);
    t.drop_now().await;
}

#[tokio::test]
async fn the_read_pool_cannot_read_user_email() {
    let t = FreshDb::migrated().await;
    let mut su = t.superuser().await;
    let columns: Vec<String> = sqlx::query_scalar(
        "SELECT column_name::text FROM information_schema.columns \
         WHERE table_schema = 'forge' AND table_name = 'users' ORDER BY ordinal_position",
    )
    .fetch_all(&mut su)
    .await
    .expect("columns");
    for needed in [
        "id",
        "username",
        "display_name",
        "state",
        "email",
        "employee_number",
    ] {
        assert!(columns.iter().any(|c| c == needed), "forge.users.{needed}");
    }
    assert!(
        columns.iter().any(|c| c == "external_id"),
        "forge.users.external_id"
    );
    let read = t.db.pool().executor();
    // Every column but the three personal ones is readable.
    let readable: Vec<&str> = columns
        .iter()
        .map(String::as_str)
        .filter(|c| !matches!(*c, "email" | "employee_number" | "external_id"))
        .collect();
    sqlx::query(&format!("SELECT {} FROM forge.users", readable.join(", ")))
        .fetch_all(read)
        .await
        .expect("every non-personal column");
    for personal in ["email", "employee_number", "external_id", "*"] {
        let err = sqlx::query(&format!("SELECT {personal} FROM forge.users"))
            .fetch_all(read)
            .await
            .map_err(|e| crate::read_query_error("test.pii", e))
            .expect_err(personal);
        assert_reason(&err, ErrorCode::DbQueryFailed, "permission_denied");
    }
    // lower(email) in a WHERE clause reads the column too.
    let err = sqlx::query("SELECT id FROM forge.users WHERE lower(email) = 'x'")
        .fetch_all(read)
        .await
        .map_err(|e| crate::read_query_error("test.pii", e))
        .expect_err("email in a predicate");
    assert_reason(&err, ErrorCode::DbQueryFailed, "permission_denied");
    drop(su);
    t.drop_now().await;
}

#[tokio::test]
async fn a_truncate_grant_on_an_audited_table_is_reported() {
    let _lock = ClusterLock::shared().await;
    let t = FreshDb::migrated().await;
    crate::bootstrap::check(&t.db, &[])
        .await
        .expect("doctor is clean before the grant");
    for table in ["users", "namespaces", "namespace_tombstones"] {
        t.exec_superuser(&format!("GRANT TRUNCATE ON forge.{table} TO forge_app"))
            .await;
        let text = crate::bootstrap::check(&t.db, &[])
            .await
            .expect_err(table)
            .to_string();
        assert!(text.contains("guard=privileges"), "{table}: {text}");
        assert!(text.contains(&format!("forge.{table}")), "{table}: {text}");
        t.exec_superuser(&format!("REVOKE TRUNCATE ON forge.{table} FROM forge_app"))
            .await;
    }
    // The ACLs are shipped again, but the grants themselves were DDL outside
    // a migration: doctor keeps reporting them (ddl_audit).
    let err = crate::bootstrap::check(&t.db, &[])
        .await
        .expect_err("the manual grants stay on record");
    assert_reason(&err, ErrorCode::AppendOnlyViolation, "unexpected_ddl");
    t.drop_now().await;
}
