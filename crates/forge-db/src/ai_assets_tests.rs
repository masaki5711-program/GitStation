//! `ai_assets.enabled` against a migrated database (plan 17 P17-3,
//! ADR-0035): off by default, a change audited in the same transaction,
//! the decision run on the locked project, and turning it off removes
//! nothing else.

use forge_core::RequestId;
use forge_errors::ErrorCode;

use super::*;
use crate::change::{Actor, ChangeContext};
use crate::projects::{self, CreatedProject, NewProject};
use crate::test_support::*;
use crate::users::{self, NewUser};

fn system(request: &str) -> ChangeContext {
    ChangeContext::new(Actor::System, RequestId::parse(request).expect("id"))
}

fn as_user(user: UserId, request: &str) -> ChangeContext {
    ChangeContext::new(Actor::User(user), RequestId::parse(request).expect("id"))
}

struct World {
    t: FreshDb,
    alice: UserId,
    web: CreatedProject,
}

async fn world() -> World {
    let t = FreshDb::migrated().await;
    let alice = users::create(
        &t.db,
        &system("req-user-alice"),
        &NewUser::human("alice", "alice", "alice@example.test"),
    )
    .await
    .expect("alice");
    let web = projects::create(
        &t.db,
        &system("req-project-web"),
        &NewProject::new("web", alice.namespace_id, "Web", alice.user_id),
    )
    .await
    .expect("web");
    World {
        t,
        alice: alice.user_id,
        web,
    }
}

impl World {
    async fn audit(&self) -> Vec<(String, serde_json::Value)> {
        sqlx::query_as(
            "SELECT action, details FROM forge.audit_log WHERE project_id = $1 \
             AND action LIKE 'ai_assets.%' ORDER BY id",
        )
        .bind(self.web.project_id.get())
        .fetch_all(&mut self.t.superuser().await)
        .await
        .expect("audit")
    }

    async fn set(&self, enabled: bool, request: &str) -> (ChangeOutcome, AiAssetsSetting) {
        let (outcome, setting, ()) = set_enabled_decided(
            &self.t.db,
            &as_user(self.alice, request),
            self.web.project_id,
            self.alice,
            enabled,
            |_| Ok(()),
        )
        .await
        .expect("set");
        (outcome, setting)
    }
}

#[tokio::test]
async fn a_project_is_off_until_a_person_turns_it_on_and_each_change_is_audited() {
    let w = world().await;
    let fresh = setting(&w.t.db, w.web.project_id).await.expect("read");
    assert!(!fresh.enabled);
    assert_eq!(fresh.changed_by, None);

    let (outcome, on) = w.set(true, "req-on").await;
    assert_eq!(outcome, ChangeOutcome::Changed);
    assert!(on.enabled);
    assert_eq!(on.changed_by, Some(w.alice));
    assert!(on.changed_at.is_some());
    assert_eq!(setting(&w.t.db, w.web.project_id).await.expect("read"), on);

    // Already on: nothing written.
    let (again, _) = w.set(true, "req-on-again").await;
    assert_eq!(again, ChangeOutcome::Unchanged);
    let audit = w.audit().await;
    assert_eq!(audit.len(), 1, "{audit:?}");
    assert_eq!(audit[0].0, "ai_assets.enabled");
    assert_eq!(
        audit[0].1,
        serde_json::json!({"project_id": w.web.project_id.get(), "from": false, "to": true})
    );
    w.t.drop_now().await;
}

#[tokio::test]
async fn disable_keeps_past_releases() {
    // Turning it off is one audited row and nothing else: the history (the
    // audit trail of both changes) stays, and no row of the project is
    // removed (P17-7's releases live beside it and are never touched here).
    let w = world().await;
    let _ = w.set(true, "req-on").await;
    let (outcome, off) = w.set(false, "req-off").await;
    assert_eq!(outcome, ChangeOutcome::Changed);
    assert!(!off.enabled);
    let actions: Vec<String> = w.audit().await.into_iter().map(|(a, _)| a).collect();
    assert_eq!(actions, vec!["ai_assets.enabled", "ai_assets.disabled"]);
    let rows: i64 =
        sqlx::query_scalar("SELECT count(*) FROM forge.project_ai_assets WHERE project_id = $1")
            .bind(w.web.project_id.get())
            .fetch_one(&mut w.t.superuser().await)
            .await
            .expect("count");
    assert_eq!(rows, 1, "the setting row stays, off");
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_refused_decision_writes_nothing() {
    let w = world().await;
    let refused = set_enabled_decided(
        &w.t.db,
        &as_user(w.alice, "req-refused"),
        w.web.project_id,
        w.alice,
        true,
        |_| -> Result<(), ForgeError> { Err(ForgeError::new(ErrorCode::Forbidden)) },
    )
    .await
    .expect_err("refused");
    assert_eq!(refused.code(), ErrorCode::Forbidden);
    assert!(
        !setting(&w.t.db, w.web.project_id)
            .await
            .expect("read")
            .enabled
    );
    w.t.drop_now().await;
}
