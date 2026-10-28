//! The audited tables of the ledger (`tables.toml`, kind = "audited") and the
//! doctor check of their `forge.audited_change_present` triggers (D1,
//! ADR-0016 §1). The ledger is the single source: its fields are the
//! trigger's arguments, and every column it does not declare unaudited is in
//! the trigger's `UPDATE OF` list.

use std::borrow::Cow;

use forge_errors::{ErrorCode, ForgeError};
use sqlx::postgres::PgConnection;

use super::guard_missing;
use crate::read_query_error;

/// Where the audit row's `resource_type` comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AuditResource {
    /// A literal resource type, e.g. `"user"`.
    Literal(String),
    /// `"=column"` in the ledger: read from that column of the row.
    Column(String),
}

impl AuditResource {
    /// The trigger argument: the literal, or `=column`.
    fn argument(&self) -> Cow<'_, str> {
        match self {
            Self::Literal(literal) => Cow::Borrowed(literal),
            Self::Column(column) => Cow::Owned(format!("={column}")),
        }
    }
}

/// One `kind = "audited"` table of the ledger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AuditedTable {
    /// Bare table name (schema `forge`).
    pub name: String,
    pub audit_resource: AuditResource,
    pub audit_id_column: String,
    pub unaudited_columns: Vec<String>,
    pub txn_audited_columns: Vec<String>,
}

impl AuditedTable {
    /// A column the ledger lists in two roles that disarm each other: the
    /// resource columns (id, `=column`) unaudited or txn-audited (a re-keyed
    /// row would go unaudited), or a column both unaudited and txn-audited.
    fn conflicting_column(&self) -> Option<&str> {
        let resource_column = match &self.audit_resource {
            AuditResource::Column(column) => Some(column),
            AuditResource::Literal(_) => None,
        };
        let exempt =
            |c: &String| self.unaudited_columns.contains(c) || self.txn_audited_columns.contains(c);
        std::iter::once(&self.audit_id_column)
            .chain(resource_column)
            .find(|c| exempt(c))
            .or_else(|| {
                self.unaudited_columns
                    .iter()
                    .find(|c| self.txn_audited_columns.contains(c))
            })
            .map(String::as_str)
    }

    /// `tgargs` as PostgreSQL stores it: each argument NUL-terminated.
    fn trigger_args(&self) -> Vec<u8> {
        [
            self.audit_resource.argument(),
            Cow::Borrowed(self.audit_id_column.as_str()),
            Cow::Owned(self.unaudited_columns.join(",")),
            Cow::Owned(self.txn_audited_columns.join(",")),
        ]
        .iter()
        .flat_map(|a| a.bytes().chain([0]))
        .collect()
    }
}

/// The parsed ledger: what doctor and the root gate need.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Ledger {
    /// Bare names of `guarded_functions`.
    pub guarded_functions: Vec<String>,
    pub audited: Vec<AuditedTable>,
}

fn invalid(reason: &'static str) -> ForgeError {
    // The ledger is built into the binary: a failure here is a build defect.
    ForgeError::new(ErrorCode::ConfigInvalid)
        .with_context("config_key", "tables.toml")
        .with_context("reason", reason)
}

fn strings(
    value: Option<&toml::Value>,
    table: &str,
    field: &str,
) -> Result<Vec<String>, ForgeError> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    value
        .as_array()
        .and_then(|items| {
            items
                .iter()
                .map(|v| v.as_str().map(str::to_owned))
                .collect::<Option<Vec<_>>>()
        })
        .ok_or_else(|| {
            invalid("not_a_string_array")
                .with_context("table", table)
                .with_context("field", field)
        })
}

fn required<'a>(entry: &'a toml::Table, table: &str, field: &str) -> Result<&'a str, ForgeError> {
    entry
        .get(field)
        .and_then(toml::Value::as_str)
        .filter(|s| !s.is_empty() && *s != "=")
        .ok_or_else(|| {
            invalid("audited_field_missing")
                .with_context("table", table)
                .with_context("field", field)
        })
}

/// The kinds of `tables.toml` (its header).
const KINDS: &[&str] = &["audited", "derived", "log"];

impl Ledger {
    pub(crate) fn parse(text: &str) -> Result<Self, ForgeError> {
        let root: toml::Table = text.parse().map_err(|_| invalid("not_toml"))?;
        let guarded_functions = strings(root.get("guarded_functions"), "-", "guarded_functions")?;
        let tables = root
            .get("tables")
            .and_then(toml::Value::as_table)
            .ok_or_else(|| invalid("tables_missing"))?;
        let mut audited = Vec::new();
        for (name, entry) in tables {
            let Some(entry) = entry.as_table() else {
                return Err(invalid("table_not_a_table").with_context("table", name));
            };
            // A misspelt kind would silently drop the table from the check.
            let kind = entry.get("kind").and_then(toml::Value::as_str);
            if !kind.is_some_and(|k| KINDS.contains(&k)) {
                return Err(invalid("kind_unknown").with_context("table", name));
            }
            if kind != Some("audited") {
                continue;
            }
            let resource = required(entry, name, "audit_resource")?;
            let audit_resource = match resource.strip_prefix('=') {
                Some(column) => AuditResource::Column(column.to_owned()),
                None => AuditResource::Literal(resource.to_owned()),
            };
            let table = AuditedTable {
                name: name.to_owned(),
                audit_resource,
                audit_id_column: required(entry, name, "audit_id_column")?.to_owned(),
                unaudited_columns: strings(
                    entry.get("unaudited_columns"),
                    name,
                    "unaudited_columns",
                )?,
                txn_audited_columns: strings(
                    entry.get("txn_audited_columns"),
                    name,
                    "txn_audited_columns",
                )?,
            };
            if let Some(column) = table.conflicting_column() {
                return Err(invalid("audited_column_conflict")
                    .with_context("table", name)
                    .with_context("column", column));
            }
            audited.push(table);
        }
        Ok(Self {
            guarded_functions,
            audited,
        })
    }

    /// The ledger built into this binary (`tables.toml`).
    pub(crate) fn embedded() -> Result<Self, ForgeError> {
        Self::parse(include_str!("../../tables.toml"))
    }
}

/// `pg_trigger.tgtype` of AFTER … FOR EACH ROW on INSERT, DELETE and UPDATE
/// (ROW 1 | INSERT 4 | DELETE 8 | UPDATE 16; BEFORE 2, TRUNCATE 32 and
/// INSTEAD 64 unset).
const AFTER_ROW_INSERT_DELETE_UPDATE: i16 = 1 | 4 | 8 | 16;

/// The trigger's name: the one `guarded_changes` and the README use.
const TRIGGER_NAME: &str = "audited_change_present";

/// Column types the trigger can compare with `audit_log.resource_id`.
const INTEGER_TYPES: &[&str] = &["smallint", "integer", "bigint"];

fn weakened(table: &str, detail: &'static str) -> ForgeError {
    guard_missing("audited_trigger")
        .with_context("table", table)
        .with_context("detail", detail)
}

/// One trigger executing `forge.audited_change_present`.
#[derive(sqlx::FromRow)]
struct Trigger {
    name: String,
    enabled: String,
    tgtype: i16,
    is_constraint: bool,
    deferred: bool,
    no_when: bool,
    args: Vec<u8>,
    update_of: Vec<String>,
}

impl Trigger {
    /// The first property that differs from the ledger, if any.
    fn difference(
        &self,
        table: &AuditedTable,
        expected_update_of: &[&str],
    ) -> Option<&'static str> {
        if self.name != TRIGGER_NAME {
            Some("name")
        } else if !self.is_constraint || !self.deferred {
            Some("not_deferred_constraint")
        } else if self.enabled != "A" {
            Some("not_always")
        } else if !self.no_when {
            Some("when")
        } else if self.tgtype != AFTER_ROW_INSERT_DELETE_UPDATE {
            Some("events")
        } else if self.args != table.trigger_args() {
            Some("arguments")
        } else if self.update_of != expected_update_of {
            Some("update_of")
        } else {
            None
        }
    }
}

/// Every audited table's trigger, against the ledger: the table and the
/// columns the ledger names exist (the id column is an integer); exactly one
/// trigger executes `forge.audited_change_present`, named
/// `audited_change_present`, a deferred constraint trigger enabled ALWAYS,
/// AFTER INSERT OR DELETE OR UPDATE OF every column but the unaudited ones,
/// FOR EACH ROW without WHEN, with the ledger's arguments.
/// `guard=audited_trigger` with `table` and `detail` (the first difference).
pub(crate) async fn check_audited_triggers(
    conn: &mut PgConnection,
    ledger: &Ledger,
) -> Result<(), ForgeError> {
    for table in &ledger.audited {
        let relation = format!("forge.{}", table.name);
        let columns: Vec<(String, String)> = sqlx::query_as(
            "SELECT a.attname::text, pg_catalog.format_type(a.atttypid, NULL) \
             FROM pg_catalog.pg_attribute a \
             WHERE a.attrelid = pg_catalog.to_regclass($1) AND a.attnum > 0 \
             AND NOT a.attisdropped ORDER BY a.attname",
        )
        .bind(&relation)
        .fetch_all(&mut *conn)
        .await
        .map_err(|e| read_query_error("bootstrap.audited_columns", e))?;
        if columns.is_empty() {
            return Err(weakened(&table.name, "table_missing"));
        }
        let type_of = |name: &str| {
            columns
                .iter()
                .find(|(c, _)| c == name)
                .map(|(_, t)| t.as_str())
        };
        if !type_of(&table.audit_id_column).is_some_and(|t| INTEGER_TYPES.contains(&t)) {
            return Err(
                weakened(&table.name, "id_column").with_context("column", &table.audit_id_column)
            );
        }
        let resource_column = match &table.audit_resource {
            AuditResource::Column(column) => Some(column),
            AuditResource::Literal(_) => None,
        };
        for column in table
            .unaudited_columns
            .iter()
            .chain(&table.txn_audited_columns)
            .chain(resource_column)
        {
            if type_of(column).is_none() {
                return Err(weakened(&table.name, "ledger_column").with_context("column", column));
            }
        }
        let expected: Vec<&str> = columns
            .iter()
            .map(|(c, _)| c.as_str())
            .filter(|c| !table.unaudited_columns.iter().any(|u| u == c))
            .collect();
        let triggers: Vec<Trigger> = sqlx::query_as(
            "SELECT t.tgname::text AS name, t.tgenabled::text AS enabled, t.tgtype, \
             t.tgconstraint <> 0 AS is_constraint, \
             t.tgdeferrable AND t.tginitdeferred AS deferred, \
             t.tgqual IS NULL AS no_when, t.tgargs AS args, \
             ARRAY(SELECT a.attname::text FROM pg_catalog.pg_attribute a \
                   WHERE a.attrelid = t.tgrelid \
                   AND a.attnum = ANY (t.tgattr::pg_catalog.int2[]) ORDER BY a.attname) \
               AS update_of \
             FROM pg_catalog.pg_trigger t \
             WHERE t.tgrelid = pg_catalog.to_regclass($1) AND NOT t.tgisinternal \
             AND t.tgfoid = pg_catalog.to_regprocedure('forge.audited_change_present()')",
        )
        .bind(&relation)
        .fetch_all(&mut *conn)
        .await
        .map_err(|e| read_query_error("bootstrap.audited_triggers", e))?;
        let [trigger] = triggers.as_slice() else {
            let detail = if triggers.is_empty() {
                "missing"
            } else {
                "extra"
            };
            return Err(weakened(&table.name, detail));
        };
        if let Some(detail) = trigger.difference(table, &expected) {
            return Err(weakened(&table.name, detail));
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "audited_tests.rs"]
mod tests;
