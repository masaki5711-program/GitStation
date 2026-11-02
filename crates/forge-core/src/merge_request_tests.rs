use serde_json::json;

use crate::ids::{MergeRequestId, UserId};
use crate::{
    AuditDetails, EventType, MergeRequestChange, MergeRequestField, MergeRequestState,
    MergeRequestTransition, RefName,
};

fn branch(name: &str) -> RefName {
    RefName::parse(&format!("refs/heads/{name}")).expect("branch")
}

#[test]
fn merge_request_states_are_the_check_values() {
    let names: Vec<&str> = MergeRequestState::ALL.iter().map(|s| s.as_str()).collect();
    assert_eq!(names, ["open", "merged", "closed"]);
    for s in MergeRequestState::ALL {
        assert_eq!(MergeRequestState::parse(s.as_str()), Some(*s));
    }
    for other in ["", "Open", "opened", "draft", "open "] {
        assert_eq!(MergeRequestState::parse(other), None, "{other:?}");
    }
}

/// ADR-0024 §6: each change's details publish the event of its transition,
/// named like its audit action, with ids, ref names and field names only.
#[test]
fn merge_request_details_have_their_exact_shape_and_publish_their_event() {
    let mr = MergeRequestId::new(9);
    let opened = MergeRequestChange::Opened {
        merge_request: mr,
        iid: 3,
        author: UserId::new(4),
        source_branch: branch("feature/x"),
        target_branch: branch("main"),
        draft: true,
    };
    let updated = MergeRequestChange::Updated {
        merge_request: mr,
        iid: 3,
        fields: vec![MergeRequestField::Title, MergeRequestField::TargetBranch],
        target_branch: Some((branch("main"), branch("release"))),
    };
    let untargeted = MergeRequestChange::Updated {
        merge_request: mr,
        iid: 3,
        fields: vec![MergeRequestField::Description],
        target_branch: None,
    };
    let draft = MergeRequestChange::DraftChanged {
        merge_request: mr,
        iid: 3,
        draft: false,
    };
    let closed = MergeRequestChange::Closed {
        merge_request: mr,
        iid: 3,
    };
    let reopened = MergeRequestChange::Reopened {
        merge_request: mr,
        iid: 3,
    };
    assert_eq!(
        opened.to_value(),
        json!({"merge_request_id": 9, "iid": 3, "author_id": 4,
               "source_branch": "refs/heads/feature/x", "target_branch": "refs/heads/main",
               "draft": true})
    );
    assert_eq!(
        updated.to_value(),
        json!({"merge_request_id": 9, "iid": 3, "fields": ["title", "target_branch"],
               "target_branch": {"from": "refs/heads/main", "to": "refs/heads/release"}})
    );
    assert_eq!(
        untargeted.to_value(),
        json!({"merge_request_id": 9, "iid": 3, "fields": ["description"],
               "target_branch": null})
    );
    assert_eq!(
        draft.to_value(),
        json!({"merge_request_id": 9, "iid": 3, "draft": false})
    );
    assert_eq!(closed.to_value(), json!({"merge_request_id": 9, "iid": 3}));
    assert_eq!(reopened.to_value(), closed.to_value());
    let expected = [
        (
            opened,
            EventType::MergeRequestOpened,
            "merge_request_opened",
        ),
        (
            updated,
            EventType::MergeRequestUpdated,
            "merge_request_updated",
        ),
        (
            draft,
            EventType::MergeRequestDraftChanged,
            "merge_request_draft_changed",
        ),
        (
            closed,
            EventType::MergeRequestClosed,
            "merge_request_closed",
        ),
        (
            reopened,
            EventType::MergeRequestReopened,
            "merge_request_reopened",
        ),
    ];
    for (change, event, prefix) in expected {
        assert_eq!(change.merge_request(), mr);
        assert_eq!(change.dedup_prefix(), prefix);
        let details = AuditDetails::MergeRequest(change);
        assert_eq!(details.event_type(), Some(event));
        assert_eq!(details.action(), event.as_str());
        assert_eq!(details.event_payload(), details.to_value());
    }
}

#[test]
fn every_transition_maps_to_its_own_event() {
    let events: Vec<&str> = MergeRequestTransition::ALL
        .iter()
        .map(|t| t.event_type().as_str())
        .collect();
    assert_eq!(
        events,
        [
            "merge_request.opened",
            "merge_request.closed",
            "merge_request.reopened",
            "merge_request.draft_changed",
            "merge_request.updated"
        ]
    );
}
