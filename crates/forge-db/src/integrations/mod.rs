//! Integrations and their project bindings (M3 I1a, plan 11 §5 I1, 02 §2,
//! ADR-0016 §1 / §7, ADR-0016 Amendment I1a).
//!
//! - [`create`] / [`update`]: an integration (`forge.integrations`) — its
//!   kind, the service account it acts as (fixed for its life), whether it
//!   is enabled and its event type allowlist; one `integration.created` /
//!   `integration.updated` audit row on (`integration`, id), no event.
//! - [`bind_decided`] / [`bind_as_decided`] / [`unbind_decided`] /
//!   [`unbind_binding_decided`] / [`update_binding_decided`]: bind a
//!   project to an external (kanflow) project of an integration, change a
//!   binding's setting, or unbind it, decided inside the transaction
//!   (forge-hosting: `Authorized::reconfirm`; an act-as request's assertion
//!   consumed in the same transaction, M3 X3). A project has one binding
//!   that resolves work items, or any number that do not (a shared spec
//!   repository, plan 03 §7.1; `bind.rs`). Binding makes
//!   the service account a Reporter of the project when it is no member
//!   yet (`granted_membership`); unbinding removes that membership only if
//!   the binding granted it and it is still Reporter (a membership someone
//!   gave or changed by hand is kept). One `integration.bound` /
//!   `integration.unbound` audit row on the project, plus the membership's
//!   own `member.added` / `member.removed` audit row and event.
//! - [`get`], [`list`], [`bindings`], [`project_bindings`], [`binding`],
//!   [`external_bindings`]: reads on the read pool.
//!
//! Lock order of a bind or unbind (that of `projects::member_writes`): the
//! project row `FOR UPDATE` (with its deletion), the caller's membership
//! rows and account row `FOR SHARE`, `decide`, then the integration row
//! `FOR UPDATE` (so the 200-binding count of [`MAX_BINDINGS`] holds
//! against concurrent binds of other projects), the project's binding, the
//! service account's membership row `FOR UPDATE` and its user row `FOR
//! SHARE`. An integration update locks only the integration row; neither
//! ever locks a project row after a user row.
//!
//! These commands do not decide who may call them beyond `decide`: the
//! CHECKs, the unique indexes, the guard `integrations_service_account_guard`
//! and the deferred audit triggers are the backstop.

use forge_core::ids::{IntegrationId, ProjectId, UserId};
use forge_core::integration::{default_event_types, event_type_entry_ok, MAX_EVENT_TYPE_ENTRIES};
use forge_core::{IntegrationField, IntegrationKind};
use forge_errors::{ErrorCode, ForgeError};

use crate::access::Account;
use crate::projects::ProjectAccessFacts;

mod admin;
mod bind;
mod read;
#[cfg(test)]
mod tests;

pub use admin::{create, update};
pub(crate) use bind::clear_grant;
pub use bind::{
    bind_as_decided, bind_decided, unbind_binding_decided, unbind_decided, update_binding_decided,
};
pub use read::{binding, bindings, external_bindings, get, list, project_bindings};

/// The most projects one integration may be bound to (ADR-0016 §1: the
/// feed decides every bound project on every read). `FG-INT-0012` beyond.
pub const MAX_BINDINGS: i64 = 200;
/// The most integrations [`list`] returns (it says when it cut).
pub const MAX_LISTED: i64 = 500;

/// The audit resource of an integration's own changes (tables.toml).
pub(super) const RESOURCE: &str = "integration";

/// The request to create an integration.
#[derive(Debug, Clone)]
pub struct NewIntegration {
    pub kind: IntegrationKind,
    /// The service account it acts as (one integration per account).
    pub service_account: UserId,
    /// The event type allowlist; `None`: every integration-class event
    /// (`["*"]`).
    pub event_types: Option<Vec<String>>,
}

/// The fields an update may change (`None`: unchanged).
#[derive(Debug, Clone, Default)]
pub struct Changes {
    pub enabled: Option<bool>,
    pub event_types: Option<Vec<String>>,
}

/// What [`update`] did.
#[must_use = "Unchanged wrote nothing; callers report it differently"]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateOutcome {
    /// One `integration.updated` row naming these fields.
    Updated(Vec<IntegrationField>),
    /// Every given value equals the stored one: nothing was written.
    Unchanged,
}

/// One integration as the administration sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Integration {
    pub id: IntegrationId,
    pub kind: IntegrationKind,
    pub service_account: UserId,
    pub service_account_username: String,
    pub enabled: bool,
    /// Sorted, without duplicates.
    pub event_types: Vec<String>,
    /// How many projects are bound to it.
    pub bindings: i64,
    pub created_by: Option<UserId>,
    /// RFC 3339, UTC.
    pub created_at: String,
    pub updated_at: String,
}

/// What [`list`] read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    pub items: Vec<Integration>,
    /// More than [`MAX_LISTED`] exist.
    pub truncated: bool,
}

/// One binding of a project to an external project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    /// The binding's id (`integration_bindings.generation`, M3 X3).
    pub id: i64,
    pub integration: IntegrationId,
    pub kind: IntegrationKind,
    pub project: ProjectId,
    pub external_project_id: String,
    /// The binding made the service account a Reporter of the project.
    pub granted_membership: bool,
    /// The binding resolves `PBI-123` in the project's branches and merge
    /// requests (M3 X3; `false`: a shared spec repository).
    pub resolves_work_items: bool,
    pub created_by: Option<UserId>,
    /// RFC 3339, UTC.
    pub created_at: String,
}

/// A binding with its project's path and name (the integration's list of
/// its kanflow project's repositories, M3 X3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundRepository {
    pub binding: Binding,
    /// The project's full path (`group/name`); `None` while the namespace
    /// row is missing (never for a live project).
    pub path: Option<String>,
    pub display_name: String,
}

/// What a bind asks for (M3 X3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NewBinding<'a> {
    pub integration: IntegrationId,
    pub external_project_id: &'a str,
    /// `false`: a shared spec repository (several kanflow projects).
    pub resolves_work_items: bool,
}

/// What a binding's change did (M3 X3).
#[must_use = "Unchanged wrote nothing; callers report it differently"]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindingUpdate {
    /// One `integration.binding_changed` row.
    Updated,
    /// The binding had that setting already: nothing written (an act-as
    /// request's assertion is still consumed).
    Unchanged,
}

/// What a bind did.
#[must_use = "Unchanged wrote nothing; callers report it differently"]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindOutcome {
    /// Written: one `integration.bound` row, and when `granted_membership`
    /// the service account's `member.added` row and event.
    Bound { granted_membership: bool },
    /// The project was bound to that external project already, with that
    /// setting: nothing written (an act-as request's assertion is still
    /// consumed).
    Unchanged,
}

/// What an unbind did: one `integration.unbound` row, and when
/// `membership_removed` the service account's `member.removed` row and
/// event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnbindOutcome {
    pub membership_removed: bool,
}

/// What a bind's or unbind's `decide` sees (the facts of
/// `projects::update_decided`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BindingFacts {
    pub account: Account,
    pub project: ProjectAccessFacts,
}

/// `FG-WEB-0006` (`field_invalid`) for `field` with `rule`.
pub(super) fn field_invalid(field: &'static str, rule: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::RequestInvalid)
        .with_context("reason", "field_invalid")
        .with_context("field", field)
        .with_context("rule", rule)
}

pub(super) fn not_found(resource: &'static str, reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::NotFound)
        .with_context("resource", resource)
        .with_context("reason", reason)
}

/// The allowlist as stored: every entry checked (`FG-WEB-0006
/// field=event_types`), sorted, without duplicates.
pub(super) fn event_types(given: Option<&[String]>) -> Result<Vec<String>, ForgeError> {
    let mut types = match given {
        Some(types) => types.to_vec(),
        None => default_event_types(),
    };
    if types.is_empty() || types.len() > MAX_EVENT_TYPE_ENTRIES {
        return Err(field_invalid("event_types", "count").with_context("count", types.len()));
    }
    if types.iter().any(|t| !event_type_entry_ok(t)) {
        return Err(field_invalid("event_types", "unknown_event_type"));
    }
    types.sort();
    types.dedup();
    Ok(types)
}
