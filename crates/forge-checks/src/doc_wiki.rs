//! The linked wiki for `forge/doc-lint` (plan 20 W3, ADR-0040 §5 and
//! Amendment W3): what `[[wiki:ID]]` references are checked against. The
//! lint itself is pure (`forge_doc::lint::lint_with_wiki`); this reads the
//! project's link (`forge.project_wikis`, owner side only) and the wiki
//! project's default-branch tip with the document lint's bounds
//! ([`DocLimits`]).
//!
//! The wiki is read only when a document of the head contains `[[wiki:`.
//! The answers ([`WikiRead::state`], logged as `doc_wiki`):
//!
//! - `unchecked`: no document names the wiki — nothing read;
//! - `not_linked`: the project has no linked wiki (it may be a wiki itself)
//!   — every wiki reference is `wiki_not_linked`;
//! - `linked`: the tip was read (an unborn default branch is an empty
//!   wiki) — references that do not resolve are `wiki_ref_broken`;
//! - `withheld`: the wiki is more private than this project (this project
//!   `internal`, the wiki `private`). The job reads with the system's
//!   rights and the run is read by everyone who reads this project, so a
//!   per-id answer would tell them which wiki pages exist: the references
//!   are not checked and the summary says why — nothing of the wiki is
//!   read;
//! - `bounded`, `not_ready`, `read_failed`: the wiki's tip hit a bound,
//!   its repository is not ready (or the project is gone), or a read failed
//!   (git, the database, a timeout; logged `forge.doc_lint.wiki_unavailable`
//!   with the code and the request id). The references are not checked and
//!   the summary says so. A failure of the wiki never fails the job: the
//!   wiki must not block this project's merges.
//!
//! The run's lines never carry the wiki project's id, path or repository
//! state (they are read by this project's readers). Both rules are
//! warnings (`FG-DOC-0008`).

use std::time::Duration;

use forge_core::namespace_resolver::ProjectAccessLoader;
use forge_core::{ProjectId, RepositoryState, RequestId, Visibility};
use forge_db::Db;
use forge_doc::lint::{Source, WikiIndex, WikiRefs};
use forge_errors::{ErrorCode, ForgeError};
use forge_git::browse::Browser;

use crate::doc_lint::{read_tree, DocLimits, DocTree};

/// The caller name of the wiki's reads (the browse API's per-caller key).
const CALLER: &str = "doc_lint_wiki";

/// What makes the job read the wiki. Matched on the raw bytes, also
/// inside code fences and comments where the parser keeps no reference:
/// over-reading is intentional (one more read, never a missed check).
pub const WIKI_NEEDLE: &[u8] = b"[[wiki:";

/// The summary's line when the wiki is more private than this project.
pub const WITHHELD_NOTE: &str =
    "Wiki の参照は確かめていません（Wiki がこのプロジェクトより非公開のため） / \
     Wiki references ([[wiki:…]]) were not checked: the wiki is more private than this project";

/// What was read of the linked wiki (module docs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WikiRead {
    pub refs: WikiRefs,
    /// `unchecked`, `not_linked`, `linked`, `withheld`, `bounded`,
    /// `not_ready` or `read_failed`.
    pub state: &'static str,
    /// A line of the run's summary when the references were not checked
    /// although a document names the wiki.
    pub note: Option<String>,
}

impl WikiRead {
    fn of(refs: WikiRefs, state: &'static str) -> Self {
        Self {
            refs,
            state,
            note: None,
        }
    }

    /// Nothing read: no document names the wiki.
    pub fn unchecked() -> Self {
        Self::of(WikiRefs::Unchecked, "unchecked")
    }

    /// The references left unchecked, the summary saying why (never the
    /// wiki project's id or state: module docs).
    pub fn left_unchecked(state: &'static str) -> Self {
        let note = match state {
            "withheld" => WITHHELD_NOTE.to_owned(),
            "bounded" => format!(
                "Wiki references ([[wiki:…]]) were not checked: the wiki's pages were read only \
                 in part [{}].",
                ErrorCode::WikiReferenceBroken.id()
            ),
            _ => format!(
                "Wiki references ([[wiki:…]]) were not checked: the wiki could not be read \
                 ({state}) [{}].",
                ErrorCode::WikiReferenceBroken.id()
            ),
        };
        Self {
            refs: WikiRefs::Unchecked,
            state,
            note: Some(note),
        }
    }
}

/// Whether a document of `docs` names the linked wiki ([`WIKI_NEEDLE`]).
pub fn names_wiki(docs: &DocTree) -> bool {
    docs.markdown
        .iter()
        .any(|(_, bytes)| bytes.windows(WIKI_NEEDLE.len()).any(|w| w == WIKI_NEEDLE))
}

/// Whether the wiki is more private than the project (module docs).
pub fn more_private(project: Visibility, wiki: Visibility) -> bool {
    matches!((project, wiki), (Visibility::Internal, Visibility::Private))
}

/// What one read of the wiki is about (for the logs).
#[derive(Debug, Clone, Copy)]
pub struct WikiReadFor<'a> {
    pub project: ProjectId,
    /// The project's effective visibility.
    pub visibility: Visibility,
    pub request_id: &'a RequestId,
}

/// A failed read as the run's answer: `read_failed`, logged once with the
/// code and the request id (module docs).
pub fn settle(read: Result<WikiRead, ForgeError>, about: WikiReadFor<'_>) -> WikiRead {
    match read {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(
                target: "forge_checks",
                event = "forge.doc_lint.wiki_unavailable",
                error_code = e.code().id(),
                error = %e,
                request_id = %about.request_id,
                project_id = about.project.get(),
                "the linked wiki could not be read; wiki references are left unchecked"
            );
            WikiRead::left_unchecked("read_failed")
        }
    }
}

async fn within<T>(
    operation: &'static str,
    limit: Duration,
    work: impl std::future::Future<Output = Result<T, ForgeError>>,
) -> Result<T, ForgeError> {
    tokio::time::timeout(limit, work).await.unwrap_or_else(|_| {
        Err(ForgeError::new(ErrorCode::RequestTimeout)
            .with_context("operation", operation)
            .with_context("timeout_ms", limit.as_millis()))
    })
}

/// The linked wiki of `about.project` for the lint of `docs` (module
/// docs). Never an error: a failure is `read_failed`.
pub async fn read_wiki(
    (db, browse, projects): (&Db, &Browser, &dyn ProjectAccessLoader),
    about: WikiReadFor<'_>,
    docs: &DocTree,
    (limits, lookup_timeout): (&DocLimits, Duration),
) -> WikiRead {
    if !names_wiki(docs) {
        return WikiRead::unchecked();
    }
    let read = read_linked((db, browse, projects), about, (limits, lookup_timeout)).await;
    settle(read, about)
}

async fn read_linked(
    (db, browse, projects): (&Db, &Browser, &dyn ProjectAccessLoader),
    about: WikiReadFor<'_>,
    (limits, lookup_timeout): (&DocLimits, Duration),
) -> Result<WikiRead, ForgeError> {
    let project = about.project;
    let link = within(
        "wiki_link_load",
        lookup_timeout,
        forge_db::wikis::link_of(db, project),
    )
    .await?;
    // A wiki project is the link's other side: it has no wiki of its own.
    let Some(link) = link.filter(|l| l.project == project) else {
        return Ok(WikiRead::of(WikiRefs::NotLinked, "not_linked"));
    };
    let record = within(
        "wiki_project_load",
        lookup_timeout,
        projects.load_project(link.wiki, None),
    )
    .await?;
    let Some(record) = record.filter(|r| r.id == link.wiki) else {
        return Ok(WikiRead::left_unchecked("not_ready"));
    };
    if more_private(about.visibility, record.access.visibility) {
        return Ok(WikiRead::left_unchecked("withheld"));
    }
    let repo = record.repository;
    if repo.state != RepositoryState::Ready {
        return Ok(WikiRead::left_unchecked("not_ready"));
    }
    // Another process pushed to the wiki: never answer from a cached ref.
    browse.invalidate(repo.id);
    let refs = browse.refs(repo.id).await?;
    let Some(branch) = refs.default_branch.as_deref() else {
        return Ok(WikiRead::of(
            WikiRefs::Linked(WikiIndex::default()),
            "linked",
        ));
    };
    let tip = match browse
        .resolve_ref(repo.id, &format!("refs/heads/{branch}"), CALLER)
        .await
    {
        Ok(rev) => rev.commit,
        Err(e) if e.code() == ErrorCode::RepositoryPathNotResolved => {
            return Ok(WikiRead::of(
                WikiRefs::Linked(WikiIndex::default()),
                "linked",
            ))
        }
        Err(e) => return Err(e.with_context("wiki_project_id", link.wiki.get())),
    };
    let tree = read_tree(browse, repo.id, &tip, limits)
        .await
        .map_err(|e| e.with_context("wiki_project_id", link.wiki.get()))?;
    if tree.incomplete.is_some() {
        return Ok(WikiRead::left_unchecked("bounded"));
    }
    let sources: Vec<Source<'_>> = tree
        .markdown
        .iter()
        .map(|(path, bytes)| Source { path, bytes })
        .collect();
    Ok(WikiRead::of(
        WikiRefs::Linked(WikiIndex::build(&sources)),
        "linked",
    ))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)] // tests may fail loudly

    use super::*;

    #[test]
    fn only_a_wiki_reference_makes_the_wiki_read() {
        let mut docs = DocTree::default();
        docs.markdown
            .push(("A-1.md".into(), b"see [[A-2]] and [[g/wiki:X-1]]".to_vec()));
        assert!(!names_wiki(&docs));
        docs.markdown
            .push(("A-3.md".into(), b"see [[wiki:WIKI-0001]]".to_vec()));
        assert!(names_wiki(&docs));
    }

    #[test]
    fn an_unchecked_wiki_says_why_without_its_id() {
        for state in ["bounded", "not_ready", "read_failed"] {
            let r = WikiRead::left_unchecked(state);
            assert_eq!(r.refs, WikiRefs::Unchecked);
            let note = r.note.unwrap_or_default();
            assert!(note.contains("FG-DOC-0008"), "{note}");
            assert!(!note.contains("project"), "{note}");
        }
        let w = WikiRead::left_unchecked("withheld");
        assert_eq!(w.note.as_deref(), Some(WITHHELD_NOTE));
    }

    #[test]
    fn only_a_wiki_more_private_than_the_project_is_withheld() {
        use Visibility::{Internal, Private};
        assert!(more_private(Internal, Private));
        assert!(!more_private(Private, Private));
        assert!(!more_private(Private, Internal));
        assert!(!more_private(Internal, Internal));
    }

    /// W3 review HIGH: a failed read of the wiki (git, a timeout) never
    /// fails the job: the references are left unchecked, `read_failed`.
    #[test]
    fn a_wiki_read_failure_is_read_failed_not_a_job_error() {
        let id = RequestId::parse("req-wiki-read").expect("id");
        let about = WikiReadFor {
            project: ProjectId::new(7),
            visibility: Visibility::Private,
            request_id: &id,
        };
        let r = settle(Err(ForgeError::new(ErrorCode::GitUnavailable)), about);
        assert_eq!(r.state, "read_failed");
        assert_eq!(r.refs, WikiRefs::Unchecked);
        assert!(r.note.is_some());
        let ok = settle(Ok(WikiRead::unchecked()), about);
        assert_eq!(ok.state, "unchecked");
    }
}
