//! Integrations and their project bindings (M3 I1a, plan 11 §5 I1, 02 §2,
//! ADR-0016 §1, §7): the kinds of integration, the shape of an external
//! project id, the event type allowlist entries, and the audit details of
//! a change. Shared by forge-db (which stores them) and forge-web (which
//! parses requests with them).

use crate::event_type::{EventType, VisibilityClass};
use crate::ids::{IntegrationId, ProjectId, UserId};

/// The kind of system an integration connects. Only kanflow (the PM tool,
/// plan 02) exists; the table CHECK `integrations_kind_known` lists the
/// same names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IntegrationKind {
    Kanflow,
}

impl IntegrationKind {
    pub const ALL: [IntegrationKind; 1] = [Self::Kanflow];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Kanflow => "kanflow",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.as_str() == raw)
    }
}

/// The longest external project id (the CHECK
/// `integration_bindings_external_project_id_format`).
pub const MAX_EXTERNAL_PROJECT_ID: usize = 100;

/// Whether `raw` is an external project id: 1..=100 characters of
/// `A-Z a-z 0-9 . _ : -` (kanflow's project id is opaque to forge; this
/// keeps it printable, short and free of path or log syntax).
pub fn external_project_id_ok(raw: &str) -> bool {
    !raw.is_empty()
        && raw.len() <= MAX_EXTERNAL_PROJECT_ID
        && raw
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-'))
}

/// At most this many entries in an integration's `event_types` (the CHECK
/// `integrations_event_types_count`).
pub const MAX_EVENT_TYPE_ENTRIES: usize = 64;

/// The allowlist entry that admits every integration-class event type,
/// those added later included (the default of a new integration).
pub const ALL_EVENT_TYPES: &str = "*";

/// Whether `raw` is one entry of an integration's event type allowlist
/// (ADR-0016 §7: the feed narrows the bound projects' integration-class
/// events by it): [`ALL_EVENT_TYPES`], an event type of the integration
/// class (`merge_request.opened`), or a family of them (`merge_request.*`)
/// that names at least one today. An unknown name, a type of another class
/// or a family without integration-class types is refused.
pub fn event_type_entry_ok(raw: &str) -> bool {
    let integration = |t: &EventType| t.visibility_class() == VisibilityClass::Integration;
    if raw == ALL_EVENT_TYPES {
        return true;
    }
    match raw.strip_suffix(".*") {
        Some(family) if !family.is_empty() && !family.contains('*') => {
            EventType::ALL.iter().filter(|t| integration(t)).any(|t| {
                t.as_str()
                    .strip_prefix(family)
                    .is_some_and(|rest| rest.starts_with('.'))
            })
        }
        Some(_) => false,
        None => EventType::parse(raw).is_some_and(|t| integration(&t)),
    }
}

/// The allowlist a new integration gets when none is given:
/// [`ALL_EVENT_TYPES`] alone.
pub fn default_event_types() -> Vec<String> {
    vec![ALL_EVENT_TYPES.to_owned()]
}

/// A field `integration.updated` changed (names only).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum IntegrationField {
    Enabled,
    EventTypes,
}

impl IntegrationField {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Enabled => "enabled",
            Self::EventTypes => "event_types",
        }
    }
}

/// The audit details of an integration change (`integration.*`, M3 I1a):
/// audit-only (no domain event; a binding's membership grant publishes
/// its own `member.added` / `member.removed`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IntegrationChange {
    /// `integration.created`, on (`integration`, id).
    Created {
        integration: IntegrationId,
        kind: IntegrationKind,
        service_account: UserId,
        event_types: Vec<String>,
    },
    /// `integration.updated`, on (`integration`, id): the fields changed
    /// and the state after the change.
    Updated {
        integration: IntegrationId,
        fields: Vec<IntegrationField>,
        enabled: bool,
        event_types: Vec<String>,
    },
    /// `integration.bound`, on (`project`, id): whether the service account
    /// was made a Reporter by this binding.
    Bound {
        integration: IntegrationId,
        project: ProjectId,
        external_project_id: String,
        service_account: UserId,
        granted_membership: bool,
    },
    /// `integration.unbound`, on (`project`, id): whether the membership the
    /// binding granted was removed with it.
    Unbound {
        integration: IntegrationId,
        project: ProjectId,
        external_project_id: String,
        service_account: UserId,
        membership_removed: bool,
    },
}

impl IntegrationChange {
    pub fn action(&self) -> &'static str {
        match self {
            Self::Created { .. } => "integration.created",
            Self::Updated { .. } => "integration.updated",
            Self::Bound { .. } => "integration.bound",
            Self::Unbound { .. } => "integration.unbound",
        }
    }

    /// Created `{"integration_id","kind","service_account_id","event_types"}`,
    /// Updated `{"integration_id","fields","enabled","event_types"}`,
    /// Bound `{"integration_id","project_id","external_project_id",
    /// "service_account_id","granted_membership"}`, Unbound the same with
    /// `"membership_removed"`.
    pub fn to_value(&self) -> serde_json::Value {
        match self {
            Self::Created {
                integration,
                kind,
                service_account,
                event_types,
            } => serde_json::json!({
                "integration_id": integration.get(),
                "kind": kind.as_str(),
                "service_account_id": service_account.get(),
                "event_types": event_types,
            }),
            Self::Updated {
                integration,
                fields,
                enabled,
                event_types,
            } => serde_json::json!({
                "integration_id": integration.get(),
                "fields": fields.iter().map(|f| f.as_str()).collect::<Vec<_>>(),
                "enabled": enabled,
                "event_types": event_types,
            }),
            Self::Bound {
                integration,
                project,
                external_project_id,
                service_account,
                granted_membership,
            } => serde_json::json!({
                "integration_id": integration.get(),
                "project_id": project.get(),
                "external_project_id": external_project_id,
                "service_account_id": service_account.get(),
                "granted_membership": granted_membership,
            }),
            Self::Unbound {
                integration,
                project,
                external_project_id,
                service_account,
                membership_removed,
            } => serde_json::json!({
                "integration_id": integration.get(),
                "project_id": project.get(),
                "external_project_id": external_project_id,
                "service_account_id": service_account.get(),
                "membership_removed": membership_removed,
            }),
        }
    }
}
