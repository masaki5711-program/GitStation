//! Which open merge requests a landing can affect (plan 18 S4, ADR-0037
//! (c)): the changed-path sets and [`may_interact`]. Pure — no I/O.
//!
//! A landing on a target branch used to re-check the mergeability of every
//! open merge request targeting it. With the path sets, only the merge
//! requests whose changes **may interact** with what landed are checked
//! again; the result of the others is carried forward to the new target
//! tip (forge-db `review::refresh`). The carried answer gates only what is
//! shown and what may be enqueued: the merge itself (the train's
//! `merge-tree`, the direct merge's trial merge) rebuilds the combined
//! result before anything lands, so a wrong "does not interact" costs a
//! late conflict message (`FG-REVIEW-0006`), never a bad target.
//!
//! # The rule (version [`RULE_VERSION`])
//!
//! [`may_interact`] answers **true** (check again) when
//!
//! 1. either set is missing or incomplete (see below), or
//! 2. both changed the same file, or
//! 3. a file of one side is a directory of the other (`a` and `a/b`), or
//! 4. one side deleted something under a directory the other side changed
//!    something under (a directory deletion or rename against additions
//!    below it — both directions; the deleted side lists **every** ancestor
//!    directory of every deleted path, so a single deletion in `docs/`
//!    checks again every merge request touching `docs/`: wider than
//!    needed, never narrower).
//!
//! Two changes under the same directory that share no file and delete
//! nothing there do not interact.
//!
//! # Keys
//!
//! Paths are compared as **keys**: the path split on `/`, each character
//! case-folded (`to_uppercase` then `to_lowercase`), so `Docs/A.md` and
//! `docs/a.md` are the same key. git's own merge is case-sensitive; the
//! fold makes a case-only difference an interaction, for the checkouts on
//! case-insensitive file systems. Unicode normalisation (NFC / NFD) is
//! **not** applied: two spellings of one visible name are different keys.
//!
//! # Incomplete sets
//!
//! A set that cannot be trusted to list everything is **incomplete**, and
//! an incomplete set interacts with everything ([`PathSet::build`]
//! answers the reason, for the log):
//!
//! - more than the cap of files, directories or deleted directories
//!   (`review.paths_max`), or more than [`SET_BYTES_MAX`] bytes of keys;
//! - a path git printed that is not UTF-8, or whose listing was cut short
//!   (the callers map both to [`Incomplete`]);
//! - a symbolic link or a submodule on either side of a change;
//! - a path with an empty, `.` or `..` component, or a NUL;
//! - a **global** path — one that changes how other paths merge or what
//!   the repository's rules are: `.gitattributes` and `.gitmodules` in any
//!   directory, anything under `.forge/`, and `.forge-ci.yml` and
//!   `CODEOWNERS` at the root.
//!
//! An incomplete set is stored and sent with empty lists.
//!
//! # The SQL twin
//!
//! forge-db's carry-forward statement decides the same rule in SQL over
//! the stored columns (`review::mergeability::paths`); a property test
//! there proves the two agree on every input, including sets that never
//! went through [`PathSet::build`] ([`PathSet::from_stored`]).

use std::collections::BTreeSet;

use crate::{ObjectId, RefName};

/// The version of the rule and of the key format. A stored or sent set of
/// another version is unknown (checked again).
pub const RULE_VERSION: u32 = 1;

/// The largest `review.paths_max`: no stored list is longer (the table's
/// constraint says the same).
pub const PATHS_MAX_LIMIT: usize = 20_000;

/// The bytes of keys one set may hold; above it the set is incomplete.
pub const SET_BYTES_MAX: usize = 1024 * 1024;

/// The serialized bytes of the `landed` block of a `repository.pushed`
/// payload; a larger landing is sent incomplete (its lists empty).
pub const LANDED_BUDGET: usize = 16 * 1024;

/// The payload key of the landed block.
pub const LANDED_KEY: &str = "landed";

/// One changed path of a diff without rename detection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedPath {
    /// As git printed it (`/`-separated, no quoting).
    pub path: String,
    /// The path does not exist on the new side.
    pub deleted: bool,
    /// A symbolic link or a submodule on either side.
    pub special: bool,
}

/// Why a path set is incomplete (the `reason` of the log line).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Incomplete {
    /// More paths than the cap.
    TooMany,
    /// More than [`SET_BYTES_MAX`] bytes of keys, or a listing longer
    /// than the bytes read of it at most.
    TooLarge,
    /// A path that changes how other paths merge.
    Global,
    /// A symbolic link or a submodule.
    Special,
    /// A path with an empty, `.` or `..` component, or a NUL.
    Unusual,
    /// A path that is not UTF-8.
    NonUtf8,
    /// The listing could not be read.
    Unread,
    /// The two sides have more than one merge base: the merge works from
    /// a virtual base, which no single diff describes.
    MergeBases,
}

impl Incomplete {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::TooMany => "too_many",
            Self::TooLarge => "too_large",
            Self::Global => "global_path",
            Self::Special => "special_entry",
            Self::Unusual => "unusual_path",
            Self::NonUtf8 => "non_utf8",
            Self::Unread => "unread",
            Self::MergeBases => "several_merge_bases",
        }
    }
}

/// The keys one change touches: its files, every ancestor directory of
/// them, and every ancestor directory of what it deleted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathSet {
    files: Vec<String>,
    dirs: Vec<String>,
    deleted_dirs: Vec<String>,
    complete: bool,
}

impl PathSet {
    /// The set that interacts with everything.
    pub fn incomplete() -> Self {
        Self {
            files: Vec::new(),
            dirs: Vec::new(),
            deleted_dirs: Vec::new(),
            complete: false,
        }
    }

    /// The complete set of `entries`, or why there is none (module docs).
    /// `max` caps each of the three lists.
    pub fn build<I>(entries: I, max: usize) -> Result<Self, Incomplete>
    where
        I: IntoIterator<Item = ChangedPath>,
    {
        let max = max.min(PATHS_MAX_LIMIT);
        let mut files = BTreeSet::new();
        let mut dirs = BTreeSet::new();
        let mut deleted_dirs = BTreeSet::new();
        let mut bytes = 0_usize;
        for entry in entries {
            if entry.special {
                return Err(Incomplete::Special);
            }
            let parts = key_parts(&entry.path).ok_or(Incomplete::Unusual)?;
            if is_global(&parts) {
                return Err(Incomplete::Global);
            }
            for depth in 1..parts.len() {
                let dir = parts[..depth].join("/");
                if entry.deleted && deleted_dirs.insert(dir.clone()) {
                    // clone: the same key goes in both lists
                    bytes += dir.len();
                }
                if !dirs.contains(&dir) {
                    bytes += dir.len();
                    dirs.insert(dir);
                }
            }
            let file = parts.join("/");
            if !files.contains(&file) {
                bytes += file.len();
                files.insert(file);
            }
            if files.len() > max || dirs.len() > max || deleted_dirs.len() > max {
                return Err(Incomplete::TooMany);
            }
            if bytes > SET_BYTES_MAX {
                return Err(Incomplete::TooLarge);
            }
        }
        Ok(Self {
            files: files.into_iter().collect(),
            dirs: dirs.into_iter().collect(),
            deleted_dirs: deleted_dirs.into_iter().collect(),
            complete: true,
        })
    }

    /// A set as it was stored or sent: the lists are taken as they are
    /// (they were keys when they were written; nothing is folded again).
    pub fn from_stored(
        files: Vec<String>,
        dirs: Vec<String>,
        deleted_dirs: Vec<String>,
        complete: bool,
    ) -> Self {
        Self {
            files,
            dirs,
            deleted_dirs,
            complete,
        }
    }

    /// The set of a row's columns; `None` (unknown) when any of them is
    /// null.
    pub fn from_columns(
        files: Option<Vec<String>>,
        dirs: Option<Vec<String>>,
        deleted_dirs: Option<Vec<String>>,
        complete: Option<bool>,
    ) -> Option<Self> {
        Some(Self::from_stored(files?, dirs?, deleted_dirs?, complete?))
    }

    pub fn files(&self) -> &[String] {
        &self.files
    }

    pub fn dirs(&self) -> &[String] {
        &self.dirs
    }

    pub fn deleted_dirs(&self) -> &[String] {
        &self.deleted_dirs
    }

    pub fn is_complete(&self) -> bool {
        self.complete
    }
}

/// The key components of `path`; `None` for a path no key stands for.
fn key_parts(path: &str) -> Option<Vec<String>> {
    if path.is_empty() || path.contains('\0') {
        return None;
    }
    let mut parts = Vec::new();
    for part in path.split('/') {
        if part.is_empty() || part == "." || part == ".." {
            return None;
        }
        parts.push(
            part.chars()
                .flat_map(char::to_uppercase)
                .flat_map(char::to_lowercase)
                .collect::<String>(),
        );
    }
    Some(parts)
}

/// Whether the key `parts` name a path that changes how other paths merge
/// or what the repository's rules are (module docs).
fn is_global(parts: &[String]) -> bool {
    let (Some(first), Some(last)) = (parts.first(), parts.last()) else {
        return false;
    };
    last == ".gitattributes"
        || last == ".gitmodules"
        || first == ".forge"
        || (parts.len() == 1 && (first == ".forge-ci.yml" || first == "codeowners"))
}

/// Whether the change `mr` may interact with the landed change `landed`
/// (module docs: the rule). `None` is an unknown set.
pub fn may_interact(mr: Option<&PathSet>, landed: Option<&PathSet>) -> bool {
    let (Some(mr), Some(landed)) = (mr, landed) else {
        return true;
    };
    if !mr.complete || !landed.complete {
        return true;
    }
    overlap(&mr.files, &landed.files)
        || overlap(&mr.files, &landed.dirs)
        || overlap(&mr.dirs, &landed.files)
        || overlap(&mr.dirs, &landed.deleted_dirs)
        || overlap(&mr.deleted_dirs, &landed.dirs)
}

/// Whether the two lists share an element (exact strings).
fn overlap(a: &[String], b: &[String]) -> bool {
    let (small, large) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    let set: BTreeSet<&str> = large.iter().map(String::as_str).collect();
    small.iter().any(|s| set.contains(s.as_str()))
}

/// What one landing changed on a target branch: the `landed` block of the
/// `repository.pushed` payload of a merge (queue or direct). A queue batch
/// sends the same block — the whole batch's range — with each of its
/// entries' events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LandedPaths {
    /// The branch that moved (`refs/heads/…`).
    pub target: RefName,
    /// The target's tip before the landing.
    pub old: ObjectId,
    /// The target's tip after it.
    pub new: ObjectId,
    /// The paths of `old..new`.
    pub paths: PathSet,
}

/// What a `repository.pushed` payload says about the landed paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Landed {
    /// No block: a direct push, or a payload written before the path sets.
    Absent,
    /// A block that cannot be used, and why (`rule`, `malformed`,
    /// `incomplete`, `oversized`): every merge request is checked again.
    Unusable(&'static str),
    /// A complete set under the current rule.
    Usable(LandedPaths),
}

impl LandedPaths {
    /// `{"rule","ref","old","new","paths_complete","changed_files",
    /// "changed_dirs","deleted_parent_dirs"}`, at most [`LANDED_BUDGET`]
    /// serialized bytes: a larger set is sent incomplete, its lists empty.
    pub fn to_value(&self) -> serde_json::Value {
        let full = self.value_of(&self.paths);
        if !self.paths.complete || full.to_string().len() <= LANDED_BUDGET {
            return full;
        }
        self.value_of(&PathSet::incomplete())
    }

    fn value_of(&self, paths: &PathSet) -> serde_json::Value {
        let empty: &[String] = &[];
        let (files, dirs, deleted) = if paths.complete {
            (paths.files(), paths.dirs(), paths.deleted_dirs())
        } else {
            (empty, empty, empty)
        };
        serde_json::json!({
            "rule": RULE_VERSION,
            "ref": self.target.as_str(),
            "old": self.old.as_str(),
            "new": self.new.as_str(),
            "paths_complete": paths.complete,
            "changed_files": files,
            "changed_dirs": dirs,
            "deleted_parent_dirs": deleted,
        })
    }

    /// The landed block of a `repository.pushed` payload. The budget
    /// [`LandedPaths::to_value`] keeps is enforced here as well: a block
    /// of more than [`LANDED_BUDGET`] serialized bytes — whoever wrote it
    /// — is `oversized`, so the statement that compares it with every
    /// candidate row never gets more than the budget's worth of keys.
    pub fn read(payload: &serde_json::Value) -> Landed {
        let Some(block) = payload.get(LANDED_KEY) else {
            return Landed::Absent;
        };
        if block.is_null() {
            return Landed::Absent;
        }
        if block.to_string().len() > LANDED_BUDGET {
            return Landed::Unusable("oversized");
        }
        if block.get("rule").and_then(serde_json::Value::as_u64) != Some(u64::from(RULE_VERSION)) {
            return Landed::Unusable("rule");
        }
        let text = |key: &str| block.get(key).and_then(serde_json::Value::as_str);
        let (Some(target), Some(old), Some(new)) = (text("ref"), text("old"), text("new")) else {
            return Landed::Unusable("malformed");
        };
        let (Ok(target), Ok(old), Ok(new)) = (
            RefName::parse(target),
            ObjectId::parse(old),
            ObjectId::parse(new),
        ) else {
            return Landed::Unusable("malformed");
        };
        match block.get("paths_complete") {
            Some(serde_json::Value::Bool(true)) => {}
            Some(serde_json::Value::Bool(false)) => return Landed::Unusable("incomplete"),
            _ => return Landed::Unusable("malformed"),
        }
        let list = |key: &str| -> Option<Vec<String>> {
            let items = block.get(key)?.as_array()?;
            if items.len() > PATHS_MAX_LIMIT {
                return None;
            }
            items
                .iter()
                .map(|v| v.as_str().map(str::to_owned))
                .collect()
        };
        let (Some(files), Some(dirs), Some(deleted)) = (
            list("changed_files"),
            list("changed_dirs"),
            list("deleted_parent_dirs"),
        ) else {
            return Landed::Unusable("malformed");
        };
        Landed::Usable(Self {
            target,
            old,
            new,
            paths: PathSet::from_stored(files, dirs, deleted, true),
        })
    }
}
