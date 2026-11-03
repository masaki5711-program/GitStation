//! Creating and changing integrations (the administration).

use forge_core::ids::{IntegrationId, UserId};
use forge_core::{AuditDetails, IntegrationChange, IntegrationField};
use forge_errors::ForgeError;

use super::{
    event_types, field_invalid, not_found, Changes, NewIntegration, UpdateOutcome, RESOURCE,
};
use crate::change::{ChangeContext, ChangeRecord, ChangeTx, Settle};
use crate::{query_error, Db};

/// Create an integration acting as `new.service_account`, with one
/// `integration.created` audit row; `created_by` is the administrator.
///
/// Errors (nothing written): `FG-WEB-0006 field=service_account_id`
/// `rule=not_service_account` (absent, a person or a bot),
/// `service_account_disabled` or `service_account_in_use` (it has an
/// integration already, also when a concurrent create won);
/// `field=event_types` (`count`, `unknown_event_type`).
pub async fn create(
    db: &Db,
    ctx: &ChangeContext,
    new: &NewIntegration,
    created_by: UserId,
) -> Result<IntegrationId, ForgeError> {
    let types = event_types(new.event_types.as_deref())?;
    let account = new.service_account;
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        // The account row FOR SHARE (a concurrent disable waits): its kind
        // never changes; a disabled account gets no integration (I1a
        // review: nothing waits to come alive when it is enabled again).
        let row: Option<(String, String)> = tx
            .step(|c| async move {
                sqlx::query_as("SELECT kind, state FROM forge.users WHERE id = $1 FOR SHARE")
                    .bind(account.get())
                    .fetch_optional(c)
                    .await
                    .map_err(|e| query_error("integrations.create.account", e))
            })
            .await?;
        match row {
            Some((kind, _)) if kind != "service_account" => {
                return Err(field_invalid("service_account_id", "not_service_account"));
            }
            None => return Err(field_invalid("service_account_id", "not_service_account")),
            Some((_, state)) if state != "active" => {
                return Err(field_invalid("service_account_id", "service_account_disabled"));
            }
            Some(_) => {}
        }
        let taken: bool = tx
            .step(|c| async move {
                sqlx::query_scalar(
                    "SELECT EXISTS (SELECT 1 FROM forge.integrations WHERE service_account_id = $1)",
                )
                .bind(account.get())
                .fetch_one(c)
                .await
                .map_err(|e| query_error("integrations.create.taken", e))
            })
            .await?;
        if taken {
            return Err(field_invalid("service_account_id", "service_account_in_use"));
        }
        let kind_name = new.kind.as_str();
        let stored = &types;
        let id: i64 = tx
            .step(|c| async move {
                sqlx::query_scalar(
                    "INSERT INTO forge.integrations (kind, service_account_id, event_types, \
                     created_by) VALUES ($1, $2, $3, $4) RETURNING id",
                )
                .bind(kind_name)
                .bind(account.get())
                .bind(stored)
                .bind(created_by.get())
                .fetch_one(c)
                .await
                .map_err(|e| {
                    // Two creates at once: the loser passed the check above
                    // before the winner committed and meets the index.
                    if is_unique_violation(&e, IN_USE_INDEX) {
                        field_invalid("service_account_id", "service_account_in_use")
                    } else {
                        query_error("integrations.create", e)
                    }
                })
            })
            .await?;
        let integration = IntegrationId::new(id);
        let details = AuditDetails::Integration(IntegrationChange::Created {
            integration,
            kind: new.kind,
            service_account: account,
            event_types: types.clone(), // clone: the audit row keeps them
        });
        let record = ChangeRecord::audit_only(&details, RESOURCE, id);
        Ok(Settle::Commit(record, integration))
    }
    .await;
    tx.settle("integration.created", outcome).await
}

/// The unique constraint of one integration per service account.
const IN_USE_INDEX: &str = "integrations_one_per_service_account";

/// Whether `e` is SQLSTATE 23505 on `constraint`.
fn is_unique_violation(e: &sqlx::Error, constraint: &str) -> bool {
    e.as_database_error()
        .is_some_and(|d| d.code().as_deref() == Some("23505") && d.constraint() == Some(constraint))
}

/// Enable or disable an integration, or replace its event type allowlist.
/// The row is locked `FOR UPDATE`; nothing is written when every given
/// value equals the stored one.
///
/// Errors (nothing written): `FG-REPO-0005 resource=integration`,
/// `FG-WEB-0006 field=event_types`, `field=enabled
/// rule=service_account_disabled` (enabling while its service account is
/// disabled).
pub async fn update(
    db: &Db,
    ctx: &ChangeContext,
    id: IntegrationId,
    changes: &Changes,
) -> Result<UpdateOutcome, ForgeError> {
    let new_types = changes
        .event_types
        .as_deref()
        .map(|t| event_types(Some(t)))
        .transpose()?;
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        // The integration row FOR UPDATE, then its service account's row
        // FOR SHARE (the order of a bind; a concurrent disable waits).
        let row: Option<(bool, Vec<String>, bool)> = tx
            .step(|c| async move {
                sqlx::query_as(
                    "SELECT i.enabled, i.event_types, u.state = 'active'                      FROM forge.integrations i JOIN forge.users u ON u.id = i.service_account_id                      WHERE i.id = $1 FOR UPDATE OF i FOR SHARE OF u",
                )
                .bind(id.get())
                .fetch_optional(c)
                .await
                .map_err(|e| query_error("integrations.update.lock", e))
            })
            .await?;
        let Some((enabled, stored_types, account_active)) = row else {
            return Err(not_found("integration", "absent"));
        };
        let mut stored_sorted = stored_types;
        stored_sorted.sort();
        stored_sorted.dedup();
        let mut fields = Vec::new();
        let to_enabled = changes.enabled.unwrap_or(enabled);
        if to_enabled != enabled {
            // Re-review: the rule of `create` — nothing is switched on for
            // a disabled service account (disabling stays possible).
            if to_enabled && !account_active {
                return Err(field_invalid("enabled", "service_account_disabled"));
            }
            fields.push(IntegrationField::Enabled);
        }
        let to_types = match new_types {
            Some(types) if types != stored_sorted => {
                fields.push(IntegrationField::EventTypes);
                types
            }
            Some(_) | None => stored_sorted,
        };
        if fields.is_empty() {
            return Ok(Settle::Discard(UpdateOutcome::Unchanged));
        }
        let stored = &to_types;
        tx.step(|c| async move {
            sqlx::query(
                "UPDATE forge.integrations SET enabled = $2, event_types = $3, \
                 updated_at = now() WHERE id = $1",
            )
            .bind(id.get())
            .bind(to_enabled)
            .bind(stored)
            .execute(c)
            .await
            .map_err(|e| query_error("integrations.update", e))
        })
        .await?;
        let details = AuditDetails::Integration(IntegrationChange::Updated {
            integration: id,
            fields: fields.clone(), // clone: the audit row and the answer both keep them
            enabled: to_enabled,
            event_types: to_types.clone(), // clone: as above
        });
        let record = ChangeRecord::audit_only(&details, RESOURCE, id.get());
        Ok(Settle::Commit(record, UpdateOutcome::Updated(fields)))
    }
    .await;
    tx.settle("integration.updated", outcome).await
}
