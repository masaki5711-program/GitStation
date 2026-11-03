use crate::work_item::{
    find_refs, LinkActorKind, LinkSource, UnlinkSource, WorkItem, WorkItemChange, WorkItemRef,
};
use crate::{EventType, IntegrationId, IntegrationKind, MergeRequestId, ProjectId, UserId};

/// The shared vectors kanflow tests its K1 matcher against (plan 02 §2).
const VECTORS: &str = include_str!("../../../docs/api/pbi-ref-vectors.json");

fn refs(text: &str) -> Vec<String> {
    find_refs(text)
        .into_iter()
        .map(|r| r.as_str().to_owned())
        .collect()
}

#[test]
fn pbi_ref_vectors_match_shared_contract() {
    let doc: serde_json::Value = serde_json::from_str(VECTORS).expect("vectors are JSON");
    assert_eq!(doc["version"], 1);
    let vectors = doc["vectors"].as_array().expect("vectors");
    assert!(vectors.len() >= 30, "the contract keeps its cases");
    for v in vectors {
        let text = v["text"].as_str().expect("text");
        let expected: Vec<String> = v["refs"]
            .as_array()
            .expect("refs")
            .iter()
            .map(|r| r.as_str().expect("ref").to_owned())
            .collect();
        assert_eq!(refs(text), expected, "{text:?}: {}", v["note"]);
        // Every expected ref is canonical (what the API and the table take).
        for r in &expected {
            assert_eq!(
                WorkItemRef::parse(r).map(|w| w.to_string()),
                Some(r.clone())
            );
        }
    }
}

#[test]
fn pbi_430_is_not_pbi_43() {
    assert_eq!(refs("PBI-430"), ["PBI-430"]);
    assert_eq!(refs("feature/pbi-430-x"), ["PBI-430"]);
    assert_eq!(refs("(PBI #430)"), ["PBI-430"]);
    assert!(!refs("PBI-430 PBI-4300").contains(&"PBI-43".to_owned()));
}

#[test]
fn a_long_text_is_scanned_to_the_end() {
    let text = format!("{}PBI-9", "x ".repeat(10_000));
    assert_eq!(refs(&text), ["PBI-9"]);
    // Many candidates that never match do not stop the scan.
    let text = format!("{}pbi-5", "pbipbipbi ".repeat(1_000));
    assert_eq!(refs(&text), ["PBI-5"]);
}

#[test]
fn only_the_canonical_form_parses() {
    for ok in ["PBI-1", "PBI-0", "PBI-1234567"] {
        assert!(WorkItemRef::parse(ok).is_some(), "{ok}");
    }
    for bad in [
        "",
        "PBI-",
        "pbi-1",
        "PBI #1",
        "PBI-01",
        "PBI-12345678",
        "PBI-1 ",
        "PBI-1a",
        "PBI-١",
        "ICE-1",
        "PBI--1",
    ] {
        assert!(WorkItemRef::parse(bad).is_none(), "{bad:?}");
    }
}

#[test]
fn link_sources_round_trip() {
    for s in LinkSource::ALL {
        assert_eq!(LinkSource::parse(s.as_str()), Some(s));
    }
    assert_eq!(LinkSource::parse("Auto"), None);
    for s in UnlinkSource::ALL {
        assert_eq!(UnlinkSource::parse(s.as_str()), Some(s));
    }
    for k in LinkActorKind::ALL {
        assert_eq!(LinkActorKind::parse(k.as_str()), Some(k));
        assert_eq!(k.is_person(), k == LinkActorKind::Human);
    }
}

fn item() -> WorkItem {
    WorkItem {
        id: 7,
        integration: IntegrationId::new(3),
        kind: IntegrationKind::Kanflow,
        external_project_id: "kf-12".to_owned(),
        reference: WorkItemRef::parse("PBI-43").expect("ref"),
    }
}

#[test]
fn work_item_events_carry_the_reference_and_are_integration_class() {
    let linked = WorkItemChange::Linked {
        project: ProjectId::new(5),
        merge_request: MergeRequestId::new(9),
        iid: 2,
        link: 11,
        work_item: item(),
        source: LinkSource::Manual,
        by: Some((UserId::new(4), LinkActorKind::Agent)),
        replaces: Some(8),
    };
    assert_eq!(linked.event_type(), EventType::WorkItemLinked);
    assert_eq!(linked.action(), "work_item.linked");
    assert_eq!(linked.dedup_prefix(), "work_item_linked");
    assert_eq!(
        linked.to_value(),
        serde_json::json!({
            "merge_request_id": 9, "iid": 2, "link_id": 11, "work_item_id": 7,
            "work_item": {"provider": "kanflow", "project_id": "kf-12", "ref": "PBI-43"},
            "integration_id": 3, "source": "manual", "by": 4, "by_kind": "agent",
            "replaces_link_id": 8,
        })
    );
    let unlinked = WorkItemChange::Unlinked {
        project: ProjectId::new(5),
        merge_request: MergeRequestId::new(9),
        iid: 2,
        link: 11,
        work_item: item(),
        source: UnlinkSource::Unbound,
        by: None,
    };
    assert_eq!(unlinked.event_type(), EventType::WorkItemUnlinked);
    assert_eq!(unlinked.dedup_prefix(), "work_item_unlinked");
    assert_eq!(unlinked.to_value()["by"], serde_json::Value::Null);
    for t in [EventType::WorkItemLinked, EventType::WorkItemUnlinked] {
        assert_eq!(
            t.visibility_class(),
            crate::VisibilityClass::Integration,
            "{t:?}"
        );
        assert!(crate::integration::event_type_entry_ok(t.as_str()));
    }
    assert!(crate::integration::event_type_entry_ok("work_item.*"));
}
