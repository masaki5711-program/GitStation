//! `forge admin events new-epoch` (M3 I1c, ADR-0016 §7, ADR-0018 §5): give
//! the event feed a new epoch after the database was restored from a backup
//! or rebuilt in the same cluster. Every integration's cursor then answers
//! 410 `epoch_mismatch` and the consumers resync (docs/api/integration-feed.md),
//! instead of continuing from positions that may now name other events.
//!
//! A mutating operator command and, in production, an approved runbook step
//! (ops/runbooks/integration-feed-lag.md): it connects as `forge_maint`
//! (`database.maint_url` / `database.maint_password_file`, ADR-0018 §1) and
//! calls `forge.feed_new_epoch('manual', <request id>)` (owned by
//! forge_seq; forge_app cannot), which writes the `feed.epoch_rotated`
//! audit row. The command logs `FG-DB-0014` FeedEpochChanged (warn) with
//! both epochs and prints them.

use forge_core::config::DatabaseConfig;
use forge_core::{telemetry, RequestId};
use forge_db::feed;
use forge_db::Db;
use forge_errors::ForgeError;

use crate::runtime::block_on;

pub fn new_epoch(cfg: &DatabaseConfig, request_id: &RequestId) -> Result<String, ForgeError> {
    block_on(async {
        let mut maint = Db::connect_maint(cfg).await?;
        let rotated = feed::new_epoch(&mut maint, request_id).await;
        maint.close().await;
        let rotated = rotated?;
        telemetry::log_failure(&feed::epoch_changed(&rotated), request_id);
        let show = |b: &[u8; 16]| {
            b.iter().fold(String::with_capacity(32), |mut s, x| {
                s.push_str(&format!("{x:02x}"));
                s
            })
        };
        Ok(format!(
            "event feed epoch rotated: {} -> {} (every integration resyncs on its next read)\n",
            show(&rotated.old),
            show(&rotated.new)
        ))
    })
}
