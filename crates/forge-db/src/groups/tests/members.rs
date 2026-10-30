//! Direct memberships: add / change / remove, their audit rows, and the
//! last-owner rule under concurrency.

use forge_core::Role;

use super::*;

/// A top-level group `acme` owned by `alice`, plus `bob`.
async fn acme(t: &FreshDb) -> (CreatedGroup, UserId, UserId) {
    let alice = user(t, "alice").await;
    let bob = user(t, "bob").await;
    let acme = create_ok(t, &group("acme", None, alice)).await;
    (acme, alice, bob)
}

async fn role_of(su: &mut PgConnection, group: GroupId, user: UserId) -> Option<String> {
    sqlx::query_scalar("SELECT role FROM forge.group_members WHERE group_id = $1 AND user_id = $2")
        .bind(group.get())
        .bind(user.get())
        .fetch_optional(su)
        .await
        .expect("role")
}

#[tokio::test]
async fn member_changes_write_one_audit_row_each() {
    let t = FreshDb::migrated().await;
    let (acme, _, bob) = acme(&t).await;
    let g = acme.group_id;
    add_member(&t.db, &ctx("req-add"), g, bob, Role::Developer)
        .await
        .expect("add");
    let changed = change_member(
        &t.db,
        &ctx("req-change"),
        g,
        bob,
        Role::Developer,
        Role::Maintainer,
    )
    .await
    .expect("change");
    assert_eq!(
        changed,
        MemberChange::Changed {
            from: Role::Developer
        }
    );
    remove_member(&t.db, &ctx("req-remove"), g, bob, Role::Maintainer)
        .await
        .expect("remove");
    let mut su = t.superuser().await;
    for (request, action, details) in [
        (
            "req-add",
            "member.added",
            json!({"user_id": bob.get(), "role": "developer"}),
        ),
        (
            "req-change",
            "member.role_changed",
            json!({"user_id": bob.get(), "from": "developer", "to": "maintainer"}),
        ),
        (
            "req-remove",
            "member.removed",
            json!({"user_id": bob.get(), "role": "maintainer"}),
        ),
    ] {
        assert_eq!(
            audit_rows(&mut su, request).await,
            vec![(action.to_owned(), "group".to_owned(), g.get())],
            "{request}"
        );
        let stored: serde_json::Value =
            sqlx::query_scalar("SELECT details FROM forge.audit_log WHERE request_id = $1")
                .bind(request)
                .fetch_one(&mut su)
                .await
                .expect("details");
        assert_eq!(stored, details, "{request}");
    }
    assert_eq!(role_of(&mut su, g, bob).await, None);
    assert_eq!(count(&mut su, "SELECT count(*) FROM forge.events").await, 0);
    t.drop_now().await;
}

#[tokio::test]
async fn changing_to_the_same_role_writes_nothing() {
    let t = FreshDb::migrated().await;
    let (acme, _, bob) = acme(&t).await;
    add_member(&t.db, &ctx("req-add"), acme.group_id, bob, Role::Reporter)
        .await
        .expect("add");
    let same = change_member(
        &t.db,
        &ctx("req-same"),
        acme.group_id,
        bob,
        Role::Reporter,
        Role::Reporter,
    )
    .await
    .expect("same role");
    assert_eq!(same, MemberChange::Unchanged);
    let mut su = t.superuser().await;
    assert!(audit_rows(&mut su, "req-same").await.is_empty());
    t.drop_now().await;
}

#[tokio::test]
async fn adding_an_existing_member_is_membership_conflict() {
    let t = FreshDb::migrated().await;
    let (acme, alice, bob) = acme(&t).await;
    let g = acme.group_id;
    let err = add_member(&t.db, &ctx("req-again"), g, alice, Role::Guest)
        .await
        .expect_err("alice is the owner");
    assert_reason(&err, ErrorCode::MembershipConflict, "already_member");
    assert_eq!(err.code().http_status(), 409);
    let err = change_member(&t.db, &ctx("req-change"), g, bob, Role::Guest, Role::Guest)
        .await
        .expect_err("bob is no member");
    assert_reason(&err, ErrorCode::MembershipConflict, "not_member");
    let err = remove_member(&t.db, &ctx("req-remove"), g, bob, Role::Guest)
        .await
        .expect_err("bob is no member");
    assert_reason(&err, ErrorCode::MembershipConflict, "not_member");
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
    let err = add_member(&t.db, &ctx("req-bot-add"), g, bot, Role::Developer)
        .await
        .expect_err("bots are never members");
    assert_reason(&err, ErrorCode::MembershipConflict, "bot_member");
    let err = add_member(
        &t.db,
        &ctx("req-nogroup"),
        GroupId::new(9_999),
        bob,
        Role::Guest,
    )
    .await
    .expect_err("unknown group");
    assert_reason(&err, ErrorCode::NotFound, "absent");
    assert!(err.to_string().contains("resource=group"), "{err}");
    let mut su = t.superuser().await;
    for request in [
        "req-again",
        "req-change",
        "req-remove",
        "req-bot-add",
        "req-nogroup",
    ] {
        assert!(audit_rows(&mut su, request).await.is_empty(), "{request}");
    }
    t.drop_now().await;
}

#[tokio::test]
async fn the_last_owner_of_a_top_level_group_stays() {
    let t = FreshDb::migrated().await;
    let (acme, alice, bob) = acme(&t).await;
    let g = acme.group_id;
    let err = remove_member(&t.db, &ctx("req-rm"), g, alice, Role::Owner)
        .await
        .expect_err("alice is the only owner");
    assert_reason(&err, ErrorCode::MembershipConflict, "last_owner");
    let err = change_member(
        &t.db,
        &ctx("req-demote"),
        g,
        alice,
        Role::Owner,
        Role::Maintainer,
    )
    .await
    .expect_err("demoting the only owner");
    assert_reason(&err, ErrorCode::MembershipConflict, "last_owner");
    // A disabled second owner does not count.
    add_member(&t.db, &ctx("req-bob"), g, bob, Role::Owner)
        .await
        .expect("bob owner");
    let disabled = users::disable(&t.db, &ctx("req-disable-bob"), bob)
        .await
        .expect("disable bob");
    assert_eq!(disabled, users::DisableOutcome::Disabled);
    let err = remove_member(&t.db, &ctx("req-rm-2"), g, alice, Role::Owner)
        .await
        .expect_err("bob is disabled");
    assert_reason(&err, ErrorCode::MembershipConflict, "last_owner");
    // A subgroup relies on its ancestors' owners: its only direct owner may go.
    let team = create_ok(&t, &group("team", Some(acme.namespace_id), alice)).await;
    remove_member(&t.db, &ctx("req-rm-sub"), team.group_id, alice, Role::Owner)
        .await
        .expect("a subgroup may have no direct owner");
    t.drop_now().await;
}

/// Two owners remove each other at the same time: the group row lock makes
/// the second one see the first, so exactly one succeeds.
#[tokio::test]
async fn removing_the_last_owner_is_refused_under_concurrency() {
    let t = FreshDb::migrated().await;
    let (acme, alice, bob) = acme(&t).await;
    let g = acme.group_id;
    add_member(&t.db, &ctx("req-bob"), g, bob, Role::Owner)
        .await
        .expect("bob owner");
    let (ctx_a, ctx_b) = (ctx("req-rm-alice"), ctx("req-rm-bob"));
    let (a, b) = tokio::join!(
        remove_member(&t.db, &ctx_a, g, alice, Role::Owner),
        remove_member(&t.db, &ctx_b, g, bob, Role::Owner),
    );
    let refused: Vec<_> = [a, b].into_iter().filter_map(Result::err).collect();
    assert_eq!(
        refused.len(),
        1,
        "exactly one removal is refused: {refused:?}"
    );
    assert_reason(&refused[0], ErrorCode::MembershipConflict, "last_owner");
    let mut su = t.superuser().await;
    let owners = count(
        &mut su,
        &format!(
            "SELECT count(*) FROM forge.group_members WHERE group_id = {} AND role = 'owner'",
            g.get()
        ),
    )
    .await;
    assert_eq!(owners, 1, "one owner remains");
    t.drop_now().await;
}

/// Wait (bounded) until a backend of this test's database waits on a lock.
async fn until_someone_waits(t: &FreshDb) {
    let mut su = t.superuser().await;
    for _ in 0..500 {
        let waiting = count(
            &mut su,
            "SELECT count(*) FROM pg_stat_activity WHERE wait_event_type = 'Lock' \
             AND datname = current_database()",
        )
        .await;
        if waiting > 0 {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("nobody waited on a lock within 10 s");
}

/// The other Owner is being disabled (uncommitted) while an Owner is
/// removed: the removal waits for that user row and then no longer counts
/// them, so it is refused instead of leaving no active Owner.
#[tokio::test]
async fn removing_an_owner_while_the_other_is_disabled_keeps_an_owner() {
    let t = FreshDb::migrated().await;
    let (acme, alice, bob) = acme(&t).await;
    let g = acme.group_id;
    add_member(&t.db, &ctx("req-bob"), g, bob, Role::Owner)
        .await
        .expect("bob owner");
    let mut disabling = t.db.write_pool().begin().await.expect("disabling");
    sqlx::raw_sql(&format!(
        "{}; UPDATE forge.users SET state = 'disabled' WHERE id = {}",
        audit("user", bob.get(), "succeeded"),
        bob.get()
    ))
    .execute(&mut *disabling)
    .await
    .expect("disable bob, uncommitted");
    let rm_ctx = ctx("req-rm-alice");
    let removing = remove_member(&t.db, &rm_ctx, g, alice, Role::Owner);
    let committing = async {
        until_someone_waits(&t).await;
        disabling.commit().await.expect("bob is disabled");
    };
    let (removed, ()) = tokio::join!(removing, committing);
    let err = removed.expect_err("bob no longer counts");
    assert_reason(&err, ErrorCode::MembershipConflict, "last_owner");
    let mut su = t.superuser().await;
    assert_eq!(role_of(&mut su, g, alice).await.as_deref(), Some("owner"));
    t.drop_now().await;
}

#[tokio::test]
async fn a_disabled_user_can_be_added_as_a_member() {
    let t = FreshDb::migrated().await;
    let (acme, _, bob) = acme(&t).await;
    let disabled = users::disable(&t.db, &ctx("req-disable-bob"), bob)
        .await
        .expect("disable bob");
    assert_eq!(disabled, users::DisableOutcome::Disabled);
    add_member(&t.db, &ctx("req-add"), acme.group_id, bob, Role::Developer)
        .await
        .expect("a disabled user may be added");
    let mut su = t.superuser().await;
    assert_eq!(
        role_of(&mut su, acme.group_id, bob).await.as_deref(),
        Some("developer")
    );
    t.drop_now().await;
}

#[tokio::test]
async fn an_unknown_group_or_user_is_not_found() {
    let t = FreshDb::migrated().await;
    let (acme, _, bob) = acme(&t).await;
    let nobody = UserId::new(9_999);
    let nogroup = GroupId::new(9_999);
    let err = add_member(&t.db, &ctx("req-1"), acme.group_id, nobody, Role::Guest)
        .await
        .expect_err("unknown user");
    assert_reason(&err, ErrorCode::NotFound, "absent");
    assert!(err.to_string().contains("resource=user"), "{err}");
    let err = change_member(&t.db, &ctx("req-2"), nogroup, bob, Role::Guest, Role::Guest)
        .await
        .expect_err("unknown group");
    assert!(err.to_string().contains("resource=group"), "{err}");
    let err = remove_member(&t.db, &ctx("req-3"), nogroup, bob, Role::Guest)
        .await
        .expect_err("unknown group");
    assert!(err.to_string().contains("resource=group"), "{err}");
    t.drop_now().await;
}

/// One Owner is demoted while the other is removed: both take the group
/// lock, so exactly one of them is refused and one Owner remains.
#[tokio::test]
async fn demoting_and_removing_the_two_owners_at_once_keeps_one() {
    let t = FreshDb::migrated().await;
    let (acme, alice, bob) = acme(&t).await;
    let g = acme.group_id;
    add_member(&t.db, &ctx("req-bob"), g, bob, Role::Owner)
        .await
        .expect("bob owner");
    let (ctx_a, ctx_b) = (ctx("req-demote-alice"), ctx("req-rm-bob"));
    let (demoted, removed) = tokio::join!(
        change_member(&t.db, &ctx_a, g, alice, Role::Owner, Role::Developer),
        remove_member(&t.db, &ctx_b, g, bob, Role::Owner),
    );
    let refused: Vec<_> = [demoted.err(), removed.err()]
        .into_iter()
        .flatten()
        .collect();
    assert_eq!(refused.len(), 1, "exactly one is refused: {refused:?}");
    assert_reason(&refused[0], ErrorCode::MembershipConflict, "last_owner");
    assert!(refused[0]
        .to_string()
        .contains(&format!("group_id={}", g.get())));
    let mut su = t.superuser().await;
    let owners = count(
        &mut su,
        &format!(
            "SELECT count(*) FROM forge.group_members WHERE group_id = {} AND role = 'owner'",
            g.get()
        ),
    )
    .await;
    assert_eq!(owners, 1, "one owner remains");
    t.drop_now().await;
}

#[tokio::test]
async fn an_unknown_member_is_not_a_member_and_an_unknown_group_is_not_found() {
    let t = FreshDb::migrated().await;
    let (acme, _, bob) = acme(&t).await;
    let nobody = UserId::new(9_999);
    for err in [
        change_member(
            &t.db,
            &ctx("req-1"),
            acme.group_id,
            nobody,
            Role::Guest,
            Role::Guest,
        )
        .await
        .expect_err("unknown user"),
        remove_member(&t.db, &ctx("req-2"), acme.group_id, nobody, Role::Guest)
            .await
            .expect_err("unknown user"),
    ] {
        assert_reason(&err, ErrorCode::MembershipConflict, "not_member");
    }
    let err = add_member(&t.db, &ctx("req-3"), GroupId::new(9_999), bob, Role::Guest)
        .await
        .expect_err("unknown group");
    assert_reason(&err, ErrorCode::NotFound, "absent");
    assert!(err.to_string().contains("resource=group"), "{err}");
    t.drop_now().await;
}

/// A stored role outside the five (changed outside forge-db) is
/// `FG-DB-0018` `unknown_role`, the role loader's code, not `FG-SYS-0001`
/// (ADR-0019 §15 item 2).
#[tokio::test]
async fn an_unknown_stored_role_is_membership_data_invalid() {
    let t = FreshDb::migrated().await;
    let (acme, _, bob) = acme(&t).await;
    let g = acme.group_id;
    add_member(&t.db, &ctx("req-add"), g, bob, Role::Developer)
        .await
        .expect("add");
    // The constraint and the audit triggers are dropped / off for the tamper
    // only (this database is dropped).
    t.exec_superuser(&format!(
        "ALTER TABLE forge.group_members DROP CONSTRAINT group_members_role; \
         ALTER TABLE forge.group_members DISABLE TRIGGER USER; \
         UPDATE forge.group_members SET role = 'superuser' WHERE group_id = {} AND user_id = {}; \
         ALTER TABLE forge.group_members ENABLE TRIGGER USER",
        g.get(),
        bob.get()
    ))
    .await;
    for err in [
        change_member(
            &t.db,
            &ctx("req-change"),
            g,
            bob,
            Role::Developer,
            Role::Guest,
        )
        .await
        .expect_err("change"),
        remove_member(&t.db, &ctx("req-remove"), g, bob, Role::Developer)
            .await
            .expect_err("remove"),
    ] {
        assert_reason(&err, ErrorCode::MembershipDataInvalid, "unknown_role");
        let text = err.to_string();
        for part in [
            "resource=group".to_owned(),
            format!("id={}", g.get()),
            format!("user={}", bob.get()),
            "raw=superuser".to_owned(),
        ] {
            assert!(text.contains(&part), "missing {part:?} in {text}");
        }
    }
    t.drop_now().await;
}

/// The role a caller authorized against is a precondition (D4 review): a
/// promotion to Owner committed between `authorize()` (which saw
/// Developer) and the demotion or removal makes both `role_changed`, and
/// nothing is written.
#[tokio::test]
async fn a_role_changed_after_authorization_is_role_changed() {
    let t = FreshDb::migrated().await;
    let (acme, _, bob) = acme(&t).await;
    let g = acme.group_id;
    add_member(&t.db, &ctx("req-add"), g, bob, Role::Developer)
        .await
        .expect("add");
    // The caller loads bob's role (Developer) and authorizes; meanwhile an
    // Owner promotes bob.
    let authorized = Role::Developer;
    let _ = change_member(
        &t.db,
        &ctx("req-promote"),
        g,
        bob,
        Role::Developer,
        Role::Owner,
    )
    .await
    .expect("promote");
    for err in [
        change_member(&t.db, &ctx("req-demote"), g, bob, authorized, Role::Guest)
            .await
            .expect_err("demote"),
        remove_member(&t.db, &ctx("req-remove"), g, bob, authorized)
            .await
            .expect_err("remove"),
    ] {
        assert_reason(&err, ErrorCode::MembershipConflict, "role_changed");
        let text = err.to_string();
        assert!(text.contains("expected=developer"), "{text}");
        assert!(text.contains("actual=owner"), "{text}");
    }
    let mut su = t.superuser().await;
    assert_eq!(role_of(&mut su, g, bob).await.as_deref(), Some("owner"));
    for request in ["req-demote", "req-remove"] {
        assert!(audit_rows(&mut su, request).await.is_empty(), "{request}");
    }
    t.drop_now().await;
}
