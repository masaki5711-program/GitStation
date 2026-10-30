//! What `forge doctor` `users.administration` reads (D3 MR2, ADR-0019 §10):
//! no active human administrator, or a top-level group without a direct
//! active Owner, is `FG-AUTH-0010` (warn).

use forge_core::Role;
use forge_errors::Severity;

use super::guard::{group_under, top_level};
use super::*;
use crate::groups;

async fn read(t: &FreshDb) -> Administration {
    administration(&t.db)
        .await
        .expect("administration")
        .expect("migrated")
}

#[tokio::test]
async fn administration_orphaned_is_reported_for_no_admin_and_an_ownerless_group() {
    let t = FreshDb::migrated().await;
    // A fresh install: no users yet, nothing orphaned, but no admin either.
    let fresh = read(&t).await;
    assert!(fresh.is_fresh_install(), "{fresh:?}");
    assert_eq!(fresh.active_human_admins, 0);

    let alice = account(&t, "alice", UserKind::Human, true).await;
    let bob = account(&t, "bob", UserKind::Human, false).await;
    let acme = top_level(&t, "acme", alice).await;
    // A subgroup whose only direct owner leaves inherits acme's owners.
    let team = group_under(&t, "team", Some(&acme), alice).await;
    groups::add_member(&t.db, &ctx("req-add-bob"), team.group_id, bob, Role::Owner)
        .await
        .expect("bob owner of team");
    groups::remove_member(&t.db, &ctx("req-rm-alice"), team.group_id, alice)
        .await
        .expect("alice leaves team");
    let healthy = read(&t).await;
    assert!(!healthy.is_fresh_install());
    assert_eq!(
        (healthy.active_human_admins, healthy.ownerless_groups),
        (1, 0)
    );
    healthy
        .check()
        .expect("an admin and an owned top-level group");

    // A forced disable of alice orphans both.
    let _ = disable_with(&t.db, &ctx("req-force"), alice, Guard::Force)
        .await
        .expect("forced");
    let orphaned = read(&t).await;
    assert_eq!(orphaned.ownerless_group_ids, vec![acme.group_id]);
    let err = orphaned.check().expect_err("no admin, acme has no owner");
    assert_reason(&err, ErrorCode::AdministrationOrphaned, "no_admin");
    assert_eq!(err.code().severity(), Severity::Warn);
    let text = err.to_string();
    for part in [
        "active_human_admins=0",
        "ownerless_groups=1",
        &format!("group_ids={}", acme.group_id.get()),
    ] {
        assert!(text.contains(part), "missing {part:?} in {text}");
    }

    // Recovery: bob becomes the admin; acme still has no owner.
    bootstrap_admin(&t.db, &ctx("req-boot"), &Bootstrap::Promote(bob))
        .await
        .expect("bootstrap bob");
    let err = read(&t).await.check().expect_err("acme has no owner");
    assert_reason(&err, ErrorCode::AdministrationOrphaned, "ownerless_group");
    assert!(err.to_string().contains("active_human_admins=1"), "{err}");
    groups::add_member(
        &t.db,
        &ctx("req-add-bob-acme"),
        acme.group_id,
        bob,
        Role::Owner,
    )
    .await
    .expect("bob owner of acme");
    read(&t).await.check().expect("recovered");
    t.drop_now().await;
}

#[tokio::test]
async fn administration_before_the_migration_is_none() {
    let t = FreshDb::new().await;
    assert_eq!(
        administration(&t.db).await.expect("unmigrated"),
        None,
        "nothing to check before the schema exists"
    );
    t.drop_now().await;
}
