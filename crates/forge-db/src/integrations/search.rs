//! The audit row of a repository search acting for a person (M3 X3
//! review, security MEDIUM): who searched (the person and the service
//! account, the change context's actor), for which integration, the text
//! and how many repositories were listed.

use forge_core::ids::IntegrationId;
use forge_errors::ForgeError;

use super::RESOURCE;
use crate::change::{AuditEntry, ChangeContext, ChangeRecord, ChangeTx, Outcome, Settle};
use crate::Db;

/// The audit action.
pub const SEARCHED: &str = "integration.repository_searched";

/// One `integration.repository_searched` audit row on (`integration`, id)
/// with `{"integration_id", "q", "results"}`, in its own transaction. The
/// caller validated `q` (`lists::project_search`: 1..=100 characters).
pub async fn record_repository_search(
    db: &Db,
    ctx: &ChangeContext,
    integration: IntegrationId,
    q: &str,
    results: usize,
) -> Result<(), ForgeError> {
    let tx = ChangeTx::begin(db, ctx).await?;
    let record = ChangeRecord {
        audit: AuditEntry {
            action: SEARCHED,
            outcome: Outcome::Succeeded,
            resource_type: RESOURCE,
            resource_id: Some(integration.get()),
            project_id: None,
            error_code: None,
            details: serde_json::json!({
                "integration_id": integration.get(),
                "q": q,
                "results": results,
            }),
        },
        affected: Vec::new(),
        events: Vec::new(),
    };
    let outcome: Result<Settle<()>, ForgeError> = Ok(Settle::Commit(record, ()));
    tx.settle(SEARCHED, outcome).await
}
