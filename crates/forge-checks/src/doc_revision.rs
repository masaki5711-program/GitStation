//! Document revisions and edit claims (plan 13 §7.5, stage 4 part A,
//! ADR-0027): which documents a merge request revises — derived from git,
//! nothing written into a file —, the rule "one open revision per
//! document", and the edit claims' commands.
//!
//! **Revised documents** ([`revised_documents`]): the `.md` files whose
//! blob differs between the merge base with the target and the head
//! (added, changed or removed), each a document when its front matter
//! names a `doctype` and its file is named after its `id` (`<id>.md`), or
//! a translation (`<id>.<lang>.md` with `lang` and `translation_of: <id>`)
//! — the document index's rule (forge-web `documents`). A removed file is
//! read at the merge base. Two listings (`ls-tree`) and one read of the
//! changed files within [`DocLimits`]; a bound hit is named
//! ([`Revised::incomplete`]) and what was read is kept.
//!
//! **The rule** (ADR-0027 §1): the earliest open merge request revising a
//! document holds it. Opening a merge request that would revise a document
//! another open one revises is refused ([`DocRevisions::refuse_second`],
//! `FG-DOC-0006`, forge-web's create route); a push is never refused —
//! the `check.spec_index` job records each version's documents again
//! ([`crate::spec_checks`]) and logs a second revision it finds
//! (`forge.doc_revision.second_revision`, the mark), which the status
//! route shows (`held: false`). TODO(plan 13 stage 4 part B): refuse the
//! merge of a second revision in `evaluate_merge`, and the e-approval
//! additions (ADR-0028); the `single_revision` / `free` setting per type.
//!
//! **Edit claims**: every write is decided twice (as [`crate::Checks`]):
//! the route proves `DocumentEditClaim` / `DocumentEditClaimRevoke`;
//! forge-db's `*_edit_claim_decided` lock the project, the caller's roles
//! and account and call `decide` — here `Authorized::reconfirm`. Only this
//! crate calls them (`tests/decided_callers.rs`).

use std::collections::BTreeSet;

use forge_auth::{Action, Authorized, ProjectCtx};
use forge_core::doc_revision::DocKey;
use forge_core::{MergeRequestId, ObjectId, ProjectId, RefName, RepoId, UserId};
use forge_db::checks::doc_revisions::{
    self as db, ClaimOutcome, EditClaim, OpenRevision, MAX_DOCUMENTS_PER_MERGE_REQUEST,
};
use forge_db::Db;
use forge_doc::document::parse_document;
use forge_errors::{ErrorCode, ForgeError};
use forge_git::browse::{BlobListing, Browser, ListedBlob, RefLookup};

use crate::doc_lint::{is_document_file, read_cap, select, DocLimits, DocTree};
use crate::{account, caller, change_context, loaded, require, ChecksRequest};

/// The documents a revision changes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Revised {
    /// Sorted, without duplicates.
    pub keys: Vec<DocKey>,
    /// The first bound hit (`doc_tree`, `doc_files`, `doc_bytes`, …):
    /// `keys` may then miss some.
    pub incomplete: Option<&'static str>,
}

/// The file name without `.md` (`None`: not a `.md`).
fn stem_of(path: &str) -> Option<&str> {
    path.rsplit('/').next()?.strip_suffix(".md")
}

/// The document `bytes` (at `path`) is, by the document index's rule:
/// `<id>.md` naming a `doctype`, or a translation `<id>.<lang>.md` with
/// `lang` and `translation_of: <id>`. `None`: no document.
pub fn document_key(path: &str, bytes: &[u8]) -> Option<DocKey> {
    let doc = parse_document(bytes);
    let (Some(id), Some(_), Some(stem)) = (doc.id(), doc.doctype(), stem_of(path)) else {
        return None;
    };
    let of = doc.meta_text("translation_of");
    if stem == id && of.is_none() {
        return Some(DocKey::original(id));
    }
    let lang = doc.meta_text("lang")?;
    (of == Some(id) && stem == format!("{id}.{lang}")).then(|| DocKey {
        doc_id: id.to_owned(),
        lang: Some(lang.to_owned()),
    })
}

/// The `.md` files of `head` that are new or whose blob differs from
/// `base`'s, and the `.md` files of `base` that `head` no longer has.
/// Without a base every `.md` of the head is new.
pub fn changed_markdown(
    base: Option<&BlobListing>,
    head: &BlobListing,
) -> (BlobListing, BlobListing) {
    let before: std::collections::HashMap<&str, &str> = base
        .map(|b| {
            b.blobs
                .iter()
                .filter(|x| is_document_file(&x.path))
                .map(|x| (x.path.as_str(), x.oid.as_str()))
                .collect()
        })
        .unwrap_or_default();
    let now: std::collections::HashSet<&str> = head.blobs.iter().map(|x| x.path.as_str()).collect();
    let changed: Vec<ListedBlob> = head
        .blobs
        .iter()
        .filter(|x| is_document_file(&x.path))
        .filter(|x| before.get(x.path.as_str()) != Some(&x.oid.as_str()))
        .cloned() // clone: the read owns its list
        .collect();
    let removed: Vec<ListedBlob> = base
        .map(|b| {
            b.blobs
                .iter()
                .filter(|x| is_document_file(&x.path) && !now.contains(x.path.as_str()))
                .cloned() // clone: as above
                .collect()
        })
        .unwrap_or_default();
    (
        BlobListing {
            blobs: changed,
            truncated: false,
        },
        BlobListing {
            blobs: removed,
            truncated: false,
        },
    )
}

/// Read the selected `.md` files of `listing` within `limits` (the
/// document lint's selection and reads).
async fn read_listed(
    browse: &Browser,
    repo: RepoId,
    listing: &BlobListing,
    limits: &DocLimits,
) -> Result<DocTree, ForgeError> {
    let (chosen, mut tree) = select(listing, limits);
    let contents = browse.read_blobs(repo, &chosen.whole).await?;
    for (b, bytes) in chosen.whole.iter().zip(contents) {
        tree.add(&b.path, bytes);
    }
    for b in &chosen.prefix {
        let bytes = browse.read_blob_prefix(repo, b, read_cap(&b.path)).await?;
        tree.add(&b.path, bytes);
    }
    Ok(tree)
}

/// The documents `head` revises against `base` (module docs). Errors:
/// forge-git's (`FG-GIT-…`, the browse timeout and caps), each with the
/// commit.
pub async fn revised_documents(
    browse: &Browser,
    repo: RepoId,
    base: Option<&ObjectId>,
    head: &ObjectId,
    limits: &DocLimits,
) -> Result<Revised, ForgeError> {
    let at = |c: &ObjectId| {
        let c = c.as_str().to_owned();
        move |e: ForgeError| e.with_context("commit", c)
    };
    let head_listing = browse
        .blobs_in_tree(repo, head.as_str(), limits.max_tree_entries)
        .await
        .map_err(at(head))?;
    let base_listing = match base {
        Some(b) if b == head => return Ok(Revised::default()),
        Some(b) => Some(
            browse
                .blobs_in_tree(repo, b.as_str(), limits.max_tree_entries)
                .await
                .map_err(at(b))?,
        ),
        None => None,
    };
    let mut incomplete = None;
    if head_listing.truncated || base_listing.as_ref().is_some_and(|b| b.truncated) {
        incomplete = Some("doc_tree");
    }
    let (changed, removed) = changed_markdown(base_listing.as_ref(), &head_listing);
    let mut keys = BTreeSet::new();
    let now = read_listed(browse, repo, &changed, limits)
        .await
        .map_err(at(head))?;
    incomplete = incomplete.or(now.incomplete);
    for (path, bytes) in &now.markdown {
        keys.extend(document_key(path, bytes));
    }
    if let Some(b) = base {
        let gone = read_listed(browse, repo, &removed, limits)
            .await
            .map_err(at(b))?;
        incomplete = incomplete.or(gone.incomplete);
        for (path, bytes) in &gone.markdown {
            keys.extend(document_key(path, bytes));
        }
    }
    let mut keys: Vec<DocKey> = keys.into_iter().collect();
    if keys.len() > MAX_DOCUMENTS_PER_MERGE_REQUEST {
        keys.truncate(MAX_DOCUMENTS_PER_MERGE_REQUEST);
        incomplete = incomplete.or(Some("doc_files"));
    }
    Ok(Revised { keys, incomplete })
}

/// What opening a merge request from `source` into `target` would revise
/// (forge-web's create route, before the merge request exists): the
/// source's head and its documents against the merge base with the
/// target's tip. `None` when the source branch has no commit (the create
/// command answers that itself).
pub async fn revised_by_branches(
    browse: &Browser,
    repo: RepoId,
    source: &RefName,
    target: &RefName,
    limits: &DocLimits,
) -> Result<Option<(ObjectId, Revised)>, ForgeError> {
    let commit_of = |lookup: RefLookup| -> Result<Option<ObjectId>, ForgeError> {
        match lookup {
            RefLookup::Commit(tip) => ObjectId::parse(&tip.commit).map(Some).map_err(|_| {
                ForgeError::new(ErrorCode::Internal)
                    .with_context("reason", "resolved_commit_invalid")
                    .with_context("repo_id", repo.get())
            }),
            RefLookup::Missing | RefLookup::NotACommit => Ok(None),
        }
    };
    let Some(head) = commit_of(browse.find_ref(repo, source).await?)? else {
        return Ok(None);
    };
    let base = match commit_of(browse.find_ref(repo, target).await?)? {
        Some(tip) => browse.merge_base(repo, &head, &tip).await?,
        None => None,
    };
    let revised = revised_documents(browse, repo, base.as_ref(), &head, limits).await?;
    Ok(Some((head, revised)))
}

/// The open revisions of a document and its live edit claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentStatus {
    /// Every language, the earliest first per language.
    pub revisions: Vec<OpenRevision>,
    pub claim: Option<EditClaim>,
}

/// The revision rule's reads and writes and the edit claims' commands
/// (`forge serve` builds one beside [`crate::Checks`]).
#[derive(Clone)]
pub struct DocRevisions {
    db: Db,
}

impl std::fmt::Debug for DocRevisions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DocRevisions").finish_non_exhaustive()
    }
}

/// `FG-DOC-0006` for the first of `keys` an open merge request revises.
fn refusal(project: ProjectId, open: &[OpenRevision], keys: &[DocKey]) -> Option<ForgeError> {
    let held = open.iter().find(|r| keys.contains(&r.key))?;
    let mut e = ForgeError::new(ErrorCode::DocRevisionOpen)
        .with_context("doc_id", held.key.doc_id.clone()) // clone: the log's copy
        .with_context("holder_iid", held.iid)
        .with_context("holder_since", held.since.clone()) // clone: as above
        .with_context("project_id", project.get());
    if let Some(lang) = &held.key.lang {
        e = e.with_context("lang", lang.clone()); // clone: as above
    }
    Some(e)
}

impl DocRevisions {
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    /// `GET …/documents/{doc_id}/status` (`RepoBrowse`): the open
    /// revisions of `doc_id` (and its translations) and its live claim.
    pub async fn status(
        &self,
        proof: &Authorized<ProjectCtx>,
        doc_id: &str,
    ) -> Result<DocumentStatus, ForgeError> {
        require(proof.action(), Action::RepoBrowse)?;
        let project = *proof.id();
        Ok(DocumentStatus {
            revisions: db::document_revisions(&self.db, project, doc_id).await?,
            claim: db::edit_claim(&self.db, project, doc_id).await?,
        })
    }

    /// The first `limit` merges applied to `branch` of the proof's project,
    /// oldest first, and whether there are more (`GET …/versions`,
    /// `RepoBrowse`).
    pub async fn applied_merges(
        &self,
        proof: &Authorized<ProjectCtx>,
        branch: &str,
        limit: usize,
    ) -> Result<(Vec<db::AppliedMerge>, bool), ForgeError> {
        require(proof.action(), Action::RepoBrowse)?;
        db::applied_merges(&self.db, *proof.id(), branch, limit).await
    }

    /// Refuse to open a merge request revising `keys` of `project` when an
    /// open one revises any of them already (`FG-DOC-0006`, naming the
    /// earliest). The caller decided `MergeRequestCreate`.
    pub async fn refuse_second(
        &self,
        project: ProjectId,
        keys: &[DocKey],
    ) -> Result<(), ForgeError> {
        let open = db::open_revisions(&self.db, project, keys).await?;
        match refusal(project, &open, keys) {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// A merge request was opened revising `keys` at `head`: record them,
    /// and release its author's claims on those documents (the merge
    /// request is now what others see). The number of claims released.
    pub async fn record_opened(
        &self,
        project: ProjectId,
        mr: MergeRequestId,
        author: UserId,
        head: &ObjectId,
        keys: &[DocKey],
    ) -> Result<u64, ForgeError> {
        db::record_merge_request_documents(&self.db, project, mr, head.as_str(), keys).await?;
        let originals: Vec<String> = keys
            .iter()
            .filter(|k| k.lang.is_none())
            .map(|k| k.doc_id.clone()) // clone: bound as an array
            .collect();
        db::release_claims_of(&self.db, project, author, &originals).await
    }

    /// `PUT …/documents/{doc_id}/edit-claim` (`DocumentEditClaim`): take
    /// or extend the caller's claim. `FG-DOC-0007` when another person's
    /// claim is live.
    pub async fn take(
        &self,
        request: &ChecksRequest,
        proof: &Authorized<ProjectCtx>,
        doc_id: &str,
        merge_request_iid: Option<i64>,
    ) -> Result<(ClaimOutcome, EditClaim), ForgeError> {
        require(proof.action(), Action::DocumentEditClaim)?;
        let user = caller(proof.principal(), Action::DocumentEditClaim)?;
        let project = *proof.id();
        let ctx = change_context(request, proof.principal(), user);
        let (outcome, claim, ()) = db::take_edit_claim_decided(
            &self.db,
            &ctx,
            project,
            user,
            doc_id,
            merge_request_iid,
            |facts| {
                let fresh = ProjectCtx::new(loaded(&facts.project));
                proof.reconfirm(account(facts.account), fresh).map(drop)
            },
        )
        .await?;
        tracing::info!(
            target: "forge_checks",
            event = "forge.doc_revision.claimed",
            request_id = %request.request_id,
            project_id = project.get(),
            doc_id,
            user_id = user.get(),
            outcome = ?outcome,
            expires_at = %claim.expires_at,
            "edit claim taken or extended"
        );
        Ok((outcome, claim))
    }

    /// `DELETE …/documents/{doc_id}/edit-claim` (`DocumentEditClaim`):
    /// release the caller's own live claim.
    pub async fn release(
        &self,
        request: &ChecksRequest,
        proof: &Authorized<ProjectCtx>,
        doc_id: &str,
    ) -> Result<(), ForgeError> {
        require(proof.action(), Action::DocumentEditClaim)?;
        let user = caller(proof.principal(), Action::DocumentEditClaim)?;
        let project = *proof.id();
        let ctx = change_context(request, proof.principal(), user);
        db::release_edit_claim_decided(&self.db, &ctx, project, user, doc_id, |facts| {
            let fresh = ProjectCtx::new(loaded(&facts.project));
            proof.reconfirm(account(facts.account), fresh).map(drop)
        })
        .await
    }

    /// `POST …/documents/{doc_id}/edit-claim/revoke`
    /// (`DocumentEditClaimRevoke`): remove the live claim, whoever holds
    /// it (audited). The claim removed.
    pub async fn revoke(
        &self,
        request: &ChecksRequest,
        proof: &Authorized<ProjectCtx>,
        doc_id: &str,
    ) -> Result<EditClaim, ForgeError> {
        require(proof.action(), Action::DocumentEditClaimRevoke)?;
        let user = caller(proof.principal(), Action::DocumentEditClaimRevoke)?;
        let project = *proof.id();
        let ctx = change_context(request, proof.principal(), user);
        let (claim, ()) =
            db::revoke_edit_claim_decided(&self.db, &ctx, project, user, doc_id, |facts| {
                let fresh = ProjectCtx::new(loaded(&facts.project));
                proof.reconfirm(account(facts.account), fresh).map(drop)
            })
            .await?;
        tracing::info!(
            target: "forge_checks",
            event = "forge.doc_revision.claim_revoked",
            request_id = %request.request_id,
            project_id = project.get(),
            doc_id,
            holder_id = claim.holder.get(),
            user_id = user.get(),
            "edit claim revoked"
        );
        Ok(claim)
    }
}

#[cfg(test)]
#[path = "doc_revision_tests.rs"]
mod tests;
