//! `forge admin runner register | list | disable` against a migrated
//! database (M4 C4): the token is printed once and authenticates as the
//! runner; the list never shows it; a non-administrator registrant and bad
//! names are refused before anything is written; a disabled runner's token
//! is refused. Synthetic data only.

use forge_core::RequestId;
use forge_db::change::{Actor, ChangeContext};
use forge_db::test_support::FreshDb;
use forge_db::users::{self, NewUser};
use forge_errors::ErrorCode;

use super::*;

fn rid(request: &str) -> RequestId {
    RequestId::parse(request).expect("request id")
}

async fn human(t: &FreshDb, name: &str, admin: bool) {
    let ctx = ChangeContext::new(Actor::System, rid(&format!("req-user-{name}")));
    let mut new = NewUser::human(name, name, &format!("{name}@example.test"));
    new.is_admin = admin;
    users::create(&t.db, &ctx, &new).await.expect("user");
}

fn args(name: &str, admin: &str) -> RegisterArgs {
    RegisterArgs {
        name: name.to_owned(),
        tags: Some("podman, linux,linux".to_owned()),
        description: Some("synthetic runner".to_owned()),
        admin: admin.to_owned(),
    }
}

#[tokio::test]
async fn register_prints_the_token_once_and_disable_stops_it() {
    let t = FreshDb::migrated().await;
    human(&t, "ada", true).await;
    human(&t, "bob", false).await;
    let out = register_with(&t.db, &rid("req-cli-runner-1"), &args("linux-1", "ada"))
        .await
        .expect("registered");
    let token = out
        .lines()
        .find_map(|l| l.strip_prefix("token      "))
        .expect("token line")
        .to_owned();
    assert!(token.starts_with("fgr_"), "{out}");
    assert!(out.contains("tags       linux,podman"), "{out}");
    let who = ci_runners::authenticate(&t.db, &rid("req-auth"), &token)
        .await
        .expect("the token authenticates");
    let runner_id = match who {
        forge_core::CiIdentity::Runner { runner_id } => runner_id,
        other => panic!("not a runner: {other:?}"),
    };
    let listed = list_with(&t.db).await.expect("list");
    assert!(listed.contains("linux-1"), "{listed}");
    assert!(!listed.contains(&token), "the list shows no token");
    assert!(!listed.contains("fgr_"), "{listed}");
    // Refused before anything is written.
    for (a, code) in [
        (args("linux-2", "bob"), ErrorCode::Forbidden),
        (args("Bad Name", "ada"), ErrorCode::RequestInvalid),
        (args("linux-1", "ada"), ErrorCode::RequestInvalid),
        (
            RegisterArgs {
                tags: Some("ok,--privileged".to_owned()),
                ..args("linux-3", "ada")
            },
            ErrorCode::RequestInvalid,
        ),
    ] {
        let err = register_with(&t.db, &rid("req-cli-runner-x"), &a)
            .await
            .expect_err("refused");
        assert_eq!(err.code(), code, "{a:?}");
    }
    assert_eq!(list_with(&t.db).await.expect("list").lines().count(), 2);
    // Disabled: the token is refused at once.
    let out = disable_with(&t.db, &rid("req-cli-runner-off"), runner_id)
        .await
        .expect("disabled");
    assert!(out.contains("disabled"), "{out}");
    let err = ci_runners::authenticate(&t.db, &rid("req-auth-2"), &token)
        .await
        .expect_err("refused");
    assert_eq!(err.code(), ErrorCode::Unauthenticated);
    let err = disable_with(&t.db, &rid("req-cli-runner-none"), runner_id + 100)
        .await
        .expect_err("absent");
    assert_eq!(err.code(), ErrorCode::NotFound);
    t.drop_now().await;
}
