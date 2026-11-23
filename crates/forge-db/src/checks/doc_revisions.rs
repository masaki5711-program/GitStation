//! Document revisions and edit claims (plan 13 §7.5, stage 4 part A,
//! ADR-0027; migration 20270105100000). Both tables are derived state.
//!
//! - **Revisions** (`forge.merge_request_documents`): the documents each
//!   merge request revises, derived from git by forge-checks and written by
//!   [`record_merge_request_documents`] (when a merge request is opened and
//!   for each of its versions). [`open_revisions`] reads the open merge
//!   requests revising some documents, the earliest first: the earliest
//!   holds the revision (ADR-0027 §1).
//! - **Edit claims** (`forge.document_edit_claims`): one per document of a
//!   project; [`edit_claim`] reads the live one (`expires_at > now()`, the
//!   database's clock). The writes are decided again inside their
//!   transaction (forge-checks reconfirms `authorize()` on the locked
//!   facts; only it calls them — `forge-checks/tests/decided_callers.rs`):
//!   [`take_edit_claim_decided`] (take or extend, `FG-DOC-0007` when a
//!   live claim is another person's), [`release_edit_claim_decided`] (the
//!   caller's own), [`revoke_edit_claim_decided`] (anybody's, audited
//!   `document.edit_claim.revoked`). [`release_claims_of`] drops a person's
//!   claims when they open a merge request revising those documents.

use forge_core::doc_revision::{
    DocEditClaimChange, DocKey, EDIT_CLAIM_MINUTES, MAX_LIVE_CLAIMS_PER_PERSON,
};
use forge_core::{AuditDetails, MergeRequestId, ProjectId, UserId};
use forge_errors::{ErrorCode, ForgeError};

use crate::change::{AuditEntry, ChangeContext, ChangeRecord, ChangeTx, Outcome, Settle};
use crate::projects::{lock_shared, locked_access_facts, ProjectUpdateFacts};
use crate::{query_error, read_query_error, Db};

const TS: &str = "YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"";

/// Documents one merge request may record at most (forge-checks reads no
/// more changed `.md` files than its document bound either).
pub const MAX_DOCUMENTS_PER_MERGE_REQUEST: usize = 2_000;

/// An open merge request revising a document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenRevision {
    pub key: DocKey,
    pub merge_request: MergeRequestId,
    pub iid: i64,
    pub title: String,
    pub author: UserId,
    /// RFC 3339, UTC: when it first revised the document.
    pub since: String,
    pub head_sha: String,
}

/// A live edit claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditClaim {
    pub doc_id: String,
    pub holder: UserId,
    pub merge_request_iid: Option<i64>,
    /// RFC 3339, UTC.
    pub claimed_at: String,
    /// RFC 3339, UTC: when it lapses unless extended.
    pub expires_at: String,
}

/// What [`take_edit_claim_decided`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimOutcome {
    /// No live claim was there (none, or a lapsed one replaced).
    Taken,
    /// The caller's own live claim, extended.
    Extended,
}

fn keys_columns(keys: &[DocKey]) -> (Vec<String>, Vec<String>) {
    keys.iter()
        .map(|k| (k.doc_id.clone(), k.lang_column().to_owned())) // clone: bound as arrays
        .unzip()
}

fn key_of(doc_id: String, lang: String) -> DocKey {
    DocKey {
        doc_id,
        lang: (!lang.is_empty()).then_some(lang),
    }
}

type RevisionRow = (String, String, i64, i64, String, i64, String, String);

/// The open merge requests of `project` revising any of `keys`, by key,
/// then the earliest first (`since`, then the merge request's id). One
/// statement on the read pool.
pub async fn open_revisions(
    db: &Db,
    project: ProjectId,
    keys: &[DocKey],
) -> Result<Vec<OpenRevision>, ForgeError> {
    const QUERY: &str = "doc_revisions.open";
    if keys.is_empty() {
        return Ok(Vec::new());
    }
    let (ids, langs) = keys_columns(keys);
    let sql = format!(
        "SELECT d.doc_id, d.lang, m.id, m.iid, m.title, m.author_id, \
         to_char(d.since AT TIME ZONE 'UTC', '{TS}'), d.head_sha \
         FROM forge.merge_request_documents d \
         JOIN forge.merge_requests m ON m.id = d.merge_request_id \
         JOIN unnest($2::text[], $3::text[]) AS k(doc_id, lang) \
           ON k.doc_id = d.doc_id AND k.lang = d.lang \
         WHERE d.project_id = $1 AND m.state = 'open' \
         ORDER BY d.doc_id, d.lang, d.since, m.id"
    );
    let rows: Vec<RevisionRow> = db
        .within(QUERY, async {
            sqlx::query_as(&sql)
                .bind(project.get())
                .bind(ids)
                .bind(langs)
                .fetch_all(db.pool().executor())
                .await
                .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    Ok(rows.into_iter().map(revision_of).collect())
}

/// The open merge requests of `project` revising `doc_id` in any language
/// (the original and its translations), by language, then the earliest
/// first. One statement on the read pool.
pub async fn document_revisions(
    db: &Db,
    project: ProjectId,
    doc_id: &str,
) -> Result<Vec<OpenRevision>, ForgeError> {
    const QUERY: &str = "doc_revisions.document";
    let sql = format!(
        "SELECT d.doc_id, d.lang, m.id, m.iid, m.title, m.author_id,          to_char(d.since AT TIME ZONE 'UTC', '{TS}'), d.head_sha          FROM forge.merge_request_documents d          JOIN forge.merge_requests m ON m.id = d.merge_request_id          WHERE d.project_id = $1 AND d.doc_id = $2 AND m.state = 'open'          ORDER BY d.lang, d.since, m.id"
    );
    let rows: Vec<RevisionRow> = db
        .within(QUERY, async {
            sqlx::query_as(&sql)
                .bind(project.get())
                .bind(doc_id)
                .fetch_all(db.pool().executor())
                .await
                .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    Ok(rows.into_iter().map(revision_of).collect())
}

fn revision_of(row: RevisionRow) -> OpenRevision {
    let (doc_id, lang, mr, iid, title, author, since, head_sha) = row;
    OpenRevision {
        key: key_of(doc_id, lang),
        merge_request: MergeRequestId::new(mr),
        iid,
        title,
        author: UserId::new(author),
        since,
        head_sha,
    }
}

/// Replace the documents merge request `mr` (of `project`) revises with
/// `keys`, as derived at `head_sha`: rows no longer revised are removed,
/// new ones inserted, and a document it revised already keeps its `since`.
/// One transaction on the write pool. `FG-SYS-0001
/// reason=too_many_documents` past [`MAX_DOCUMENTS_PER_MERGE_REQUEST`].
pub async fn record_merge_request_documents(
    db: &Db,
    project: ProjectId,
    mr: MergeRequestId,
    head_sha: &str,
    keys: &[DocKey],
) -> Result<(), ForgeError> {
    const QUERY: &str = "doc_revisions.record";
    if keys.len() > MAX_DOCUMENTS_PER_MERGE_REQUEST {
        return Err(ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "too_many_documents")
            .with_context("documents", keys.len())
            .with_context("merge_request_id", mr.get()));
    }
    let (ids, langs) = keys_columns(keys);
    db.within(QUERY, async {
        let mut tx = db
            .write_pool()
            .begin()
            .await
            .map_err(|e| query_error(QUERY, e))?;
        sqlx::query(
            "DELETE FROM forge.merge_request_documents d WHERE d.merge_request_id = $1 \
             AND NOT EXISTS (SELECT 1 FROM unnest($2::text[], $3::text[]) AS k(doc_id, lang) \
             WHERE k.doc_id = d.doc_id AND k.lang = d.lang)",
        )
        .bind(mr.get())
        .bind(&ids)
        .bind(&langs)
        .execute(&mut *tx)
        .await
        .map_err(|e| query_error(QUERY, e))?;
        sqlx::query(
            "INSERT INTO forge.merge_request_documents \
             (merge_request_id, project_id, doc_id, lang, head_sha) \
             SELECT $1, $2, k.doc_id, k.lang, $5 FROM unnest($3::text[], $4::text[]) AS k(doc_id, lang) \
             ON CONFLICT (merge_request_id, doc_id, lang) DO UPDATE SET head_sha = EXCLUDED.head_sha",
        )
        .bind(mr.get())
        .bind(project.get())
        .bind(&ids)
        .bind(&langs)
        .bind(head_sha)
        .execute(&mut *tx)
        .await
        .map_err(|e| query_error(QUERY, e))?;
        tx.commit().await.map_err(|e| query_error(QUERY, e))
    })
    .await
}

/// A merge applied to a branch: what changed the branch's tree, oldest
/// first (a document's versions are the ones that changed its blob).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedMerge {
    pub iid: i64,
    pub title: String,
    /// The branch's tip before and after the merge.
    pub before: String,
    pub after: String,
    /// RFC 3339, UTC.
    pub merged_at: String,
}

/// The first `limit` merges applied to `branch` (`refs/heads/…`) of
/// `project`, oldest first, and whether there are more. One statement on
/// the read pool.
pub async fn applied_merges(
    db: &Db,
    project: ProjectId,
    branch: &str,
    limit: usize,
) -> Result<(Vec<AppliedMerge>, bool), ForgeError> {
    const QUERY: &str = "doc_revisions.applied_merges";
    let fetch = i64::try_from(limit.saturating_add(1)).unwrap_or(i64::MAX);
    let sql = format!(
        "SELECT m.iid, m.title, a.expected_old, a.new_sha,          to_char(a.finished_at AT TIME ZONE 'UTC', '{TS}')          FROM forge.merge_request_merges a          JOIN forge.merge_requests m ON m.id = a.merge_request_id          WHERE a.project_id = $1 AND a.state = 'applied' AND a.target_branch = $2          AND a.expected_old IS NOT NULL AND a.new_sha IS NOT NULL          ORDER BY a.finished_at, a.id LIMIT $3"
    );
    let rows: Vec<(i64, String, String, String, String)> = db
        .within(QUERY, async {
            sqlx::query_as(&sql)
                .bind(project.get())
                .bind(branch)
                .bind(fetch)
                .fetch_all(db.pool().executor())
                .await
                .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    let more = rows.len() > limit;
    Ok((
        rows.into_iter()
            .take(limit)
            .map(|(iid, title, before, after, merged_at)| AppliedMerge {
                iid,
                title,
                before,
                after,
                merged_at,
            })
            .collect(),
        more,
    ))
}

type ClaimRow = (String, i64, Option<i64>, String, String);

fn claim_of(row: ClaimRow) -> EditClaim {
    let (doc_id, holder, merge_request_iid, claimed_at, expires_at) = row;
    EditClaim {
        doc_id,
        holder: UserId::new(holder),
        merge_request_iid,
        claimed_at,
        expires_at,
    }
}

fn claim_columns() -> String {
    format!(
        "doc_id, user_id, merge_request_iid, to_char(claimed_at AT TIME ZONE 'UTC', '{TS}'), \
         to_char(expires_at AT TIME ZONE 'UTC', '{TS}')"
    )
}

/// The live edit claim on `doc_id` of `project` (`None`: none, or lapsed).
pub async fn edit_claim(
    db: &Db,
    project: ProjectId,
    doc_id: &str,
) -> Result<Option<EditClaim>, ForgeError> {
    const QUERY: &str = "doc_revisions.claim";
    let sql = format!(
        "SELECT {} FROM forge.document_edit_claims \
         WHERE project_id = $1 AND doc_id = $2 AND expires_at > now()",
        claim_columns()
    );
    let row: Option<ClaimRow> = db
        .within(QUERY, async {
            sqlx::query_as(&sql)
                .bind(project.get())
                .bind(doc_id)
                .fetch_optional(db.pool().executor())
                .await
                .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    Ok(row.map(claim_of))
}

/// Remove `user`'s claims on `doc_ids` of `project` (they opened a merge
/// request revising them: the merge request is now the visible marker).
/// The number removed.
pub async fn release_claims_of(
    db: &Db,
    project: ProjectId,
    user: UserId,
    doc_ids: &[String],
) -> Result<u64, ForgeError> {
    const QUERY: &str = "doc_revisions.release_on_open";
    if doc_ids.is_empty() {
        return Ok(0);
    }
    db.within(QUERY, async {
        sqlx::query(
            "DELETE FROM forge.document_edit_claims \
             WHERE project_id = $1 AND user_id = $2 AND doc_id = ANY($3::text[])",
        )
        .bind(project.get())
        .bind(user.get())
        .bind(doc_ids)
        .execute(db.write_pool())
        .await
        .map(|r| r.rows_affected())
        .map_err(|e| query_error(QUERY, e))
    })
    .await
}

async fn decided_facts(
    tx: &mut ChangeTx,
    project: ProjectId,
    caller: UserId,
) -> Result<ProjectUpdateFacts, ForgeError> {
    let locked = lock_shared(tx, project).await?;
    locked_access_facts(tx, project, caller, &locked).await
}

/// The claim row of `doc_id` (live or lapsed), locked for the
/// transaction; whether it is live.
async fn locked_claim(
    tx: &mut ChangeTx,
    project: ProjectId,
    doc_id: &str,
) -> Result<Option<(EditClaim, bool)>, ForgeError> {
    let doc = doc_id.to_owned();
    // The row may be absent: one change of a document's claim at a time.
    tx.step(|c| async move {
        sqlx::query(
            "SELECT pg_advisory_xact_lock(hashtextextended('doc_edit_claim:' || $1::text || ':' || $2, 0))",
        )
        .bind(project.get())
        .bind(doc)
        .execute(c)
        .await
        .map_err(|e| query_error("doc_revisions.claim_lock", e))
    })
    .await?;
    let sql = format!(
        "SELECT {}, expires_at > now() FROM forge.document_edit_claims \
         WHERE project_id = $1 AND doc_id = $2 FOR UPDATE",
        claim_columns()
    );
    let doc = doc_id.to_owned();
    let row: Option<(String, i64, Option<i64>, String, String, bool)> = tx
        .step(|c| async move {
            sqlx::query_as(&sql)
                .bind(project.get())
                .bind(doc)
                .fetch_optional(c)
                .await
                .map_err(|e| query_error("doc_revisions.claim_read", e))
        })
        .await?;
    Ok(row.map(|(d, h, i, at, until, live)| (claim_of((d, h, i, at, until)), live)))
}

fn held_by_another(project: ProjectId, claim: &EditClaim) -> ForgeError {
    ForgeError::new(ErrorCode::DocEditClaimHeld)
        .with_context("doc_id", claim.doc_id.clone()) // clone: the log's copy
        .with_context("holder_id", claim.holder.get())
        .with_context("expires_at", claim.expires_at.clone()) // clone: as above
        .with_context("project_id", project.get())
}

fn no_claim(project: ProjectId, doc_id: &str) -> ForgeError {
    ForgeError::new(ErrorCode::NotFound)
        .with_context("resource", "document_edit_claim")
        .with_context("reason", "absent")
        .with_context("doc_id", doc_id.to_owned())
        .with_context("project_id", project.get())
}

/// Take `doc_id`'s claim for `caller`, or extend the caller's own live
/// one, for [`EDIT_CLAIM_MINUTES`] from now (the database's clock), linked
/// to open merge request `merge_request_iid` when given. Errors (nothing
/// written): `decide`'s, `FG-DOC-0007` (a live claim of another person),
/// `FG-REPO-0005 resource=merge_request` (no such open merge request in
/// the project), `FG-REPO-0005 resource=project`.
pub async fn take_edit_claim_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    project: ProjectId,
    caller: UserId,
    doc_id: &str,
    merge_request_iid: Option<i64>,
    decide: impl FnOnce(&ProjectUpdateFacts) -> Result<P, ForgeError>,
) -> Result<(ClaimOutcome, EditClaim, P), ForgeError> {
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let decided = decide(&decided_facts(&mut tx, project, caller).await?)?;
        if let Some(iid) = merge_request_iid {
            let open: Option<i64> = tx
                .step(|c| async move {
                    sqlx::query_scalar(
                        "SELECT id FROM forge.merge_requests \
                         WHERE project_id = $1 AND iid = $2 AND state = 'open'",
                    )
                    .bind(project.get())
                    .bind(iid)
                    .fetch_optional(c)
                    .await
                    .map_err(|e| query_error("doc_revisions.claim_mr", e))
                })
                .await?;
            if open.is_none() {
                return Err(ForgeError::new(ErrorCode::NotFound)
                    .with_context("resource", "merge_request")
                    .with_context("reason", "not_open")
                    .with_context("iid", iid)
                    .with_context("project_id", project.get()));
            }
        }
        let current = locked_claim(&mut tx, project, doc_id).await?;
        let outcome = match &current {
            Some((claim, true)) if claim.holder != caller => {
                return Err(held_by_another(project, claim));
            }
            Some((_, true)) => ClaimOutcome::Extended,
            Some((_, false)) | None => ClaimOutcome::Taken,
        };
        if outcome == ClaimOutcome::Taken {
            let doc = doc_id.to_owned();
            let held: i64 = tx
                .step(|c| async move {
                    sqlx::query_scalar(
                        "SELECT count(*) FROM forge.document_edit_claims \
                         WHERE project_id = $1 AND user_id = $2 AND doc_id <> $3 \
                         AND expires_at > now()",
                    )
                    .bind(project.get())
                    .bind(caller.get())
                    .bind(doc)
                    .fetch_one(c)
                    .await
                    .map_err(|e| query_error("doc_revisions.claim_count", e))
                })
                .await?;
            if held >= MAX_LIVE_CLAIMS_PER_PERSON {
                return Err(ForgeError::new(ErrorCode::RequestInvalid)
                    .with_context("reason", "too_many_claims")
                    .with_context("limit", MAX_LIVE_CLAIMS_PER_PERSON)
                    .with_context("project_id", project.get()));
            }
        }
        let sql = format!(
            "INSERT INTO forge.document_edit_claims \
             (project_id, doc_id, user_id, merge_request_iid, claimed_at, expires_at) \
             VALUES ($1, $2, $3, $4, now(), now() + make_interval(mins => $5)) \
             ON CONFLICT (project_id, doc_id) DO UPDATE SET \
             user_id = EXCLUDED.user_id, \
             merge_request_iid = COALESCE(EXCLUDED.merge_request_iid, \
               CASE WHEN $6 THEN forge.document_edit_claims.merge_request_iid END), \
             claimed_at = CASE WHEN $6 THEN forge.document_edit_claims.claimed_at ELSE now() END, \
             expires_at = EXCLUDED.expires_at \
             RETURNING {}",
            claim_columns()
        );
        let doc = doc_id.to_owned();
        let extended = outcome == ClaimOutcome::Extended;
        let minutes = i32::try_from(EDIT_CLAIM_MINUTES).map_err(|_| {
            ForgeError::new(ErrorCode::Internal).with_context("reason", "claim_minutes_range")
        })?;
        let row: ClaimRow = tx
            .step(|c| async move {
                sqlx::query_as(&sql)
                    .bind(project.get())
                    .bind(doc)
                    .bind(caller.get())
                    .bind(merge_request_iid)
                    .bind(minutes)
                    .bind(extended)
                    .fetch_one(c)
                    .await
                    .map_err(|e| query_error("doc_revisions.claim_take", e))
            })
            .await?;
        Ok(Settle::CommitDerived((outcome, claim_of(row), decided)))
    }
    .await;
    tx.settle("document.edit_claim.take", outcome).await
}

/// Release the caller's own live claim on `doc_id`. Errors (nothing
/// written): `decide`'s, `FG-DOC-0007` (the live claim is another
/// person's), `FG-REPO-0005 resource=document_edit_claim` (no live claim).
pub async fn release_edit_claim_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    project: ProjectId,
    caller: UserId,
    doc_id: &str,
    decide: impl FnOnce(&ProjectUpdateFacts) -> Result<P, ForgeError>,
) -> Result<P, ForgeError> {
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let decided = decide(&decided_facts(&mut tx, project, caller).await?)?;
        match locked_claim(&mut tx, project, doc_id).await? {
            Some((claim, true)) if claim.holder != caller => Err(held_by_another(project, &claim)),
            Some((_, true)) => {
                delete_claim(&mut tx, project, doc_id).await?;
                Ok(Settle::CommitDerived(decided))
            }
            Some((_, false)) | None => Err(no_claim(project, doc_id)),
        }
    }
    .await;
    tx.settle("document.edit_claim.release", outcome).await
}

async fn delete_claim(
    tx: &mut ChangeTx,
    project: ProjectId,
    doc_id: &str,
) -> Result<(), ForgeError> {
    let doc = doc_id.to_owned();
    tx.step(|c| async move {
        sqlx::query("DELETE FROM forge.document_edit_claims WHERE project_id = $1 AND doc_id = $2")
            .bind(project.get())
            .bind(doc)
            .execute(c)
            .await
            .map_err(|e| query_error("doc_revisions.claim_delete", e))
    })
    .await?;
    Ok(())
}

/// Revoke the live claim on `doc_id`, whoever holds it (a Maintainer's
/// decision; also the caller's own). Audited `document.edit_claim.revoked`
/// on the project. Errors (nothing written): `decide`'s, `FG-REPO-0005
/// resource=document_edit_claim` (no live claim). The claim removed.
pub async fn revoke_edit_claim_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    project: ProjectId,
    caller: UserId,
    doc_id: &str,
    decide: impl FnOnce(&ProjectUpdateFacts) -> Result<P, ForgeError>,
) -> Result<(EditClaim, P), ForgeError> {
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let decided = decide(&decided_facts(&mut tx, project, caller).await?)?;
        let Some((claim, true)) = locked_claim(&mut tx, project, doc_id).await? else {
            return Err(no_claim(project, doc_id));
        };
        delete_claim(&mut tx, project, doc_id).await?;
        let details = AuditDetails::DocEditClaim(DocEditClaimChange::Revoked {
            project,
            doc_id: claim.doc_id.clone(), // clone: the audit row's copy
            holder: claim.holder,
            merge_request_iid: claim.merge_request_iid,
            expires_at: claim.expires_at.clone(), // clone: as above
        });
        let record = ChangeRecord {
            audit: AuditEntry {
                action: details.action(),
                outcome: Outcome::Succeeded,
                resource_type: "project",
                resource_id: Some(project.get()),
                project_id: Some(project),
                error_code: None,
                details: details.to_value(),
            },
            affected: Vec::new(),
            events: Vec::new(),
        };
        Ok(Settle::Commit(record, (claim, decided)))
    }
    .await;
    tx.settle("document.edit_claim.revoked", outcome).await
}

#[cfg(test)]
#[path = "doc_revisions_tests.rs"]
mod tests;
