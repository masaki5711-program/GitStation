//! The portal news outbox and the widget reads (ADR-0036) against a
//! migrated database: nothing staged while news is off, one row and one
//! job per `source_id` while on, the read-only link rules, the member
//! projects and the newest pipeline / deployment of each.

use std::time::Duration;

use forge_core::portal::NewsTopic;
use forge_core::{RequestId, Role};
use serde_json::json;

use super::*;
use crate::change::{Actor, ChangeContext};
use crate::groups::{self, NewGroup};
use crate::projects::{self, NewProject};
use crate::test_support::*;
use crate::users::{self, NewUser};

fn system(request: &str) -> ChangeContext {
    ChangeContext::new(
        Actor::System,
        RequestId::parse(request).expect("request id"),
    )
}

async fn person(t: &FreshDb, name: &str, staff: Option<&str>, admin: bool) -> UserId {
    let mut new = NewUser::human(name, name, &format!("{name}@example.test"));
    new.employee_number = staff.map(str::to_owned);
    new.is_admin = admin;
    users::create(&t.db, &system(&format!("req-user-{name}")), &new)
        .await
        .expect("user")
        .user_id
}

async fn project_of(t: &FreshDb, owner: UserId, group: &str) -> ProjectId {
    let g = groups::create(
        &t.db,
        &system(&format!("req-{group}")),
        &NewGroup {
            visibility: forge_core::Visibility::Private,
            segment: group.into(),
            parent: None,
            display_name: group.into(),
            description: String::new(),
            owner,
        },
    )
    .await
    .expect("group");
    projects::create(
        &t.db,
        &system(&format!("req-{group}-p")),
        &NewProject::new("app", g.namespace_id, "App", owner),
    )
    .await
    .expect("project")
    .project_id
}

fn policy() -> NewsPolicy {
    NewsPolicy::new(3, Duration::from_secs(5)).expect("policy")
}

fn draft(project: ProjectId, audience: Vec<UserId>, source: &str) -> NewsDraft {
    NewsDraft {
        source_id: source.to_owned(),
        topic: NewsTopic::DeployFailed,
        project,
        facts: json!({ "tier": "prod", "sha": "0123456789ab", "status": "failed" }),
        audience,
        request_id: RequestId::parse("req-news").expect("id"),
    }
}

async fn count(t: &FreshDb, sql: &str) -> i64 {
    t.json_rows(&format!("SELECT count(*) AS n FROM ({sql}) x"))
        .await[0]["n"]
        .as_i64()
        .expect("count")
}

#[tokio::test]
async fn nothing_is_staged_while_news_is_off() {
    let t = FreshDb::migrated().await;
    let olga = person(&t, "olga", None, false).await;
    let p = project_of(&t, olga, "acme").await;
    assert!(t.db.portal_news().is_none(), "off by default");
    let staged = stage_alone(&t.db, &draft(p, vec![olga], "gitstation-deploy-1-failed"))
        .await
        .expect("stage");
    assert_eq!(staged, None);
    assert_eq!(count(&t, "SELECT 1 FROM forge.portal_news").await, 0);
    assert_eq!(
        count(
            &t,
            "SELECT 1 FROM forge.jobs WHERE kind = 'portal.news_send'"
        )
        .await,
        0
    );
}

#[tokio::test]
async fn the_same_source_id_is_staged_once_with_one_job() {
    let t = FreshDb::migrated().await;
    let olga = person(&t, "olga", Some("SYN0001"), false).await;
    let p = project_of(&t, olga, "acme").await;
    let db = t.db.clone().with_portal_news(Some(policy()));
    let first = stage_alone(
        &db,
        &draft(p, vec![olga, olga], "gitstation-deploy-7-failed"),
    )
    .await
    .expect("stage")
    .expect("a row");
    let again = stage_alone(&db, &draft(p, vec![olga], "gitstation-deploy-7-failed"))
        .await
        .expect("stage again");
    assert_eq!(again, None, "idempotent on source_id");
    assert_eq!(count(&t, "SELECT 1 FROM forge.portal_news").await, 1);
    let jobs = t
        .json_rows("SELECT payload, max_attempts FROM forge.jobs WHERE kind = 'portal.news_send'")
        .await;
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0]["payload"]["news_id"], first);
    assert_eq!(jobs[0]["max_attempts"], 3);
    // The row as the sender reads it: the audience once, the dates in UTC.
    let news = news(&db, first).await.expect("read").expect("row");
    assert_eq!(news.audience, vec![olga]);
    assert_eq!(news.state, "pending");
    assert!(news.occurred_at.ends_with('Z') && news.expires_at > news.occurred_at);
    // Nobody to tell: nothing staged.
    let empty = stage_alone(&db, &draft(p, Vec::new(), "gitstation-deploy-8-failed"))
        .await
        .expect("stage");
    assert_eq!(empty, None);
    // The outcome is recorded once, from pending only.
    assert!(record_sent(&db, first, 1).await.expect("sent"));
    assert!(!record_sent(&db, first, 2).await.expect("again"));
    let staff = audience(&db, &[olga]).await.expect("audience");
    assert_eq!(staff[0].staff_id.as_deref(), Some("SYN0001"));
    let h = health(&db).await.expect("health");
    assert_eq!((h.pending, h.failed_24h), (0, 0));
}

#[tokio::test]
async fn a_deployments_end_stages_its_news_in_the_same_change() {
    let t = FreshDb::migrated().await;
    let olga = person(&t, "olga", Some("SYN0001"), false).await;
    let p = project_of(&t, olga, "acme").await;
    let db = t.db.clone().with_portal_news(Some(policy()));
    // An abandoned (stale) running deployment ends failed, with its news.
    t.commit_audited(
        ("project", p.get()),
        &[format!(
            "INSERT INTO forge.deploy_environments (project_id, tier, requires_approval, \
                 auto_rollback, created_by, updated_by) VALUES ({}, 'stg', false, false, {o}, {o})",
            p.get(),
            o = olga.get()
        )],
    )
    .await;
    let env = t
        .json_rows("SELECT id FROM forge.deploy_environments")
        .await[0]["id"]
        .as_i64()
        .expect("env");
    t.commit_audited(
        ("project", p.get()),
        &[format!(
            "INSERT INTO forge.deployments (project_id, environment_id, tier, sha, status, \
             requested_by, started_at, heartbeat_at, request_id) VALUES ({}, {env}, 'stg', \
             '{}', 'running', {}, now() - interval '1 hour', now() - interval '1 hour', 'req-d')",
            p.get(),
            "a".repeat(40),
            olga.get()
        )],
    )
    .await;
    let ended = crate::deploy::abandon_stale(&db, &RequestId::generate(), 60)
        .await
        .expect("abandon");
    assert_eq!(ended.len(), 1);
    let rows = t
        .json_rows("SELECT topic, source_id, audience FROM forge.portal_news")
        .await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["topic"], "deploy_failed");
    assert_eq!(rows[0]["audience"], json!([olga.get()]));
}

#[tokio::test]
async fn an_unlinked_portal_user_is_refused() {
    let t = FreshDb::migrated().await;
    let iss = "https://sso.example.test/realms/iotpf";
    let ada = person(&t, "ada", Some("SYN0001"), false).await;
    person(&t, "root", Some("SYN0002"), true).await;
    person(&t, "twin1", Some("syn0003"), false).await;
    person(&t, "twin2", Some("SYN0003"), false).await;
    let reason = |e: ForgeError| {
        assert_eq!(e.code(), ErrorCode::PortalUserNotLinked);
        e.context()
            .iter()
            .find(|(k, _)| *k == "reason")
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    };
    // The employee number, without the case, links the one user.
    let found = portal_user(&t.db, iss, "sub-ada", Some(" syn0001 "), true)
        .await
        .expect("linked by staff id");
    assert_eq!(found.user, ada);
    assert!(found.active);
    for (staff, auto_link, want) in [
        (Some("SYN9999"), true, "unlinked"),
        (None, true, "employee_number_missing"),
        (Some("SYN0003"), true, "employee_number_ambiguous"),
        (Some("SYN0002"), true, "privileged_account"),
        (Some("SYN0001"), false, "auto_link_disabled"),
    ] {
        let e = portal_user(&t.db, iss, "sub-x", staff, auto_link)
            .await
            .expect_err("refused");
        assert_eq!(reason(e), want, "{staff:?}");
    }
    // A linked identity wins (and the number then does not matter); ada's
    // number under another subject of the same issuer is not hers.
    t.commit_audited(
        ("user", ada.get()),
        &[format!(
            "INSERT INTO forge.user_identities (user_id, issuer, subject) VALUES ({}, '{iss}', 'sub-ada')",
            ada.get()
        )],
    )
    .await;
    let linked = portal_user(&t.db, iss, "sub-ada", None, false)
        .await
        .expect("linked");
    assert_eq!(linked.user, ada);
    let other = portal_user(&t.db, iss, "sub-someone-else", Some("SYN0001"), true)
        .await
        .expect_err("linked to another subject");
    assert_eq!(reason(other), "employee_number_linked");
    // Nothing was written by any of these.
    assert_eq!(count(&t, "SELECT 1 FROM forge.user_identities").await, 1);
}

#[tokio::test]
async fn the_widget_reads_name_members_projects_and_their_newest_state() {
    let t = FreshDb::migrated().await;
    let olga = person(&t, "olga", None, false).await;
    let dave = person(&t, "dave", None, false).await;
    let eve = person(&t, "eve", None, false).await;
    let a = project_of(&t, olga, "acme").await;
    let b = project_of(&t, olga, "beta").await;
    projects::add_member(&t.db, &system("req-m"), a, dave, Role::Reporter)
        .await
        .expect("member");
    assert_eq!(
        member_projects(&t.db, olga, 200).await.expect("olga"),
        vec![b, a]
    );
    assert_eq!(
        member_projects(&t.db, dave, 200).await.expect("dave"),
        vec![a]
    );
    assert!(member_projects(&t.db, eve, 200)
        .await
        .expect("eve")
        .is_empty());
    let none = latest_pipelines(&t.db, &[a, b], "refs/heads/main")
        .await
        .expect("pipelines");
    assert_eq!(none, vec![(a, None), (b, None)]);
    let deploys = latest_deployments(&t.db, &[b]).await.expect("deploys");
    assert_eq!(deploys, vec![(b, None)]);
    // The spec release's audience: the people with a role on the project.
    let mut conn = t.db.write_pool().acquire().await.expect("conn");
    let people = project_people(&mut conn, a).await.expect("people");
    assert!(
        people.contains(&olga) && people.contains(&dave),
        "{people:?}"
    );
    assert!(!people.contains(&eve));
}
