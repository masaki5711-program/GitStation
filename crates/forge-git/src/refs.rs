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

/// `core.packedRefsTimeout` (ms) of every ref-writing git forge runs
/// (`update-ref`, receive-pack; M3 O3): `packed-refs.lock` is held by the
/// maintenance's `pack-refs` and by any transaction deleting a packed ref,
/// and git's default (1 s) would fail the writer instead of waiting.
pub const PACKED_REFS_TIMEOUT_MS: &str = "10000";

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
            // M3 O3: wait out the maintenance's pack-refs (and a push's
            // deletion) holding `packed-refs.lock`, instead of git's 1 s.
            .config_override("core.packedRefsTimeout", PACKED_REFS_TIMEOUT_MS)
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

impl Git {
    /// `refs/pipelines/<pipeline_id>/head` at `sha` (M4 C4, ADR-0029 §3):
    /// created when missing, kept when it already points there. A pipeline
    /// ref at another commit is a defect (`FG-SYS-0001
    /// reason=pipeline_ref_moved`; pipeline ids are never reused and
    /// receive-pack cannot write the hidden prefix). Two jobs creating it at
    /// once: the loser reads it back.
    pub async fn ensure_pipeline_ref(
        &self,
        storage: &StorageRoot,
        id: RepoId,
        pipeline_id: i64,
        sha: &ObjectId,
        timeout: Duration,
    ) -> Result<RefName, ForgeError> {
        let name = crate::hidden_refs::pipeline_head_ref(pipeline_id)?;
        let moved = |at: &ObjectId| {
            ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "pipeline_ref_moved")
                .with_context("pipeline_id", pipeline_id)
                .with_context("repo_id", id.get())
                .with_context("at", at.as_str().to_owned())
        };
        match self.read_ref(storage, id, &name, timeout).await? {
            Some(at) if at == *sha => return Ok(name),
            Some(at) => return Err(moved(&at)),
            None => {}
        }
        let create = [RefChange::Create {
            name: name.clone(), // clone: the caller gets the name back
            new: sha.clone(),   // clone: the change owns its id
        }];
        match self.update_refs(storage, id, &create, timeout).await {
            Ok(_) => Ok(name),
            Err(e) => match self.read_ref(storage, id, &name, timeout).await? {
                Some(at) if at == *sha => Ok(name),
                Some(at) => Err(moved(&at)),
                None => Err(e.with_context("pipeline_id", pipeline_id)),
            },
        }
    }
}

/// One `refs/pipelines/<pipeline_id>/head` of a repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipelineRef {
    pub pipeline_id: i64,
    pub name: RefName,
    pub at: ObjectId,
}

/// The pipeline id of `refs/pipelines/<id>/head`, `None` for any other
/// name.
fn pipeline_id_of(name: &RefName) -> Option<i64> {
    let id = name
        .as_str()
        .strip_prefix(crate::hidden_refs::PIPELINE_REF_PREFIX)?
        .strip_suffix("/head")?;
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) || id.starts_with('0') {
        return None;
    }
    id.parse().ok()
}

impl Git {
    /// Delete `refs/pipelines/<pipeline_id>/head` (M4 C4 review: the ref
    /// pins its commit only while the pipeline runs; deleted when it
    /// reaches a terminal state, the maintenance sweep is the backstop).
    /// `false`: there was no such ref (already deleted). A retried
    /// pipeline's next job fetch creates it again
    /// ([`Git::ensure_pipeline_ref`]).
    pub async fn delete_pipeline_ref(
        &self,
        storage: &StorageRoot,
        id: RepoId,
        pipeline_id: i64,
        timeout: Duration,
    ) -> Result<bool, ForgeError> {
        let name = crate::hidden_refs::pipeline_head_ref(pipeline_id)?;
        let Some(at) = self.read_ref(storage, id, &name, timeout).await? else {
            return Ok(false);
        };
        let delete = [RefChange::Delete {
            name: name.clone(), // clone: read again below on a lost CAS
            old: at,
        }];
        match self.update_refs(storage, id, &delete, timeout).await {
            Ok(_) => Ok(true),
            // Deleted meanwhile (the sweep, another completion): done.
            Err(e) => match self.read_ref(storage, id, &name, timeout).await? {
                None => Ok(false),
                Some(_) => Err(e.with_context("pipeline_id", pipeline_id)),
            },
        }
    }

    /// Every pipeline ref of the repository, by pipeline id. A name under
    /// `refs/pipelines/` that forge did not build is `FG-GIT-0003
    /// reason=unexpected_output` (only forge writes the prefix).
    pub async fn pipeline_refs(
        &self,
        storage: &StorageRoot,
        id: RepoId,
        timeout: Duration,
    ) -> Result<Vec<PipelineRef>, ForgeError> {
        let pattern =
            RefName::parse(crate::hidden_refs::PIPELINE_REF_PREFIX.trim_end_matches('/'))?;
        let mut refs = Vec::new();
        for (name, at) in self.read_refs(storage, id, &pattern, timeout).await? {
            let Some(pipeline_id) = pipeline_id_of(&name) else {
                return Err(unexpected("for-each-ref")
                    .with_context("repo_id", id.get())
                    .with_context("ref", name.as_str().to_owned()));
            };
            refs.push(PipelineRef {
                pipeline_id,
                name,
                at,
            });
        }
        refs.sort_by_key(|r| r.pipeline_id);
        Ok(refs)
    }

    /// Delete `refs` in one transaction, each only if it is still at the
    /// listed commit (the maintenance sweep of finished pipelines' refs).
    pub async fn delete_pipeline_refs(
        &self,
        storage: &StorageRoot,
        id: RepoId,
        refs: &[PipelineRef],
        timeout: Duration,
    ) -> Result<RefUpdateResult, ForgeError> {
        let changes: Vec<RefChange> = refs
            .iter()
            .map(|r| RefChange::Delete {
                name: r.name.clone(), // clone: the change owns its name
                old: r.at.clone(),    // clone: as above
            })
            .collect();
        self.update_refs(storage, id, &changes, timeout).await
    }
}

/// One `refs/merge-queue/<queue_id>/train` of a repository (M4 C8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeQueueRef {
    pub queue_id: i64,
    pub name: RefName,
    pub at: ObjectId,
}

/// The queue id of `refs/merge-queue/<id>/train`, `None` for any other
/// name.
fn queue_id_of(name: &RefName) -> Option<i64> {
    let id = name
        .as_str()
        .strip_prefix(crate::hidden_refs::MERGE_QUEUE_REF_PREFIX)?
        .strip_suffix("/train")?;
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) || id.starts_with('0') {
        return None;
    }
    id.parse().ok()
}

impl Git {
    /// Every merge queue train ref of the repository, by queue id (the
    /// maintenance sweep). A name under `refs/merge-queue/` that forge did
    /// not build is `FG-GIT-0003 reason=unexpected_output`.
    pub async fn merge_queue_refs(
        &self,
        storage: &StorageRoot,
        id: RepoId,
        timeout: Duration,
    ) -> Result<Vec<MergeQueueRef>, ForgeError> {
        let pattern =
            RefName::parse(crate::hidden_refs::MERGE_QUEUE_REF_PREFIX.trim_end_matches('/'))?;
        let mut refs = Vec::new();
        for (name, at) in self.read_refs(storage, id, &pattern, timeout).await? {
            let Some(queue_id) = queue_id_of(&name) else {
                return Err(unexpected("for-each-ref")
                    .with_context("repo_id", id.get())
                    .with_context("ref", name.as_str().to_owned()));
            };
            refs.push(MergeQueueRef { queue_id, name, at });
        }
        refs.sort_by_key(|r| r.queue_id);
        Ok(refs)
    }

    /// Delete `refs` in one transaction, each only if it is still at the
    /// listed commit (a batch that ended, the maintenance sweep).
    pub async fn delete_merge_queue_refs(
        &self,
        storage: &StorageRoot,
        id: RepoId,
        refs: &[MergeQueueRef],
        timeout: Duration,
    ) -> Result<RefUpdateResult, ForgeError> {
        let changes: Vec<RefChange> = refs
            .iter()
            .map(|r| RefChange::Delete {
                name: r.name.clone(), // clone: the change owns its name
                old: r.at.clone(),    // clone: as above
            })
            .collect();
        self.update_refs(storage, id, &changes, timeout).await
    }
}

#[cfg(test)]
#[path = "refs_tests.rs"]
mod tests;
