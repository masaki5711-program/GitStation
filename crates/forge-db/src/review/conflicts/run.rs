//! Running scans (see the module documentation of `conflicts`).

use forge_core::ids::{MergeRequestId, ProjectId};
use forge_core::{
    AuditDetails, DedupKey, DedupPart, MergeRequestChange, MergeRequestState, ObjectId, RefName,
};
use forge_errors::{ErrorCode, ForgeError};

use super::{PairOutcome, ScanStatus};
use crate::change::{
    AuditEntry, ChangeContext, ChangeRecord, ChangeTx, DomainEvent, Outcome, Settle,
};
use crate::{query_error, Db};

/// How a run starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanStart {
    /// A newer request exists (or the row is gone): its job scans.
    Superseded,
    /// Status `scanning`; scan.
    Scan,
}

/// Start the run of request `seq` of `mr`. The last reason stays until the
/// result is stored (a reader still sees why the last scan ended).
pub async fn begin_scan(db: &Db, mr: MergeRequestId, seq: i64) -> Result<ScanStart, ForgeError> {
    const QUERY: &str = "review.conflict_scan_begin";
    db.within(QUERY, async {
        let mut conn = db
            .write_pool()
            .acquire()
            .await
            .map_err(|e| query_error(QUERY, e))?;
        let started = sqlx::query(
            "UPDATE forge.merge_request_conflict_scans SET status = 'scanning', \
             updated_at = now() WHERE merge_request_id = $1 AND scan_seq = $2",
        )
        .bind(mr.get())
        .bind(seq)
        .execute(&mut *conn)
        .await
        .map_err(|e| query_error(QUERY, e))?;
        Ok(if started.rows_affected() == 1 {
            ScanStart::Scan
        } else {
            ScanStart::Superseded
        })
    })
    .await
}

/// End request `seq` of `mr` without a result (the merge request is no
/// longer open): `failed` with `reason`, so no row stays `scanning` (a
/// reopen requests a new scan). `true` when written.
pub async fn abandon_scan(
    db: &Db,
    mr: MergeRequestId,
    seq: i64,
    reason: &'static str,
) -> Result<bool, ForgeError> {
    const QUERY: &str = "review.conflict_scan_abandon";
    db.within(QUERY, async {
        let mut conn = db
            .write_pool()
            .acquire()
            .await
            .map_err(|e| query_error(QUERY, e))?;
        sqlx::query(
            "UPDATE forge.merge_request_conflict_scans SET status = 'failed', reason = $3, \
             updated_at = now() WHERE merge_request_id = $1 AND scan_seq = $2 \
             AND status IN ('pending', 'scanning')",
        )
        .bind(mr.get())
        .bind(seq)
        .bind(reason)
        .execute(&mut *conn)
        .await
        .map(|r| r.rows_affected() > 0)
        .map_err(|e| query_error(QUERY, e))
    })
    .await
}

/// Another open merge request on the same target branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peer {
    pub merge_request: MergeRequestId,
    pub iid: i64,
    /// Its recorded head (the hidden version ref keeps the commit).
    pub head: ObjectId,
    pub draft: bool,
}

/// The peers of a scan ([`scan_peers`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peers {
    /// At most the caller's limit, most recently updated first.
    pub peers: Vec<Peer>,
    /// How many there are in all (more than `peers`: past the cap).
    pub total: i64,
    /// Peers whose stored head is not an object id (never written by
    /// forge; logged by the caller, the scan is `incomplete peer_invalid`).
    pub invalid: Vec<MergeRequestId>,
}

/// The open merge requests of `project` on `target` but `mr` that have a
/// head (drafts included), most recently updated first, at most `limit`;
/// and how many there are in all.
pub async fn scan_peers(
    db: &Db,
    project: ProjectId,
    target: &RefName,
    mr: MergeRequestId,
    limit: i64,
) -> Result<Peers, ForgeError> {
    const QUERY: &str = "review.conflict_scan_peers";
    db.within(QUERY, async {
        let mut conn = db
            .write_pool()
            .acquire()
            .await
            .map_err(|e| query_error(QUERY, e))?;
        let rows: Vec<(i64, i64, String, bool)> = sqlx::query_as(
            "SELECT id, iid, head_sha, draft FROM forge.merge_requests \
             WHERE project_id = $1 AND state = 'open' AND target_branch = $2 AND id <> $3 \
             AND head_sha IS NOT NULL ORDER BY updated_at DESC, id DESC LIMIT $4",
        )
        .bind(project.get())
        .bind(target.as_str())
        .bind(mr.get())
        .bind(limit)
        .fetch_all(&mut *conn)
        .await
        .map_err(|e| query_error(QUERY, e))?;
        let total: i64 = if i64::try_from(rows.len()).unwrap_or(i64::MAX) < limit {
            i64::try_from(rows.len()).unwrap_or(i64::MAX)
        } else {
            sqlx::query_scalar(
                "SELECT count(*) FROM forge.merge_requests WHERE project_id = $1 \
                 AND state = 'open' AND target_branch = $2 AND id <> $3 \
                 AND head_sha IS NOT NULL",
            )
            .bind(project.get())
            .bind(target.as_str())
            .bind(mr.get())
            .fetch_one(&mut *conn)
            .await
            .map_err(|e| query_error(QUERY, e))?
        };
        let mut peers = Vec::with_capacity(rows.len());
        let mut invalid = Vec::new();
        for (id, iid, head, draft) in rows {
            let other = MergeRequestId::new(id);
            match ObjectId::parse(&head) {
                Ok(head) => peers.push(Peer {
                    merge_request: other,
                    iid,
                    head,
                    draft,
                }),
                Err(_) => invalid.push(other),
            }
        }
        Ok(Peers {
            peers,
            total,
            invalid,
        })
    })
    .await
}

/// What a scan found for one pair (this merge request and `other`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairResult {
    pub other: MergeRequestId,
    pub other_iid: i64,
    pub other_head: ObjectId,
    pub outcome: PairOutcome,
    pub reason: Option<&'static str>,
    pub conflicts: Vec<String>,
    pub conflicts_truncated: bool,
}

/// The result of one scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanResult {
    /// `Scanned`, `Incomplete` or `Failed`.
    pub status: ScanStatus,
    pub reason: Option<&'static str>,
    /// The head scanned (`None`: no version yet).
    pub head: Option<ObjectId>,
    pub target_branch: RefName,
    /// The target tip the scan read (`None`: not read).
    pub target_sha: Option<ObjectId>,
    pub peers: i32,
    pub pairs_tried: i32,
    pub pairs_skipped: i32,
    pub pairs: Vec<PairResult>,
}

/// What [`record_scan`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanRecorded {
    /// A newer request exists, the merge request is no longer open, or its
    /// head or target moved since the run started: nothing written.
    Superseded,
    /// Stored; `predicted`: the other merge requests of the pairs that
    /// entered `conflict` (an event each side); `skipped`: pairs not
    /// written because the other side moved since the scan read it.
    Stored {
        predicted: Vec<MergeRequestId>,
        skipped: usize,
        /// What was stored (`incomplete peer_moved` when a pair was
        /// skipped).
        status: ScanStatus,
        reason: Option<&'static str>,
    },
}

/// `(scan_seq, project_id, iid, state, head_sha, target_branch)` under the
/// scan row's lock.
type LockedRow = (i64, i64, i64, String, Option<String>, String);

fn audit_entry(project: ProjectId, details: &AuditDetails) -> AuditEntry {
    AuditEntry {
        action: details.action(),
        outcome: Outcome::Succeeded,
        resource_type: "project",
        resource_id: Some(project.get()),
        project_id: Some(project),
        error_code: None,
        details: details.to_value(),
    }
}

/// The event of one side of a pair that entered `conflict`.
fn side_event(
    project: ProjectId,
    change: MergeRequestChange,
) -> Result<(AuditEntry, DomainEvent), ForgeError> {
    let MergeRequestChange::ConflictPredicted {
        merge_request,
        other_merge_request,
        prediction_seq,
        ..
    } = &change
    else {
        return Err(ForgeError::new(ErrorCode::Internal).with_context("reason", "not_a_prediction"));
    };
    let dedup = DedupKey::new(
        change.dedup_prefix(),
        &[
            DedupPart::Id(merge_request.get()),
            DedupPart::Id(other_merge_request.get()),
            DedupPart::Id(*prediction_seq),
        ],
    )?;
    let aggregate = merge_request.get();
    let details = AuditDetails::MergeRequest(change);
    let event_type = details.event_type().ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "audit_only_event")
    })?;
    Ok((
        audit_entry(project, &details),
        DomainEvent {
            event_type,
            schema_version: 1,
            aggregate_type: "merge_request",
            aggregate_id: aggregate,
            project_id: Some(project),
            payload: details.to_value(),
            dedup_key: Some(dedup),
        },
    ))
}

/// The facts of this side for one pair's write.
struct Side {
    mr: MergeRequestId,
    project: ProjectId,
    head: ObjectId,
    target: RefName,
}

/// Write `pair` of `side` on `tx`: the row is created if missing, locked,
/// and updated: `Some(Some(seq))` when it entered `conflict` (its new
/// `prediction_seq`), `Some(None)` otherwise; `None` (nothing written) when
/// the other side is no longer the one scanned.
async fn write_pair(
    tx: &mut ChangeTx,
    side: &Side,
    pair: &PairResult,
) -> Result<Option<Option<i64>>, ForgeError> {
    let other = pair.other;
    let other_now: Option<(String, Option<String>, String)> = tx
        .step(|c| async move {
            sqlx::query_as(
                "SELECT state, head_sha, target_branch FROM forge.merge_requests WHERE id = $1",
            )
            .bind(other.get())
            .fetch_optional(c)
            .await
            .map_err(|e| query_error("review.conflict_pair_other", e))
        })
        .await?;
    let live = other_now.is_some_and(|(state, head, target)| {
        state == MergeRequestState::Open.as_str()
            && head.as_deref() == Some(pair.other_head.as_str())
            && target == side.target.as_str()
    });
    if !live {
        // The other side moved (or closed) since the scan read it: its own
        // scan, or this one's next, writes the pair.
        return Ok(None);
    }
    let (low, high, low_head, high_head) = if side.mr.get() < other.get() {
        (side.mr, other, &side.head, &pair.other_head)
    } else {
        (other, side.mr, &pair.other_head, &side.head)
    };
    let (project, target) = (side.project, side.target.as_str());
    let (low_head, high_head) = (low_head.as_str(), high_head.as_str());
    tx.step(|c| async move {
        // Create the row so the lock below always has one to wait on (two
        // scans of the same pair serialize here, never both "first").
        sqlx::query(
            "INSERT INTO forge.merge_request_conflict_pairs (low_mr_id, high_mr_id, project_id, \
             low_head_sha, high_head_sha, target_branch, outcome) \
             VALUES ($1, $2, $3, $4, $5, $6, 'no_overlap') \
             ON CONFLICT (low_mr_id, high_mr_id) DO NOTHING",
        )
        .bind(low.get())
        .bind(high.get())
        .bind(project.get())
        .bind(low_head)
        .bind(high_head)
        .bind(target)
        .execute(c)
        .await
        .map(drop)
        .map_err(|e| query_error("review.conflict_pair_create", e))
    })
    .await?;
    let (previous, seq): (Option<String>, i64) = tx
        .step(|c| async move {
            sqlx::query_as(
                "SELECT last_decided, prediction_seq FROM forge.merge_request_conflict_pairs \
                 WHERE low_mr_id = $1 AND high_mr_id = $2 FOR UPDATE",
            )
            .bind(low.get())
            .bind(high.get())
            .fetch_one(c)
            .await
            .map_err(|e| query_error("review.conflict_pair_lock", e))
        })
        .await?;
    // Measured against the last outcome that was not `failed`: conflict →
    // failed → conflict is one prediction (and one mail), not two.
    let entered = pair.outcome == PairOutcome::Conflict
        && previous.as_deref().and_then(PairOutcome::parse) != Some(PairOutcome::Conflict);
    let decided = (pair.outcome != PairOutcome::Failed).then(|| pair.outcome.as_str());
    let seq = if entered { seq + 1 } else { seq };
    let outcome = pair.outcome.as_str();
    let reason = pair.reason;
    let (conflicts, truncated) = (&pair.conflicts, pair.conflicts_truncated);
    tx.step(|c| async move {
        sqlx::query(
            "UPDATE forge.merge_request_conflict_pairs SET low_head_sha = $3, \
             high_head_sha = $4, target_branch = $5, outcome = $6, reason = $7, \
             conflicts = $8, conflicts_truncated = $9, prediction_seq = $10, \
             last_decided = COALESCE($11, last_decided), \
             checked_at = now(), updated_at = now() WHERE low_mr_id = $1 AND high_mr_id = $2",
        )
        .bind(low.get())
        .bind(high.get())
        .bind(low_head)
        .bind(high_head)
        .bind(target)
        .bind(outcome)
        .bind(reason)
        .bind(conflicts)
        .bind(truncated)
        .bind(seq)
        .bind(decided)
        .execute(c)
        .await
        .map(drop)
        .map_err(|e| query_error("review.conflict_pair_store", e))
    })
    .await?;
    Ok(Some(entered.then_some(seq)))
}

/// Store `result` for request `seq` of `mr`, as `ctx`'s actor (the
/// worker: `system`): only while `seq` is the latest, the merge request is
/// open and its head and target are the ones scanned.
pub async fn record_scan(
    db: &Db,
    ctx: &ChangeContext,
    mr: MergeRequestId,
    seq: i64,
    result: &ScanResult,
) -> Result<ScanRecorded, ForgeError> {
    const ACTION: &str = "merge_request.conflict_scanned";
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let row: Option<LockedRow> = tx
            .step(|c| async move {
                sqlx::query_as(
                    "SELECT s.scan_seq, m.project_id, m.iid, m.state, m.head_sha, \
                     m.target_branch FROM forge.merge_request_conflict_scans s \
                     JOIN forge.merge_requests m ON m.id = s.merge_request_id \
                     WHERE s.merge_request_id = $1 FOR UPDATE OF s",
                )
                .bind(mr.get())
                .fetch_optional(c)
                .await
                .map_err(|e| query_error("review.conflict_scan_lock", e))
            })
            .await?;
        let Some((latest, project, iid, state, head, target)) = row else {
            return Ok(Settle::Discard(ScanRecorded::Superseded));
        };
        let scanned_head = result.head.as_ref().map(ObjectId::as_str);
        if latest == seq && state != MergeRequestState::Open.as_str() {
            // Closed or merged mid-run: never left `scanning` (review LOW);
            // a reopen requests a new scan.
            tx.step(|c| async move {
                sqlx::query(
                    "UPDATE forge.merge_request_conflict_scans SET status = 'failed', \
                     reason = 'not_open', updated_at = now() WHERE merge_request_id = $1",
                )
                .bind(mr.get())
                .execute(c)
                .await
                .map(drop)
                .map_err(|e| query_error("review.conflict_scan_closed", e))
            })
            .await?;
            return Ok(Settle::CommitDerived(ScanRecorded::Superseded));
        }
        if latest != seq
            || head.as_deref() != scanned_head
            || target != result.target_branch.as_str()
        {
            return Ok(Settle::Discard(ScanRecorded::Superseded));
        }
        let project = ProjectId::new(project);
        let mut audits = Vec::new();
        let mut events = Vec::new();
        let mut predicted = Vec::new();
        let mut skipped = 0usize;
        if let Some(head) = result.head.as_ref() {
            let side = Side {
                mr,
                project,
                head: head.clone(), // clone: the side owns its id
                target: result.target_branch.clone(), // clone: as above
            };
            // A stable lock order across scans: by the pair's (low, high).
            let mut pairs: Vec<&PairResult> = result.pairs.iter().collect();
            pairs.sort_by_key(|p| {
                let (a, b) = (mr.get(), p.other.get());
                (a.min(b), a.max(b))
            });
            for pair in pairs {
                match write_pair(&mut tx, &side, pair).await? {
                    None => skipped += 1,
                    Some(None) => {}
                    Some(Some(prediction_seq)) => {
                        let conflict_files = pair.conflicts.len();
                        let files_truncated = pair.conflicts_truncated;
                        for (this, this_iid, this_head, other, other_iid, other_head) in [
                            (mr, iid, head, pair.other, pair.other_iid, &pair.other_head),
                            (pair.other, pair.other_iid, &pair.other_head, mr, iid, head),
                        ] {
                            let (audit, event) = side_event(
                                project,
                                MergeRequestChange::ConflictPredicted {
                                    merge_request: this,
                                    iid: this_iid,
                                    other_merge_request: other,
                                    other_iid,
                                    head_sha: this_head.clone(), // clone: the change owns its ids
                                    other_head_sha: other_head.clone(), // clone: as above
                                    target_branch: result.target_branch.clone(), // clone: as above
                                    prediction_seq,
                                    conflict_files,
                                    files_truncated,
                                },
                            )?;
                            audits.push(audit);
                            events.push(event);
                        }
                        predicted.push(pair.other);
                    }
                }
            }
        }
        // A peer that moved or closed after the scan read it was not
        // written: say so (review MEDIUM) — its own scan, or the next one
        // of this merge request, writes the pair.
        let (status, reason) = if skipped > 0 && result.status == ScanStatus::Scanned {
            (ScanStatus::Incomplete, Some("peer_moved"))
        } else {
            (result.status, result.reason)
        };
        let skipped_total = result
            .pairs_skipped
            .saturating_add(i32::try_from(skipped).unwrap_or(i32::MAX));
        let target_sha = result.target_sha.as_ref().map(ObjectId::as_str);
        tx.step(|c| async move {
            sqlx::query(
                "UPDATE forge.merge_request_conflict_scans SET status = $2, reason = $3, \
                 scanned_head_sha = $4, scanned_target_branch = $5, scanned_at = now(), \
                 peers = $6, pairs_tried = $7, pairs_skipped = $8, scanned_target_sha = $9, \
                 sweep_attempts = CASE WHEN $2 = 'scanned' THEN 0 ELSE sweep_attempts END, \
                 updated_at = now() WHERE merge_request_id = $1",
            )
            .bind(mr.get())
            .bind(status.as_str())
            .bind(reason)
            .bind(scanned_head)
            .bind(result.target_branch.as_str())
            .bind(result.peers)
            .bind(result.pairs_tried)
            .bind(skipped_total)
            .bind(target_sha)
            .execute(c)
            .await
            .map(drop)
            .map_err(|e| query_error("review.conflict_scan_store", e))
        })
        .await?;
        let recorded = ScanRecorded::Stored {
            predicted,
            skipped,
            status,
            reason,
        };
        let mut audits = audits.into_iter();
        let Some(audit) = audits.next() else {
            return Ok(Settle::CommitDerived(recorded));
        };
        Ok(Settle::Commit(
            ChangeRecord {
                audit,
                affected: audits.collect(),
                events,
            },
            recorded,
        ))
    }
    .await;
    tx.settle(ACTION, outcome).await
}
