//! `forge doctor` — read-only checks of everything `forge serve` needs:
//! configuration, database (reachable, TLS, version), schema, storage root
//! and the git binary. One line per check (`--json` for scripts and the
//! later diag bundle). Nothing is created, migrated or changed: the schema
//! check uses the read-only `forge_db::status`.
//!
//! A check that depends on an earlier failed one is `skip`. The command
//! fails with the most severe error found (alert before warn before info),
//! so the exit code and the logged FG code name the worst problem.

use std::fmt::Write as _;
use std::path::Path;

use forge_core::config::{self, Config};
use forge_db::{Db, MIGRATOR};
use forge_errors::{ForgeError, Severity};
use forge_git::{Git, GitConfig, StorageRoot};
use serde_json::{json, Value};

use crate::runtime::block_on;

/// The outcome of one check.
enum Outcome {
    Ok(String),
    Fail(ForgeError),
    Skip(&'static str),
}

struct Check {
    name: &'static str,
    outcome: Outcome,
}

/// The rendered report and the most severe failure, if any.
pub struct Report {
    pub text: String,
    pub failure: Option<ForgeError>,
}

pub fn run(config_file: Option<&Path>, as_json: bool) -> Result<Report, ForgeError> {
    let checks = block_on(async { Ok(collect(config_file).await) })?;
    let text = if as_json {
        render_json(&checks)
    } else {
        render_text(&checks)
    };
    Ok(Report {
        text,
        failure: most_severe(checks),
    })
}

async fn collect(config_file: Option<&Path>) -> Vec<Check> {
    let mut checks = Vec::new();
    let cfg = config::Env::from_process().and_then(|env| config::load(config_file, &env));
    let cfg: Option<Config> = match cfg {
        Ok(cfg) => {
            checks.push(ok("config", "loaded"));
            Some(cfg)
        }
        Err(e) => {
            checks.push(fail("config", e));
            None
        }
    };
    let Some(cfg) = cfg else {
        for name in ["database", "schema", "storage"] {
            checks.push(skip(name, "configuration invalid"));
        }
        checks.push(git_check().await);
        return checks;
    };

    match Db::connect(&cfg.database).await {
        Ok(db) => {
            checks.push(ok("database", db.target()));
            checks.push(match forge_db::status(&db, &MIGRATOR).await {
                Ok(st) => match st.incompatibility() {
                    None => ok(
                        "schema",
                        &format!("compatible ({} applied)", st.applied.len()),
                    ),
                    Some(e) => fail("schema", e.with_context("target", db.target())),
                },
                Err(e) => fail("schema", e),
            });
            db.pool().close().await;
        }
        Err(e) => {
            checks.push(fail("database", e));
            checks.push(skip("schema", "database unavailable"));
        }
    }

    checks.push(match cfg.storage_root() {
        Ok(root) => match StorageRoot::open(root) {
            Ok(opened) => ok("storage", &opened.path().display().to_string()),
            Err(e) => fail("storage", e),
        },
        Err(e) => fail("storage", e),
    });
    checks.push(git_check().await);
    checks
}

/// `git version` from PATH, with an isolated HOME (no user configuration).
async fn git_check() -> Check {
    let git = Git::new(GitConfig::new("git", std::env::temp_dir()));
    match forge_git::version::detect(&git).await {
        Ok(v) => ok("git", &format!("git {v}")),
        Err(e) => fail("git", e),
    }
}

fn ok(name: &'static str, detail: &str) -> Check {
    Check {
        name,
        outcome: Outcome::Ok(detail.to_owned()),
    }
}

fn fail(name: &'static str, e: ForgeError) -> Check {
    Check {
        name,
        outcome: Outcome::Fail(e),
    }
}

fn skip(name: &'static str, why: &'static str) -> Check {
    Check {
        name,
        outcome: Outcome::Skip(why),
    }
}

fn rank(severity: Severity) -> u8 {
    match severity {
        Severity::Alert => 3,
        Severity::Warn => 2,
        Severity::Info => 1,
    }
}

fn most_severe(checks: Vec<Check>) -> Option<ForgeError> {
    checks
        .into_iter()
        .filter_map(|c| match c.outcome {
            Outcome::Fail(e) => Some(e),
            Outcome::Ok(_) | Outcome::Skip(_) => None,
        })
        // The first of the most severe: earlier checks are more fundamental.
        .rev()
        .max_by_key(|e| rank(e.code().severity()))
}

fn render_text(checks: &[Check]) -> String {
    let mut out = String::new();
    for c in checks {
        // Writing to a String cannot fail.
        let _ = match &c.outcome {
            Outcome::Ok(detail) => writeln!(out, "ok    {:<9} {detail}", c.name),
            Outcome::Fail(e) => writeln!(out, "FAIL  {:<9} {e}", c.name),
            Outcome::Skip(why) => writeln!(out, "skip  {:<9} {why}", c.name),
        };
    }
    out
}

fn render_json(checks: &[Check]) -> String {
    let items: Vec<Value> = checks
        .iter()
        .map(|c| match &c.outcome {
            Outcome::Ok(detail) => json!({ "name": c.name, "status": "ok", "detail": detail }),
            Outcome::Fail(e) => json!({
                "name": c.name,
                "status": "fail",
                "error_code": e.code().id(),
                "detail": e.to_string(),
            }),
            Outcome::Skip(why) => json!({ "name": c.name, "status": "skip", "detail": why }),
        })
        .collect();
    let all_ok = checks.iter().all(|c| matches!(c.outcome, Outcome::Ok(_)));
    let mut text = json!({ "ok": all_ok, "checks": items }).to_string();
    text.push('\n');
    text
}

#[cfg(test)]
mod tests {
    use forge_errors::ErrorCode;

    use super::*;

    #[test]
    fn the_most_severe_failure_wins_and_ties_go_to_the_earlier_check() {
        let checks = vec![
            ok("config", "loaded"),
            fail("database", ForgeError::new(ErrorCode::DbUnavailable)),
            fail("storage", ForgeError::new(ErrorCode::StorageMissing)),
            fail("git", ForgeError::new(ErrorCode::GitUnavailable)),
        ];
        let worst = most_severe(checks).expect("a failure");
        assert_eq!(worst.code(), ErrorCode::DbUnavailable);
    }

    #[test]
    fn json_lists_every_check_with_status_and_code() {
        let checks = vec![
            ok("config", "loaded"),
            fail("database", ForgeError::new(ErrorCode::DbUnavailable)),
            skip("schema", "database unavailable"),
        ];
        let v: Value = serde_json::from_str(&render_json(&checks)).expect("json");
        assert_eq!(v["ok"], false);
        assert_eq!(v["checks"][1]["error_code"], "FG-DB-0001");
        assert_eq!(v["checks"][2]["status"], "skip");
    }
}
