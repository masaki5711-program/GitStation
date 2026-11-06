//! Binding a project to an integration, changing a binding's settings and
//! unbinding it, decided inside the transaction (module docs of
//! `integrations`).
//!
//! The rules of M3 X3 (plan 03 §7.1, plan 02 §2, ADR-0016 Amendment X3),
//! checked under the project row lock (every bind, change and unbind of a
//! project locks it first, so they run one at a time per project):
//!
//! - a binding that **resolves** work items (`PBI-123` in the project's
//!   branches and merge requests) is its project's only binding: a second
//!   kanflow project for a repository with code is `FG-INT-0016`;
//! - a binding that does **not** resolve (a shared spec repository) may be
//!   one of several of its project, to several kanflow projects, never next
//!   to a resolving one;
//! - one external project once per (integration, project).
//!
//! The partial unique index `integration_bindings_one_resolving_per_project`
//! is the backstop of the first rule.

use forge_core::ids::{IntegrationId, ProjectId, UserId};
use forge_core::integration::external_project_id_ok;
use forge_core::{AuditDetails, IntegrationChange, IntegrationKind, Role};
use forge_errors::{ErrorCode, ForgeError};

use super::read::kind_of;
use super::{
    field_invalid, not_found, BindOutcome, Binding, BindingFacts, BindingUpdate, NewBinding,
    UnbindOutcome, MAX_BINDINGS,
};
use crate::act_as::{consume_on, ActAsUse};
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

/// An enabled integration with an active service account, or
/// `FG-INT-0017` (`integration_disabled`, `service_account_disabled`).
fn available(
    locked: &LockedIntegration,
    integration: IntegrationId,
    project: ProjectId,
) -> Result<(), ForgeError> {
    let unavailable = |reason: &'static str| {
        ForgeError::new(ErrorCode::IntegrationDisabled)
            .with_context("reason", reason)
            .with_context("integration_id", integration.get())
            .with_context("service_account_id", locked.account.get())
            .with_context("project_id", project.get())
    };
    if !locked.enabled {
        return Err(unavailable("integration_disabled"));
    }
    if !locked.account_active {
        return Err(unavailable("service_account_disabled"));
    }
    Ok(())
}

/// An act-as change's assertion belongs to this integration's service
/// account (the backstop under the lock of `authorize()`'s "the
/// assertion's integration is the request's"): `FG-AUTH-0003
/// reason=act_as_other_integration` otherwise.
fn act_as_matches(
    act_as: Option<&ActAsUse>,
    integration: IntegrationId,
    account: UserId,
) -> Result<(), ForgeError> {
    match act_as {
        Some(using) if using.integration != integration || using.via != account => {
            Err(ForgeError::new(ErrorCode::Forbidden)
                .with_context("reason", "act_as_other_integration")
                .with_context("integration_id", integration.get()))
        }
        Some(_) | None => Ok(()),
    }
}

/// One stored binding of the locked project.
#[derive(Debug, Clone)]
struct Row {
    id: i64,
    integration: IntegrationId,
    kind: IntegrationKind,
    external: String,
    granted: bool,
    resolves: bool,
    created_by: Option<i64>,
    /// RFC 3339, UTC.
    created_at: String,
}

impl Row {
    fn binding(&self, project: ProjectId) -> Binding {
        Binding {
            id: self.id,
            integration: self.integration,
            kind: self.kind,
            project,
            external_project_id: self.external.clone(), // clone: the answer owns it
            granted_membership: self.granted,
            resolves_work_items: self.resolves,
            created_by: self.created_by.map(UserId::new),
            created_at: self.created_at.clone(),
        }
    }
}

type RawRow = (i64, i64, String, String, bool, bool, Option<i64>, String);

/// Every binding of `project`, by id (the project row is locked, so no
/// other bind, change or unbind of it runs meanwhile).
async fn bindings_of(tx: &mut ChangeTx, project: ProjectId) -> Result<Vec<Row>, ForgeError> {
    let rows: Vec<RawRow> = tx
        .step(|c| async move {
            sqlx::query_as(
                "SELECT b.generation, b.integration_id, i.kind, b.external_project_id, \
                 b.granted_membership, b.resolves_work_items, b.created_by, \
                 to_char(b.created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') \
                 FROM forge.integration_bindings b \
                 JOIN forge.integrations i ON i.id = b.integration_id \
                 WHERE b.project_id = $1 ORDER BY b.generation",
            )
            .bind(project.get())
            .fetch_all(c)
            .await
            .map_err(|e| query_error("integrations.bindings_of", e))
        })
        .await?;
    rows.into_iter()
        .map(
            |(id, integration, kind, external, granted, resolves, created_by, created_at)| {
                Ok(Row {
                    id,
                    integration: IntegrationId::new(integration),
                    kind: kind_of(integration, &kind)?,
                    external,
                    granted,
                    resolves,
                    created_by,
                    created_at,
                })
            },
        )
        .collect()
}

/// Clear `granted_membership` of `project`'s bindings whose integration's
/// service account is `user` (I1a review): a person removed the membership
/// a binding granted, or changed its role (re-review), so the membership
/// as it stands (or as given again later) is the person's and an unbind
/// must keep it. Called by the project member removals and role changes
/// inside their change (their `member.removed` / `member.role_changed` row
/// on the project covers the binding's audit trigger). No binding, another
/// user or an already cleared grant: nothing written. forge_app may update
/// granted_membership and resolves_work_items only (20261114100200,
/// 20261129100000).
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

fn project_audit(details: &AuditDetails, project: ProjectId) -> AuditEntry {
    AuditEntry {
        action: details.action(),
        outcome: Outcome::Succeeded,
        resource_type: "project",
        resource_id: Some(project.get()),
        project_id: Some(project),
        error_code: None,
        details: details.to_value(),
    }
}

/// The record of a binding change: its rows on the project (the first is
/// the change's own), the membership's row and event when the membership
/// changed, and the consumed assertion's row of an act-as change.
fn binding_record(
    details: &[AuditDetails],
    project: ProjectId,
    membership: Option<&AuditDetails>,
    consumed: Option<AuditEntry>,
) -> Result<ChangeRecord, ForgeError> {
    let mut rows = details.iter().map(|d| project_audit(d, project));
    let audit = rows.next().ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "binding_change_without_row")
    })?;
    let mut record = ChangeRecord {
        audit,
        affected: rows.collect(),
        events: Vec::new(),
    };
    if let Some(member) = membership {
        let member = evented_record(member, project)?;
        record.affected.push(member.audit);
        record.events.extend(member.events);
    }
    record.affected.extend(consumed);
    Ok(record)
}

/// What an act-as change that wrote nothing still commits: its consumed
/// assertion (ADR-0025 §5.3), or nothing at all.
async fn nothing_written<T>(
    tx: &mut ChangeTx,
    act_as: Option<&ActAsUse>,
    request: &str,
    answer: T,
) -> Result<Settle<T>, ForgeError> {
    match act_as {
        None => Ok(Settle::Discard(answer)),
        Some(using) => {
            let entry = consume_on(tx, using, request).await?;
            Ok(Settle::Commit(
                ChangeRecord {
                    audit: entry,
                    affected: Vec::new(),
                    events: Vec::new(),
                },
                answer,
            ))
        }
    }
}

async fn consumed(
    tx: &mut ChangeTx,
    act_as: Option<&ActAsUse>,
    request: &str,
) -> Result<Option<AuditEntry>, ForgeError> {
    match act_as {
        None => Ok(None),
        Some(using) => consume_on(tx, using, request).await.map(Some),
    }
}

fn conflict(
    reason: &'static str,
    project: ProjectId,
    integration: IntegrationId,
    bound: IntegrationId,
) -> ForgeError {
    ForgeError::new(ErrorCode::BindingConflict)
        .with_context("reason", reason)
        .with_context("project_id", project.get())
        .with_context("integration_id", integration.get())
        .with_context("bound_integration_id", bound.get())
}

/// Bind `project` to `external_project_id` of `integration` as a binding
/// that resolves work items (the project route's bind, M3 I1a): [`bind_as_decided`]
/// without an assertion.
pub async fn bind_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    project: ProjectId,
    viewer: UserId,
    integration: IntegrationId,
    external_project_id: &str,
    decide: impl FnOnce(&BindingFacts) -> Result<P, ForgeError>,
) -> Result<(BindOutcome, Binding, P), ForgeError> {
    let new = NewBinding {
        integration,
        external_project_id,
        resolves_work_items: true,
    };
    bind_as_decided(db, ctx, project, viewer, &new, None, decide).await
}

/// Bind `project` as `new` says, after `decide` on the locked project
/// facts; `act_as` is the assertion an act-as request consumes in this
/// transaction (also when nothing else is written). A project pending
/// deletion is refused here too (`FG-REPO-0006`), whatever `decide`
/// returned.
///
/// Errors (nothing written): `FG-WEB-0006 field=external_project_id`;
/// `FG-REPO-0005` `resource=project` / `resource=integration`;
/// `FG-REPO-0006`; `FG-INT-0016` (`bound_to_other_integration`,
/// `bound_to_other_external_project`: the project has a resolving binding;
/// `shared_bindings_present`: a resolving bind next to shared ones;
/// `bound_with_other_setting`: this binding exists with the other
/// setting); `FG-INT-0012 reason=binding_limit`; `FG-INT-0017`
/// (`integration_disabled`, `service_account_disabled`); `FG-AUTH-0003
/// reason=act_as_other_integration`; `FG-AUTH-0020 reason=consumed`;
/// whatever `decide` returns. The answer carries the binding as written
/// (or as found, for `Unchanged`), so no read after the commit is needed.
pub async fn bind_as_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    project: ProjectId,
    viewer: UserId,
    new: &NewBinding<'_>,
    act_as: Option<&ActAsUse>,
    decide: impl FnOnce(&BindingFacts) -> Result<P, ForgeError>,
) -> Result<(BindOutcome, Binding, P), ForgeError> {
    let (integration, external_project_id, resolves) = (
        new.integration,
        new.external_project_id,
        new.resolves_work_items,
    );
    if !external_project_id_ok(external_project_id) {
        return Err(field_invalid("external_project_id", "format"));
    }
    let request = ctx.request_id.as_str().to_owned();
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
        available(&locked, integration, project)?;
        act_as_matches(act_as, integration, account)?;
        let existing = bindings_of(&mut tx, project).await?;
        if let Some(same) = existing
            .iter()
            .find(|b| b.integration == integration && b.external == external_project_id)
        {
            if same.resolves != resolves {
                return Err(conflict(
                    "bound_with_other_setting",
                    project,
                    integration,
                    same.integration,
                ));
            }
            let found = same.binding(project);
            return nothing_written(
                &mut tx,
                act_as,
                &request,
                (BindOutcome::Unchanged, found, decided),
            )
            .await;
        }
        if let Some(resolving) = existing.iter().find(|b| b.resolves) {
            let reason = if resolving.integration == integration {
                "bound_to_other_external_project"
            } else {
                "bound_to_other_integration"
            };
            return Err(conflict(
                reason,
                project,
                integration,
                resolving.integration,
            ));
        }
        if let (true, Some(shared)) = (resolves, existing.first()) {
            return Err(conflict(
                "shared_bindings_present",
                project,
                integration,
                shared.integration,
            ));
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
        let (id, created_by, created_at): (i64, Option<i64>, String) = tx
            .step(|c| async move {
                sqlx::query_as(
                    "INSERT INTO forge.integration_bindings (integration_id, project_id, \
                     external_project_id, granted_membership, resolves_work_items, created_by) \
                     VALUES ($1, $2, $3, $4, $5, $6) RETURNING generation, created_by, \
                     to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"')",
                )
                .bind(integration.get())
                .bind(project.get())
                .bind(external_project_id)
                .bind(granted)
                .bind(resolves)
                .bind(viewer.get())
                .fetch_one(c)
                .await
                .map_err(|e| query_error("integrations.bind", e))
            })
            .await?;
        let written = Binding {
            id,
            integration,
            kind: locked.kind,
            project,
            external_project_id: external_project_id.to_owned(),
            granted_membership: granted,
            resolves_work_items: resolves,
            created_by: created_by.map(UserId::new),
            created_at,
        };
        let details = AuditDetails::Integration(IntegrationChange::Bound {
            integration,
            project,
            external_project_id: external_project_id.to_owned(),
            service_account: account,
            granted_membership: granted,
            binding: id,
            resolves_work_items: resolves,
        });
        let added = AuditDetails::ProjectMemberAdded {
            user: account,
            role: Role::Reporter,
        };
        let used = consumed(&mut tx, act_as, &request).await?;
        let record = binding_record(&[details], project, granted.then_some(&added), used)?;
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

/// Change binding `binding` of `integration` on `project` (M3 X3, `PATCH`):
/// today its one setting, `resolves_work_items`, and only from `false` to
/// `true` (a shared spec project becomes the project's resolving binding
/// when it is the project's only binding). `true` to `false` is refused
/// (`FG-INT-0016 reason=resolution_fixed`): the links I1b made would stay;
/// unbinding ends them, then a shared bind. The same value: `Unchanged`.
///
/// Errors (nothing written): `FG-REPO-0005` `resource=integration_binding
/// reason=absent` (no such binding of this integration on this project);
/// `FG-INT-0016` (`resolution_fixed`, `shared_bindings_present`);
/// `FG-INT-0017`; the act-as and `decide` errors of [`bind_as_decided`].
#[allow(clippy::too_many_arguments)] // the binding's ids, the setting, the assertion
pub async fn update_binding_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    project: ProjectId,
    viewer: UserId,
    integration: IntegrationId,
    binding: i64,
    resolves_work_items: bool,
    act_as: Option<&ActAsUse>,
    decide: impl FnOnce(&BindingFacts) -> Result<P, ForgeError>,
) -> Result<(BindingUpdate, Binding, P), ForgeError> {
    let request = ctx.request_id.as_str().to_owned();
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let facts = binding_facts(&mut tx, project, viewer).await?;
        let decided = decide(&facts)?;
        if facts.project.access.pending_deletion {
            return Err(ForgeError::new(ErrorCode::PendingDeletion)
                .with_context("action", "IntegrationBindingManage")
                .with_context("project_id", project.get()));
        }
        let existing = bindings_of(&mut tx, project).await?;
        let Some(row) = existing
            .iter()
            .find(|b| b.id == binding && b.integration == integration)
            .cloned()
        else {
            return Err(not_found("integration_binding", "absent"));
        };
        let locked = lock_integration(&mut tx, integration).await?;
        available(&locked, integration, project)?;
        act_as_matches(act_as, integration, locked.account)?;
        if row.resolves == resolves_work_items {
            let found = row.binding(project);
            return nothing_written(
                &mut tx,
                act_as,
                &request,
                (BindingUpdate::Unchanged, found, decided),
            )
            .await;
        }
        if row.resolves {
            return Err(conflict(
                "resolution_fixed",
                project,
                integration,
                integration,
            ));
        }
        if let Some(other) = existing.iter().find(|b| b.id != binding) {
            return Err(conflict(
                "shared_bindings_present",
                project,
                integration,
                other.integration,
            ));
        }
        tx.step(|c| async move {
            sqlx::query(
                "UPDATE forge.integration_bindings SET resolves_work_items = true \
                 WHERE generation = $1",
            )
            .bind(binding)
            .execute(c)
            .await
            .map_err(|e| query_error("integrations.update_binding", e))
        })
        .await?;
        let mut changed = row.binding(project);
        changed.resolves_work_items = true;
        let details = AuditDetails::Integration(IntegrationChange::BindingChanged {
            integration,
            project,
            binding,
            external_project_id: row.external.clone(), // clone: the answer keeps it too
            resolves_work_items: true,
        });
        let used = consumed(&mut tx, act_as, &request).await?;
        let record = binding_record(&[details], project, None, used)?;
        Ok(Settle::Commit(
            record,
            (BindingUpdate::Updated, changed, decided),
        ))
    }
    .await;
    tx.settle("integration.binding_changed", outcome).await
}

/// Unbind `project` from `integration`: every binding of the integration on
/// the project (the project route's unbind, M3 I1a), after `decide` on the
/// locked project facts. See [`unbind_binding_decided`] for what an unbind
/// does.
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
    unbind_some(db, ctx, project, viewer, integration, None, None, decide).await
}

/// Unbind binding `binding` of `integration` on `project` (M3 X3, `DELETE
/// /api/v1/integrations/{id}/bindings/{binding_id}`), after `decide` on the
/// locked project facts; `act_as` is consumed in this transaction. A
/// project pending deletion is refused by `decide` (`authorize()`:
/// `FG-REPO-0006`); its bindings go with the purge. A disabled integration
/// or service account may be unbound (it only takes access away).
///
/// The service account's membership the binding granted moves to the
/// integration's next binding on the project when one remains (a shared
/// spec project still bound to another kanflow project keeps its
/// Reporter); otherwise it is removed when the grant still stands and it is
/// still a direct Reporter. The project's open links to the binding's work
/// items end with it (`unbound`).
///
/// Errors (nothing written): `FG-REPO-0005` `resource=project`, or
/// `resource=integration_binding reason=not_bound`; `FG-AUTH-0003
/// reason=act_as_other_integration`; `FG-AUTH-0020 reason=consumed`;
/// whatever `decide` returns.
#[allow(clippy::too_many_arguments)] // the binding's ids and the assertion
pub async fn unbind_binding_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    project: ProjectId,
    viewer: UserId,
    integration: IntegrationId,
    binding: i64,
    act_as: Option<&ActAsUse>,
    decide: impl FnOnce(&BindingFacts) -> Result<P, ForgeError>,
) -> Result<(UnbindOutcome, P), ForgeError> {
    unbind_some(
        db,
        ctx,
        project,
        viewer,
        integration,
        Some(binding),
        act_as,
        decide,
    )
    .await
}

/// One removed binding's audit facts.
struct Removed {
    row: Row,
    links_ended: i64,
}

#[allow(clippy::too_many_arguments)] // see the two callers
async fn unbind_some<P>(
    db: &Db,
    ctx: &ChangeContext,
    project: ProjectId,
    viewer: UserId,
    integration: IntegrationId,
    only: Option<i64>,
    act_as: Option<&ActAsUse>,
    decide: impl FnOnce(&BindingFacts) -> Result<P, ForgeError>,
) -> Result<(UnbindOutcome, P), ForgeError> {
    let request = ctx.request_id.as_str().to_owned();
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let facts = binding_facts(&mut tx, project, viewer).await?;
        let decided = decide(&facts)?;
        let of_integration: Vec<Row> = bindings_of(&mut tx, project)
            .await?
            .into_iter()
            .filter(|b| b.integration == integration)
            .collect();
        let (gone, remaining): (Vec<Row>, Vec<Row>) = of_integration
            .into_iter()
            .partition(|b| only.is_none_or(|id| b.id == id));
        if gone.is_empty() {
            return Err(not_found("integration_binding", "not_bound"));
        }
        let account = lock_integration(&mut tx, integration).await?.account;
        act_as_matches(act_as, integration, account)?;
        let granted = gone.iter().any(|b| b.granted);
        // The grant moves to the integration's next binding on the project
        // (M3 X3): the membership still serves that binding.
        let moved_to = match (granted, remaining.first()) {
            (true, Some(next)) => {
                let next_id = next.id;
                tx.step(|c| async move {
                    sqlx::query(
                        "UPDATE forge.integration_bindings SET granted_membership = true \
                         WHERE generation = $1",
                    )
                    .bind(next_id)
                    .execute(c)
                    .await
                    .map_err(|e| query_error("integrations.unbind.move_grant", e))
                })
                .await?;
                Some(next_id)
            }
            (true, None) | (false, _) => None,
        };
        let removed = if granted && moved_to.is_none() {
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
        let mut ended = Vec::with_capacity(gone.len());
        for row in gone {
            // I1b review: the project's open links to this binding's work
            // items end with it (`unbound`, by the person; the trigger
            // merge_request_work_items_audited needs this change's
            // integration.unbound audit row). No work_item.unlinked events:
            // an unbound project's events reach no integration (the feed's
            // visible set), and a change carries at most 100 events.
            let ext = row.external.clone(); // clone: the audit keeps the row
            let links_ended = tx
                .step(|c| async move {
                    sqlx::query(
                        "UPDATE forge.merge_request_work_items SET unlinked_at = now(), \
                         unlink_source = 'unbound', unlinked_by = $4, unlinked_by_kind = 'human' \
                         WHERE project_id = $1 AND unlinked_at IS NULL AND work_item_id IN \
                         (SELECT id FROM forge.work_items WHERE integration_id = $2 \
                         AND external_project_id = $3)",
                    )
                    .bind(project.get())
                    .bind(integration.get())
                    .bind(&ext)
                    .bind(viewer.get())
                    .execute(c)
                    .await
                    .map_err(|e| query_error("integrations.unbind.work_item_links", e))
                })
                .await?
                .rows_affected();
            let id = row.id;
            tx.step(|c| async move {
                sqlx::query("DELETE FROM forge.integration_bindings WHERE generation = $1")
                    .bind(id)
                    .execute(c)
                    .await
                    .map_err(|e| query_error("integrations.unbind", e))
            })
            .await?;
            ended.push(Removed {
                row,
                links_ended: i64::try_from(links_ended).unwrap_or(i64::MAX),
            });
        }
        let details: Vec<AuditDetails> = ended
            .into_iter()
            .map(|r| {
                AuditDetails::Integration(IntegrationChange::Unbound {
                    integration,
                    project,
                    external_project_id: r.row.external,
                    service_account: account,
                    membership_removed: removed && r.row.granted,
                    work_item_links_ended: r.links_ended,
                    binding: r.row.id,
                    grant_moved_to: moved_to.filter(|_| r.row.granted),
                })
            })
            .collect();
        let gone_member = AuditDetails::ProjectMemberRemoved {
            user: account,
            role: Role::Reporter,
        };
        let used = consumed(&mut tx, act_as, &request).await?;
        let record = binding_record(&details, project, removed.then_some(&gone_member), used)?;
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
