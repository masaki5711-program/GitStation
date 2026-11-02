//! Reading and writing refs forge owns (M3 R1b, ADR-0024 §2): the merge
//! request version refs (`hidden_refs`) and the tip of a source branch.
//!
//! - [`Git::read_refs`]: `for-each-ref --format=%(objectname) %(refname)`
//!   for one validated [`RefName`] pattern (git matches it as a prefix of
//!   whole components), every line parsed back into a [`RefName`] and an
//!   [`ObjectId`] (anything else is `FG-GIT-0003
//!   reason=unexpected_output`, never skipped).
//! - [`Git::update_refs`]: ONE `update-ref -z --stdin` transaction
//!   (`start`, the changes, `prepare`, `commit`): either every change
//!   applies or none does. A create fails when the ref exists, an update /
//!   delete when the ref's current value is not the given old one (CAS).
//!   `core.hooksPath` is pinned to forge's empty directory, so no
//!   `reference-transaction` hook runs; object ids are validated by type
//!   and every name is a validated `RefName`; nothing reaches argv.
//!
//! Every invocation has its caller's timeout (`FG-GIT-0002`); a refused
//! transaction is `FG-GIT-0003` with git's stderr in the log context only.
//! The caller tells a lost CAS from other failures by reading the refs
//! again (the version job, `forge_review::versions`).

use std::collections::BTreeMap;
use std::time::Duration;

use forge_core::{ObjectId, RefName, RepoId};
use forge_errors::{ErrorCode, ForgeError};

use crate::command::Git;
use crate::storage::StorageRoot;

/// One change of an [`Git::update_refs`] transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefChange {
    /// Create `name` at `new`; fails when it exists.
    Create { name: RefName, new: ObjectId },
    /// Move `name` from `old` to `new` (compare and swap).
    Update {
        name: RefName,
        new: ObjectId,
        old: ObjectId,
    },
    /// Delete `name` if it is still `old` (compare and swap).
    Delete { name: RefName, old: ObjectId },
}

impl RefChange {
    pub fn name(&self) -> &RefName {
        match self {
            Self::Create { name, .. } | Self::Update { name, .. } | Self::Delete { name, .. } => {
                name
            }
        }
    }

    fn encode(&self, out: &mut Vec<u8>) {
        let mut field = |s: &str| {
            out.extend_from_slice(s.as_bytes());
            out.push(0);
        };
        match self {
            Self::Create { name, new } => {
                field(&format!("create {}", name.as_str()));
                field(new.as_str());
            }
            Self::Update { name, new, old } => {
                field(&format!("update {}", name.as_str()));
                field(new.as_str());
                field(old.as_str());
            }
            Self::Delete { name, old } => {
                field(&format!("delete {}", name.as_str()));
                field(old.as_str());
            }
        }
    }
}

/// What [`Git::update_refs`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefUpdateResult {
    /// Every change applied in one transaction.
    Applied { changes: usize },
    /// Nothing was asked: git did not run.
    Nothing,
}

/// The `update-ref -z --stdin` input of `changes`, wrapped in an explicit
/// transaction.
pub(crate) fn transaction_input(changes: &[RefChange]) -> Vec<u8> {
    let mut out = b"start\0".to_vec();
    for change in changes {
        change.encode(&mut out);
    }
    out.extend_from_slice(b"prepare\0commit\0");
    out
}

fn unexpected(subcommand: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::GitProcessFailed)
        .with_context("subcommand", subcommand)
        .with_context("reason", "unexpected_output")
}

impl Git {
    /// Every ref matching `pattern` (itself and the refs below it) with its
    /// object id, in name order.
    pub async fn read_refs(
        &self,
        storage: &StorageRoot,
        id: RepoId,
        pattern: &RefName,
        timeout: Duration,
    ) -> Result<BTreeMap<RefName, ObjectId>, ForgeError> {
        let repo = storage.existing(id)?;
        let out = self
            .command("for-each-ref")
            .arg("--format=%(objectname) %(refname)")
            .revision(pattern)
            .repo(&repo)
            .timeout(timeout)
            .output()
            .await?;
        let text = String::from_utf8(out.stdout).map_err(|_| unexpected("for-each-ref"))?;
        let mut refs = BTreeMap::new();
        for line in text.lines().filter(|l| !l.is_empty()) {
            let (oid, name) = line
                .split_once(' ')
                .ok_or_else(|| unexpected("for-each-ref"))?;
            let oid = ObjectId::parse(oid).map_err(|_| unexpected("for-each-ref"))?;
            let name = RefName::parse(name).map_err(|_| unexpected("for-each-ref"))?;
            refs.insert(name, oid);
        }
        Ok(refs)
    }

    /// The object `name` points to, `None` when the ref does not exist.
    pub async fn read_ref(
        &self,
        storage: &StorageRoot,
        id: RepoId,
        name: &RefName,
        timeout: Duration,
    ) -> Result<Option<ObjectId>, ForgeError> {
        let mut refs = self.read_refs(storage, id, name, timeout).await?;
        Ok(refs.remove(name))
    }

    /// Apply `changes` in one `update-ref` transaction (module docs).
    /// `FG-GIT-0003` when git refuses it (a lost compare-and-swap, a ref
    /// that already exists, a locked ref): nothing changed.
    pub async fn update_refs(
        &self,
        storage: &StorageRoot,
        id: RepoId,
        changes: &[RefChange],
        timeout: Duration,
    ) -> Result<RefUpdateResult, ForgeError> {
        if changes.is_empty() {
            return Ok(RefUpdateResult::Nothing);
        }
        let repo = storage.existing(id)?;
        let cmd = self.command("update-ref");
        let cmd = self
            .with_empty_hooks(cmd)?
            .arg("-z")
            .arg("--stdin")
            .repo(&repo)
            .stdin(transaction_input(changes))
            .timeout(timeout);
        cmd.output().await.map_err(|e| {
            e.with_context("repo_id", id.get())
                .with_context("changes", changes.len())
        })?;
        Ok(RefUpdateResult::Applied {
            changes: changes.len(),
        })
    }
}

#[cfg(test)]
#[path = "refs_tests.rs"]
mod tests;
