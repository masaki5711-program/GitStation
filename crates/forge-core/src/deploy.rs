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
    lower_token(name, 64, true).map_err(|_| "invalid_script_name")?;
    // A shell or an interpreter would turn the next argument (the verb)
    // into code; the target's dispatcher maps a name to one fixed script.
    let stem = name.split('.').next().unwrap_or(name);
    let interpreter = INTERPRETERS.contains(&stem)
        || ["python", "perl", "ruby", "php", "node", "lua"]
            .iter()
            .any(|p| stem.starts_with(p));
    if interpreter {
        return Err("interpreter_script_name");
    }
    Ok(())
}

/// Names a target script may not have (shells and command runners).
const INTERPRETERS: &[&str] = &[
    "sh",
    "bash",
    "dash",
    "zsh",
    "ksh",
    "csh",
    "tcsh",
    "fish",
    "busybox",
    "env",
    "sudo",
    "su",
    "doas",
    "exec",
    "eval",
    "xargs",
    "nohup",
    "timeout",
    "nice",
    "setsid",
    "awk",
    "gawk",
    "sed",
    "tclsh",
    "expect",
    "pwsh",
    "powershell",
    "cmd",
];

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
    if host == "localhost" || host.ends_with(".localhost") {
        return Err("denied_address");
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
    /// The worker side (the system): `deployment.started` (`running`),
    /// `.succeeded`, `.failed`, `.rolled_back`, `.rollback_failed`,
    /// `.blocked`, `.abandoned` (a `failed` the sweep wrote).
    Progress {
        project: ProjectId,
        deployment: i64,
        tier: DeployTier,
        status: DeploymentStatus,
        abandoned: bool,
        error_code: Option<String>,
        failed_target: Option<String>,
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
            Self::Progress {
                abandoned: true, ..
            } => "deployment.abandoned",
            Self::Progress { status, .. } => match status {
                DeploymentStatus::Running => "deployment.started",
                DeploymentStatus::Succeeded => "deployment.succeeded",
                DeploymentStatus::Failed => "deployment.failed",
                DeploymentStatus::RolledBack => "deployment.rolled_back",
                DeploymentStatus::RollbackFailed => "deployment.rollback_failed",
                DeploymentStatus::Blocked => "deployment.blocked",
                DeploymentStatus::PendingApproval | DeploymentStatus::Queued => {
                    "deployment.requested"
                }
            },
        }
    }

    pub fn project(&self) -> ProjectId {
        match self {
            Self::EnvironmentSet { project, .. }
            | Self::TargetSet { project, .. }
            | Self::Requested { project, .. }
            | Self::Approved { project, .. }
            | Self::Progress { project, .. } => *project,
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
            Self::Progress {
                project,
                deployment,
                tier,
                status,
                error_code,
                failed_target,
                ..
            } => serde_json::json!({
                "project_id": project.get(),
                "deployment_id": deployment,
                "environment": tier.as_str(),
                "status": status.as_str(),
                "error_code": error_code,
                "failed_target": failed_target,
            }),
        }
    }
}

/// One network `address/prefix` (IPv4 or IPv6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IpNet {
    addr: std::net::IpAddr,
    prefix: u8,
}

impl IpNet {
    /// Parse `10.0.0.0/8` or `fd00::/8` (a bare address is a /32 or /128).
    pub fn parse(raw: &str) -> Result<Self, &'static str> {
        let (addr, prefix) = match raw.split_once('/') {
            Some((a, p)) => (a, Some(p)),
            None => (raw, None),
        };
        let addr: std::net::IpAddr = addr.trim().parse().map_err(|_| "invalid_network")?;
        let max = if addr.is_ipv4() { 32 } else { 128 };
        let prefix = match prefix {
            None => max,
            Some(p) => p.trim().parse::<u8>().map_err(|_| "invalid_network")?,
        };
        if prefix > max {
            return Err("invalid_network");
        }
        Ok(Self { addr, prefix })
    }

    pub fn contains(&self, ip: std::net::IpAddr) -> bool {
        let ip = canonical(ip);
        match (self.addr, ip) {
            (std::net::IpAddr::V4(net), std::net::IpAddr::V4(a)) => {
                prefix_eq(&net.octets(), &a.octets(), self.prefix)
            }
            (std::net::IpAddr::V6(net), std::net::IpAddr::V6(a)) => {
                prefix_eq(&net.octets(), &a.octets(), self.prefix)
            }
            (std::net::IpAddr::V4(_), std::net::IpAddr::V6(_))
            | (std::net::IpAddr::V6(_), std::net::IpAddr::V4(_)) => false,
        }
    }
}

fn prefix_eq(a: &[u8], b: &[u8], bits: u8) -> bool {
    let full = usize::from(bits / 8);
    let rest = bits % 8;
    if a.get(..full) != b.get(..full) {
        return false;
    }
    if rest == 0 {
        return true;
    }
    let mask = 0xffu8 << (8 - rest);
    match (a.get(full), b.get(full)) {
        (Some(x), Some(y)) => x & mask == y & mask,
        _ => false,
    }
}

/// An IPv4-mapped IPv6 address as its IPv4 address.
fn canonical(ip: std::net::IpAddr) -> std::net::IpAddr {
    match ip {
        std::net::IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => std::net::IpAddr::V4(v4),
            None => std::net::IpAddr::V6(v6),
        },
        std::net::IpAddr::V4(_) => ip,
    }
}

/// Where forge may connect for a deployment (M4 C11 review): never
/// loopback, unspecified, link-local (169.254.169.254 included),
/// multicast or broadcast; never a port forge itself listens on; and,
/// when `allowed` is set (`deploy.allowed_networks`), only inside it.
/// Checked at registration (a literal address) and at every connect after
/// name resolution.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TargetPolicy {
    pub allowed: Vec<IpNet>,
    pub forge_ports: Vec<u16>,
}

impl TargetPolicy {
    /// May forge connect to `ip:port`? `Err` is the log reason.
    pub fn check(&self, ip: std::net::IpAddr, port: u16) -> Result<(), &'static str> {
        let ip = canonical(ip);
        let denied = match ip {
            std::net::IpAddr::V4(v4) => {
                v4.is_loopback()
                    || v4.is_unspecified()
                    || v4.is_link_local()
                    || v4.is_multicast()
                    || v4.is_broadcast()
            }
            std::net::IpAddr::V6(v6) => {
                v6.is_loopback()
                    || v6.is_unspecified()
                    || v6.is_multicast()
                    || (v6.segments()[0] & 0xffc0) == 0xfe80
            }
        };
        if denied {
            return Err("denied_address");
        }
        if self.forge_ports.contains(&port) {
            return Err("forge_port");
        }
        if !self.allowed.is_empty() && !self.allowed.iter().any(|n| n.contains(ip)) {
            return Err("outside_allowed_networks");
        }
        Ok(())
    }

    /// The registration check: a literal address is checked now, a name at
    /// every connect.
    pub fn check_registered(&self, host: &str, port: u16) -> Result<(), &'static str> {
        match host.parse::<std::net::IpAddr>() {
            Ok(ip) => self.check(ip, port),
            Err(_) if self.forge_ports.contains(&port) => Err("forge_port"),
            Err(_) => Ok(()),
        }
    }
}

#[cfg(test)]
#[path = "deploy_tests.rs"]
mod tests;
