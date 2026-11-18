//! The merge requests auto-merge is told about (plan 18 S3, ADR-0037 (a)).
//!
//! The worker no longer polls every few seconds: it waits for
//! `forge_events_sequenced` (the outbox sequencer's notification) and for
//! `forge_auto_merge` (a mergeability check that answered "can be
//! merged", which is no event unless a conflict went away), decides only
//! the merge requests those name, and sweeps every merge request as the
//! safety net.
//!
//! - [`events_high`]: where the worker starts reading (the events before
//!   it are the first sweep's).
//! - [`named_by_events`]: the open merge requests the events after a
//!   cursor name — a `merge_request` aggregate (opened, pushed, approved,
//!   a resolved discussion, the draft flag, …), and a `check.completed`
//!   on a merge request's head or on its merge result
//!   (`forge.merge_result_refs`: `ci/pipeline` reports there).
//! - [`named`]: the candidates among merge request ids (the payloads of
//!   `forge_auto_merge`).
//!
//! All three are hints' readers: a merge request they do not return is
//! found by the sweep ([`super::candidates`]). Read on the write pool —
//! the answer is about rows committed a moment ago.

use forge_core::ids::{MergeRequestId, ProjectId};
use forge_errors::ForgeError;

use super::Candidate;
use crate::{query_error, Db};

/// The rows auto-merge acts on (the filter of [`super::candidates`],
/// without its recheck clock): open, with a head, in a live project whose
/// `merge.auto` acts. `m` is `forge.merge_requests`.
const ACTS: &str = "m.state = 'open' AND m.head_sha IS NOT NULL \
    AND EXISTS (SELECT 1 FROM forge.projects p \
                 WHERE p.id = m.project_id AND p.deleted_at IS NULL) \
    AND COALESCE((SELECT a.mode FROM forge.project_auto_merge a \
                   WHERE a.project_id = m.project_id), 'full') IN ('conditional', 'full')";

fn candidate((mr, project, iid): (i64, i64, i64)) -> Candidate {
    Candidate {
        merge_request: MergeRequestId::new(mr),
        project: ProjectId::new(project),
        iid,
    }
}

/// The sequence number of the newest sequenced event (0: none yet).
pub async fn events_high(db: &Db) -> Result<i64, ForgeError> {
    const QUERY: &str = "auto_merge.events_high";
    db.within(QUERY, async {
        sqlx::query_scalar("SELECT COALESCE(max(seq), 0) FROM forge.events")
            .fetch_one(db.write_pool())
            .await
            .map_err(|e| query_error(QUERY, e))
    })
    .await
}

/// What [`named_by_events`] read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Named {
    /// The sequence number read up to (the next cursor; the given one when
    /// no event is newer).
    pub upto: i64,
    /// Events read (`limit`: more may be waiting — read again).
    pub events: i64,
    /// The merge requests they name that auto-merge acts on, each once.
    pub candidates: Vec<Candidate>,
}

/// The merge requests named by at most `limit` sequenced events after
/// `after` (module docs). One statement, one snapshot.
pub async fn named_by_events(db: &Db, after: i64, limit: i64) -> Result<Named, ForgeError> {
    const QUERY: &str = "auto_merge.named_by_events";
    type Row = (Option<i64>, i64, Option<i64>, Option<i64>, Option<i64>);
    let rows: Vec<Row> = db
        .within(QUERY, async {
            sqlx::query_as(&format!(
                "WITH ev AS ( \
                   SELECT e.seq, e.event_type, e.aggregate_type, e.aggregate_id, e.project_id, \
                          e.payload ->> 'head_sha' AS sha \
                     FROM forge.events e WHERE e.seq > $1 ORDER BY e.seq LIMIT $2), \
                 hit AS ( \
                   SELECT ev.aggregate_id AS id FROM ev \
                    WHERE ev.aggregate_type = 'merge_request' \
                   UNION \
                   SELECT o.id FROM ev JOIN forge.merge_requests o \
                       ON o.project_id = ev.project_id AND o.state = 'open' \
                      AND o.head_sha = ev.sha \
                    WHERE ev.event_type = 'check.completed' \
                   UNION \
                   SELECT r.merge_request_id FROM ev JOIN forge.merge_result_refs r \
                       ON r.project_id = ev.project_id AND r.result_sha = ev.sha \
                    WHERE ev.event_type = 'check.completed') \
                 SELECT (SELECT max(ev.seq) FROM ev), (SELECT count(*) FROM ev), \
                        c.id, c.project_id, c.iid \
                   FROM (SELECT 1) one \
                   LEFT JOIN (SELECT m.id, m.project_id, m.iid FROM hit \
                                JOIN forge.merge_requests m ON m.id = hit.id \
                               WHERE {ACTS}) c ON true \
                  ORDER BY c.id"
            ))
            .bind(after)
            .bind(limit)
            .fetch_all(db.write_pool())
            .await
            .map_err(|e| query_error(QUERY, e))
        })
        .await?;
    let (upto, events) = rows
        .first()
        .map_or((after, 0), |r| (r.0.unwrap_or(after), r.1));
    let candidates = rows
        .into_iter()
        .filter_map(|(_, _, mr, project, iid)| Some(candidate((mr?, project?, iid?))))
        .collect();
    Ok(Named {
        upto,
        events,
        candidates,
    })
}

/// The candidates among the merge requests `ids` (module docs): the ones
/// auto-merge acts on, each once, whatever their last look was.
pub async fn named(db: &Db, ids: &[i64]) -> Result<Vec<Candidate>, ForgeError> {
    const QUERY: &str = "auto_merge.named";
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let rows: Vec<(i64, i64, i64)> = db
        .within(QUERY, async {
            sqlx::query_as(&format!(
                "SELECT m.id, m.project_id, m.iid FROM forge.merge_requests m \
                  WHERE m.id = ANY($1::bigint[]) AND {ACTS} ORDER BY m.id"
            ))
            .bind(ids)
            .fetch_all(db.write_pool())
            .await
            .map_err(|e| query_error(QUERY, e))
        })
        .await?;
    Ok(rows.into_iter().map(candidate).collect())
}
