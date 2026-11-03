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
}
