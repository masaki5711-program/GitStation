//! Domain event types (ADR-0016 §5): the one list of the `forge.events`
//! `event_type` values forge writes. A variant joins in the commit that adds
//! its writer (step D3 MR1b: the token events, and `repository.created`
//! for D4; D4 MR2: `project.created`; D4 MR4: `member.*` of projects; D4 MR5: `protected_ref.*`; M1 phase 9 MR2: `repository.pushed`); subscribers match it without `_`.

/// Who may read an event type (ADR-0016 §5 table); the feed and webhooks
/// decide with it through `authorize()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VisibilityClass {
    /// Integration feed and webhooks; project Reporter.
    Integration,
    /// Admin feed and webhooks naming the type; Maintainer.
    Maintainer,
    /// Admin feed and webhooks naming the type; Developer.
    Deployment,
    /// Admin feed only; administrator with the `admin` scope.
    Admin,
}

impl VisibilityClass {
    /// Every class, in declaration order.
    pub const ALL: [VisibilityClass; 4] = [
        VisibilityClass::Integration,
        VisibilityClass::Maintainer,
        VisibilityClass::Deployment,
        VisibilityClass::Admin,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            VisibilityClass::Integration => "integration",
            VisibilityClass::Maintainer => "maintainer",
            VisibilityClass::Deployment => "deployment",
            VisibilityClass::Admin => "admin",
        }
    }
}

/// The type of a domain event: `<aggregate>.<past-tense verb>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EventType {
    /// `project.created` (D4 MR2; integration class, step D plan: the one
    /// `project.*` event of that class).
    ProjectCreated,
    /// `repository.created` (D4; integration class, D-plan answer Q7).
    RepositoryCreated,
    /// `token.created` (D3, ADR-0019).
    TokenCreated,
    /// `token.revoked` (D3, ADR-0019).
    TokenRevoked,
    /// `member.added` of a project (D4 MR4; maintainer class, ADR-0016 §5).
    /// Group member changes stay audit-only.
    MemberAdded,
    /// `member.role_changed` of a project (D4 MR4; maintainer class).
    MemberRoleChanged,
    /// `member.removed` of a project (D4 MR4; maintainer class).
    MemberRemoved,
    /// `protected_ref.created` (D4 MR5; maintainer class, ADR-0016 §5).
    ProtectedRefCreated,
    /// `protected_ref.updated` (D4 MR5; maintainer class).
    ProtectedRefUpdated,
    /// `protected_ref.deleted` (D4 MR5; maintainer class).
    ProtectedRefDeleted,
    /// `repository.pushed` (M1 phase 9 MR2; integration class, ADR-0016 §5):
    /// one per applied push, written by `repositories::record_push`.
    RepositoryPushed,
}

impl EventType {
    /// Every event type, in declaration order.
    pub const ALL: [EventType; 11] = [
        EventType::ProjectCreated,
        EventType::RepositoryCreated,
        EventType::TokenCreated,
        EventType::TokenRevoked,
        EventType::MemberAdded,
        EventType::MemberRoleChanged,
        EventType::MemberRemoved,
        EventType::ProtectedRefCreated,
        EventType::ProtectedRefUpdated,
        EventType::ProtectedRefDeleted,
        EventType::RepositoryPushed,
    ];

    /// The stored name (passes the `forge.events.event_type` CHECK; tested).
    pub fn as_str(self) -> &'static str {
        match self {
            EventType::ProjectCreated => "project.created",
            EventType::RepositoryCreated => "repository.created",
            EventType::TokenCreated => "token.created",
            EventType::TokenRevoked => "token.revoked",
            EventType::MemberAdded => "member.added",
            EventType::MemberRoleChanged => "member.role_changed",
            EventType::MemberRemoved => "member.removed",
            EventType::ProtectedRefCreated => "protected_ref.created",
            EventType::ProtectedRefUpdated => "protected_ref.updated",
            EventType::ProtectedRefDeleted => "protected_ref.deleted",
            EventType::RepositoryPushed => "repository.pushed",
        }
    }

    /// The type named `name` (its `as_str`); `None` for any other string.
    pub fn parse(name: &str) -> Option<EventType> {
        Self::ALL.into_iter().find(|t| t.as_str() == name)
    }

    /// The least privilege that may read events of this type.
    pub fn visibility_class(self) -> VisibilityClass {
        match self {
            EventType::ProjectCreated
            | EventType::RepositoryCreated
            | EventType::RepositoryPushed => VisibilityClass::Integration,
            EventType::TokenCreated | EventType::TokenRevoked => VisibilityClass::Admin,
            EventType::MemberAdded
            | EventType::MemberRoleChanged
            | EventType::MemberRemoved
            | EventType::ProtectedRefCreated
            | EventType::ProtectedRefUpdated
            | EventType::ProtectedRefDeleted => VisibilityClass::Maintainer,
        }
    }
}
