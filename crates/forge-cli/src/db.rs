//! `forge db status` (read-only) and `forge db migrate` (mutating — an
//! approved runbook step in production: ops/runbooks/db-migration.md).

use std::fmt::Write as _;

use forge_core::config::DatabaseConfig;
use forge_db::{Db, SchemaStatus, MIGRATOR};
use forge_errors::ForgeError;

use crate::runtime::block_on;

pub fn status(cfg: &DatabaseConfig) -> Result<String, ForgeError> {
    block_on(async {
        let db = Db::connect(cfg).await?;
        let st = forge_db::status(&db, &MIGRATOR).await?;
        Ok(render(db.target(), &st))
    })
}

pub fn migrate(cfg: &DatabaseConfig) -> Result<String, ForgeError> {
    block_on(async {
        let db = Db::connect(cfg).await?;
        let applied = forge_db::migrate(&db, &MIGRATOR, cfg.migrate_statement_timeout).await?;
        Ok(if applied.is_empty() {
            format!("{}: up to date\n", db.target())
        } else {
            format!("{}: applied {}\n", db.target(), list(&applied))
        })
    })
}

fn list(versions: &[i64]) -> String {
    if versions.is_empty() {
        return "-".to_owned();
    }
    versions
        .iter()
        .map(i64::to_string)
        .collect::<Vec<_>>()
        .join(" ")
}

/// One line per category; the verdict is what `forge serve` would decide.
fn render(target: &str, st: &SchemaStatus) -> String {
    let mut out = String::new();
    // Writing to a String cannot fail.
    let _ = writeln!(out, "database   {target}");
    let _ = writeln!(out, "applied    {}", list(&st.applied));
    let _ = writeln!(out, "pending    {}", list(&st.pending));
    let _ = writeln!(out, "unknown    {}", list(&st.unknown));
    let _ = writeln!(out, "modified   {}", list(&st.modified));
    let _ = writeln!(out, "dirty      {}", list(&st.dirty));
    let verdict = match st.incompatibility() {
        None => "compatible".to_owned(),
        Some(e) => format!("incompatible [{}] — see `forge explain FG-DB-0004`", e),
    };
    let _ = writeln!(out, "serve      {verdict}");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_names_every_category_and_the_verdict() {
        let st = SchemaStatus {
            applied: vec![1],
            pending: vec![2, 3],
            ..SchemaStatus::default()
        };
        let text = render("db:5432/forge", &st);
        assert!(text.contains("database   db:5432/forge"), "{text}");
        assert!(text.contains("pending    2 3"), "{text}");
        assert!(text.contains("unknown    -"), "{text}");
        assert!(
            text.contains("incompatible [FG-DB-0004 reason=pending versions=2,3]"),
            "{text}"
        );
        let ok = render("t", &SchemaStatus::default());
        assert!(ok.contains("serve      compatible"), "{ok}");
    }
}
