//! The linked wiki for `forge/doc-lint` (plan 20 W3, ADR-0040 §5): what
//! `[[wiki:ID]]` references are checked against. The lint itself is pure
//! (`forge_doc::lint::lint_with_wiki`); this reads the project's link
//! (`forge.project_wikis`, owner side only) and the wiki project's
//! default-branch tip with the document lint's bounds ([`DocLimits`]).
//!
//! The wiki is read only when a document of the head contains `[[wiki:`.
//! The answers ([`WikiRead::state`], logged as `doc_wiki`):
//!
//! - `unchecked`: no document names the wiki — nothing read;
//! - `not_linked`: the project has no linked wiki (it may be a wiki itself)
//!   — every wiki reference is `wiki_not_linked`;
//! - `linked`: the tip was read (an unborn default branch is an empty
//!   wiki) — references that do not resolve are `wiki_ref_broken`;
//! - `bounded` / `not_ready`: the wiki's tip hit a bound (`FG-DOC-0002`'s)
//!   or its repository is not ready (or the wiki project is gone) — wiki
//!   references are left unchecked and the run's summary says so, never a
//!   silent pass.
//!
//! Both rules are warnings (`FG-DOC-0008`). A failure to read (git, the
//! database, a timeout) is the job's error like any other read of the head:
//! the job is retried.

use std::time::Duration;

use forge_core::namespace_resolver::ProjectAccessLoader;
use forge_core::{ProjectId, RepositoryState};
use forge_db::Db;
use forge_doc::document::WIKI_PREFIX;
use forge_doc::lint::{Source, WikiIndex, WikiRefs};
use forge_errors::{ErrorCode, ForgeError};
use forge_git::browse::Browser;

use crate::doc_lint::{read_tree, DocLimits, DocTree};

/// The caller name of the wiki's reads (the browse API's per-caller key).
const CALLER: &str = "doc_lint_wiki";

/// What was read of the linked wiki (module docs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WikiRead {
    pub refs: WikiRefs,
    /// `unchecked`, `not_linked`, `linked`, `bounded` or `not_ready`.
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

    fn left_unchecked(state: &'static str, why: String) -> Self {
        Self {
            refs: WikiRefs::Unchecked,
            state,
            note: Some(format!(
                "Wiki references ([[wiki:…]]) were not checked: {why} [{}].",
                ErrorCode::WikiReferenceBroken.id()
            )),
        }
    }
}

/// Whether a document of `docs` names the linked wiki (`[[wiki:`).
pub fn names_wiki(docs: &DocTree) -> bool {
    let needle = format!("[[{WIKI_PREFIX}:");
    docs.markdown
        .iter()
        .any(|(_, bytes)| bytes.windows(needle.len()).any(|w| w == needle.as_bytes()))
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

/// The linked wiki of `project` for the lint of `docs` (module docs).
pub async fn read_wiki(
    db: &Db,
    browse: &Browser,
    projects: &dyn ProjectAccessLoader,
    project: ProjectId,
    docs: &DocTree,
    (limits, lookup_timeout): (&DocLimits, Duration),
) -> Result<WikiRead, ForgeError> {
    if !names_wiki(docs) {
        return Ok(WikiRead::unchecked());
    }
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
        return Ok(WikiRead::left_unchecked(
            "not_ready",
            format!("the wiki project {} is gone", link.wiki.get()),
        ));
    };
    let repo = record.repository;
    if repo.state != RepositoryState::Ready {
        return Ok(WikiRead::left_unchecked(
            "not_ready",
            format!(
                "the wiki repository is {} (wiki project {})",
                repo.state.as_str(),
                link.wiki.get()
            ),
        ));
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
    if let Some(bound) = tree.incomplete {
        return Ok(WikiRead::left_unchecked(
            "bounded",
            format!(
                "the wiki's tip was read only in part ({bound}, {})",
                ErrorCode::DocIndexBounded.id()
            ),
        ));
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
    fn an_unchecked_wiki_says_why() {
        let r = WikiRead::left_unchecked("bounded", "the reason".into());
        assert_eq!(r.refs, WikiRefs::Unchecked);
        let note = r.note.unwrap_or_default();
        assert!(note.contains("the reason") && note.contains("FG-DOC-0008"));
    }
}
