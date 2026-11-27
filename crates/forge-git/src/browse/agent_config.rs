//! A commit's Claude Code configuration as the agent lock sees it (plan 16
//! A7, ADR-0032 Amendment A7): read for `forge/agent-lock` (forge-checks)
//! and by the builder (forge-marketplace), the same listing for both.
//!
//! ```text
//! git ls-tree -z --long <commit>                             (the root)
//! git ls-tree -r -z --long --full-tree <commit> -- <.claude>  (each managed dir)
//! git ls-tree -r -z --long --full-tree <commit> -- .gitstation
//! git cat-file --batch                                       (the contents)
//! ```
//!
//! The managed area is every root entry a developer's file system takes for
//! `.claude`, `CLAUDE.md` or `.mcp.json` (ASCII case folded) and, for the
//! lock, `.gitstation/agent.lock.json` exactly. Symlinks and submodules in
//! it are kept as `not_a_regular_file` (never followed, never skipped); a
//! listing cut short or over the caller's bounds is reported, never read in
//! part as if whole.

use std::collections::BTreeMap;

use forge_core::RepoId;
use forge_errors::{ErrorCode, ForgeError};

use super::path::RepoPath;
use super::scan::ListedBlob;
use super::tree::EntryKind;
use super::Browser;

/// The lock's path.
const LOCK_PATH: &str = ".gitstation/agent.lock.json";

/// What [`Browser::agent_config`] found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentConfigListing {
    /// The regular files of the managed area, by path, with their bytes.
    pub files: BTreeMap<String, Vec<u8>>,
    /// The lock's bytes, when it is there (a regular file).
    pub lock: Option<Vec<u8>>,
    /// `(reason, path)`: `not_a_regular_file` (a symlink, a submodule, a
    /// `.claude` that is a file), `tree_unreadable` (a listing cut short),
    /// `too_many_files`, `too_large`. When any is a bound, `files` is empty.
    pub unreadable: Vec<(&'static str, String)>,
}

impl AgentConfigListing {
    /// No managed file, no lock, nothing unreadable.
    pub fn is_empty(&self) -> bool {
        self.files.is_empty() && self.lock.is_none() && self.unreadable.is_empty()
    }

    /// Whether a bound stopped the read (the files are not all there).
    pub fn bounded(&self) -> bool {
        self.unreadable
            .iter()
            .any(|(r, _)| matches!(*r, "tree_unreadable" | "too_many_files" | "too_large"))
    }
}

/// A root name of the managed area (`Some(true)`: the directory).
fn managed_root(name: &str) -> Option<bool> {
    match name.to_ascii_lowercase().as_str() {
        ".claude" => Some(true),
        "claude.md" | ".mcp.json" => Some(false),
        _ => None,
    }
}

fn dir_path(name: &str) -> Result<RepoPath, ForgeError> {
    RepoPath::parse(name).map_err(|reason| {
        ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "agent_config_path")
            .with_context("detail", reason)
    })
}

impl Browser {
    /// The Claude Code configuration of `commit` (module docs): at most
    /// `max_files` files and `max_bytes` bytes (the lock included).
    pub async fn agent_config(
        &self,
        repo: RepoId,
        commit: &str,
        max_files: usize,
        max_bytes: u64,
    ) -> Result<AgentConfigListing, ForgeError> {
        let mut out = AgentConfigListing::default();
        let root = self.tree(repo, commit, &RepoPath::root()).await?;
        if root.truncated {
            out.unreadable.push(("tree_unreadable", "/".to_owned()));
            return Ok(out);
        }
        let mut listed: Vec<ListedBlob> = Vec::new();
        let mut lock: Option<ListedBlob> = None;
        for entry in &root.entries {
            if entry.name == ".gitstation" && entry.kind == EntryKind::Tree {
                let found = self
                    .part_entries(repo, commit, &dir_path(".gitstation")?, max_files)
                    .await?;
                if found.truncated {
                    out.unreadable
                        .push(("tree_unreadable", ".gitstation".to_owned()));
                }
                for e in found.entries {
                    let path = format!(".gitstation/{}", e.path);
                    if path != LOCK_PATH {
                        continue;
                    }
                    match (e.kind, e.size) {
                        (EntryKind::Blob, Some(size)) => {
                            lock = Some(ListedBlob {
                                path,
                                oid: e.oid,
                                size,
                            });
                        }
                        _ => out.unreadable.push(("not_a_regular_file", path)),
                    }
                }
                continue;
            }
            let Some(is_dir) = managed_root(&entry.name) else {
                continue;
            };
            match (is_dir, entry.kind) {
                (true, EntryKind::Tree) => {
                    let found = self
                        .part_entries(repo, commit, &dir_path(&entry.name)?, max_files)
                        .await?;
                    if found.truncated {
                        out.unreadable.push(("tree_unreadable", entry.name.clone()));
                        // clone: the report owns it
                    }
                    for e in found.entries {
                        let path = format!("{}/{}", entry.name, e.path);
                        match (e.kind, e.size) {
                            (EntryKind::Blob, Some(size)) => listed.push(ListedBlob {
                                path,
                                oid: e.oid,
                                size,
                            }),
                            _ => out.unreadable.push(("not_a_regular_file", path)),
                        }
                    }
                }
                (false, EntryKind::Blob) => listed.push(ListedBlob {
                    path: entry.name.clone(), // clone: the listing owns its path
                    oid: entry.oid.clone(),   // clone: as above
                    size: entry.blob_size()?,
                }),
                _ => out
                    .unreadable
                    .push(("not_a_regular_file", entry.name.clone())), // clone: as above
            }
        }
        if out.bounded() {
            return Ok(out);
        }
        if listed.len() > max_files {
            out.unreadable
                .push(("too_many_files", listed.len().to_string()));
            return Ok(out);
        }
        let total = listed
            .iter()
            .chain(lock.iter())
            .map(|b| b.size)
            .fold(0u64, u64::saturating_add);
        if total > max_bytes {
            out.unreadable.push(("too_large", total.to_string()));
            return Ok(out);
        }
        let has_lock = lock.is_some();
        listed.extend(lock);
        let mut contents = self.read_blobs(repo, &listed).await?;
        if has_lock {
            out.lock = contents.pop();
            listed.pop();
        }
        out.files = listed.into_iter().map(|b| b.path).zip(contents).collect();
        Ok(out)
    }
}
