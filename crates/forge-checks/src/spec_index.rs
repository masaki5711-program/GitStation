//! Indexing one commit's requirements and `@req` tags (M3 X4, ADR-0023
//! Amendment X4) — read with git through the browse API and parsed by
//! `forge_core::spec_index`. Shared by `forge/spec-lint`,
//! `forge/spec-trace` (the `check.spec_index` job) and forge-web's spec
//! API (`GET …/specs/…`).
//!
//! 1. `.forge/repo.yml` at the commit ([`crate::spec_refs::read_repo_config`]).
//!    The repository's own specification directories are its `path:`
//!    entries, or `spec/` when it states none (absent, refused, or only
//!    `repo:` entries: plan 03 §4.4's default).
//! 2. The Markdown files under them (`ls-tree -r`; `*.md`, regular files
//!    only), read with `cat-file --batch` and parsed. **Bounds**
//!    ([`IndexLimits`]): at most `max_files` files (`spec_files`), a file
//!    over `max_file_bytes` is not read and is a `file_too_large` problem,
//!    and at most `max_total_bytes` are read in all (`spec_bytes`).
//! 3. Every text line naming `@req` in the whole tree (`git grep -F`, at
//!    most `max_tag_output_bytes` of output: `tag_lines`; its own time
//!    limit `grep_timeout`). Skipped for a referenced specification (only
//!    its documents are read).
//!
//! A bound hit is never a silent pass: [`SpecIndex::incomplete`] names it
//! and every check and answer says so (`FG-REPO-0031`). A git failure is
//! returned (the job retries; the route answers its code).

use std::time::Duration;

use forge_core::repo_config::SpecSource;
use forge_core::req_tags::REQ_TAG;
use forge_core::spec_index::{ExternalScope, SpecIndex};
use forge_core::RepoId;
use forge_errors::ForgeError;
use forge_git::browse::{Browser, ListedBlob, RepoPath};

use crate::spec_refs::{read_repo_config, RepoConfigRead};

/// The specification directory of a repository that states none.
pub const DEFAULT_SPEC_DIR: &str = "spec/";

/// The bounds of one index (module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexLimits {
    /// Markdown files read at most (`spec_files` past it).
    pub max_files: usize,
    /// A larger file is not read (`file_too_large`).
    pub max_file_bytes: u64,
    /// Bytes read in all (`spec_bytes` past it).
    pub max_total_bytes: u64,
    /// `git grep` output read at most (`tag_lines` past it).
    pub max_tag_output_bytes: usize,
    /// One `git grep`.
    pub grep_timeout: Duration,
}

impl Default for IndexLimits {
    fn default() -> Self {
        Self {
            max_files: 1_000,
            max_file_bytes: 256 * 1024,
            max_total_bytes: 8 * 1024 * 1024,
            max_tag_output_bytes: 8 * 1024 * 1024,
            grep_timeout: Duration::from_secs(20),
        }
    }
}

/// Whether the tags are read too.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tags {
    Read,
    /// A referenced specification: its documents only.
    Skip,
}

/// One commit, indexed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Indexed {
    /// `.forge/repo.yml` at the commit.
    pub config: RepoConfigRead,
    /// The own specification directories read (as written).
    pub dirs: Vec<String>,
    pub index: SpecIndex,
    /// The ids the `repo:` entries take from elsewhere (everything when the
    /// file is refused: it may refer to anything).
    pub external: ExternalScope,
}

/// The own specification directories `config` states (module docs, 1).
pub fn own_dirs(config: &RepoConfigRead) -> Vec<String> {
    let stated: Vec<String> = match config {
        RepoConfigRead::Valid(c) => c
            .specs
            .iter()
            .filter_map(|e| match &e.source {
                SpecSource::Path(p) => Some(p.as_str().to_owned()),
                SpecSource::Repo { .. } => None,
            })
            .collect(),
        RepoConfigRead::Absent | RepoConfigRead::Invalid(_) => Vec::new(),
    };
    if stated.is_empty() {
        vec![DEFAULT_SPEC_DIR.to_owned()]
    } else {
        stated
    }
}

fn is_markdown(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.ends_with(".md") || lower.ends_with(".markdown")
}

/// Index `commit` of `repo` (module docs). Errors: git (its codes, the
/// browse limits), with the commit.
pub async fn index_commit(
    browse: &Browser,
    repo: RepoId,
    commit: &str,
    limits: &IndexLimits,
    tags: Tags,
) -> Result<Indexed, ForgeError> {
    let config = read_repo_config(browse, repo, commit).await?;
    let external = match &config {
        RepoConfigRead::Valid(c) => ExternalScope::of(c),
        RepoConfigRead::Invalid(_) => ExternalScope::everything(),
        RepoConfigRead::Absent => ExternalScope::default(),
    };
    let dirs = own_dirs(&config);
    let paths: Vec<RepoPath> = dirs
        .iter()
        .filter_map(|d| RepoPath::parse(d).ok())
        .collect();
    let mut index = SpecIndex::default();
    let listed = browse
        .blobs_under(repo, commit, &paths, limits.max_files.saturating_mul(4))
        .await
        .map_err(|e| e.with_context("commit", commit.to_owned()))?;
    let markdown: Vec<&ListedBlob> = listed
        .blobs
        .iter()
        .filter(|b| is_markdown(&b.path))
        .collect();
    if listed.truncated || markdown.len() > limits.max_files {
        index.left_out("spec_files");
    }
    let mut to_read: Vec<ListedBlob> = Vec::new();
    let mut total = 0u64;
    for b in markdown.into_iter().take(limits.max_files) {
        if b.size > limits.max_file_bytes {
            index.add_unreadable(&b.path, "file_too_large");
            continue;
        }
        if total.saturating_add(b.size) > limits.max_total_bytes {
            index.left_out("spec_bytes");
            break;
        }
        total = total.saturating_add(b.size);
        to_read.push(b.clone()); // clone: the read owns its list
    }
    let contents = browse
        .read_blobs(repo, &to_read)
        .await
        .map_err(|e| e.with_context("commit", commit.to_owned()))?;
    for (b, bytes) in to_read.iter().zip(contents) {
        index.add_document(&b.path, &bytes);
    }
    // In path order whatever git listed first (the answers are by path).
    index.documents.sort_by(|a, b| a.path.cmp(&b.path));
    if tags == Tags::Read {
        let hits = browse
            .grep_fixed(
                repo,
                commit,
                REQ_TAG,
                limits.max_tag_output_bytes,
                limits.grep_timeout,
            )
            .await
            .map_err(|e| e.with_context("commit", commit.to_owned()))?;
        if hits.truncated {
            index.left_out("tag_lines");
        }
        // Review X4 (HIGH-2): `@req` in a specification document (its
        // prose, an example in a code fence) is no test, so Markdown under
        // the own specification directories is never a tag's place.
        let prefixes: Vec<String> = dirs
            .iter()
            .map(|d| format!("{}/", d.trim_end_matches('/')))
            .collect();
        for h in &hits.hits {
            let in_spec =
                is_markdown(&h.path) && prefixes.iter().any(|p| h.path.starts_with(p.as_str()));
            if !in_spec {
                index.add_tag_line(&h.path, h.line, &h.text);
            }
        }
    }
    Ok(Indexed {
        config,
        dirs,
        index,
        external,
    })
}
