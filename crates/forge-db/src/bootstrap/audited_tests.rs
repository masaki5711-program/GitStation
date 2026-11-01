//! Parser tests of the ledger (no database needed).

use super::*;

const LEDGER: &str = r#"
guarded_functions = ["audited_change_present"]

[tables.people]
guarded = true
kind = "audited"
ro_columns = ["*"]
audit_resource = "person"
audit_id_column = "id"
unaudited_columns = ["last_seen_at"]
txn_audited_columns = ["path"]

[tables.things]
guarded = true
kind = "audited"
ro_columns = ["*"]
audit_resource = "=kind"
audit_id_column = "id"
unaudited_columns = []
txn_audited_columns = []

[tables.plain]
guarded = false
kind = "derived"
ro_columns = ["*"]
"#;

#[test]
fn the_ledger_parses_audited_entries() {
    let ledger = Ledger::parse(LEDGER).expect("parses");
    assert_eq!(ledger.guarded_functions, vec!["audited_change_present"]);
    assert_eq!(ledger.audited.len(), 2, "derived tables are not audited");
    let people = ledger
        .audited
        .iter()
        .find(|t| t.name == "people")
        .expect("people");
    assert_eq!(
        people.audit_resource,
        AuditResource::Literal("person".into())
    );
    assert_eq!(people.audit_id_column, "id");
    assert_eq!(people.unaudited_columns, vec!["last_seen_at"]);
    assert_eq!(people.txn_audited_columns, vec!["path"]);
    let things = ledger
        .audited
        .iter()
        .find(|t| t.name == "things")
        .expect("things");
    assert_eq!(things.audit_resource, AuditResource::Column("kind".into()));
    assert!(things.unaudited_columns.is_empty());
}

#[test]
fn an_audited_entry_without_audit_resource_is_rejected() {
    let text = LEDGER.replace("audit_resource = \"person\"\n", "");
    let err = Ledger::parse(&text).expect_err("audit_resource is required");
    let text = err.to_string();
    assert!(
        text.contains("audit_resource") && text.contains("people"),
        "names the field and the table: {text}"
    );
}

#[test]
fn an_audited_entry_without_audit_id_column_is_rejected() {
    let text = LEDGER.replacen("audit_id_column = \"id\"\n", "", 1);
    let err = Ledger::parse(&text).expect_err("audit_id_column is required");
    assert!(err.to_string().contains("audit_id_column"), "{err}");
}

#[test]
fn the_embedded_ledger_lists_audited_change_present_as_guarded() {
    let ledger = Ledger::embedded().expect("tables.toml parses");
    assert_eq!(
        ledger.guarded_functions,
        vec![
            "audited_change_present",
            "repositories_state_guard",
            "purge_delete_guard"
        ]
    );
}

#[test]
fn a_misspelt_kind_is_rejected_not_skipped() {
    let text = LEDGER.replace(
        "kind = \"audited\"\nro_columns = [\"*\"]\naudit_resource = \"person\"",
        "kind = \"audit\"\nro_columns = [\"*\"]\naudit_resource = \"person\"",
    );
    let err = Ledger::parse(&text).expect_err("unknown kind");
    let text = err.to_string();
    assert!(
        text.contains("reason=kind_unknown") && text.contains("people"),
        "{text}"
    );
}

#[test]
fn an_equals_sign_alone_is_not_a_resource_column() {
    let text = LEDGER.replace("audit_resource = \"person\"", "audit_resource = \"=\"");
    let err = Ledger::parse(&text).expect_err("no column after =");
    let text = err.to_string();
    assert!(
        text.contains("reason=audited_field_missing") && text.contains("audit_resource"),
        "{text}"
    );
}

#[test]
fn malformed_ledgers_are_config_invalid_with_their_reason() {
    for (name, text, reason) in [
        ("not toml", "[tables.x", "not_toml"),
        ("no tables", "guarded_functions = []", "tables_missing"),
        (
            "columns not strings",
            &*LEDGER.replace(
                "unaudited_columns = [\"last_seen_at\"]",
                "unaudited_columns = [1]",
            ),
            "not_a_string_array",
        ),
    ] {
        let err = Ledger::parse(text).expect_err(name);
        assert_eq!(err.code(), ErrorCode::ConfigInvalid, "{name}");
        let text = err.to_string();
        assert!(text.contains(&format!("reason={reason}")), "{name}: {text}");
        assert!(text.contains("tables.toml"), "{name}: {text}");
    }
}

#[test]
fn columns_in_conflicting_roles_are_rejected() {
    for (name, from, to, column) in [
        (
            "the id column unaudited",
            "unaudited_columns = [\"last_seen_at\"]",
            "unaudited_columns = [\"last_seen_at\", \"id\"]",
            "id",
        ),
        (
            "the id column txn-audited",
            "txn_audited_columns = [\"path\"]",
            "txn_audited_columns = [\"path\", \"id\"]",
            "id",
        ),
        (
            "the resource column unaudited",
            "unaudited_columns = []",
            "unaudited_columns = [\"kind\"]",
            "kind",
        ),
        (
            "a column both unaudited and txn-audited",
            "txn_audited_columns = [\"path\"]",
            "txn_audited_columns = [\"path\", \"last_seen_at\"]",
            "last_seen_at",
        ),
    ] {
        let text = LEDGER.replacen(from, to, 1);
        assert_ne!(text, LEDGER, "{name}: the case edits the ledger");
        let err = Ledger::parse(&text).expect_err(name);
        let text = err.to_string();
        assert!(
            text.contains("reason=audited_column_conflict"),
            "{name}: {text}"
        );
        assert!(text.contains(&format!("column={column}")), "{name}: {text}");
    }
}

#[test]
fn a_ledger_entry_that_is_not_a_table_is_rejected() {
    let err = Ledger::parse("[tables]\nx = 1").expect_err("not a table");
    assert!(
        err.to_string().contains("reason=table_not_a_table"),
        "{err}"
    );
}
