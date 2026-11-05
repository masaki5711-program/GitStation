//! The Checks API check of `forge doctor` (M3 C1, ADR-0024 §10, runbook
//! checks-reporting): `checks.reporters` — every check a protected branch's
//! merge rule requires has an active service account that may report it
//! **on that rule's project** (a grant is for one project). A required
//! check without one blocks every merge into those branches
//! (`required_check_missing`), so it is a `warn` (no FG code: nothing
//! failed, a merge condition cannot be met). Read pool, two statements.

use std::collections::BTreeMap;

use forge_core::CheckNamespace;
use forge_db::Db;

use super::{fail, ok, warn, Check};

const NAME: &str = "checks.reporters";
const RETENTION: &str = "checks.retention";

/// The doctor line of `unreported` (merge rules per namespace whose project
/// has no reporter of it) against `reporters` (active grants per
/// namespace, all projects).
pub(super) fn reporters_line(
    unreported: &BTreeMap<CheckNamespace, i64>,
    reporters: &BTreeMap<CheckNamespace, i64>,
) -> Check {
    let have: Vec<String> = reporters
        .iter()
        .map(|(ns, n)| format!("{}/: {n} grant(s)", ns.as_str()))
        .collect();
    let have = if have.is_empty() {
        "no grant".to_owned()
    } else {
        have.join(", ")
    };
    if unreported.is_empty() {
        return ok(
            NAME,
            &format!("every required check has a reporter on its project ({have})"),
        );
    }
    let missing: Vec<String> = unreported
        .iter()
        .map(|(ns, rules)| format!("{}/ ({rules} rule(s))", ns.as_str()))
        .collect();
    warn(
        NAME,
        &format!(
            "required checks no service account may report on their project: {} — every \
             merge into those branches is blocked (runbook checks-reporting); {have}",
            missing.join(", ")
        ),
    )
}

/// `checks.reporters`.
pub(super) async fn reporters_check(db: &Db) -> Check {
    let unreported = match forge_db::checks::unreported_requirements(db).await {
        Ok(r) => r,
        Err(e) => return fail(NAME, e),
    };
    match forge_db::checks::reporters(db).await {
        Ok(reporters) => reporters_line(&unreported, &reporters),
        Err(e) => fail(NAME, e),
    }
}

/// The doctor line of the check run retention (feat/db-check-run-retention):
/// `warn` (no FG code: nothing failed, the worker is late) while due runs
/// remain past the window plus a day.
pub(super) fn retention_line(backlog: &forge_db::checks::RetentionBacklog, days: u32) -> Check {
    if backlog.due == 0 {
        return ok(
            RETENTION,
            &format!("no check run past the {days}-day window is left"),
        );
    }
    let more = if backlog.due >= forge_db::checks::BACKLOG_COUNT_LIMIT {
        "at least "
    } else {
        ""
    };
    let oldest = backlog
        .oldest_age_days
        .map_or_else(String::new, |d| format!(", the oldest {d} days old"));
    warn(
        RETENTION,
        &format!(
            "{more}{} check run(s) past the {days}-day window are left{oldest} (is the worker's              check_run_retention task running? runbook checks-reporting, E)",
            backlog.due
        ),
    )
}

/// `checks.retention`.
pub(super) async fn retention_check(db: &Db, days: u32) -> Check {
    match forge_db::checks::retention_backlog(db, days).await {
        Ok(backlog) => retention_line(&backlog, days),
        Err(e) => fail(RETENTION, e),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)

    use super::*;
    use crate::doctor::Outcome;

    fn line(unreported: &[(CheckNamespace, i64)], reporters: &[(CheckNamespace, i64)]) -> Check {
        reporters_line(
            &unreported.iter().copied().collect(),
            &reporters.iter().copied().collect(),
        )
    }

    #[test]
    fn a_required_namespace_without_a_reporter_warns() {
        // A grant of ci on another project does not report this one.
        let c = line(&[(CheckNamespace::Ci, 2)], &[(CheckNamespace::Ci, 1)]);
        let Outcome::Warn(detail) = &c.outcome else {
            panic!("a warning");
        };
        assert!(detail.contains("ci/ (2 rule(s))"), "{detail}");
        // forge/ has no reporter until forge's own checks arrive.
        let c = line(&[(CheckNamespace::Forge, 1)], &[]);
        assert!(matches!(c.outcome, Outcome::Warn(_)));
    }

    #[test]
    fn every_required_check_with_a_reporter_is_ok() {
        let c = line(&[], &[(CheckNamespace::Ci, 1)]);
        assert!(matches!(c.outcome, Outcome::Ok(_)));
        let c = line(&[], &[]);
        assert!(matches!(c.outcome, Outcome::Ok(_)));
    }

    #[test]
    fn a_retention_backlog_warns_and_none_is_ok() {
        use forge_db::checks::RetentionBacklog;
        let none = retention_line(&RetentionBacklog::default(), 180);
        assert!(matches!(none.outcome, Outcome::Ok(_)));
        let late = retention_line(
            &RetentionBacklog {
                due: 3,
                oldest_age_days: Some(250),
            },
            180,
        );
        let Outcome::Warn(detail) = &late.outcome else {
            panic!("a warning");
        };
        assert!(
            detail.contains("3 check run(s)") && detail.contains("250 days"),
            "{detail}"
        );
        let capped = retention_line(
            &RetentionBacklog {
                due: forge_db::checks::BACKLOG_COUNT_LIMIT,
                oldest_age_days: None,
            },
            180,
        );
        let Outcome::Warn(detail) = &capped.outcome else {
            panic!("a warning");
        };
        assert!(detail.starts_with("at least "), "{detail}");
    }
}
