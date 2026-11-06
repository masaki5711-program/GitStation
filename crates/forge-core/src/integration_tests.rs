use crate::ids::{IntegrationId, ProjectId, UserId};
use crate::integration::*;
use crate::AuditDetails;

#[test]
fn integration_kinds_round_trip() {
    for k in IntegrationKind::ALL {
        assert_eq!(IntegrationKind::parse(k.as_str()), Some(k));
    }
    assert_eq!(IntegrationKind::parse("gitlab"), None);
    assert_eq!(IntegrationKind::parse("Kanflow"), None);
}

#[test]
fn external_project_ids_are_short_and_printable() {
    for ok in ["P1", "kanflow:42", "team-a.board_7", &"x".repeat(100)] {
        assert!(external_project_id_ok(ok), "{ok}");
    }
    for bad in [
        "",
        &"x".repeat(101),
        "a b",
        "a/b",
        "a\nb",
        "プロジェクト",
        "a%2Fb",
    ] {
        assert!(!external_project_id_ok(bad), "{bad:?}");
    }
}

#[test]
fn event_type_entries_admit_the_integration_class_only() {
    for ok in [
        "*",
        "merge_request.opened",
        "merge_request.*",
        "repository.pushed",
        "repository.*",
        "project.*",
        // M3 I1b: the work item links joined the integration class.
        "work_item.*",
        "work_item.linked",
    ] {
        assert!(event_type_entry_ok(ok), "{ok}");
    }
    // Another class (maintainer / admin), unknown names and families,
    // malformed wildcards.
    for bad in [
        "member.added",
        "member.*",
        "token.created",
        "token.*",
        "project.purged",
        "merge_request.nope",
        "commit.*",
        ".*",
        "merge_*",
        "merge_request.**",
        "*.opened",
        "",
        "merge",
    ] {
        assert!(!event_type_entry_ok(bad), "{bad:?}");
    }
}

#[test]
fn a_new_integration_admits_every_integration_event_by_default() {
    assert_eq!(default_event_types(), vec![ALL_EVENT_TYPES.to_owned()]);
    assert!(default_event_types().iter().all(|e| event_type_entry_ok(e)));
}

#[test]
fn integration_audit_details_name_their_action_and_publish_no_event() {
    let bound = AuditDetails::Integration(IntegrationChange::Bound {
        integration: IntegrationId::new(3),
        project: ProjectId::new(9),
        external_project_id: "P1".to_owned(),
        service_account: UserId::new(4),
        granted_membership: true,
        binding: 21,
        resolves_work_items: false,
    });
    assert_eq!(bound.action(), "integration.bound");
    assert_eq!(bound.event_type(), None);
    assert_eq!(
        bound.to_value(),
        serde_json::json!({
            "integration_id": 3,
            "project_id": 9,
            "external_project_id": "P1",
            "service_account_id": 4,
            "granted_membership": true,
            "binding_id": 21,
            "resolves_work_items": false,
        })
    );
    let unbound = AuditDetails::Integration(IntegrationChange::Unbound {
        integration: IntegrationId::new(3),
        project: ProjectId::new(9),
        external_project_id: "P1".to_owned(),
        service_account: UserId::new(4),
        membership_removed: false,
        work_item_links_ended: 3,
        binding: 21,
        grant_moved_to: Some(22),
    });
    assert_eq!(unbound.action(), "integration.unbound");
    assert_eq!(unbound.to_value()["membership_removed"], false);
    assert_eq!(unbound.to_value()["work_item_links_ended"], 3);
    assert_eq!(unbound.to_value()["grant_moved_to"], 22);
    let changed = AuditDetails::Integration(IntegrationChange::BindingChanged {
        integration: IntegrationId::new(3),
        project: ProjectId::new(9),
        binding: 21,
        external_project_id: "P1".to_owned(),
        resolves_work_items: true,
    });
    assert_eq!(changed.action(), "integration.binding_changed");
    assert_eq!(changed.event_type(), None);
    assert_eq!(changed.to_value()["resolves_work_items"], true);
    let created = AuditDetails::Integration(IntegrationChange::Created {
        integration: IntegrationId::new(3),
        kind: IntegrationKind::Kanflow,
        service_account: UserId::new(4),
        event_types: default_event_types(),
    });
    assert_eq!(created.action(), "integration.created");
    assert_eq!(created.to_value()["kind"], "kanflow");
    let updated = AuditDetails::Integration(IntegrationChange::Updated {
        integration: IntegrationId::new(3),
        fields: vec![IntegrationField::Enabled],
        enabled: false,
        event_types: default_event_types(),
    });
    assert_eq!(updated.action(), "integration.updated");
    assert_eq!(updated.to_value()["fields"], serde_json::json!(["enabled"]));
    assert_eq!(updated.event_type(), None);
}
