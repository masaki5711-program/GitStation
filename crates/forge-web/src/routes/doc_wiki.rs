//! `[[wiki:ID]]` in the document API (plan 20 W3, ADR-0040 §5): a
//! reference to a page of the project's linked wiki resolves against the
//! wiki project's default-branch tip, **decided per viewer**.
//!
//! 1. Only a document that has a wiki reference looks anything up (the
//!    link, the wiki project, its tip): the cost of the rest is unchanged.
//! 2. The link (`forge.project_wikis`, owner side only — a wiki has no
//!    wiki of its own): none → `no_wiki`, but only for a viewer who may
//!    manage the link (`prove(ProjectWikiLink)` on the owner: Maintainer,
//!    who sees the link anyway). Everyone else gets `unreadable`, the same
//!    answer as a linked wiki they may not read — no oracle of the link
//!    (ADR-0040 Amendment W3-R).
//! 3. The wiki project is loaded for the viewer and `prove(RepoBrowse)`
//!    decides (the document API's own action, so the same rule as opening
//!    the wiki's documents directly). Denied, absent, or a repository that
//!    is not ready → `unreadable`: the id only. Nothing of the wiki is read
//!    then, so the answer cannot tell whether the page exists.
//! 4. Allowed → one read of the wiki's tip with the lint's bounds (charged
//!    to the caller's spec budget like any other index); an unborn default
//!    branch is an empty wiki. A page is `found` (title, block kind), more
//!    confidential than the document shown → `hidden`, absent → `broken`.
//!    For an agent audience a page confidential at the tip (by id or by its
//!    file's path) is `hidden` (plan 06 D-5, the same rule as this
//!    repository's references).
//!
//! A denial is an answer, never an error (a `debug` event with the
//! request id). A failure to read the wiki's git (its storage, a timeout)
//! never fails the owner's document: the wiki references become
//! `unreadable` and `forge.documents.wiki_unavailable` (warn) carries the
//! code and the request id. The link and the project lookups (the
//! database) fail the request with their code, as every other lookup of
//! the document API does.

use forge_auth::{prove, Action, Principal, ProjectCtx};
use forge_checks::doc_lint::DocTree;
use forge_core::{ProjectId, RepositoryState, UserId};
use forge_doc::document::{Classification, Document};
use forge_doc::render::{Resolver, TargetDoc, WikiLookup};
use forge_errors::{ErrorCode, ForgeError};

use crate::app::AppState;
use crate::routes::browse::Target;
use crate::routes::documents::{index_at, secret, DocIndex, TipMarks};
use crate::routes::projects::{load_bounded, loaded};
use crate::routes::specs::{charge, revision};
use crate::routes::write::hosting;

/// What a view knows of the project's linked wiki (module docs).
pub(crate) enum WikiSide {
    /// No linked wiki (or no wiki reference in the document: never asked).
    NotLinked,
    /// The viewer may not read the wiki: id only.
    Unreadable,
    /// The wiki's tip as read for the viewer.
    Read {
        project: ProjectId,
        index: Box<DocIndex>,
    },
}

impl WikiSide {
    /// The wiki project when its pages may be shown.
    pub(crate) fn project(&self) -> Option<ProjectId> {
        match self {
            Self::Read { project, .. } => Some(*project),
            Self::NotLinked | Self::Unreadable => None,
        }
    }

    fn lookup(&self, id: &str, agent: bool) -> WikiLookup {
        match self {
            Self::NotLinked => WikiLookup::NotLinked,
            Self::Unreadable => WikiLookup::Unreadable,
            Self::Read { index, .. } => {
                let Some(mut page) = index.document(id) else {
                    return WikiLookup::Page(None);
                };
                if agent && secret(index, &TipMarks::IsTip, id, None) {
                    page.classification = Classification::Confidential;
                }
                WikiLookup::Page(Some(page))
            }
        }
    }
}

/// A view's resolver: this repository's references through `here`, the
/// wiki's through `wiki`.
pub(crate) struct ViewResolver<'a> {
    pub(crate) here: &'a dyn Resolver,
    pub(crate) wiki: &'a WikiSide,
    pub(crate) agent: bool,
}

impl Resolver for ViewResolver<'_> {
    fn document(&self, id: &str) -> Option<TargetDoc> {
        self.here.document(id)
    }

    fn wiki(&self, id: &str) -> WikiLookup {
        self.wiki.lookup(id, self.agent)
    }
}

/// Who reads the view: the decided principal, the owner project's context
/// (for the link's own action) and the request id (for the logs).
pub(crate) struct WikiViewer<'a> {
    pub(crate) principal: &'a Principal,
    pub(crate) owner: Option<ProjectCtx>,
    pub(crate) request_id: String,
}

impl WikiViewer<'_> {
    /// `no_wiki` for a viewer who may manage the link, else `unreadable`.
    fn without_wiki(&self) -> Result<WikiSide, ForgeError> {
        let Some(owner) = self.owner.clone() else {
            return Ok(WikiSide::Unreadable);
        };
        match prove(self.principal, Action::ProjectWikiLink, owner) {
            Ok(_) => Ok(WikiSide::NotLinked),
            Err(e) if e.code() == ErrorCode::Internal => Err(e),
            Err(_) => Ok(WikiSide::Unreadable),
        }
    }
}

/// The linked wiki of `t`'s project for `doc`'s view (module docs).
pub(crate) async fn wiki_side(
    state: &AppState,
    viewer: &WikiViewer<'_>,
    t: &Target,
    caller: UserId,
    doc: &Document,
) -> Result<WikiSide, ForgeError> {
    if !doc.refs.iter().any(|r| r.wiki) {
        return Ok(WikiSide::NotLinked);
    }
    let link = {
        let limit = state.lookup_timeout;
        match tokio::time::timeout(limit, hosting(state)?.wiki_link(t.project)).await {
            Ok(result) => result?,
            Err(_) => {
                return Err(ForgeError::new(ErrorCode::RequestTimeout)
                    .with_context("operation", "wiki_link_load")
                    .with_context("timeout_ms", limit.as_millis()))
            }
        }
    };
    let Some(link) = link.filter(|l| l.project == t.project) else {
        return viewer.without_wiki();
    };
    let Some(record) = load_bounded(state, link.wiki, Some(caller))
        .await?
        .filter(|r| r.id == link.wiki)
    else {
        return Ok(WikiSide::Unreadable);
    };
    let ctx = ProjectCtx::new(loaded(&record));
    match prove(viewer.principal, Action::RepoBrowse, ctx) {
        Ok(_) => {}
        Err(e) if e.code() == ErrorCode::Internal => return Err(e),
        Err(e) => {
            tracing::debug!(
                target: "forge_web",
                event = "forge.documents.wiki_denied",
                request_id = %viewer.request_id,
                project_id = t.project.get(),
                error_code = e.code().id(),
                "the viewer may not read the linked wiki: its references show the id only"
            );
            return Ok(WikiSide::Unreadable);
        }
    }
    if record.repository.state != RepositoryState::Ready {
        return Ok(WikiSide::Unreadable);
    }
    charge(state, caller, 1)?;
    let wiki = Target {
        smart: std::sync::Arc::clone(&t.smart),
        project: record.id,
        repository: record.repository,
        principal: t.principal.clone(), // clone: the wiki's read has its own target
    };
    let read = match revision(&wiki, None).await {
        Ok(tip) => index_at(&wiki, &tip).await,
        Err(e) if e.code() == ErrorCode::RepositoryPathNotResolved => {
            Ok(DocIndex::build(DocTree::default()))
        }
        Err(e) => Err(e),
    };
    match read {
        Ok(index) => Ok(WikiSide::Read {
            project: record.id,
            index: Box::new(index),
        }),
        Err(e) => {
            tracing::warn!(
                target: "forge_web",
                event = "forge.documents.wiki_unavailable",
                request_id = %viewer.request_id,
                project_id = t.project.get(),
                wiki_project_id = record.id.get(),
                error_code = e.code().id(),
                error = %e,
                "the linked wiki could not be read: its references show the id only"
            );
            Ok(WikiSide::Unreadable)
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic)] // tests may fail loudly

    use forge_checks::doc_lint::DocTree;
    use forge_core::ProjectId;
    use forge_doc::document::Classification;
    use forge_doc::render::WikiLookup;

    use super::WikiSide;
    use crate::routes::documents::DocIndex;

    const PAGE: &str = "---\ndoctype: wiki.page\nid: WIKI-0002\ntitle: 機密の手順\nowner: o\nclassification: confidential\n---\n\n本文。\n";

    fn side() -> WikiSide {
        let mut tree = DocTree::default();
        tree.add("WIKI-0002.md", PAGE.as_bytes().to_vec());
        tree.sort();
        WikiSide::Read {
            project: ProjectId::new(9),
            index: Box::new(DocIndex::build(tree)),
        }
    }

    /// W3 review LOW: a wiki page confidential at the tip is a confidential
    /// target for an agent (the view then hides it: its id only).
    #[test]
    fn agent_confidential_wiki_page_is_hidden() {
        let s = side();
        let WikiLookup::Page(Some(page)) = s.lookup("WIKI-0002", true) else {
            panic!("a page");
        };
        assert_eq!(page.classification, Classification::Confidential);
        assert_eq!(s.lookup("WIKI-0404", true), WikiLookup::Page(None));
        assert_eq!(
            WikiSide::Unreadable.lookup("WIKI-0002", true),
            WikiLookup::Unreadable
        );
        assert_eq!(
            WikiSide::NotLinked.lookup("WIKI-0002", false),
            WikiLookup::NotLinked
        );
    }
}
