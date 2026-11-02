//! Writing the commit of a merge (M3 R4, ADR-0024 §4.1): `git commit-tree`
//! with the trial merge's tree, the parents the method asks for, the message
//! on standard input and the author / committer from the database.
//!
//! - argv: `-c core.hooksPath=<empty> commit-tree -p <parent>… -F -
//!   --end-of-options <tree>` (pinned by `merge_plumbing_argv_on_git_2_38`).
//!   Parents and the tree are validated object ids (40 / 64 hex, never a
//!   `-`); the message (user input, at most [`MAX_MESSAGE_BYTES`]) goes on
//!   standard input, never in argv.
//! - The identity goes in `GIT_AUTHOR_NAME` / `_EMAIL` and
//!   `GIT_COMMITTER_NAME` / `_EMAIL` only (no shell; the scrubbed
//!   environment of [`crate::GitCommand`] stays as it is — the pinned
//!   variables are applied after these, so nothing overrides them).
//!   [`CommitIdentity::new`] refuses a value with a newline, a NUL, `<` or
//!   `>`, and an empty e-mail; a name that is empty after trimming becomes
//!   the account's username (git refuses an empty name) and says so
//!   ([`CommitIdentity::fell_back`]).
//! - [`Git::ref_locked`]: whether `<ref>.lock` exists (a failed `update-ref`
//!   on a locked ref is retried, ADR-0024 §4.2 step 7).

use std::ffi::OsString;
use std::time::Duration;

use forge_core::{ObjectId, RefName, RepoId};
use forge_errors::{ErrorCode, ForgeError};

use crate::command::{Git, GitCommand};
use crate::revision::Revision;
use crate::storage::StorageRoot;

/// The longest commit message a merge writes (ADR-0024 §4.1: 64 KiB).
pub const MAX_MESSAGE_BYTES: usize = 64 * 1024;

/// One side of a commit's identity (validated; module docs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitIdentity {
    name: String,
    email: String,
    fell_back: bool,
}

fn identity_invalid(field: &'static str, rule: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::Internal)
        .with_context("reason", "commit_identity_invalid")
        .with_context("field", field)
        .with_context("rule", rule)
}

fn check_value(field: &'static str, value: &str) -> Result<(), ForgeError> {
    if value.contains(['\n', '\r', '\0']) {
        return Err(identity_invalid(field, "control_character"));
    }
    if value.contains(['<', '>']) {
        return Err(identity_invalid(field, "angle_bracket"));
    }
    Ok(())
}

impl CommitIdentity {
    /// The identity of an account: its display name (or `username` when
    /// that is empty after trimming) and its e-mail. `FG-SYS-0001
    /// reason=commit_identity_invalid` (with `field` and `rule`, never the
    /// value) for a value git would misread.
    pub fn new(display_name: &str, email: &str, username: &str) -> Result<Self, ForgeError> {
        let trimmed = display_name.trim();
        let (name, fell_back) = if trimmed.is_empty() {
            (username.trim(), true)
        } else {
            (trimmed, false)
        };
        if name.is_empty() {
            return Err(identity_invalid("name", "empty"));
        }
        let email = email.trim();
        if email.is_empty() {
            return Err(identity_invalid("email", "empty"));
        }
        check_value("name", name)?;
        check_value("email", email)?;
        Ok(Self {
            name: name.to_owned(),
            email: email.to_owned(),
            fell_back,
        })
    }

    /// The display name was empty: the username is used instead.
    pub fn fell_back(&self) -> bool {
        self.fell_back
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn email(&self) -> &str {
        &self.email
    }
}

/// What one merge commit is made of.
#[derive(Debug, Clone)]
pub struct NewCommit<'a> {
    pub tree: &'a ObjectId,
    /// In order (the first is the old target: ADR-0024 §4.1).
    pub parents: &'a [ObjectId],
    pub message: &'a str,
    pub author: &'a CommitIdentity,
    pub committer: &'a CommitIdentity,
}

impl GitCommand {
    /// The commit identity, passed in the environment only.
    fn identity(mut self, author: &CommitIdentity, committer: &CommitIdentity) -> Self {
        for (key, value) in [
            ("GIT_AUTHOR_NAME", author.name()),
            ("GIT_AUTHOR_EMAIL", author.email()),
            ("GIT_COMMITTER_NAME", committer.name()),
            ("GIT_COMMITTER_EMAIL", committer.email()),
        ] {
            self = self.env(key, OsString::from(value));
        }
        self
    }
}

impl Git {
    /// `commit-tree` for `commit` in `repo` (module docs), hooks pinned to
    /// the empty directory.
    pub(crate) fn commit_tree_command(
        &self,
        repo: &std::path::Path,
        commit: &NewCommit<'_>,
    ) -> Result<GitCommand, ForgeError> {
        let mut cmd = self.with_empty_hooks(self.command("commit-tree"))?;
        for parent in commit.parents {
            let parent = Revision::object_id(parent.as_str())?;
            cmd = cmd.arg("-p").arg_owned(parent.as_str());
        }
        Ok(cmd
            .arg("-F")
            .arg("-")
            .revision(Revision::object_id(commit.tree.as_str())?)
            .repo(repo)
            .identity(commit.author, commit.committer))
    }

    /// Write `commit` in repository `id`; its object id. `FG-SYS-0001
    /// reason=commit_message_too_long` past [`MAX_MESSAGE_BYTES`] (the
    /// caller checks first), `FG-GIT-0002` on timeout, `FG-GIT-0003` when git
    /// refuses.
    pub async fn commit_tree(
        &self,
        storage: &StorageRoot,
        id: RepoId,
        commit: &NewCommit<'_>,
        timeout: Duration,
    ) -> Result<ObjectId, ForgeError> {
        if commit.message.len() > MAX_MESSAGE_BYTES {
            return Err(ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "commit_message_too_long")
                .with_context("bytes", commit.message.len()));
        }
        let repo = storage.existing(id)?;
        let out = self
            .commit_tree_command(&repo, commit)?
            .stdin(commit.message.as_bytes().to_vec())
            .timeout(timeout)
            .output()
            .await
            .map_err(|e| e.with_context("repo_id", id.get()))?;
        let text = String::from_utf8_lossy(&out.stdout);
        ObjectId::parse(text.trim()).map_err(|_| {
            ForgeError::new(ErrorCode::GitProcessFailed)
                .with_context("subcommand", "commit-tree")
                .with_context("reason", "unexpected_output")
        })
    }

    /// Whether `<repo>/<name>.lock` exists: another git process holds the
    /// ref (a failed `update-ref` on it is worth a retry). A storage error
    /// is returned, never "not locked".
    pub fn ref_locked(
        &self,
        storage: &StorageRoot,
        id: RepoId,
        name: &RefName,
    ) -> Result<bool, ForgeError> {
        let repo = storage.existing(id)?;
        let lock = repo.join(format!("{}.lock", name.as_str()));
        lock.try_exists().map_err(|e| {
            ForgeError::new(ErrorCode::StorageIoFailed)
                .with_context("operation", "ref_lock_check")
                .with_context("repo_id", id.get())
                .with_source(e)
        })
    }
}

#[cfg(test)]
#[path = "commit_tests.rs"]
mod tests;
