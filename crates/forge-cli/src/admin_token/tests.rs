//! `forge admin token …` against a migrated database.

use forge_db::test_support::FreshDb;
use forge_db::users::NewUser;

use super::*;
use crate::worker::loop_tests::Captured;

fn rid(request: &str) -> RequestId {
    RequestId::parse(request).expect("request id")
}

async fn alice(t: &FreshDb) {
    let ctx = ChangeContext::new(Actor::System, rid("req-cli-user"));
    users::create(
        &t.db,
        &ctx,
        &NewUser::human("alice", "Alice", "alice@example.test"),
    )
    .await
    .expect("create user");
}

fn args(scopes: &str) -> IssueArgs {
    IssueArgs {
        username: "alice".to_owned(),
        scopes: scopes.to_owned(),
        expires_in: Some("30d".to_owned()),
        name: "laptop".to_owned(),
        agent: false,
    }
}

#[test]
fn scopes_and_days_parse_and_unknown_values_are_token_request_invalid() {
    assert_eq!(
        parse_scopes("read_api, write_repository").expect("scopes"),
        vec![Scope::ReadApi, Scope::WriteRepository]
    );
    let err = parse_scopes("read_api,teleport").expect_err("unknown");
    assert_eq!(err.code(), ErrorCode::TokenRequestInvalid);
    assert!(err.to_string().contains("reason=scope_unknown"), "{err}");
    assert!(!err.to_string().contains("teleport"), "not echoed: {err}");
    assert_eq!(
        parse_days("90d").expect("days"),
        Duration::from_secs(90 * 86400)
    );
    for bad in ["90", "d", "-1d", "1.5d", "90 d", ""] {
        let err = parse_days(bad).expect_err(bad);
        assert!(
            err.to_string().contains("reason=expires_malformed"),
            "{err}"
        );
    }
}

#[tokio::test]
async fn token_issue_prints_the_token_once_and_logs_only_the_public_id() {
    let t = FreshDb::migrated().await;
    alice(&t).await;
    let logs = Captured::default();
    let guard = logs.install();
    let out = issue_with(
        &t.db,
        &rid("req-cli-issue"),
        &args("read_api,read_repository"),
    )
    .await
    .expect("issue");
    drop(guard);
    let token = out
        .lines()
        .find_map(|l| l.strip_prefix("token       "))
        .expect("token line")
        .to_owned();
    assert!(token.starts_with("fgp_"), "{out}");
    assert_eq!(out.matches(&token).count(), 1, "printed once: {out}");
    let public_id = token.split('_').nth(1).expect("public id");
    let events = logs.events();
    let issued: Vec<_> = events
        .iter()
        .filter(|e| e["fields"]["event"] == "forge.admin.token.issued")
        .collect();
    assert_eq!(issued.len(), 1, "{events:?}");
    assert_eq!(issued[0]["fields"]["public_id"], public_id);
    let all = serde_json::to_string(&events).expect("json");
    assert!(!all.contains(&token), "the token is never logged");
    assert!(
        !all.contains(token.rsplit('_').next().expect("secret")),
        "nor its secret"
    );
    // The printed token authenticates.
    let identity = tokens::authenticate(&t.db, &rid("req-cli-auth"), &token)
        .await
        .expect("authenticates");
    assert_eq!(identity.scopes, vec![Scope::ReadApi, Scope::ReadRepository]);
    t.drop_now().await;
}

#[tokio::test]
async fn token_revoke_and_list_show_the_state_without_secrets() {
    let t = FreshDb::migrated().await;
    alice(&t).await;
    let out = issue_with(&t.db, &rid("req-cli-i"), &args("read_api"))
        .await
        .expect("issue");
    let token = out
        .lines()
        .find_map(|l| l.strip_prefix("token       "))
        .expect("token line")
        .to_owned();
    let token_id: i64 = out
        .lines()
        .find_map(|l| l.strip_prefix("token_id    "))
        .expect("id line")
        .parse()
        .expect("id");
    let first = revoke_with(&t.db, &rid("req-cli-r1"), token_id)
        .await
        .expect("revoke");
    assert_eq!(first, format!("token {token_id}: revoked\n"));
    let again = revoke_with(&t.db, &rid("req-cli-r2"), token_id)
        .await
        .expect("again");
    assert!(again.contains("already revoked"), "{again}");
    let listed = list_with(&t.db, "alice").await.expect("list");
    assert_eq!(listed.lines().count(), 2, "header + one token: {listed}");
    assert!(
        listed.contains("laptop") && listed.contains("read_api"),
        "{listed}"
    );
    assert!(!listed.contains(&token), "{listed}");
    let err = list_with(&t.db, "nobody-here")
        .await
        .expect_err("unknown user");
    assert_eq!(err.code(), ErrorCode::NotFound);
    t.drop_now().await;
}
