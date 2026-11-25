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

use forge_client::forge_ci;

use crate::api::JobToken;
use crate::process::Cmd;

/// The file read at the pinned commit.
pub const DEFINITION_FILE: &str = ".forge-ci.yml";

/// The directories of the autofix step (M4 C14).
#[derive(Debug, Clone, Copy)]
pub struct AutofixDirs<'a> {
    /// The fixer's work tree (what the container left).
    pub work: &'a Path,
    /// The autofix repository (outside every mount).
    pub git_dir: &'a Path,
    /// Its index file (outside every mount).
    pub index: &'a Path,
}

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
    /// The runner's `ca_file`: the fetch's `GIT_SSL_CAINFO`.
    ca_file: Option<PathBuf>,
}

impl GitPlan {
    pub fn new(git: PathBuf, empty_config: PathBuf, empty_hooks: PathBuf) -> Self {
        Self {
            git,
            empty_config,
            empty_hooks,
            ca_file: None,
        }
    }

    /// Trust `ca_file` (the runner's configured CA bundle) in the fetch.
    #[must_use]
    pub fn with_ca_file(mut self, ca_file: Option<PathBuf>) -> Self {
        self.ca_file = ca_file;
        self
    }

    /// `git -c … -C <repo> <subcommand>` with the fixed hardening.
    fn base(&self, repo: &Path, subcommand: &str) -> Cmd {
        self.base_home(repo, repo, subcommand)
    }

    /// [`GitPlan::base`] with `HOME` at `home`.
    fn base_home(&self, repo: &Path, home: &Path, subcommand: &str) -> Cmd {
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
            .env("HOME", home.as_os_str())
    }

    /// `git init --quiet` in the (fresh, empty) work directory.
    pub fn init(&self, work: &Path) -> Cmd {
        self.base(work, "init").arg("--quiet")
    }

    /// The fetch of the pipeline's ref from `url` (the job's fetch route)
    /// with `token`: shallow (`--depth=1`), no tags, no submodules; the
    /// refspec after `--`.
    pub fn fetch(&self, work: &Path, url: &str, pipeline_id: i64, token: &JobToken) -> Cmd {
        self.fetch_depth(work, url, pipeline_id, token, 1)
    }

    /// Plan 21 G2: the same fetch again with `--depth=<depth>` — the job's
    /// `fetch_depth` from the pinned, hash-checked definition — so the
    /// shallow clone holds that many commits of the pipeline's history
    /// (`HEAD^1` with 2). The value is a number, never text from the claim;
    /// `None` outside 2..=[`forge_ci::MAX_FETCH_DEPTH`] (the lint refuses
    /// such a file first, so this is a second wall, never a silent clamp).
    pub fn deepen(
        &self,
        work: &Path,
        url: &str,
        pipeline_id: i64,
        token: &JobToken,
        depth: u32,
    ) -> Option<Cmd> {
        (2..=forge_ci::MAX_FETCH_DEPTH)
            .contains(&depth)
            .then(|| self.fetch_depth(work, url, pipeline_id, token, depth))
    }

    fn fetch_depth(
        &self,
        work: &Path,
        url: &str,
        pipeline_id: i64,
        token: &JobToken,
        depth: u32,
    ) -> Cmd {
        let mut header = OsString::from("Authorization: Bearer ");
        header.push(token.expose());
        let cmd = self
            .base(work, "fetch")
            .arg("--quiet")
            .arg("--no-tags")
            .arg("--no-recurse-submodules")
            .arg("--no-write-fetch-head")
            .arg(format!("--depth={depth}"))
            .arg("--")
            .arg(url)
            .arg(format!("+refs/pipelines/{pipeline_id}/head:refs/forge/job"))
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "http.extraHeader")
            .env("GIT_CONFIG_VALUE_0", header);
        match &self.ca_file {
            Some(ca) => cmd.env("GIT_SSL_CAINFO", ca.as_os_str()),
            None => cmd,
        }
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

    // ── M4 C14: the autofix bundle (ADR-0017 Amendment C14) ──
    //
    // After a fixer's script exited 0 and its container is gone, the work
    // tree is recorded in a repository of its own OUTSIDE every mount
    // (`JobDirs::autofix_git`, a fresh fetch of the pinned commit): the
    // checkout's `.git`, which the container could write, is never read.
    // `GIT_DIR`, `GIT_WORK_TREE` and `GIT_INDEX_FILE` point there; `HOME`
    // is the autofix repository (no ignore file of the work tree's choosing).

    /// `git init --quiet --bare` of the autofix repository (created empty).
    pub fn autofix_init(&self, git_dir: &Path) -> Cmd {
        self.base(git_dir, "init").arg("--quiet").arg("--bare")
    }

    fn autofix_step(&self, a: &AutofixDirs<'_>, subcommand: &str) -> Cmd {
        self.base_home(a.work, a.git_dir, subcommand)
            .env("GIT_DIR", a.git_dir.as_os_str())
            .env("GIT_WORK_TREE", a.work.as_os_str())
            .env("GIT_INDEX_FILE", a.index.as_os_str())
    }

    /// The index from the pinned commit.
    pub fn autofix_read_tree(&self, a: &AutofixDirs<'_>, sha: &ObjectId) -> Cmd {
        // A validated 40 / 64 hex id: never read as an option.
        self.autofix_step(a, "read-tree").arg(sha.as_str())
    }

    /// Every change of the work tree into the index (never a `.git`).
    pub fn autofix_add(&self, a: &AutofixDirs<'_>) -> Cmd {
        self.autofix_step(a, "add").arg("--all")
    }

    /// The index as a tree, printed.
    pub fn autofix_write_tree(&self, a: &AutofixDirs<'_>) -> Cmd {
        self.autofix_step(a, "write-tree").captured()
    }

    /// The pinned commit's tree, printed.
    pub fn autofix_input_tree(&self, a: &AutofixDirs<'_>, sha: &ObjectId) -> Cmd {
        self.autofix_step(a, "rev-parse")
            .arg("--verify")
            .arg("--end-of-options")
            .arg(format!("{}^{{tree}}", sha.as_str()))
            .captured()
    }

    /// The transport commit of `tree` on the pinned commit, printed (a
    /// fixed identity and message: forge uses only the tree).
    pub fn autofix_commit(&self, a: &AutofixDirs<'_>, tree: &ObjectId, sha: &ObjectId) -> Cmd {
        // Validated hex ids only (never read as an option).
        self.autofix_step(a, "commit-tree")
            .arg(tree.as_str())
            .arg("-p")
            .arg(sha.as_str())
            .arg("-m")
            .arg("autofix transport")
            .env("GIT_AUTHOR_NAME", "forge-runner")
            .env("GIT_AUTHOR_EMAIL", "runner@forge.invalid")
            .env("GIT_COMMITTER_NAME", "forge-runner")
            .env("GIT_COMMITTER_EMAIL", "runner@forge.invalid")
            .captured()
    }

    /// `refs/forge/autofix` at the transport commit.
    pub fn autofix_ref(&self, a: &AutofixDirs<'_>, commit: &ObjectId) -> Cmd {
        self.autofix_step(a, "update-ref")
            .arg(forge_core::autofix::BUNDLE_REF)
            .arg(commit.as_str())
    }

    /// The bundle of `refs/forge/autofix` without the pinned commit's
    /// history (its only prerequisite).
    pub fn autofix_bundle(&self, a: &AutofixDirs<'_>, out: &Path, sha: &ObjectId) -> Cmd {
        self.autofix_step(a, "bundle")
            .arg("create")
            .arg(out.as_os_str())
            .arg(forge_core::autofix::BUNDLE_REF)
            .arg(format!("^{}", sha.as_str()))
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

    /// M4 C4 review: the runner's `ca_file` is the fetch's CA bundle;
    /// without one git's default applies.
    #[test]
    fn the_fetch_trusts_the_configured_ca_file() {
        let token = JobToken::new("fgj_abc_secret".to_owned());
        let url = "https://forge.internal/api/v1/runner/jobs/7/repo.git";
        let work = Path::new("/w");
        let cmd = plan().fetch(work, url, 3, &token);
        assert_eq!(cmd.env_value("GIT_SSL_CAINFO"), None);
        let with_ca = plan().with_ca_file(Some(PathBuf::from("/etc/forge-runner/ca.pem")));
        let cmd = with_ca.fetch(work, url, 3, &token);
        assert_eq!(
            cmd.env_value("GIT_SSL_CAINFO"),
            Some(std::ffi::OsStr::new("/etc/forge-runner/ca.pem"))
        );
    }

    /// Plan 21 G2: the first fetch is `--depth=1`; the deepening fetch
    /// carries the job's depth (2..=50, nothing outside it), everything
    /// else as the first.
    #[test]
    fn the_deepening_fetch_carries_the_jobs_depth_and_nothing_else_changes() {
        let token = JobToken::new("fgj_abc_secret".to_owned());
        let url = "https://forge.internal/api/v1/runner/jobs/7/repo.git";
        let work = Path::new("/w");
        let depth_of = |cmd: &Cmd| -> Vec<String> {
            cmd.argv()
                .into_iter()
                .filter(|a| a.starts_with("--depth"))
                .collect()
        };
        let first = plan().fetch(work, url, 3, &token);
        assert_eq!(depth_of(&first), ["--depth=1"]);
        for asked in [0, 1, 51, u32::MAX] {
            assert!(
                plan().deepen(work, url, 3, &token, asked).is_none(),
                "{asked}"
            );
        }
        for (asked, argv) in [(2, "--depth=2"), (50, "--depth=50")] {
            let deeper = plan()
                .deepen(work, url, 3, &token, asked)
                .expect("in range");
            assert_eq!(depth_of(&deeper), [argv], "{asked}");
            let same = |c: &Cmd| -> Vec<String> {
                c.argv()
                    .into_iter()
                    .filter(|a| !a.starts_with("--depth"))
                    .collect()
            };
            assert_eq!(same(&deeper), same(&first), "{asked}");
            assert_eq!(
                deeper.env_value("GIT_CONFIG_VALUE_0"),
                first.env_value("GIT_CONFIG_VALUE_0")
            );
        }
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
