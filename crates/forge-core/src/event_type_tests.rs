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
    // D4 plan: project.created is the one project event of the integration
    // class (project.visibility_changed stays maintainer, ADR-0016 §5).
    assert_eq!(
        EventType::ProjectCreated.visibility_class(),
        VisibilityClass::Integration
    );
}

/// ADR-0016 §10 Amendment D5: deletion and restore are integration events
/// (an integration must learn that a project went away or came back).
#[test]
fn project_deleted_and_restored_are_integration_class() {
    for t in [EventType::ProjectDeleted, EventType::ProjectRestored] {
        assert_eq!(t.visibility_class(), VisibilityClass::Integration, "{t:?}");
    }
}

/// Step D5 plan answer (ADR-0016 §10 Amendment D5): the purge is an admin
/// event — the project is gone, so no project role can authorize a reader.
#[test]
fn project_purged_is_admin_class() {
    assert_eq!(
        EventType::ProjectPurged.visibility_class(),
        VisibilityClass::Admin
    );
}

/// D5 MR5: a project's own move is an integration event (step D5 plan
/// answer "other project.* integration"); a group move and a user rename
/// name no project, so they are instance-wide admin events (ADR-0016 §5).
#[test]
fn move_events_are_integration_for_projects_and_admin_otherwise() {
    assert_eq!(
        EventType::ProjectMoved.visibility_class(),
        VisibilityClass::Integration
    );
    for t in [EventType::GroupMoved, EventType::UserRenamed] {
        assert_eq!(t.visibility_class(), VisibilityClass::Admin, "{t:?}");
    }
}

#[test]
fn all_lists_every_event_type_once() {
    let names: Vec<&str> = EventType::ALL.iter().map(|t| t.as_str()).collect();
    assert_eq!(
        names,
        [
            "project.created",
            "repository.created",
            "token.created",
            "token.revoked",
            "member.added",
            "member.role_changed",
            "member.removed",
            "protected_ref.created",
            "protected_ref.updated",
            "protected_ref.deleted",
            "repository.pushed",
            "project.visibility_changed",
            "project.deleted",
            "project.restored",
            "project.purged",
            "project.moved",
            "group.moved",
            "user.renamed"
        ]
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
        forced: false,
        ownerless_groups: 0,
        ownerless_projects: 0,
        left_without_admin: false,
    };
    assert_eq!(disabled.event_type(), None);
    let bootstrapped = AuditDetails::AdminBootstrapped {
        created: false,
        namespace_id: None,
    };
    assert_eq!(bootstrapped.event_type(), None);
    let created = AuditDetails::UserCreated {
        kind: UserKind::Human,
        namespace_id: NamespaceId::new(1),
        is_admin: false,
    };
    assert_eq!(created.event_type(), None);
}

/// D4 MR4: a project's `member.*` events are maintainer class (ADR-0016 §5,
/// D4 plan answer 3); nothing lower may read them.
#[test]
fn project_member_events_are_maintainer_class() {
    for t in [
        EventType::MemberAdded,
        EventType::MemberRoleChanged,
        EventType::MemberRemoved,
    ] {
        assert_eq!(t.visibility_class(), VisibilityClass::Maintainer, "{t:?}");
        assert!(t.as_str().starts_with("member."), "{t:?}");
    }
}

/// D4 MR5: `protected_ref.*` stays maintainer class (ADR-0016 §5 over Q7).
#[test]
fn protected_ref_events_are_maintainer_class() {
    for t in [
        EventType::ProtectedRefCreated,
        EventType::ProtectedRefUpdated,
        EventType::ProtectedRefDeleted,
    ] {
        assert_eq!(t.visibility_class(), VisibilityClass::Maintainer, "{t:?}");
        assert!(t.as_str().starts_with("protected_ref."), "{t:?}");
    }
}

/// M1 phase 9 MR2: `repository.pushed` is an integration event (ADR-0016 §5).
#[test]
fn repository_pushed_is_integration_class() {
    assert_eq!(EventType::RepositoryPushed.as_str(), "repository.pushed");
    assert_eq!(
        EventType::RepositoryPushed.visibility_class(),
        VisibilityClass::Integration
    );
}
