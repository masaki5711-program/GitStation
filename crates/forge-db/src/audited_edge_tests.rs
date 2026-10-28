//! `forge.audited_change_present` (D1), the less common paths: deletes,
//! denied rows on the txn-audited path, a missing resource type, a changed
//! resource, and triggers the ledger does not describe.

use forge_errors::ErrorCode;

use crate::audited_tests::{assert_refused, audit, commit, exec, probe_db, seed, sql, SECRET};
use crate::test_support::*;

#[tokio::test]
async fn a_delete_with_its_audit_row_commits() {
    let t = probe_db().await;
    seed(&t, 1).await;
    commit(
        &t,
        &[
            sql("DELETE FROM forge.audit_probe WHERE id = 1"),
            audit("probe", 1, "succeeded"),
        ],
    )
    .await
    .expect("delete with its audit row");
    t.drop_now().await;
}

#[tokio::test]
async fn a_path_rewrite_with_only_a_denied_audit_row_is_refused() {
    let t = probe_db().await;
    seed(&t, 1).await;
    let err = commit(
        &t,
        &[
            sql("UPDATE forge.audit_probe SET path = 'moved'"),
            audit("probe", 1, "denied"),
        ],
    )
    .await
    .expect_err("a denied audit row covers nothing");
    assert_refused(&err, "audit_probe");
    t.drop_now().await;
}

#[tokio::test]
async fn a_row_without_its_resource_type_is_refused() {
    let t = probe_db().await;
    let err = commit(
        &t,
        &[
            sql("INSERT INTO forge.audit_probe_kinded (id, kind, note) VALUES (1, NULL, 'n')"),
            audit("alpha", 1, "succeeded"),
        ],
    )
    .await
    .expect_err("NULL kind matches no audit row");
    assert_refused(&err, "audit_probe_kinded");
    t.drop_now().await;
}

#[tokio::test]
async fn a_changed_resource_needs_audit_rows_for_the_old_and_the_new_one() {
    let t = probe_db().await;
    seed(&t, 1).await;
    let rekey = "UPDATE forge.audit_probe SET id = 2 WHERE id = 1";
    let err = commit(&t, &[sql(rekey), audit("probe", 2, "succeeded")])
        .await
        .expect_err("only the new id is audited");
    assert_refused(&err, "audit_probe");
    commit(
        &t,
        &[
            sql(rekey),
            audit("probe", 1, "succeeded"),
            audit("probe", 2, "succeeded"),
        ],
    )
    .await
    .expect("both ids audited");
    // The resource type read from the row: a re-typed row needs both too.
    commit(
        &t,
        &[
            sql("INSERT INTO forge.audit_probe_kinded (id, kind, note) VALUES (1, 'alpha', 'n')"),
            audit("alpha", 1, "succeeded"),
        ],
    )
    .await
    .expect("seed kinded");
    let retype = "UPDATE forge.audit_probe_kinded SET kind = 'beta' WHERE id = 1";
    let err = commit(&t, &[sql(retype), audit("beta", 1, "succeeded")])
        .await
        .expect_err("only the new type is audited");
    assert_refused(&err, "audit_probe_kinded");
    t.drop_now().await;
}

#[tokio::test]
async fn a_trigger_the_ledger_does_not_describe_is_refused_as_config() {
    let t = probe_db().await;
    let mut su = t.superuser().await;
    exec(
        &mut su,
        "CREATE TABLE forge.audit_probe_bad (id bigint PRIMARY KEY, label text);
         GRANT SELECT, INSERT ON forge.audit_probe_bad TO forge_app;
         CREATE CONSTRAINT TRIGGER three_args AFTER INSERT ON forge.audit_probe_bad
             DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
             EXECUTE FUNCTION forge.audited_change_present('bad', 'id', '');
         CREATE TABLE forge.audit_probe_text (code text PRIMARY KEY);
         GRANT SELECT, INSERT ON forge.audit_probe_text TO forge_app;
         CREATE CONSTRAINT TRIGGER text_id AFTER INSERT ON forge.audit_probe_text
             DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
             EXECUTE FUNCTION forge.audited_change_present('text', 'code', '', '');",
    )
    .await;
    let err = commit(
        &t,
        &[
            sql("INSERT INTO forge.audit_probe_bad (id) VALUES (1)"),
            audit("bad", 1, "succeeded"),
        ],
    )
    .await
    .expect_err("three arguments");
    assert_reason(&err, ErrorCode::AuditMissing, "audited_change_config");
    let err = commit(
        &t,
        &[sql(&format!(
            "INSERT INTO forge.audit_probe_text (code) VALUES ('{SECRET}')"
        ))],
    )
    .await
    .expect_err("a non-integer id");
    assert_reason(&err, ErrorCode::AuditMissing, "audited_change_config");
    let text = chain(&err);
    assert!(text.contains("table=audit_probe_text"), "{text}");
    assert!(!text.contains(SECRET), "no row data in the error: {text}");
    t.drop_now().await;
}

#[tokio::test]
async fn a_path_rewrite_needs_an_audit_row_of_its_own_resource_type() {
    let t = probe_db().await;
    seed(&t, 1).await;
    let err = commit(
        &t,
        &[
            sql("UPDATE forge.audit_probe SET path = 'moved'"),
            audit("other", 1, "succeeded"),
        ],
    )
    .await
    .expect_err("an audit row of another resource type");
    assert_refused(&err, "audit_probe");
    t.drop_now().await;
}

#[tokio::test]
async fn ids_outside_bigint_are_config_errors_and_negative_ids_work() {
    let t = probe_db().await;
    let mut su = t.superuser().await;
    exec(
        &mut su,
        "CREATE TABLE forge.audit_probe_wide (id numeric PRIMARY KEY);
         GRANT SELECT, INSERT ON forge.audit_probe_wide TO forge_app;
         CREATE CONSTRAINT TRIGGER audited_change_present AFTER INSERT ON forge.audit_probe_wide
             DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
             EXECUTE FUNCTION forge.audited_change_present('wide', 'id', '', '');",
    )
    .await;
    let err = commit(
        &t,
        &[sql(
            "INSERT INTO forge.audit_probe_wide (id) VALUES (9223372036854775808)",
        )],
    )
    .await
    .expect_err("above bigint");
    assert_reason(&err, ErrorCode::AuditMissing, "audited_change_config");
    commit(
        &t,
        &[
            sql("INSERT INTO forge.audit_probe_wide (id) VALUES (-5)"),
            audit("wide", -5, "succeeded"),
        ],
    )
    .await
    .expect("a negative id with its audit row");
    t.drop_now().await;
}

#[tokio::test]
async fn a_trigger_naming_a_missing_column_is_refused_as_config() {
    let t = probe_db().await;
    let mut su = t.superuser().await;
    exec(
        &mut su,
        "CREATE TABLE forge.audit_probe_typo (id bigint PRIMARY KEY);
         GRANT SELECT, INSERT ON forge.audit_probe_typo TO forge_app;
         CREATE CONSTRAINT TRIGGER audited_change_present AFTER INSERT ON forge.audit_probe_typo
             DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
             EXECUTE FUNCTION forge.audited_change_present('=knd', 'id', '', '');",
    )
    .await;
    let err = commit(
        &t,
        &[
            sql("INSERT INTO forge.audit_probe_typo (id) VALUES (1)"),
            audit("typo", 1, "succeeded"),
        ],
    )
    .await
    .expect_err("the =column does not exist");
    assert_reason(&err, ErrorCode::AuditMissing, "audited_change_config");
    t.drop_now().await;
}
