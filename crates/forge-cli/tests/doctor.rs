#![allow(clippy::expect_used)] // tests may fail loudly (rust/testing.md)
//! `forge doctor` and `forge db` end to end. The database tests need the
//! development database (ADR-0011) and fail with the compose hint when it
//! is down; they run against a freshly bootstrapped database (ADR-0018).

use std::io::Write as _;
use std::process::{Command, Output};

use forge_db::test_support::{dev_url, FreshDb, HINT};

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
    let t = rt.block_on(FreshDb::new());
    let (vars, _pw) = t.env();
    let storage = tempfile::tempdir().expect("storage root");
    let root = storage.path().to_string_lossy().into_owned();
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
        "ok    storage",
        "ok    git",
    ] {
        assert!(text.contains(line), "missing {line:?} in:\n{text}");
    }
    // Read-only: the bookkeeping table is not created by the schema check.
    let again = forge_in(&["db", "status"], &vars, &[]);
    assert!(again.status.success(), "{}", stdout(&again));
    assert!(
        stdout(&again).contains("applied    -"),
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
    assert!(stdout(&out).contains("up to date"), "{}", stdout(&out));
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
    rt.block_on(forge_db::test_support::ensure_cluster());
    let mut pw = tempfile::NamedTempFile::new().expect("password file");
    pw.write_all(b"forge_dev").expect("write");
    let pw_path = pw.path().to_string_lossy().into_owned();
    let url = dev_url();
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

fn forge(args: &[&str], env: &[(&str, &str)]) -> Output {
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
    cmd.output().expect("run forge")
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
