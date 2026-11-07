//! The push mirror (M3 O2, ADR-0024 Amendment O2): one `git push` of a
//! repository's branches and tags to its GitLab copy.
//!
//! ```text
//! git -c core.hooksPath=<empty> -c credential.helper= -c protocol.allow=never
//!     -c protocol.https.allow=always [-c http.sslCAInfo=<ca>]
//!     push --porcelain --prune --no-verify --end-of-options <url>
//!     +refs/heads/*:refs/heads/* +refs/tags/*:refs/tags/*
//! ```
//!
//! - **Explicit refspecs only** (never `--mirror` or `refs/*`): the hidden
//!   merge request refs (`refs/merge-requests/*`) never leave GitStation
//!   (ADR-0024 Risks). `+` and `--prune`: GitStation is the canonical copy,
//!   so a force-push and a deleted branch reach GitLab too.
//! - **The token is never in the URL, the argument vector or a log**: the
//!   `Authorization` header goes in `GIT_CONFIG_COUNT` / `GIT_CONFIG_KEY_0`
//!   / `GIT_CONFIG_VALUE_0` (`http.extraHeader`) in the child's environment
//!   only; `credential.helper` is emptied so no stored credential is used,
//!   and stderr is redacted (`redact::stderr_head`) before it reaches an
//!   error.
//! - **Transport**: `protocol.allow=never` with only `https` (or `http`
//!   when the policy allowed it) let through, so a stored URL can never
//!   reach `file://`, `ext::` or `ssh`.
//! - A timeout (`mirror.push_timeout_secs`) and the process tree is killed
//!   at it (git spawns `git-remote-https` and `pack-objects`).
//!
//! Every failure is `FG-INT-0013 PushMirrorFailed` with `reason`
//! (`rejected` with `ref`, `auth`, `unreachable`, `timeout`, `git` with
//! `exit_code`) and the redacted `stderr`; the caller logs it.

use std::ffi::OsString;
use std::path::Path;
use std::time::Duration;

use forge_core::config::Secret;
use forge_core::ids::RepoId;
use forge_core::mirror::MirrorUrl;
use forge_errors::{ErrorCode, ForgeError};

use crate::command::Git;
use crate::storage::StorageRoot;

/// The refspecs a mirror pushes (module docs).
pub const MIRROR_REFSPECS: [&str; 2] = ["+refs/heads/*:refs/heads/*", "+refs/tags/*:refs/tags/*"];
/// stdout read at most (the porcelain report: one line per ref).
const PORCELAIN_BYTES: usize = 4 * 1024 * 1024;

/// Where a mirror pushes to.
pub enum MirrorDestination<'a> {
    /// The GitLab copy: a URL that passed the policy, the `Authorization`
    /// header built from the credential file, the CA bundle if configured.
    Remote {
        url: &'a MirrorUrl,
        authorization: &'a Secret,
        ca_file: Option<&'a Path>,
    },
    /// Tests only: a local bare repository (plan 11 §7: the nextest
    /// destination). Never built from a stored setting.
    #[cfg(any(test, feature = "test-support"))]
    Local(&'a Path),
}

impl std::fmt::Debug for MirrorDestination<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Remote { url, ca_file, .. } => f
                .debug_struct("Remote")
                .field("origin", &url.origin())
                .field("ca_file", ca_file)
                .finish_non_exhaustive(),
            #[cfg(any(test, feature = "test-support"))]
            Self::Local(p) => f.debug_tuple("Local").field(p).finish(),
        }
    }
}

/// What one push changed on the destination.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MirrorPushReport {
    /// Refs created or moved (`*`, ` `, `+` in the porcelain report).
    pub updated: u32,
    /// Refs deleted (`-`).
    pub deleted: u32,
    /// Refs already up to date (`=`).
    pub up_to_date: u32,
}

fn failed(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::PushMirrorFailed).with_context("reason", reason)
}

/// The reason of a failed push read from git's stderr (lower-cased).
fn classify(stderr: &str) -> &'static str {
    let s = stderr.to_ascii_lowercase();
    if s.contains("authentication failed")
        || s.contains("http basic: access denied")
        || s.contains("returned error: 401")
        || s.contains("returned error: 403")
        || s.contains("could not read username")
        || s.contains("permission denied")
    {
        "auth"
    } else if s.contains("could not resolve host")
        || s.contains("failed to connect")
        || s.contains("connection refused")
        || s.contains("ssl")
        || s.contains("certificate")
        || s.contains("returned error: 5")
        || s.contains("timed out")
        || s.contains("does not appear to be a git repository")
    {
        "unreachable"
    } else {
        "git"
    }
}

/// Count the porcelain report; the first rejected ref (`!`), if any.
fn read_porcelain(stdout: &[u8]) -> (MirrorPushReport, Option<String>) {
    let mut report = MirrorPushReport::default();
    let mut rejected = None;
    for line in String::from_utf8_lossy(stdout).lines() {
        let mut parts = line.splitn(3, '\t');
        let (Some(flag), Some(refs)) = (parts.next(), parts.next()) else {
            continue;
        };
        match flag {
            "*" | " " | "+" => report.updated += 1,
            "-" => report.deleted += 1,
            "=" => report.up_to_date += 1,
            // `<src>:<dst>`; the destination ref names what GitLab refused.
            "!" if rejected.is_none() => {
                let dst = refs.rsplit(':').next().unwrap_or(refs);
                rejected = Some(dst.chars().take(200).collect());
            }
            _ => {}
        }
    }
    (report, rejected)
}

impl Git {
    /// Push `id`'s branches and tags to `dest` (module docs).
    pub async fn push_mirror(
        &self,
        storage: &StorageRoot,
        id: RepoId,
        dest: &MirrorDestination<'_>,
        timeout: Duration,
    ) -> Result<MirrorPushReport, ForgeError> {
        let repo = storage.existing(id)?;
        let mut cmd = self
            .with_empty_hooks(self.command("push"))?
            .config_override("credential.helper", "")
            .config_override("protocol.allow", "never");
        let target: OsString = match dest {
            MirrorDestination::Remote {
                url,
                authorization,
                ca_file,
            } => {
                let scheme = if url.is_https() {
                    "protocol.https.allow"
                } else {
                    "protocol.http.allow"
                };
                cmd = cmd.config_override(scheme, "always");
                if let Some(ca) = ca_file {
                    cmd = cmd.config_override("http.sslCAInfo", ca.as_os_str());
                }
                // The header lives in the child's environment only.
                let mut value = OsString::from("Authorization: ");
                value.push(authorization.expose());
                cmd = cmd
                    .env("GIT_CONFIG_COUNT", OsString::from("1"))
                    .env("GIT_CONFIG_KEY_0", OsString::from("http.extraHeader"))
                    .env("GIT_CONFIG_VALUE_0", value);
                OsString::from(url.as_str())
            }
            #[cfg(any(test, feature = "test-support"))]
            MirrorDestination::Local(path) => {
                cmd = cmd.config_override("protocol.file.allow", "always");
                path.as_os_str().to_os_string()
            }
        };
        let captured = cmd
            .arg("--porcelain")
            .arg("--prune")
            .arg("--no-verify")
            .end_operand(target)
            .end_operand(OsString::from(MIRROR_REFSPECS[0]))
            .end_operand(OsString::from(MIRROR_REFSPECS[1]))
            .repo(&repo)
            .supervise_tree()
            .timeout(timeout)
            .output_accepting(PORCELAIN_BYTES, &[0, 1, 128])
            .await
            .map_err(|e| {
                let reason = if e.code() == ErrorCode::GitTimeout {
                    "timeout"
                } else {
                    "git"
                };
                failed(reason)
                    .with_context("repo_id", id.get())
                    .with_context("cause", e.code().id())
            })?;
        let (report, rejected) = read_porcelain(&captured.output.stdout);
        // Already the redacted head (`Captured::stderr`).
        let stderr = captured.stderr;
        match captured.exit_code {
            Some(0) => Ok(report),
            Some(code) => {
                let mut e = match rejected {
                    Some(r) => failed("rejected").with_context("ref", r),
                    None => failed(classify(&stderr)),
                };
                e = e
                    .with_context("repo_id", id.get())
                    .with_context("exit_code", code);
                if !stderr.is_empty() {
                    e = e.with_context("stderr", stderr);
                }
                Err(e)
            }
            None => Err(failed("git")
                .with_context("repo_id", id.get())
                .with_context("detail", "output_truncated")),
        }
    }
}

#[cfg(test)]
#[path = "mirror_tests.rs"]
mod tests;
