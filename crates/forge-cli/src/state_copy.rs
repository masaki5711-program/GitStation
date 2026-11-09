//! The authorization state copy outside the backup (M3 O1, ADR-0019
//! Amendment plan 11 P1, step 2; forge-db `restore::state`). `forge worker`
//! writes one every `backup.state_copy_interval_secs` (and `forge backup`
//! one after each backup) into `backup.state_copy_dir`:
//!
//! ```text
//! <backup.state_copy_dir>/state-<YYYYMMDDTHHMMSSZ>-<micros>.fgbk   (0600, AES-256-GCM, kind 2)
//! ```
//!
//! Encrypted with the backup key, so a replaced or changed file is refused
//! (and a restore without a usable copy removes every administrator flag,
//! never widens). The newest [`KEEP`] files are kept. `forge restore finish`
//! uses the newest copy taken at or after the restored generation's
//! database step ([`newest_usable`]).

use std::fs;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

use forge_core::config::{BackupConfig, BackupKey};
use forge_db::restore::state::{self, AuthzState};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};

use crate::backup::{create_private_file, crypt};

/// State copies kept.
pub const KEEP: usize = 48;
const PREFIX: &str = "state-";
const SUFFIX: &str = ".fgbk";
/// The authenticated context of a state copy (crypt.rs).
const STATE_CONTEXT: &[u8] = b"state";
/// The largest state copy read back (bytes).
const MAX_BYTES: u64 = 256 << 20;

fn failed(reason: &'static str, operation: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::BackupFailed)
        .with_context("reason", reason)
        .with_context("operation", operation)
}

/// Read the state now and write it as a new copy; older copies beyond
/// [`KEEP`] are removed. Returns the new file.
pub async fn write(db: &Db, dir: &Path, key: &BackupKey) -> Result<PathBuf, ForgeError> {
    let st = state::read(db).await?;
    let clock = forge_db::restore::clock(db).await?;
    let name = format!(
        "{PREFIX}{}-{:020}{SUFFIX}",
        clock.generation, st.taken_at_micros
    );
    let json = serde_json::to_vec(&st).map_err(|_| failed("storage_io", "serialize_state_copy"))?;
    let tmp = dir.join(format!(".{name}.tmp"));
    let path = dir.join(&name);
    let io = |op: &'static str| move |e: std::io::Error| failed("storage_io", op).with_source(e);
    let file = create_private_file(&tmp).map_err(io("create_state_copy"))?;
    let mut sealed = crypt::SealWriter::new(file, key, crypt::Kind::StateCopy, STATE_CONTEXT)
        .map_err(io("open_state_copy"))?;
    sealed.write_all(&json).map_err(io("write_state_copy"))?;
    let file = sealed.finish().map_err(io("finish_state_copy"))?;
    file.sync_all().map_err(io("sync_state_copy"))?;
    fs::rename(&tmp, &path).map_err(io("rename_state_copy"))?;
    prune(dir).map_err(io("prune_state_copies"))?;
    tracing::info!(
        target: "forge_cli",
        event = "forge.backup.state_copy_written",
        file = %name,
        taken_at = %st.taken_at,
        users = st.users.len(),
        "authorization state copy written"
    );
    Ok(path)
}

/// The copies in `dir`, newest first (by name: the time is in it).
fn copies(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut out: Vec<PathBuf> = fs::read_dir(dir)?
        .filter_map(Result::ok)
        .filter(|e| {
            e.file_name()
                .to_str()
                .is_some_and(|n| n.starts_with(PREFIX) && n.ends_with(SUFFIX))
        })
        .map(|e| e.path())
        .collect();
    out.sort();
    out.reverse();
    Ok(out)
}

fn prune(dir: &Path) -> std::io::Result<()> {
    for old in copies(dir)?.into_iter().skip(KEEP) {
        fs::remove_file(old)?;
    }
    Ok(())
}

/// Read and check one copy.
pub fn read(path: &Path, key: &BackupKey) -> std::io::Result<AuthzState> {
    let file = fs::File::open(path)?;
    if file.metadata()?.len() > MAX_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "too_large",
        ));
    }
    let mut reader = crypt::OpenReader::new(file, key, crypt::Kind::StateCopy, STATE_CONTEXT)?;
    let mut json = Vec::new();
    reader.read_to_end(&mut json)?;
    let st: AuthzState = serde_json::from_slice(&json)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "json"))?;
    if st.format != state::FORMAT {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "format",
        ));
    }
    Ok(st)
}

/// Why no copy was used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unusable {
    NotConfigured,
    /// No copy, or none taken at or after the database step.
    NoneNewEnough,
    /// Copies exist but none could be read (`skipped` names them).
    Unreadable,
}

impl Unusable {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::NotConfigured => "not_configured",
            Self::NoneNewEnough => "none_new_enough",
            Self::Unreadable => "unreadable",
        }
    }
}

/// The newest readable copy taken at or after `not_before_micros` (the
/// restored generation's database clock). When the newest copy does not
/// open it is `Unreadable` (no copy: every administrator flag is removed)
/// unless `allow_older`, which goes on to the next older one.
pub fn newest_usable(
    dir: Option<&Path>,
    key: &BackupKey,
    not_before_micros: i64,
    allow_older: bool,
) -> Result<(PathBuf, AuthzState), Unusable> {
    let Some(dir) = dir else {
        return Err(Unusable::NotConfigured);
    };
    let Ok(all) = copies(dir) else {
        return Err(Unusable::Unreadable);
    };
    let mut unreadable = 0usize;
    for path in all {
        match read(&path, key) {
            Ok(st) if st.taken_at_micros >= not_before_micros => return Ok((path, st)),
            // Sorted newest first: everything after is older still.
            Ok(_) => break,
            Err(e) => {
                unreadable += 1;
                tracing::warn!(
                    target: "forge_cli",
                    event = "forge.restore.state_copy_skipped",
                    error_code = %ErrorCode::RestoreVerificationFailed,
                    reason = "archive_corrupt",
                    file = %path.display(),
                    error = %e,
                    "a state copy could not be read"
                );
                // The newest copy may hold the latest narrowings: an older
                // one would silently lose them (`--allow-older-copy` accepts
                // that after the operator checked why it does not open).
                if !allow_older {
                    return Err(Unusable::Unreadable);
                }
            }
        }
    }
    if unreadable > 0 {
        Err(Unusable::Unreadable)
    } else {
        Err(Unusable::NoneNewEnough)
    }
}

/// The worker task: a copy every interval while `forge worker` runs. A
/// failed write is logged once per attempt (FG-SYS-0008) and retried at
/// the next interval; it never stops the worker.
pub async fn run_task(db: Db, cfg: BackupConfig, mut shutdown: tokio::sync::watch::Receiver<bool>) {
    let mut every = tokio::time::interval(cfg.state_copy_interval);
    every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = every.tick() => {}
            _ = shutdown.changed() => return,
        }
        let request_id = forge_core::RequestId::generate();
        let attempt = async {
            let prepared = crate::backup::prepare(&cfg)?;
            let Some(dir) = prepared.state_copy_dir else {
                return Ok(None);
            };
            write(&db, &dir, &prepared.key).await.map(Some)
        };
        match tokio::time::timeout(Duration::from_secs(120), attempt).await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => forge_core::telemetry::log_failure(
                &e.with_context("operation", "state_copy"),
                &request_id,
            ),
            Err(_) => forge_core::telemetry::log_failure(
                &failed("storage_io", "state_copy_timeout"),
                &request_id,
            ),
        }
    }
}
