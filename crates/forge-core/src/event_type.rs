//! Domain event types (ADR-0016 §5): the one list of the `forge.events`
//! `event_type` values forge writes. A variant joins in the commit that adds
//! its writer (step D3 MR1b: the token events, and `repository.created`
//! for D4; D4 MR2: `project.created`); subscribers match it without `_`.

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
}

impl EventType {
    /// Every event type, in declaration order.
    pub const ALL: [EventType; 4] = [
        EventType::ProjectCreated,
        EventType::RepositoryCreated,
        EventType::TokenCreated,
        EventType::TokenRevoked,
    ];

    /// The stored name (passes the `forge.events.event_type` CHECK; tested).
    pub fn as_str(self) -> &'static str {
        match self {
            EventType::ProjectCreated => "project.created",
            EventType::RepositoryCreated => "repository.created",
            EventType::TokenCreated => "token.created",
            EventType::TokenRevoked => "token.revoked",
        }
    }

    /// The type named `name` (its `as_str`); `None` for any other string.
    pub fn parse(name: &str) -> Option<EventType> {
        Self::ALL.into_iter().find(|t| t.as_str() == name)
    }

    /// The least privilege that may read events of this type.
    pub fn visibility_class(self) -> VisibilityClass {
        match self {
            EventType::ProjectCreated | EventType::RepositoryCreated => {
                VisibilityClass::Integration
            }
            EventType::TokenCreated | EventType::TokenRevoked => VisibilityClass::Admin,
        }
    }
}
