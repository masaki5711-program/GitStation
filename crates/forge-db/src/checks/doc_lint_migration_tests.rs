//! The document lint's migration setting against a migrated database (plan
//! 13 Q13; migration 20261203100000): the decided set / clear with their
//! audit rows, the bounds by the database's date, the expiry without a
//! write, and the table's CHECK.

use forge_core::ids::{ProjectId, UserId};
use forge_core::RequestId;
use forge_errors::ErrorCode;

use super::*;
use crate::change::{Actor, ChangeContext};
use crate::projects::{self, CreatedProject, NewProject};
use crate::test_support::*;
use crate::users::{self, NewUser};

fn system(request: &str) -> ChangeContext {
    ChangeContext::new(
        Actor::System,
        RequestId::parse(request).expect("request id"),
    )
}

fn as_user(user: UserId, request: &str) -> ChangeContext {
    ChangeContext::new(
        Actor::User(user),
        RequestId::parse(request).expect("request id"),
    )
}

struct World {
    t: FreshDb,
    maintainer: UserId,
    app: CreatedProject,
}

async fn world() -> World {
    let t = FreshDb::migrated().await;
    let maintainer = users::create(
        &t.db,
        &system("req-user-mira"),
        &NewUser::human("mira", "Mira", "mira@example.test"),
    )
    .await
    .expect("user");
    let app = projects::create(
        &t.db,
        &system("req-project-app"),
        &NewProject::new("app", maintainer.namespace_id, "App", maintainer.user_id),
    )
    .await
    .expect("app");
    World {
        t,
        maintainer: maintainer.user_id,
        app,
    }
}

impl World {
    fn project(&self) -> ProjectId {
        self.app.project_id
    }

    async fn date(&self, days: i32) -> String {
        sqlx::query_scalar("SELECT to_char((now() AT TIME ZONE 'UTC')::date + $1, 'YYYY-MM-DD')")
            .bind(days)
            .fetch_one(&mut self.t.superuser().await)
            .await
            .expect("date")
    }

    async fn set(&self, request: &str, until: &str) -> Result<SetOutcome, ForgeError> {
        set_doc_lint_migration_decided(
            &self.t.db,
            &as_user(self.maintainer, request),
            self.project(),
            self.maintainer,
            until,
            |_| Ok(()),
        )
        .await
        .map(|(o, ())| o)
    }

    async fn audit(&self, action: &str) -> Vec<serde_json::Value> {
        sqlx::query_scalar(
            "SELECT details FROM forge.audit_log WHERE action = $1 AND resource_type = 'project' \
             AND resource_id = $2 ORDER BY id",
        )
        .bind(action)
        .bind(self.project().get())
        .fetch_all(&mut self.t.superuser().await)
        .await
        .expect("audit")
    }
}

#[tokio::test]
async fn setting_a_date_audits_it_and_it_is_in_force_through_its_day() {
    let w = world().await;
    let before = read(&w.t.db, w.project()).await.expect("read");
    assert_eq!(before.setting, None);
    assert_eq!(before.today, w.date(0).await);
    assert_eq!(before.max_until, w.date(180).await);
    assert_eq!(before.in_force(), None);

    let until = w.date(30).await;
    assert_eq!(
        w.set("req-set", &until).await.expect("set"),
        SetOutcome::Created
    );
    let got = read(&w.t.db, w.project()).await.expect("read");
    let s = got.setting.clone().expect("setting");
    assert_eq!(s.warn_until, until);
    assert_eq!(s.set_by, w.maintainer);
    assert!(s.in_force);
    assert_eq!(got.in_force(), Some(until.as_str()));
    assert_eq!(
        in_force(&w.t.db, w.project()).await.expect("in force"),
        Some(until.clone())
    );
    assert_eq!(
        w.audit("doc_lint_migration.set").await,
        vec![serde_json::json!({
            "project_id": w.project().get(), "warn_until": until, "previous": null
        })]
    );

    // The same date again: nothing written.
    assert_eq!(
        w.set("req-same", &until).await.expect("same"),
        SetOutcome::Unchanged
    );
    assert_eq!(w.audit("doc_lint_migration.set").await.len(), 1);
    // Another date: updated, the previous one named.
    let later = w.date(180).await;
    assert_eq!(
        w.set("req-later", &later).await.expect("later"),
        SetOutcome::Updated
    );
    let rows = w.audit("doc_lint_migration.set").await;
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1]["previous"], serde_json::json!(until));
    assert_eq!(rows[1]["warn_until"], serde_json::json!(later));
}

#[tokio::test]
async fn a_date_outside_the_bounds_is_refused_by_the_database_date() {
    let w = world().await;
    for (days, reason) in [
        (0, "not_in_future"),
        (-1, "not_in_future"),
        (181, "too_far"),
    ] {
        let until = w.date(days).await;
        let e = w.set("req-bad", &until).await.expect_err("refused");
        assert_eq!(e.code(), ErrorCode::DocLintMigrationUntilInvalid, "{days}");
        let ctx = e.context();
        let get = |k: &str| {
            ctx.iter()
                .find(|(key, _)| *key == k)
                .map(|(_, v)| v.clone())
        };
        assert_eq!(get("reason").as_deref(), Some(reason), "{days}");
        assert_eq!(get("today"), Some(w.date(0).await));
    }
    assert_eq!(
        read(&w.t.db, w.project()).await.expect("read").setting,
        None
    );
    assert!(w.audit("doc_lint_migration.set").await.is_empty());
}

#[tokio::test]
async fn an_expired_date_stops_applying_without_a_write_and_can_be_cleared() {
    let w = world().await;
    let until = w.date(10).await;
    w.set("req-set", &until).await.expect("set");
    // Ten days later, as the database sees it: the row is backdated.
    w.t.commit_audited(
        ("project", w.project().get()),
        &[format!(
            "UPDATE forge.doc_lint_migrations SET set_at = now() - interval '11 days', \
             warn_until = (now() AT TIME ZONE 'UTC')::date - 1 WHERE project_id = {}",
            w.project().get()
        )],
    )
    .await;
    let got = read(&w.t.db, w.project()).await.expect("read");
    let s = got.setting.clone().expect("kept");
    assert!(!s.in_force);
    assert_eq!(got.in_force(), None);
    assert_eq!(
        in_force(&w.t.db, w.project()).await.expect("in force"),
        None
    );

    clear_doc_lint_migration_decided(
        &w.t.db,
        &as_user(w.maintainer, "req-clear"),
        w.project(),
        w.maintainer,
        |_| Ok(()),
    )
    .await
    .expect("clear");
    assert_eq!(
        read(&w.t.db, w.project()).await.expect("read").setting,
        None
    );
    assert_eq!(
        w.audit("doc_lint_migration.cleared").await,
        vec![serde_json::json!({"project_id": w.project().get(), "warn_until": s.warn_until})]
    );
    let e = clear_doc_lint_migration_decided(
        &w.t.db,
        &as_user(w.maintainer, "req-clear-again"),
        w.project(),
        w.maintainer,
        |_| Ok(()),
    )
    .await
    .expect_err("none");
    assert_eq!(e.code(), ErrorCode::NotFound);
}

#[tokio::test]
async fn a_refused_decision_writes_nothing() {
    let w = world().await;
    let until = w.date(5).await;
    let e = set_doc_lint_migration_decided(
        &w.t.db,
        &as_user(w.maintainer, "req-refused"),
        w.project(),
        w.maintainer,
        &until,
        |_| -> Result<(), ForgeError> { Err(ForgeError::new(ErrorCode::Forbidden)) },
    )
    .await
    .expect_err("refused");
    assert_eq!(e.code(), ErrorCode::Forbidden);
    assert_eq!(
        read(&w.t.db, w.project()).await.expect("read").setting,
        None
    );
}

#[tokio::test]
async fn the_table_refuses_a_date_outside_the_bounds_of_the_day_it_was_set() {
    let w = world().await;
    for days in [0, 181] {
        let mut tx = w.t.db.write_pool().begin().await.expect("begin");
        let refused = sqlx::query(
            "INSERT INTO forge.doc_lint_migrations (project_id, warn_until, set_by) \
             VALUES ($1, (now() AT TIME ZONE 'UTC')::date + $2, $3)",
        )
        .bind(w.project().get())
        .bind(days)
        .bind(w.maintainer.get())
        .execute(&mut *tx)
        .await;
        assert!(refused.is_err(), "{days}");
    }
}
