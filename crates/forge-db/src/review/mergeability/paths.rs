//! Path-aware carry-forward (plan 18 S4, ADR-0037 (c); see the module
//! documentation of `mergeability` and forge-core's `merge_paths`).
//!
//! A landing used to request a check for every open merge request
//! targeting the branch. With the landed paths in the `repository.pushed`
//! payload, the `merge-request-refresh` subscriber first runs
//! [`carry_forward_on`]: ONE statement that moves `checked_target_sha` of
//! the decided rows the landing cannot interact with from the landing's
//! old tip to its new one. Only the others are requested.
//!
//! What makes a carried row true:
//!
//! - it is only taken from a row that answers for the merge request's
//!   **current head** (`checked_head_sha = m.head_sha`) — the stored path
//!   set was written by the check that wrote that head's result
//!   (`run::record_check`, one UPDATE), so it is that head's set;
//! - it is only taken from a row that answers for the landing's **old
//!   tip** (`checked_target_sha = <old>`): the result held at `old`, the
//!   landing changed nothing the head's paths can interact with, so it
//!   holds at `new`. A row at any other tip (an event that arrives late or
//!   twice, a second landing whose first was not carried) is not carried;
//! - only `can_be_merged` / `cannot_be_merged` are carried (an undecided
//!   row has nothing to keep, a row being checked gets its own answer);
//! - the statement locks each row it moves. A check, a new version's
//!   downgrade or a request that commits first is seen by the statement's
//!   re-check of the row (READ COMMITTED); one that comes after overwrites
//!   the carried value as it would any other.
//!
//! `computed_target_sha` is not moved: it keeps the tip the result was
//! computed against (set by `record_check`; a row older than the column
//! takes its `checked_target_sha` on the first carry). A row is **carried**
//! exactly when the two differ. The carried answer gates what is shown and
//! what may be enqueued; the merge itself rebuilds the result
//! (`FG-REVIEW-0006` when it conflicts after all), after which
//! [`recheck_if_carried`] replaces the carried answer by a computed one.

use forge_core::ids::{MergeRequestId, ProjectId};
use forge_core::merge_paths::{LandedPaths, PathSet};
use forge_core::{ObjectId, RequestId};
use forge_errors::ForgeError;
use sqlx::postgres::PgConnection;

use super::{request_check_on, ANSWERING};
use crate::{query_error, Db};

/// The SQL twin of `forge_core::merge_paths::may_interact` (rule version
/// 1): a boolean expression over the mergeability row `g` and four bound
/// parameters — the landed set's files, directories, deleted directories
/// (`text[]`) and whether it is complete (`boolean`), given as placeholders
/// (`"$4"`). True (check again) when either set is unknown or incomplete,
/// or the lists share a key; never NULL. The property test
/// `may_interact_matches_its_sql_twin` proves the two agree.
pub fn interacts_sql(files: &str, dirs: &str, deleted: &str, complete: &str) -> String {
    format!(
        "(g.paths_complete IS NOT TRUE OR g.head_files IS NULL OR g.head_dirs IS NULL \
         OR g.head_deleted_dirs IS NULL OR {complete}::boolean IS NOT TRUE \
         OR {files}::text[] IS NULL OR {dirs}::text[] IS NULL OR {deleted}::text[] IS NULL \
         OR g.head_files && {files}::text[] OR g.head_files && {dirs}::text[] \
         OR g.head_dirs && {files}::text[] OR g.head_dirs && {deleted}::text[] \
         OR g.head_deleted_dirs && {dirs}::text[])"
    )
}

/// Carry the decided results of the open merge requests of `project`
/// targeting `landed.target` forward from `landed.old` to `landed.new`
/// where the landing cannot interact with their paths (module docs), on
/// `conn`, the subscriber's transaction. The merge requests carried.
pub(crate) async fn carry_forward_on(
    conn: &mut PgConnection,
    project: ProjectId,
    landed: &LandedPaths,
) -> Result<Vec<i64>, ForgeError> {
    let twin = interacts_sql("$5", "$6", "$7", "$8");
    sqlx::query_scalar(&format!(
        "UPDATE forge.merge_request_mergeability g SET checked_target_sha = $4, \
         computed_target_sha = COALESCE(g.computed_target_sha, g.checked_target_sha), \
         updated_at = now() \
         FROM forge.merge_requests m \
         WHERE m.id = g.merge_request_id AND m.project_id = $1 AND m.state = 'open' \
         AND m.target_branch = $2 AND g.status IN ('can_be_merged', 'cannot_be_merged') \
         AND g.checked_head_sha = m.head_sha AND g.checked_target_sha = $3 \
         AND NOT {twin} \
         RETURNING g.merge_request_id"
    ))
    .bind(project.get())
    .bind(landed.target.as_str())
    .bind(landed.old.as_str())
    .bind(landed.new.as_str())
    .bind(landed.paths.files())
    .bind(landed.paths.dirs())
    .bind(landed.paths.deleted_dirs())
    .bind(landed.paths.is_complete())
    .fetch_all(conn)
    .await
    .map_err(|e| query_error("review.mergeability_carry", e))
}

/// The open merge requests of `project` targeting `target` whose row
/// already answers for their head and `tip` — carried by an earlier event
/// of the same landing (a queue batch writes one per entry), by an earlier
/// run of this one, or checked after the branch moved: nothing to request.
pub(crate) async fn settled_on(
    conn: &mut PgConnection,
    project: ProjectId,
    target: &str,
    tip: &ObjectId,
) -> Result<Vec<i64>, ForgeError> {
    sqlx::query_scalar(&format!(
        "SELECT g.merge_request_id FROM forge.merge_request_mergeability g \
         JOIN forge.merge_requests m ON m.id = g.merge_request_id \
         WHERE m.project_id = $1 AND m.state = 'open' AND m.target_branch = $2 \
         AND {ANSWERING} AND g.checked_head_sha = m.head_sha AND g.checked_target_sha = $3"
    ))
    .bind(project.get())
    .bind(target)
    .bind(tip.as_str())
    .fetch_all(conn)
    .await
    .map_err(|e| query_error("review.mergeability_settled", e))
}

/// The stored changed-path set of `mr`'s checked head (`None`: no row, or
/// unknown — never read, or stored before the columns existed). One
/// bounded statement on the write pool (the tests and the diagnosis read
/// what was just committed).
pub async fn stored_paths(db: &Db, mr: MergeRequestId) -> Result<Option<PathSet>, ForgeError> {
    const QUERY: &str = "review.mergeability_paths";
    type Row = (
        Option<Vec<String>>,
        Option<Vec<String>>,
        Option<Vec<String>>,
        Option<bool>,
    );
    let row: Option<Row> = db
        .within(QUERY, async {
            let mut conn = db
                .write_pool()
                .acquire()
                .await
                .map_err(|e| query_error(QUERY, e))?;
            sqlx::query_as(
                "SELECT head_files, head_dirs, head_deleted_dirs, paths_complete \
                 FROM forge.merge_request_mergeability WHERE merge_request_id = $1",
            )
            .bind(mr.get())
            .fetch_optional(&mut *conn)
            .await
            .map_err(|e| query_error(QUERY, e))
        })
        .await?;
    Ok(row.and_then(|(files, dirs, deleted, complete)| {
        PathSet::from_columns(files, dirs, deleted, complete)
    }))
}

/// Request a computed check of `mr` when its stored result is a carried
/// one (the merge found a conflict the carried answer did not show:
/// `FG-REVIEW-0006` in the train). `Ok(None)`: the row is not carried, or
/// the merge request is gone or not open — nothing requested. The new seq
/// otherwise.
pub async fn recheck_if_carried(
    db: &Db,
    mr: MergeRequestId,
    origin: RequestId,
) -> Result<Option<i64>, ForgeError> {
    const QUERY: &str = "review.mergeability_recheck_carried";
    db.within(QUERY, async {
        let mut tx = db
            .write_pool()
            .begin()
            .await
            .map_err(|e| query_error(QUERY, e))?;
        let project: Option<i64> = sqlx::query_scalar(
            "SELECT m.project_id FROM forge.merge_request_mergeability g \
             JOIN forge.merge_requests m ON m.id = g.merge_request_id \
             WHERE g.merge_request_id = $1 AND m.state = 'open' \
             AND g.status IN ('can_be_merged', 'cannot_be_merged') \
             AND g.computed_target_sha IS NOT NULL \
             AND g.computed_target_sha <> g.checked_target_sha FOR UPDATE OF g",
        )
        .bind(mr.get())
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| query_error(QUERY, e))?;
        let Some(project) = project else {
            return Ok(None);
        };
        let seq = request_check_on(&mut tx, mr, ProjectId::new(project), origin, None).await?;
        tx.commit().await.map_err(|e| query_error(QUERY, e))?;
        Ok(Some(seq))
    })
    .await
}
