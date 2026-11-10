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
use std::collections::HashSet;

use forge_git::browse::{BlobListing, Browser, ListedBlob, RepoPath};

use crate::doc_lint::{self, DocLimits, DocTree};
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

fn external_of(config: &RepoConfigRead) -> ExternalScope {
    match config {
        RepoConfigRead::Valid(c) => ExternalScope::of(c),
        RepoConfigRead::Invalid(_) => ExternalScope::everything(),
        RepoConfigRead::Absent => ExternalScope::default(),
    }
}

/// Where the `spec.system` type is defined (ADR-0026 §7: both formats of a
/// specification are that type).
const SPEC_SYSTEM_TYPE_PATH: &str = ".forge/doctypes/spec.system.yml";

/// Read the repository's `spec.system` definition at `commit` and apply
/// its default classification to the index (review 0d59521: a document
/// never reads less confidential than its type). No definition changes
/// nothing; one that cannot be used (not a file, too large, invalid)
/// withholds every document from an AI (fail closed; `forge/doc-lint`
/// names the definition). Errors: git (the browse limits), with the commit.
async fn apply_spec_system_type(
    browse: &Browser,
    repo: RepoId,
    commit: &str,
    index: &mut SpecIndex,
) -> Result<(), ForgeError> {
    use forge_doc::document::TypeDefault;
    use forge_git::browse::SmallFile;
    let path = RepoPath::parse(SPEC_SYSTEM_TYPE_PATH).map_err(|rule| {
        ForgeError::new(forge_errors::ErrorCode::Internal)
            .with_context("reason", "spec_system_type_path_invalid")
            .with_context("rule", rule)
    })?;
    let max = u64::try_from(forge_doc::doctype::MAX_DOCTYPE_BYTES).unwrap_or(u64::MAX);
    let file = browse
        .small_file(repo, commit, &path, max)
        .await
        .map_err(|e| e.with_context("commit", commit.to_owned()))?;
    let ty = match file {
        SmallFile::Absent => return Ok(()),
        SmallFile::NotAFile | SmallFile::TooLarge => TypeDefault::Unresolved,
        SmallFile::Found(bytes) => {
            match forge_doc::doctype::parse_doctype(forge_doc::legacy::SPEC_SYSTEM, &bytes) {
                Ok(t) => TypeDefault::Known(t.default_classification),
                // forge/doc-lint reports the definition (doctype_invalid).
                Err(_) => TypeDefault::Unresolved,
            }
        }
    };
    index.apply_spec_system_type(ty);
    Ok(())
}

/// The own specification directories as git pathspecs (no root, no
/// trailing `/`).
fn dir_paths(dirs: &[String]) -> Vec<RepoPath> {
    dirs.iter()
        .filter_map(|d| RepoPath::parse(d).ok())
        .collect()
}

/// The Markdown files of `listed` (the files under the specification
/// directories) to read, within the bounds; a bound hit is named in
/// `index` (module docs, 2).
fn select_spec(
    listed: &BlobListing,
    limits: &IndexLimits,
    index: &mut SpecIndex,
) -> Vec<ListedBlob> {
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
    to_read
}

/// Every `@req` line of the tree, outside the specification documents
/// (module docs, 3).
async fn read_tags(
    browse: &Browser,
    repo: RepoId,
    commit: &str,
    limits: &IndexLimits,
    dirs: &[String],
    index: &mut SpecIndex,
) -> Result<(), ForgeError> {
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
    Ok(())
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
    let external = external_of(&config);
    let dirs = own_dirs(&config);
    let paths = dir_paths(&dirs);
    let mut index = SpecIndex::default();
    let listed = browse
        .blobs_under(repo, commit, &paths, limits.max_files.saturating_mul(4))
        .await
        .map_err(|e| e.with_context("commit", commit.to_owned()))?;
    let to_read = select_spec(&listed, limits, &mut index);
    let contents = browse
        .read_blobs(repo, &to_read)
        .await
        .map_err(|e| e.with_context("commit", commit.to_owned()))?;
    for (b, bytes) in to_read.iter().zip(contents) {
        index.add_document(&b.path, &bytes);
    }
    apply_spec_system_type(browse, repo, commit, &mut index).await?;
    // In path order whatever git listed first (the answers are by path).
    index.documents.sort_by(|a, b| a.path.cmp(&b.path));
    if tags == Tags::Read {
        read_tags(browse, repo, commit, limits, &dirs, &mut index).await?;
    }
    Ok(Indexed {
        config,
        dirs,
        index,
        external,
    })
}

/// The files of `whole` (one listing of the whole tree) that
/// [`Browser::blobs_under`] lists for `paths` with the cap `max`: under one
/// of the directories (literal prefixes), in the same order.
fn under_dirs(whole: &BlobListing, paths: &[RepoPath], max: usize) -> BlobListing {
    let dirs: Vec<&str> = paths
        .iter()
        .filter(|d| !d.is_root())
        .map(|d| d.as_str().trim_end_matches('/'))
        .collect();
    let mut out = BlobListing {
        blobs: Vec::new(),
        truncated: whole.truncated,
    };
    let under = whole.blobs.iter().filter(|b| {
        dirs.iter().any(|d| {
            b.path
                .strip_prefix(d)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
        })
    });
    for b in under {
        if out.blobs.len() >= max {
            out.truncated = true;
            break;
        }
        out.blobs.push(b.clone()); // clone: the selection owns its list
    }
    out
}

/// A merge request version's head, read **once** for the
/// `check.spec_index` job (plan 13 stage 2): one listing of the whole tree
/// and one read of the blobs give both the specification index (the same
/// as [`index_commit`] with [`Tags::Read`]: the own directories' files are
/// taken from that listing) and the documents of `forge/doc-lint`
/// ([`crate::doc_lint`]). A listing cut at the tree's bound leaves the
/// documents incomplete (`doc_tree`), never a silent pass; the
/// specification directories are then listed on their own, so the index
/// stays the one [`index_commit`] builds. Errors: as
/// [`index_commit`].
pub async fn index_head(
    browse: &Browser,
    repo: RepoId,
    commit: &str,
    limits: &IndexLimits,
    doc_limits: &DocLimits,
) -> Result<(Indexed, DocTree), ForgeError> {
    let config = read_repo_config(browse, repo, commit).await?;
    let external = external_of(&config);
    let dirs = own_dirs(&config);
    let paths = dir_paths(&dirs);
    let whole = browse
        .blobs_in_tree(repo, commit, doc_limits.max_tree_entries)
        .await
        .map_err(|e| e.with_context("commit", commit.to_owned()))?;
    let mut index = SpecIndex::default();
    let spec_cap = limits.max_files.saturating_mul(4);
    // A whole tree past its bound says nothing about the specification
    // directories: list them on their own as `index_commit` does (review
    // MR3), so forge/spec-lint's answer never depends on the tree's size.
    let spec_listed = if whole.truncated {
        browse
            .blobs_under(repo, commit, &paths, spec_cap)
            .await
            .map_err(|e| e.with_context("commit", commit.to_owned()))?
    } else {
        under_dirs(&whole, &paths, spec_cap)
    };
    let spec = select_spec(&spec_listed, limits, &mut index);
    let (doc_blobs, mut docs) = doc_lint::select(&whole, doc_limits);
    // One read: the specification files, then the documents not among them.
    let spec_paths: HashSet<&str> = spec.iter().map(|b| b.path.as_str()).collect();
    let extra: Vec<&ListedBlob> = doc_blobs
        .whole
        .iter()
        .filter(|b| !spec_paths.contains(b.path.as_str()))
        .collect();
    let mut to_read: Vec<ListedBlob> = spec.clone(); // clone: the read owns its list
    to_read.extend(extra.into_iter().cloned());
    let contents = browse
        .read_blobs(repo, &to_read)
        .await
        .map_err(|e| e.with_context("commit", commit.to_owned()))?;
    let wanted: HashSet<&str> = doc_blobs.whole.iter().map(|b| b.path.as_str()).collect();
    for (i, (b, bytes)) in to_read.iter().zip(contents).enumerate() {
        if i < spec.len() {
            index.add_document(&b.path, &bytes);
        }
        if wanted.contains(b.path.as_str()) {
            docs.add(&b.path, bytes);
        }
    }
    // Files over the parser's limit: only what the parser needs.
    for b in &doc_blobs.prefix {
        let bytes = browse
            .read_blob_prefix(repo, b, doc_lint::read_cap(&b.path))
            .await
            .map_err(|e| e.with_context("commit", commit.to_owned()))?;
        docs.add(&b.path, bytes);
    }
    apply_spec_system_type(browse, repo, commit, &mut index).await?;
    index.documents.sort_by(|a, b| a.path.cmp(&b.path));
    docs.sort();
    read_tags(browse, repo, commit, limits, &dirs, &mut index).await?;
    Ok((
        Indexed {
            config,
            dirs,
            index,
            external,
        },
        docs,
    ))
}
