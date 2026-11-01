#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! The pure parts of the lists: the query and the page walk (budget cuts,
//! denied rows never shown, the cursor position) and the cursor binding.

use forge_core::{CursorKeys, UserId};
use forge_errors::{ErrorCode, ForgeError};

use super::*;

const AGE: Duration = Duration::from_secs(3600);

fn rule(err: &ForgeError) -> (ErrorCode, String, String) {
    let get = |k: &str| {
        err.context()
            .iter()
            .find(|(key, _)| *key == k)
            .map(|(_, v)| v.to_string())
            .unwrap_or_default()
    };
    (err.code(), get("param"), get("rule"))
}

#[test]
fn the_query_takes_limit_and_cursor_only() {
    assert_eq!(
        parse_query(None).expect("none"),
        ListQuery {
            limit: DEFAULT_LIMIT,
            cursor: None
        }
    );
    assert_eq!(
        parse_query(Some("limit=100&cursor=abc")).expect("both"),
        ListQuery {
            limit: 100,
            cursor: Some("abc".to_owned())
        }
    );
    assert_eq!(parse_query(Some("limit=1")).expect("one").limit, 1);
    let cases = [
        ("limit=0", "limit", "out_of_range"),
        ("limit=101", "limit", "out_of_range"),
        ("limit=01", "limit", "out_of_range"),
        ("limit=+5", "limit", "out_of_range"),
        ("limit=abc", "limit", "out_of_range"),
        ("limit=", "limit", "out_of_range"),
        ("limit=99999999999999999999999", "limit", "out_of_range"),
        ("limit=5&limit=6", "limit", "repeated"),
        ("cursor=a&cursor=b", "cursor", "repeated"),
        ("cursor=", "cursor", "empty"),
        ("page=2", "query", "unknown_parameter"),
        ("Limit=2", "query", "unknown_parameter"),
    ];
    for (query, param, expected) in cases {
        let err = parse_query(Some(query)).expect_err(query);
        assert_eq!(
            rule(&err),
            (
                ErrorCode::RequestInvalid,
                param.to_owned(),
                expected.to_owned()
            ),
            "{query}"
        );
    }
}

/// Rows 1..=n; `visible` decides which `show` lets through.
fn walk(
    n: i64,
    after: Option<i64>,
    limit: usize,
    budget: usize,
    visible: impl Fn(i64) -> bool,
) -> Paged<i64> {
    let start = after.unwrap_or(0) + 1;
    let rows: Vec<i64> = (start..=n).take(budget + 1).collect();
    paginate(
        &rows,
        after,
        limit,
        budget,
        |r| *r,
        |r| Ok(visible(*r).then_some(*r)),
    )
    .expect("page")
}

#[test]
fn a_page_never_shows_a_denied_row_and_stops_at_the_limit() {
    let page = walk(10, None, 3, 1000, |id| id % 2 == 0);
    assert_eq!(page.items, [2, 4, 6]);
    assert_eq!(page.next_after, Some(6), "the last row consumed");
    assert!(page.has_more);
    let last = walk(10, Some(6), 3, 1000, |id| id % 2 == 0);
    assert_eq!(last.items, [8, 10]);
    assert_eq!(last.next_after, None);
    assert!(!last.has_more);
    // Exactly `limit` rows left: the last page says so without an extra page.
    let exact = walk(4, None, 4, 1000, |_| true);
    assert_eq!(exact.items, [1, 2, 3, 4]);
    assert!(!exact.has_more);
    assert_eq!(exact.next_after, None);
}

#[test]
fn a_budget_cut_page_is_short_and_continues_after_the_last_scanned_row() {
    // Budget 4, only 9 and 10 visible: two empty pages, then the rows.
    let first = walk(10, None, 2, 4, |id| id >= 9);
    assert!(first.items.is_empty());
    assert!(first.has_more);
    assert_eq!(first.next_after, Some(4), "the last row scanned");
    let second = walk(10, first.next_after, 2, 4, |id| id >= 9);
    assert!(second.items.is_empty());
    assert_eq!(second.next_after, Some(8));
    let third = walk(10, second.next_after, 2, 4, |id| id >= 9);
    assert_eq!(third.items, [9, 10]);
    assert!(!third.has_more);
    // Exactly the budget left and nothing after: the end.
    let end = walk(4, None, 10, 4, |_| false);
    assert!(end.items.is_empty());
    assert!(!end.has_more);
    assert_eq!(end.next_after, None);
}

#[test]
fn a_failure_fails_the_page_and_rows_out_of_order_are_refused() {
    let rows = [1i64, 2, 3];
    let failed = paginate(
        &rows,
        None,
        10,
        10,
        |r| *r,
        |r| {
            if *r == 2 {
                Err(ForgeError::new(ErrorCode::Internal))
            } else {
                Ok(Some(*r))
            }
        },
    );
    assert_eq!(failed.expect_err("fails").code(), ErrorCode::Internal);
    for (rows, after) in [
        (vec![3i64, 2], None),
        (vec![5], Some(5)),
        (vec![4], Some(5)),
    ] {
        let err =
            paginate(&rows, after, 10, 10, |r| *r, |r| Ok(Some(*r))).expect_err("out of order");
        assert_eq!(err.code(), ErrorCode::Internal, "{rows:?} after {after:?}");
    }
}

#[test]
fn only_an_internal_error_is_a_failure_every_denial_hides_the_row() {
    for code in [
        ErrorCode::NotFound,
        ErrorCode::Forbidden,
        ErrorCode::PendingDeletion,
        ErrorCode::InsufficientScope,
    ] {
        assert!(denied_or_failed(ForgeError::new(code)).is_ok(), "{code:?}");
    }
    assert!(denied_or_failed(ForgeError::new(ErrorCode::Internal)).is_err());
}

fn keys() -> CursorKeys {
    CursorKeys::parse(&format!("1 {}\n", "ab".repeat(32))).expect("keys")
}

#[test]
fn a_cursor_opens_only_for_its_list_and_caller() {
    let keys = keys();
    let alice = UserId::new(1);
    let bob = UserId::new(2);
    let cursor = next_cursor(&keys, List::Projects, alice, &Filter::NONE, Some(42))
        .expect("seal")
        .expect("a cursor");
    assert_eq!(
        open_cursor(
            &keys,
            List::Projects,
            alice,
            &Filter::NONE,
            Some(&cursor),
            AGE
        )
        .expect("own"),
        Some(42)
    );
    for (list, who) in [(List::Projects, bob), (List::Groups, alice)] {
        let err =
            open_cursor(&keys, list, who, &Filter::NONE, Some(&cursor), AGE).expect_err("foreign");
        assert_eq!(err.code(), ErrorCode::RequestInvalid);
        assert!(
            err.context()
                .iter()
                .any(|(k, v)| *k == "rule" && v == "tag"),
            "{err}"
        );
    }
    assert_eq!(
        next_cursor(&keys, List::Projects, alice, &Filter::NONE, None).expect("none"),
        None
    );
    // A payload that is not one positive id.
    for payload in [&[0u8; 4][..], &0i64.to_be_bytes(), &(-1i64).to_be_bytes()] {
        let sealed = keys
            .encode(&List::Projects.purpose(alice, &Filter::NONE), payload)
            .expect("seal");
        let err = open_cursor(
            &keys,
            List::Projects,
            alice,
            &Filter::NONE,
            Some(&sealed),
            AGE,
        )
        .expect_err("payload");
        assert!(
            err.context()
                .iter()
                .any(|(k, v)| *k == "rule" && v == "payload"),
            "{err}"
        );
    }
}

#[test]
fn the_lists_are_documented_with_their_page_schemas_and_parameters() {
    let registry = crate::app::registry();
    let doc: serde_json::Value =
        serde_json::from_str(&crate::registry::openapi_json(registry.defs(), "1")).expect("json");
    for (path, action, schema) in [
        (PROJECTS_PATH, "ProjectList", "ProjectPage"),
        (GROUPS_PATH, "GroupList", "GroupPage"),
    ] {
        let op = &doc["paths"][path]["get"];
        assert_eq!(op["x-forge-action"], action);
        assert_eq!(
            op["responses"]["200"]["content"]["application/json"]["schema"]["$ref"],
            format!("#/components/schemas/{schema}")
        );
        assert_eq!(op["parameters"][0]["name"], "limit");
        assert_eq!(op["parameters"][0]["in"], "query");
        assert_eq!(op["parameters"][0]["schema"]["maximum"], 100);
        assert_eq!(op["parameters"][1]["name"], "cursor");
        let page = &doc["components"]["schemas"][schema];
        assert_eq!(page["additionalProperties"], false);
        assert!(page["required"]
            .as_array()
            .expect("required")
            .iter()
            .any(|f| f == "next_cursor"));
    }
    assert!(doc["components"]["schemas"].get("ListedGroup").is_some());
}

fn busy_scope(err: &ForgeError) -> (ErrorCode, String, String) {
    let get = |k: &str| {
        err.context()
            .iter()
            .find(|(key, _)| *key == k)
            .map(|(_, v)| v.to_string())
            .unwrap_or_default()
    };
    (err.code(), get("reason"), get("scope"))
}

#[test]
fn the_per_user_cap_refuses_at_once_and_a_dropped_slot_is_returned() {
    let slots = ListSlots::new(8, 2);
    let (alice, bob) = (UserId::new(1), UserId::new(2));
    let a1 = slots.acquire(alice).expect("first");
    let a2 = slots.acquire(alice).expect("second");
    assert_eq!(slots.held(alice), 2);
    let err = slots.acquire(alice).expect_err("third");
    assert_eq!(
        busy_scope(&err),
        (
            ErrorCode::ServerBusy,
            "list_concurrency".into(),
            "principal".into()
        )
    );
    // Another user is not affected, and a refusal leaks no claim.
    let _b = slots.acquire(bob).expect("other user");
    assert_eq!(slots.held(alice), 2);
    drop(a1);
    let _a3 = slots.acquire(alice).expect("returned on drop");
    drop(a2);
    assert_eq!(slots.held(alice), 1);
}

#[test]
fn the_global_cap_refuses_at_once_and_returns_both_the_permit_and_the_claim() {
    let slots = ListSlots::new(2, 2);
    let (alice, bob, carol) = (UserId::new(1), UserId::new(2), UserId::new(3));
    let _a = slots.acquire(alice).expect("a");
    let b = slots.acquire(bob).expect("b");
    let err = slots.acquire(carol).expect_err("full");
    assert_eq!(
        busy_scope(&err),
        (
            ErrorCode::ServerBusy,
            "list_concurrency".into(),
            "global".into()
        )
    );
    // The refused caller's claim was returned (no leak toward her cap).
    assert_eq!(slots.held(carol), 0);
    drop(b);
    let _c = slots.acquire(carol).expect("permit returned on drop");
}

#[test]
fn an_expired_cursor_is_cursor_invalid_rule_expired() {
    let keys = CursorKeys::parse(&format!("1 {}\n", "ab".repeat(32))).expect("keys");
    let alice = UserId::new(42);
    let purpose = List::Projects.purpose(alice, &Filter::NONE);
    let old = keys
        .encode_at(&purpose, &5i64.to_be_bytes(), 1_000)
        .expect("seal");
    // Opened at the real clock the 1970 cursor is far past any max age.
    let err = open_cursor(&keys, List::Projects, alice, &Filter::NONE, Some(&old), AGE)
        .expect_err("expired");
    assert_eq!(
        rule(&err),
        (
            ErrorCode::RequestInvalid,
            String::new(),
            "expired".to_owned()
        )
    );
    let fresh = next_cursor(&keys, List::Projects, alice, &Filter::NONE, Some(5)).expect("seal");
    assert_eq!(
        open_cursor(
            &keys,
            List::Projects,
            alice,
            &Filter::NONE,
            fresh.as_deref(),
            AGE
        )
        .expect("fresh"),
        Some(5)
    );
}

/// M2 MR6: the filters, strictly; a filter the list does not have is an
/// unknown parameter.
#[test]
fn the_filters_are_parsed_strictly_per_list() {
    let f = parse_filters(
        Some("kind=spec&membership=true&state=pending_deletion&limit=5"),
        List::Projects,
    )
    .expect("all");
    assert_eq!(f.kind, Some(forge_core::RepositoryKind::Spec));
    assert!(f.membership && f.pending_deletion);
    assert_eq!(
        parse_filters(None, List::Projects).expect("none"),
        Filter::NONE
    );
    assert_eq!(
        parse_query(Some("kind=system&limit=3"))
            .expect("paging")
            .limit,
        3
    );
    let cases = [
        (List::Projects, "kind=other", "kind", "invalid_value"),
        (List::Projects, "kind=", "kind", "invalid_value"),
        (
            List::Projects,
            "membership=yes",
            "membership",
            "invalid_value",
        ),
        (List::Projects, "state=deleted", "state", "invalid_value"),
        (List::Projects, "kind=spec&kind=spec", "kind", "repeated"),
        (List::Groups, "kind=spec", "query", "unknown_parameter"),
        (
            List::Groups,
            "state=pending_deletion",
            "query",
            "unknown_parameter",
        ),
    ];
    for (list, query, param, expected) in cases {
        let err = parse_filters(Some(query), list).expect_err(query);
        assert_eq!(
            rule(&err),
            (
                ErrorCode::RequestInvalid,
                param.to_owned(),
                expected.to_owned()
            ),
            "{query}"
        );
    }
}

/// M2 MR6: a cursor is bound to its filters — under any other filter set
/// it fails the tag, and without filters the purpose is M1's.
#[test]
fn a_cursor_opens_only_under_its_own_filters() {
    let keys = keys();
    let alice = UserId::new(1);
    let spec = Filter {
        kind: Some(forge_core::RepositoryKind::Spec),
        ..Filter::NONE
    };
    let cursor = next_cursor(&keys, List::Projects, alice, &spec, Some(7))
        .expect("seal")
        .expect("a cursor");
    assert_eq!(
        open_cursor(&keys, List::Projects, alice, &spec, Some(&cursor), AGE).expect("same"),
        Some(7)
    );
    let others = [
        Filter::NONE,
        Filter {
            kind: Some(forge_core::RepositoryKind::System),
            ..Filter::NONE
        },
        Filter {
            membership: true,
            ..spec
        },
        Filter {
            pending_deletion: true,
            ..spec
        },
    ];
    for other in others {
        let err = open_cursor(&keys, List::Projects, alice, &other, Some(&cursor), AGE)
            .expect_err("another filter");
        assert!(
            err.context()
                .iter()
                .any(|(k, v)| *k == "rule" && v == "tag"),
            "{other:?}: {err}"
        );
    }
    assert_eq!(
        List::Projects.purpose(alice, &Filter::NONE),
        "projects.list:user=1"
    );
}
