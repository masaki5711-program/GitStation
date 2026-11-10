//! The requirement index of one commit, and what is decided from it (M3
//! X4, plan 03 §4–§5, plan 06 D-7, ADR-0023 Amendment X4). Pure: forge-checks
//! reads the files with git and feeds them here.
//!
//! - [`SpecIndex`]: the specification documents of a repository's own
//!   specification directories (`spec_doc`) and every `@req` tag of its
//!   tree (`req_tags`), with what was left out when a bound was hit
//!   ([`SpecIndex::incomplete`]).
//! - [`lint`] (`forge/spec-lint`): every document's problems, an id defined
//!   twice (`duplicate_id`), a document name used twice
//!   (`duplicate_spec`), and a tag naming an id of one of **this
//!   repository's own families** that it does not define and no `repo:`
//!   entry's scope covers (`broken_reference`). A tag of another family is
//!   no repository's to judge here (spec-trace calls it `unknown`).
//! - [`diff`]: requirement by requirement between two indexes (added,
//!   changed — body or priority —, removed, deprecated).
//! - [`trace_merge_request`] (`forge/spec-trace`, advisory): the
//!   requirements a merge request touches — added or changed in its own
//!   specification, named by a tag it adds, or the criteria of a work item
//!   it is linked to — and whether a `@req` test names each at its head.
//! - [`trace_table`] (`GET …/specs/trace`): each requirement with the tests
//!   naming it.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use crate::repo_config::{RepoConfig, ScopePattern, SpecSource};
use crate::req_tags::{scan_line, CriterionId, TagScan, TagTarget};
use crate::spec_doc::{parse_spec_document, Requirement, RequirementId, SpecDocument};
use crate::work_item::WorkItemRef;

/// One indexed specification document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexedDocument {
    pub path: String,
    pub doc: SpecDocument,
}

/// One id of a `@req` tag, where it was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexedTag {
    pub path: String,
    pub line: usize,
    pub target: TagTarget,
}

/// What followed a tag but is no id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MalformedTag {
    pub path: String,
    pub line: usize,
    pub raw: String,
}

/// The documents and tags of one commit (module docs).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SpecIndex {
    /// In path order as added.
    pub documents: Vec<IndexedDocument>,
    pub tags: Vec<IndexedTag>,
    pub malformed: Vec<MalformedTag>,
    /// The first bound that left something out (`spec_files`,
    /// `spec_bytes`, `tag_lines`): the answers are about what was read.
    pub incomplete: Option<&'static str>,
}

/// A requirement and the document it is defined in.
#[derive(Debug, Clone, Copy)]
pub struct Located<'a> {
    pub path: &'a str,
    pub requirement: &'a Requirement,
}

impl SpecIndex {
    /// Parse `bytes` (the file at `path`) and keep it when it is a
    /// specification document.
    pub fn add_document(&mut self, path: &str, bytes: &[u8]) {
        let doc = parse_spec_document(bytes);
        if doc.is_spec() {
            self.documents.push(IndexedDocument {
                path: path.to_owned(),
                doc,
            });
        }
    }

    /// Keep a document that could not be read (too large): `rule` on line 0.
    pub fn add_unreadable(&mut self, path: &str, rule: &'static str) {
        let mut doc = SpecDocument::default();
        doc.problems
            .push(crate::spec_doc::SpecProblem { line: 0, rule });
        self.documents.push(IndexedDocument {
            path: path.to_owned(),
            doc,
        });
    }

    /// Scan one line (`line` in `path`) for tags.
    pub fn add_tag_line(&mut self, path: &str, line: usize, text: &str) {
        for scan in scan_line(text) {
            match scan {
                TagScan::Target(target) => self.tags.push(IndexedTag {
                    path: path.to_owned(),
                    line,
                    target,
                }),
                TagScan::Malformed(raw) => self.malformed.push(MalformedTag {
                    path: path.to_owned(),
                    line,
                    raw,
                }),
            }
        }
    }

    /// Apply the repository's `spec.system` type (`.forge/doctypes/
    /// spec.system.yml`, read by whoever builds the index) to every
    /// document: a stricter default withholds them all from an AI
    /// ([`crate::spec_doc::SpecDocument::apply_type_default`]). Call it once
    /// the documents are added.
    pub fn apply_spec_system_type(&mut self, ty: forge_doc::document::TypeDefault) {
        for d in &mut self.documents {
            d.doc.apply_type_default(ty);
        }
    }

    /// Mark the index incomplete (the first reason is kept).
    pub fn left_out(&mut self, reason: &'static str) {
        self.incomplete.get_or_insert(reason);
    }

    /// The documents an AI agent is not given (M3 X5, plan 06 D-5:
    /// [`SpecDocument::confidential`]), by path.
    pub fn confidential_paths(&self) -> std::collections::BTreeSet<&str> {
        self.documents
            .iter()
            .filter(|d| d.doc.confidential())
            .map(|d| d.path.as_str())
            .collect()
    }

    /// Each requirement id with its first definition, by id.
    pub fn requirements(&self) -> Vec<Located<'_>> {
        self.by_id().into_values().collect()
    }

    fn by_id(&self) -> BTreeMap<&RequirementId, Located<'_>> {
        let mut out = BTreeMap::new();
        for d in &self.documents {
            for r in &d.doc.requirements {
                out.entry(&r.id).or_insert(Located {
                    path: &d.path,
                    requirement: r,
                });
            }
        }
        out
    }

    /// The tags naming each target (one pass; the trace looks many up).
    fn tags_by_target(&self) -> HashMap<&TagTarget, Vec<&IndexedTag>> {
        let mut out: HashMap<&TagTarget, Vec<&IndexedTag>> = HashMap::new();
        for t in &self.tags {
            out.entry(&t.target).or_default().push(t);
        }
        out
    }

    /// The families this repository numbers requirements in.
    fn families(&self) -> HashSet<&str> {
        self.documents
            .iter()
            .flat_map(|d| d.doc.requirements.iter().map(|r| r.id.family()))
            .collect()
    }
}

/// The ids `.forge/repo.yml`'s `repo:` entries take from elsewhere: their
/// `scope:` patterns, or every id when an entry states no scope.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExternalScope {
    pub all: bool,
    pub patterns: Vec<ScopePattern>,
}

impl ExternalScope {
    pub fn of(config: &RepoConfig) -> Self {
        let mut scope = Self::default();
        for entry in &config.specs {
            if let SpecSource::Repo { .. } = entry.source {
                if entry.scope.is_empty() {
                    scope.all = true;
                }
                scope.patterns.extend(entry.scope.iter().cloned()); // clone: the scope owns its patterns
            }
        }
        scope
    }

    /// Every id (a file that cannot be read may refer to anything).
    pub fn everything() -> Self {
        Self {
            all: true,
            patterns: Vec::new(),
        }
    }

    pub fn covers(&self, id: &str) -> bool {
        self.all || self.patterns.iter().any(|p| p.matches(id))
    }
}

// ── spec-lint ──────────────────────────────────────────────────────────

/// One rule broken (`FG-REPO-0028` lists the rules).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LintFinding {
    pub path: String,
    /// 1-based; 0 for the whole file.
    pub line: usize,
    pub rule: &'static str,
    /// The id or name concerned (`duplicate_*`, `broken_reference`).
    pub detail: Option<String>,
}

/// `forge/spec-lint` (module docs), by path and line.
pub fn lint(index: &SpecIndex, external: &ExternalScope) -> Vec<LintFinding> {
    let mut found = Vec::new();
    for d in &index.documents {
        for p in &d.doc.problems {
            found.push(LintFinding {
                path: d.path.clone(), // clone: the finding owns its path
                line: p.line,
                rule: p.rule,
                detail: None,
            });
        }
    }
    let mut first_id: BTreeMap<&RequirementId, (&str, usize)> = BTreeMap::new();
    let mut first_spec: BTreeMap<&str, &str> = BTreeMap::new();
    for d in &index.documents {
        if let Some(meta) = &d.doc.meta {
            if let Some(first) = first_spec.get(meta.spec.as_str()) {
                found.push(LintFinding {
                    path: d.path.clone(), // clone: as above
                    line: meta.spec_line,
                    rule: "duplicate_spec",
                    detail: Some(format!("{} (first in {first})", meta.spec)),
                });
            } else {
                first_spec.insert(&meta.spec, &d.path);
            }
        }
        for r in &d.doc.requirements {
            match first_id.get(&r.id) {
                Some((path, line)) => found.push(LintFinding {
                    path: d.path.clone(), // clone: as above
                    line: r.line,
                    rule: "duplicate_id",
                    detail: Some(format!("{} (first at {path}:{line})", r.id)),
                }),
                None => {
                    first_id.insert(&r.id, (&d.path, r.line));
                }
            }
        }
    }
    let families = index.families();
    for t in &index.tags {
        let TagTarget::Requirement(id) = &t.target else {
            continue;
        };
        let own = families.contains(id.family());
        if own && !first_id.contains_key(id) && !external.covers(id.as_str()) {
            found.push(LintFinding {
                path: t.path.clone(), // clone: as above
                line: t.line,
                rule: "broken_reference",
                detail: Some(id.as_str().to_owned()),
            });
        }
    }
    found.sort_by(|a, b| (&a.path, a.line).cmp(&(&b.path, b.line)));
    found
}

// ── the requirement diff ───────────────────────────────────────────────

/// How a requirement changed between two indexes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    Added,
    /// Its body or priority changed.
    Changed,
    /// Its id is gone (ids are deprecated, never removed: plan 03 §4.2).
    Removed,
    /// It became deprecated.
    Deprecated,
}

impl ChangeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Changed => "changed",
            Self::Removed => "removed",
            Self::Deprecated => "deprecated",
        }
    }
}

/// One requirement's change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequirementChange {
    pub id: RequirementId,
    pub kind: ChangeKind,
    /// Where it is defined in `to` (in `from` when removed).
    pub path: String,
    pub line: usize,
}

/// Requirement by requirement from `from` to `to`, by id. A requirement
/// that moved to another file or line without a change is none.
pub fn diff(from: &SpecIndex, to: &SpecIndex) -> Vec<RequirementChange> {
    let before = from.by_id();
    let after = to.by_id();
    let mut out = Vec::new();
    let change = |id: &RequirementId, kind, at: &Located<'_>| RequirementChange {
        id: id.clone(), // clone: the change owns its id
        kind,
        path: at.path.to_owned(),
        line: at.requirement.line,
    };
    let ids: BTreeSet<&RequirementId> = before.keys().chain(after.keys()).copied().collect();
    for id in ids {
        match (before.get(id), after.get(id)) {
            (None, Some(a)) => out.push(change(id, ChangeKind::Added, a)),
            (Some(b), None) => out.push(change(id, ChangeKind::Removed, b)),
            (Some(b), Some(a)) => {
                let (old, new) = (b.requirement, a.requirement);
                if !old.deprecated && new.deprecated {
                    out.push(change(id, ChangeKind::Deprecated, a));
                } else if old.body != new.body || old.priority != new.priority {
                    out.push(change(id, ChangeKind::Changed, a));
                }
            }
            (None, None) => {}
        }
    }
    out
}

// ── spec-trace ─────────────────────────────────────────────────────────

/// What a trace row is about.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum TraceSubject {
    Requirement(RequirementId),
    Criterion(CriterionId),
    /// A linked work item: covered by any test of one of its criteria.
    WorkItem(WorkItemRef),
}

impl TraceSubject {
    fn key(&self) -> String {
        self.to_string()
    }
}

impl std::fmt::Display for TraceSubject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Requirement(r) => r.fmt(f),
            Self::Criterion(c) => c.fmt(f),
            Self::WorkItem(w) => w.fmt(f),
        }
    }
}

/// Why the merge request touches a subject.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TouchReason {
    /// Added or changed in this repository's own specification.
    SpecChanged,
    /// A `@req` tag naming it was added.
    TagAdded,
    /// The merge request is linked to the work item.
    WorkItemLinked,
}

impl TouchReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SpecChanged => "spec_changed",
            Self::TagAdded => "tag_added",
            Self::WorkItemLinked => "work_item_linked",
        }
    }
}

/// Whether a touched subject has a test at the head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TraceState {
    /// This many tags name it.
    Tested(usize),
    /// No tag names it (`FG-REPO-0029`).
    Untested,
    /// A tag names an id nothing here defines or takes from a referenced
    /// specification (`FG-REPO-0030`).
    Unknown,
}

impl TraceState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Tested(_) => "tested",
            Self::Untested => "untested",
            Self::Unknown => "unknown",
        }
    }
}

/// One touched subject.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceRow {
    pub subject: TraceSubject,
    /// In [`TouchReason`] order, each once.
    pub reasons: Vec<TouchReason>,
    pub state: TraceState,
    /// Where to look: the definition (spec changed) or the first added tag.
    pub at: Option<(String, usize)>,
}

/// What a merge request touches (module docs).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MrTrace {
    /// By subject.
    pub rows: Vec<TraceRow>,
    /// Malformed tags the merge request adds.
    pub malformed: Vec<MalformedTag>,
    /// The base's or head's index was incomplete (its reason).
    pub incomplete: Option<&'static str>,
}

impl MrTrace {
    /// Nothing to point out: every touched subject is tested, no added tag
    /// is malformed, and both indexes are complete.
    pub fn advisory_clear(&self) -> bool {
        self.incomplete.is_none()
            && self.malformed.is_empty()
            && self
                .rows
                .iter()
                .all(|r| matches!(r.state, TraceState::Tested(_)))
    }
}

/// `forge/spec-trace` for a merge request whose merge base is indexed as
/// `base` and head as `head`, linked to `linked` (module docs).
pub fn trace_merge_request(
    base: &SpecIndex,
    head: &SpecIndex,
    linked: &[WorkItemRef],
    external: &ExternalScope,
) -> MrTrace {
    let mut rows: BTreeMap<String, TraceRow> = BTreeMap::new();
    let mut touch = |subject: TraceSubject, reason, state, at: Option<(String, usize)>| {
        let row = rows.entry(subject.key()).or_insert(TraceRow {
            subject,
            reasons: Vec::new(),
            state,
            at,
        });
        if !row.reasons.contains(&reason) {
            row.reasons.push(reason);
            row.reasons.sort();
        }
    };
    let defined = head.by_id();
    let naming = head.tags_by_target();
    let tests_of = |target: &TagTarget| naming.get(target).map_or(0, Vec::len);
    for c in diff(base, head) {
        if !matches!(c.kind, ChangeKind::Added | ChangeKind::Changed) {
            continue;
        }
        if defined.get(&c.id).is_some_and(|l| l.requirement.deprecated) {
            continue;
        }
        let target = TagTarget::Requirement(c.id.clone()); // clone: the subject owns its id
        let state = match tests_of(&target) {
            0 => TraceState::Untested,
            n => TraceState::Tested(n),
        };
        touch(
            TraceSubject::Requirement(c.id),
            TouchReason::SpecChanged,
            state,
            Some((c.path, c.line)),
        );
    }
    let before: HashSet<(&str, &TagTarget)> = base
        .tags
        .iter()
        .map(|t| (t.path.as_str(), &t.target))
        .collect();
    for t in &head.tags {
        if before.contains(&(t.path.as_str(), &t.target)) {
            continue;
        }
        let tests = tests_of(&t.target);
        let (subject, state) = match &t.target {
            TagTarget::Requirement(id) => {
                let known = defined.contains_key(id) || external.covers(id.as_str());
                let state = if known {
                    TraceState::Tested(tests)
                } else {
                    TraceState::Unknown
                };
                (TraceSubject::Requirement(id.clone()), state) // clone: as above
            }
            TagTarget::Criterion(c) => (
                TraceSubject::Criterion(c.clone()), // clone: as above
                TraceState::Tested(tests),
            ),
        };
        touch(
            subject,
            TouchReason::TagAdded,
            state,
            Some((t.path.clone(), t.line)), // clone: the row owns its place
        );
    }
    for item in linked {
        let tests = head
            .tags
            .iter()
            .filter(|t| matches!(&t.target, TagTarget::Criterion(c) if c.work_item() == item))
            .count();
        let state = match tests {
            0 => TraceState::Untested,
            n => TraceState::Tested(n),
        };
        touch(
            TraceSubject::WorkItem(item.clone()), // clone: as above
            TouchReason::WorkItemLinked,
            state,
            None,
        );
    }
    let before_malformed: HashSet<(&str, &str)> = base
        .malformed
        .iter()
        .map(|m| (m.path.as_str(), m.raw.as_str()))
        .collect();
    let malformed = head
        .malformed
        .iter()
        .filter(|m| !before_malformed.contains(&(m.path.as_str(), m.raw.as_str())))
        .cloned() // clone: the trace owns its findings
        .collect();
    MrTrace {
        rows: rows.into_values().collect(),
        malformed,
        incomplete: base.incomplete.or(head.incomplete),
    }
}

// ── the trace table ────────────────────────────────────────────────────

/// One requirement with the tests naming it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceTableRow {
    pub id: RequirementId,
    pub path: String,
    pub line: usize,
    pub deprecated: bool,
    /// Every tag naming it.
    pub test_count: usize,
    /// The first of them (at most the caller's cap), as path and line.
    pub tests: Vec<(String, usize)>,
}

/// Each requirement of `spec` (within `scope`, when given) with the tags of
/// `tags` naming it, by id; at most `max_tests` places per row.
pub fn trace_table(
    spec: &SpecIndex,
    scope: Option<&[ScopePattern]>,
    tags: &SpecIndex,
    max_tests: usize,
) -> Vec<TraceTableRow> {
    let in_scope = |id: &RequirementId| {
        scope.is_none_or(|patterns| patterns.iter().any(|p| p.matches(id.as_str())))
    };
    let by_target = tags.tags_by_target();
    spec.by_id()
        .into_iter()
        .filter(|(id, _)| in_scope(id))
        .map(|(id, at)| {
            let target = TagTarget::Requirement(id.clone()); // clone: the probe owns its id
            let naming: &[&IndexedTag] = by_target.get(&target).map_or(&[], Vec::as_slice);
            TraceTableRow {
                id: id.clone(), // clone: the row owns its id
                path: at.path.to_owned(),
                line: at.requirement.line,
                deprecated: at.requirement.deprecated,
                test_count: naming.len(),
                tests: naming
                    .iter()
                    .take(max_tests)
                    .map(|t| (t.path.clone(), t.line)) // clone: as above
                    .collect(),
            }
        })
        .collect()
}

#[cfg(test)]
#[path = "spec_index_tests.rs"]
mod tests;
