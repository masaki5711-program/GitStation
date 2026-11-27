//! The AI first-pass review's check run (plan 19 L5-5): forge's own runs
//! are written through forge-checks only (`decided_callers`), so the
//! worker's AI review records its run here.

use forge_core::ObjectId;
use forge_core::ProjectId;
use forge_db::change::ChangeContext;
use forge_db::checks::{record_forge_check_run, NewCheckRun, Reported};
use forge_db::Db;
use forge_errors::ForgeError;

/// Record the AI review's run on `(project, head)` (no decision: forge's
/// worker). The caller bounds it with its own timeout.
pub async fn record_ai_review_run(
    db: &Db,
    ctx: &ChangeContext,
    target: (ProjectId, &ObjectId),
    run: &NewCheckRun,
) -> Result<Reported, ForgeError> {
    record_forge_check_run(db, ctx, target, run).await
}
