//! Deployments (M4 C11, plan 14 §4.1, ADR-0031): the environment tiers,
//! the deployment's state machine, the checks of what a target may hold
//! (host, user, script name, pinned host key) and the audit details of a
//! person's change. Pure: no I/O, no clock (the database's `now()` decides
//! freezes and approval expiry).
//!
//! Every value that reaches the remote command line (`<script> <verb>
//! <sha>`) is restricted to `[A-Za-z0-9._-]`: ssh joins its remote
//! arguments into one string the target's shell reads, so nothing that
//! shell interprets may ever get there (ADR-0031 decision 3).

use base64::Engine as _;
use sha2::{Digest, Sha256};

use crate::ProjectId;

/// An environment's tier: one environment per tier per project.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum DeployTier {
    Dev,
    Stg,
    Prod,
}

impl DeployTier {
    pub const ALL: [DeployTier; 3] = [DeployTier::Dev, DeployTier::Stg, DeployTier::Prod];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Dev => "dev",
            Self::Stg => "stg",
            Self::Prod => "prod",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.as_str() == raw)
    }

    /// Production: approval always required, never rolled back or deployed
    /// by automation (the table's CHECKs say the same).
    pub fn is_prod(self) -> bool {
        self == Self::Prod
    }
}

/// A deployment's state (`deployments.status`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeploymentStatus {
    /// Waiting for a `DeployApprove`.
    PendingApproval,
    /// Approved (or no approval needed): the worker takes it.
    Queued,
    /// The worker is running it.
    Running,
    Succeeded,
    /// Failed; nothing rolled back (prod, or no target changed yet).
    Failed,
    /// Failed and every changed target is back at the previous version.
    RolledBack,
    /// Failed and the rollback failed too (`FG-DEPLOY-0006`).
    RollbackFailed,
    /// Not run: a freeze window or an expired approval at execution time.
    Blocked,
}

impl DeploymentStatus {
    pub const ALL: [DeploymentStatus; 8] = [
        Self::PendingApproval,
        Self::Queued,
        Self::Running,
        Self::Succeeded,
        Self::Failed,
        Self::RolledBack,
        Self::RollbackFailed,
        Self::Blocked,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::PendingApproval => "pending_approval",
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::RolledBack => "rolled_back",
            Self::RollbackFailed => "rollback_failed",
            Self::Blocked => "blocked",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.as_str() == raw)
    }

    /// No further change.
    pub fn is_final(self) -> bool {
        match self {
            Self::PendingApproval | Self::Queued | Self::Running => false,
            Self::Succeeded
            | Self::Failed
            | Self::RolledBack
            | Self::RollbackFailed
            | Self::Blocked => true,
        }
    }

    /// The transitions the state machine allows (the table's guard
    /// trigger holds the same).
    pub fn may_become(self, next: Self) -> bool {
        match (self, next) {
            (Self::PendingApproval, Self::Queued | Self::Blocked) => true,
            (Self::Queued, Self::Running | Self::Blocked) => true,
            (
                Self::Running,
                Self::Succeeded | Self::Failed | Self::RolledBack | Self::RollbackFailed,
            ) => true,
            (
                Self::PendingApproval
                | Self::Queued
                | Self::Running
                | Self::Succeeded
                | Self::Failed
                | Self::RolledBack
                | Self::RollbackFailed
                | Self::Blocked,
                _,
            ) => false,
        }
    }
}

/// What the remote script is asked to do (its first argument).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteVerb {
    Deploy,
    Health,
    Rollback,
}

impl RemoteVerb {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Deploy => "deploy",
            Self::Health => "health",
            Self::Rollback => "rollback",
        }
    }
}

/// The longest freeze window list of one environment.
pub const MAX_FREEZE_WINDOWS: usize = 50;
/// The most targets of one environment.
pub const MAX_TARGETS_PER_ENVIRONMENT: i64 = 20;
/// `approval_ttl_minutes` bounds (default 60).
pub const DEFAULT_APPROVAL_TTL_MINUTES: i32 = 60;
pub const MAX_APPROVAL_TTL_MINUTES: i32 = 24 * 60;

/// A target's name: `[a-z0-9][a-z0-9-]{0,62}`. `Err` is the log reason.
pub fn check_target_name(name: &str) -> Result<(), &'static str> {
    lower_token(name, 63, false).map_err(|_| "invalid_target_name")
}

/// The per-target script's registered name: `[a-z0-9][a-z0-9._-]{0,63}`
/// (no `/`: the target resolves it, ADR-0031 decision 3).
pub fn check_script_name(name: &str) -> Result<(), &'static str> {
    lower_token(name, 64, true).map_err(|_| "invalid_script_name")
}

/// The login user on the target: `[a-z_][a-z0-9_-]{0,31}`.
pub fn check_user(user: &str) -> Result<(), &'static str> {
    let bytes = user.as_bytes();
    let first_ok = bytes
        .first()
        .is_some_and(|b| b.is_ascii_lowercase() || *b == b'_');
    let rest_ok = bytes
        .iter()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_' || *b == b'-');
    if first_ok && rest_ok && bytes.len() <= 32 {
        Ok(())
    } else {
        Err("invalid_user")
    }
}

/// The target's host: a DNS name (labels `[a-z0-9-]`, not starting or
/// ending with `-`, at most 253 bytes) or an IPv4 address. Never starts
/// with `-` (it could not be read as an option even after `--`, but the
/// rule is kept simple).
pub fn check_host(host: &str) -> Result<(), &'static str> {
    if host.is_empty() || host.len() > 253 {
        return Err("invalid_host");
    }
    let label_ok = |l: &str| {
        !l.is_empty()
            && l.len() <= 63
            && !l.starts_with('-')
            && !l.ends_with('-')
            && l.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    };
    if host.split('.').all(label_ok) {
        Ok(())
    } else {
        Err("invalid_host")
    }
}

/// The SSH port: 1..=65535.
pub fn check_port(port: i32) -> Result<u16, &'static str> {
    u16::try_from(port)
        .ok()
        .filter(|p| *p != 0)
        .ok_or("invalid_port")
}

/// A lowercase token: first `[a-z0-9]`, then `[a-z0-9-]` (and `._` when
/// `dots`), at most `max` bytes.
fn lower_token(raw: &str, max: usize, dots: bool) -> Result<(), ()> {
    let b = raw.as_bytes();
    let first = b
        .first()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    let rest = b.iter().all(|c| {
        c.is_ascii_lowercase()
            || c.is_ascii_digit()
            || *c == b'-'
            || (dots && (*c == b'.' || *c == b'_'))
    });
    if first && rest && b.len() <= max {
        Ok(())
    } else {
        Err(())
    }
}

/// The host key types a target may pin (OpenSSH names).
pub const HOST_KEY_TYPES: &[&str] = &[
    "ssh-ed25519",
    "ecdsa-sha2-nistp256",
    "ecdsa-sha2-nistp384",
    "ecdsa-sha2-nistp521",
    "rsa-sha2-512",
    "rsa-sha2-256",
];

/// A target's pinned host public key (ADR-0031 decision 4): its type and
/// the base64 of the key blob, exactly as in a `known_hosts` line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostKey {
    key_type: String,
    blob_b64: String,
    fingerprint: String,
}

impl HostKey {
    /// Parse a key type and its base64 blob. The blob must decode, be at
    /// most 2 KiB, and name the same algorithm inside (the first string of
    /// an SSH public key blob). `rsa-sha2-*` pins an `ssh-rsa` blob.
    pub fn parse(key_type: &str, blob_b64: &str) -> Result<Self, &'static str> {
        if !HOST_KEY_TYPES.contains(&key_type) {
            return Err("unsupported_host_key_type");
        }
        if blob_b64.is_empty() || blob_b64.len() > 2800 {
            return Err("invalid_host_key");
        }
        let blob = base64::engine::general_purpose::STANDARD
            .decode(blob_b64)
            .map_err(|_| "invalid_host_key")?;
        let inner = blob_algorithm(&blob).ok_or("invalid_host_key")?;
        let expected = if key_type.starts_with("rsa-sha2-") {
            "ssh-rsa"
        } else {
            key_type
        };
        if inner != expected {
            return Err("host_key_type_mismatch");
        }
        let digest = Sha256::digest(&blob);
        Ok(Self {
            key_type: key_type.to_owned(),
            blob_b64: blob_b64.to_owned(),
            fingerprint: format!(
                "SHA256:{}",
                base64::engine::general_purpose::STANDARD_NO_PAD.encode(digest)
            ),
        })
    }

    pub fn key_type(&self) -> &str {
        &self.key_type
    }

    pub fn blob_b64(&self) -> &str {
        &self.blob_b64
    }

    /// The type as a `known_hosts` line names it (`ssh-rsa` for the
    /// `rsa-sha2-*` pins).
    fn line_type(&self) -> &str {
        if self.key_type.starts_with("rsa-sha2-") {
            "ssh-rsa"
        } else {
            &self.key_type
        }
    }

    /// `SHA256:<unpadded base64>` as `ssh-keygen -l` prints it.
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    /// The single `known_hosts` line that pins this key for `host:port`
    /// (`[host]:port` unless the port is 22).
    pub fn known_hosts_line(&self, host: &str, port: u16) -> String {
        let name = if port == 22 {
            host.to_owned()
        } else {
            format!("[{host}]:{port}")
        };
        format!("{name} {} {}\n", self.line_type(), self.blob_b64)
    }
}

/// The algorithm name at the start of an SSH public key blob (a
/// big-endian u32 length, then that many bytes).
fn blob_algorithm(blob: &[u8]) -> Option<&str> {
    let len_bytes: [u8; 4] = blob.get(..4)?.try_into().ok()?;
    let len = usize::try_from(u32::from_be_bytes(len_bytes)).ok()?;
    let name = blob.get(4..4usize.checked_add(len)?)?;
    std::str::from_utf8(name).ok()
}

/// A person's (or the system's) change of deployments, as the audit row's
/// details. Never a key, a token or script output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeployChange {
    /// `deploy_environment.created` / `.updated`.
    EnvironmentSet {
        project: ProjectId,
        tier: DeployTier,
        created: bool,
        requires_approval: bool,
        auto_rollback: bool,
        approval_ttl_minutes: i32,
        freeze_windows: usize,
        auto_deploy_ref: Option<String>,
    },
    /// `deploy_target.created` / `.updated`.
    TargetSet {
        project: ProjectId,
        tier: DeployTier,
        name: String,
        created: bool,
        host: String,
        port: u16,
        user: String,
        script: String,
        fingerprint: String,
        enabled: bool,
    },
    /// `deployment.requested`.
    Requested {
        project: ProjectId,
        deployment: i64,
        tier: DeployTier,
        sha: String,
        status: DeploymentStatus,
    },
    /// `deployment.approved`.
    Approved {
        project: ProjectId,
        deployment: i64,
        tier: DeployTier,
        sha: String,
    },
}

impl DeployChange {
    pub fn action(&self) -> &'static str {
        match self {
            Self::EnvironmentSet { created: true, .. } => "deploy_environment.created",
            Self::EnvironmentSet { created: false, .. } => "deploy_environment.updated",
            Self::TargetSet { created: true, .. } => "deploy_target.created",
            Self::TargetSet { created: false, .. } => "deploy_target.updated",
            Self::Requested { .. } => "deployment.requested",
            Self::Approved { .. } => "deployment.approved",
        }
    }

    pub fn project(&self) -> ProjectId {
        match self {
            Self::EnvironmentSet { project, .. }
            | Self::TargetSet { project, .. }
            | Self::Requested { project, .. }
            | Self::Approved { project, .. } => *project,
        }
    }

    pub fn to_value(&self) -> serde_json::Value {
        match self {
            Self::EnvironmentSet {
                project,
                tier,
                requires_approval,
                auto_rollback,
                approval_ttl_minutes,
                freeze_windows,
                auto_deploy_ref,
                ..
            } => serde_json::json!({
                "project_id": project.get(),
                "environment": tier.as_str(),
                "requires_approval": requires_approval,
                "auto_rollback": auto_rollback,
                "approval_ttl_minutes": approval_ttl_minutes,
                "freeze_windows": freeze_windows,
                "auto_deploy_ref": auto_deploy_ref,
            }),
            Self::TargetSet {
                project,
                tier,
                name,
                host,
                port,
                user,
                script,
                fingerprint,
                enabled,
                ..
            } => serde_json::json!({
                "project_id": project.get(),
                "environment": tier.as_str(),
                "target": name,
                "host": host,
                "port": port,
                "user": user,
                "script": script,
                "host_key_fingerprint": fingerprint,
                "enabled": enabled,
            }),
            Self::Requested {
                project,
                deployment,
                tier,
                sha,
                status,
            } => serde_json::json!({
                "project_id": project.get(),
                "deployment_id": deployment,
                "environment": tier.as_str(),
                "sha": sha,
                "status": status.as_str(),
            }),
            Self::Approved {
                project,
                deployment,
                tier,
                sha,
            } => serde_json::json!({
                "project_id": project.get(),
                "deployment_id": deployment,
                "environment": tier.as_str(),
                "sha": sha,
            }),
        }
    }
}

#[cfg(test)]
#[path = "deploy_tests.rs"]
mod tests;
