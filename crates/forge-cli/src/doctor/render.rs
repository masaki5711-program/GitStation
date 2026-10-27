//! Rendering of the doctor report: one line per check, or JSON.

use std::fmt::Write as _;

use forge_errors::ForgeError;
use serde_json::{json, Value};

use super::{Check, Outcome};

/// Context keys safe to print: they name where to look, never values,
/// sources, paths or server messages (those stay in the log — the error
/// contract keeps internals out of command output).
const SAFE_CONTEXT: &[&str] = &[
    "config_key",
    "reason",
    "target",
    "probe",
    "pool",
    "versions",
    "sqlstate",
    "renamed_to",
    "timeout_key",
    "guard",
    "role_check",
    "needed",
    "usable",
];

/// `FG-… key=value …` with the allow-listed context only.
pub(super) fn summary(e: &ForgeError) -> String {
    let mut out = e.code().id().to_owned();
    for (k, v) in e.context() {
        if SAFE_CONTEXT.contains(k) {
            // Writing to a String cannot fail.
            let _ = write!(out, " {k}={v}");
        }
    }
    out
}

pub(super) fn text(checks: &[Check]) -> String {
    let mut out = String::new();
    for c in checks {
        // Writing to a String cannot fail.
        let _ = match &c.outcome {
            Outcome::Ok(detail) => writeln!(out, "ok    {:<14} {detail}", c.name),
            Outcome::Fail(e) => writeln!(out, "FAIL  {:<14} {}", c.name, summary(e)),
            Outcome::Skip(why) => writeln!(out, "skip  {:<14} {why}", c.name),
        };
    }
    out
}

pub(super) fn json(checks: &[Check]) -> String {
    let items: Vec<Value> = checks
        .iter()
        .map(|c| match &c.outcome {
            Outcome::Ok(detail) => json!({ "name": c.name, "status": "ok", "detail": detail }),
            Outcome::Fail(e) => json!({
                "name": c.name,
                "status": "fail",
                "error_code": e.code().id(),
                "detail": summary(e),
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

    use super::super::{fail, ok, skip};
    use super::*;

    #[test]
    fn output_shows_only_allow_listed_context_never_sources_or_paths() {
        let e = ForgeError::new(ErrorCode::StorageIoFailed)
            .with_context("reason", "unreadable")
            .with_context("path", "/srv/forge/secret-dir")
            .with_source(std::io::Error::other("db_message with row value 42"));
        let text = summary(&e);
        assert!(text.starts_with("FG-REPO-"), "{text}");
        assert!(text.contains("reason=unreadable"), "{text}");
        assert!(!text.contains("secret-dir"), "{text}");
        assert!(!text.contains("row value"), "{text}");
    }

    #[test]
    fn a_guard_finding_names_the_guard_but_not_the_role_list() {
        let e = ForgeError::new(ErrorCode::AppendOnlyViolation)
            .with_context("reason", "role_membership_changed")
            .with_context("unexpected", "dba_mallory");
        let text = summary(&e);
        assert!(text.contains("reason=role_membership_changed"), "{text}");
        assert!(
            !text.contains("dba_mallory"),
            "role names stay in the log: {text}"
        );
    }

    #[test]
    fn json_lists_every_check_with_status_and_code() {
        let checks = vec![
            ok("config", "loaded"),
            fail("database", ForgeError::new(ErrorCode::DbUnavailable)),
            skip("schema", "database unavailable"),
        ];
        let v: Value = serde_json::from_str(&json(&checks)).expect("json");
        assert_eq!(v["ok"], false);
        assert_eq!(v["checks"][1]["error_code"], "FG-DB-0001");
        assert_eq!(v["checks"][2]["status"], "skip");
    }
}
