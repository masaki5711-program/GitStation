//! The document lint's migration setting (plan 13 Q13, ADR-0026 "決まった
//! こと"): broken document references (`ref_broken`) are errors; a
//! repository may lower them to warnings while it migrates its documents
//! into GitStation, **until a date**. The date is required, after today
//! and at most [`MAX_DAYS`] days ahead when set; the day after it they are
//! errors again by themselves (nothing is written at the expiry).
//!
//! "Today" is always the database's date in UTC (forge-db compares it in
//! SQL), never a client's or the web server's clock. This module holds the
//! date's shape (pure) and the audit row's details.

use crate::ids::ProjectId;

/// The longest migration, in days after today (UTC) when it is set.
pub const MAX_DAYS: u32 = 180;

/// The rule that is lowered (forge-doc's broken reference).
pub const LOWERED_RULE: &str = "ref_broken";

/// `raw` is a calendar date written `YYYY-MM-DD` (four-digit year from
/// 2000, a real month and day — 2028-02-29 yes, 2026-02-29 no).
pub fn is_calendar_date(raw: &str) -> bool {
    let b = raw.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return false;
    }
    let digits = |r: std::ops::Range<usize>| -> Option<u32> {
        let mut n = 0u32;
        for &c in &b[r] {
            if !c.is_ascii_digit() {
                return None;
            }
            n = n * 10 + u32::from(c - b'0');
        }
        Some(n)
    };
    let (Some(year), Some(month), Some(day)) = (digits(0..4), digits(5..7), digits(8..10)) else {
        return false;
    };
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return false,
    };
    year >= 2000 && (1..=days).contains(&day)
}

/// A change of the setting (`audit_log.details`, on the project; no
/// event).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DocLintMigrationChange {
    /// `doc_lint_migration.set`: a new date (`previous`: the date it
    /// replaced, also an expired one).
    Set {
        project: ProjectId,
        warn_until: String,
        previous: Option<String>,
    },
    /// `doc_lint_migration.cleared`: ended before its date (or an expired
    /// one removed).
    Cleared {
        project: ProjectId,
        warn_until: String,
    },
}

impl DocLintMigrationChange {
    pub fn action(&self) -> &'static str {
        match self {
            Self::Set { .. } => "doc_lint_migration.set",
            Self::Cleared { .. } => "doc_lint_migration.cleared",
        }
    }

    pub fn to_value(&self) -> serde_json::Value {
        match self {
            Self::Set {
                project,
                warn_until,
                previous,
            } => serde_json::json!({
                "project_id": project.get(),
                "warn_until": warn_until,
                "previous": previous,
            }),
            Self::Cleared {
                project,
                warn_until,
            } => serde_json::json!({
                "project_id": project.get(),
                "warn_until": warn_until,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_calendar_dates_are_dates() {
        for good in ["2026-11-10", "2028-02-29", "2000-02-29", "2026-12-31"] {
            assert!(is_calendar_date(good), "{good}");
        }
        for bad in [
            "",
            "2026-2-01",
            "2026-02-29",
            "2100-02-29",
            "2026-13-01",
            "2026-00-10",
            "2026-04-31",
            "2026-01-00",
            "1999-12-31",
            "2026/01/01",
            "2026-01-01T00",
            "２０２６-01-01",
            "+026-01-01",
        ] {
            assert!(!is_calendar_date(bad), "{bad}");
        }
    }

    #[test]
    fn the_audit_details_name_the_dates_only() {
        let set = DocLintMigrationChange::Set {
            project: ProjectId::new(7),
            warn_until: "2026-12-01".into(),
            previous: None,
        };
        assert_eq!(set.action(), "doc_lint_migration.set");
        assert_eq!(
            set.to_value(),
            serde_json::json!({"project_id": 7, "warn_until": "2026-12-01", "previous": null})
        );
        let cleared = DocLintMigrationChange::Cleared {
            project: ProjectId::new(7),
            warn_until: "2026-12-01".into(),
        };
        assert_eq!(cleared.action(), "doc_lint_migration.cleared");
        assert_eq!(
            cleared.to_value(),
            serde_json::json!({"project_id": 7, "warn_until": "2026-12-01"})
        );
    }
}
