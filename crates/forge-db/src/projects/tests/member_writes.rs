//! M1 phase 8b MR8: member changes decided inside their transaction —
//! `projects::{add,change,remove}_member_decided` and
//! `groups::{add,change,remove}_member_decided` — and what they hand to
//! `decide`: the caller's re-read role, the member's direct role read under
//! the lock (the decision's `from`) and the last-owner fact.

use forge_core::Role;

use super::*;
use crate::groups::GroupMemberFacts;

fn deny() -> ForgeError {
    ForgeError::new(ErrorCode::Forbidden).with_context("reason", "test_denied")
}

async fn rows(su: &mut PgConnection, sql: &str) -> i64 {
    count(su, sql).await
}

#[tokio::test]
async fn project_member_commands_decide_on_the_locked_facts() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let bob = user(&t, "bob").await;
    let carol = user(&t, "carol").await;
    let acme = group(&t, "acme", None, alice.user_id).await;
    groups::add_member(
        &t.db,
        &ctx("req-bob"),
        acme.group_id,
        bob.user_id,
        Role::Maintainer,
    )
    .await
    .expect("bob maintains acme");
    let web = create_ok(&t, &project("web", acme.namespace_id, alice.user_id)).await;
    let p = web.project_id;

    // add: bob (inherited Maintainer) adds carol; decide sees no role yet.
    let seen = add_member_decided(
        &t.db,
        &ctx("req-add"),
        p,
        bob.user_id,
        carol.user_id,
        Role::Developer,
        |f: &ProjectMemberFacts| Ok(*f),
    )
    .await
    .expect("add");
    assert_eq!(seen.project.access.role, Some(Role::Maintainer));
    assert_eq!(seen.member_role, None);
    assert!(!seen.removes_last_owner);
    assert!(seen.account.active);

    // change: decide sees carol's stored role, never the request's.
    let (changed, seen) = change_member_decided(
        &t.db,
        &ctx("req-change"),
        p,
        bob.user_id,
        carol.user_id,
        Role::Reporter,
        |f| Ok(*f),
    )
    .await
    .expect("change");
    assert_eq!(
        changed,
        MemberChange::Changed {
            from: Role::Developer
        }
    );
    assert_eq!(seen.member_role, Some(Role::Developer));
    let (again, _) = change_member_decided(
        &t.db,
        &ctx("req-again"),
        p,
        bob.user_id,
        carol.user_id,
        Role::Reporter,
        |_| Ok(()),
    )
    .await
    .expect("unchanged");
    assert_eq!(again, MemberChange::Unchanged);

    // a refusing decision writes nothing.
    let err = remove_member_decided(
        &t.db,
        &ctx("req-denied"),
        p,
        bob.user_id,
        carol.user_id,
        |_| Err::<(), _>(deny()),
    )
    .await
    .expect_err("denied");
    assert_eq!(err.code(), ErrorCode::Forbidden);

    remove_member_decided(
        &t.db,
        &ctx("req-remove"),
        p,
        bob.user_id,
        carol.user_id,
        |f| {
            assert_eq!(f.member_role, Some(Role::Reporter));
            Ok(())
        },
    )
    .await
    .expect("remove");

    // conflicts come after the decision.
    let err = remove_member_decided(
        &t.db,
        &ctx("req-not-member"),
        p,
        bob.user_id,
        carol.user_id,
        |f| {
            assert_eq!(f.member_role, None);
            Ok(())
        },
    )
    .await
    .expect_err("not a member");
    assert_reason(&err, ErrorCode::MembershipConflict, "not_member");
    let err = add_member_decided(
        &t.db,
        &ctx("req-already"),
        p,
        bob.user_id,
        alice.user_id,
        Role::Guest,
        |f| {
            assert_eq!(f.member_role, Some(Role::Owner));
            Ok(())
        },
    )
    .await
    .expect_err("already a member");
    assert_reason(&err, ErrorCode::MembershipConflict, "already_member");

    let mut su = t.superuser().await;
    // add, change, remove: one audit row and one event each; nothing else.
    for (action, n) in [
        ("member.added", 1),
        ("member.role_changed", 1),
        ("member.removed", 1),
    ] {
        let sql = format!(
            "SELECT count(*) FROM forge.audit_log WHERE action = '{action}' \
             AND resource_type = 'project'"
        );
        assert_eq!(rows(&mut su, &sql).await, n, "{action}");
        let sql = format!(
            "SELECT count(*) FROM forge.events WHERE event_type = '{action}' \
             AND project_id = {}",
            p.get()
        );
        assert_eq!(rows(&mut su, &sql).await, n, "{action}");
    }
    t.drop_now().await;
}

#[tokio::test]
async fn the_last_owner_fact_reaches_decide_and_is_a_backstop() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let bob = user(&t, "bob").await;
    // bob owns alice/web directly; alice (the inherited Owner) is disabled.
    let web = create_ok(&t, &project("web", alice.namespace_id, bob.user_id)).await;
    let disabled = users::disable(&t.db, &ctx("req-disable-alice"), alice.user_id)
        .await
        .expect("bob remains");
    assert_eq!(disabled, users::DisableOutcome::Disabled);
    let p = web.project_id;
    let (seen, err) = {
        let mut seen = None;
        let err = remove_member_decided(&t.db, &ctx("req-rm"), p, bob.user_id, bob.user_id, |f| {
            seen = Some(*f);
            Ok(())
        })
        .await
        .expect_err("last owner");
        (seen.expect("decided"), err)
    };
    assert!(seen.removes_last_owner);
    assert_eq!(seen.member_role, Some(Role::Owner));
    // decide allowed (a buggy closure): the command still refuses.
    assert_reason(&err, ErrorCode::MembershipConflict, "last_owner");
    let mut seen = None;
    let err = change_member_decided(
        &t.db,
        &ctx("req-demote"),
        p,
        bob.user_id,
        bob.user_id,
        Role::Maintainer,
        |f| {
            seen = Some(*f);
            Ok(())
        },
    )
    .await
    .expect_err("last owner");
    assert!(seen.expect("decided").removes_last_owner);
    assert_reason(&err, ErrorCode::MembershipConflict, "last_owner");
    // Owner to Owner takes nothing away.
    let (unchanged, seen) = change_member_decided(
        &t.db,
        &ctx("req-same"),
        p,
        bob.user_id,
        bob.user_id,
        Role::Owner,
        |f| Ok(*f),
    )
    .await
    .expect("unchanged");
    assert_eq!(unchanged, MemberChange::Unchanged);
    assert!(!seen.removes_last_owner);
    let mut su = t.superuser().await;
    assert_eq!(
        rows(
            &mut su,
            "SELECT count(*) FROM forge.project_members WHERE role = 'owner'"
        )
        .await,
        1
    );
    t.drop_now().await;
}

#[tokio::test]
async fn group_member_commands_decide_on_the_locked_facts() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let bob = user(&t, "bob").await;
    let acme = group(&t, "acme", None, alice.user_id).await;
    let api = group(&t, "api", Some(acme.namespace_id), alice.user_id).await;
    let g = acme.group_id;
    groups::add_member_decided(
        &t.db,
        &ctx("req-add"),
        g,
        alice.user_id,
        bob.user_id,
        Role::Developer,
        |f: &GroupMemberFacts| {
            assert_eq!(
                (f.group, f.role, f.member_role),
                (g, Some(Role::Owner), None)
            );
            Ok(())
        },
    )
    .await
    .expect("add");
    let (changed, ()) = groups::change_member_decided(
        &t.db,
        &ctx("req-change"),
        g,
        alice.user_id,
        bob.user_id,
        Role::Maintainer,
        |f| {
            assert_eq!(f.member_role, Some(Role::Developer));
            Ok(())
        },
    )
    .await
    .expect("change");
    assert_eq!(
        changed,
        MemberChange::Changed {
            from: Role::Developer
        }
    );
    // alice is acme's only direct Owner: a top-level group keeps one.
    let mut seen = None;
    let err = groups::remove_member_decided(
        &t.db,
        &ctx("req-rm-alice"),
        g,
        alice.user_id,
        alice.user_id,
        |f| {
            seen = Some(*f);
            Ok(())
        },
    )
    .await
    .expect_err("last owner");
    assert!(seen.expect("decided").removes_last_owner);
    assert_reason(&err, ErrorCode::MembershipConflict, "last_owner");
    // A subgroup may be left to its ancestors' Owners.
    groups::remove_member_decided(
        &t.db,
        &ctx("req-rm-api"),
        api.group_id,
        alice.user_id,
        alice.user_id,
        |f| {
            assert!(!f.removes_last_owner);
            assert_eq!(f.role, Some(Role::Owner));
            Ok(())
        },
    )
    .await
    .expect("subgroup");
    let err = groups::remove_member_decided(
        &t.db,
        &ctx("req-denied"),
        g,
        alice.user_id,
        bob.user_id,
        |_| Err::<(), _>(deny()),
    )
    .await
    .expect_err("denied");
    assert_eq!(err.code(), ErrorCode::Forbidden);
    let mut su = t.superuser().await;
    assert_eq!(
        rows(
            &mut su,
            "SELECT count(*) FROM forge.audit_log WHERE action LIKE 'member.%' \
             AND resource_type = 'group' AND request_id IN ('req-add', 'req-change', 'req-rm-api')"
        )
        .await,
        3
    );
    // Group member changes stay audit-only.
    assert_eq!(
        rows(
            &mut su,
            "SELECT count(*) FROM forge.events WHERE event_type LIKE 'member.%'"
        )
        .await,
        0
    );
    t.drop_now().await;
}

#[tokio::test]
async fn member_pages_list_direct_members_in_user_id_order() {
    use crate::members::{direct_role, one, page, MemberScope};
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let bob = user(&t, "bob").await;
    let carol = user(&t, "carol").await;
    let web = create_ok(&t, &project("web", alice.namespace_id, alice.user_id)).await;
    let scope = MemberScope::Project(web.project_id);
    for (who, role) in [(bob.user_id, Role::Developer), (carol.user_id, Role::Guest)] {
        add_member(&t.db, &ctx("req-add"), web.project_id, who, role)
            .await
            .expect("add");
    }
    let first = page(&t.db, scope, None, 2).await.expect("page");
    let ids: Vec<_> = first.iter().map(|m| (m.user_id, m.role)).collect();
    assert_eq!(
        ids,
        [(alice.user_id, Role::Owner), (bob.user_id, Role::Developer)]
    );
    assert_eq!(first[0].username, "alice");
    assert!(first[0].active && first[0].created_at.ends_with('Z'));
    let rest = page(&t.db, scope, Some(bob.user_id), 100)
        .await
        .expect("rest");
    assert_eq!(rest.len(), 1);
    assert_eq!(rest[0].user_id, carol.user_id);
    assert_eq!(
        direct_role(&t.db, scope, bob.user_id).await.expect("role"),
        Some(Role::Developer)
    );
    let acme = group(&t, "acme", None, alice.user_id).await;
    let gscope = MemberScope::Group(acme.group_id);
    assert_eq!(
        direct_role(&t.db, gscope, bob.user_id).await.expect("role"),
        None
    );
    assert_eq!(
        page(&t.db, gscope, None, 0).await.expect("clamped").len(),
        1
    );
    let bob_row = one(&t.db, scope, bob.user_id).await.expect("one");
    assert_eq!(bob_row.map(|m| m.role), Some(Role::Developer));
    assert_eq!(one(&t.db, gscope, bob.user_id).await.expect("one"), None);
    t.drop_now().await;
}
