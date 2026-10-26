#![allow(clippy::expect_used)] // tests may fail loudly (rust/testing.md)
//! `forge doctor` end to end. The full-pass test needs the development
//! database (ADR-0011) and fails with the compose hint when it is down.

use std::io::Write as _;
use std::process::{Command, Output};

const HINT: &str = "start the development database: \
    docker compose -f ops/dev/compose.yml up -d --wait (in Git/)";

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
fn doctor_passes_against_the_dev_database_and_changes_nothing() {
    let url = std::env::var("FORGE_TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://forge_dev@127.0.0.1:55432/forge_dev".to_owned());
    let mut pw = tempfile::NamedTempFile::new().expect("password file");
    pw.write_all(b"forge_dev").expect("write");
    let pw_path = pw.path().to_string_lossy().into_owned();
    let storage = tempfile::tempdir().expect("storage root");
    let root = storage.path().to_string_lossy().into_owned();
    let env = [
        ("FORGE_DATABASE_URL", url.as_str()),
        ("FORGE_DATABASE_PASSWORD_FILE", pw_path.as_str()),
        ("FORGE_DATABASE_TLS_MODE", "disable"),
        ("FORGE_STORAGE_ROOT", root.as_str()),
    ];
    let out = forge(&["doctor"], &env);
    let text = stdout(&out);
    assert!(
        out.status.success(),
        "{HINT}\n{text}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    for line in [
        "ok    config",
        "ok    database",
        "ok    schema",
        "ok    storage",
        "ok    git",
    ] {
        assert!(text.contains(line), "missing {line:?} in:\n{text}");
    }
    // Read-only: the bookkeeping table is not created by the schema check.
    let again = forge(&["db", "status"], &env);
    assert!(again.status.success());
    assert!(
        stdout(&again).contains("applied    -"),
        "{}",
        stdout(&again)
    );
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
