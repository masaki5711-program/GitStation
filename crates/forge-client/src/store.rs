//! Where the token lives (plan 01 §3.11, ADR-0019): the OS credential store
//! first (Windows Credential Manager, Linux Secret Service), a 0600 file when
//! that is unavailable — with a visible warning — and never the screen or a log.
//!
//! [`Token`] has no `Display` and a redacted `Debug`; the only way to read it
//! is [`Token::expose`], used where the header is built or git is answered.

use crate::error::{ClientError, Result};
use forge_errors::ErrorCode;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

/// A personal access token. Never printed, never logged.
#[derive(Clone, PartialEq, Eq)]
pub struct Token(String);

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Token(<redacted>)")
    }
}

impl Token {
    /// Accept what a pasted token can legitimately be: non-empty, printable
    /// ASCII without whitespace (it ends up in a header). The value itself is
    /// never part of an error.
    pub fn parse(raw: &str) -> Result<Self> {
        let t = raw.trim();
        let ok = !t.is_empty() && t.len() <= 512 && t.bytes().all(|b| b.is_ascii_graphic());
        if !ok {
            return Err(ClientError::local(
                ErrorCode::CliUsageInvalid,
                "reason=token_malformed (expected one token, printable ASCII, no spaces)",
            ));
        }
        Ok(Self(t.to_string()))
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

/// One place a token can be kept. Errors are reasons without secrets.
pub trait SecretBackend {
    fn name(&self) -> &'static str;
    fn get(&self, host: &str) -> std::result::Result<Option<Token>, String>;
    fn set(&self, host: &str, token: &Token) -> std::result::Result<(), String>;
    /// `true` when something was removed.
    fn delete(&self, host: &str) -> std::result::Result<bool, String>;
}

// ── OS credential store ────────────────────────────────────────────────

pub struct KeyringBackend;

fn init_os_store() -> std::result::Result<(), String> {
    use std::sync::OnceLock;
    static INIT: OnceLock<std::result::Result<(), String>> = OnceLock::new();
    INIT.get_or_init(|| {
        #[cfg(target_os = "linux")]
        {
            let store = dbus_secret_service_keyring_store::Store::new()
                .map_err(|e| format!("secret_service_unavailable: {}", short(&e)))?;
            keyring_core::set_default_store(store);
            Ok(())
        }
        #[cfg(windows)]
        {
            let store = windows_native_keyring_store::Store::new()
                .map_err(|e| format!("credential_manager_unavailable: {}", short(&e)))?;
            keyring_core::set_default_store(store);
            Ok(())
        }
        #[cfg(not(any(target_os = "linux", windows)))]
        {
            Err("no_os_credential_store_on_this_platform".to_string())
        }
    })
    .clone()
}

fn short(e: &dyn std::fmt::Display) -> String {
    // Platform errors can carry long D-Bus texts; keep one bounded line.
    e.to_string().chars().take(160).collect()
}

fn entry(host: &str) -> std::result::Result<keyring_core::Entry, String> {
    init_os_store()?;
    keyring_core::Entry::new("gstn", host).map_err(|e| short(&e))
}

impl SecretBackend for KeyringBackend {
    fn name(&self) -> &'static str {
        "keyring"
    }

    fn get(&self, host: &str) -> std::result::Result<Option<Token>, String> {
        match entry(host)?.get_password() {
            Ok(p) => Token::parse(&p)
                .map(Some)
                .map_err(|_| "stored_value_malformed".to_string()),
            Err(keyring_core::Error::NoEntry) => Ok(None),
            Err(e) => Err(short(&e)),
        }
    }

    fn set(&self, host: &str, token: &Token) -> std::result::Result<(), String> {
        entry(host)?
            .set_password(token.expose())
            .map_err(|e| short(&e))
    }

    fn delete(&self, host: &str) -> std::result::Result<bool, String> {
        match entry(host)?.delete_credential() {
            Ok(()) => Ok(true),
            Err(keyring_core::Error::NoEntry) => Ok(false),
            Err(e) => Err(short(&e)),
        }
    }
}

// ── 0600 file ──────────────────────────────────────────────────────────

pub struct FileBackend {
    path: PathBuf,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CredFile {
    #[serde(default)]
    hosts: BTreeMap<String, String>,
}

impl FileBackend {
    pub fn new(dir: &Path) -> Self {
        Self {
            path: dir.join("credentials.toml"),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn read(&self) -> std::result::Result<CredFile, String> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            match std::fs::metadata(&self.path) {
                Ok(m) if m.permissions().mode() & 0o077 != 0 => {
                    return Err(format!(
                        "insecure_permissions mode={:o} (must be 0600)",
                        m.permissions().mode() & 0o777
                    ))
                }
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(CredFile::default())
                }
                Err(e) => return Err(format!("unreadable: {}", e.kind())),
            }
        }
        match std::fs::read_to_string(&self.path) {
            Ok(t) => toml::from_str(&t).map_err(|_| "file_malformed".to_string()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(CredFile::default()),
            Err(e) => Err(format!("unreadable: {}", e.kind())),
        }
    }

    fn write(&self, f: &CredFile) -> std::result::Result<(), String> {
        let dir = self.path.parent().ok_or("no_parent_directory")?;
        create_private_dir(dir)?;
        let text = toml::to_string(f).map_err(|_| "serialize_failed".to_string())?;
        // Write a private temp file next to the target, then rename over it:
        // the secret is never visible with a wider mode, even briefly.
        let tmp = dir.join(format!(".credentials.{}.tmp", std::process::id()));
        let mut o = std::fs::OpenOptions::new();
        o.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            o.mode(0o600);
        }
        let mut file = o
            .open(&tmp)
            .map_err(|e| format!("create_failed: {}", e.kind()))?;
        let done = file
            .write_all(text.as_bytes())
            .and_then(|()| file.sync_all())
            .map_err(|e| format!("write_failed: {}", e.kind()));
        drop(file);
        if let Err(e) = done {
            return cleanup(&tmp, e);
        }
        if let Err(e) = std::fs::rename(&tmp, &self.path) {
            return cleanup(&tmp, format!("rename_failed: {}", e.kind()));
        }
        Ok(())
    }
}

fn cleanup(tmp: &Path, reason: String) -> std::result::Result<(), String> {
    match std::fs::remove_file(tmp) {
        Ok(()) => Err(reason),
        Err(e) => Err(format!("{reason}; temp_file_left: {}", e.kind())),
    }
}

fn create_private_dir(dir: &Path) -> std::result::Result<(), String> {
    if dir.exists() {
        return Ok(());
    }
    let mut b = std::fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        b.mode(0o700);
    }
    b.create(dir)
        .map_err(|e| format!("mkdir_failed: {}", e.kind()))
}

impl SecretBackend for FileBackend {
    fn name(&self) -> &'static str {
        "file"
    }

    fn get(&self, host: &str) -> std::result::Result<Option<Token>, String> {
        match self.read()?.hosts.get(host) {
            Some(v) => Token::parse(v)
                .map(Some)
                .map_err(|_| "stored_value_malformed".to_string()),
            None => Ok(None),
        }
    }

    fn set(&self, host: &str, token: &Token) -> std::result::Result<(), String> {
        let mut f = self.read().or_else(|e| {
            // A wrong-mode or unreadable file is not silently overwritten.
            if e.starts_with("insecure_permissions") || e.starts_with("unreadable") {
                Err(e)
            } else {
                Ok(CredFile::default())
            }
        })?;
        f.hosts.insert(host.to_string(), token.expose().to_string());
        self.write(&f)
    }

    fn delete(&self, host: &str) -> std::result::Result<bool, String> {
        let mut f = self.read()?;
        let removed = f.hosts.remove(host).is_some();
        if removed {
            self.write(&f)?;
        }
        Ok(removed)
    }
}

// ── the composite ──────────────────────────────────────────────────────

/// Where a loaded token came from (shown by `auth status`; never the value).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Env,
    Keyring,
    File,
}

#[derive(Debug)]
pub struct Saved {
    pub backend: &'static str,
    /// Shown to the user on stderr.
    pub warnings: Vec<String>,
}

#[derive(Debug)]
pub struct Loaded {
    pub token: Token,
    pub source: Source,
    pub warnings: Vec<String>,
}

pub struct TokenStore {
    primary: Box<dyn SecretBackend>,
    fallback: Option<Box<dyn SecretBackend>>,
}

impl TokenStore {
    pub fn new(primary: Box<dyn SecretBackend>, fallback: Option<Box<dyn SecretBackend>>) -> Self {
        Self { primary, fallback }
    }

    /// `auto` (default): OS store, file fallback; `keyring`: OS store only;
    /// `file`: the 0600 file only (CI, headless hosts, tests).
    pub fn open(dir: &Path, mode: &str) -> Result<Self> {
        match mode {
            "" | "auto" => Ok(Self::new(
                Box::new(KeyringBackend),
                Some(Box::new(FileBackend::new(dir))),
            )),
            "keyring" => Ok(Self::new(Box::new(KeyringBackend), None)),
            "file" => Ok(Self::new(Box::new(FileBackend::new(dir)), None)),
            _ => Err(ClientError::local(
                ErrorCode::CliConfigInvalid,
                "config_key=GSTN_CREDENTIAL_STORE reason=invalid (auto|keyring|file)",
            )),
        }
    }

    fn failure(&self, backend: &str, reason: &str) -> ClientError {
        ClientError::local(
            ErrorCode::CliCredentialStoreFailed,
            format!("backend={backend} reason={reason}"),
        )
    }

    pub fn save(&self, host: &str, token: &Token) -> Result<Saved> {
        let mut warnings = Vec::new();
        match self.primary.set(host, token) {
            Ok(()) => {
                if let Some(fb) = &self.fallback {
                    // A stale copy in the file must not outlive a good store.
                    if let Err(e) = fb.delete(host) {
                        warnings.push(format!(
                            "could not remove an older token from the {} store: {e}",
                            fb.name()
                        ));
                    }
                }
                Ok(Saved {
                    backend: self.primary.name(),
                    warnings,
                })
            }
            Err(primary_err) => {
                let Some(fb) = &self.fallback else {
                    return Err(self.failure(self.primary.name(), &primary_err));
                };
                fb.set(host, token).map_err(|e| {
                    self.failure(
                        fb.name(),
                        &format!("{e}; {} also failed: {primary_err}", self.primary.name()),
                    )
                })?;
                warnings.push(format!(
                    "the OS credential store is not available ({primary_err}); the token was stored in a file readable only by you (mode 0600)"
                ));
                Ok(Saved {
                    backend: fb.name(),
                    warnings,
                })
            }
        }
    }

    pub fn load(&self, host: &str, env_token: Option<&str>) -> Result<Option<Loaded>> {
        let mut warnings = Vec::new();
        if let Some(raw) = env_token.filter(|t| !t.trim().is_empty()) {
            return Ok(Some(Loaded {
                token: Token::parse(raw).map_err(|_| {
                    ClientError::local(
                        ErrorCode::CliConfigInvalid,
                        "config_key=GSTN_TOKEN reason=invalid",
                    )
                })?,
                source: Source::Env,
                warnings,
            }));
        }
        let source_of = |name: &str| {
            if name == "keyring" {
                Source::Keyring
            } else {
                Source::File
            }
        };
        let mut backends: Vec<&dyn SecretBackend> = vec![self.primary.as_ref()];
        if let Some(fb) = &self.fallback {
            backends.push(fb.as_ref());
        }
        for b in backends {
            match b.get(host) {
                Ok(Some(token)) => {
                    return Ok(Some(Loaded {
                        token,
                        source: source_of(b.name()),
                        warnings,
                    }))
                }
                Ok(None) => {}
                Err(e) => warnings.push(format!(
                    "the {} credential store could not be read: {e}",
                    b.name()
                )),
            }
        }
        if warnings.is_empty() {
            return Ok(None);
        }
        // Nothing found and a store failed: say so instead of "not logged in" alone.
        Err(ClientError::local(
            ErrorCode::CliNotLoggedIn,
            warnings.join("; "),
        ))
    }

    /// Remove the token everywhere it may be. A store that cannot be reached
    /// is a warning when another store was cleaned (a headless host never had
    /// a keyring entry); it is an error when no store could be cleaned.
    pub fn delete(&self, host: &str) -> Result<Deleted> {
        let mut removed = false;
        let mut clean = 0;
        let mut failures = Vec::new();
        let mut backends: Vec<&dyn SecretBackend> = vec![self.primary.as_ref()];
        if let Some(fb) = &self.fallback {
            backends.push(fb.as_ref());
        }
        let total = backends.len();
        for b in backends {
            match b.delete(host) {
                Ok(r) => {
                    removed |= r;
                    clean += 1;
                }
                Err(e) => failures.push(format!("{}: {e}", b.name())),
            }
        }
        if clean == 0 && total > 0 {
            return Err(self.failure("all", &failures.join("; ")));
        }
        Ok(Deleted {
            removed,
            warnings: failures
                .into_iter()
                .map(|f| format!("a credential store could not be cleaned ({f})"))
                .collect(),
        })
    }
}

#[derive(Debug)]
pub struct Deleted {
    pub removed: bool,
    pub warnings: Vec<String>,
}
