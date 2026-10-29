//! The D2 schema (groups, group_members; migration 20261029100000): audit
//! enforcement, constraints and privileges. Probes as forge_app commit
//! (where the deferred trigger fires); constraint probes run as superuser
//! and are rolled back.

use sqlx::Connection as _;

use crate::audited_tests::{assert_refused, audit, commit, sql};
use crate::test_support::*;

/// User 1 and group 1, with their audit rows.
fn seed() -> Vec<String> {
    vec![
        audit("user", 1, "succeeded"),
        sql("INSERT INTO forge.users (id, kind, username, display_name, email) \
             OVERRIDING SYSTEM VALUE VALUES (1, 'human', 'alice', 'A', 'alice@example.test')"),
        audit("group", 1, "succeeded"),
        sql("INSERT INTO forge.groups (id, display_name) OVERRIDING SYSTEM VALUE VALUES (1, 'Acme')"),
    ]
}

#[tokio::test]
async fn a_member_change_without_its_audit_row_cannot_commit() {
    let t = FreshDb::migrated().await;
    commit(&t, &seed()).await.expect("seed");
    let add =
        sql("INSERT INTO forge.group_members (group_id, user_id, role) VALUES (1, 1, 'owner')");
    let err = commit(&t, std::slice::from_ref(&add))
        .await
        .expect_err("no audit row");
    assert_refused(&err, "group_members");
    // An audit row of another group does not cover it.
    let err = commit(&t, &[audit("group", 2, "succeeded"), add.clone()])
        .await
        .expect_err("other group");
    assert_refused(&err, "group_members");
    commit(&t, &[audit("group", 1, "succeeded"), add])
        .await
        .expect("with its audit row");
    for change in [
        "UPDATE forge.group_members SET role = 'guest'",
        "DELETE FROM forge.group_members",
        "UPDATE forge.groups SET display_name = 'Other'",
    ] {
        let table = if change.contains("groups SET") {
            "groups"
        } else {
            "group_members"
        };
        let err = commit(&t, &[sql(change)]).await.expect_err(change);
        assert_refused(&err, table);
    }
    commit(
        &t,
        &[sql("UPDATE forge.group_members SET updated_at = now()")],
    )
    .await
    .expect("updated_at is unaudited");
    t.drop_now().await;
}

#[tokio::test]
async fn a_group_cannot_be_deleted_by_forge_app() {
    let t = FreshDb::migrated().await;
    commit(&t, &seed()).await.expect("seed");
    let err = commit(
        &t,
        &[
            audit("group", 1, "succeeded"),
            sql("DELETE FROM forge.groups"),
        ],
    )
    .await
    .expect_err("no DELETE grant (Q11)");
    assert_reason(
        &err,
        forge_errors::ErrorCode::DbQueryFailed,
        "permission_denied",
    );
    t.drop_now().await;
}

#[tokio::test]
async fn the_d2_constraints_refuse_their_rows() {
    let t = FreshDb::migrated().await;
    commit(&t, &seed()).await.expect("seed");
    let mut su = t.superuser().await;
    for (what, probe, state) in [
        (
            "unknown role",
            "INSERT INTO forge.group_members VALUES (1, 1, 'admin')",
            "23514",
        ),
        (
            "unknown group",
            "INSERT INTO forge.group_members (group_id, user_id, role) VALUES (9, 1, 'owner')",
            "23503",
        ),
        (
            "unknown user",
            "INSERT INTO forge.group_members (group_id, user_id, role) VALUES (1, 9, 'owner')",
            "23503",
        ),
        (
            "empty name",
            "INSERT INTO forge.groups (display_name) VALUES ('')",
            "23514",
        ),
        (
            "long description",
            "INSERT INTO forge.groups (display_name, description) VALUES ('A', repeat('x', 2001))",
            "23514",
        ),
        (
            "a bot member",
            "INSERT INTO forge.users (id, kind, username, display_name) OVERRIDING SYSTEM VALUE \n             VALUES (2, 'bot', 'ci-bot', 'CI'); \n             INSERT INTO forge.group_members (group_id, user_id, role) VALUES (1, 2, 'guest')",
            "23514",
        ),
    ] {
        let mut tx = su.begin().await.expect("begin");
        let code = sqlx::raw_sql(probe)
            .execute(&mut *tx)
            .await
            .err()
            .and_then(|e| {
                e.as_database_error()
                    .and_then(|d| d.code())
                    .map(|c| c.into_owned())
            });
        tx.rollback().await.expect("rollback");
        assert_eq!(code.as_deref(), Some(state), "{what}");
    }
    let mut tx = su.begin().await.expect("begin");
    sqlx::raw_sql(
        "INSERT INTO forge.group_members (group_id, user_id, role) VALUES (1, 1, 'owner'); \
         INSERT INTO forge.group_members (group_id, user_id, role) VALUES (1, 1, 'guest')",
    )
    .execute(&mut *tx)
    .await
    .expect_err("one row per member");
    tx.rollback().await.expect("rollback");
    drop(su);
    t.drop_now().await;
}
