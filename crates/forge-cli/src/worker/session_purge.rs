//! The `session_purge` task of `forge worker` (M2 MR6, the MR3 handoff's
//! deferred cleanup): every `web.session_purge_interval_secs` it deletes web
//! sessions 30 days past their revocation or absolute expiry (forge-db
//! `sessions::purge_expired`: one change per batch of at most
//! [`forge_db::sessions::PURGE_MAX_BATCH`] rows, one `session.purged` audit
//! row per owner, actor `system`). A full batch runs again at once, so a
//! backlog drains without waiting a period per batch.
//!
//! A failed run (the database: `FG-DB-0001` / `FG-DB-0002`, a refused
//! delete `FG-DB-0009`) is returned and logged once per failure streak by
//! the supervisor with `error_code` and `task=session_purge`, then retried
//! after the backoff; nothing is half done (one transaction per batch).

use forge_core::RequestId;
use forge_db::change::{Actor, ChangeContext};
use forge_db::sessions::{purge_expired, PURGE_MAX_BATCH};
use forge_db::Db;
use forge_errors::ForgeError;

use super::supervise::Next;

/// The task's name in the supervisor's logs.
pub(crate) const NAME: &str = "session_purge";
/// The audit label of the worker's changes.
const ACTOR_LABEL: &str = "forge-worker";

/// One run: one batch; a full one asks to run again at once.
pub(crate) async fn run_session_purge(db: &Db) -> Result<Next, ForgeError> {
    let request_id = RequestId::generate();
    let mut ctx = ChangeContext::new(Actor::System, request_id.clone()); // clone: logged below
    ctx.actor_label = Some(ACTOR_LABEL.to_owned());
    let purged = purge_expired(db, &ctx, PURGE_MAX_BATCH).await?;
    if purged.sessions > 0 {
        tracing::info!(
            target: "forge_cli",
            task = NAME,
            request_id = %request_id,
            sessions = purged.sessions,
            users = purged.users,
            "forge.worker.sessions_purged"
        );
    }
    Ok(if purged.sessions >= i64::from(PURGE_MAX_BATCH) {
        Next::Immediately
    } else {
        Next::AfterPeriod
    })
}

#[cfg(test)]
mod tests {
    use forge_db::test_support::FreshDb;

    use super::*;

    /// A run against a migrated database: old sessions go, live ones stay,
    /// and a partial batch waits for the period.
    #[tokio::test]
    async fn a_run_purges_old_sessions_and_waits_for_the_period() {
        let t = FreshDb::migrated().await;
        let user = forge_db::users::create(
            &t.db,
            &ChangeContext::new(Actor::System, RequestId::parse("req-user").expect("id")),
            &forge_db::users::NewUser::human("alice", "Alice", "alice@example.test"),
        )
        .await
        .expect("user")
        .user_id;
        let _live = forge_db::test_support::issue_session(&t.db, user).await;
        t.commit_audited(
            ("user", user.get()),
            &[format!(
                "INSERT INTO forge.web_sessions (public_id, secret_sha256, user_id, created_at, \
                 last_seen_at, idle_expires_at, absolute_expires_at) VALUES ('00000000000000f1', \
                 decode(repeat('cd', 32), 'hex'), {}, now() - interval '40 days', \
                 now() - interval '40 days', now() - interval '40 days' + interval '1 hour', \
                 now() - interval '40 days' + interval '2 hours')",
                user.get()
            )],
        )
        .await;
        let next = run_session_purge(&t.db).await.expect("run");
        assert!(matches!(next, Next::AfterPeriod));
        let left = t
            .json_rows("SELECT public_id FROM forge.web_sessions")
            .await;
        assert_eq!(left.len(), 1, "{left:?}");
        let rows = t
            .json_rows(
                "SELECT actor_kind, actor_label FROM forge.audit_log WHERE action = 'session.purged'",
            )
            .await;
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0]["actor_kind"], "system");
        t.drop_now().await;
    }
}
