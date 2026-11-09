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
    let done = revoke_all_credentials(&t.db, &ctx("req-revoke-1"))
        .await
        .expect("revoke");
    assert_eq!(
        done,
        Revoked {
            users: 2,
            tokens: 2,
            sessions: 1,
            projects: 0,
            webhooks: 0
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
    let again = revoke_all_credentials(&t.db, &ctx("req-revoke-2"))
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
        remove_all_admin_flags(&t.db, &ctx("req-rm-0"), "not_configured")
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
    let removed = remove_all_admin_flags(&t.db, &ctx("req-rm-1"), "not_configured")
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
