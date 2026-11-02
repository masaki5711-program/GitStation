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
            "merge_request.updated",
            "merge_request.pushed",
            "merge_request.conflict_detected",
            "merge_request.conflict_resolved",
            "merge_request.commented",
            "merge_request.merged",
            "merge_request.source_branch_deleted"
        ]
    );
}

/// M3 R4: the merge and the source branch deletion are transitions with
/// their own events, deduplicated by the merge attempt (ADR-0024 §4.2
/// steps 9 and 10); the payload carries ids, names and SHAs only.
#[test]
fn merged_and_source_deleted_are_transitions_keyed_by_the_merge() {
    use crate::{MergeMethod, ObjectId, RefName};
    let mr = MergeRequestId::new(9);
    let oid = |c: &str| ObjectId::parse(&c.repeat(40)).expect("oid");
    let merged = MergeRequestChange::Merged {
        merge_request: mr,
        iid: 3,
        merge_id: 17,
        method: MergeMethod::Squash,
        target_branch: RefName::parse("refs/heads/main").expect("ref"),
        target_sha: oid("a"),
        merged_sha: oid("b"),
        head_sha: oid("c"),
        merged_by: UserId::new(4),
    };
    assert_eq!(merged.transition(), MergeRequestTransition::Merge);
    assert_eq!(merged.dedup_prefix(), "merge_request_merged");
    assert_eq!(merged.action(), "merge_request.merged");
    let value = merged.to_value();
    assert_eq!(value["method"], "squash");
    assert_eq!(value["merged_by"], 4);
    assert_eq!(value["merge_id"], 17);
    let deleted = MergeRequestChange::SourceBranchDeleted {
        merge_request: mr,
        iid: 3,
        merge_id: 17,
        source_branch: RefName::parse("refs/heads/feature").expect("ref"),
        old: oid("c"),
    };
    assert_eq!(
        deleted.transition(),
        MergeRequestTransition::SourceBranchDelete
    );
    assert_eq!(deleted.dedup_prefix(), "merge_source_deleted");
    assert_eq!(deleted.to_value()["source_branch"], "refs/heads/feature");
}

/// M3 R1b: a push is the `merge_request.pushed` transition; its event is
/// deduplicated per head commit (ADR-0024 §6). The version changes that are
/// not transitions are audit-only.
#[test]
fn pushed_is_a_transition_and_version_changes_are_audit_only() {
    use crate::{MergeRequestVersionChange, ObjectId};
    let mr = MergeRequestId::new(9);
    let head = ObjectId::parse(&"c".repeat(40)).expect("oid");
    let before = ObjectId::parse(&"b".repeat(40)).expect("oid");
    let pushed = MergeRequestChange::Pushed {
        merge_request: mr,
        iid: 3,
        version: 2,
        head_sha: head.clone(),
        previous_head: Some(before.clone()),
        pushed_by: Some(UserId::new(4)),
    };
    assert_eq!(pushed.transition(), MergeRequestTransition::Push);
    assert_eq!(pushed.dedup_prefix(), "merge_request_pushed");
    assert_eq!(
        pushed.to_value(),
        json!({"merge_request_id": 9, "iid": 3, "version": 2, "head_sha": "c".repeat(40),
               "previous_head": "b".repeat(40), "pushed_by": 4})
    );
    let details = AuditDetails::MergeRequest(pushed);
    assert_eq!(details.event_type(), Some(EventType::MergeRequestPushed));
    assert_eq!(details.action(), "merge_request.pushed");
    let restored = AuditDetails::MergeRequestVersion(MergeRequestVersionChange::HeadRestored {
        merge_request: mr,
        iid: 3,
        version: 1,
        head_sha: before,
        previous_head: Some(head),
    });
    assert_eq!(restored.event_type(), None);
    assert_eq!(restored.action(), "merge_request.head_restored");
    let purged = AuditDetails::MergeRequestVersion(MergeRequestVersionChange::VersionRefsPurged {
        merge_request: mr,
        iid: 3,
        versions: vec![1, 2],
    });
    assert_eq!(purged.event_type(), None);
    assert_eq!(purged.action(), "merge_request.version_refs_purged");
    let back = AuditDetails::MergeRequestVersion(MergeRequestVersionChange::PurgedHeadPushed {
        merge_request: mr,
        iid: 3,
        version: 1,
        head_sha: ObjectId::parse(&"d".repeat(40)).expect("oid"),
    });
    assert_eq!(back.event_type(), None);
    assert_eq!(back.action(), "merge_request.purged_head_pushed");
    assert_eq!(back.to_value()["version"], 1);
    assert_eq!(
        purged.to_value(),
        json!({"merge_request_id": 9, "iid": 3, "versions": [1, 2]})
    );
}

/// M3 R2 (ADR-0024 §3, §6): a conflict flip is a transition with its own
/// event, deduplicated per check; the payload carries ids, SHAs and the
/// file count, never a path.
#[test]
fn conflict_flips_are_transitions_with_ids_and_shas_only() {
    use crate::ObjectId;
    let mr = MergeRequestId::new(9);
    let head = ObjectId::parse(&"c".repeat(40)).expect("oid");
    let target = ObjectId::parse(&"d".repeat(40)).expect("oid");
    let detected = MergeRequestChange::ConflictDetected {
        merge_request: mr,
        iid: 3,
        check_seq: 5,
        head_sha: head.clone(),
        target_sha: target.clone(),
        conflict_files: 2,
        files_truncated: false,
    };
    let resolved = MergeRequestChange::ConflictResolved {
        merge_request: mr,
        iid: 3,
        check_seq: 6,
        head_sha: head,
        target_sha: target,
    };
    assert_eq!(
        detected.transition(),
        MergeRequestTransition::ConflictDetect
    );
    assert_eq!(
        resolved.transition(),
        MergeRequestTransition::ConflictResolve
    );
    assert_eq!(detected.dedup_prefix(), "merge_request_conflict_detected");
    assert_eq!(resolved.dedup_prefix(), "merge_request_conflict_resolved");
    assert_eq!(detected.merge_request(), mr);
    assert_eq!(
        detected.to_value(),
        json!({"merge_request_id": 9, "iid": 3, "check_seq": 5, "head_sha": "c".repeat(40),
               "target_sha": "d".repeat(40), "conflict_files": 2, "files_truncated": false})
    );
    assert_eq!(
        resolved.to_value(),
        json!({"merge_request_id": 9, "iid": 3, "check_seq": 6, "head_sha": "c".repeat(40),
               "target_sha": "d".repeat(40)})
    );
    for (change, event) in [
        (detected, EventType::MergeRequestConflictDetected),
        (resolved, EventType::MergeRequestConflictResolved),
    ] {
        let details = AuditDetails::MergeRequest(change);
        assert_eq!(details.event_type(), Some(event));
        assert_eq!(details.action(), event.as_str());
        assert_eq!(details.event_payload(), details.to_value());
    }
}

/// M3 R2: the bases of a version are an audit-only change.
#[test]
fn recorded_bases_are_audit_only() {
    use crate::{MergeRequestVersionChange, ObjectId};
    let bases = AuditDetails::MergeRequestVersion(MergeRequestVersionChange::BasesRecorded {
        merge_request: MergeRequestId::new(9),
        iid: 3,
        version: 2,
        base_sha: ObjectId::parse(&"a".repeat(40)).expect("oid"),
        merge_base_sha: None,
    });
    assert_eq!(bases.event_type(), None);
    assert_eq!(bases.action(), "merge_request.version_bases_recorded");
    assert_eq!(
        bases.to_value(),
        json!({"merge_request_id": 9, "iid": 3, "version": 2, "base_sha": "a".repeat(40),
               "merge_base_sha": null})
    );
}

/// M3 R2: the mergeability statuses are the table's CHECK values; only
/// "can" and "cannot" are decisions.
#[test]
fn mergeability_statuses_are_the_check_values_and_only_two_decide() {
    use crate::MergeabilityStatus;
    let names: Vec<&str> = MergeabilityStatus::ALL.iter().map(|s| s.as_str()).collect();
    assert_eq!(
        names,
        [
            "unchecked",
            "checking",
            "can_be_merged",
            "cannot_be_merged",
            "check_failed"
        ]
    );
    for s in MergeabilityStatus::ALL {
        assert_eq!(MergeabilityStatus::parse(s.as_str()), Some(*s));
    }
    assert_eq!(MergeabilityStatus::parse("mergeable"), None);
    let decided: Vec<&str> = MergeabilityStatus::ALL
        .iter()
        .filter(|s| s.is_decided())
        .map(|s| s.as_str())
        .collect();
    assert_eq!(decided, ["can_be_merged", "cannot_be_merged"]);
}

/// M3 R2: the reasons are the stored values; only the ones a rerun on the
/// same pair repeats answer that pair.
#[test]
fn check_reasons_round_trip_and_only_deterministic_ones_answer_a_pair() {
    use crate::CheckReason;
    for r in CheckReason::ALL {
        assert_eq!(CheckReason::parse(r.as_str()), Some(*r));
    }
    let answering: Vec<&str> = CheckReason::ALL
        .iter()
        .filter(|r| r.is_deterministic())
        .map(|r| r.as_str())
        .collect();
    assert_eq!(
        answering,
        [
            "conflict",
            "target_missing",
            "unrelated_histories",
            "no_version"
        ]
    );
}

/// M3 R3 (ADR-0024 §6): comments and thread resolution are one transition
/// (`merge_request.commented`) whose `data.action` tells them apart; ids
/// and flags only, never the body or the path. A note dedups on its id, a
/// resolution on the discussion and its revision.
#[test]
fn comments_are_one_transition_with_an_action_and_ids_only() {
    use crate::{CommentAction, DiscussionId, DiscussionKind, NoteId};
    let added = MergeRequestChange::Commented {
        merge_request: MergeRequestId::new(7),
        iid: 3,
        action: CommentAction::NoteAdded,
        discussion: DiscussionId::new(11),
        kind: DiscussionKind::Inline,
        note: Some(NoteId::new(21)),
        discussion_started: true,
        revision: 1,
    };
    assert_eq!(added.transition(), MergeRequestTransition::Comment);
    assert_eq!(added.action(), "merge_request.commented");
    assert_eq!(added.dedup_prefix(), "merge_request_note_added");
    assert_eq!(
        added.to_value(),
        json!({
            "merge_request_id": 7,
            "iid": 3,
            "action": "note_added",
            "discussion_id": 11,
            "kind": "inline",
            "note_id": 21,
            "discussion_started": true,
        })
    );
    let resolved = MergeRequestChange::Commented {
        merge_request: MergeRequestId::new(7),
        iid: 3,
        action: CommentAction::ThreadResolved,
        discussion: DiscussionId::new(11),
        kind: DiscussionKind::General,
        note: None,
        discussion_started: false,
        revision: 2,
    };
    assert_eq!(resolved.dedup_prefix(), "merge_request_thread_resolved");
    assert_eq!(resolved.to_value()["note_id"], serde_json::Value::Null);
    let details = AuditDetails::MergeRequest(resolved);
    assert_eq!(details.event_type(), Some(EventType::MergeRequestCommented));
    assert_eq!(details.event_payload(), details.to_value());
}
