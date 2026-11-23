//! The search index's shared vocabulary (plan 20 K2, ADR-0040): what kind
//! of thing an entry is. forge-db stores it (`search_entries.source_kind`),
//! forge-web filters and decides hits by it.

/// What one index entry is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SourceKind {
    /// A project: its path, name and description.
    Project,
    /// A merge request: its title and description (never the diff).
    MergeRequest,
}

impl SourceKind {
    /// Every kind, in the order the API documents them.
    pub const ALL: [SourceKind; 2] = [SourceKind::Project, SourceKind::MergeRequest];

    /// The stored and the API name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::MergeRequest => "merge_request",
        }
    }

    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.as_str() == raw)
    }
}

/// What narrows the candidates of a query (never widens: every returned
/// row is still decided with `authorize()`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CandidateFilter {
    /// Only these kinds (empty: every kind).
    pub kinds: Vec<SourceKind>,
    /// Only this project.
    pub project: Option<crate::ProjectId>,
    /// Leave out `confidential` rows (an AI audience, plan 06 D-5: they
    /// are neither shown nor counted).
    pub exclude_confidential: bool,
}

/// One ranked candidate of a query, as stored: shown only after
/// `authorize()` allowed its project (and kind).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchCandidate {
    pub kind: SourceKind,
    pub project: crate::ProjectId,
    /// `''` for a project; a merge request's iid (decimal).
    pub key: String,
    pub title: String,
    /// The stored body (the snippet's source).
    pub body: String,
    /// `public`, `internal` or `confidential`.
    pub classification: String,
    /// The body was cut when it was indexed.
    pub truncated: bool,
    /// The source's change time, UTC, RFC 3339.
    pub updated_at: String,
}

/// The search's reads, for `GET /api/v1/search` (plan 20 K3). forge-db
/// implements it and `forge serve` injects it (forge-web never depends on
/// forge-db, CLAUDE.md §3-9). **It decides nothing**: the candidates are
/// every matching row whatever the viewer may see (narrowed only by
/// `filter`), and the project records carry the viewer's raw access facts
/// for `authorize()`. The caller bounds each call with its own timeout.
pub trait SearchReader: Send + Sync {
    /// The ranked candidates of `tsquery` (a
    /// [`crate::search_text::SearchQuery::tsquery_literal`]) after
    /// skipping `offset`, at most `max_rows`. One statement under the
    /// search's own statement timeout; errors are store failures
    /// (`FG-DB-…`).
    fn candidates<'a>(
        &'a self,
        tsquery: &'a str,
        filter: &'a CandidateFilter,
        offset: u32,
        max_rows: u32,
    ) -> crate::namespace_resolver::LookupFuture<'a, Vec<SearchCandidate>>;

    /// The projects of `ids` that exist, each with `viewer`'s access facts
    /// (one statement, as the project list reads its rows).
    fn project_records<'a>(
        &'a self,
        ids: &'a [crate::ProjectId],
        viewer: crate::UserId,
    ) -> crate::namespace_resolver::LookupFuture<'a, Vec<crate::ProjectRecord>>;
}
