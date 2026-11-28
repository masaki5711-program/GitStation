//! The AI asset draft's reads (plan 17 P17-10, ADR-0039 Amendment
//! P17-10): which draft improvement merge requests may get a draft, and
//! the facts its write is decided on. The rows it writes are the gateway's
//! (`llm_requests` / `llm_advice`, purpose `ai_asset_draft`); the draft
//! itself is a branch in git.
//!
//! ADR-0039 Amendment P17-10b: once the branch is written, the worker
//! records the proposal ([`record_proposed`]: the audit row and the event
//! `merge_request.ai_draft_proposed`, which tells the merge request's
//! author); the merge request screen reads it back ([`latest_proposed`],
//! the newest audit row of the merge request — no table of its own).

use forge_core::{
    AuditDetails, DedupKey as EventDedupKey, DedupPart, EventType, MergeRequestChange,
    MergeRequestId, ProjectId,
};
use forge_errors::{ErrorCode, ForgeError};

use crate::change::{
    AuditEntry, ChangeContext, ChangeRecord, ChangeTx, Committed, DomainEvent, Outcome,
};
use crate::{query_error, read_query_error, Db};

/// The audit action (and event type) of a proposal.
const PROPOSED: &str = "merge_request.ai_draft_proposed";
/// The most paths a proposal names (the output policy keeps at most 3).
const MAX_FILES: usize = 3;
/// The longest asset path a proposal may name.
const MAX_PATH_BYTES: usize = 512;

/// The most candidates one pass reads.
pub const MAX_CANDIDATES: i64 = 10;

/// A draft improvement merge request that may get a draft.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub merge_request: MergeRequestId,
    pub project: ProjectId,
    pub iid: i64,
    pub title: String,
    pub description: String,
    pub source_branch: String,
    pub target_branch: String,
    pub head_sha: String,
}

/// The open draft merge requests from `refs/heads/improve/*` with a head,
/// in projects whose `ai_assets.enabled` is on and that are not being
/// deleted, least recently updated first, at most `limit` (read pool).
pub async fn candidates(db: &Db, limit: i64) -> Result<Vec<Candidate>, ForgeError> {
    const QUERY: &str = "llm.asset_draft.candidates";
    type Row = (i64, i64, i64, String, String, String, String, String);
    let rows: Vec<Row> = db
        .within(QUERY, async {
            sqlx::query_as(
                "SELECT m.id, m.project_id, m.iid, m.title, m.description, m.source_branch, \
                   m.target_branch, m.head_sha \
                 FROM forge.merge_requests m \
                 JOIN forge.project_ai_assets a ON a.project_id = m.project_id AND a.enabled \
                 JOIN forge.projects p ON p.id = m.project_id AND p.deleted_at IS NULL \
                 WHERE m.state = 'open' AND m.draft AND m.head_sha IS NOT NULL \
                   AND m.source_branch LIKE 'refs/heads/improve/_%' \
                 ORDER BY m.updated_at, m.id LIMIT $1",
            )
            .bind(limit)
            .fetch_all(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    Ok(rows
        .into_iter()
        .map(|r| Candidate {
            merge_request: MergeRequestId::new(r.0),
            project: ProjectId::new(r.1),
            iid: r.2,
            title: r.3,
            description: r.4,
            source_branch: r.5,
            target_branch: r.6,
            head_sha: r.7,
        })
        .collect())
}

/// The facts the draft write is decided on, read in one statement on the
/// write pool (fresh) right before the write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetFacts {
    pub mr_open: bool,
    pub mr_draft: bool,
    pub source_branch: String,
    pub head_sha: Option<String>,
    /// The project's `ai_assets.enabled` now.
    pub ai_assets_enabled: bool,
    /// The project is being deleted now.
    pub pending_deletion: bool,
}

/// The facts of merge request `mr` of `project` (`None`: no such merge
/// request).
pub async fn target_facts(
    db: &Db,
    project: ProjectId,
    mr: MergeRequestId,
) -> Result<Option<TargetFacts>, ForgeError> {
    const QUERY: &str = "llm.asset_draft.target_facts";
    type Row = (bool, bool, String, Option<String>, Option<bool>, bool);
    let row: Option<Row> = db
        .within(QUERY, async {
            sqlx::query_as(
                "SELECT m.state = 'open', m.draft, m.source_branch, m.head_sha, a.enabled, \
                   p.deleted_at IS NOT NULL \
                 FROM forge.merge_requests m \
                 JOIN forge.projects p ON p.id = m.project_id \
                 LEFT JOIN forge.project_ai_assets a ON a.project_id = m.project_id \
                 WHERE m.id = $1 AND m.project_id = $2",
            )
            .bind(mr.get())
            .bind(project.get())
            .fetch_optional(db.write_pool())
            .await
            .map_err(|e| query_error(QUERY, e))
        })
        .await?;
    Ok(row.map(|r| TargetFacts {
        mr_open: r.0,
        mr_draft: r.1,
        source_branch: r.2,
        head_sha: r.3,
        ai_assets_enabled: r.4.unwrap_or(false),
        pending_deletion: r.5,
    }))
}

/// The draft requests already made about one merge request and input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Asks {
    /// Queued or running.
    pub live: i64,
    /// Ended (succeeded, failed or abandoned) within the last `recent_secs`.
    pub recent: i64,
    /// Every request of this input.
    pub total: i64,
}

/// [`Asks`] of `input_sha256` about merge request `mr`. An answered
/// request does not end the asking by itself: only the draft branch does
/// (an answer whose commit failed, or that proposed nothing acceptable, is
/// asked again within the bounds).
pub async fn asks(
    db: &Db,
    mr: MergeRequestId,
    input_sha256: &str,
    recent_secs: i64,
) -> Result<Asks, ForgeError> {
    const QUERY: &str = "llm.asset_draft.asks";
    let (live, recent, total): (i64, i64, i64) = db
        .within(QUERY, async {
            sqlx::query_as(
                "SELECT count(*) FILTER (WHERE state IN ('queued', 'running')), \
                 count(*) FILTER (WHERE state IN ('succeeded', 'failed', 'abandoned') \
                 AND finished_at > now() - make_interval(secs => $3)), \
                 count(*) \
                 FROM forge.llm_requests \
                 WHERE purpose = 'ai_asset_draft' AND subject_kind = 'merge_request' \
                 AND subject_id = $1 AND input_sha256 = $2",
            )
            .bind(mr.get())
            .bind(input_sha256)
            .bind(recent_secs as f64)
            .fetch_one(db.write_pool())
            .await
            .map_err(|e| query_error(QUERY, e))
        })
        .await?;
    Ok(Asks {
        live,
        recent,
        total,
    })
}

/// The `merge_request.ai_draft_proposed` audit row and event (a
/// [`MergeRequestChange::AiDraftProposed`]) on `project`; `false` when it
/// was recorded already (dedup by the merge request and the commit).
pub async fn record_proposed(
    db: &Db,
    ctx: &ChangeContext,
    project: ProjectId,
    change: MergeRequestChange,
) -> Result<bool, ForgeError> {
    let MergeRequestChange::AiDraftProposed {
        merge_request,
        commit,
        ..
    } = &change
    else {
        return Err(ForgeError::new(ErrorCode::Internal).with_context("reason", "not_an_ai_draft"));
    };
    let dedup = EventDedupKey::new(
        change.dedup_prefix(),
        &[
            DedupPart::Id(merge_request.get()),
            DedupPart::Object(commit),
        ],
    )?;
    let aggregate: MergeRequestId = *merge_request;
    let details = AuditDetails::MergeRequest(change);
    let event_type: EventType = details.event_type().ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "audit_only_event")
    })?;
    let value = details.to_value();
    let record = ChangeRecord {
        audit: AuditEntry {
            action: details.action(),
            outcome: Outcome::Succeeded,
            resource_type: "project",
            resource_id: Some(project.get()),
            project_id: Some(project),
            error_code: None,
            details: value.clone(), // clone: the same JSON is the payload
        },
        affected: Vec::new(),
        events: vec![DomainEvent {
            event_type,
            schema_version: 1,
            aggregate_type: "merge_request",
            aggregate_id: aggregate.get(),
            project_id: Some(project),
            payload: value,
            dedup_key: Some(dedup),
        }],
    };
    let tx = ChangeTx::begin(db, ctx).await?;
    Ok(match tx.commit(record).await? {
        Committed::Recorded { .. } => true,
        Committed::Deduped => false,
    })
}

/// A recorded proposal, as the merge request screen shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProposedDraft {
    /// `refs/heads/ai-draft/<id>`.
    pub branch: String,
    pub commit_sha: String,
    /// The merge request's head the draft commit is over.
    pub parent_sha: String,
    /// The asset paths the commit changes (at most 3).
    pub files: Vec<String>,
    pub dropped_edits: i64,
    pub advice_id: i64,
    /// UTC, RFC 3339.
    pub proposed_at: String,
}

/// The newest proposal recorded for merge request `mr` of `project`
/// (`None`: none). A stored row that does not have the recorded shape is
/// `FG-INTERNAL` with `reason=ai_draft_record_invalid` (never shown half).
pub async fn latest_proposed(
    db: &Db,
    project: ProjectId,
    mr: MergeRequestId,
) -> Result<Option<ProposedDraft>, ForgeError> {
    const QUERY: &str = "llm.asset_draft.latest_proposed";
    let row: Option<(serde_json::Value, String)> = db
        .within(QUERY, async {
            sqlx::query_as(
                "SELECT details, \
                   to_char(occurred_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') \
                 FROM forge.audit_log WHERE project_id = $1 AND action = $2 \
                   AND outcome = 'succeeded' AND details->>'merge_request_id' = $3::text \
                 ORDER BY id DESC LIMIT 1",
            )
            .bind(project.get())
            .bind(PROPOSED)
            .bind(mr.get().to_string())
            .fetch_optional(db.write_pool())
            .await
            .map_err(|e| query_error(QUERY, e))
        })
        .await?;
    row.map(|(d, at)| proposed_from(&d, at)).transpose()
}

/// The stored details as a [`ProposedDraft`] (pure).
fn proposed_from(d: &serde_json::Value, proposed_at: String) -> Result<ProposedDraft, ForgeError> {
    let invalid = |field: &'static str| {
        ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "ai_draft_record_invalid")
            .with_context("field", field)
    };
    let text = |field: &'static str| -> Result<String, ForgeError> {
        d.get(field)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| invalid(field))
    };
    let count = |field: &'static str| -> Result<i64, ForgeError> {
        d.get(field)
            .and_then(serde_json::Value::as_i64)
            .filter(|n| *n >= 0)
            .ok_or_else(|| invalid(field))
    };
    let files = d
        .get("files")
        .and_then(serde_json::Value::as_array)
        .filter(|list| list.len() <= MAX_FILES)
        .ok_or_else(|| invalid("files"))?
        .iter()
        .map(|f| {
            f.as_str()
                .map(str::to_owned)
                .ok_or_else(|| invalid("files"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    // The writer is code; the reader still trusts only the recorded shape
    // (the strings become links on the screen).
    if files
        .iter()
        .any(|f| f.is_empty() || f.len() > MAX_PATH_BYTES)
    {
        return Err(invalid("files"));
    }
    let object = |field: &'static str| -> Result<String, ForgeError> {
        let raw = text(field)?;
        forge_core::ObjectId::parse(&raw).map_err(|_| invalid(field))?;
        Ok(raw)
    };
    let branch = text("branch")?;
    if forge_core::RefName::parse(&branch).is_err() || !branch.starts_with("refs/heads/ai-draft/") {
        return Err(invalid("branch"));
    }
    Ok(ProposedDraft {
        branch,
        commit_sha: object("commit_sha")?,
        parent_sha: object("parent_sha")?,
        files,
        dropped_edits: count("dropped_edits")?,
        advice_id: count("advice_id")?,
        proposed_at,
    })
}

#[cfg(test)]
mod proposed_tests {
    use super::*;

    #[test]
    fn a_stored_proposal_reads_back_and_a_malformed_one_is_refused() {
        let good = serde_json::json!({
            "merge_request_id": 9, "iid": 3, "branch": "refs/heads/ai-draft/ir-1",
            "commit_sha": "b".repeat(40), "parent_sha": "a".repeat(40),
            "files": ["skills/a.md"], "dropped_edits": 1, "advice_id": 5,
            "llm_request_id": 6,
        });
        let read = proposed_from(&good, "2026-11-28T00:00:00Z".to_owned()).expect("read");
        assert_eq!(read.files, ["skills/a.md"]);
        assert_eq!(read.dropped_edits, 1);
        assert_eq!(read.branch, "refs/heads/ai-draft/ir-1");
        for (field, bad) in [
            ("files", serde_json::json!(["a", "b", "c", "d"])),
            ("files", serde_json::json!([1])),
            ("dropped_edits", serde_json::json!(-1)),
            ("commit_sha", serde_json::json!(null)),
            ("commit_sha", serde_json::json!("not-a-sha")),
            ("branch", serde_json::json!("refs/heads/feature")),
            ("files", serde_json::json!([""])),
        ] {
            let mut d = good.clone();
            d[field] = bad;
            let err = proposed_from(&d, String::new()).expect_err(field);
            assert_eq!(err.code(), ErrorCode::Internal, "{field}");
        }
    }
}
