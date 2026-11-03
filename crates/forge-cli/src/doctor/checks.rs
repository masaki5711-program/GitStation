//! The Checks API check of `forge doctor` (M3 C1, ADR-0024 §10, runbook
//! checks-reporting): `checks.reporters` — every namespace a protected
//! branch's merge rule requires a check of has an active service account
//! that may report it. A namespace without one blocks every merge into
//! those branches (`required_check_missing`), so it is a `warn` (no FG
//! code: nothing failed, a merge condition cannot be met). Read pool, two
//! statements.

use std::collections::BTreeMap;

use forge_core::CheckNamespace;
use forge_db::Db;

use super::{fail, ok, warn, Check};

const NAME: &str = "checks.reporters";

/// The doctor line of `required` (rules per namespace) against
/// `reporters` (active reporters per namespace).
pub(super) fn reporters_line(
    required: &BTreeMap<CheckNamespace, i64>,
    reporters: &BTreeMap<CheckNamespace, i64>,
) -> Check {
    let missing: Vec<String> = required
        .iter()
        .filter(|(ns, _)| reporters.get(ns).copied().unwrap_or(0) == 0)
        .map(|(ns, rules)| format!("{}/ ({rules} rule(s))", ns.as_str()))
        .collect();
    let have: Vec<String> = reporters
        .iter()
        .map(|(ns, n)| format!("{}/: {n}", ns.as_str()))
        .collect();
    let have = if have.is_empty() {
        "no reporter".to_owned()
    } else {
        have.join(", ")
    };
    if missing.is_empty() {
        ok(
            NAME,
            &format!("every required check namespace has a reporter ({have})"),
        )
    } else {
        warn(
            NAME,
            &format!(
                "required checks no service account may report: {} — every merge into \
                 those branches is blocked (runbook checks-reporting); reporters: {have}",
                missing.join(", ")
            ),
        )
    }
}

/// `checks.reporters`.
pub(super) async fn reporters_check(db: &Db) -> Check {
    let required = match forge_db::checks::required_namespaces(db).await {
        Ok(r) => r,
        Err(e) => return fail(NAME, e),
    };
    match forge_db::checks::reporters(db).await {
        Ok(reporters) => reporters_line(&required, &reporters),
        Err(e) => fail(NAME, e),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)

    use super::*;
    use crate::doctor::Outcome;

    fn line(required: &[(CheckNamespace, i64)], reporters: &[(CheckNamespace, i64)]) -> Check {
        reporters_line(
            &required.iter().copied().collect(),
            &reporters.iter().copied().collect(),
        )
    }

    #[test]
    fn a_required_namespace_without_a_reporter_warns() {
        let c = line(
            &[(CheckNamespace::Ci, 2), (CheckNamespace::Kanflow, 1)],
            &[(CheckNamespace::Ci, 1)],
        );
        let Outcome::Warn(detail) = &c.outcome else {
            panic!("a warning");
        };
        assert!(detail.contains("kanflow/ (1 rule(s))"), "{detail}");
        assert!(!detail.contains("ci/ ("), "{detail}");
        // forge/ has no reporter until forge's own checks arrive.
        let c = line(&[(CheckNamespace::Forge, 1)], &[]);
        assert!(matches!(c.outcome, Outcome::Warn(_)));
    }

    #[test]
    fn every_required_namespace_with_a_reporter_is_ok() {
        let c = line(&[(CheckNamespace::Ci, 2)], &[(CheckNamespace::Ci, 1)]);
        assert!(matches!(c.outcome, Outcome::Ok(_)));
        let c = line(&[], &[]);
        assert!(matches!(c.outcome, Outcome::Ok(_)));
    }
}
