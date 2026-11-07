//! The push mirror checks of `forge doctor` (M3 O2, ADR-0024 Amendment
//! O2, runbook gitlab-mirror), from `forge_db::mirror::report` on the read
//! pool (which never sees a mirror's full URL):
//!
//! - `mirror.config` — warn when mirrors exist but `mirror.credentials_dir`
//!   is not set (the worker's mirror lane is off: nothing is pushed) or is
//!   not a readable directory; warn when `mirror.allow_http` is on (tests
//!   only); otherwise ok, saying `mirror.allowed_hosts` is empty (nothing
//!   can be set) or how many hosts it lists.
//! - `mirror.status` — `FG-INT-0013` when an enabled mirror's last attempt
//!   failed (`failures > 0`): its repository ids and the last code.
//! - `mirror.lag` — `FG-INT-0014` when an enabled mirror has waited longer
//!   than `mirror.lag_alert_secs` (the worker's alert, seen from here):
//!   the repository ids and the longest wait.
//!
//! Both codes are alerts, as the worker logs them; each line names at most
//! `forge_db::mirror::LIST_MAX` repositories (ids only, never a URL).

use forge_core::config::MirrorConfig;
use forge_db::mirror::{report, Lagging, MirrorReport};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};

use super::{fail, ok, skip, warn, Check};

pub(super) const CONFIG: &str = "mirror.config";
pub(super) const STATUS: &str = "mirror.status";
pub(super) const LAG: &str = "mirror.lag";
/// The three names (for the database-unavailable skips).
pub(super) const NAMES: [&str; 3] = [CONFIG, STATUS, LAG];

fn ids(list: &[Lagging]) -> String {
    list.iter()
        .map(|l| l.repo.get().to_string())
        .collect::<Vec<_>>()
        .join(",")
}

/// Whether the directory can be listed (`None`: not configured).
fn dir_readable(cfg: &MirrorConfig) -> Option<bool> {
    cfg.credentials_dir
        .as_deref()
        .map(|d| std::fs::read_dir(d).is_ok())
}

/// `mirror.config` for `r` (pure apart from `readable`).
fn config_line(r: &MirrorReport, cfg: &MirrorConfig, readable: Option<bool>) -> Check {
    let hosts = cfg.policy.hosts();
    if r.mirrors > 0 && readable.is_none() {
        return warn(
            CONFIG,
            &format!(
                "{} mirror(s) ({} enabled) but mirror.credentials_dir is not set: forge worker's mirror lane is off and nothing is pushed (runbook gitlab-mirror)",
                r.mirrors, r.enabled
            ),
        );
    }
    if readable == Some(false) {
        return warn(
            CONFIG,
            "mirror.credentials_dir is not a readable directory for this user: every push fails with FG-INT-0013 reason=credential_unreadable",
        );
    }
    if cfg.policy.allow_http() {
        return warn(
            CONFIG,
            "mirror.allow_http is true: mirrors may push over plain http (tests only; set it to false in production)",
        );
    }
    if hosts.is_empty() {
        return ok(
            CONFIG,
            "mirror.allowed_hosts is empty (the default): no mirror can be set until the operator names the GitLab host",
        );
    }
    let listed = hosts
        .iter()
        .map(|h| format!("{}:{}", h.host(), h.port()))
        .collect::<Vec<_>>()
        .join(",");
    ok(
        CONFIG,
        &format!(
            "mirror.allowed_hosts {listed}; credentials_dir {}; {} mirror(s), {} enabled",
            match readable {
                Some(true) => "readable",
                Some(false) | None => "not set",
            },
            r.mirrors,
            r.enabled
        ),
    )
}

/// `mirror.status` and `mirror.lag` for `r` (pure: the tests feed reports).
fn progress_lines(r: &MirrorReport, cfg: &MirrorConfig) -> [Check; 2] {
    let status = match r.failing.first() {
        Some(first) => fail(
            STATUS,
            ForgeError::new(ErrorCode::PushMirrorFailed)
                .with_context("reason", "failing")
                .with_context("mirrors", r.failing.len())
                .with_context("repo_ids", ids(&r.failing))
                .with_context(
                    "last_error_code",
                    first.last_error_code.as_deref().unwrap_or(""),
                ),
        ),
        None => ok(
            STATUS,
            &format!("{} enabled mirror(s); no last attempt failed", r.enabled),
        ),
    };
    let lag = if r.lagging.is_empty() {
        ok(
            LAG,
            &format!(
                "no enabled mirror waits longer than {} s",
                cfg.lag_alert.as_secs()
            ),
        )
    } else {
        let longest = r.lagging.iter().map(|l| l.lag_secs).max().unwrap_or(0);
        fail(
            LAG,
            ForgeError::new(ErrorCode::PushMirrorLagging)
                .with_context("reason", "lagging")
                .with_context("mirrors", r.lagging.len())
                .with_context("repo_ids", ids(&r.lagging))
                .with_context("max_lag_secs", longest)
                .with_context("threshold_secs", cfg.lag_alert.as_secs()),
        )
    };
    [status, lag]
}

/// The three checks.
pub(super) async fn mirror_checks(db: &Db, cfg: &MirrorConfig) -> [Check; 3] {
    match report(db, cfg.lag_alert).await {
        Ok(r) => {
            let [status, lag] = progress_lines(&r, cfg);
            [config_line(&r, cfg, dir_readable(cfg)), status, lag]
        }
        Err(e) => [
            skip(CONFIG, "mirror report unavailable"),
            fail(STATUS, e),
            skip(LAG, "mirror report unavailable"),
        ],
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use forge_core::ids::{ProjectId, RepoId};
    use forge_core::mirror::MirrorPolicy;
    use forge_db::test_support::FreshDb;

    use super::super::Outcome;
    use super::*;

    fn cfg(hosts: &str, http: bool) -> MirrorConfig {
        MirrorConfig {
            policy: MirrorPolicy::parse(hosts, http).expect("policy"),
            lag_alert: Duration::from_secs(900),
            ..MirrorConfig::default()
        }
    }

    fn mirror(repo: i64, lag: i64) -> Lagging {
        Lagging {
            repo: RepoId::new(repo),
            project: ProjectId::new(repo + 100),
            pending_since: "2026-11-07T00:00:00.000000Z".to_owned(),
            lag_secs: lag,
            last_success_at: None,
            last_error_code: Some("FG-INT-0013".to_owned()),
        }
    }

    fn report_of(mirrors: i64, failing: Vec<Lagging>, lagging: Vec<Lagging>) -> MirrorReport {
        MirrorReport {
            mirrors,
            enabled: mirrors,
            failing,
            lagging,
        }
    }

    fn describe(c: &Check) -> String {
        match &c.outcome {
            Outcome::Ok(s) | Outcome::Warn(s) => s.clone(), // clone: the message for the assertion
            Outcome::Fail(e) => e.code().id().to_owned(),
            Outcome::Skip(s) => (*s).to_owned(),
        }
    }

    fn is_ok(c: &Check) -> bool {
        matches!(c.outcome, Outcome::Ok(_))
    }

    fn is_warn(c: &Check) -> bool {
        matches!(c.outcome, Outcome::Warn(_))
    }

    fn code(c: &Check) -> Option<ErrorCode> {
        match &c.outcome {
            Outcome::Fail(e) => Some(e.code()),
            Outcome::Ok(_) | Outcome::Warn(_) | Outcome::Skip(_) => None,
        }
    }

    #[test]
    fn the_config_line_warns_for_a_lane_that_cannot_push_and_for_http() {
        let none = report_of(0, Vec::new(), Vec::new());
        let some = report_of(2, Vec::new(), Vec::new());
        let https = cfg("gitlab.example.invalid", false);
        // Empty allowlist: ok, said.
        let empty = config_line(&none, &cfg("", false), None);
        assert!(is_ok(&empty), "{}", describe(&empty));
        assert!(describe(&empty).contains("empty"));
        // Mirrors without credentials_dir: warn.
        assert!(is_warn(&config_line(&some, &https, None)));
        // No mirror and no directory yet: ok.
        assert!(is_ok(&config_line(&none, &https, None)));
        // An unreadable directory: warn.
        assert!(is_warn(&config_line(&none, &https, Some(false))));
        // http allowed: warn.
        assert!(is_warn(&config_line(
            &some,
            &cfg("gitlab.example.invalid", true),
            Some(true)
        )));
        let fine = config_line(&some, &https, Some(true));
        assert!(is_ok(&fine), "{}", describe(&fine));
        assert!(describe(&fine).contains("gitlab.example.invalid:443"));
    }

    #[test]
    fn failing_and_lagging_mirrors_are_int_0013_and_int_0014() {
        let c = cfg("gitlab.example.invalid", false);
        let [status, lag] = progress_lines(&report_of(3, Vec::new(), Vec::new()), &c);
        assert!(is_ok(&status) && is_ok(&lag));
        let [status, lag] = progress_lines(
            &report_of(3, vec![mirror(4, 0), mirror(5, 0)], vec![mirror(5, 1200)]),
            &c,
        );
        assert_eq!(code(&status), Some(ErrorCode::PushMirrorFailed));
        assert_eq!(code(&lag), Some(ErrorCode::PushMirrorLagging));
        let Outcome::Fail(e) = &status.outcome else {
            panic!("fail");
        };
        let ctx = e.context();
        assert!(
            ctx.iter().any(|(k, v)| *k == "repo_ids" && v == "4,5"),
            "{ctx:?}"
        );
        let Outcome::Fail(e) = &lag.outcome else {
            panic!("fail");
        };
        assert!(e
            .context()
            .iter()
            .any(|(k, v)| *k == "max_lag_secs" && v == "1200"));
    }

    #[tokio::test]
    async fn an_empty_database_is_ok() {
        let t = FreshDb::migrated().await;
        let checks = mirror_checks(&t.db, &cfg("", false)).await;
        for c in &checks {
            assert!(is_ok(c), "{}: {}", c.name, describe(c));
        }
        t.drop_now().await;
    }
}
