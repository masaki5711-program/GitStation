//! The SSH server's host key (M4 C13, ADR-0032 §2): one Ed25519 key in
//! OpenSSH format at `git.ssh.host_key_file`. Generated once by `forge
//! admin ssh-host-key generate` (an existing file is never overwritten:
//! the key users pinned in `known_hosts` must not change by accident);
//! `forge serve` only reads it. Problems are `FG-SYS-0002
//! config_key=git.ssh.host_key_file` with `reason` = `missing`,
//! `unreadable`, `invalid`, `insecure_permissions` (Unix: readable by
//! group or others), `target_exists` or `write_failed`.

use std::io::Write as _;
use std::path::Path;

use forge_errors::{ErrorCode, ForgeError};
use russh::keys::ssh_key::private::{Ed25519Keypair, KeypairData};
use russh::keys::ssh_key::LineEnding;
use russh::keys::{Algorithm, PrivateKey};

use forge_core::config::git_ssh::HOST_KEY_FILE_KEY;

fn problem(reason: &'static str, path: &Path) -> ForgeError {
    ForgeError::new(ErrorCode::ConfigInvalid)
        .with_context("config_key", HOST_KEY_FILE_KEY)
        .with_context("reason", reason)
        .with_context("path", path.display())
}

/// A loaded host key. No `Debug` of the private half.
#[derive(Clone)]
pub struct HostKey {
    key: PrivateKey,
    fingerprint: String,
}

impl std::fmt::Debug for HostKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostKey")
            .field("fingerprint", &self.fingerprint)
            .finish_non_exhaustive()
    }
}

impl HostKey {
    /// `SHA256:…` (what `ssh-keygen -l` and the client's first-connection
    /// prompt show).
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    /// The public half, `ssh-ed25519 AAAA…` (for `known_hosts`).
    pub fn public_openssh(&self) -> Result<String, ForgeError> {
        self.key.public_key().to_openssh().map_err(|e| {
            ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "host_key_encode")
                .with_source(std::io::Error::other(e.to_string()))
        })
    }

    pub(crate) fn private(&self) -> &PrivateKey {
        &self.key
    }

    fn from_key(key: PrivateKey) -> Self {
        let fingerprint = super::fingerprint_of(&key);
        Self { key, fingerprint }
    }
}

/// Read the host key. It must be an unencrypted Ed25519 key; on Unix the
/// file must not be readable by group or others.
pub fn load_host_key(path: &Path) -> Result<HostKey, ForgeError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(problem("missing", path).with_source(e))
        }
        Err(e) => return Err(problem("unreadable", path).with_source(e)),
    };
    check_permissions(path)?;
    let key = PrivateKey::from_openssh(text.as_bytes())
        .map_err(|e| problem("invalid", path).with_context("detail", e.to_string()))?;
    if key.is_encrypted() {
        return Err(problem("invalid", path).with_context("detail", "encrypted"));
    }
    if key.algorithm() != Algorithm::Ed25519 {
        return Err(problem("invalid", path).with_context("detail", "not_ed25519"));
    }
    Ok(HostKey::from_key(key))
}

#[cfg(unix)]
fn check_permissions(path: &Path) -> Result<(), ForgeError> {
    use std::os::unix::fs::PermissionsExt as _;
    let mode = std::fs::metadata(path)
        .map_err(|e| problem("unreadable", path).with_source(e))?
        .permissions()
        .mode();
    if mode & 0o077 != 0 {
        return Err(problem("insecure_permissions", path)
            .with_context("mode", format!("{:o}", mode & 0o777)));
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_permissions(_path: &Path) -> Result<(), ForgeError> {
    // Windows (development only): the file's ACL is the operator's.
    Ok(())
}

/// Make a new Ed25519 host key at `path` (mode 0600 on Unix). An existing
/// file is never replaced (`target_exists`).
pub fn generate_host_key(path: &Path) -> Result<HostKey, ForgeError> {
    let mut seed = [0u8; 32];
    getrandom::getrandom(&mut seed).map_err(|e| {
        ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "rng_unavailable")
            .with_context("source", format!("{e:?}"))
    })?;
    let keypair = Ed25519Keypair::from_seed(&seed);
    seed.fill(0);
    let key =
        PrivateKey::new(KeypairData::Ed25519(keypair), "gitstation-ssh-host-key").map_err(|e| {
            ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "host_key_build")
                .with_source(std::io::Error::other(e.to_string()))
        })?;
    let text = key.to_openssh(LineEnding::LF).map_err(|e| {
        ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "host_key_encode")
            .with_source(std::io::Error::other(e.to_string()))
    })?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::AlreadyExists {
            problem("target_exists", path)
        } else {
            problem("write_failed", path).with_source(e)
        }
    })?;
    file.write_all(text.as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|e| problem("write_failed", path).with_source(e))?;
    Ok(HostKey::from_key(key))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn reason(err: &ForgeError) -> String {
        err.context()
            .iter()
            .find(|(k, _)| *k == "reason")
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    }

    #[test]
    fn a_generated_key_loads_back_and_is_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ssh_host_ed25519_key");
        let made = generate_host_key(&path).unwrap();
        assert!(made.fingerprint().starts_with("SHA256:"));
        assert!(made.public_openssh().unwrap().starts_with("ssh-ed25519 "));
        let loaded = load_host_key(&path).unwrap();
        assert_eq!(loaded.fingerprint(), made.fingerprint());
        let again = generate_host_key(&path).unwrap_err();
        assert_eq!(reason(&again), "target_exists");
        assert_eq!(
            load_host_key(&path).unwrap().fingerprint(),
            made.fingerprint()
        );
        assert!(!format!("{made:?}").contains("PRIVATE"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert_eq!(
                reason(&load_host_key(&path).unwrap_err()),
                "insecure_permissions"
            );
        }
    }

    #[test]
    fn a_missing_or_foreign_file_names_its_reason() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("absent");
        let err = load_host_key(&path).unwrap_err();
        assert_eq!(err.code(), ErrorCode::ConfigInvalid);
        assert_eq!(reason(&err), "missing");
        let junk = dir.path().join("junk");
        std::fs::write(&junk, "not a key").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&junk, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        assert_eq!(reason(&load_host_key(&junk).unwrap_err()), "invalid");
    }
}
