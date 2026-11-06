//! The ledger's `ro_columns` (tables.toml) against what `forge_ro` can really
//! SELECT after all migrations. A grant added by a migration without the
//! ledger entry (or the reverse) fails here, listing table and columns.

use std::collections::{BTreeMap, BTreeSet};

use crate::test_support::*;

type Cols = BTreeSet<String>;

/// Table -> declared columns; `None` for "*" (all columns of the table).
fn ledger_ro() -> BTreeMap<String, Option<Cols>> {
    let root: toml::Table = include_str!("../tables.toml").parse().expect("tables.toml");
    root["tables"]
        .as_table()
        .expect("tables")
        .iter()
        .map(|(name, entry)| {
            let cols: Vec<String> = entry
                .get("ro_columns")
                .map(|v| {
                    v.as_array()
                        .expect("ro_columns is an array")
                        .iter()
                        .map(|c| c.as_str().expect("ro column is a string").to_owned())
                        .collect()
                })
                .unwrap_or_default();
            let all = cols.iter().any(|c| c == "*");
            (name.clone(), (!all).then(|| cols.into_iter().collect()))
        })
        .collect()
}

/// Describes every difference between the ledger and the live grants.
fn differences(
    ledger: &BTreeMap<String, Option<Cols>>,
    all_columns: &BTreeMap<String, Cols>,
    readable: &BTreeMap<String, Cols>,
) -> Vec<String> {
    let mut out = Vec::new();
    for (table, actual) in readable {
        if !ledger.contains_key(table) {
            out.push(format!(
                "{table}: forge_ro reads {actual:?} but the table is not in tables.toml"
            ));
        }
    }
    for (table, declared) in ledger {
        let empty = Cols::new();
        let actual = readable.get(table).unwrap_or(&empty);
        let declared = match declared {
            None => all_columns.get(table).unwrap_or(&empty).clone(),
            Some(cols) => cols.clone(),
        };
        let missing: Vec<_> = declared.difference(actual).collect();
        let extra: Vec<_> = actual.difference(&declared).collect();
        if !missing.is_empty() || !extra.is_empty() {
            out.push(format!(
                "{table}: ledger lists but forge_ro cannot select {missing:?}; \
                 forge_ro can select but ledger omits {extra:?}"
            ));
        }
    }
    out
}

#[test]
fn differences_names_table_missing_and_extra() {
    let set = |c: &[&str]| c.iter().map(|s| (*s).to_owned()).collect::<Cols>();
    let ledger = BTreeMap::from([
        ("a".to_owned(), Some(set(&["x", "y"]))),
        ("b".to_owned(), Some(set(&[]))),
    ]);
    let all = BTreeMap::from([("a".to_owned(), set(&["x", "y", "z"]))]);
    let readable = BTreeMap::from([
        ("a".to_owned(), set(&["x", "z"])),
        ("c".to_owned(), set(&["q"])),
    ]);
    let d = differences(&ledger, &all, &readable);
    assert_eq!(d.len(), 2, "{d:?}");
    assert!(d.iter().any(|l| l.starts_with("c:")));
    assert!(d
        .iter()
        .any(|l| l.starts_with("a:") && l.contains("\"y\"") && l.contains("\"z\"")));
}

#[tokio::test]
async fn forge_ro_columns_match_the_ledger_for_every_table() {
    let t = FreshDb::migrated().await;
    // Ordinary tables of schema forge, and the columns forge_ro can SELECT
    // (column grants and table-level SELECT alike).
    let rows = t
        .json_rows(
            "SELECT c.relname AS tbl, a.attname AS col, \
                    pg_catalog.has_column_privilege('forge_ro', c.oid, a.attnum, 'SELECT') AS ro \
             FROM pg_catalog.pg_class c \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             JOIN pg_catalog.pg_attribute a ON a.attrelid = c.oid \
             WHERE n.nspname = 'forge' AND c.relkind IN ('r', 'p') \
               AND a.attnum > 0 AND NOT a.attisdropped",
        )
        .await;
    let mut all_columns: BTreeMap<String, Cols> = BTreeMap::new();
    let mut readable: BTreeMap<String, Cols> = BTreeMap::new();
    for r in &rows {
        let tbl = r["tbl"].as_str().expect("tbl").to_owned();
        let col = r["col"].as_str().expect("col").to_owned();
        all_columns
            .entry(tbl.clone())
            .or_default()
            .insert(col.clone());
        if r["ro"].as_bool().expect("ro") {
            readable.entry(tbl).or_default().insert(col);
        }
    }
    // forge_audit (the audit registry, outside the ledger) is readable by design;
    // any other schema must stay closed to forge_ro.
    let others = t
        .json_rows(
            "SELECT DISTINCT n.nspname || '.' || c.relname AS tbl \
             FROM pg_catalog.pg_class c \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             JOIN pg_catalog.pg_attribute a ON a.attrelid = c.oid \
             WHERE n.nspname NOT IN ('forge', 'forge_audit', 'pg_catalog', 'information_schema', 'pg_toast') \
               AND c.relkind IN ('r', 'p', 'v', 'm') AND a.attnum > 0 AND NOT a.attisdropped \
               AND pg_catalog.has_column_privilege('forge_ro', c.oid, a.attnum, 'SELECT')",
        )
        .await;
    let ledger = ledger_ro();
    let mut diffs = differences(&ledger, &all_columns, &readable);
    for r in &others {
        diffs.push(format!(
            "{}: outside schema forge, readable by forge_ro",
            r["tbl"]
        ));
    }
    assert!(
        diffs.is_empty(),
        "forge_ro grants differ from tables.toml ro_columns:\n{}",
        diffs.join("\n")
    );
    t.drop_now().await;
}
