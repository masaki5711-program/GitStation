//! D5 MR3: `delete_decided` / `restore_decided` — the grace period, the
//! decision inside the transaction, the events with their dedup keys and
//! `FG-REPO-0019`.

use std::time::Duration;

use forge_core::{EventType, Role};

use super::*;
use crate::audited_tests::sql;

const GRACE: Duration = Duration::from_secs(3 * 86_400);

fn deny() -> ForgeError {
    ForgeError::new(ErrorCode::Forbidden).with_context("reason", "test_denied")
}

type Row = (bool, bool, Option<i64>);

/// `deleted_at` set, `purge_after` set, `deleted_by` of the project.
async fn marks(su: &mut PgConnection, project: ProjectId) -> Row {
    sqlx::query_as(
        "SELECT deleted_at IS NOT NULL, purge_after IS NOT NULL, deleted_by \
         FROM forge.projects WHERE id = $1",
    )
    .bind(project.get())
    .fetch_one(su)
    .await
    .expect("project")
}

async fn rows(su: &mut PgConnection, action: &str) -> i64 {
    count(
        su,
        &format!("SELECT count(*) FROM forge.audit_log WHERE action = '{action}'"),
    )
    .await
}

#[tokio::test]
async fn delete_sets_the_grace_period_and_publishes_one_integration_event() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let web = create_ok(&t, &project("web", alice.namespace_id, alice.user_id)).await;
    let (deletion, seen) = delete_decided(
        &t.db,
        &ctx("req-delete"),
        web.project_id,
        alice.user_id,
        GRACE,
        |facts| Ok(*facts),
    )
    .await
    .expect("delete");
    // decide saw the locked facts: a live project, alice its Owner.
    assert_eq!(seen.project.project, web.project_id);
    assert!(!seen.project.access.pending_deletion);
    assert_eq!(seen.project.access.role, Some(Role::Owner));
    assert_eq!(deletion.deleted_by, Some(alice.user_id));
    let mut su = t.superuser().await;
    assert_eq!(
        marks(&mut su, web.project_id).await,
        (true, true, Some(alice.user_id.get()))
    );
    // purge_after = deleted_at + the grace period, exactly.
    let secs: i64 = sqlx::query_scalar(
        "SELECT (extract(epoch FROM purge_after - deleted_at))::bigint \
         FROM forge.projects WHERE id = $1",
    )
    .bind(web.project_id.get())
    .fetch_one(&mut su)
    .await
    .expect("grace");
    assert_eq!(secs, 3 * 86_400);
    let audit: (String, Option<i64>, serde_json::Value) = sqlx::query_as(
        "SELECT resource_type, project_id, details FROM forge.audit_log \
         WHERE action = 'project.deleted'",
    )
    .fetch_one(&mut su)
    .await
    .expect("audit row");
    assert_eq!(audit.0, "project");
    assert_eq!(audit.1, Some(web.project_id.get()));
    assert_eq!(
        audit.2,
        json!({"project_id": web.project_id.get(), "deleted_at": deletion.deleted_at,
               "purge_after": deletion.purge_after, "deleted_by": alice.user_id.get()})
    );
    let event: (String, Option<i64>, serde_json::Value, String) = sqlx::query_as(
        "SELECT event_type, project_id, payload, dedup_key FROM forge.events \
         WHERE event_type = 'project.deleted'",
    )
    .fetch_one(&mut su)
    .await
    .expect("event");
    assert_eq!(event.0, EventType::ProjectDeleted.as_str());
    assert_eq!(event.1, Some(web.project_id.get()));
    assert_eq!(event.2, audit.2);
    let epoch: i64 = sqlx::query_scalar(
        "SELECT floor(extract(epoch FROM deleted_at) * 1000000)::bigint \
         FROM forge.projects WHERE id = $1",
    )
    .bind(web.project_id.get())
    .fetch_one(&mut su)
    .await
    .expect("epoch");
    assert_eq!(
        event.3,
        format!("project_deleted:{}:{epoch}", web.project_id.get())
    );
    t.drop_now().await;
}

#[tokio::test]
async fn a_refusing_decision_an_absent_project_or_a_second_delete_writes_nothing() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let web = create_ok(&t, &project("web", alice.namespace_id, alice.user_id)).await;
    let err = delete_decided(
        &t.db,
        &ctx("req-denied"),
        web.project_id,
        alice.user_id,
        GRACE,
        |_| Err::<(), _>(deny()),
    )
    .await
    .expect_err("denied");
    assert_eq!(err.code(), ErrorCode::Forbidden);
    let err = delete_decided(
        &t.db,
        &ctx("req-absent"),
        ProjectId::new(9999),
        alice.user_id,
        GRACE,
        |_| Ok(()),
    )
    .await
    .expect_err("absent");
    assert_reason(&err, ErrorCode::NotFound, "absent");
    let mut su = t.superuser().await;
    assert_eq!(marks(&mut su, web.project_id).await, (false, false, None));
    assert_eq!(rows(&mut su, "project.deleted").await, 0);
    // A deleted project cannot be deleted again, even when decide lets it
    // through (authorize denies with FG-REPO-0006 first): no second row.
    delete_decided(
        &t.db,
        &ctx("req-first"),
        web.project_id,
        alice.user_id,
        GRACE,
        |_| Ok(()),
    )
    .await
    .expect("first");
    let err = delete_decided(
        &t.db,
        &ctx("req-second"),
        web.project_id,
        alice.user_id,
        GRACE,
        |facts| {
            assert!(facts.project.access.pending_deletion, "decide sees it");
            Ok(())
        },
    )
    .await
    .expect_err("second");
    assert_eq!(err.code(), ErrorCode::PendingDeletion);
    assert_eq!(rows(&mut su, "project.deleted").await, 1);
    assert_eq!(
        count(
            &mut su,
            "SELECT count(*) FROM forge.events WHERE event_type = 'project.deleted'"
        )
        .await,
        1
    );
    t.drop_now().await;
}

#[tokio::test]
async fn restore_clears_the_deletion_and_publishes_one_integration_event() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let web = create_ok(&t, &project("web", alice.namespace_id, alice.user_id)).await;
    let (deleted, ()) = delete_decided(
        &t.db,
        &ctx("req-delete"),
        web.project_id,
        alice.user_id,
        GRACE,
        |_| Ok(()),
    )
    .await
    .expect("delete");
    // A refusing decision leaves the deletion as it is.
    let err = restore_decided(
        &t.db,
        &ctx("req-denied"),
        web.project_id,
        alice.user_id,
        |_| Err::<(), _>(deny()),
    )
    .await
    .expect_err("denied");
    assert_eq!(err.code(), ErrorCode::Forbidden);
    let mut su = t.superuser().await;
    assert_eq!(
        marks(&mut su, web.project_id).await,
        (true, true, Some(alice.user_id.get()))
    );
    let (restored, seen) = restore_decided(
        &t.db,
        &ctx("req-restore"),
        web.project_id,
        alice.user_id,
        |facts| Ok(facts.project.access),
    )
    .await
    .expect("restore");
    // The project row is loaded although it is pending deletion, and
    // decide sees that.
    assert!(seen.pending_deletion);
    assert_eq!(seen.role, Some(Role::Owner));
    assert_eq!(restored, deleted, "the cleared deletion is handed back");
    assert_eq!(marks(&mut su, web.project_id).await, (false, false, None));
    let event: (Option<i64>, serde_json::Value, String) = sqlx::query_as(
        "SELECT project_id, payload, dedup_key FROM forge.events \
         WHERE event_type = 'project.restored'",
    )
    .fetch_one(&mut su)
    .await
    .expect("event");
    assert_eq!(event.0, Some(web.project_id.get()));
    assert_eq!(event.1["deleted_at"], json!(deleted.deleted_at));
    assert_eq!(event.1["deleted_by"], json!(alice.user_id.get()));
    assert!(
        event
            .2
            .starts_with(&format!("project_restored:{}:", web.project_id.get())),
        "{}",
        event.2
    );
    assert_eq!(rows(&mut su, "project.restored").await, 1);
    // delete -> restore -> delete in quick succession publishes each time
    // (the dedup key carries the deletion's own time).
    delete_decided(
        &t.db,
        &ctx("req-delete2"),
        web.project_id,
        alice.user_id,
        GRACE,
        |_| Ok(()),
    )
    .await
    .expect("delete again");
    assert_eq!(rows(&mut su, "project.deleted").await, 2);
    t.drop_now().await;
}

#[tokio::test]
async fn restoring_a_project_that_is_not_deleted_or_is_purging_is_restore_unavailable() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let web = create_ok(&t, &project("web", alice.namespace_id, alice.user_id)).await;
    // Not deleted.
    let err = restore_decided(
        &t.db,
        &ctx("req-live"),
        web.project_id,
        alice.user_id,
        |_| Ok(()),
    )
    .await
    .expect_err("not deleted");
    assert_reason(&err, ErrorCode::RestoreUnavailable, "not_deleted");
    // A refusing decision comes before the answer about the state.
    let err = restore_decided(
        &t.db,
        &ctx("req-live-denied"),
        web.project_id,
        alice.user_id,
        |_| Err::<(), _>(deny()),
    )
    .await
    .expect_err("denied first");
    assert_eq!(err.code(), ErrorCode::Forbidden);
    // Deleted, and the purge has started: the repository is `purging`.
    delete_decided(
        &t.db,
        &ctx("req-delete"),
        web.project_id,
        alice.user_id,
        GRACE,
        |_| Ok(()),
    )
    .await
    .expect("delete");
    let mut su = t.superuser().await;
    commit(
        &t,
        &[
            audit("project", web.project_id.get(), "succeeded"),
            sql(&format!(
                "UPDATE forge.repositories SET state = 'purging' WHERE project_id = {}",
                web.project_id.get()
            )),
        ],
    )
    .await
    .expect("purging");
    let err = restore_decided(
        &t.db,
        &ctx("req-purging"),
        web.project_id,
        alice.user_id,
        |_| Ok(()),
    )
    .await
    .expect_err("purging");
    assert_reason(&err, ErrorCode::RestoreUnavailable, "purging");
    // Nothing changed: still pending deletion, no restore audit row/event.
    assert_eq!(
        marks(&mut su, web.project_id).await,
        (true, true, Some(alice.user_id.get()))
    );
    assert_eq!(rows(&mut su, "project.restored").await, 0);
    assert_eq!(
        count(
            &mut su,
            "SELECT count(*) FROM forge.events WHERE event_type = 'project.restored'"
        )
        .await,
        0
    );
    t.drop_now().await;
}

/// A purge start that has written `purging` but not committed yet: the
/// restore waits on the repository row (`FOR UPDATE`) and then answers
/// `purging` — it never restores a project whose purge won the race.
#[tokio::test]
async fn a_restore_waits_for_a_purge_start_in_flight_and_loses_to_it() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let web = create_ok(&t, &project("web", alice.namespace_id, alice.user_id)).await;
    delete_decided(
        &t.db,
        &ctx("req-delete"),
        web.project_id,
        alice.user_id,
        GRACE,
        |_| Ok(()),
    )
    .await
    .expect("delete");
    let mut su = t.superuser().await;
    let mut purge = t.db.write_pool().begin().await.expect("begin");
    sqlx::raw_sql(&audit("project", web.project_id.get(), "succeeded"))
        .execute(&mut *purge)
        .await
        .expect("audit row");
    sqlx::query("UPDATE forge.repositories SET state = 'purging' WHERE project_id = $1")
        .bind(web.project_id.get())
        .execute(&mut *purge)
        .await
        .expect("purge start, uncommitted");
    let db = t.db.clone();
    let (id, user_id) = (web.project_id, alice.user_id);
    let restore = tokio::spawn(async move {
        restore_decided(&db, &ctx("req-restore"), id, user_id, |_| Ok(())).await
    });
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    assert!(
        !restore.is_finished(),
        "the restore waits for the purge start"
    );
    purge.commit().await.expect("commit");
    let err = restore.await.expect("join").expect_err("lost the race");
    assert_reason(&err, ErrorCode::RestoreUnavailable, "purging");
    assert_eq!(
        marks(&mut su, web.project_id).await,
        (true, true, Some(alice.user_id.get()))
    );
    t.drop_now().await;
}
