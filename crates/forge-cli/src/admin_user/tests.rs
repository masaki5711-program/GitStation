//! `forge admin user disable` and `forge admin bootstrap-admin` against a
//! migrated database.

use forge_core::UserKind;
use forge_db::change::{Actor, ChangeContext};
use forge_db::test_support::FreshDb;
use forge_db::users::NewUser;
use forge_errors::ErrorCode;

use super::*;
use crate::worker::loop_tests::Captured;

fn rid(request: &str) -> RequestId {
    RequestId::parse(request).expect("request id")
}

async fn user(t: &FreshDb, name: &str, kind: UserKind, is_admin: bool) {
    let ctx = ChangeContext::new(Actor::System, rid(&format!("req-cli-user-{name}")));
    let email = format!("{name}@example.test");
    let new = NewUser {
        kind,
        email: (kind == UserKind::Human).then_some(email.clone()),
        is_admin,
        ..NewUser::human(name, name, &email)
    };
    users::create(&t.db, &ctx, &new).await.expect("create user");
}

fn promote(username: &str) -> BootstrapArgs {
    BootstrapArgs {
        username: username.to_owned(),
        create: None,
    }
}

#[tokio::test]
async fn user_disable_refuses_the_last_admin_and_force_passes_it() {
    let t = FreshDb::migrated().await;
    user(&t, "alice", UserKind::Human, true).await;
    let err = disable_with_db(&t.db, &rid("req-cli-d1"), "alice", false)
        .await
        .expect_err("the last admin");
    assert_eq!(err.code(), ErrorCode::AdministrationConflict, "{err}");
    assert!(err.to_string().contains("reason=last_admin"), "{err}");

    let logs = Captured::default();
    let guard = logs.install();
    let out = disable_with_db(&t.db, &rid("req-cli-d2"), "alice", true)
        .await
        .expect("forced");
    drop(guard);
    assert!(out.starts_with("user alice: disabled (forced)"), "{out}");
    assert!(out.contains("no active human administrator"), "{out}");
    assert!(out.contains("bootstrap-admin"), "{out}");
    let events = logs.events();
    let disabled: Vec<_> = events
        .iter()
        .filter(|e| e["fields"]["event"] == "forge.admin.user.disabled")
        .collect();
    assert_eq!(disabled.len(), 1, "{events:?}");
    assert_eq!(disabled[0]["fields"]["forced"], true);
    // Past the guard: warn, with what it left behind.
    assert_eq!(disabled[0]["level"], "WARN", "{events:?}");
    assert_eq!(disabled[0]["fields"]["outcome"], "disabled_past_guard");
    assert_eq!(disabled[0]["fields"]["ownerless_groups"], 0);
    assert_eq!(disabled[0]["fields"]["ownerless_projects"], 0);
    assert_eq!(disabled[0]["fields"]["left_without_admin"], true);
    let all = serde_json::to_string(&events).expect("json");
    assert!(
        !all.contains("alice"),
        "the log names ids, not names: {all}"
    );

    let again = disable_with_db(&t.db, &rid("req-cli-d3"), "alice", false)
        .await
        .expect("already disabled");
    assert_eq!(again, "user alice: already disabled (nothing changed)\n");
    let err = disable_with_db(&t.db, &rid("req-cli-d4"), "nobody-here", false)
        .await
        .expect_err("unknown");
    assert_eq!(err.code(), ErrorCode::NotFound);
    t.drop_now().await;
}

#[tokio::test]
async fn user_disable_of_a_plain_user_says_disabled() {
    let t = FreshDb::migrated().await;
    user(&t, "bob", UserKind::Human, false).await;
    let logs = Captured::default();
    let guard = logs.install();
    let out = disable_with_db(&t.db, &rid("req-cli-bob"), "bob", false)
        .await
        .expect("disable");
    drop(guard);
    assert_eq!(out, "user bob: disabled\n");
    let events = logs.events();
    let disabled = events
        .iter()
        .find(|e| e["fields"]["event"] == "forge.admin.user.disabled")
        .expect("logged");
    assert_eq!(disabled["level"], "INFO", "{events:?}");
    t.drop_now().await;
}

#[tokio::test]
async fn bootstrap_admin_promotes_or_creates_and_is_refused_once_an_admin_exists() {
    let t = FreshDb::migrated().await;
    user(&t, "alice", UserKind::Human, false).await;
    user(&t, "ci-bot", UserKind::Bot, false).await;
    let err = bootstrap_with(&t.db, &rid("req-cli-b0"), &promote("ci-bot"))
        .await
        .expect_err("a bot");
    assert!(err.to_string().contains("reason=not_human"), "{err}");

    let out = bootstrap_with(&t.db, &rid("req-cli-b1"), &promote("alice"))
        .await
        .expect("promote");
    assert_eq!(out, "user alice: administrator (promoted)\n");
    let err = bootstrap_with(
        &t.db,
        &rid("req-cli-b2"),
        &BootstrapArgs {
            username: "root".to_owned(),
            create: Some(NewAdmin {
                email: "root@example.test".to_owned(),
                display_name: None,
            }),
        },
    )
    .await
    .expect_err("alice is an admin");
    assert_eq!(err.code(), ErrorCode::AdministrationConflict, "{err}");
    assert!(err.to_string().contains("reason=admin_exists"), "{err}");
    t.drop_now().await;
}

#[tokio::test]
async fn bootstrap_admin_create_makes_a_new_human_admin() {
    let t = FreshDb::migrated().await;
    let out = bootstrap_with(
        &t.db,
        &rid("req-cli-c1"),
        &BootstrapArgs {
            username: "root".to_owned(),
            create: Some(NewAdmin {
                email: "root@example.test".to_owned(),
                display_name: Some("Root Admin".to_owned()),
            }),
        },
    )
    .await
    .expect("create");
    assert_eq!(out, "user root: administrator (created)\n");
    let id = users::id_by_username(&t.db, "root").await.expect("exists");
    // The new admin can be disabled only with --force (the last admin).
    let err = disable_with_db(&t.db, &rid("req-cli-c2"), "root", false)
        .await
        .expect_err("last admin");
    assert!(err.to_string().contains("reason=last_admin"), "{err}");
    assert!(
        err.to_string().contains(&format!("user={}", id.get())),
        "{err}"
    );
    t.drop_now().await;
}
