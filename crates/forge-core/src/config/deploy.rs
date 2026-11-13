//! `[deploy]`: deployments over SSH from `forge worker` (M4 C11, ADR-0031).
//!
//! | key | default | range |
//! |---|---|---|
//! | `deploy.ssh_key_file` | none | an absolute path; the private key ssh reads (`-i`), a secret file (ADR-0013: the forge user's, mode 0600). Without it no deployment runs (`FG-DEPLOY-0008`) |
//! | `deploy.ssh_program` | `ssh` | the ssh client (a name on `PATH` or an absolute path) |
//! | `deploy.connect_timeout_secs` | 10 | 1 ..= 120 — ssh `ConnectTimeout` |
//! | `deploy.script_timeout_secs` | 900 | 10 ..= 7200 — one `deploy` / `rollback` call |
//! | `deploy.health_timeout_secs` | 120 | 5 ..= 1800 — one `health` call |
//!
//! forge never reads the key's content; it checks the file before each
//! deployment (`forge_deploy`) and in `forge doctor`.

use std::path::PathBuf;
use std::time::Duration;

use forge_errors::ForgeError;

use super::{database, invalid, unsigned};

pub const SSH_KEY_FILE_KEY: &str = "deploy.ssh_key_file";
const PROGRAM_KEY: &str = "deploy.ssh_program";
const CONNECT_KEY: &str = "deploy.connect_timeout_secs";
const SCRIPT_KEY: &str = "deploy.script_timeout_secs";
const HEALTH_KEY: &str = "deploy.health_timeout_secs";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeployConfig {
    /// `deploy.ssh_key_file`.
    pub ssh_key_file: Option<PathBuf>,
    /// `deploy.ssh_program`.
    pub ssh_program: PathBuf,
    pub connect_timeout: Duration,
    pub script_timeout: Duration,
    pub health_timeout: Duration,
}

impl Default for DeployConfig {
    fn default() -> Self {
        Self {
            ssh_key_file: None,
            ssh_program: PathBuf::from("ssh"),
            connect_timeout: Duration::from_secs(10),
            script_timeout: Duration::from_secs(900),
            health_timeout: Duration::from_secs(120),
        }
    }
}

impl DeployConfig {
    pub(super) fn load<'a>(
        get: impl Fn(&str) -> Option<&'a str> + Copy,
    ) -> Result<Self, ForgeError> {
        let d = Self::default();
        let ssh_key_file = match get(SSH_KEY_FILE_KEY).map(str::trim) {
            None => None,
            Some("") => return Err(invalid(SSH_KEY_FILE_KEY, "empty")),
            Some(p) => Some(database::password_path(SSH_KEY_FILE_KEY, p)?),
        };
        let ssh_program = match get(PROGRAM_KEY).map(str::trim) {
            None => d.ssh_program,
            Some("") => return Err(invalid(PROGRAM_KEY, "empty")),
            Some(p) if p.chars().any(char::is_control) => {
                return Err(invalid(PROGRAM_KEY, "invalid"))
            }
            Some(p) => PathBuf::from(p),
        };
        let secs = |key: &'static str, default: Duration, min: u32, max: u32| {
            let default = u32::try_from(default.as_secs()).unwrap_or(max);
            match unsigned(get, key, default)? {
                n if n < min => Err(invalid(key, "below_min")),
                n if n > max => Err(invalid(key, "above_max")),
                n => Ok(Duration::from_secs(u64::from(n))),
            }
        };
        Ok(Self {
            ssh_key_file,
            ssh_program,
            connect_timeout: secs(CONNECT_KEY, d.connect_timeout, 1, 120)?,
            script_timeout: secs(SCRIPT_KEY, d.script_timeout, 10, 7200)?,
            health_timeout: secs(HEALTH_KEY, d.health_timeout, 5, 1800)?,
        })
    }
}
