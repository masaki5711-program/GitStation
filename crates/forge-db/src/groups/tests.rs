//! The group commands against a migrated database. Inspection uses a
//! superuser connection.

use forge_core::{RequestId, UserKind};
use forge_errors::ErrorCode;
use serde_json::json;
use sqlx::postgres::PgConnection;

use super::*;
use crate::audited_tests::{audit, commit};
use crate::change::Actor;
use crate::test_support::*;
use crate::users::{self, NewUser};

mod members;

fn ctx(request: &str) -> ChangeContext {
    ChangeContext::new(
        Actor::System,
        RequestId::parse(request).expect("request id"),
    )
}

async fn count(su: &mut PgConnection, sql: &str) -> i64 {
    sqlx::query_scalar(sql)
        .fetch_one(su)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
}

async fn audit_rows(su: &mut PgConnection, request: &str) -> Vec<(String, String, i64)> {
    sqlx::query_as(
        "SELECT action, resource_type, resource_id FROM forge.audit_log \
         WHERE request_id = $1 ORDER BY id",
    )
    .bind(request)
    .fetch_all(su)
    .await
    .expect("audit rows")
}

/// An active human named `name`.
async fn user(t: &FreshDb, name: &str) -> UserId {
    users::create(
        &t.db,
        &ctx(&format!("req-user-{name}")),
        &NewUser::human(name, name, &format!("{name}@example.test")),
    )
    .await
    .expect("user")
    .user_id
}

fn group(segment: &str, parent: Option<NamespaceId>, owner: UserId) -> NewGroup {
    NewGroup {
        segment: segment.to_owned(),
        parent,
        display_name: format!("Group {segment}"),
        description: String::new(),
        owner,
    }
}

async fn create_ok(t: &FreshDb, new: &NewGroup) -> CreatedGroup {
    create(
        &t.db,
        &ctx(&format!(
            "req-group-{}",
            new.segment.chars().take(24).collect::<String>()
        )),
        new,
    )
    .await
    .unwrap_or_else(|e| panic!("{}: {e}", new.segment))
}

#[tokio::test]
async fn group_create_writes_group_namespace_owner_and_one_audit_row() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let created = create(&t.db, &ctx("req-acme"), &group("acme", None, alice))
        .await
        .expect("create acme");
    let mut su = t.superuser().await;
    let ns: (String, String, String, i64, i64, bool) = sqlx::query_as(
        "SELECT kind, segment, full_path, depth::bigint, owner_id, parent_id IS NULL \
         FROM forge.namespaces WHERE id = $1",
    )
    .bind(created.namespace_id.get())
    .fetch_one(&mut su)
    .await
    .expect("namespace");
    assert_eq!(
        ns,
        (
            "group".into(),
            "acme".into(),
            "acme".into(),
            1,
            created.group_id.get(),
            true
        )
    );
    let owner: String = sqlx::query_scalar(
        "SELECT role FROM forge.group_members WHERE group_id = $1 AND user_id = $2",
    )
    .bind(created.group_id.get())
    .bind(alice.get())
    .fetch_one(&mut su)
    .await
    .expect("owner row");
    assert_eq!(owner, "owner");
    assert_eq!(
        audit_rows(&mut su, "req-acme").await,
        vec![(
            "group.created".to_owned(),
            "group".to_owned(),
            created.group_id.get()
        )]
    );
    let details: serde_json::Value =
        sqlx::query_scalar("SELECT details FROM forge.audit_log WHERE request_id = 'req-acme'")
            .fetch_one(&mut su)
            .await
            .expect("details");
    assert_eq!(
        details,
        json!({"namespace_id": created.namespace_id.get(), "parent_namespace_id": null,
               "owner_id": alice.get()})
    );
    assert_eq!(count(&mut su, "SELECT count(*) FROM forge.events").await, 0);
    t.drop_now().await;
}

#[tokio::test]
async fn a_subgroup_is_written_below_its_parent_and_may_use_a_reserved_word() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let acme = create_ok(&t, &group("acme", None, alice)).await;
    let api = create_ok(&t, &group("api", Some(acme.namespace_id), alice)).await;
    let mut su = t.superuser().await;
    let row: (String, i64, i64) = sqlx::query_as(
        "SELECT full_path, depth::bigint, parent_id FROM forge.namespaces WHERE id = $1",
    )
    .bind(api.namespace_id.get())
    .fetch_one(&mut su)
    .await
    .expect("subgroup");
    assert_eq!(row, ("acme/api".into(), 2, acme.namespace_id.get()));
    // A user and a group below a group are not siblings at the top level.
    create_ok(&t, &group("alice", Some(acme.namespace_id), alice)).await;
    t.drop_now().await;
}

#[tokio::test]
async fn a_subgroup_under_a_user_namespace_is_rejected() {
    let t = FreshDb::migrated().await;
    let created = users::create(
        &t.db,
        &ctx("req-alice"),
        &NewUser::human("alice", "Alice", "alice@example.test"),
    )
    .await
    .expect("alice");
    let err = create(
        &t.db,
        &ctx("req-under-user"),
        &group("team", Some(created.namespace_id), created.user_id),
    )
    .await
    .expect_err("groups nest only under groups");
    assert_reason(&err, ErrorCode::GroupRequestInvalid, "parent_not_group");
    assert_eq!(err.code().http_status(), 400);
    let mut su = t.superuser().await;
    assert_eq!(count(&mut su, "SELECT count(*) FROM forge.groups").await, 0);
    t.drop_now().await;
}

#[tokio::test]
async fn group_paths_obey_depth_and_length_limits() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    // 20 levels are allowed, the 21st is too deep.
    let mut parent = None;
    for level in 1..=20 {
        let created = create_ok(&t, &group(&format!("l{level}"), parent, alice)).await;
        parent = Some(created.namespace_id);
    }
    let err = create(&t.db, &ctx("req-l21"), &group("l21", parent, alice))
        .await
        .expect_err("depth 21");
    assert_reason(&err, ErrorCode::NamespacePathInvalid, "too_deep");
    // A full path over 255 characters.
    let long = "a".repeat(64);
    let mut parent = None;
    for level in 0..3 {
        let created = create_ok(&t, &group(&format!("{long}{level}")[1..], parent, alice)).await;
        parent = Some(created.namespace_id);
    }
    // 3 × 64 + 2 slashes = 194; a fourth 64-character segment makes 259.
    let err = create(&t.db, &ctx("req-long"), &group(&long, parent, alice))
        .await
        .expect_err("path over 255");
    assert_reason(&err, ErrorCode::NamespacePathInvalid, "path_too_long");
    assert!(err.to_string().contains("len=259"), "{err}");
    t.drop_now().await;
}

#[tokio::test]
async fn an_invalid_group_request_is_group_request_invalid() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let cases = [
        (
            NewGroup {
                display_name: String::new(),
                ..group("g1", None, alice)
            },
            "display_name_length",
        ),
        (
            NewGroup {
                display_name: "x".repeat(256),
                ..group("g2", None, alice)
            },
            "display_name_length",
        ),
        (
            NewGroup {
                description: "x".repeat(2001),
                ..group("g3", None, alice)
            },
            "description_length",
        ),
        (
            NewGroup {
                display_name: "a\0b".into(),
                ..group("g4", None, alice)
            },
            "nul_character",
        ),
    ];
    for (new, reason) in cases {
        let err = create(&t.db, &ctx(&format!("req-{}", new.segment)), &new)
            .await
            .expect_err(reason);
        assert_reason(&err, ErrorCode::GroupRequestInvalid, reason);
    }
    // An inactive owner.
    let disabled = users::disable(&t.db, &ctx("req-disable"), alice)
        .await
        .expect("disable");
    assert_eq!(disabled, users::DisableOutcome::Disabled);
    let err = create(&t.db, &ctx("req-g5"), &group("g5", None, alice))
        .await
        .expect_err("inactive owner");
    assert_reason(&err, ErrorCode::GroupRequestInvalid, "owner_inactive");
    // A bot cannot own (or be a member of) a group.
    let bot = users::create(
        &t.db,
        &ctx("req-bot"),
        &NewUser {
            kind: UserKind::Bot,
            email: None,
            ..NewUser::human("ci-bot", "CI", "unused@example.test")
        },
    )
    .await
    .expect("bot")
    .user_id;
    let err = create(&t.db, &ctx("req-g6"), &group("g6", None, bot))
        .await
        .expect_err("bot owner");
    assert_reason(&err, ErrorCode::MembershipConflict, "bot_member");
    // Unknown parent and owner.
    let err = create(
        &t.db,
        &ctx("req-g7"),
        &group("g7", Some(NamespaceId::new(9_999)), bot),
    )
    .await
    .expect_err("unknown parent");
    assert_reason(&err, ErrorCode::NotFound, "absent");
    assert!(err.to_string().contains("resource=namespace"), "{err}");
    let err = create(
        &t.db,
        &ctx("req-g8"),
        &group("g8", None, UserId::new(9_999)),
    )
    .await
    .expect_err("unknown owner");
    assert!(err.to_string().contains("resource=user"), "{err}");
    let mut su = t.superuser().await;
    assert_eq!(count(&mut su, "SELECT count(*) FROM forge.groups").await, 0);
    t.drop_now().await;
}

#[tokio::test]
async fn a_taken_or_tombstoned_or_reserved_group_path_is_rejected() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    commit(
        &t,
        &[
            audit("group", 999, "succeeded"),
            "INSERT INTO forge.namespace_tombstones (old_path, kind, cause_type, cause_id) \
             VALUES ('ghost', 'deleted', 'group', 999)"
                .to_owned(),
        ],
    )
    .await
    .expect("seed tombstone");
    let cases = [
        ("alice", ErrorCode::NamespacePathTaken, Some("sibling")),
        ("ghost", ErrorCode::NamespacePathTaken, Some("tombstoned")),
        ("api", ErrorCode::NamespacePathReserved, Some("reserved")),
    ];
    for (name, code, reason) in cases {
        let err = create(
            &t.db,
            &ctx(&format!("req-{name}")),
            &group(name, None, alice),
        )
        .await
        .expect_err(name);
        if let Some(reason) = reason {
            assert_reason(&err, code, reason);
        }
        assert!(!chain(&err).contains(name), "{name}: {}", chain(&err));
    }
    let mut su = t.superuser().await;
    assert_eq!(count(&mut su, "SELECT count(*) FROM forge.groups").await, 0);
    t.drop_now().await;
}

#[tokio::test]
async fn a_duplicate_subgroup_under_one_parent_is_path_taken() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let acme = create_ok(&t, &group("acme", None, alice)).await;
    let first = create_ok(&t, &group("team", Some(acme.namespace_id), alice)).await;
    let err = create(
        &t.db,
        &ctx("req-team-again"),
        &group("team", Some(acme.namespace_id), alice),
    )
    .await
    .expect_err("acme/team exists");
    assert_reason(&err, ErrorCode::NamespacePathTaken, "sibling");
    let mut su = t.superuser().await;
    assert!(audit_rows(&mut su, "req-team-again").await.is_empty());
    let details: serde_json::Value = sqlx::query_scalar(
        "SELECT details FROM forge.audit_log WHERE action = 'group.created' AND resource_id = $1",
    )
    .bind(first.group_id.get())
    .fetch_one(&mut su)
    .await
    .expect("subgroup audit details");
    assert_eq!(
        details,
        json!({"namespace_id": first.namespace_id.get(),
               "parent_namespace_id": acme.namespace_id.get(), "owner_id": alice.get()})
    );
    t.drop_now().await;
}

/// Two creates of one path at once: the path lock serializes them, one wins.
#[tokio::test]
async fn two_concurrent_creates_of_one_path_make_one_group() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let (ctx_a, ctx_b) = (ctx("req-a"), ctx("req-b"));
    let (new_a, new_b) = (group("acme", None, alice), group("acme", None, alice));
    let (a, b) = tokio::join!(create(&t.db, &ctx_a, &new_a), create(&t.db, &ctx_b, &new_b));
    let errs: Vec<_> = [a, b].into_iter().filter_map(Result::err).collect();
    assert_eq!(errs.len(), 1, "exactly one create fails: {errs:?}");
    assert_reason(&errs[0], ErrorCode::NamespacePathTaken, "sibling");
    let mut su = t.superuser().await;
    assert_eq!(count(&mut su, "SELECT count(*) FROM forge.groups").await, 1);
    t.drop_now().await;
}
