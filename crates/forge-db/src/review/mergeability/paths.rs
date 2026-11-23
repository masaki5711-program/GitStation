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
//!   **current head** (`checked_head_sha = m.head_sha`);
//! - it is only taken from a row whose path set is its **own**
//!   ([`OWN_SET`]): `run::record_check` stamps the set with the head it
//!   checked (`paths_head_sha`) and its request's seq (`paths_seq`) in the
//!   UPDATE that writes the result. A writer that does not know the
//!   columns — the binary before them, while serve and worker are
//!   restarted one after the other or after a rollback — stores its result
//!   under a new `check_seq` and leaves the set, the stamp and
//!   `computed_target_sha` of an earlier check behind; that row's stamp
//!   does not match, so it is checked again instead of being carried on a
//!   set that is not its head's (or was computed against another tip). A
//!   check request bumps `check_seq` too: a row waiting for its check is
//!   not its own either, and is not decided anyway;
//! - it is only taken from a set of this binary's **rule version**
//!   (`paths_rule = RULE_VERSION`): a set built for another version of the
//!   rule is unknown;
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
//! computed against (set by `record_check`). A row is **carried** exactly
//! when it is its own and the two differ; every reader takes the column
//! through the same stamp (`STATUS_COLUMNS`, [`recheck_if_carried`]), so a
//! row stored by a writer that does not know the columns is never shown
//! or treated as carried. The carried answer gates what is shown and
//! what may be enqueued; the merge itself rebuilds the result
//! (`FG-REVIEW-0006` when it conflicts after all), after which
//! [`recheck_if_carried`] replaces the carried answer by a computed one.

use forge_core::ids::{MergeRequestId, ProjectId};
use forge_core::merge_paths::{LandedPaths, PathSet, RULE_VERSION};
use forge_core::{ObjectId, RequestId};
use forge_errors::ForgeError;
use sqlx::postgres::PgConnection;

use super::{request_check_on, ANSWERING};
use crate::{query_error, Db};

/// True for a row `g` whose stored path set and `computed_target_sha`
/// were written with its checked pair (module docs): the stamp of the
/// check that wrote them is the checked head and the row's current seq.
/// NULL (not true) for a row that never had a stamp.
pub(super) const OWN_SET: &str = own_set!();

/// The rule version as it is stored (`paths_rule`).
pub(super) fn rule_version() -> i32 {
    // A small constant; a value that does not fit can never be stored,
    // so nothing would be carried.
    i32::try_from(RULE_VERSION).unwrap_or(i32::MAX)
}

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
///
/// **Bounds.** The statement runs on a write-pool session, which carries
/// `statement_timeout` (`database.statement_timeout_ms`, 15 s by default)
/// and `lock_timeout` (`database.lock_timeout_ms`) from its connect
/// options (`options::connect_options`); the subscriber's handler is
/// bounded again on the client (`subscribers::run`: `Timeouts::handler`
/// around `handle`, `Timeouts::step` around the step). A timeout fails the
/// handler: its savepoint is rolled back, the failure counted, the event
/// retried — nothing is carried by half. The bound lists are at most
/// `LANDED_BUDGET` bytes (enforced where the block is written and where
/// it is read); the candidate rows are the open merge requests of one
/// project and target branch, found through the partial unique index
/// `merge_requests_one_open (project_id, source_branch, target_branch)
/// WHERE state = 'open'` (the project's open rows, the target branch
/// filtered in the index; `merge_requests_open_project (project_id, id)
/// WHERE state = 'open'` reaches the same rows), then this table's
/// primary key — the narrowing the fan-out statement of the refresh has
/// always used.
///
/// **Locks.** Only the mergeability rows it moves are locked (the target
/// of the UPDATE); `forge.merge_requests` is read without a lock. That
/// read of `head_sha` is safe because a new version writes the
/// mergeability row *after* the head, in the same transaction
/// (`versions`: `move_head`, then `request::downgrade_on`): a version
/// that committed first is seen (the head no longer equals
/// `checked_head_sha`, the row is `unchecked`); one that holds the
/// mergeability row makes this statement wait and re-check the row
/// (READ COMMITTED: `unchecked` now, not taken); one that has moved the
/// head but not yet reached the mergeability row waits for this
/// statement's row lock and then downgrades the carried row, whose
/// `checked_head_sha` is not the new head. In every order the row of a
/// new head ends `unchecked`. No lock on `forge.merge_requests` means no
/// lock order between the two tables, so no deadlock with the version,
/// edit or merge transactions, which lock the merge request first.
pub(crate) async fn carry_forward_on(
    conn: &mut PgConnection,
    project: ProjectId,
    landed: &LandedPaths,
) -> Result<Vec<i64>, ForgeError> {
    let twin = interacts_sql("$5", "$6", "$7", "$8");
    sqlx::query_scalar(&format!(
        "UPDATE forge.merge_request_mergeability g SET checked_target_sha = $4, \
         updated_at = now() \
         FROM forge.merge_requests m \
         WHERE m.id = g.merge_request_id AND m.project_id = $1 AND m.state = 'open' \
         AND m.target_branch = $2 AND g.status IN ('can_be_merged', 'cannot_be_merged') \
         AND g.checked_head_sha = m.head_sha AND g.checked_target_sha = $3 \
         AND {OWN_SET} AND g.paths_rule = $9 AND g.computed_target_sha IS NOT NULL \
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
    .bind(rule_version())
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

/// Plan 18 S6 (ADR-0037 (f)): the SQL of "the open merge request `m` may
/// interact with merge request `{me}`" for the conflict scan's peers — the
/// S4 predicate ([`interacts_sql`]) between the two head path sets, each
/// counted only when it is its row's own ([`OWN_SET`]), of the current rule
/// version (`{rule}`) and for the merge request's current head. Unknown on
/// either side (no row, another head, incomplete) is "may interact": the
/// pair is scanned, as before the filter. Never NULL.
pub(crate) fn peer_interacts_sql(me: &str, rule: &str) -> String {
    let own = |alias: &str, mr: &str| {
        format!(
            "{alias}.paths_head_sha = {alias}.checked_head_sha AND {alias}.paths_seq = {alias}.check_seq              AND {alias}.paths_rule = {rule} AND {alias}.checked_head_sha = {mr}.head_sha"
        )
    };
    let mine = |column: &str| {
        format!(
            "(SELECT s.{column} FROM forge.merge_request_mergeability s              JOIN forge.merge_requests sm ON sm.id = s.merge_request_id              WHERE s.merge_request_id = {me} AND {})",
            own("s", "sm")
        )
    };
    let interacts = interacts_sql(
        &mine("head_files"),
        &mine("head_dirs"),
        &mine("head_deleted_dirs"),
        &mine("paths_complete"),
    );
    format!(
        "(NOT EXISTS (SELECT 1 FROM forge.merge_request_mergeability g           WHERE g.merge_request_id = m.id AND {})          OR EXISTS (SELECT 1 FROM forge.merge_request_mergeability g           WHERE g.merge_request_id = m.id AND {} AND {interacts}))",
        own("g", "m"),
        own("g", "m"),
    )
}

/// The current rule version as the bound value of
/// [`peer_interacts_sql`]'s `rule`.
pub(crate) fn paths_rule() -> i32 {
    rule_version()
}

/// The stored changed-path set of `mr`'s checked head (`None`: no row, or
/// unknown — never read, stored before the columns existed, not the
/// row's own ([`OWN_SET`]) or built for another rule version). One
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
            sqlx::query_as(&format!(
                "SELECT g.head_files, g.head_dirs, g.head_deleted_dirs, g.paths_complete \
                 FROM forge.merge_request_mergeability g WHERE g.merge_request_id = $1 \
                 AND {OWN_SET} AND g.paths_rule = $2"
            ))
            .bind(mr.get())
            .bind(rule_version())
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
        let project: Option<i64> = sqlx::query_scalar(&format!(
            "SELECT m.project_id FROM forge.merge_request_mergeability g \
             JOIN forge.merge_requests m ON m.id = g.merge_request_id \
             WHERE g.merge_request_id = $1 AND m.state = 'open' \
             AND g.status IN ('can_be_merged', 'cannot_be_merged') \
             AND {OWN_SET} AND g.computed_target_sha IS NOT NULL \
             AND g.computed_target_sha <> g.checked_target_sha FOR UPDATE OF g"
        ))
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
