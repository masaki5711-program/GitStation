//! The backup checks of `forge doctor` (M3 O1, plan 11 §6, runbook
//! backup-restore). No database: they read `backup.dir` and
//! `backup.state_copy_dir` only (names and manifests, never the archives).
//!
//! - `backup.last_success`: the newest complete generation and its age.
//!   Not configured: `warn`. None, or older than `backup.max_age_hours`:
//!   FG-SYS-0008 (`never` / `overdue`, alert). A directory reachable by
//!   others: FG-SYS-0008 `insecure_permissions`.
//! - `backup.state_copy`: the newest state copy. Not configured, or older
//!   than three intervals: `warn` (a restore then removes every
//!   administrator flag, or re-applies older narrowings).

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use forge_core::config::BackupConfig;
use forge_errors::{ErrorCode, ForgeError};

use super::{fail, ok, warn, Check};
use crate::backup;

pub(super) const NAMES: [&str; 2] = ["backup.last_success", "backup.state_copy"];

/// Seconds since the Unix epoch of `YYYYMMDDTHHMMSSZ` (UTC).
pub(crate) fn generation_secs(name: &str) -> Option<i64> {
    if !backup::is_generation_name(name) {
        return None;
    }
    let num = |a: usize, b: usize| name.get(a..b)?.parse::<i64>().ok();
    let (y, m, d) = (num(0, 4)?, num(4, 6)?, num(6, 8)?);
    let (hh, mm, ss) = (num(9, 11)?, num(11, 13)?, num(13, 15)?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || hh > 23 || mm > 59 || ss > 60 {
        return None;
    }
    // Days from the civil date (Howard Hinnant's algorithm).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + hh * 3600 + mm * 60 + ss)
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

fn hours(secs: i64) -> String {
    format!("{:.1} h", secs as f64 / 3600.0)
}

fn sys_0008(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::BackupFailed).with_context("reason", reason)
}

/// `backup.last_success` for the generations in `dir` at `now`.
pub(crate) fn last_success(dir: &Path, max_age: Duration, now: i64) -> Check {
    let name = NAMES[0];
    let dir = match backup::check_private_dir(dir) {
        Ok(d) => d,
        Err(e) => return fail(name, e),
    };
    let gens = match backup::generations(&dir) {
        Ok(g) => g,
        Err(e) => {
            return fail(
                name,
                sys_0008("storage_io")
                    .with_context("operation", "list")
                    .with_source(e),
            )
        }
    };
    let Some((newest, path)) = gens.last() else {
        return fail(name, sys_0008("never").with_context("path", dir.display()));
    };
    let Some(at) = generation_secs(newest) else {
        return fail(
            name,
            sys_0008("storage_io").with_context("generation", newest),
        );
    };
    let age = now.saturating_sub(at);
    let max = i64::try_from(max_age.as_secs()).unwrap_or(i64::MAX);
    if age > max {
        return fail(
            name,
            sys_0008("overdue")
                .with_context("generation", newest)
                .with_context("age_secs", age)
                .with_context("max_age_secs", max),
        );
    }
    let label = backup::read_manifest(path)
        .map(|m| format!(", database {} {}", m.db.tool, m.db.label))
        .unwrap_or_default();
    ok(
        name,
        &format!(
            "newest {newest} ({} ago{label}); {} generation(s) kept",
            hours(age),
            gens.len()
        ),
    )
}

/// `backup.state_copy` for the copies in `dir` at `now`.
pub(crate) fn state_copy(dir: &Path, interval: Duration, now: i64) -> Check {
    let name = NAMES[1];
    let dir = match backup::check_private_dir(dir) {
        Ok(d) => d,
        Err(e) => return fail(name, e),
    };
    let newest = std::fs::read_dir(&dir)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|e| e.file_name().to_str().map(str::to_owned))
        .filter(|n| n.starts_with("state-") && n.ends_with(".fgbk"))
        .max();
    let Some(newest) = newest else {
        return warn(
            name,
            "no state copy yet (forge worker writes one every backup.state_copy_interval_secs): a \
             restore now would remove every administrator flag",
        );
    };
    let at = newest.get(6..22).and_then(generation_secs);
    let age = at.map(|a| now.saturating_sub(a));
    let stale = i64::try_from(interval.as_secs().saturating_mul(3)).unwrap_or(i64::MAX);
    match age {
        Some(a) if a <= stale => ok(name, &format!("newest {newest} ({} ago)", hours(a))),
        Some(a) => warn(
            name,
            &format!(
                "newest {newest} is {} old (more than three intervals): is forge worker running? \
                 (runbook backup-restore)",
                hours(a)
            ),
        ),
        None => warn(name, &format!("unexpected file name {newest}")),
    }
}

pub(super) fn backup_checks(cfg: &BackupConfig) -> [Check; 2] {
    let now = now_secs();
    let last = match &cfg.dir {
        None => warn(
            NAMES[0],
            "backup.dir not configured: forge backup cannot run (runbook backup-restore)",
        ),
        Some(dir) => last_success(dir, cfg.max_age, now),
    };
    let copy = match &cfg.state_copy_dir {
        None => warn(
            NAMES[1],
            "backup.state_copy_dir not configured: a restore removes every administrator flag \
             instead of re-applying the removed permissions",
        ),
        Some(dir) => state_copy(dir, cfg.state_copy_interval, now),
    };
    [last, copy]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generation_names_are_utc_seconds() {
        assert_eq!(generation_secs("19700101T000000Z"), Some(0));
        assert_eq!(generation_secs("20261109T021500Z"), Some(1_794_190_500));
        assert_eq!(generation_secs("20261309T021500Z"), None);
        assert_eq!(generation_secs("2026-11-09"), None);
    }

    #[cfg(unix)]
    #[test]
    fn last_success_is_never_overdue_or_ok() {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = tempfile::tempdir().expect("tmp");
        let dir = tmp.path().join("b");
        std::fs::create_dir(&dir).expect("dir");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).expect("mode");
        let day = Duration::from_secs(26 * 3600);
        let reason = |c: &Check| match &c.outcome {
            super::super::Outcome::Fail(e) => e
                .context()
                .iter()
                .find(|(k, _)| *k == "reason")
                .map(|(_, v)| v.clone()),
            super::super::Outcome::Ok(_)
            | super::super::Outcome::Warn(_)
            | super::super::Outcome::Skip(_) => None,
        };
        let c = last_success(&dir, day, 1_794_190_500);
        assert_eq!(reason(&c).as_deref(), Some("never"));
        let g = dir.join("20261109T021500Z");
        std::fs::create_dir(&g).expect("gen");
        std::fs::write(g.join(backup::MANIFEST), b"{}").expect("manifest");
        let c = last_success(&dir, day, 1_794_190_500 + 3600);
        assert!(matches!(c.outcome, super::super::Outcome::Ok(_)));
        let c = last_success(&dir, day, 1_794_190_500 + 27 * 3600);
        assert_eq!(reason(&c).as_deref(), Some("overdue"));
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).expect("mode");
        let c = last_success(&dir, day, 1_794_190_500 + 3600);
        assert_eq!(reason(&c).as_deref(), Some("insecure_permissions"));
    }
}
