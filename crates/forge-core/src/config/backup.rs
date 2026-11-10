//! `[backup]`: `forge backup` / `forge restore` (M3 O1, ADR-0019 Amendment
//! plan 11 P1, plan 11 §10 Q11, runbook backup-restore).
//!
//! | key | default | rule |
//! |---|---|---|
//! | `backup.dir` | — | absolute path of the local directory that holds the generations (a separate disk; mode 0700, owned by the forge account). Without it `forge backup` refuses (FG-SYS-0008 `not_configured`) |
//! | `backup.key_file` | — | absolute path of the encryption key: one line of 64 hex digits (32 bytes, AES-256-GCM), mode 0600, **outside** `backup.dir` |
//! | `backup.generations` | 7 | 1 ..= 100 complete generations kept (older ones are removed after a success) |
//! | `backup.state_copy_dir` | — | absolute path, outside `backup.dir`, of the authorization state copy that `forge worker` writes every `backup.state_copy_interval_secs` (restore re-applies the narrowings it holds); without it restore removes every administrator flag |
//! | `backup.state_copy_interval_secs` | 300 | 60 ..= 3600 |
//! | `backup.pgbackrest_bin` | `/usr/bin/pgbackrest` | absolute path of the pgBackRest binary (the database step) |
//! | `backup.pgbackrest_stanza` | `forge` | `[a-z0-9_-]{1,64}` |
//! | `backup.db_timeout_secs` | 3600 | 60 ..= 86 400, the database step |
//! | `backup.repo_timeout_secs` | 3600 | 60 ..= 86 400, the repository copy |
//! | `backup.max_age_hours` | 26 | 1 ..= 720, doctor `backup.last_success` (a daily backup plus slack) |
//!
//! Every problem is `FG-SYS-0002` with `config_key` and `reason`. The key
//! is read when it is used ([`BackupConfig::read_key`]); its value never
//! enters an error or a log.

use std::path::{Path, PathBuf};
use std::time::Duration;

use forge_errors::ForgeError;

use super::secret::read_secret_text;
use super::{database, invalid, unsigned};

pub const DIR_KEY: &str = "backup.dir";
pub const KEY_FILE_KEY: &str = "backup.key_file";
pub const GENERATIONS_KEY: &str = "backup.generations";
pub const STATE_COPY_DIR_KEY: &str = "backup.state_copy_dir";
pub const STATE_COPY_INTERVAL_KEY: &str = "backup.state_copy_interval_secs";
pub const PGBACKREST_BIN_KEY: &str = "backup.pgbackrest_bin";
pub const PGBACKREST_STANZA_KEY: &str = "backup.pgbackrest_stanza";
pub const DB_TIMEOUT_KEY: &str = "backup.db_timeout_secs";
pub const REPO_TIMEOUT_KEY: &str = "backup.repo_timeout_secs";
pub const MAX_AGE_KEY: &str = "backup.max_age_hours";

/// The length of the backup key in bytes (AES-256).
pub const KEY_BYTES: usize = 32;

/// The 32-byte key. `Debug` never shows it.
#[derive(Clone)]
pub struct BackupKey([u8; KEY_BYTES]);

impl std::fmt::Debug for BackupKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("BackupKey(..)")
    }
}

impl BackupKey {
    pub fn from_bytes(bytes: [u8; KEY_BYTES]) -> Self {
        Self(bytes)
    }

    pub fn expose(&self) -> &[u8; KEY_BYTES] {
        &self.0
    }

    /// One line of 64 hex digits (surrounding white space ignored).
    pub fn parse_hex(text: &str) -> Option<Self> {
        let text = text.trim();
        if text.len() != KEY_BYTES * 2 || !text.is_ascii() {
            return None;
        }
        let mut out = [0u8; KEY_BYTES];
        for (i, byte) in out.iter_mut().enumerate() {
            let pair = text.get(i * 2..i * 2 + 2)?;
            *byte = u8::from_str_radix(pair, 16).ok()?;
        }
        Some(Self(out))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupConfig {
    pub dir: Option<PathBuf>,
    pub key_file: Option<PathBuf>,
    pub generations: u32,
    pub state_copy_dir: Option<PathBuf>,
    pub state_copy_interval: Duration,
    pub pgbackrest_bin: PathBuf,
    pub pgbackrest_stanza: String,
    pub db_timeout: Duration,
    pub repo_timeout: Duration,
    pub max_age: Duration,
}

impl Default for BackupConfig {
    fn default() -> Self {
        Self {
            dir: None,
            key_file: None,
            generations: Self::DEFAULT_GENERATIONS,
            state_copy_dir: None,
            state_copy_interval: Duration::from_secs(Self::DEFAULT_STATE_COPY_INTERVAL_SECS.into()),
            pgbackrest_bin: PathBuf::from(Self::DEFAULT_PGBACKREST_BIN),
            pgbackrest_stanza: Self::DEFAULT_STANZA.to_owned(),
            db_timeout: Duration::from_secs(Self::DEFAULT_TIMEOUT_SECS.into()),
            repo_timeout: Duration::from_secs(Self::DEFAULT_TIMEOUT_SECS.into()),
            max_age: Duration::from_secs(u64::from(Self::DEFAULT_MAX_AGE_HOURS) * 3600),
        }
    }
}

fn stanza_ok(s: &str) -> bool {
    (1..=64).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

impl BackupConfig {
    pub const DEFAULT_GENERATIONS: u32 = 7;
    pub const MAX_GENERATIONS: u32 = 100;
    pub const DEFAULT_STATE_COPY_INTERVAL_SECS: u32 = 300;
    pub const DEFAULT_PGBACKREST_BIN: &'static str = "/usr/bin/pgbackrest";
    pub const DEFAULT_STANZA: &'static str = "forge";
    pub const DEFAULT_TIMEOUT_SECS: u32 = 3600;
    pub const DEFAULT_MAX_AGE_HOURS: u32 = 26;

    pub(super) fn load<'a>(
        get: impl Fn(&str) -> Option<&'a str> + Copy,
    ) -> Result<Self, ForgeError> {
        let text = |key: &'static str| match get(key).map(str::trim) {
            Some("") => Err(invalid(key, "empty")),
            other => Ok(other),
        };
        let path = |key: &'static str| {
            text(key)?
                .map(|p| database::password_path(key, p))
                .transpose()
        };
        let bounded = |key: &'static str, default: u32, min: u32, max: u32| match unsigned(
            get, key, default,
        )? {
            n if n < min => Err(invalid(key, "below_min")),
            n if n > max => Err(invalid(key, "above_max")),
            n => Ok(n),
        };
        let secs = |n: u32| Duration::from_secs(n.into());
        let stanza = match text(PGBACKREST_STANZA_KEY)? {
            None => Self::DEFAULT_STANZA.to_owned(),
            Some(s) if stanza_ok(s) => s.to_owned(),
            Some(_) => return Err(invalid(PGBACKREST_STANZA_KEY, "invalid")),
        };
        let cfg = Self {
            dir: path(DIR_KEY)?,
            key_file: path(KEY_FILE_KEY)?,
            generations: bounded(
                GENERATIONS_KEY,
                Self::DEFAULT_GENERATIONS,
                1,
                Self::MAX_GENERATIONS,
            )?,
            state_copy_dir: path(STATE_COPY_DIR_KEY)?,
            state_copy_interval: secs(bounded(
                STATE_COPY_INTERVAL_KEY,
                Self::DEFAULT_STATE_COPY_INTERVAL_SECS,
                60,
                3600,
            )?),
            pgbackrest_bin: path(PGBACKREST_BIN_KEY)?
                .unwrap_or_else(|| PathBuf::from(Self::DEFAULT_PGBACKREST_BIN)),
            pgbackrest_stanza: stanza,
            db_timeout: secs(bounded(
                DB_TIMEOUT_KEY,
                Self::DEFAULT_TIMEOUT_SECS,
                60,
                86_400,
            )?),
            repo_timeout: secs(bounded(
                REPO_TIMEOUT_KEY,
                Self::DEFAULT_TIMEOUT_SECS,
                60,
                86_400,
            )?),
            max_age: Duration::from_secs(
                u64::from(bounded(MAX_AGE_KEY, Self::DEFAULT_MAX_AGE_HOURS, 1, 720)?) * 3600,
            ),
        };
        // A stolen backup must not carry its own key or its own state copy
        // (checked on the configured paths; `forge backup` checks the
        // resolved ones again).
        if let Some(dir) = &cfg.dir {
            if cfg.key_file.as_deref().is_some_and(|k| k.starts_with(dir)) {
                return Err(invalid(KEY_FILE_KEY, "inside_backup_dir"));
            }
            if cfg
                .state_copy_dir
                .as_deref()
                .is_some_and(|s| s.starts_with(dir) || dir.starts_with(s))
            {
                return Err(invalid(STATE_COPY_DIR_KEY, "inside_backup_dir"));
            }
        }
        Ok(cfg)
    }

    /// Read the key now: an absolute regular file of at most 256 bytes,
    /// readable only by its owner (Unix), holding one line of 64 hex
    /// digits. `None` without `backup.key_file`. Problems are `FG-SYS-0002`
    /// (`config_key=backup.key_file`); the content never enters an error.
    pub fn read_key(&self) -> Option<Result<BackupKey, ForgeError>> {
        let path = self.key_file.as_deref()?;
        Some(read_key_file(path))
    }
}

/// [`BackupConfig::read_key`] for one path.
pub fn read_key_file(path: &Path) -> Result<BackupKey, ForgeError> {
    let text = read_secret_text(KEY_FILE_KEY, path, 256)?;
    BackupKey::parse_hex(&text)
        .ok_or_else(|| invalid(KEY_FILE_KEY, "invalid").with_context("path", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load(pairs: &[(&str, &str)]) -> Result<BackupConfig, ForgeError> {
        BackupConfig::load(|k| pairs.iter().find(|(key, _)| *key == k).map(|(_, v)| *v))
    }

    fn reason(e: &ForgeError) -> String {
        e.context()
            .iter()
            .find(|(k, _)| *k == "reason")
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    }

    #[test]
    fn defaults_and_bounds() {
        let cfg = load(&[]).expect("defaults");
        assert_eq!(cfg.generations, 7);
        assert_eq!(cfg.dir, None);
        assert_eq!(cfg.pgbackrest_stanza, "forge");
        assert_eq!(cfg.max_age, Duration::from_secs(26 * 3600));
        let err = load(&[(GENERATIONS_KEY, "0")]).expect_err("zero");
        assert_eq!(reason(&err), "below_min");
        let err = load(&[(GENERATIONS_KEY, "101")]).expect_err("too many");
        assert_eq!(reason(&err), "above_max");
        let err = load(&[(DIR_KEY, "relative/dir")]).expect_err("relative");
        assert_eq!(reason(&err), "not_absolute");
        let err = load(&[(PGBACKREST_STANZA_KEY, "Bad Stanza")]).expect_err("stanza");
        assert_eq!(reason(&err), "invalid");
    }

    /// `unix` as an absolute path of the host (Windows: on drive C:), so
    /// the containment rule is what is tested, not `not_absolute`.
    fn abs(unix: &str) -> String {
        if cfg!(windows) {
            format!("C:{}", unix.replace('/', "\\"))
        } else {
            unix.to_owned()
        }
    }

    #[test]
    fn the_key_and_the_state_copy_stay_outside_the_backup_dir() {
        let (dir, key_in, state_in) = (
            abs("/srv/backup"),
            abs("/srv/backup/key"),
            abs("/srv/backup/state"),
        );
        let err = load(&[(DIR_KEY, &dir), (KEY_FILE_KEY, &key_in)]).expect_err("key inside");
        assert_eq!(reason(&err), "inside_backup_dir");
        let err =
            load(&[(DIR_KEY, &dir), (STATE_COPY_DIR_KEY, &state_in)]).expect_err("state inside");
        assert_eq!(reason(&err), "inside_backup_dir");
        let (key_out, state_out) = (abs("/etc/forge/backup.key"), abs("/srv/forge-state"));
        load(&[
            (DIR_KEY, &dir),
            (KEY_FILE_KEY, &key_out),
            (STATE_COPY_DIR_KEY, &state_out),
        ])
        .expect("outside");
    }

    #[test]
    fn the_key_is_64_hex_digits_and_never_shown() {
        let hex = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
        let key = BackupKey::parse_hex(&format!("{hex}\n")).expect("key");
        assert_eq!(key.expose()[1], 0x11);
        assert_eq!(format!("{key:?}"), "BackupKey(..)");
        assert!(BackupKey::parse_hex(&hex[..62]).is_none());
        assert!(BackupKey::parse_hex(&format!("{}zz", &hex[..62])).is_none());
    }
}
