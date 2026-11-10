//! The git calls of a job (M4 C4): a fresh repository in the job's work
//! directory, a fetch of exactly `refs/pipelines/<pipeline_id>/head` from
//! the job's fetch route with the job's token, the check that the fetched
//! commit is the pinned one, the read of `.forge-ci.yml` at it, and the
//! checkout. Every call is an argv ([`Cmd`]):
//!
//! - nothing from the claim reaches argv but the pinned SHA (40 / 64
//!   lower-case hex, [`ObjectId`]) and the pipeline id (a number); the
//!   server URL comes from the runner's configuration;
//! - the job's token never reaches argv: it is the value of
//!   `http.extraHeader` in git's environment configuration
//!   (`GIT_CONFIG_COUNT` / `GIT_CONFIG_KEY_0` / `GIT_CONFIG_VALUE_0`), set
//!   on the fetch only;
//! - no system or global configuration (`GIT_CONFIG_NOSYSTEM`,
//!   `GIT_CONFIG_GLOBAL` = an empty file the runner owns), no credential
//!   helper, no prompt, no hooks, only http(s) transports, no redirects
//!   (`http.followRedirects=false`: the token never follows one).

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use forge_core::ObjectId;

use crate::api::JobToken;
use crate::process::Cmd;

/// The file read at the pinned commit.
pub const DEFINITION_FILE: &str = ".forge-ci.yml";

/// Builds the git calls.
#[derive(Debug, Clone)]
pub struct GitPlan {
    /// The `git` program.
    git: PathBuf,
    /// An empty file: git's global configuration (nothing from the runner
    /// user's `~/.gitconfig` applies).
    empty_config: PathBuf,
    /// An empty directory: `core.hooksPath`.
    empty_hooks: PathBuf,
}

impl GitPlan {
    pub fn new(git: PathBuf, empty_config: PathBuf, empty_hooks: PathBuf) -> Self {
        Self {
            git,
            empty_config,
            empty_hooks,
        }
    }

    /// `git -c … -C <repo> <subcommand>` with the fixed hardening.
    fn base(&self, repo: &Path, subcommand: &str) -> Cmd {
        let mut hooks = OsString::from("core.hooksPath=");
        hooks.push(&self.empty_hooks);
        Cmd::new(self.git.as_os_str())
            .arg("-c")
            .arg("credential.helper=")
            .arg("-c")
            .arg(hooks)
            .arg("-c")
            .arg("protocol.allow=never")
            .arg("-c")
            .arg("protocol.https.allow=always")
            .arg("-c")
            .arg("protocol.http.allow=always")
            .arg("-c")
            // A redirect never carries the job's token to another URL.
            .arg("http.followRedirects=false")
            .arg("-c")
            .arg("advice.detachedHead=false")
            .arg("-C")
            .arg(repo.as_os_str())
            .arg(subcommand)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", self.empty_config.as_os_str())
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ASKPASS", "")
            .env("HOME", repo.as_os_str())
    }

    /// `git init --quiet` in the (fresh, empty) work directory.
    pub fn init(&self, work: &Path) -> Cmd {
        self.base(work, "init").arg("--quiet")
    }

    /// The fetch of the pipeline's ref from `url` (the job's fetch route)
    /// with `token`: shallow, no tags, no submodules; the refspec after
    /// `--`.
    pub fn fetch(&self, work: &Path, url: &str, pipeline_id: i64, token: &JobToken) -> Cmd {
        let mut header = OsString::from("Authorization: Bearer ");
        header.push(token.expose());
        self.base(work, "fetch")
            .arg("--quiet")
            .arg("--no-tags")
            .arg("--no-recurse-submodules")
            .arg("--no-write-fetch-head")
            .arg("--depth=1")
            .arg("--")
            .arg(url)
            .arg(format!("+refs/pipelines/{pipeline_id}/head:refs/forge/job"))
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "http.extraHeader")
            .env("GIT_CONFIG_VALUE_0", header)
    }

    /// The commit the fetch wrote (`refs/forge/job`), printed.
    pub fn fetched(&self, work: &Path) -> Cmd {
        self.base(work, "rev-parse")
            .arg("--verify")
            .arg("--end-of-options")
            .arg("refs/forge/job^{commit}")
            .captured()
    }

    /// `.forge-ci.yml` at the pinned commit, printed.
    pub fn definition(&self, work: &Path, sha: &ObjectId) -> Cmd {
        self.base(work, "cat-file")
            .arg("blob")
            .arg(format!("{}:{DEFINITION_FILE}", sha.as_str()))
            .captured()
    }

    /// Check out the pinned commit (detached).
    pub fn checkout(&self, work: &Path, sha: &ObjectId) -> Cmd {
        self.base(work, "checkout")
            .arg("--quiet")
            .arg("--detach")
            .arg(sha.as_str())
            .arg("--")
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)] // tests may fail loudly

    use super::*;

    fn plan() -> GitPlan {
        GitPlan::new(
            PathBuf::from("git"),
            PathBuf::from("/runner/empty.gitconfig"),
            PathBuf::from("/runner/empty-hooks"),
        )
    }

    /// The token rides in git's environment configuration, never argv; the
    /// refspec and URL come after `--`.
    #[test]
    fn the_fetch_carries_the_token_in_the_environment_only() {
        let token = JobToken::new("fgj_abc_secret".to_owned());
        let cmd = plan().fetch(
            Path::new("/runner/job/work"),
            "https://forge.internal/api/v1/runner/jobs/7/repo.git",
            3,
            &token,
        );
        let argv = cmd.argv();
        assert!(argv.iter().all(|a| !a.contains("fgj_")), "{argv:?}");
        let dash = argv.iter().position(|a| a == "--").expect("--");
        assert_eq!(
            &argv[dash + 1..],
            [
                "https://forge.internal/api/v1/runner/jobs/7/repo.git",
                "+refs/pipelines/3/head:refs/forge/job"
            ]
        );
        assert_eq!(
            cmd.env_value("GIT_CONFIG_VALUE_0"),
            Some(std::ffi::OsStr::new("Authorization: Bearer fgj_abc_secret"))
        );
        assert_eq!(
            cmd.env_value("GIT_CONFIG_KEY_0"),
            Some(std::ffi::OsStr::new("http.extraHeader"))
        );
        for hardening in [
            "credential.helper=",
            "protocol.allow=never",
            "protocol.https.allow=always",
            // M4 C4 review: a redirect never takes the token elsewhere.
            "http.followRedirects=false",
        ] {
            assert!(argv.iter().any(|a| a == hardening), "{hardening}");
        }
        assert_eq!(
            cmd.env_value("GIT_CONFIG_NOSYSTEM"),
            Some(std::ffi::OsStr::new("1"))
        );
    }

    #[test]
    fn only_the_fetch_carries_a_token() {
        let sha = ObjectId::parse(&"a".repeat(40)).expect("sha");
        let work = Path::new("/w");
        for cmd in [
            plan().init(work),
            plan().fetched(work),
            plan().definition(work, &sha),
            plan().checkout(work, &sha),
        ] {
            assert_eq!(cmd.env_value("GIT_CONFIG_VALUE_0"), None, "{cmd:?}");
        }
    }
}
