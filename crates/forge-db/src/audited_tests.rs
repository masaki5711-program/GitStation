//! `forge.audited_change_present` (D1): a change to an audited table commits
//! only together with an audit row of the same transaction for the same
//! resource. Probe tables are created as superuser in a fresh, migrated
//! database; each test groups several cases in one database.
//!
//! Trigger arguments (TG_ARGV, in this order):
//! 0 resource type (literal, or `=column`), 1 id column,
//! 2 unaudited columns (comma separated, may be empty),
//! 3 txn-audited columns (comma separated, may be empty).

use forge_errors::{ErrorCode, ForgeError};
use sqlx::postgres::PgConnection;

use crate::bootstrap::audited::{AuditResource, AuditedTable, Ledger};
use crate::query_error;
use crate::test_support::*;

pub(crate) const SECRET: &str = "sentinel-row-value-7731";

const PROBE_DDL: &str = "\
CREATE TABLE forge.audit_probe (
    id bigint PRIMARY KEY, name text, token_hash text, path text, last_used_at timestamptz);
GRANT SELECT, INSERT, UPDATE, DELETE ON forge.audit_probe TO forge_app;
CREATE TABLE forge.audit_probe_kinded (id bigint PRIMARY KEY, kind text, note text);
GRANT SELECT, INSERT, UPDATE, DELETE ON forge.audit_probe_kinded TO forge_app;";

pub(crate) const PROBE_TRIGGER: &str = "\
CREATE CONSTRAINT TRIGGER audited_change_present
    AFTER INSERT OR DELETE OR UPDATE OF id, name, token_hash, path ON forge.audit_probe
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
    EXECUTE FUNCTION forge.audited_change_present('probe', 'id', 'last_used_at', 'path');
ALTER TABLE forge.audit_probe ENABLE ALWAYS TRIGGER audited_change_present;";

const KINDED_TRIGGER: &str = "\
CREATE CONSTRAINT TRIGGER audited_change_present
    AFTER INSERT OR DELETE OR UPDATE OF id, kind, note ON forge.audit_probe_kinded
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
    EXECUTE FUNCTION forge.audited_change_present('=kind', 'id', '', '');
ALTER TABLE forge.audit_probe_kinded ENABLE ALWAYS TRIGGER audited_change_present;";

pub(crate) fn probe_ledger() -> Ledger {
    let s = |v: &[&str]| v.iter().map(|x| (*x).to_owned()).collect::<Vec<_>>();
    Ledger {
        guarded_functions: s(&["audited_change_present"]),
        audited: vec![
            AuditedTable {
                name: "audit_probe".into(),
                audit_resource: AuditResource::Literal("probe".into()),
                audit_id_column: "id".into(),
                unaudited_columns: s(&["last_used_at"]),
                txn_audited_columns: s(&["path"]),
            },
            AuditedTable {
                name: "audit_probe_kinded".into(),
                audit_resource: AuditResource::Column("kind".into()),
                audit_id_column: "id".into(),
                unaudited_columns: vec![],
                txn_audited_columns: vec![],
            },
        ],
    }
}

pub(crate) async fn exec(conn: &mut PgConnection, sql: &str) {
    sqlx::raw_sql(sql)
        .execute(conn)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
}

/// A migrated database with both probe tables and their triggers.
pub(crate) async fn probe_db() -> FreshDb {
    let t = FreshDb::migrated().await;
    let mut su = t.superuser().await;
    exec(&mut su, PROBE_DDL).await;
    exec(&mut su, PROBE_TRIGGER).await;
    exec(&mut su, KINDED_TRIGGER).await;
    t
}

pub(crate) fn audit(resource: &str, id: i64, outcome: &str) -> String {
    format!(
        "INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, resource_id, \
         request_id) VALUES ('system', 'probe.changed', '{outcome}', '{resource}', {id}, 'r-1')"
    )
}

/// One transaction as forge_app: the statements, then COMMIT (where the
/// deferred trigger fires).
pub(crate) async fn commit(t: &FreshDb, stmts: &[String]) -> Result<(), ForgeError> {
    let mut tx =
        t.db.write_pool()
            .begin()
            .await
            .map_err(|e| query_error("test.begin", e))?;
    for sql in stmts {
        sqlx::raw_sql(sql)
            .execute(&mut *tx)
            .await
            .map_err(|e| query_error("test.statement", e))?;
    }
    tx.commit().await.map_err(|e| query_error("test.commit", e))
}

pub(crate) fn sql(s: &str) -> String {
    s.to_owned()
}

pub(crate) fn assert_refused(err: &ForgeError, table: &str) {
    assert_reason(err, ErrorCode::AuditMissing, "audited_change");
    let text = chain(err);
    assert!(text.contains("sqlstate=XF002"), "{text}");
    assert!(text.contains(&format!("table={table}")), "{text}");
    assert!(!text.contains(SECRET), "no row data in the error: {text}");
}

/// Seed a row together with its audit row.
pub(crate) async fn seed(t: &FreshDb, id: i64) {
    commit(
        t,
        &[
            sql(&format!(
                "INSERT INTO forge.audit_probe (id, name, token_hash, path) \
                 VALUES ({id}, 'a', 'h', 'p/{id}')"
            )),
            audit("probe", id, "succeeded"),
        ],
    )
    .await
    .expect("seed with its audit row");
}

#[tokio::test]
async fn a_state_change_without_its_audit_row_cannot_commit() {
    let t = probe_db().await;
    // Insert, update and delete each need their audit row.
    let err = commit(
        &t,
        &[sql(&format!(
            "INSERT INTO forge.audit_probe (id, name) VALUES (1, '{SECRET}')"
        ))],
    )
    .await
    .expect_err("insert without audit");
    assert_refused(&err, "audit_probe");
    seed(&t, 2).await;
    let err = commit(
        &t,
        &[sql(&format!(
            "UPDATE forge.audit_probe SET name = '{SECRET}' WHERE id = 2"
        ))],
    )
    .await
    .expect_err("update without audit");
    assert_refused(&err, "audit_probe");
    let err = commit(&t, &[sql("DELETE FROM forge.audit_probe WHERE id = 2")])
        .await
        .expect_err("delete without audit");
    assert_refused(&err, "audit_probe");
    // With the audit row the same change commits.
    commit(
        &t,
        &[
            sql("UPDATE forge.audit_probe SET name = 'b' WHERE id = 2"),
            audit("probe", 2, "succeeded"),
        ],
    )
    .await
    .expect("update with its audit row");
    t.drop_now().await;
}

#[tokio::test]
async fn an_audit_row_for_another_resource_does_not_cover_a_change() {
    let t = probe_db().await;
    seed(&t, 1).await;
    for (resource, id) in [("probe", 99), ("other", 1)] {
        let err = commit(
            &t,
            &[
                sql("UPDATE forge.audit_probe SET name = 'b' WHERE id = 1"),
                audit(resource, id, "succeeded"),
            ],
        )
        .await
        .expect_err("another resource's audit row");
        assert_refused(&err, "audit_probe");
    }
    // The audit row of the seeding transaction does not count either.
    let err = commit(
        &t,
        &[sql("UPDATE forge.audit_probe SET name = 'c' WHERE id = 1")],
    )
    .await
    .expect_err("stale audit row");
    assert_refused(&err, "audit_probe");
    t.drop_now().await;
}

#[tokio::test]
async fn a_denied_audit_row_does_not_cover_a_change() {
    let t = probe_db().await;
    seed(&t, 1).await;
    for outcome in ["denied", "failed"] {
        let err = commit(
            &t,
            &[
                sql("UPDATE forge.audit_probe SET name = 'b' WHERE id = 1"),
                audit("probe", 1, outcome),
            ],
        )
        .await
        .expect_err(outcome);
        assert_refused(&err, "audit_probe");
    }
    t.drop_now().await;
}

#[tokio::test]
async fn declared_unaudited_columns_need_no_audit_row() {
    let t = probe_db().await;
    seed(&t, 1).await;
    commit(
        &t,
        &[sql(
            "UPDATE forge.audit_probe SET last_used_at = now() WHERE id = 1",
        )],
    )
    .await
    .expect("last_used_at is unaudited");
    t.drop_now().await;
}

#[tokio::test]
async fn a_mixed_update_including_a_new_column_needs_an_audit_row() {
    let t = probe_db().await;
    seed(&t, 1).await;
    let mixed =
        "UPDATE forge.audit_probe SET last_used_at = now(), token_hash = 'new' WHERE id = 1";
    let err = commit(&t, &[sql(mixed)])
        .await
        .expect_err("token_hash changed too");
    assert_refused(&err, "audit_probe");
    commit(&t, &[sql(mixed), audit("probe", 1, "succeeded")])
        .await
        .expect("with its audit row");
    t.drop_now().await;
}

#[tokio::test]
async fn a_path_rewrite_needs_an_audit_row_in_the_transaction() {
    let t = probe_db().await;
    seed(&t, 1).await;
    seed(&t, 2).await;
    let err = commit(&t, &[sql("UPDATE forge.audit_probe SET path = 'moved'")])
        .await
        .expect_err("no audit row at all");
    assert_refused(&err, "audit_probe");
    // Any audit row of the transaction covers a txn-audited-only change
    // (a bulk rewrite is audited once, not once per row).
    commit(
        &t,
        &[
            sql("UPDATE forge.audit_probe SET path = 'moved'"),
            audit("probe", 1, "succeeded"),
        ],
    )
    .await
    .expect("one audit row covers the rewrite");
    // ... but not when an audited column changed as well.
    let err = commit(
        &t,
        &[
            sql("UPDATE forge.audit_probe SET path = 'again', name = 'z' WHERE id = 2"),
            audit("probe", 1, "succeeded"),
        ],
    )
    .await
    .expect_err("name changed for row 2, audit row is for row 1");
    assert_refused(&err, "audit_probe");
    t.drop_now().await;
}

#[tokio::test]
async fn an_unchanged_update_needs_no_audit_row() {
    let t = probe_db().await;
    seed(&t, 1).await;
    commit(
        &t,
        &[sql("UPDATE forge.audit_probe SET name = name WHERE id = 1")],
    )
    .await
    .expect("no change, no audit row");
    t.drop_now().await;
}

#[tokio::test]
async fn the_resource_type_can_be_read_from_the_row() {
    let t = probe_db().await;
    let insert = "INSERT INTO forge.audit_probe_kinded (id, kind, note) VALUES (1, 'alpha', 'n')";
    let err = commit(&t, &[sql(insert), audit("beta", 1, "succeeded")])
        .await
        .expect_err("audit row names another resource type");
    assert_refused(&err, "audit_probe_kinded");
    commit(&t, &[sql(insert), audit("alpha", 1, "succeeded")])
        .await
        .expect("audit row of the row's own kind");
    t.drop_now().await;
}
