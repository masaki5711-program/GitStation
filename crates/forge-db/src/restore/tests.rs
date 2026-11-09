//! forge-db restore (M3 O1): revocation and the narrowing-only re-apply,
//! against a migrated database. Synthetic data only. The end-to-end tests
//! (backup → restore → verify → finish) are forge-cli's.

use forge_core::ids::UserId;
use forge_core::{RequestId, Role, Scope};

use super::*;
use crate::change::Actor;
use crate::test_support::FreshDb;

fn ctx(request: &str) -> ChangeContext {
    ChangeContext::new(Actor::System, RequestId::parse(request).expect("id"))
}

async fn human(t: &FreshDb, name: &str) -> UserId {
    crate::users::create(
        &t.db,
        &ctx(&format!("req-u-{name}")),
        &crate::users::NewUser::human(name, name, &format!("{name}@example.test")),
    )
    .await
    .expect("user")
    .user_id
}

async fn count(t: &FreshDb, sql: &str) -> i64 {
    let rows = t.json_rows(sql).await;
    rows[0]
        .as_object()
        .and_then(|o| o.values().next())
        .and_then(serde_json::Value::as_i64)
        .expect("a count")
}

#[tokio::test]
async fn revoking_every_credential_is_audited_per_user_and_idempotent() {
    let t = FreshDb::migrated().await;
    let (a, b) = (human(&t, "ada").await, human(&t, "bob").await);
    for u in [a, b] {
        crate::tokens::issue(
            &t.db,
            &ctx("req-tok"),
            &crate::tokens::NewToken {
                user: u,
                name: "cli".to_owned(),
                scopes: vec![Scope::ReadApi],
                expires_in: None,
                is_agent: false,
            },
        )
        .await
        .expect("token");
    }
    crate::test_support::issue_session(&t.db, b).await;
    let done = revoke_all_credentials(&t.db, &ctx("req-revoke-1"), "req-begin")
        .await
        .expect("revoke");
    assert_eq!(
        done,
        Revoked {
            users: 2,
            tokens: 2,
            sessions: 1,
            projects: 0,
            webhooks: 0,
            push_mirrors: 0,
            integrations: 0,
            assertions: 0,
        }
    );
    assert_eq!(
        count(
            &t,
            "SELECT count(*) FROM forge.tokens WHERE revoked_reason = 'restore'"
        )
        .await,
        2
    );
    assert_eq!(
        count(
            &t,
            "SELECT count(*) FROM forge.web_sessions WHERE revoked_reason = 'restore'"
        )
        .await,
        1
    );
    assert_eq!(
        count(
            &t,
            "SELECT count(*) FROM forge.audit_log WHERE action = 'restore.credentials_revoked' \
             AND resource_type = 'user'"
        )
        .await,
        2
    );
    let again = revoke_all_credentials(&t.db, &ctx("req-revoke-2"), "req-begin")
        .await
        .expect("again");
    assert_eq!(again, Revoked::default(), "nothing left to revoke");
    t.drop_now().await;
}

#[tokio::test]
async fn a_copy_never_widens_what_the_database_holds() {
    let t = FreshDb::migrated().await;
    let (a, b) = (human(&t, "cal").await, human(&t, "dot").await);
    let mut copy = state::read(&t.db).await.expect("state");
    // A replaced copy that tries to give rights: b an administrator and
    // enabled, a an owner of a group that does not hold it.
    for u in &mut copy.users {
        if u.id == b.get() {
            u.is_admin = true;
        }
    }
    copy.group_members.push(state::Membership {
        scope: 1,
        user: a.get(),
        role: Role::Owner.as_str().to_owned(),
    });
    t.commit_audited(
        ("user", b.get()),
        &[format!(
            "UPDATE forge.users SET state = 'disabled' WHERE id = {}",
            b.get()
        )],
    )
    .await;
    let done = state::reapply(&t.db, &ctx("req-reapply"), &copy)
        .await
        .expect("reapply");
    assert_eq!(done, state::Reapplied::default(), "nothing to narrow");
    assert_eq!(
        count(&t, "SELECT count(*) FROM forge.users WHERE is_admin").await,
        0
    );
    assert_eq!(
        count(
            &t,
            "SELECT count(*) FROM forge.users WHERE state = 'disabled'"
        )
        .await,
        1,
        "b stays disabled"
    );
    assert_eq!(
        count(&t, "SELECT count(*) FROM forge.group_members").await,
        0
    );
    t.drop_now().await;
}

#[tokio::test]
async fn removing_every_admin_flag_lists_them_once() {
    let t = FreshDb::migrated().await;
    let a = human(&t, "eli").await;
    assert!(
        remove_all_admin_flags(&t.db, &ctx("req-rm-0"), "not_configured", "req-begin")
            .await
            .expect("none")
            .is_empty()
    );
    t.commit_audited(
        ("user", a.get()),
        &[format!(
            "UPDATE forge.users SET is_admin = true WHERE id = {}",
            a.get()
        )],
    )
    .await;
    let removed = remove_all_admin_flags(&t.db, &ctx("req-rm-1"), "not_configured", "req-begin")
        .await
        .expect("removed");
    assert_eq!(
        removed,
        vec![AdminRemoved {
            user: a,
            username: "eli".to_owned()
        }]
    );
    assert_eq!(
        count(
            &t,
            "SELECT count(*) FROM forge.audit_log WHERE action = 'restore.admin_flags_removed'"
        )
        .await,
        2,
        "the instance row and the user's row"
    );
    t.drop_now().await;
}

/// A project of a new user, and a service account owned by them.
async fn project_and_account(t: &FreshDb, name: &str) -> (i64, UserId, UserId, i64) {
    let owner = crate::users::create(
        &t.db,
        &ctx(&format!("req-u-{name}")),
        &crate::users::NewUser::human(name, name, &format!("{name}@example.test")),
    )
    .await
    .expect("user");
    let p = crate::projects::create(
        &t.db,
        &ctx(&format!("req-p-{name}")),
        &crate::projects::NewProject::new("app", owner.namespace_id, "App", owner.user_id),
    )
    .await
    .expect("project");
    let sa = crate::service_accounts::create(
        &t.db,
        &ctx(&format!("req-sa-{name}")),
        &crate::service_accounts::NewServiceAccount {
            username: format!("{name}-bot"),
            display_name: "Bot".to_owned(),
            owner: owner.user_id,
            purpose: "synthetic test account".to_owned(),
        },
        owner.user_id,
    )
    .await
    .expect("service account")
    .user_id;
    (p.project_id.get(), owner.user_id, sa, p.repository_id.get())
}

#[tokio::test]
async fn push_mirrors_and_integrations_are_disabled_with_reason_restore() {
    let t = FreshDb::migrated().await;
    let (p, owner, sa, repo) = project_and_account(&t, "fio").await;
    t.commit_audited(
        ("project", p),
        &[format!(
            "INSERT INTO forge.push_mirrors (repo_id, project_id, url, origin, credential_name, \
             created_by) VALUES ({repo}, {p}, 'https://gitlab.example.invalid/x.git', \
             'https://gitlab.example.invalid', 'mirror', {})",
            owner.get()
        )],
    )
    .await;
    t.commit_audited(
        ("integration", 9001),
        &[format!(
            "INSERT INTO forge.integrations (id, kind, service_account_id, event_types) \
             OVERRIDING SYSTEM VALUE VALUES (9001, 'kanflow', {}, ARRAY['*'])",
            sa.get()
        )],
    )
    .await;
    let done = revoke_all_credentials(&t.db, &ctx("req-revoke-m"), "req-begin-m")
        .await
        .expect("revoke");
    assert_eq!((done.push_mirrors, done.integrations), (1, 1));
    assert_eq!(
        count(&t, "SELECT count(*) FROM forge.push_mirrors WHERE enabled").await,
        0
    );
    assert_eq!(
        count(&t, "SELECT count(*) FROM forge.integrations WHERE enabled").await,
        0
    );
    assert_eq!(
        count(
            &t,
            "SELECT count(*) FROM forge.audit_log WHERE action IN \
             ('restore.push_mirrors_disabled', 'restore.integration_disabled') \
             AND details->>'reason' = 'restore' AND details->>'begin' = 'req-begin-m'"
        )
        .await,
        2
    );
    let again = revoke_all_credentials(&t.db, &ctx("req-revoke-m2"), "req-begin-m")
        .await
        .expect("again");
    assert_eq!(again, Revoked::default());
    t.drop_now().await;
}

#[tokio::test]
async fn visibility_approvers_and_check_grants_are_narrowed_from_the_copy() {
    let t = FreshDb::migrated().await;
    let (p, owner, sa, _) = project_and_account(&t, "gus").await;
    let (u1, u2) = (human(&t, "hui").await, human(&t, "ivo").await);
    t.commit_audited(
        ("project", p),
        &[
            format!("UPDATE forge.projects SET visibility = 'internal' WHERE id = {p}"),
            format!(
                "INSERT INTO forge.approval_rules (project_id, name, required_approvals) \
                 VALUES ({p}, 'core', 1)"
            ),
            format!(
                "INSERT INTO forge.approval_rule_approvers (rule_id, project_id, user_id) \
                 SELECT id, {p}, u FROM forge.approval_rules, unnest(ARRAY[{}, {}]) AS u \
                 WHERE project_id = {p}",
                u1.get(),
                u2.get()
            ),
            format!(
                "INSERT INTO forge.check_namespace_grants (user_id, project_id, namespace, \
                 granted_by) VALUES ({}, {p}, 'ci', {})",
                sa.get(),
                owner.get()
            ),
        ],
    )
    .await;
    // The copy: taken after the project became private again, ivo left the
    // rule's approvers and the ci grant was revoked.
    let mut copy = state::read(&t.db).await.expect("state");
    copy.private_projects.push(p);
    for r in &mut copy.approval_rules {
        r.approvers.retain(|a| a.user == Some(u1.get()));
    }
    copy.check_grants.clear();
    let done = state::reapply(&t.db, &ctx("req-reapply-v"), &copy)
        .await
        .expect("reapply");
    assert_eq!(done.visibility_narrowed, 1);
    assert_eq!(done.approvers_narrowed, 1);
    assert_eq!(done.check_grants_revoked, 1);
    assert_eq!(
        count(
            &t,
            &format!(
                "SELECT count(*) FROM forge.projects WHERE id = {p} AND visibility = 'private'"
            )
        )
        .await,
        1
    );
    assert_eq!(
        count(
            &t,
            &format!(
                "SELECT count(*) FROM forge.approval_rule_approvers WHERE user_id = {}",
                u2.get()
            )
        )
        .await,
        0
    );
    assert_eq!(
        count(&t, "SELECT count(*) FROM forge.check_namespace_grants").await,
        0
    );
    // A copy that would empty the approvers ("any Developer") or make the
    // project internal changes nothing.
    let mut wider = state::read(&t.db).await.expect("state");
    wider.private_projects.clear();
    for r in &mut wider.approval_rules {
        r.approvers.clear();
    }
    let none = state::reapply(&t.db, &ctx("req-reapply-w"), &wider)
        .await
        .expect("reapply");
    assert_eq!(none, state::Reapplied::default());
    t.drop_now().await;
}

#[tokio::test]
async fn the_repository_lock_is_exclusive_and_ends_with_its_connection() {
    let t = FreshDb::migrated().await;
    let repo = forge_core::ids::RepoId::new(4242);
    let mut held = RepoMaintLock::try_lock(&t.db, repo)
        .await
        .expect("try")
        .expect("free");
    held.alive().await.expect("alive");
    assert!(RepoMaintLock::try_lock(&t.db, repo)
        .await
        .expect("try")
        .is_none());
    assert!(
        RepoMaintLock::lock(&t.db, repo, std::time::Duration::from_millis(600))
            .await
            .expect("lock")
            .is_none(),
        "waited, still held"
    );
    // Another repository is not affected.
    RepoMaintLock::try_lock(&t.db, forge_core::ids::RepoId::new(4243))
        .await
        .expect("try")
        .expect("free")
        .release()
        .await;
    drop(held);
    let again = RepoMaintLock::lock(&t.db, repo, std::time::Duration::from_secs(5))
        .await
        .expect("lock")
        .expect("dropping the lock released it");
    again.release().await;
    t.drop_now().await;
}
