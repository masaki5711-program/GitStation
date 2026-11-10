//! The database step of `forge backup` (M3 O1, plan 11 §10 Q11: PostgreSQL
//! through pgBackRest, IoT 12 §11.6). Behind [`DbBackupTool`] so the rest
//! of the backup is tested with a fake; the real tool is [`PgBackRest`]:
//!
//! ```text
//! <backup.pgbackrest_bin> --stanza=<stanza> --type=full --log-level-console=warn backup
//! <backup.pgbackrest_bin> --stanza=<stanza> --output=json info
//! ```
//!
//! Each runs with an argument vector (never a shell), a cleared
//! environment but `PATH` / `HOME` / `LANG` / `PGBACKREST_*` (pgBackRest
//! reads its repository, cipher and retention from `/etc/pgbackrest/
//! pgbackrest.conf`: runbook backup-restore), `backup.db_timeout_secs`, and
//! is killed when the timeout passes. pgBackRest encrypts its repository
//! itself (`repo1-cipher-type=aes-256-cbc`) and expires old backups by
//! `repo1-retention-full` (set it to `backup.generations`).

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use forge_errors::{ErrorCode, ForgeError};

/// The head of stderr kept in an error (redacted).
const STDERR_HEAD: usize = 512;

/// The database backup a run made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DbBackup {
    pub tool: &'static str,
    /// The tool's name of the backup (pgBackRest: its label, e.g.
    /// `20261109-021500F`), for `pgbackrest restore --set=<label>`.
    pub label: String,
}

/// The database step.
pub trait DbBackupTool {
    /// Back the database up now, or fail with `FG-SYS-0008`.
    fn backup(&self) -> impl std::future::Future<Output = Result<DbBackup, ForgeError>>;
}

pub(crate) fn failed(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::BackupFailed).with_context("reason", reason)
}

/// pgBackRest.
#[derive(Debug, Clone)]
pub struct PgBackRest {
    pub bin: PathBuf,
    pub stanza: String,
    pub timeout: Duration,
}

/// Remove what may look like a secret from tool output (a URL's
/// credentials, `password=` / `key=` values), then keep the head.
pub(crate) fn redact(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let mut out = String::with_capacity(text.len().min(STDERR_HEAD));
    for word in text.split_whitespace() {
        let lower = word.to_ascii_lowercase();
        let w = if lower.contains("password")
            || lower.contains("passwd")
            || lower.contains("secret")
            || lower.contains("key=")
            || lower.contains("cipher-pass")
            || word.contains('@') && word.contains("://")
        {
            "[redacted]"
        } else {
            word
        };
        if out.len() + w.len() + 1 > STDERR_HEAD {
            break;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(w);
    }
    out
}

impl PgBackRest {
    async fn run(&self, args: &[&str], step: &'static str) -> Result<Vec<u8>, ForgeError> {
        if !self.bin.is_file() {
            return Err(failed("db_tool_missing").with_context("path", self.bin.display()));
        }
        let mut cmd = tokio::process::Command::new(&self.bin);
        cmd.arg(format!("--stanza={}", self.stanza))
            .args(args)
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for (k, v) in std::env::vars_os() {
            let keep = k.to_str().is_some_and(|k| {
                matches!(k, "PATH" | "HOME" | "LANG") || k.starts_with("PGBACKREST_")
            });
            if keep {
                cmd.env(k, v);
            }
        }
        let child = cmd.spawn().map_err(|e| {
            failed("db_backup_failed")
                .with_context("step", step)
                .with_context("operation", "spawn")
                .with_source(e)
        })?;
        match tokio::time::timeout(self.timeout, child.wait_with_output()).await {
            // Dropping the future drops the child: kill_on_drop kills it.
            Err(_) => Err(failed("db_backup_timeout")
                .with_context("step", step)
                .with_context("timeout_secs", self.timeout.as_secs())),
            Ok(Err(e)) => Err(failed("db_backup_failed")
                .with_context("step", step)
                .with_context("operation", "wait")
                .with_source(e)),
            Ok(Ok(out)) if out.status.success() => Ok(out.stdout),
            Ok(Ok(out)) => Err(failed("db_backup_failed")
                .with_context("step", step)
                .with_context(
                    "exit_code",
                    out.status
                        .code()
                        .map_or_else(|| "signal".to_owned(), |c| c.to_string()),
                )
                .with_context("stderr", redact(&out.stderr))),
        }
    }
}

/// The label of the newest backup in `pgbackrest info --output=json`.
pub(crate) fn newest_label(info: &[u8]) -> Option<String> {
    let v: serde_json::Value = serde_json::from_slice(info).ok()?;
    let backups = v.as_array()?.first()?.get("backup")?.as_array()?;
    let label = backups.last()?.get("label")?.as_str()?;
    let ok = !label.is_empty()
        && label.len() <= 64
        && label
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    ok.then(|| label.to_owned())
}

/// Every label in `pgbackrest info --output=json` (empty before the first
/// backup of a stanza).
pub(crate) fn labels(info: &[u8]) -> Vec<String> {
    let v: serde_json::Value = match serde_json::from_slice(info) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    v.as_array()
        .and_then(|a| a.first())
        .and_then(|s| s.get("backup"))
        .and_then(|b| b.as_array())
        .map(|b| {
            b.iter()
                .filter_map(|x| x.get("label").and_then(|l| l.as_str()))
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

impl DbBackupTool for PgBackRest {
    /// `info` before and after `backup`: the label is the newest one that
    /// was not there before (a backup someone else made meanwhile, or none
    /// at all, is `db_backup_unexpected`, never a wrong label).
    async fn backup(&self) -> Result<DbBackup, ForgeError> {
        let before = self.run(&["--output=json", "info"], "info").await?;
        let before = labels(&before);
        self.run(
            &["--type=full", "--log-level-console=warn", "backup"],
            "backup",
        )
        .await?;
        let info = self.run(&["--output=json", "info"], "info").await?;
        let label = newest_label(&info)
            .filter(|l| !before.contains(l))
            .ok_or_else(|| failed("db_backup_unexpected"))?;
        Ok(DbBackup {
            tool: "pgbackrest",
            label,
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    /// The fake database step of the tests.
    #[derive(Debug, Default)]
    pub(crate) struct FakeDb {
        pub fail: bool,
        pub calls: AtomicUsize,
    }

    impl DbBackupTool for FakeDb {
        async fn backup(&self) -> Result<DbBackup, ForgeError> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                return Err(failed("db_backup_failed").with_context("exit_code", 1));
            }
            Ok(DbBackup {
                tool: "fake",
                label: format!("20261109-02150{n}F"),
            })
        }
    }

    #[test]
    fn the_newest_label_is_read_from_pgbackrest_info() {
        let info = br#"[{"name":"forge","backup":[{"label":"20261108-021500F"},{"label":"20261109-021500F"}]}]"#;
        assert_eq!(newest_label(info).as_deref(), Some("20261109-021500F"));
        assert_eq!(newest_label(b"[]"), None);
        assert_eq!(newest_label(br#"[{"backup":[{"label":"../x"}]}]"#), None);
    }

    #[test]
    fn stderr_is_redacted() {
        let out =
            redact(b"ERROR: connect postgres://forge:hunter2@db/forge failed password=hunter2");
        assert!(!out.contains("hunter2"), "{out}");
        assert!(out.starts_with("ERROR: connect [redacted] failed"), "{out}");
    }

    #[tokio::test]
    async fn a_missing_binary_a_failing_tool_and_a_hanging_tool_are_sys_0008() {
        let tool = PgBackRest {
            bin: PathBuf::from("/nonexistent/pgbackrest"),
            stanza: "forge".to_owned(),
            timeout: Duration::from_secs(5),
        };
        let err = tool.backup().await.expect_err("missing");
        forge_db::test_support::assert_reason(&err, ErrorCode::BackupFailed, "db_tool_missing");
        // The failing, hanging and answering tools are shell scripts (pgBackRest
        // itself is Linux-only).
        #[cfg(unix)]
        scripted_tools(tool).await;
    }

    #[cfg(unix)]
    async fn scripted_tools(tool: PgBackRest) {
        let tmp = tempfile::tempdir().expect("tmp");
        let script = |name: &str, body: &str| {
            let p = tmp.path().join(name);
            forge_git::test_git::write_script(&p, &format!("#!/bin/sh\n{body}\n"));
            p
        };
        let failing = PgBackRest {
            bin: script("fail", "echo 'ERROR: password=topsecret' >&2; exit 3"),
            ..tool.clone()
        };
        let err = failing.backup().await.expect_err("fails");
        forge_db::test_support::assert_reason(&err, ErrorCode::BackupFailed, "db_backup_failed");
        assert!(!format!("{:?}", err.context()).contains("topsecret"));
        let hanging = PgBackRest {
            bin: script("hang", "exec sleep 30"),
            timeout: Duration::from_millis(300),
            ..tool.clone()
        };
        let err = hanging.backup().await.expect_err("hangs");
        forge_db::test_support::assert_reason(&err, ErrorCode::BackupFailed, "db_backup_timeout");
        // The same newest label before and after: not this run's backup.
        let unchanged = PgBackRest {
            bin: script(
                "same",
                "case \"$*\" in *info*) echo '[{\"backup\":[{\"label\":\"20261108-021500F\"}]}]';; esac",
            ),
            ..tool.clone()
        };
        let err = unchanged.backup().await.expect_err("no new backup");
        forge_db::test_support::assert_reason(
            &err,
            ErrorCode::BackupFailed,
            "db_backup_unexpected",
        );
        let marker = tmp.path().join("backed-up");
        let ok = PgBackRest {
            bin: script(
                "ok",
                &format!(
                    "case \"$*\" in *info*) if [ -e '{m}' ]; then echo '[{{\"backup\":[{{\"label\":\"20261108-021500F\"}},{{\"label\":\"20261109-021500F\"}}]}}]'; else echo '[{{\"backup\":[{{\"label\":\"20261108-021500F\"}}]}}]'; fi;; *backup*) touch '{m}';; esac",
                    m = marker.display()
                ),
            ),
            ..tool
        };
        let done = ok.backup().await.expect("backup");
        assert_eq!(done.label, "20261109-021500F");
    }
}
