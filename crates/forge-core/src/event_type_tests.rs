use crate::event_type::{EventType, VisibilityClass};
use crate::ids::{NamespaceId, UserId};
use crate::{AuditDetails, UserKind};

/// The shape of the `forge.events.event_type` CHECK:
/// `^[a-z][a-z_]*(\.[a-z][a-z_]*){1,3}$`.
fn matches_db_shape(name: &str) -> bool {
    let parts: Vec<&str> = name.split('.').collect();
    (2..=4).contains(&parts.len())
        && parts.iter().all(|p| {
            p.starts_with(|c: char| c.is_ascii_lowercase())
                && p.chars().all(|c| c.is_ascii_lowercase() || c == '_')
        })
}

#[test]
fn event_type_names_have_the_db_shape_and_are_distinct() {
    let mut seen = std::collections::HashSet::new();
    for t in EventType::ALL {
        assert!(matches_db_shape(t.as_str()), "{t:?}");
        assert!(seen.insert(t.as_str()), "{t:?} repeats a name");
        assert_eq!(EventType::parse(t.as_str()), Some(t));
    }
    for other in [
        "",
        "token",
        "Token.created",
        "token.Created",
        "token.created ",
        "x.y",
    ] {
        assert_eq!(EventType::parse(other), None, "{other:?}");
    }
}

#[test]
fn token_events_are_admin_class() {
    assert_eq!(
        EventType::TokenCreated.visibility_class(),
        VisibilityClass::Admin
    );
    assert_eq!(
        EventType::TokenRevoked.visibility_class(),
        VisibilityClass::Admin
    );
    // ADR-0016 §10 and the D-plan answer Q7: repository.created is an
    // integration event.
    assert_eq!(
        EventType::RepositoryCreated.visibility_class(),
        VisibilityClass::Integration
    );
}

#[test]
fn visibility_class_names_are_the_adr_names() {
    let names: Vec<&str> = VisibilityClass::ALL.iter().map(|c| c.as_str()).collect();
    assert_eq!(names, ["integration", "maintainer", "deployment", "admin"]);
}

#[test]
fn only_published_audit_actions_name_an_event_type() {
    let user = UserId::new(7);
    let token = AuditDetails::TokenRevoked {
        token_id: 1,
        user,
        public_id: "0123456789abcdef".to_owned(),
    };
    assert_eq!(token.event_type(), Some(EventType::TokenRevoked));
    assert_eq!(
        token.event_type().map(EventType::as_str),
        Some(token.action())
    );
    // user.* is audit-only (D-plan answer Q4).
    let disabled = AuditDetails::UserDisabled {
        kind: UserKind::Human,
        revoked_tokens: 0,
    };
    assert_eq!(disabled.event_type(), None);
    let created = AuditDetails::UserCreated {
        kind: UserKind::Human,
        namespace_id: NamespaceId::new(1),
        is_admin: false,
    };
    assert_eq!(created.event_type(), None);
}
