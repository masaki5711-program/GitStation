//! `forge admin …` — operator commands that inspect, never mutate.

use std::path::Path;

use forge_core::RepoId;
use forge_errors::{ErrorCode, ForgeError};
use forge_git::StorageRoot;

/// `forge admin repo-path <repo_id>`: where a repository lives on disk
/// (ADR-0009 — the path is derived from the id only) and whether it exists.
///
/// Only a genuinely missing directory prints `missing`; a rejected path
/// (symlink, not a directory — possible tampering) or an I/O failure is
/// returned as an error, so the CLI boundary logs it with its code.
pub fn repo_path(storage_root: &Path, repo_id: i64) -> Result<String, ForgeError> {
    let root = StorageRoot::open(storage_root)?;
    let id = RepoId::new(repo_id);
    let path = root.path_for(id);
    let state = match root.existing(id) {
        Ok(_) => "exists",
        Err(e) if e.code() == ErrorCode::StorageMissing => "missing",
        Err(e) => return Err(e),
    };
    Ok(format!("{}\t{state}\n", path.display()))
}

/// `forge admin act-as claimed-mac --value <v> [--kid <k>]` (M3 S2,
/// ADR-0025 §5.2): the audit's `act_as_claimed` for a `Forge-Act-As` value,
/// with the key id it was computed with. The operator's host is the trust
/// anchor (as ADR-0019 §8); nothing is written. No key file is
/// `FG-SYS-0002 config_key=auth.act_as_hmac_keys_file reason=missing`.
pub fn act_as_claimed_mac(
    cfg: &forge_core::config::Config,
    value: &str,
    kid: Option<i16>,
) -> Result<String, ForgeError> {
    let keys = cfg.auth.act_as_keys()?.ok_or_else(|| {
        ForgeError::new(forge_errors::ErrorCode::ConfigInvalid)
            .with_context("config_key", forge_core::act_as::KEYS_CONFIG_KEY)
            .with_context("reason", "missing")
    })?;
    let (kid, mac) = keys.claimed(value, kid)?;
    Ok(format!(
        "act_as_key_id  {kid}
act_as_claimed {mac}
"
    ))
}

/// The host key file of the SSH transport (M4 C13): `git.ssh.host_key_file`,
/// or `FG-SYS-0002 reason=missing`.
fn ssh_host_key_path(cfg: &forge_core::config::Config) -> Result<&Path, ForgeError> {
    cfg.git_ssh.host_key_file.as_deref().ok_or_else(|| {
        ForgeError::new(ErrorCode::ConfigInvalid)
            .with_context("config_key", forge_core::config::git_ssh::HOST_KEY_FILE_KEY)
            .with_context("reason", "missing")
    })
}

/// What `ssh-host-key generate` / `show` print: the fingerprint users
/// compare on their first connection and the `known_hosts` line.
fn ssh_host_key_report(
    key: &forge_web::ssh::HostKey,
    clone_host: &str,
) -> Result<String, ForgeError> {
    let public = key.public_openssh()?;
    // `host` for port 22, `[host]:port` otherwise (OpenSSH's known_hosts).
    let (host, port) = match clone_host.strip_prefix('[') {
        Some(rest) => match rest.split_once(']') {
            Some((v6, tail)) => (v6, tail.strip_prefix(':')),
            None => (clone_host, None),
        },
        None => match clone_host.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (clone_host, None),
        },
    };
    let known_host = match port {
        None | Some("22") => host.to_owned(),
        Some(port) => format!("[{host}]:{port}"),
    };
    Ok(format!(
        "fingerprint  {}\npublic_key   {public}\nknown_hosts  {known_host} {public}\n",
        key.fingerprint()
    ))
}

/// `forge admin ssh-host-key generate` (M4 C13, ADR-0032 §2, runbook
/// ssh-transport): make the SSH host key at `git.ssh.host_key_file` once.
/// An existing file is never replaced (`FG-SYS-0002 reason=target_exists`).
pub fn ssh_host_key_generate(cfg: &forge_core::config::Config) -> Result<String, ForgeError> {
    let path = ssh_host_key_path(cfg)?;
    let key = forge_web::ssh::generate_host_key(path)?;
    tracing::info!(
        path = %path.display(),
        fingerprint = key.fingerprint(),
        "forge.admin.ssh_host_key_generated"
    );
    ssh_host_key_report(&key, &clone_host_of(cfg))
}

/// `forge admin ssh-host-key show`: the fingerprint and public half of the
/// configured host key (reads only).
pub fn ssh_host_key_show(cfg: &forge_core::config::Config) -> Result<String, ForgeError> {
    let key = forge_web::ssh::load_host_key(ssh_host_key_path(cfg)?)?;
    ssh_host_key_report(&key, &clone_host_of(cfg))
}

/// `git.ssh.clone_host`, else `<git.ssh.bind's host>:<port>` (the report's
/// `known_hosts` line; `forge serve` derives it from `web.base_url`).
fn clone_host_of(cfg: &forge_core::config::Config) -> String {
    cfg.git_ssh
        .clone_host
        .clone() // clone: the config keeps its value
        .unwrap_or_else(|| cfg.git_ssh.bind.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// M4 C13: generate once, show the same fingerprint, never overwrite.
    #[test]
    fn the_ssh_host_key_is_generated_once_and_shown() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("ssh_host_ed25519_key");
        let path_text = path.to_string_lossy().into_owned();
        let cfg = forge_core::config::load(
            None,
            &forge_core::config::Env::from_pairs([
                ("FORGE_GIT_SSH_HOST_KEY_FILE", path_text.as_str()),
                ("FORGE_GIT_SSH_CLONE_HOST", "git.example.test:2222"),
            ]),
        )
        .expect("config");
        let made = ssh_host_key_generate(&cfg).expect("generate");
        assert!(made.starts_with("fingerprint  SHA256:"), "{made}");
        assert!(
            made.contains("known_hosts  [git.example.test]:2222 ssh-ed25519 "),
            "{made}"
        );
        assert_eq!(ssh_host_key_show(&cfg).expect("show"), made);
        let again = ssh_host_key_generate(&cfg).expect_err("exists");
        assert_eq!(again.code(), ErrorCode::ConfigInvalid);
        let none = forge_core::config::load(None, &forge_core::config::Env::from_pairs([]))
            .expect("config");
        assert_eq!(
            ssh_host_key_show(&none).expect_err("missing").code(),
            ErrorCode::ConfigInvalid
        );
    }

    #[test]
    fn repo_path_prints_id_derived_path_and_state() {
        let dir = tempfile::tempdir().expect("tempdir");
        let out = repo_path(dir.path(), 42).expect("repo-path");
        assert!(out.contains("42.git"), "{out}");
        assert!(out.trim_end().ends_with("missing"), "{out}");
    }

    #[test]
    fn repo_path_reports_exists_for_a_repository_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = StorageRoot::open(dir.path()).expect("root");
        std::fs::create_dir_all(root.path_for(RepoId::new(42))).expect("dir");
        let out = repo_path(dir.path(), 42).expect("repo-path");
        assert!(out.trim_end().ends_with("exists"), "{out}");
    }

    #[test]
    fn repo_path_does_not_report_rejected_storage_as_missing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = StorageRoot::open(dir.path()).expect("root");
        let path = root.path_for(RepoId::new(42));
        std::fs::create_dir_all(path.parent().expect("shard")).expect("shard");
        std::fs::write(&path, b"not a repository").expect("write");
        let err = repo_path(dir.path(), 42).expect_err("rejected, not missing");
        assert_eq!(err.code(), ErrorCode::StoragePathRejected);
    }

    #[test]
    fn repo_path_rejects_a_root_that_is_not_a_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("file");
        std::fs::write(&file, b"x").expect("write");
        let err = repo_path(&file, 1).expect_err("bad root");
        assert_eq!(err.code(), ErrorCode::StoragePathRejected);
    }
}
