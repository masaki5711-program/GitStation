//! `[database]`: the three logins of ADR-0018 and the pool settings.

use std::path::{Path, PathBuf};
use std::time::Duration;

use forge_errors::ForgeError;

use super::{invalid, secret, DatabaseUrl, Secret, TlsMode};

/// The roles a configured member may belong to (ADR-0018 §1).
pub const PRIVILEGED_ROLES: &[&str] = &["forge_owner", "forge_seq", "forge_purge"];

/// One allowed membership `role:member` of `database.privileged_members`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrivilegedMember {
    pub role: String,
    pub member: String,
}

#[derive(Debug)]
pub struct DatabaseConfig {
    /// `forge_app`: `serve` / `worker` (both pools).
    pub(super) url: Option<DatabaseUrl>,
    pub password: Option<Secret>,
    /// `forge_migrator`: `forge db migrate` only (ADR-0018 §2). The password
    /// file is read only by the command that uses it.
    pub(super) migrate_url: Option<DatabaseUrl>,
    pub(super) migrate_password_file: Option<PathBuf>,
    /// `forge_maint`: the approved maintenance commands (ADR-0018 §1).
    pub(super) maint_url: Option<DatabaseUrl>,
    pub(super) maint_password_file: Option<PathBuf>,
    /// Memberships of forge_owner / forge_seq / forge_purge allowed besides
    /// the fixed ones of the bootstrap (`forge doctor` reports any other).
    pub privileged_members: Vec<PrivilegedMember>,
    pub tls_mode: TlsMode,
    /// CA bundle for the internal certificate authority (PEM, absolute).
    pub tls_root_cert: Option<PathBuf>,
    /// `require` / `disable` to a non-loopback host must be allowed here
    /// explicitly (an unverified link to a remote server is never a default).
    pub tls_allow_unverified: bool,
    /// Per process, for each pool.
    pub write_max_connections: u32,
    pub read_max_connections: u32,
    /// Kept open in each pool (at most the smaller maximum).
    pub min_connections: u32,
    pub connect_timeout: Duration,
    pub acquire_timeout: Duration,
    pub statement_timeout: Duration,
    pub lock_timeout: Duration,
    pub idle_in_transaction_timeout: Duration,
    /// The statement timeout while migrating (long, but bounded: §3-2).
    pub migrate_statement_timeout: Duration,
}

impl DatabaseConfig {
    /// The database URL; required by commands that use the database.
    pub fn url(&self) -> Result<&DatabaseUrl, ForgeError> {
        self.url
            .as_ref()
            .ok_or_else(|| invalid("database.url", "missing"))
    }

    /// The migration login; required by `forge db migrate`.
    pub fn migrate_url(&self) -> Result<&DatabaseUrl, ForgeError> {
        self.migrate_url
            .as_ref()
            .ok_or_else(|| invalid("database.migrate_url", "missing"))
    }

    /// The maintenance login; required by the `forge admin` commands.
    pub fn maint_url(&self) -> Result<&DatabaseUrl, ForgeError> {
        self.maint_url
            .as_ref()
            .ok_or_else(|| invalid("database.maint_url", "missing"))
    }

    /// Read `database.migrate_password_file` now (only `forge db migrate`).
    pub fn migrate_password(&self) -> Result<Option<Secret>, ForgeError> {
        read_optional(
            "database.migrate_password_file",
            self.migrate_password_file.as_deref(),
        )
    }

    /// Read `database.maint_password_file` now (only `forge admin`).
    pub fn maint_password(&self) -> Result<Option<Secret>, ForgeError> {
        read_optional(
            "database.maint_password_file",
            self.maint_password_file.as_deref(),
        )
    }

    /// `serve` / `worker` must not be able to read the migration or the
    /// maintenance credentials (ADR-0018: a compromised server process
    /// must not become forge_owner): their password files must not be in
    /// its configuration.
    pub fn reject_privileged_logins(&self) -> Result<(), ForgeError> {
        for (key, set) in [
            (
                "database.migrate_password_file",
                self.migrate_password_file.is_some(),
            ),
            (
                "database.maint_password_file",
                self.maint_password_file.is_some(),
            ),
        ] {
            if set {
                return Err(invalid(key, "not_for_this_command"));
            }
        }
        Ok(())
    }
}

fn read_optional(key: &'static str, path: Option<&Path>) -> Result<Option<Secret>, ForgeError> {
    path.map(|p| secret::read_password(key, p)).transpose()
}

/// An absolute password file path (checked at load; read on use).
pub(super) fn password_path(key: &'static str, value: &str) -> Result<PathBuf, ForgeError> {
    let path = PathBuf::from(value);
    if !path.is_absolute() {
        return Err(invalid(key, "not_absolute"));
    }
    Ok(path)
}

/// `role:member[, role:member…]`: the role one of [`PRIVILEGED_ROLES`], the
/// member a PostgreSQL role name (`[a-z_][a-z0-9_]*`, at most 63 bytes),
/// without duplicates.
pub(super) fn privileged_members(
    key: &'static str,
    list: &str,
) -> Result<Vec<PrivilegedMember>, ForgeError> {
    let mut out: Vec<PrivilegedMember> = Vec::new();
    for entry in list.split(',').map(str::trim) {
        let Some((role, member)) = entry.split_once(':') else {
            return Err(invalid(key, "invalid"));
        };
        let (role, member) = (role.trim(), member.trim());
        // A bootstrap role is never an extra member (forge_audit, say, as a
        // member of forge_purge could rewrite the audit log).
        if !PRIVILEGED_ROLES.contains(&role)
            || !is_role_name(member)
            || member.starts_with("forge_")
        {
            return Err(invalid(key, "invalid"));
        }
        if out.iter().any(|m| m.role == role && m.member == member) {
            return Err(invalid(key, "duplicate"));
        }
        out.push(PrivilegedMember {
            role: role.to_owned(),
            member: member.to_owned(),
        });
    }
    Ok(out)
}

fn is_role_name(name: &str) -> bool {
    name.len() <= 63
        && name.starts_with(|c: char| c.is_ascii_lowercase() || c == '_')
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}
