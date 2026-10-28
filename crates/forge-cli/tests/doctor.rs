#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! `forge doctor` and `forge db` end to end. The database tests need the
//! development database (ADR-0011) and fail with the compose hint when it
//! is down; they run against a freshly bootstrapped database (ADR-0018).

use std::io::Write as _;
use std::process::{Command, Output};

use forge_db::test_support::{
    dev_url, url_as, ClusterLock, FreshDb, PasswordFile, APP_PASSWORD, HINT,
};

fn forge_in(args: &[&str], vars: &[(String, String)], extra: &[(&str, &str)]) -> Output {
    let mut env: Vec<(&str, &str)> = vars.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    env.extend_from_slice(extra);
    forge(args, &env)
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
}

#[test]
fn doctor_passes_against_a_bootstrapped_database_and_changes_nothing() {
    let rt = runtime();
    let _lock = rt.block_on(ClusterLock::shared());
    let t = rt.block_on(FreshDb::new());
    let (vars, _pw) = t.env();
    let storage = tempfile::tempdir().expect("storage root");
    let root = storage.path().to_string_lossy().into_owned();
    // A fresh database has pending migrations: doctor reports them.
    let pending = forge_in(&["doctor"], &vars, &[("FORGE_STORAGE_ROOT", root.as_str())]);
    assert_eq!(pending.status.code(), Some(1), "{}", stdout(&pending));
    assert!(
        stdout(&pending).contains("reason=pending"),
        "{}",
        stdout(&pending)
    );
    let migrated = forge_in(&["db", "migrate"], &vars, &[]);
    assert!(migrated.status.success(), "{}", stdout(&migrated));
    let out = forge_in(&["doctor"], &vars, &[("FORGE_STORAGE_ROOT", root.as_str())]);
    let text = stdout(&out);
    assert!(
        out.status.success(),
        "{HINT}\n{text}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    for line in [
        "ok    config",
        "ok    database",
        "ok    db.bootstrap",
        "ok    db.connections",
        "ok    schema",
        "ok    outbox.unsequenced_count",
        "ok    outbox.sequencer_lag",
        // C8: nothing sealed yet, nothing to verify (no keys configured).
        "ok    audit.seal_backlog nothing sealed yet",
        "ok    audit.seal_verify 0 seals",
        "ok    jobs.dead",
        "ok    jobs.expired_leases",
        "ok    jobs.queue_depth",
        "ok    subscribers.lag",
        "ok    subscribers.dead_letters",
        "ok    storage",
        "ok    git",
    ] {
        assert!(text.contains(line), "missing {line:?} in:\n{text}");
    }
    // No worker has run against this database: ok, but not hidden.
    assert!(text.contains("worker not observed"), "{text}");
    // Read-only: doctor applied nothing (the status still lists exactly the
    // migrations db migrate applied, none pending).
    let again = forge_in(&["db", "status", "--check"], &vars, &[]);
    assert!(again.status.success(), "{}", stdout(&again));
    assert!(
        stdout(&again).contains("pending    -"),
        "{}",
        stdout(&again)
    );
    rt.block_on(t.drop_now());
}

#[test]
fn db_migrate_runs_on_the_migration_login() {
    let rt = runtime();
    let t = rt.block_on(FreshDb::new());
    let (vars, _pw) = t.env();
    let out = forge_in(&["db", "migrate"], &vars, &[]);
    assert!(
        out.status.success(),
        "{}\n{}",
        stdout(&out),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout(&out).contains("applied "), "{}", stdout(&out));
    let again = forge_in(&["db", "migrate"], &vars, &[]);
    assert!(again.status.success(), "{}", stdout(&again));
    assert!(stdout(&again).contains("up to date"), "{}", stdout(&again));
    // Without the migration login the command names the missing key.
    let app_only: Vec<(String, String)> = vars
        .iter()
        .filter(|(k, _)| !k.starts_with("FORGE_DATABASE_MIGRATE_"))
        .cloned()
        .collect();
    let out = forge_in(&["db", "migrate"], &app_only, &[]);
    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("database.migrate_url"), "{err}");
    rt.block_on(t.drop_now());
}

#[test]
fn doctor_flags_a_database_without_the_bootstrap() {
    let rt = runtime();
    let _lock = rt.block_on(ClusterLock::shared());
    // forge_app on the maintenance database: the roles exist, the objects
    // of step 3 do not.
    let pw = PasswordFile::new(APP_PASSWORD);
    let pw_path = pw.path();
    let base = dev_url();
    let url = url_as("forge_app", base.rsplit_once('/').expect("db").1);
    let out = forge(
        &["doctor"],
        &[
            ("FORGE_DATABASE_URL", url.as_str()),
            ("FORGE_DATABASE_PASSWORD_FILE", pw_path.as_str()),
            ("FORGE_DATABASE_TLS_MODE", "disable"),
        ],
    );
    let text = stdout(&out);
    assert_eq!(out.status.code(), Some(1), "{HINT}\n{text}");
    assert!(text.contains("FAIL  db.bootstrap"), "{text}");
    assert!(text.contains("FG-DB-0009"), "{text}");
    assert!(text.contains("guard=bootstrap_version"), "{text}");
}

#[test]
fn doctor_names_a_weakened_guard_and_an_oversized_pool() {
    let rt = runtime();
    let _lock = rt.block_on(ClusterLock::shared());
    let t = rt.block_on(FreshDb::new());
    rt.block_on(async {
        sqlx_free_tamper(&t).await;
    });
    let (vars, _pw) = t.env();
    let out = forge_in(
        &["doctor", "--json"],
        &vars,
        &[
            ("FORGE_DATABASE_WRITE_MAX_CONNECTIONS", "500"),
            ("FORGE_DATABASE_READ_MAX_CONNECTIONS", "500"),
        ],
    );
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("json");
    assert_eq!(out.status.code(), Some(1));
    let check = |name: &str| {
        v["checks"]
            .as_array()
            .and_then(|c| c.iter().find(|c| c["name"] == name))
            .cloned()
            .unwrap_or_default()
    };
    let bootstrap = check("db.bootstrap");
    assert_eq!(bootstrap["error_code"], "FG-DB-0009", "{v}");
    assert!(
        bootstrap["detail"]
            .as_str()
            .is_some_and(|d| d.contains("guard=event_triggers")),
        "{v}"
    );
    let connections = check("db.connections");
    assert!(
        connections["detail"]
            .as_str()
            .is_some_and(|d| d.contains("reason=connection_budget")),
        "{v}"
    );
    rt.block_on(t.drop_now());
}

/// Disable an event trigger through the forge_db test support (this crate
/// has no direct database access).
async fn sqlx_free_tamper(t: &FreshDb) {
    t.exec_superuser("ALTER EVENT TRIGGER forge_ddl_audit_end DISABLE")
        .await;
}

#[test]
fn serve_refuses_a_configuration_with_the_migration_password() {
    let pw = PasswordFile::new("x");
    let path = pw.path();
    let out = forge(
        &["serve"],
        &[("FORGE_DATABASE_MIGRATE_PASSWORD_FILE", path.as_str())],
    );
    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("not_for_this_command"), "{err}");
    assert!(err.contains("database.migrate_password_file"), "{err}");
}

// ── C6: the job queue ────────────────────────────────────────────────────
#[test]
fn doctor_reports_a_recent_dead_job_and_an_unreaped_lease() {
    let rt = runtime();
    let _lock = rt.block_on(ClusterLock::shared());
    let t = rt.block_on(FreshDb::migrated());
    rt.block_on(t.exec_superuser(
        "INSERT INTO forge.jobs (kind, payload, state, attempts, max_attempts, timeout_ms, \
         origin_request_id, last_error_code, last_error, finished_at) VALUES \
         ('audit.seal', '{\"secret_marker\": 1}', 'dead', 3, 3, 1000, 'req-dead-1', \
          'FG-DB-0002', '{\"code\": \"FG-DB-0002\"}', now()); \
         INSERT INTO forge.jobs (kind, payload, state, attempts, max_attempts, timeout_ms, \
         origin_request_id, worker_id, lease_until) VALUES ('audit.seal', '{}', 'running', \
         1, 3, 1000, 'r-2', 'w1-gone', now() - interval '5 minutes')",
    ));
    let (vars, _pw) = t.env();
    let storage = tempfile::tempdir().expect("storage root");
    let root = storage.path().to_string_lossy().into_owned();
    let out = forge_in(&["doctor"], &vars, &[("FORGE_STORAGE_ROOT", root.as_str())]);
    let text = stdout(&out);
    assert_eq!(out.status.code(), Some(1), "{HINT}\n{text}");
    let line = |name: &str| {
        text.lines()
            .find(|l| l.starts_with(&format!("FAIL  {name}")))
            .unwrap_or_else(|| panic!("no {name} failure in:\n{text}"))
            .to_owned()
    };
    let dead = line("jobs.dead");
    for part in [
        "FG-DB-0006",
        "reason=dead_lettered",
        "kind=audit.seal",
        "attempts=3",
        "last_error_code=FG-DB-0002",
        "origin_request_id=req-dead-1",
    ] {
        assert!(dead.contains(part), "missing {part:?} in {dead:?}");
    }
    let leases = line("jobs.expired_leases");
    for part in ["FG-DB-0017", "reason=reaper_behind", "worker_id=w1-gone"] {
        assert!(leases.contains(part), "missing {part:?} in {leases:?}");
    }
    assert!(text.contains("ok    jobs.queue_depth"), "{text}");
    assert!(
        !text.contains("secret_marker"),
        "payloads stay out:\n{text}"
    );
    // Both alert; the earlier check (jobs.dead) is the one printed.
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("[FG-DB-0006]"), "{err}");
    rt.block_on(t.drop_now());
}
// ── end C6 ───────────────────────────────────────────────────────────────

#[test]
fn doctor_reports_a_stale_sequencer_with_a_backlog_as_outbox_sequencer_lag() {
    let rt = runtime();
    let _lock = rt.block_on(ClusterLock::shared());
    let t = rt.block_on(FreshDb::migrated());
    // Committed events and no sequencer ever ran (no heartbeat).
    rt.block_on(t.exec_superuser(
        "INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id) \
         VALUES ('system', 'repository.created', 'succeeded', 'repository', 'r-1'); \
         INSERT INTO forge.events (event_id, event_type, schema_version, aggregate_type, \
         aggregate_id, actor_kind, request_id, payload) \
         SELECT gen_random_uuid(), 'repository.created', 1, 'repository', g, 'system', 'r-1', '{}' \
         FROM generate_series(1, 3) AS g",
    ));
    let (vars, _pw) = t.env();
    let storage = tempfile::tempdir().expect("storage root");
    let root = storage.path().to_string_lossy().into_owned();
    let out = forge_in(&["doctor"], &vars, &[("FORGE_STORAGE_ROOT", root.as_str())]);
    let text = stdout(&out);
    assert_eq!(out.status.code(), Some(1), "{HINT}\n{text}");
    assert!(
        text.contains("ok    outbox.unsequenced_count 3 events"),
        "{text}"
    );
    let lag = text
        .lines()
        .find(|l| l.starts_with("FAIL  outbox.sequencer_lag"))
        .unwrap_or_else(|| panic!("no lag failure in:\n{text}"));
    for part in [
        "FG-DB-0005",
        "unsequenced_count=3",
        "heartbeat_age_ms=none",
        "lock_holder_pid=none",
    ] {
        assert!(lag.contains(part), "missing {part:?} in {lag:?}");
    }
    // Only the lag fails: the worst failure printed is FG-DB-0005.
    assert_eq!(text.matches("FAIL").count(), 1, "{text}");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("[FG-DB-0005]"), "{err}");
    rt.block_on(t.drop_now());
}

#[test]
fn worker_refuses_a_configuration_with_the_migration_password() {
    let pw = PasswordFile::new("x");
    let path = pw.path();
    let out = forge(
        &["worker"],
        &[("FORGE_DATABASE_MIGRATE_PASSWORD_FILE", path.as_str())],
    );
    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("not_for_this_command"), "{err}");
    assert!(err.contains("database.migrate_password_file"), "{err}");
}

#[test]
fn worker_refuses_a_database_with_pending_migrations_and_never_migrates() {
    let rt = runtime();
    let _lock = rt.block_on(ClusterLock::shared());
    let t = rt.block_on(FreshDb::new());
    let (vars, _pw) = t.env();
    let mut app_only: Vec<(String, String)> = vars
        .iter()
        .filter(|(k, _)| !k.starts_with("FORGE_DATABASE_MIGRATE_"))
        .cloned()
        .collect();
    // The worker needs its seal keys before it looks at the database.
    let keys_dir = tempfile::tempdir().expect("keys dir");
    let keys = keys_dir.path().join("audit_seal_keys");
    std::fs::write(&keys, format!("1 {}\n", "42".repeat(32))).expect("keys");
    app_only.push((
        "FORGE_AUDIT_SEAL_KEYS_FILE".to_owned(),
        keys.to_string_lossy().into_owned(),
    ));
    let out = forge_bounded(&["worker"], &app_only);
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{HINT}\n{err}");
    assert!(err.contains("FG-DB-0004"), "{err}");
    assert!(err.contains("reason=pending"), "{err}");
    // Nothing was applied: every migration is still pending.
    let status = forge_in(&["db", "status"], &vars, &[]);
    let text = stdout(&status);
    assert!(text.contains("applied    -"), "{text}");
    rt.block_on(t.drop_now());
}

/// `forge` that must exit by itself: killed (and the test failed) after 60 s
/// so a worker that did not refuse cannot hang the suite.
fn forge_bounded(args: &[&str], vars: &[(String, String)]) -> Output {
    let mut cmd = command(args, &[]);
    cmd.envs(vars.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn().expect("spawn forge");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while child.try_wait().expect("poll forge").is_none() {
        if std::time::Instant::now() > deadline {
            let _ = child.kill(); // best effort: the assertion below fails anyway
            panic!("forge {args:?} did not exit");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    child.wait_with_output().expect("forge output")
}

fn forge(args: &[&str], env: &[(&str, &str)]) -> Output {
    command(args, env).output().expect("run forge")
}

fn command(args: &[&str], env: &[(&str, &str)]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_forge"));
    for (name, _) in std::env::vars_os() {
        let name = name.to_string_lossy().into_owned();
        if name.starts_with("FORGE_") || name.starts_with("PG") {
            cmd.env_remove(name);
        }
    }
    cmd.args(["--lang", "en"])
        .args(args)
        .envs(env.iter().copied());
    cmd
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn doctor_reports_each_check_and_exits_1_on_failure() {
    let out = forge(&["doctor"], &[]);
    let text = stdout(&out);
    assert_eq!(out.status.code(), Some(1), "{text}");
    for line in [
        "ok    config",
        "FAIL  database",
        "skip  schema",
        "FAIL  storage",
    ] {
        assert!(text.contains(line), "missing {line:?} in:\n{text}");
    }
    assert!(text.contains("FG-SYS-0002"), "{text}");
}

#[test]
fn doctor_flags_an_invalid_config_key_and_skips_what_depends_on_it() {
    let out = forge(&["doctor", "--json"], &[("FORGE_DATABASE_URLL", "x")]);
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("json");
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(v["ok"], false);
    assert_eq!(v["checks"][0]["name"], "config");
    assert_eq!(v["checks"][0]["error_code"], "FG-SYS-0002");
    assert_eq!(v["checks"][1]["status"], "skip");
}

#[test]
fn logs_go_to_stderr_so_stdout_stays_machine_readable() {
    let out = forge(&["doctor", "--json"], &[]);
    let text = stdout(&out);
    assert_eq!(
        text.lines().count(),
        1,
        "stdout must be exactly the JSON:\n{text}"
    );
    serde_json::from_str::<serde_json::Value>(&text).expect("json");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("FG-SYS-0002"),
        "the failure is logged on stderr"
    );
}

#[test]
fn doctor_reports_an_unreachable_database_and_a_missing_storage_root() {
    let closed_port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        l.local_addr().expect("addr").port()
    };
    let url = format!("postgres://forge@127.0.0.1:{closed_port}/forge");
    let mut pw = tempfile::NamedTempFile::new().expect("password file");
    pw.write_all(b"x").expect("write");
    let pw_path = pw.path().to_string_lossy().into_owned();
    let gone = tempfile::tempdir().expect("dir");
    let missing_root = gone.path().join("not-there").to_string_lossy().into_owned();
    let out = forge(
        &["doctor"],
        &[
            ("FORGE_DATABASE_URL", url.as_str()),
            ("FORGE_DATABASE_PASSWORD_FILE", pw_path.as_str()),
            ("FORGE_DATABASE_TLS_MODE", "disable"),
            ("FORGE_DATABASE_CONNECT_TIMEOUT_MS", "3000"),
            ("FORGE_STORAGE_ROOT", missing_root.as_str()),
        ],
    );
    let text = stdout(&out);
    assert_eq!(out.status.code(), Some(1), "{text}");
    assert!(text.contains("FAIL  database"), "{text}");
    assert!(text.contains("FG-DB-0001"), "{text}");
    assert!(text.contains("reason=connect_refused"), "{text}");
    assert!(text.contains("skip  schema"), "{text}");
    assert!(text.contains("skip  outbox.unsequenced_count"), "{text}");
    assert!(text.contains("skip  outbox.sequencer_lag"), "{text}");
    assert!(text.contains("skip  subscribers.lag"), "{text}");
    assert!(text.contains("skip  subscribers.dead_letters"), "{text}");
    assert!(text.contains("FAIL  storage"), "{text}");
    assert!(
        !text.contains("not-there"),
        "paths stay in the log:\n{text}"
    );
    // Every failed check is logged (stderr), not just the worst one.
    let err = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(err.matches("FG-DB-0001").count() >= 1, "{err}");
    assert!(err.contains("FG-REPO-") || err.contains("FG-SYS-"), "{err}");
}
