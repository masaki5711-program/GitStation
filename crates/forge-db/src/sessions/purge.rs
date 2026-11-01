//! `sessions::purge_expired` (M2 MR6, the MR3 handoff's deferred cleanup):
//! delete web sessions [`PURGE_AFTER_DAYS`] days past their revocation or
//! their absolute expiry, a bounded batch per change.
//!
//! `forge.web_sessions` is a state table: a DELETE commits only with a
//! succeeded audit row for the owner (`audited_change_present`), so the
//! change writes one `session.purged` row per user it touches (the first
//! as the change's own row, the others as [`ChangeRecord::affected`]),
//! with the count only. No domain event (ADR-0021 §5). The migration
//! `20261104100000` grants `forge_app` the DELETE and narrows it with a
//! guard: only a row that is old enough by the same rule (XF001 →
//! `FG-DB-0009` otherwise), so no other writer can delete a live session.

use std::collections::BTreeMap;

use forge_core::ids::UserId;
use forge_core::AuditDetails;
use forge_errors::{ErrorCode, ForgeError};

use super::RESOURCE;
use crate::change::{ChangeContext, ChangeRecord, ChangeTx, Settle, MAX_AFFECTED};
use crate::{query_error, Db};

/// Days after its revocation or absolute expiry a session row is kept
/// (the guard of migration `20261104100000` states the same 30 days).
pub const PURGE_AFTER_DAYS: i32 = 30;

/// The most rows one change deletes (one audit row per owner, so at most
/// this many plus one — within [`MAX_AFFECTED`]).
pub const MAX_BATCH: u32 = 500;

/// What one [`purge_expired`] change deleted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Purged {
    /// Session rows deleted.
    pub sessions: i64,
    /// Users they belonged to (= `session.purged` audit rows written).
    pub users: i64,
}

/// Delete at most `batch` (1 ..= [`MAX_BATCH`]) sessions that were revoked,
/// or whose absolute lifetime ended, more than [`PURGE_AFTER_DAYS`] days
/// ago, oldest id first; rows another transaction holds are skipped (`SKIP
/// LOCKED`: a concurrent revoke never waits for the purge). Nothing to
/// delete writes nothing (`Purged::default()`). A batch out of range is
/// `FG-SYS-0001 reason=purge_batch_out_of_range`.
pub async fn purge_expired(db: &Db, ctx: &ChangeContext, batch: u32) -> Result<Purged, ForgeError> {
    if batch == 0 || batch > MAX_BATCH {
        return Err(ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "purge_batch_out_of_range")
            .with_context("batch", batch));
    }
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let owners: Vec<i64> = tx
            .step(|c| async move {
                sqlx::query_scalar(
                    "DELETE FROM forge.web_sessions WHERE id IN ( \
                       SELECT id FROM forge.web_sessions \
                       WHERE revoked_at < now() - make_interval(days => $2) \
                          OR absolute_expires_at < now() - make_interval(days => $2) \
                       ORDER BY id LIMIT $1 FOR UPDATE SKIP LOCKED) \
                     RETURNING user_id",
                )
                .bind(i64::from(batch))
                .bind(PURGE_AFTER_DAYS)
                .fetch_all(c)
                .await
                .map_err(|e| query_error("sessions.purge.delete", e))
            })
            .await?;
        let mut per_user: BTreeMap<i64, i64> = BTreeMap::new();
        for owner in &owners {
            *per_user.entry(*owner).or_insert(0) += 1;
        }
        let mut rows = per_user.iter().map(|(user, count)| {
            let details = AuditDetails::SessionsPurged {
                user: UserId::new(*user),
                purged_sessions: *count,
            };
            ChangeRecord::audit_only(&details, RESOURCE, *user)
        });
        let Some(mut record) = rows.next() else {
            return Ok(Settle::Discard(Purged::default()));
        };
        record.affected = rows.map(|r| r.audit).collect();
        if record.affected.len() > MAX_AFFECTED {
            // MAX_BATCH keeps this unreachable; never commit past the cap.
            return Err(ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "purge_too_many_owners")
                .with_context("users", per_user.len()));
        }
        let purged = Purged {
            sessions: i64::try_from(owners.len()).unwrap_or(i64::MAX),
            users: i64::try_from(per_user.len()).unwrap_or(i64::MAX),
        };
        Ok(Settle::Commit(record, purged))
    }
    .await;
    tx.settle("session.purged", outcome).await
}

#[cfg(test)]
#[path = "purge_tests.rs"]
mod tests;
