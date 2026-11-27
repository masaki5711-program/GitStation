//! `forge/agent-lock` (plan 16 A7, ADR-0032 Amendment A7): the second of
//! the three places a repository's Claude Code configuration is verified
//! (plan 16 §2.2) — on a commit, before it reaches `main`.
//!
//! [`AgentLock::verify`] reads, at the commit, the managed area
//! (`forge_git::browse::Browser::agent_config`: every root entry a
//! developer's file system takes for `.claude`, `CLAUDE.md` or `.mcp.json`,
//! symlinks and submodules kept as `not_a_regular_file`, and
//! `.gitstation/agent.lock.json`) and the registered signing keys (the
//! database: revoked keys refused),
//! and runs forge-agent's [`verify_lock`] — the very function `gstn agent
//! verify` runs — on those bytes: every managed file pinned by a part or a
//! `custom` entry, every part's digest recomputed and its signature
//! verified, a merged file recomputed from the signed sources the lock
//! carries. Then each locked version must be the one published: in the
//! database with the same digest (`FG-AGENT-0002 not_published` /
//! `digest_mismatch`), not revoked (`FG-AGENT-0006 revoked`).
//!
//! A commit with no managed file and no lock has nothing to verify
//! ([`LockVerdict::NoConfig`], a `success`). A part of the tree that cannot
//! be read whole (a listing cut, more than [`MAX_MANAGED_FILES`] files or
//! [`MAX_MANAGED_BYTES`] bytes) is a failure, never a pass.
//!
//! `forge/risk` asks the same question for its `agent_config` rule
//! (`agent_config_locked`, ADR-0017 Amendment A7), so the run and the risk
//! rule can never disagree.

use std::time::Duration;

use forge_agent::{verify_lock, KeyRing, Lock, Problem, LOCK_PATH};
use forge_core::checks::{CheckConclusion, CheckStatus, AGENT_LOCK_CHECK, MAX_CHECK_SUMMARY_BYTES};
use forge_core::{CheckName, ObjectId, RepoId};
use forge_db::checks::NewCheckRun;
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};
use forge_git::browse::Browser;

/// The most managed files read at one commit.
pub const MAX_MANAGED_FILES: usize = 2000;
/// The most bytes of managed files (and the lock) read at one commit.
pub const MAX_MANAGED_BYTES: u64 = 8 * 1024 * 1024;
/// The whole verification (the reads and the database).
pub const VERIFY_TIMEOUT: Duration = Duration::from_secs(30);

/// What the commit's configuration is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockVerdict {
    /// No managed file and no lock: nothing to verify.
    NoConfig,
    /// Every managed file is the lock's, signed and published.
    Verified {
        parts: usize,
        files: usize,
        /// Hand-added `custom` files the lock lists (neither signed nor
        /// published: `forge/risk` keeps such a configuration a stop).
        custom: usize,
    },
    /// What is wrong, most serious first.
    Failed(Vec<Problem>),
}

impl LockVerdict {
    pub fn is_green(&self) -> bool {
        !matches!(self, Self::Failed(_))
    }
}

fn problem(code: ErrorCode, reason: &'static str, subject: impl Into<String>) -> Problem {
    Problem {
        code,
        reason,
        subject: Some(subject.into()),
    }
}

fn mismatch(reason: &'static str, subject: impl Into<String>) -> Problem {
    problem(ErrorCode::AgentLockMismatch, reason, subject)
}

/// The verifier (forge-checks' handle on the database and the browser).
#[derive(Clone)]
pub struct AgentLock {
    db: Db,
    browse: Browser,
}

impl std::fmt::Debug for AgentLock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentLock").finish_non_exhaustive()
    }
}

impl AgentLock {
    pub fn new(db: Db, browse: Browser) -> Self {
        Self { db, browse }
    }

    /// Verify `commit` of `repo` (module docs), within [`VERIFY_TIMEOUT`]
    /// (`FG-WEB-0002 operation=agent_lock` past it). Errors are reads that
    /// failed (git, the database); a configuration that does not verify is
    /// a [`LockVerdict::Failed`].
    pub async fn verify(&self, repo: RepoId, commit: &ObjectId) -> Result<LockVerdict, ForgeError> {
        match tokio::time::timeout(VERIFY_TIMEOUT, self.verify_inner(repo, commit)).await {
            Ok(v) => v,
            Err(_) => Err(ForgeError::new(ErrorCode::RequestTimeout)
                .with_context("operation", "agent_lock")
                .with_context("timeout_ms", VERIFY_TIMEOUT.as_millis())),
        }
    }

    async fn verify_inner(
        &self,
        repo: RepoId,
        commit: &ObjectId,
    ) -> Result<LockVerdict, ForgeError> {
        let found = self
            .browse
            .agent_config(repo, commit.as_str(), MAX_MANAGED_FILES, MAX_MANAGED_BYTES)
            .await?;
        if found.is_empty() {
            return Ok(LockVerdict::NoConfig);
        }
        let mut problems: Vec<Problem> = found
            .unreadable
            .iter()
            .map(|(reason, path)| mismatch(reason, path.clone())) // clone: the problem owns it
            .collect();
        if found.bounded() {
            return Ok(LockVerdict::Failed(problems));
        }
        let Some(lock_bytes) = found.lock.as_deref() else {
            problems.push(mismatch("lock_missing", LOCK_PATH));
            return Ok(LockVerdict::Failed(problems));
        };
        let lock = match Lock::parse(lock_bytes) {
            Ok(lock) => lock,
            Err(p) => {
                problems.push(p);
                return Ok(LockVerdict::Failed(problems));
            }
        };
        let ring = key_ring(&self.db).await?;
        let report = verify_lock(&lock, &ring, &found.files);
        problems.extend(report.problems);
        self.published(&lock, &mut problems).await?;
        if problems.is_empty() {
            return Ok(LockVerdict::Verified {
                parts: report.parts,
                files: report.files,
                custom: lock.custom.len(),
            });
        }
        problems.sort_by_key(Problem::rank);
        Ok(LockVerdict::Failed(problems))
    }

    /// Each locked version is the one published, and not revoked.
    async fn published(&self, lock: &Lock, problems: &mut Vec<Problem>) -> Result<(), ForgeError> {
        let wanted: Vec<(String, String)> = lock
            .parts
            .iter()
            .map(|p| (p.slug.clone(), p.version.clone())) // clone: the query owns them
            .collect();
        if wanted.is_empty() {
            return Ok(());
        }
        if wanted.len() > forge_db::agent_builder::MAX_SELECTED {
            problems.push(mismatch("lock_invalid", "too_many_parts"));
            return Ok(());
        }
        let stored = forge_db::agent_builder::versions(&self.db, &wanted).await?;
        for part in &lock.parts {
            let label = format!("{}@{}", part.slug, part.version);
            match stored
                .iter()
                .find(|v| v.slug == part.slug && v.version == part.version)
            {
                None => problems.push(problem(
                    ErrorCode::PartSignatureInvalid,
                    "not_published",
                    label,
                )),
                Some(v) if v.digest != part.digest => problems.push(problem(
                    ErrorCode::PartSignatureInvalid,
                    "digest_mismatch",
                    label,
                )),
                Some(v) if v.state == "revoked" => {
                    problems.push(problem(ErrorCode::PartVersionRevoked, "revoked", label))
                }
                Some(_) => {}
            }
        }
        Ok(())
    }
}

/// The registered signing keys as forge-agent's ring (revoked keys
/// marked): what the server verifies with.
pub async fn key_ring(db: &Db) -> Result<KeyRing, ForgeError> {
    let keys = forge_db::agent_marketplace::signing_keys(db).await?;
    let listed: Vec<serde_json::Value> = keys
        .iter()
        .map(|k| {
            let mut v = serde_json::json!({"kid": k.kid, "public_key": k.public_key});
            if k.revoked {
                v["revoked_at"] = serde_json::Value::from("revoked");
            }
            v
        })
        .collect();
    let bytes = serde_json::to_vec(&serde_json::json!({ "keys": listed })).map_err(|_| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "key_ring_json")
    })?;
    KeyRing::parse(&bytes).map_err(|p| {
        ForgeError::new(ErrorCode::SigningKeyUnavailable)
            .with_context("reason", p.reason)
            .with_context("operation", "agent_lock")
    })
}

/// The `forge/agent-lock` run for `verdict`.
pub fn run_of(verdict: &LockVerdict) -> Result<NewCheckRun, ForgeError> {
    let name = CheckName::parse(AGENT_LOCK_CHECK).map_err(|_| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "agent_lock_name_invalid")
    })?;
    let (conclusion, summary) = match verdict {
        LockVerdict::NoConfig => (
            CheckConclusion::Success,
            "No Claude Code configuration (.claude, CLAUDE.md, .mcp.json) and no .gitstation/agent.lock.json in this commit: nothing to verify.".to_owned(),
        ),
        LockVerdict::Verified { parts, files, .. } => (
            CheckConclusion::Success,
            format!(
                "Verified: {files} managed file(s) are exactly what the lock pins, {parts} part version(s) signed with a registered key and published, none revoked (plan 16 A7)."
            ),
        ),
        LockVerdict::Failed(problems) => {
            let mut lines = vec![format!(
                "FAILED: {} problem(s). The Claude Code configuration is not what the marketplace signed; rebuild it with the agent builder (runbook agent-lock).",
                problems.len()
            )];
            for p in problems.iter().take(200) {
                let subject: String = p
                    .subject
                    .as_deref()
                    .unwrap_or("")
                    .chars()
                    .map(|c| if c.is_control() { '?' } else { c })
                    .take(300)
                    .collect();
                lines.push(format!("- {} {} {subject}", p.code.id(), p.reason));
            }
            (CheckConclusion::Failure, lines.join("\n"))
        }
    };
    let mut summary = summary;
    if summary.len() > MAX_CHECK_SUMMARY_BYTES {
        let mut end = MAX_CHECK_SUMMARY_BYTES - 4;
        while !summary.is_char_boundary(end) {
            end -= 1;
        }
        summary.truncate(end);
        summary.push('…');
    }
    Ok(NewCheckRun {
        name,
        status: CheckStatus::Completed,
        conclusion: Some(conclusion),
        summary,
        details_url: None,
        annotations: Vec::new(),
    })
}
