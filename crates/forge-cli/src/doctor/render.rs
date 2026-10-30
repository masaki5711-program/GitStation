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
    "found",
    "expected",
    "operation",
    // FG-DB-0005 (outbox-sequencer-lag runbook): counts, an age, a pid.
    "unsequenced_count",
    "unsequenced_events",
    "unsequenced_audit",
    "heartbeat_age_ms",
    "lock_holder_pid",
    // ── C8: FG-DB-0011 / FG-DB-0015 (audit-seal-* runbooks): ids, numbers, an age.
    "detail",
    "seal_id",
    "first_seq",
    "last_seq",
    "missing_seq",
    "key_id",
    "hash_version",
    "oldest_age_s",
    "last_sealed_seq",
    "audit_high_seq",
    "unsealed_numbered",
    "unsequenced",
    "audit_sequencing",
    // ── end C8 ──
    // C6 — FG-DB-0006 / 0007 (job-dead-lettered runbook) and FG-DB-0017
    // (job-queue-backlog: kind, ready, oldest_wait_ms, expired_leases, …): ids, counts,
    // ages, code-chosen kinds and error codes, a request id to search the
    // log for. Never the payload or last_error.
    "job_id",
    "kind",
    "attempts",
    "attempt",
    "last_error_code",
    "origin_request_id",
    "worker_id",
    "dead_recent",
    "expired_leases",
    "expired_ms",
    "ready",
    "oldest_wait_ms",
    // D3 MR2 — FG-AUTH-0010 (user-offboarding runbook): counts and group ids.
    "active_human_admins",
    "ownerless_groups",
    "group_ids",
    // ── D4 MR3b — FG-REPO-0012 (repository-provisioning runbook) and
    // FG-REPO-0002 from `repositories.storage` (repo-storage): ids, counts,
    // ages, a job state. Never the path or the provision token.
    "repo_id",
    "project_id",
    "stuck",
    "age_ms",
    "job_state",
    "threshold_ms",
    "missing",
    "checked",
    "truncated",
    // ── end D4 MR3b ──
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
            Outcome::Warn(detail) => writeln!(out, "warn  {:<14} {detail}", c.name),
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
            Outcome::Warn(detail) => json!({ "name": c.name, "status": "warn", "detail": detail }),
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
