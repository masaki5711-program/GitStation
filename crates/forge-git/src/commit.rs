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

/// `FG-REVIEW-0012` MergeIdentityInvalid (422): the account's e-mail cannot
/// be a commit identity (`field`, `rule`; never the value).
fn identity_invalid(field: &'static str, rule: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::MergeIdentityInvalid)
        .with_context("reason", "commit_identity_invalid")
        .with_context("field", field)
        .with_context("rule", rule)
}

/// git's "crud" (`ident.c`): stripped from both ends of a name and an
/// e-mail, so a value made only of it becomes empty ("empty ident").
fn crud(c: char) -> bool {
    c <= ' ' || matches!(c, '.' | ',' | ':' | ';' | '<' | '>' | '"' | '\\' | '\'')
}

/// A name as git will keep it: control characters become spaces, `<` / `>`
/// are dropped, the crud at both ends is trimmed.
fn normalized_name(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .filter(|c| !matches!(c, '<' | '>'))
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    cleaned.trim_matches(crud).to_owned()
}

impl CommitIdentity {
    /// The identity of an account (review round 1: normalised, never a
    /// permanent git failure): the display name with control characters,
    /// `<` / `>` and git's end "crud" removed — or the username when nothing
    /// is left ([`CommitIdentity::fell_back`]) — and the e-mail.
    /// `FG-REVIEW-0012` MergeIdentityInvalid (422, `field=email`) for an
    /// e-mail with a newline, a NUL, `<` / `>`, or only crud; `field=name`
    /// when neither the name nor the username leaves anything.
    pub fn new(display_name: &str, email: &str, username: &str) -> Result<Self, ForgeError> {
        let shown = normalized_name(display_name);
        let (name, fell_back) = if shown.is_empty() {
            (normalized_name(username), true)
        } else {
            (shown, false)
        };
        if name.is_empty() {
            return Err(identity_invalid("name", "empty"));
        }
        if email.chars().any(char::is_control) {
            return Err(identity_invalid("email", "control_character"));
        }
        if email.contains(['<', '>']) {
            return Err(identity_invalid("email", "angle_bracket"));
        }
        let email = email.trim_matches(crud);
        if email.is_empty() {
            return Err(identity_invalid("email", "empty"));
        }
        Ok(Self {
            name,
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

    /// Whether `<repo>/<name>.lock` or `<repo>/packed-refs.lock` exists: another git process holds the
    /// ref (a failed `update-ref` on it is worth a retry). A storage error
    /// is returned, never "not locked".
    pub fn ref_locked(
        &self,
        storage: &StorageRoot,
        id: RepoId,
        name: &RefName,
    ) -> Result<bool, ForgeError> {
        let repo = storage.existing(id)?;
        // The loose ref's lock, and `packed-refs.lock` (a pack-refs or a
        // packed-ref update holds every packed ref).
        for lock in [
            repo.join(format!("{}.lock", name.as_str())),
            repo.join("packed-refs.lock"),
        ] {
            let held = lock.try_exists().map_err(|e| {
                ForgeError::new(ErrorCode::StorageIoFailed)
                    .with_context("operation", "ref_lock_check")
                    .with_context("repo_id", id.get())
                    .with_source(e)
            })?;
            if held {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

#[cfg(test)]
#[path = "commit_tests.rs"]
mod tests;
