#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! The audit seal end to end (ADR-0018 §4, step C8): what `forge doctor`
//! reports for seals checked without keys, an unreadable key file, a
//! tampered seal and a seal backlog, and `forge worker` refusing to start
//! without `audit.seal_keys_file`. Needs the development database
//! (ADR-0011).

use std::io::Write as _;
use std::process::{Command, Output};

use forge_core::config::SealKeys;
use forge_db::seal::{seal_pending, SealOutcome};
use forge_db::sequencer::{run_once, AuditSequencing, BatchSize};
use forge_db::test_support::{ClusterLock, FreshDb, HINT};

/// The synthetic key the seals below are signed with.
const KEY_LINE: &str = "1 4242424242424242424242424242424242424242424242424242424242424242";

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
}

fn command(args: &[&str], vars: &[(String, String)], extra: &[(&str, &str)]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_forge"));
    for (name, _) in std::env::vars_os() {
        let name = name.to_string_lossy().into_owned();
        if name.starts_with("FORGE_") || name.starts_with("PG") {
            cmd.env_remove(name);
        }
    }
    cmd.args(["--lang", "en"])
        .args(args)
        .envs(vars.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .envs(extra.iter().copied());
    cmd
}

fn forge(args: &[&str], vars: &[(String, String)], extra: &[(&str, &str)]) -> Output {
    command(args, vars, extra).output().expect("run forge")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// A key file with `text` (kept until dropped).
fn key_file(text: &str) -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().expect("dir");
    let path = dir.path().join("audit_seal_keys");
    let mut f = std::fs::File::create(&path).expect("create");
    f.write_all(text.as_bytes()).expect("write");
    (dir, path.to_string_lossy().into_owned())
}

/// The doctor line of `name` (any status).
fn line(text: &str, name: &str) -> String {
    text.lines()
        .find(|l| l.get(6..).is_some_and(|rest| rest.starts_with(name)))
        .unwrap_or_else(|| panic!("no {name} line in:\n{text}"))
        .to_owned()
}

/// Number every audit row (audit sequencing on, as the superuser) and seal.
async fn number_and_seal(t: &FreshDb) {
    run_once(&t.db, BatchSize::MAX, AuditSequencing::Skip)
        .await
        .expect("feed state");
    t.exec_superuser("UPDATE forge.event_feed_state SET audit_sequencing_enabled = true")
        .await;
    while run_once(&t.db, BatchSize::MAX, AuditSequencing::IfEnabled)
        .await
        .expect("sequence")
        .audit
        .is_some()
    {}
    let keys = SealKeys::parse(KEY_LINE).expect("keys");
    while let SealOutcome::Sealed(_) = seal_pending(&t.db, &keys).await.expect("seal") {}
}

#[test]
fn doctor_reports_unchecked_hmacs_a_bad_key_file_a_tampered_seal_and_a_backlog() {
    let rt = runtime();
    let _lock = rt.block_on(ClusterLock::shared());
    let t = rt.block_on(FreshDb::migrated());
    rt.block_on(number_and_seal(&t));
    let (vars, _pw) = t.env();
    let storage = tempfile::tempdir().expect("storage root");
    let root = storage.path().to_string_lossy().into_owned();
    let storage_var = ("FORGE_STORAGE_ROOT", root.as_str());

    // Seals exist, no keys here: values and chain only — warn, not ok.
    let out = forge(&["doctor"], &vars, &[storage_var]);
    let text = stdout(&out);
    let verify = line(&text, "audit.seal_verify");
    assert!(verify.starts_with("warn  "), "{HINT}\n{text}");
    assert!(verify.contains("HMAC not checked"), "{verify}");
    assert!(
        line(&text, "audit.seal_backlog").starts_with("ok    "),
        "{text}"
    );

    // An unreadable key file is a failure of the check, never a silent skip.
    let (_bad_dir, bad) = key_file("1 not-hex\n");
    let out = forge(
        &["doctor"],
        &vars,
        &[storage_var, ("FORGE_AUDIT_SEAL_KEYS_FILE", bad.as_str())],
    );
    let text = stdout(&out);
    assert_eq!(out.status.code(), Some(1), "{text}");
    let verify = line(&text, "audit.seal_verify");
    for part in [
        "FAIL",
        "FG-SYS-0002",
        "config_key=audit.seal_keys_file",
        "reason=invalid_key",
    ] {
        assert!(verify.contains(part), "missing {part:?} in {verify:?}");
    }

    // A tampered sealed row and an old unsealed numbered row.
    rt.block_on(t.exec_superuser(
        "ALTER TABLE forge.audit_log DISABLE TRIGGER audit_log_append_only; \
         UPDATE forge.audit_log SET outcome = 'denied' WHERE seq = 1; \
         INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, \
             occurred_at) VALUES ('system', 'repository.viewed', 'succeeded', 'repository', \
             'r-late', now() - interval '6 minutes'); \
         ALTER TABLE forge.audit_log ENABLE ALWAYS TRIGGER audit_log_append_only;",
    ));
    rt.block_on(async {
        while run_once(&t.db, BatchSize::MAX, AuditSequencing::IfEnabled)
            .await
            .expect("sequence")
            .audit
            .is_some()
        {}
    });
    let (_good_dir, good) = key_file(&format!("{KEY_LINE}\n"));
    let out = forge(
        &["doctor"],
        &vars,
        &[storage_var, ("FORGE_AUDIT_SEAL_KEYS_FILE", good.as_str())],
    );
    let text = stdout(&out);
    assert_eq!(out.status.code(), Some(1), "{text}");
    let backlog = line(&text, "audit.seal_backlog");
    for part in [
        "FAIL",
        "FG-DB-0015",
        "unsealed_numbered=1",
        "audit_sequencing=on",
    ] {
        assert!(backlog.contains(part), "missing {part:?} in {backlog:?}");
    }
    let verify = line(&text, "audit.seal_verify");
    for part in [
        "FAIL",
        "FG-DB-0011",
        "reason=recompute",
        "detail=rows_sha256",
        "first_seq=1",
    ] {
        assert!(verify.contains(part), "missing {part:?} in {verify:?}");
    }
    // Disabling the guard to tamper is itself unexpected DDL: db.bootstrap
    // (FG-DB-0009, an earlier check) is the one printed; both seal alerts
    // are logged with the same request id.
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("[FG-DB-0009]"), "{err}");
    for code in ["FG-DB-0015", "FG-DB-0011"] {
        assert!(err.contains(code), "{code} not logged: {err}");
    }
    rt.block_on(t.drop_now());
}

#[test]
fn worker_refuses_to_start_without_the_seal_keys() {
    // Checked before the database is touched: no database needed.
    let out = forge(&["worker"], &[], &[]);
    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8_lossy(&out.stderr);
    for part in ["FG-SYS-0002", "audit.seal_keys_file", "reason=missing"] {
        assert!(err.contains(part), "missing {part:?} in {err}");
    }
}
