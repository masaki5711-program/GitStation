//! Binding a project to an integration and unbinding it, decided inside
//! the transaction (module docs of `integrations`).

use forge_core::ids::{IntegrationId, ProjectId, UserId};
use forge_core::integration::external_project_id_ok;
use forge_core::{AuditDetails, IntegrationChange, IntegrationKind, Role};
use forge_errors::{ErrorCode, ForgeError};

use super::read::kind_of;
use super::{
    field_invalid, not_found, BindOutcome, Binding, BindingFacts, UnbindOutcome, MAX_BINDINGS,
};
use crate::change::{AuditEntry, ChangeContext, ChangeRecord, ChangeTx, Outcome, Settle};
use crate::projects::{
    direct_role_for_update, evented_record, lock_access, locked_access_facts, member_candidate,
};
use crate::{query_error, Db};
use sqlx::postgres::PgConnection;

/// Lock and read what `decide` sees (`projects::update_decided`'s facts).
async fn binding_facts(
    tx: &mut ChangeTx,
    project: ProjectId,
    viewer: UserId,
) -> Result<BindingFacts, ForgeError> {
    let locked = lock_access(tx, project).await?;
    let facts = locked_access_facts(tx, project, viewer, &locked).await?;
    Ok(BindingFacts {
        account: facts.account,
        project: facts.project,
    })
}

/// The integration as locked: its row `FOR UPDATE`, its service account's
/// user row `FOR SHARE` (a concurrent disable of either waits for this
/// change or is seen by it).
struct LockedIntegration {
    account: UserId,
    kind: IntegrationKind,
    enabled: bool,
    account_active: bool,
}

/// Lock the integration row and its service account's row.
async fn lock_integration(
    tx: &mut ChangeTx,
    integration: IntegrationId,
) -> Result<LockedIntegration, ForgeError> {
    let row: Option<(i64, String, bool, bool)> = tx
        .step(|c| async move {
            sqlx::query_as(
                "SELECT i.service_account_id, i.kind, i.enabled, u.state = 'active' \
                 FROM forge.integrations i JOIN forge.users u ON u.id = i.service_account_id \
                 WHERE i.id = $1 FOR UPDATE OF i FOR SHARE OF u",
            )
            .bind(integration.get())
            .fetch_optional(c)
            .await
            .map_err(|e| query_error("integrations.lock", e))
        })
        .await?;
    let Some((account, kind, enabled, account_active)) = row else {
        return Err(not_found("integration", "absent"));
    };
    Ok(LockedIntegration {
        account: UserId::new(account),
        kind: kind_of(integration.get(), &kind)?,
        enabled,
        account_active,
    })
}

/// The project's binding as stored (the project row is locked, so no other
/// bind or unbind of it runs meanwhile): integration, external project id,
/// granted, creator, creation time (RFC 3339, UTC).
type BindingRow = (i64, String, bool, Option<i64>, String);

async fn binding_of(
    tx: &mut ChangeTx,
    project: ProjectId,
) -> Result<Option<BindingRow>, ForgeError> {
    tx.step(|c| async move {
        sqlx::query_as(
            "SELECT integration_id, external_project_id, granted_membership, created_by, \
             to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') \
             FROM forge.integration_bindings WHERE project_id = $1",
        )
        .bind(project.get())
        .fetch_optional(c)
        .await
        .map_err(|e| query_error("integrations.binding_of", e))
    })
    .await
}

/// Clear `granted_membership` of `project`'s binding when `user` is the
/// bound integration's service account (I1a review): the membership the
/// binding granted was removed, so a membership given again later is a
/// person's and an unbind must keep it. Called by the project member
/// removals inside their change (their `member.removed` row on the project
/// covers the binding's audit trigger). No binding, another user or an
/// already cleared grant: nothing written.
pub(crate) async fn clear_grant(
    conn: &mut PgConnection,
    project: ProjectId,
    user: UserId,
) -> Result<(), ForgeError> {
    sqlx::query(
        "UPDATE forge.integration_bindings b SET granted_membership = false \
         FROM forge.integrations i \
         WHERE b.project_id = $1 AND b.granted_membership \
         AND i.id = b.integration_id AND i.service_account_id = $2",
    )
    .bind(project.get())
    .bind(user.get())
    .execute(conn)
    .await
    .map_err(|e| query_error("integrations.clear_grant", e))?;
    Ok(())
}

/// The record of a bind or unbind: its own row on the project, plus the
/// membership's row and event when the membership changed.
fn binding_record(
    details: &AuditDetails,
    project: ProjectId,
    membership: Option<&AuditDetails>,
) -> Result<ChangeRecord, ForgeError> {
    let mut record = ChangeRecord {
        audit: AuditEntry {
            action: details.action(),
            outcome: Outcome::Succeeded,
            resource_type: "project",
            resource_id: Some(project.get()),
            project_id: Some(project),
            error_code: None,
            details: details.to_value(),
        },
        affected: Vec::new(),
        events: Vec::new(),
    };
    if let Some(member) = membership {
        let member = evented_record(member, project)?;
        record.affected.push(member.audit);
        record.events.extend(member.events);
    }
    Ok(record)
}

/// Bind `project` to `external_project_id` of `integration`, after
/// `decide` on the locked project facts. A project pending deletion is
/// refused here too (`FG-REPO-0006`), whatever `decide` returned.
///
/// Errors (nothing written): `FG-WEB-0006 field=external_project_id`;
/// `FG-REPO-0005` `resource=project` / `resource=integration`;
/// `FG-REPO-0006`; `FG-INT-0016` (`bound_to_other_integration`,
/// `bound_to_other_external_project`); `FG-INT-0012 reason=binding_limit`;
/// `FG-INT-0017` (`integration_disabled`, `service_account_disabled`);
/// whatever `decide` returns. The answer carries the binding as written
/// (or as found, for `Unchanged`), so no read after the commit is needed.
pub async fn bind_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    project: ProjectId,
    viewer: UserId,
    integration: IntegrationId,
    external_project_id: &str,
    decide: impl FnOnce(&BindingFacts) -> Result<P, ForgeError>,
) -> Result<(BindOutcome, Binding, P), ForgeError> {
    if !external_project_id_ok(external_project_id) {
        return Err(field_invalid("external_project_id", "format"));
    }
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let facts = binding_facts(&mut tx, project, viewer).await?;
        let decided = decide(&facts)?;
        if facts.project.access.pending_deletion {
            return Err(ForgeError::new(ErrorCode::PendingDeletion)
                .with_context("action", "IntegrationBindingManage")
                .with_context("project_id", project.get()));
        }
        let locked = lock_integration(&mut tx, integration).await?;
        let account = locked.account;
        let unavailable = |reason: &'static str| {
            ForgeError::new(ErrorCode::IntegrationDisabled)
                .with_context("reason", reason)
                .with_context("integration_id", integration.get())
                .with_context("service_account_id", account.get())
                .with_context("project_id", project.get())
        };
        if !locked.enabled {
            return Err(unavailable("integration_disabled"));
        }
        if !locked.account_active {
            return Err(unavailable("service_account_disabled"));
        }
        let binding =
            |external: String, granted: bool, created_by: Option<i64>, created_at| Binding {
                integration,
                kind: locked.kind,
                project,
                external_project_id: external,
                granted_membership: granted,
                created_by: created_by.map(UserId::new),
                created_at,
            };
        let conflict = |reason: &'static str, bound: IntegrationId| {
            ForgeError::new(ErrorCode::BindingConflict)
                .with_context("reason", reason)
                .with_context("project_id", project.get())
                .with_context("integration_id", integration.get())
                .with_context("bound_integration_id", bound.get())
        };
        match binding_of(&mut tx, project).await? {
            Some((bound, ..)) if bound != integration.get() => {
                return Err(conflict(
                    "bound_to_other_integration",
                    IntegrationId::new(bound),
                ));
            }
            Some((bound, external, ..)) if external != external_project_id => {
                return Err(conflict(
                    "bound_to_other_external_project",
                    IntegrationId::new(bound),
                ));
            }
            Some((_, external, granted, created_by, created_at)) => {
                let found = binding(external, granted, created_by, created_at);
                return Ok(Settle::Discard((BindOutcome::Unchanged, found, decided)));
            }
            None => {}
        }
        let bound: i64 = tx
            .step(|c| async move {
                sqlx::query_scalar(
                    "SELECT count(*) FROM forge.integration_bindings WHERE integration_id = $1",
                )
                .bind(integration.get())
                .fetch_one(c)
                .await
                .map_err(|e| query_error("integrations.bind.count", e))
            })
            .await?;
        if bound >= MAX_BINDINGS {
            return Err(ForgeError::new(ErrorCode::IntegrationLimitReached)
                .with_context("reason", "binding_limit")
                .with_context("limit", MAX_BINDINGS)
                .with_context("integration_id", integration.get())
                .with_context("project_id", project.get()));
        }
        let member = tx
            .step(|c| direct_role_for_update(c, project, account))
            .await?;
        let granted = member.is_none();
        if granted {
            tx.step(|c| member_candidate(c, account)).await?;
            tx.step(|c| async move {
                sqlx::query(
                    "INSERT INTO forge.project_members (project_id, user_id, role) \
                     VALUES ($1, $2, $3)",
                )
                .bind(project.get())
                .bind(account.get())
                .bind(Role::Reporter.as_str())
                .execute(c)
                .await
                .map_err(|e| query_error("integrations.bind.member", e))
            })
            .await?;
        }
        let (created_by, created_at): (Option<i64>, String) = tx
            .step(|c| async move {
                sqlx::query_as(
                    "INSERT INTO forge.integration_bindings (integration_id, project_id, \
                     external_project_id, granted_membership, created_by) \
                     VALUES ($1, $2, $3, $4, $5) RETURNING created_by, \
                     to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"')",
                )
                .bind(integration.get())
                .bind(project.get())
                .bind(external_project_id)
                .bind(granted)
                .bind(viewer.get())
                .fetch_one(c)
                .await
                .map_err(|e| query_error("integrations.bind", e))
            })
            .await?;
        let written = binding(
            external_project_id.to_owned(),
            granted,
            created_by,
            created_at,
        );
        let details = AuditDetails::Integration(IntegrationChange::Bound {
            integration,
            project,
            external_project_id: external_project_id.to_owned(),
            service_account: account,
            granted_membership: granted,
        });
        let added = AuditDetails::ProjectMemberAdded {
            user: account,
            role: Role::Reporter,
        };
        let record = binding_record(&details, project, granted.then_some(&added))?;
        Ok(Settle::Commit(
            record,
            (
                BindOutcome::Bound {
                    granted_membership: granted,
                },
                written,
                decided,
            ),
        ))
    }
    .await;
    tx.settle("integration.bound", outcome).await
}

/// Unbind `project` from `integration`, after `decide` on the locked
/// project facts. A project pending deletion is refused by `decide`
/// (`authorize()`: `FG-REPO-0006`); its binding goes with the purge. A
/// disabled integration or service account may be unbound (it only takes
/// access away). The service account's membership is removed when the
/// binding granted it (and no removal cleared the grant since) and it is
/// still a direct Reporter.
///
/// Errors (nothing written): `FG-REPO-0005` `resource=project`, or
/// `resource=integration_binding reason=not_bound`; whatever `decide`
/// returns.
pub async fn unbind_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    project: ProjectId,
    viewer: UserId,
    integration: IntegrationId,
    decide: impl FnOnce(&BindingFacts) -> Result<P, ForgeError>,
) -> Result<(UnbindOutcome, P), ForgeError> {
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let facts = binding_facts(&mut tx, project, viewer).await?;
        let decided = decide(&facts)?;
        let (external, granted) = match binding_of(&mut tx, project).await? {
            Some((bound, external, granted, ..)) if bound == integration.get() => {
                (external, granted)
            }
            Some(_) | None => return Err(not_found("integration_binding", "not_bound")),
        };
        let account = lock_integration(&mut tx, integration).await?.account;
        let removed = if granted {
            let role = tx
                .step(|c| direct_role_for_update(c, project, account))
                .await?;
            if role == Some(Role::Reporter) {
                tx.step(|c| async move {
                    sqlx::query(
                        "DELETE FROM forge.project_members WHERE project_id = $1 AND user_id = $2",
                    )
                    .bind(project.get())
                    .bind(account.get())
                    .execute(c)
                    .await
                    .map_err(|e| query_error("integrations.unbind.member", e))
                })
                .await?;
                true
            } else {
                false
            }
        } else {
            false
        };
        tx.step(|c| async move {
            sqlx::query(
                "DELETE FROM forge.integration_bindings \
                 WHERE integration_id = $1 AND project_id = $2",
            )
            .bind(integration.get())
            .bind(project.get())
            .execute(c)
            .await
            .map_err(|e| query_error("integrations.unbind", e))
        })
        .await?;
        let details = AuditDetails::Integration(IntegrationChange::Unbound {
            integration,
            project,
            external_project_id: external,
            service_account: account,
            membership_removed: removed,
        });
        let gone = AuditDetails::ProjectMemberRemoved {
            user: account,
            role: Role::Reporter,
        };
        let record = binding_record(&details, project, removed.then_some(&gone))?;
        Ok(Settle::Commit(
            record,
            (
                UnbindOutcome {
                    membership_removed: removed,
                },
                decided,
            ),
        ))
    }
    .await;
    tx.settle("integration.unbound", outcome).await
}
