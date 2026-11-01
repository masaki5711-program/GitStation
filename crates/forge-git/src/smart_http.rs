//! Smart HTTP: building `git upload-pack` / `git receive-pack` streams
//! (ADR-0007) with the fixed `-c` sets, and the `info/refs` advertisement
//! prefix.
//!
//! argv (nothing in it comes from the request):
//!
//! ```text
//! git -c <fixed set> -c core.hooksPath=<home>/empty-hooks \
//!     upload-pack|receive-pack [--strict] --stateless-rpc [--advertise-refs] -- <repo path>
//! ```
//!
//! The repository path is only ever [`StorageRoot::existing`] for a `RepoId`
//! (ADR-0009). The only request value that reaches git is the `Git-Protocol`
//! header, sanitised into `GIT_PROTOCOL` ([`GitCommand::git_protocol`]).

use std::num::NonZeroU64;
use std::path::Path;

use forge_core::RepoId;
use forge_errors::{ErrorCode, ForgeError};

use crate::command::{Git, GitCommand};
use crate::pkt_line;
use crate::storage::StorageRoot;

/// The two Smart HTTP services.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Service {
    UploadPack,
    ReceivePack,
}

impl Service {
    /// The `service` query value / advertisement name.
    pub fn name(self) -> &'static str {
        match self {
            Self::UploadPack => "git-upload-pack",
            Self::ReceivePack => "git-receive-pack",
        }
    }

    /// Parse the `service` query value (exact match only).
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "git-upload-pack" => Some(Self::UploadPack),
            "git-receive-pack" => Some(Self::ReceivePack),
            _ => None,
        }
    }

    fn subcommand(self) -> &'static str {
        match self {
            Self::UploadPack => "upload-pack",
            Self::ReceivePack => "receive-pack",
        }
    }
}

/// `GET info/refs` (advertise) or `POST git-<service>` (rpc).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Advertise,
    Rpc,
}

/// The wire protocol version a client asked for in `Git-Protocol`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolVersion {
    V0,
    V1,
    V2,
}

impl ProtocolVersion {
    /// From the `Git-Protocol` header (`key[=value]` pairs separated by
    /// `:`); the highest `version=` wins, anything unknown is v0 (as git does).
    pub fn from_git_protocol(header: Option<&str>) -> Self {
        let mut best = Self::V0;
        // Only a value git actually receives counts (see `git_protocol`).
        let header = header.and_then(crate::command::sanitize_git_protocol);
        for pair in header.unwrap_or_default().split(':') {
            match pair {
                "version=2" => best = Self::V2,
                "version=1" if best == Self::V0 => best = Self::V1,
                _ => {}
            }
        }
        best
    }
}

/// The bytes forge writes before git's output on `GET info/refs`:
/// `# service=<name>\n` + flush for v0 / v1; nothing for upload-pack v2
/// (git's v2 advertisement starts with `version 2`). receive-pack has no v2
/// and answers a v2 request in v0, so it always gets the prefix.
pub fn advertisement_prefix(service: Service, version: ProtocolVersion) -> Vec<u8> {
    if service == Service::UploadPack && version == ProtocolVersion::V2 {
        return Vec::new();
    }
    let line = format!("# service={}\n", service.name());
    // A fixed line far below the pkt-line maximum.
    let mut out = format!("{:04x}{line}", line.len() + 4).into_bytes();
    out.extend_from_slice(pkt_line::FLUSH);
    out
}

/// Fixed `-c` settings of upload-pack (fetch / clone).
pub const UPLOAD_PACK_CONFIG: &[(&str, &str)] = &[
    // Partial clone (`--filter`).
    ("uploadpack.allowFilter", "true"),
    // Keep-alive packets while pack-objects is still counting, so idle
    // timeouts (forge's and proxies') do not cut a slow but healthy fetch.
    ("uploadpack.keepAlive", "5"),
    // v0 / v1: refuse wants that are not advertised. v2 ignores it — see
    // ADR-0007 amendment (M1 phase 9).
    ("uploadpack.allowAnySHA1InWant", "false"),
];

/// Fixed `-c` settings of receive-pack (push); `receive.maxInputSize` is
/// added from the caller's push size limit.
pub const RECEIVE_PACK_CONFIG: &[(&str, &str)] = &[
    ("receive.fsckObjects", "true"),
    ("receive.keepAlive", "5"),
    ("receive.advertisePushOptions", "false"),
];

impl Git {
    /// `git upload-pack` for repository `id`, ready for
    /// [`GitCommand::spawn_stream`]. `git_protocol`: the raw `Git-Protocol`
    /// header (sanitised; invalid values are dropped).
    pub fn upload_pack(
        &self,
        storage: &StorageRoot,
        id: RepoId,
        phase: Phase,
        git_protocol: Option<&str>,
    ) -> Result<GitCommand, ForgeError> {
        let repo = storage.existing(id)?;
        let mut cmd = self.command(Service::UploadPack.subcommand());
        for (key, value) in UPLOAD_PACK_CONFIG {
            cmd = cmd.config_override(key, value);
        }
        cmd = self.with_empty_hooks(cmd)?.arg("--strict");
        Ok(finish_command(cmd, &repo, phase, git_protocol))
    }

    /// `git receive-pack` for repository `id`. `max_input_size`: the push
    /// size limit in bytes (`receive.maxInputSize`, enforced by index-pack).
    /// Non-zero by type: git reads `0` as "unlimited", which would silently
    /// switch the cap off.
    pub fn receive_pack(
        &self,
        storage: &StorageRoot,
        id: RepoId,
        phase: Phase,
        git_protocol: Option<&str>,
        max_input_size: NonZeroU64,
    ) -> Result<GitCommand, ForgeError> {
        let repo = storage.existing(id)?;
        let mut cmd = self.command(Service::ReceivePack.subcommand());
        for (key, value) in RECEIVE_PACK_CONFIG {
            cmd = cmd.config_override(key, value);
        }
        cmd = cmd.config_override("receive.maxInputSize", max_input_size.to_string());
        cmd = self.with_empty_hooks(cmd)?;
        Ok(finish_command(cmd, &repo, phase, git_protocol))
    }

    /// `-c core.hooksPath=<home>/empty-hooks`, after checking the directory
    /// is a real, empty directory (created if missing). No hook runs in
    /// Smart HTTP in phase 9 (ADR-0008 amendment): a hook found here would be
    /// a policy that nothing reviewed.
    fn with_empty_hooks(&self, cmd: GitCommand) -> Result<GitCommand, ForgeError> {
        let dir = self.config().empty_hooks_dir();
        ensure_empty_dir(&dir)?;
        let Some(value) = dir.to_str().filter(|_| dir.is_absolute()) else {
            return Err(hooks_dir_error("not_absolute_utf8", &dir));
        };
        Ok(cmd.config_override("core.hooksPath", value))
    }
}

fn finish_command(
    cmd: GitCommand,
    repo: &Path,
    phase: Phase,
    git_protocol: Option<&str>,
) -> GitCommand {
    let mut cmd = cmd.arg("--stateless-rpc");
    if phase == Phase::Advertise {
        cmd = cmd.arg("--advertise-refs");
    }
    if let Some(value) = git_protocol {
        cmd = cmd.git_protocol(value);
    }
    cmd.repo(repo).path(repo)
}

fn hooks_dir_error(reason: &'static str, dir: &Path) -> ForgeError {
    ForgeError::new(ErrorCode::GitUnavailable)
        .with_context("reason", "hooks_dir")
        .with_context("detail", reason)
        .with_context("path", dir.display())
}

/// A real directory (no symlink), not writable by group or others (Unix),
/// and empty. Created with mode 0700 when missing.
fn ensure_empty_dir(dir: &Path) -> Result<(), ForgeError> {
    let meta = match std::fs::symlink_metadata(dir) {
        Ok(meta) if meta.is_dir() => meta,
        Ok(_) => return Err(hooks_dir_error("not_a_directory", dir)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            create_private_dir(dir)
                .map_err(|e| hooks_dir_error("create_failed", dir).with_source(e))?;
            std::fs::symlink_metadata(dir)
                .map_err(|e| hooks_dir_error("unreadable", dir).with_source(e))?
        }
        Err(e) => return Err(hooks_dir_error("unreadable", dir).with_source(e)),
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = meta.permissions().mode();
        if mode & 0o022 != 0 {
            return Err(hooks_dir_error("insecure_permissions", dir)
                .with_context("mode", format!("{:o}", mode & 0o7777)));
        }
    }
    #[cfg(not(unix))]
    let _ = meta;
    let mut entries =
        std::fs::read_dir(dir).map_err(|e| hooks_dir_error("unreadable", dir).with_source(e))?;
    match entries.next() {
        None => Ok(()),
        Some(Ok(_)) => Err(hooks_dir_error("not_empty", dir)),
        Some(Err(e)) => Err(hooks_dir_error("unreadable", dir).with_source(e)),
    }
}

fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent)?;
    }
    #[cfg(unix)]
    let builder = {
        use std::os::unix::fs::DirBuilderExt as _;
        let mut b = std::fs::DirBuilder::new();
        b.mode(0o700);
        b
    };
    #[cfg(not(unix))]
    let builder = std::fs::DirBuilder::new();
    match builder.create(dir) {
        // A concurrent first use created it; the checks that follow apply.
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        other => other,
    }
}

#[cfg(test)]
#[path = "smart_http_tests.rs"]
mod tests;
