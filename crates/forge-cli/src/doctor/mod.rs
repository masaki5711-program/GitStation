//! `forge doctor` — read-only checks of everything `forge serve` needs:
//! configuration, database (reachable, TLS, version, read role), the
//! bootstrap guards of ADR-0018 (`db.bootstrap`), the connection budget
//! (`db.connections`), schema, the outbox backlog and sequencer lag
//! (`outbox.unsequenced_count`, `outbox.sequencer_lag`), the job queue
//! (`jobs.dead`, `jobs.expired_leases`, `jobs.queue_depth`), storage root and
//! the git binary. One line
//! per check (`--json` for scripts and the later diag bundle). Nothing is
//! created, migrated or changed: every database check runs on the read
//! pool.
//!
//! A check that depends on an earlier failed one is `skip`. The command
//! fails with the most severe error found (alert before warn before info),
//! so the exit code and the logged FG code name the worst problem.

use std::path::Path;

use forge_core::config::{self, Config};
use forge_core::{telemetry, RequestId};
use forge_db::{bootstrap, jobs, sequencer, Db, MIGRATOR};
use forge_errors::{ForgeError, Severity};
use forge_git::{Git, GitConfig, StorageRoot};

use crate::runtime::block_on;

mod render;

/// The outcome of one check.
enum Outcome {
    Ok(String),
    Fail(ForgeError),
    Skip(&'static str),
}

struct Check {
    name: &'static str,
    outcome: Outcome,
}

/// The rendered report and the most severe failure, if any.
pub struct Report {
    pub text: String,
    pub failure: Option<ForgeError>,
}

/// The checks that need a connected database.
const DB_CHECKS: [&str; 8] = [
    "db.bootstrap",
    "db.connections",
    "schema",
    "outbox.unsequenced_count",
    "outbox.sequencer_lag",
    // C6: the job queue.
    "jobs.dead",
    "jobs.expired_leases",
    "jobs.queue_depth",
];

/// Every failed check is logged once here (with `request_id`); the caller
/// prints the worst one without logging it again.
pub fn run(
    config_file: Option<&Path>,
    as_json: bool,
    request_id: &RequestId,
) -> Result<Report, ForgeError> {
    let checks = block_on(async { Ok(collect(config_file).await) })?;
    for c in &checks {
        if let Outcome::Fail(e) = &c.outcome {
            telemetry::log_failure(e, request_id);
        }
    }
    let text = if as_json {
        render::json(&checks)
    } else {
        render::text(&checks)
    };
    Ok(Report {
        text,
        failure: most_severe(checks),
    })
}

async fn collect(config_file: Option<&Path>) -> Vec<Check> {
    let mut checks = Vec::new();
    let cfg = config::Env::from_process().and_then(|env| config::load(config_file, &env));
    let cfg: Option<Config> = match cfg {
        Ok(cfg) => {
            checks.push(ok("config", "loaded"));
            Some(cfg)
        }
        Err(e) => {
            checks.push(fail("config", e));
            None
        }
    };
    let Some(cfg) = cfg else {
        for name in ["database"].into_iter().chain(DB_CHECKS).chain(["storage"]) {
            checks.push(skip(name, "configuration invalid"));
        }
        checks.push(git_check().await);
        return checks;
    };

    match Db::connect(&cfg.database).await {
        Ok(db) => {
            checks.push(ok("database", db.target()));
            checks.extend(database_checks(&db, &cfg).await);
            db.close().await;
        }
        Err(e) => {
            checks.push(fail("database", e));
            for name in DB_CHECKS {
                checks.push(skip(name, "database unavailable"));
            }
        }
    }

    checks.push(match cfg.storage_root() {
        Ok(root) => match StorageRoot::open(root) {
            Ok(opened) => ok("storage", &opened.path().display().to_string()),
            Err(e) => fail("storage", e),
        },
        Err(e) => fail("storage", e),
    });
    checks.push(git_check().await);
    checks
}

async fn database_checks(db: &Db, cfg: &Config) -> Vec<Check> {
    let bootstrap = match bootstrap::check(db, &cfg.database.privileged_members).await {
        Ok(version) => ok("db.bootstrap", &format!("version {version}, guards intact")),
        Err(e) => fail("db.bootstrap", e),
    };
    let connections = match bootstrap::check_connections(db, &cfg.database).await {
        Ok(b) => ok(
            "db.connections",
            &format!("{} needed of {} usable", b.needed, b.usable),
        ),
        Err(e) => fail("db.connections", e),
    };
    let schema = match forge_db::status(db, &MIGRATOR).await {
        Ok(st) => match st.incompatibility() {
            None => ok(
                "schema",
                &format!("compatible ({} applied)", st.applied.len()),
            ),
            Some(e) => fail("schema", e.with_context("target", db.target())),
        },
        Err(e) => fail("schema", e),
    };
    let [unsequenced, lag] = outbox_checks(db).await;
    let [dead, leases, depth] = job_checks(db).await;
    vec![
        bootstrap,
        connections,
        schema,
        unsequenced,
        lag,
        dead,
        leases,
        depth,
    ]
}

// ── C6: the job queue ────────────────────────────────────────────────────
/// `jobs.dead` (`FG-DB-0006`), `jobs.expired_leases` and `jobs.queue_depth`
/// (`FG-DB-0017`), ADR-0016 §7.
async fn job_checks(db: &Db) -> [Check; 3] {
    const NAMES: [&str; 3] = ["jobs.dead", "jobs.expired_leases", "jobs.queue_depth"];
    let h = match jobs::health(db).await {
        Ok(Some(h)) => h,
        Ok(None) => return NAMES.map(|n| skip(n, "schema not migrated")),
        Err(e) => {
            return [
                fail(NAMES[0], e),
                skip(NAMES[1], "job health unavailable"),
                skip(NAMES[2], "job health unavailable"),
            ]
        }
    };
    let dead = match h.check_dead() {
        Ok(()) => ok(
            NAMES[0],
            &format!(
                "none in the last 24 h ({} in total)",
                sequencer::shown(h.dead_total)
            ),
        ),
        Err(e) => fail(NAMES[0], e),
    };
    let leases = match h.check_expired_leases() {
        Ok(()) => ok(NAMES[1], "none unreaped"),
        Err(e) => fail(NAMES[1], e),
    };
    let depth = match h.check_queue_depth() {
        Ok(()) => ok(NAMES[2], &depth_detail(&h.depth)),
        Err(e) => fail(NAMES[2], e),
    };
    [dead, leases, depth]
}

fn depth_detail(depth: &[jobs::KindDepth]) -> String {
    if depth.is_empty() {
        return "no ready jobs".to_owned();
    }
    depth
        .iter()
        .map(|k| format!("{} {}", k.kind, sequencer::shown(k.ready)))
        .collect::<Vec<_>>()
        .join(", ")
}
// ── end C6 ───────────────────────────────────────────────────────────────

/// `outbox.unsequenced_count` (what waits for a number) and
/// `outbox.sequencer_lag` (`FG-DB-0005`, ADR-0016 §7).
async fn outbox_checks(db: &Db) -> [Check; 2] {
    match sequencer::backlog(db).await {
        Ok(Some(b)) => {
            let audit = b.unsequenced_audit.map_or_else(
                || "audit sequencing off".to_owned(),
                |n| format!("{} audit", sequencer::shown(n)),
            );
            let count = ok(
                "outbox.unsequenced_count",
                &format!("{} events, {audit}", sequencer::shown(b.unsequenced_events)),
            );
            let lag = match b.check_lag() {
                Ok(()) => ok("outbox.sequencer_lag", &lag_ok_detail(&b)),
                Err(e) => fail("outbox.sequencer_lag", e),
            };
            [count, lag]
        }
        Ok(None) => [
            skip("outbox.unsequenced_count", "schema not migrated"),
            skip("outbox.sequencer_lag", "schema not migrated"),
        ],
        Err(e) => [
            fail("outbox.unsequenced_count", e),
            skip("outbox.sequencer_lag", "backlog unavailable"),
        ],
    }
}

/// The ok detail of `outbox.sequencer_lag`. Without a backlog a stale or
/// missing heartbeat is not lag, but it is not a running worker either: say
/// so, so an operator does not read "ok" as "the worker runs".
fn lag_ok_detail(b: &sequencer::Backlog) -> String {
    match (b.heartbeat_stale(), b.heartbeat_age_ms) {
        (false, Some(age)) => format!("heartbeat {age} ms ago"),
        (true, Some(age)) => format!("no backlog, worker not observed (heartbeat {age} ms ago)"),
        (_, None) => "no backlog, worker not observed (no heartbeat yet)".to_owned(),
    }
}

/// `git version` from PATH, with an isolated HOME (no user configuration).
async fn git_check() -> Check {
    let git = Git::new(GitConfig::new("git", std::env::temp_dir()));
    match forge_git::version::detect(&git).await {
        Ok(v) => ok("git", &format!("git {v}")),
        Err(e) => fail("git", e),
    }
}

fn ok(name: &'static str, detail: &str) -> Check {
    Check {
        name,
        outcome: Outcome::Ok(detail.to_owned()),
    }
}

fn fail(name: &'static str, e: ForgeError) -> Check {
    Check {
        name,
        outcome: Outcome::Fail(e),
    }
}

fn skip(name: &'static str, why: &'static str) -> Check {
    Check {
        name,
        outcome: Outcome::Skip(why),
    }
}

fn rank(severity: Severity) -> u8 {
    match severity {
        Severity::Alert => 3,
        Severity::Warn => 2,
        Severity::Info => 1,
    }
}

fn most_severe(checks: Vec<Check>) -> Option<ForgeError> {
    checks
        .into_iter()
        .filter_map(|c| match c.outcome {
            Outcome::Fail(e) => Some(e),
            Outcome::Ok(_) | Outcome::Skip(_) => None,
        })
        // The first of the most severe: earlier checks are more fundamental.
        .rev()
        .max_by_key(|e| rank(e.code().severity()))
}

#[cfg(test)]
mod tests {
    use forge_errors::ErrorCode;

    use super::*;

    #[test]
    fn the_most_severe_failure_wins_and_ties_go_to_the_earlier_check() {
        let checks = vec![
            ok("config", "loaded"),
            fail("database", ForgeError::new(ErrorCode::DbUnavailable)),
            fail("storage", ForgeError::new(ErrorCode::StorageMissing)),
            fail("git", ForgeError::new(ErrorCode::GitUnavailable)),
        ];
        let worst = most_severe(checks).expect("a failure");
        assert_eq!(worst.code(), ErrorCode::DbUnavailable);
    }

    fn backlog(age: Option<i64>) -> sequencer::Backlog {
        sequencer::Backlog {
            unsequenced_events: 0,
            unsequenced_audit: None,
            heartbeat_age_ms: age,
            lock_holder_pid: None,
        }
    }

    #[test]
    fn an_idle_lag_check_says_when_the_worker_is_not_observed() {
        assert_eq!(lag_ok_detail(&backlog(Some(1200))), "heartbeat 1200 ms ago");
        assert_eq!(
            lag_ok_detail(&backlog(Some(90_000))),
            "no backlog, worker not observed (heartbeat 90000 ms ago)"
        );
        assert_eq!(
            lag_ok_detail(&backlog(None)),
            "no backlog, worker not observed (no heartbeat yet)"
        );
    }
}
