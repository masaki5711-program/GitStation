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
    /// A document (plan 13) at the default branch's tip (plan 20 K4): its
    /// title, id, headings, block ids and text. Wiki pages and notes are
    /// documents too. Keyed by its path.
    Document,
}

impl SourceKind {
    /// Every kind, in the order the API documents them.
    pub const ALL: [SourceKind; 3] = [
        SourceKind::Project,
        SourceKind::MergeRequest,
        SourceKind::Document,
    ];

    /// The stored and the API name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::MergeRequest => "merge_request",
            Self::Document => "document",
        }
    }

    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.as_str() == raw)
    }
}

/// What a query may match. The readable sets are the caller's decision
/// (`authorize()` on the same access facts as the project list), made
/// **before** the read: the database never ranks, counts or pages a row of
/// a project the caller cannot read, so a short page or `has_more` cannot
/// reveal hidden matches. Every returned row is still decided again with
/// `authorize()`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CandidateFilter {
    /// Only these kinds (empty: every kind).
    pub kinds: Vec<SourceKind>,
    /// Only this project.
    pub project: Option<crate::ProjectId>,
    /// Leave out `confidential` rows (an AI audience, plan 06 D-5: they
    /// are neither shown nor counted).
    pub exclude_confidential: bool,
    /// The projects whose own entry the caller may read (`ProjectRead`).
    /// Empty: no project entry matches.
    pub readable_projects: Vec<crate::ProjectId>,
    /// The projects whose merge requests the caller may read (`ProjectRead`
    /// and `MergeRequestList`). Empty: no merge request matches.
    pub readable_merge_requests: Vec<crate::ProjectId>,
    /// The projects whose documents the caller may read (`ProjectRead` and
    /// `RepoBrowse`). Empty: no document matches.
    pub readable_documents: Vec<crate::ProjectId>,
}

/// One ranked candidate of a query, as stored: shown only after
/// `authorize()` allowed its project (and kind).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchCandidate {
    pub kind: SourceKind,
    pub project: crate::ProjectId,
    /// `''` for a project; a merge request's iid (decimal); a document's
    /// path.
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
/// forge-db, CLAUDE.md §3-9). It decides nothing: it narrows by `filter`,
/// whose readable sets are the caller's `authorize()` decisions. The
/// caller bounds each call with its own timeout.
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
}
