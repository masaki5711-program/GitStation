//! Doctor side of `forge.audited_change_present` (D1): the function is a
//! registered guard compared with its migration, and every audited table's
//! trigger is compared with the ledger (`guard=audited_trigger`, `detail`).

use crate::audited_tests::{exec, probe_db, probe_ledger, PROBE_TRIGGER};
use crate::bootstrap::audited::{check_audited_triggers, AuditedTable, Ledger};
use crate::test_support::*;

#[tokio::test]
async fn the_audit_presence_function_is_a_registered_guarded_object() {
    let t = FreshDb::migrated().await;
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM forge_audit.guarded_objects \
         WHERE object_type = 'function' AND object_identity = 'forge.audited_change_present()'",
    )
    .fetch_one(t.db.pool().executor())
    .await
    .expect("registry");
    assert_eq!(n, 1);
    t.drop_now().await;
}

#[tokio::test]
async fn a_tampered_audited_change_present_body_is_reported() {
    let _lock = ClusterLock::shared().await;
    let t = FreshDb::migrated().await;
    crate::bootstrap::check(&t.db, &[])
        .await
        .expect("guards after migrate");
    let mut su = t.superuser().await;
    let exists: bool =
        sqlx::query_scalar("SELECT to_regprocedure('forge.audited_change_present()') IS NOT NULL")
            .fetch_one(&mut su)
            .await
            .expect("lookup");
    assert!(
        exists,
        "the migration creates forge.audited_change_present()"
    );
    exec(
        &mut su,
        "CREATE OR REPLACE FUNCTION forge.audited_change_present() RETURNS trigger \
         LANGUAGE plpgsql SET search_path = pg_catalog, pg_temp \
         AS $$ BEGIN RETURN NULL; END $$",
    )
    .await;
    let err = crate::bootstrap::check(&t.db, &[])
        .await
        .expect_err("a weakened guard function");
    let text = err.to_string();
    assert!(text.contains("FG-DB-0009"), "{text}");
    assert!(text.contains("guard=function_body"), "{text}");
    t.drop_now().await;
}

#[tokio::test]
async fn the_audited_trigger_check_accepts_the_shipped_triggers() {
    let t = probe_db().await;
    let mut su = t.superuser().await;
    check_audited_triggers(&mut su, &probe_ledger())
        .await
        .expect("probe triggers match the injected ledger");
    t.drop_now().await;
}

#[tokio::test]
async fn every_weakened_audited_trigger_is_reported() {
    let t = probe_db().await;
    let mut su = t.superuser().await;
    let head = "DROP TRIGGER audited_change_present ON forge.audit_probe; ";
    let always = " ALTER TABLE forge.audit_probe ENABLE ALWAYS TRIGGER audited_change_present;";
    let call =
        "EXECUTE FUNCTION forge.audited_change_present('probe', 'id', 'last_used_at', 'path');";
    let all = "AFTER INSERT OR DELETE OR UPDATE OF id, name, token_hash, path ON forge.audit_probe";
    let deferred = "DEFERRABLE INITIALLY DEFERRED";
    let cases: Vec<(&str, &str, String)> = vec![
        ("missing", "missing", head.to_owned()),
        (
            "plain trigger, not a constraint trigger",
            "not_deferred_constraint",
            format!("{head}CREATE TRIGGER audited_change_present {all} FOR EACH ROW {call}{always}"),
        ),
        (
            "not deferrable",
            "not_deferred_constraint",
            format!(
                "{head}CREATE CONSTRAINT TRIGGER audited_change_present {all} FOR EACH ROW {call}{always}"
            ),
        ),
        (
            "initially immediate",
            "not_deferred_constraint",
            format!(
                "{head}CREATE CONSTRAINT TRIGGER audited_change_present {all} DEFERRABLE INITIALLY IMMEDIATE FOR EACH ROW {call}{always}"
            ),
        ),
        (
            "origin only, not ALWAYS",
            "not_always",
            "ALTER TABLE forge.audit_probe ENABLE TRIGGER audited_change_present;".to_owned(),
        ),
        (
            "disabled",
            "not_always",
            "ALTER TABLE forge.audit_probe DISABLE TRIGGER audited_change_present;".to_owned(),
        ),
        (
            "no DELETE",
            "events",
            format!(
                "{head}CREATE CONSTRAINT TRIGGER audited_change_present AFTER INSERT OR UPDATE OF id, name, token_hash, path ON forge.audit_probe {deferred} FOR EACH ROW {call}{always}"
            ),
        ),
        (
            "UPDATE OF misses an audited column",
            "update_of",
            format!(
                "{head}CREATE CONSTRAINT TRIGGER audited_change_present AFTER INSERT OR DELETE OR UPDATE OF id, name, path ON forge.audit_probe {deferred} FOR EACH ROW {call}{always}"
            ),
        ),
        (
            "UPDATE OF includes an unaudited column",
            "update_of",
            format!(
                "{head}CREATE CONSTRAINT TRIGGER audited_change_present AFTER INSERT OR DELETE OR UPDATE OF id, name, token_hash, path, last_used_at ON forge.audit_probe {deferred} FOR EACH ROW {call}{always}"
            ),
        ),
        (
            "arguments differ from the ledger",
            "arguments",
            format!(
                "{head}CREATE CONSTRAINT TRIGGER audited_change_present {all} {deferred} FOR EACH ROW EXECUTE FUNCTION forge.audited_change_present('probe', 'id', 'last_used_at, name', 'path');{always}"
            ),
        ),
        (
            "FOR EACH STATEMENT",
            "not_deferred_constraint",
            format!(
                "{head}CREATE TRIGGER audited_change_present AFTER INSERT OR DELETE ON forge.audit_probe FOR EACH STATEMENT {call}{always}"
            ),
        ),
        (
            "a WHEN clause",
            "when",
            format!(
                "{head}CREATE CONSTRAINT TRIGGER audited_change_present {all} {deferred} FOR EACH ROW WHEN (false) {call}{always}"
            ),
        ),
        (
            "another name",
            "name",
            "ALTER TRIGGER audited_change_present ON forge.audit_probe RENAME TO renamed;".to_owned(),
        ),
        (
            "a second trigger on the function",
            "extra",
            format!("CREATE TRIGGER second AFTER INSERT ON forge.audit_probe FOR EACH ROW {call}"),
        ),
    ];
    for (name, detail, tamper) in cases {
        // Restore the shipped trigger, then weaken it.
        exec(
            &mut su,
            &format!(
                "DROP TRIGGER IF EXISTS audited_change_present ON forge.audit_probe; \
                 DROP TRIGGER IF EXISTS renamed ON forge.audit_probe; \
                 DROP TRIGGER IF EXISTS second ON forge.audit_probe; {PROBE_TRIGGER}"
            ),
        )
        .await;
        check_audited_triggers(&mut su, &probe_ledger())
            .await
            .unwrap_or_else(|e| panic!("{name}: shipped trigger rejected: {e}"));
        exec(&mut su, &tamper).await;
        let err = check_audited_triggers(&mut su, &probe_ledger())
            .await
            .expect_err(name);
        let text = err.to_string();
        assert!(text.contains("FG-DB-0009"), "{name}: {text}");
        assert!(text.contains("guard=audited_trigger"), "{name}: {text}");
        assert!(
            text.contains("table=audit_probe"),
            "{name} names the table: {text}"
        );
        assert!(text.contains(&format!("detail={detail}")), "{name}: {text}");
    }
    t.drop_now().await;
}

#[tokio::test]
async fn a_ledger_that_does_not_match_the_table_is_reported() {
    let t = probe_db().await;
    let mut su = t.superuser().await;
    exec(
        &mut su,
        "CREATE TABLE forge.audit_probe_textid (id text PRIMARY KEY)",
    )
    .await;
    type Edit = fn(&mut AuditedTable);
    let cases: [(&str, &str, Edit); 4] = [
        ("an unknown table", "table_missing", |t| {
            t.name = "no_such_table".into()
        }),
        ("a non-integer id column", "id_column", |t| {
            t.name = "audit_probe_textid".into();
        }),
        ("a misspelt unaudited column", "ledger_column", |t| {
            t.unaudited_columns = vec!["last_usd_at".into()];
        }),
        ("a misspelt txn-audited column", "ledger_column", |t| {
            t.txn_audited_columns = vec!["pth".into()];
        }),
    ];
    for (name, detail, edit) in cases {
        let mut ledger = probe_ledger();
        ledger.audited.truncate(1);
        if let Some(table) = ledger.audited.first_mut() {
            edit(table);
        }
        let err = check_audited_triggers(&mut su, &ledger)
            .await
            .expect_err(name);
        let text = err.to_string();
        assert!(text.contains("guard=audited_trigger"), "{name}: {text}");
        assert!(text.contains(&format!("detail={detail}")), "{name}: {text}");
    }
    t.drop_now().await;
}

#[test]
fn every_guarded_function_of_the_ledger_is_checked_by_doctor() {
    let ledger = Ledger::embedded().expect("tables.toml parses");
    for function in &ledger.guarded_functions {
        let signature = format!("forge.{function}()");
        assert!(
            crate::bootstrap::schema::GUARD_FUNCTIONS.contains(&signature.as_str()),
            "{signature} is guarded in tables.toml but doctor does not compare its body"
        );
    }
}

#[tokio::test]
async fn the_schema_check_runs_the_audited_trigger_check() {
    let t = probe_db().await;
    let mut su = t.superuser().await;
    // Intact triggers: the probe tables are audited but have no ACL
    // expectation, which is reported too.
    let err = crate::bootstrap::schema::check_schema_objects_with(&mut su, &probe_ledger())
        .await
        .expect_err("audited tables without an ACL expectation");
    let text = err.to_string();
    assert!(text.contains("guard=privileges"), "{text}");
    assert!(text.contains("detail=audited_table_unlisted"), "{text}");
    assert!(text.contains("object=forge.audit_probe"), "{text}");
    exec(
        &mut su,
        "ALTER TABLE forge.audit_probe ENABLE TRIGGER audited_change_present",
    )
    .await;
    let err = crate::bootstrap::schema::check_schema_objects_with(&mut su, &probe_ledger())
        .await
        .expect_err("a weakened probe trigger");
    let text = err.to_string();
    assert!(text.contains("guard=audited_trigger"), "{text}");
    assert!(text.contains("detail=not_always"), "{text}");
    t.drop_now().await;
}
