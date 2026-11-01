//! `forge admin project move`, `group move`, `user rename` against a
//! migrated database: the move is the API's, as actor `system` and exempt
//! from the rate limit.

use forge_db::change::{Actor, ChangeContext};
use forge_db::groups::{self, NewGroup};
use forge_db::projects::{self, NewProject};
use forge_db::test_support::FreshDb;
use forge_db::users::NewUser;
use forge_errors::ErrorCode;

use super::*;

fn rid(request: &str) -> RequestId {
    RequestId::parse(request).expect("request id")
}

fn system(request: &str) -> ChangeContext {
    ChangeContext::new(Actor::System, rid(request))
}

async fn count(t: &FreshDb, sql: &str) -> i64 {
    t.json_rows(sql).await[0]["n"].as_i64().expect("n")
}

#[tokio::test]
async fn the_operator_moves_past_the_rate_limit_as_system() {
    let t = FreshDb::migrated().await;
    let alice = users::create(
        &t.db,
        &system("req-cli-alice"),
        &NewUser::human("alice", "alice", "alice@example.test"),
    )
    .await
    .expect("alice");
    let acme = groups::create(
        &t.db,
        &system("req-cli-acme"),
        &NewGroup {
            segment: "acme".into(),
            parent: None,
            display_name: "acme".into(),
            description: String::new(),
            owner: alice.user_id,
        },
    )
    .await
    .expect("acme");
    let web = projects::create(
        &t.db,
        &system("req-cli-web"),
        &NewProject::new("web", acme.namespace_id, "web", alice.user_id),
    )
    .await
    .expect("web");
    // Twelve renames: past the API's 10 per 24 hours.
    for n in 0..12 {
        let out = move_project_with(
            &t.db,
            &rid(&format!("req-cli-mv{n}")),
            web.project_id.get(),
            &MoveArgs {
                namespace: Some(acme.namespace_id.get()),
                path: format!("w{n}"),
                reclaim: false,
            },
        )
        .await
        .expect("the operator is exempt");
        assert!(out.contains(&format!("\tacme/w{n}\t1\t1")), "{out}");
    }
    assert_eq!(
        count(
            &t,
            "SELECT count(*) AS n FROM forge.audit_log WHERE action = 'project.moved' \
             AND actor_kind = 'system' AND actor_label LIKE 'forge-cli:%'"
        )
        .await,
        12
    );
    // The subtree rules still hold: a group only goes below a group or the
    // top level (FG-REPO-0015), here into a personal namespace.
    let err = move_group_with(
        &t.db,
        &rid("req-cli-g"),
        acme.group_id.get(),
        &MoveArgs {
            namespace: Some(alice.namespace_id.get()),
            path: "acme".into(),
            reclaim: false,
        },
    )
    .await
    .expect_err("target kind");
    assert_eq!(err.code(), ErrorCode::TransferInvalid, "{err}");
    let out = move_group_with(
        &t.db,
        &rid("req-cli-g2"),
        acme.group_id.get(),
        &MoveArgs {
            namespace: None,
            path: "acme2".into(),
            reclaim: false,
        },
    )
    .await
    .expect("rename at the top level");
    assert!(out.contains("group\t"), "{out}");
    let out = rename_user_with(&t.db, &rid("req-cli-u"), "alice", "alicia", false)
        .await
        .expect("rename");
    assert!(out.contains("\talice\talicia\t"), "{out}");
    let err = rename_user_with(&t.db, &rid("req-cli-u2"), "nobody", "x", false)
        .await
        .expect_err("absent");
    assert_eq!(err.code(), ErrorCode::NotFound, "{err}");
    t.drop_now().await;
}
