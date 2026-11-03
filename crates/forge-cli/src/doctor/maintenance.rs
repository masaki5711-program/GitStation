//! The repository maintenance checks of `forge doctor` (M3 O3, ADR-0024
//! "実装で決めたこと（O3）", runbook repo-maintenance), from the ledger
//! `forge.repository_maintenance` on the read pool:
//!
//! - `repositories.maintenance` — `FG-GIT-0018` `reason=failing` when a
//!   repository's maintenance failed twice or more in a row (the alert of
//!   a warn code: one failure is retried the next night), else
//!   `reason=overdue` when a ready repository is more than one
//!   `maintenance.interval_days` past its due time (no worker runs the
//!   maintenance, or its window never comes);
//! - `repositories.integrity` — `FG-GIT-0019` when the last fsck of a
//!   repository failed;
//! - `repositories.refs` — a warn (no code: measure only, plan 11 Q5) when
//!   a repository has more refs than `maintenance.refs_warn`.
//!
//! Each failure names at most `REPORT_LIST_MAX` repositories (ids only).

use forge_core::config::MaintenanceConfig;
use forge_db::maintenance::{report, Flagged, MaintenanceReport};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};

use super::{fail, ok, skip, warn, Check};

pub(super) const MAINTENANCE: &str = "repositories.maintenance";
pub(super) const INTEGRITY: &str = "repositories.integrity";
pub(super) const REFS: &str = "repositories.refs";

fn ids(list: &[Flagged]) -> String {
    list.iter()
        .map(|f| f.repo_id.get().to_string())
        .collect::<Vec<_>>()
        .join(",")
}

/// The three lines of `report` (pure: the tests feed reports directly).
pub(super) fn maintenance_lines(r: &MaintenanceReport, cfg: &MaintenanceConfig) -> [Check; 3] {
    let maintenance = if r.failing > 0 {
        let worst = r.failing_list.first().map_or(0, |f| f.value);
        fail(
            MAINTENANCE,
            ForgeError::new(ErrorCode::RepositoryMaintenanceFailed)
                .with_context("reason", "failing")
                .with_context("repositories", r.failing)
                .with_context("max_consecutive", worst)
                .with_context("repo_ids", ids(&r.failing_list))
                .with_context(
                    "last_error_code",
                    r.failing_list
                        .first()
                        .and_then(|f| f.error_code.as_deref())
                        .unwrap_or(""),
                ),
        )
    } else if r.overdue > 0 {
        fail(
            MAINTENANCE,
            ForgeError::new(ErrorCode::RepositoryMaintenanceFailed)
                .with_context("reason", "overdue")
                .with_context("repositories", r.overdue)
                .with_context(
                    "max_overdue_secs",
                    r.overdue_list.first().map_or(0, |f| f.value),
                )
                .with_context("interval_secs", cfg.interval.as_secs())
                .with_context("repo_ids", ids(&r.overdue_list)),
        )
    } else {
        ok(
            MAINTENANCE,
            &format!(
                "no repository failing or overdue (every {} days)",
                cfg.interval.as_secs() / 86_400
            ),
        )
    };
    let integrity = if r.corrupt > 0 {
        fail(
            INTEGRITY,
            ForgeError::new(ErrorCode::RepositoryCorrupt)
                .with_context("reason", "last_fsck_failed")
                .with_context("repositories", r.corrupt)
                .with_context("repo_ids", ids(&r.corrupt_list)),
        )
    } else {
        ok(
            INTEGRITY,
            "the last fsck of every checked repository is clean",
        )
    };
    let refs = if r.refs_over > 0 {
        warn(
            REFS,
            &format!(
                "{} repositories have more than {} refs (largest {}: repository {}); \
                 measure only, pushes are not refused (runbook repo-maintenance)",
                r.refs_over,
                cfg.refs_warn,
                r.refs_max,
                r.refs_list.first().map_or(0, |f| f.repo_id.get())
            ),
        )
    } else {
        ok(
            REFS,
            &format!(
                "largest ref count {} (warn above {})",
                r.refs_max, cfg.refs_warn
            ),
        )
    };
    [maintenance, integrity, refs]
}

/// `repositories.maintenance`, `repositories.integrity`, `repositories.refs`.
pub(super) async fn maintenance_checks(db: &Db, cfg: &MaintenanceConfig) -> [Check; 3] {
    match report(db, cfg.interval, cfg.refs_warn).await {
        Ok(Some(r)) => maintenance_lines(&r, cfg),
        Ok(None) => [MAINTENANCE, INTEGRITY, REFS].map(|n| skip(n, "schema not migrated")),
        Err(e) => [
            fail(MAINTENANCE, e),
            skip(INTEGRITY, "maintenance ledger unavailable"),
            skip(REFS, "maintenance ledger unavailable"),
        ],
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)

    use forge_core::ids::RepoId;

    use super::*;
    use crate::doctor::Outcome;

    fn flagged(id: i64, value: i64, code: Option<&str>) -> Flagged {
        Flagged {
            repo_id: RepoId::new(id),
            value,
            error_code: code.map(str::to_owned),
        }
    }

    fn code_of(c: &Check) -> (ErrorCode, String) {
        let Outcome::Fail(e) = &c.outcome else {
            panic!("{}: a failure", c.name);
        };
        (e.code(), e.to_string())
    }

    /// Overdue → FG-GIT-0018 reason=overdue; two failures in a row →
    /// reason=failing (named first); a failed fsck → FG-GIT-0019; many refs
    /// → a warn without a code; a clean ledger → three ok lines.
    #[test]
    fn doctor_maintenance_lines_name_overdue_failing_corrupt_and_refs() {
        let cfg = MaintenanceConfig::default();
        let clean = maintenance_lines(&MaintenanceReport::default(), &cfg);
        assert!(clean.iter().all(|c| matches!(c.outcome, Outcome::Ok(_))));
        let overdue = MaintenanceReport {
            overdue: 2,
            overdue_list: vec![flagged(7, 900_000, None), flagged(9, 700_000, None)],
            ..MaintenanceReport::default()
        };
        let [m, i, r] = maintenance_lines(&overdue, &cfg);
        let (code, text) = code_of(&m);
        assert_eq!(code, ErrorCode::RepositoryMaintenanceFailed);
        assert!(
            text.contains("reason=overdue") && text.contains("repo_ids=7,9"),
            "{text}"
        );
        assert!(matches!(i.outcome, Outcome::Ok(_)));
        assert!(matches!(r.outcome, Outcome::Ok(_)));
        let bad = MaintenanceReport {
            failing: 1,
            failing_list: vec![flagged(3, 2, Some("FG-GIT-0002"))],
            corrupt: 1,
            corrupt_list: vec![flagged(4, 0, Some("FG-GIT-0019"))],
            refs_over: 1,
            refs_list: vec![flagged(5, 200_000, None)],
            refs_max: 200_000,
            ..overdue
        };
        let [m, i, r] = maintenance_lines(&bad, &cfg);
        let (_, text) = code_of(&m);
        assert!(
            text.contains("reason=failing") && text.contains("last_error_code=FG-GIT-0002"),
            "{text}"
        );
        let (code, text) = code_of(&i);
        assert_eq!(code, ErrorCode::RepositoryCorrupt);
        assert!(text.contains("repo_ids=4"), "{text}");
        let Outcome::Warn(detail) = &r.outcome else {
            panic!("refs warn");
        };
        assert!(
            detail.contains("200000") && detail.contains("repository 5"),
            "{detail}"
        );
    }
}
