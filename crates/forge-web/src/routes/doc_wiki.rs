//! `[[wiki:ID]]` in the document API (plan 20 W3, ADR-0040 §5): a
//! reference to a page of the project's linked wiki resolves against the
//! wiki project's default-branch tip, **decided per viewer**.
//!
//! 1. Only a document that has a wiki reference looks anything up (the
//!    link, the wiki project, its tip): the cost of the rest is unchanged.
//! 2. The link (`forge.project_wikis`, owner side only — a wiki has no
//!    wiki of its own): none → every wiki reference is `no_wiki`.
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
//! A denial is an answer, never an error; anything else that fails (the
//! database, git, a timeout) fails the request with its code, as every
//! other read of the document API does.

use forge_auth::{prove, Action, Principal, ProjectCtx};
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

/// The linked wiki of `t`'s project for `doc`'s view (module docs).
pub(crate) async fn wiki_side(
    state: &AppState,
    principal: &Principal,
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
        return Ok(WikiSide::NotLinked);
    };
    let Some(record) = load_bounded(state, link.wiki, Some(caller))
        .await?
        .filter(|r| r.id == link.wiki)
    else {
        return Ok(WikiSide::Unreadable);
    };
    let ctx = ProjectCtx::new(loaded(&record));
    match prove(principal, Action::RepoBrowse, ctx) {
        Ok(_) => {}
        Err(e) if e.code() == ErrorCode::Internal => return Err(e),
        Err(_) => return Ok(WikiSide::Unreadable),
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
    let index = match revision(&wiki, None).await {
        Ok(tip) => index_at(&wiki, &tip).await?,
        Err(e) if e.code() == ErrorCode::RepositoryPathNotResolved => {
            DocIndex::build(Default::default())
        }
        Err(e) => return Err(e.with_context("wiki_project_id", record.id.get())),
    };
    Ok(WikiSide::Read {
        project: record.id,
        index: Box::new(index),
    })
}
