//! `forge doctor` for the copy of the PFS org master (ADR-0038 §1, §4;
//! runbook oidc-login 「組織の台帳の写し」):
//!
//! - `org.master` (database): skipped while `org.master_sync_enabled` is
//!   off; never synced or older than `org.master_max_age_hours` is
//!   `FG-INT-0028` (alert: everything derived from the copy fails closed);
//!   otherwise the counts and the age of the last sync.

use forge_core::config::Config;
use forge_db::org_master::MasterHealth;
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};

use super::{fail, ok, skip, Check};

pub(super) const NAME: &str = "org.master";

/// `org.master`.
pub(super) async fn master_check(db: &Db, cfg: &Config) -> Check {
    if !cfg.org.master_sync_enabled {
        return skip(
            NAME,
            "org.master_sync_enabled is off: the org master is not copied",
        );
    }
    match forge_db::org_master::health(db).await {
        Ok(h) => master_line(&h, cfg.org.master_max_age.as_secs()),
        Err(e) => fail(NAME, e),
    }
}

fn master_line(h: &MasterHealth, max_age_secs: u64) -> Check {
    let max = i64::try_from(max_age_secs).unwrap_or(i64::MAX);
    let stale = |reason: &'static str| {
        let mut e = ForgeError::new(ErrorCode::OrgMasterStale)
            .with_context("reason", reason)
            .with_context("units", h.units)
            .with_context("positions", h.positions)
            .with_context("max_age_hours", max_age_secs / 3600);
        if let Some(age) = h.age_secs {
            e = e.with_context("age_secs", age);
        }
        fail(NAME, e)
    };
    match (h.age_secs, h.last_synced_at.as_deref()) {
        (Some(age), Some(at)) if h.units > 0 => {
            if age > max {
                return stale("stale");
            }
            ok(
                NAME,
                &format!(
                    "{} unit(s), {} position(s); last sync {at} ({age} s ago)",
                    h.units, h.positions
                ),
            )
        }
        _ => stale("never_synced"),
    }
}

#[cfg(test)]
mod tests {
    use super::super::Outcome;
    use super::*;

    const DAY: u64 = 24 * 3600;

    fn health(age: Option<i64>) -> MasterHealth {
        MasterHealth {
            units: if age.is_some() { 12 } else { 0 },
            positions: 3,
            last_synced_at: age.map(|_| "2026-11-20T00:00:00Z".to_owned()),
            age_secs: age,
        }
    }

    #[test]
    fn a_stale_master_is_org_master_stale() {
        let Outcome::Ok(text) = master_line(&health(Some(600)), DAY).outcome else {
            panic!("a fresh copy is ok");
        };
        assert!(text.contains("12 unit(s)"), "{text}");
        for (h, reason) in [
            (health(Some(25 * 3600)), "reason=stale"),
            (health(None), "reason=never_synced"),
        ] {
            let Outcome::Fail(e) = master_line(&h, DAY).outcome else {
                panic!("{reason} fails");
            };
            assert_eq!(e.code(), ErrorCode::OrgMasterStale);
            assert!(e.to_string().contains(reason), "{e}");
        }
    }
}
