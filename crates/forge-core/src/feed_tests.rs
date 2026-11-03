//! The feed's visible set and cursor position (M3 I1c).

use std::time::Duration;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use forge_errors::ErrorCode;

use crate::feed::*;
use crate::{CursorKeys, EventType, IntegrationId, ProjectId, VisibilityClass};

fn keys() -> CursorKeys {
    CursorKeys::parse(&format!("1 {}\n", "a1".repeat(32))).expect("keys")
}

const AGE: Duration = Duration::from_secs(3600);
const INT: IntegrationId = IntegrationId::new(7);

fn project(id: i64, generation: i64) -> VisibleProject {
    VisibleProject {
        project: ProjectId::new(id),
        generation,
    }
}

fn all() -> Vec<String> {
    vec!["*".to_owned()]
}

fn rule(err: &forge_errors::ForgeError) -> String {
    err.context()
        .iter()
        .find(|(k, _)| *k == "rule")
        .map(|(_, v)| v.clone())
        .unwrap_or_default()
}

#[test]
fn the_digest_is_independent_of_order_and_duplicates() {
    let a = VisibleSet::from_authorized_decisions(INT, vec![project(2, 5), project(1, 3)], &all());
    let b = VisibleSet::from_authorized_decisions(
        INT,
        vec![project(1, 3), project(2, 5), project(1, 3)],
        &all(),
    );
    assert_eq!(a.digest(), b.digest());
    assert_eq!(a.project_ids(), [1, 2]);
}

/// ADR-0016 Amendment I1c (the I1b dependency): the same project bound
/// again has another generation, so the set — and a cursor's digest —
/// differs although the projects are the same.
#[test]
fn a_rebind_of_the_same_project_changes_the_digest() {
    let before = VisibleSet::from_authorized_decisions(INT, vec![project(1, 3)], &all());
    let after = VisibleSet::from_authorized_decisions(INT, vec![project(1, 4)], &all());
    assert_eq!(before.project_ids(), after.project_ids());
    assert_ne!(before.digest(), after.digest());
}

#[test]
fn the_digest_names_the_integration_and_the_allowlist() {
    let set = VisibleSet::from_authorized_decisions(INT, vec![project(1, 3)], &all());
    let other =
        VisibleSet::from_authorized_decisions(IntegrationId::new(8), vec![project(1, 3)], &all());
    let narrower = VisibleSet::from_authorized_decisions(
        INT,
        vec![project(1, 3)],
        &["merge_request.*".to_owned()],
    );
    assert_ne!(set.digest(), other.digest());
    assert_ne!(set.digest(), narrower.digest());
}

#[test]
fn the_allowlist_admits_integration_class_types_only() {
    let every = allowed_event_types(&all());
    assert!(every
        .iter()
        .all(|t| t.visibility_class() == VisibilityClass::Integration));
    assert!(every.contains(&EventType::MergeRequestClosed));
    assert!(!every.contains(&EventType::MemberAdded));
    assert!(!every.contains(&EventType::TokenCreated));
    let family = allowed_event_types(&["merge_request.*".to_owned()]);
    assert!(family
        .iter()
        .all(|t| t.as_str().starts_with("merge_request.")));
    assert!(family.contains(&EventType::MergeRequestMerged));
    let one = allowed_event_types(&["work_item.linked".to_owned()]);
    assert_eq!(one, [EventType::WorkItemLinked]);
    // A maintainer-class name admits nothing, even when listed.
    assert!(allowed_event_types(&["member.added".to_owned()]).is_empty());
    assert!(VisibleSet::from_authorized_decisions(
        INT,
        vec![project(1, 1)],
        &["member.*".to_owned()]
    )
    .shows_nothing());
}

fn position(seq: i64) -> FeedPosition {
    FeedPosition {
        integration: INT,
        epoch: [9; DIGEST_BYTES],
        visible_set: [4; DIGEST_BYTES],
        seq,
    }
}

#[test]
fn a_position_round_trips_through_its_cursor() {
    let k = keys();
    let cursor = position(41).seal(&k).expect("seal");
    assert_eq!(
        FeedPosition::open(&k, INT, &cursor, AGE).expect("open"),
        position(41)
    );
    // Sealed anew each time (random nonce): the text reveals nothing.
    assert_ne!(cursor, position(41).seal(&k).expect("seal"));
}

#[test]
fn another_integrations_cursor_is_int_0008_other_integration() {
    let k = keys();
    let cursor = position(41).seal(&k).expect("seal");
    let err = FeedPosition::open(&k, IntegrationId::new(8), &cursor, AGE).expect_err("foreign");
    assert_eq!(err.code(), ErrorCode::FeedCursorInvalid);
    assert_eq!(rule(&err), "other_integration");
}

#[test]
fn a_tampered_or_malformed_cursor_is_int_0008_with_its_rule() {
    let k = keys();
    let cursor = position(41).seal(&k).expect("seal");
    let mut bytes = URL_SAFE_NO_PAD.decode(&cursor).expect("b64");
    let last = bytes.len() - 1;
    bytes[last] ^= 1;
    let tampered = URL_SAFE_NO_PAD.encode(&bytes);
    let err = FeedPosition::open(&k, INT, &tampered, AGE).expect_err("tampered");
    assert_eq!(err.code(), ErrorCode::FeedCursorInvalid);
    assert_eq!(rule(&err), "tag");
    // The value never reaches the error's context.
    assert!(err.context().iter().all(|(_, v)| !v.contains(&tampered)));
    let err = FeedPosition::open(&k, INT, "not a cursor!", AGE).expect_err("encoding");
    assert_eq!(rule(&err), "encoding");
    // Another list's cursor (same keys, other purpose) fails the tag.
    let list = k.encode("projects.list:user=1", &[0; 8]).expect("list");
    assert_eq!(
        rule(&FeedPosition::open(&k, INT, &list, AGE).expect_err("list")),
        "tag"
    );
    // The right purpose with another payload shape.
    let odd = k.encode(CURSOR_PURPOSE, &[1, 2, 3]).expect("odd");
    assert_eq!(
        rule(&FeedPosition::open(&k, INT, &odd, AGE).expect_err("odd")),
        "payload"
    );
}

#[test]
fn an_authentic_cursor_past_max_age_is_int_0007_retention() {
    let k = keys();
    let mut payload = vec![1u8];
    payload.extend_from_slice(&7u64.to_be_bytes());
    payload.extend_from_slice(&[9; DIGEST_BYTES]);
    payload.extend_from_slice(&[4; DIGEST_BYTES]);
    payload.extend_from_slice(&41u64.to_be_bytes());
    let old = k.encode_at(CURSOR_PURPOSE, &payload, 1_000).expect("old");
    let err = FeedPosition::open(&k, INT, &old, AGE).expect_err("expired");
    assert_eq!(err.code(), ErrorCode::FeedCursorExpired);
    assert!(err
        .context()
        .iter()
        .any(|(k, v)| *k == "reason" && v == "retention"));
}
