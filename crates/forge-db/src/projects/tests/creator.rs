//! Plan 15 MR5: `projects.created_by` — written by every create (the one
//! insert `create` and `create_decided` share; the HTTP route's path is
//! forge-hosting's `create_project`, tested there), read back by the
//! project loaders, and backfilled by migration 20261209143000 from the
//! `project.created` audit rows (unknown creators stay NULL; idempotent).

use sqlx::Connection;

use super::*;
use crate::resolve::load_project;

/// The backfill statement of the migration, verbatim.
fn backfill_sql() -> &'static str {
    include_str!("../../../migrations/20261209143000_project_created_by.sql")
        .split(';')
        .find(|s| s.contains("UPDATE forge.projects"))
        .expect("the backfill statement")
}

async fn created_by(su: &mut PgConnection, project: i64) -> Option<i64> {
    sqlx::query_scalar("SELECT created_by FROM forge.projects WHERE id = $1")
        .bind(project)
        .fetch_one(su)
        .await
        .expect("created_by")
}

/// One superuser transaction: a `probe.changed` audit row for every
/// project (the column is audited since the migration re-created the
/// trigger), then `sql`, then COMMIT. Returns the rows `sql` changed.
async fn audited_su(su: &mut PgConnection, sql: &str) -> u64 {
    let mut tx = su.begin().await.expect("begin");
    sqlx::raw_sql(
        "INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, \
         resource_id, request_id) SELECT 'system', 'probe.changed', 'succeeded', 'project', \
         id, 'r-backfill' FROM forge.projects",
    )
    .execute(&mut *tx)
    .await
    .expect("audit rows");
    let changed = sqlx::raw_sql(sql)
        .execute(&mut *tx)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .rows_affected();
    tx.commit().await.expect("commit");
    changed
}

#[tokio::test]
async fn every_create_writes_the_creator() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let bob = user(&t, "bob").await;
    let acme = group(&t, "acme", None, alice.user_id).await;
    // The operator path (actor `system`) naming the creator.
    let mine = create_ok(&t, &project("mine", alice.namespace_id, alice.user_id)).await;
    let for_bob = create_ok(&t, &project("for-bob", acme.namespace_id, bob.user_id)).await;
    // The decided path (what forge-hosting's route runs), as the user.
    let user_ctx = ChangeContext::new(
        Actor::User(alice.user_id),
        RequestId::parse("req-decided").expect("request id"),
    );
    let (decided, ()) = create_decided(
        &t.db,
        &user_ctx,
        &project("decided", acme.namespace_id, alice.user_id),
        |_| Ok(()),
    )
    .await
    .expect("decided");
    let mut su = t.superuser().await;
    for (p, who) in [
        (mine.project_id, alice.user_id),
        (for_bob.project_id, bob.user_id),
        (decided.project_id, alice.user_id),
    ] {
        assert_eq!(created_by(&mut su, p.get()).await, Some(who.get()), "{p:?}");
        let record = load_project(&t.db, p, Some(alice.user_id))
            .await
            .expect("load")
            .expect("exists");
        assert_eq!(record.created_by, Some(who), "{p:?}");
    }
    // The creator is the user the project.created audit row names: the
    // backfill reads the same fact.
    let named: i64 = sqlx::query_scalar(
        "SELECT (details ->> 'owner_id')::bigint FROM forge.audit_log \
         WHERE action = 'project.created' AND resource_id = $1",
    )
    .bind(for_bob.project_id.get())
    .fetch_one(&mut su)
    .await
    .expect("audit row");
    assert_eq!(named, bob.user_id.get());
    t.drop_now().await;
}

#[tokio::test]
async fn the_backfill_resolves_audited_creators_leaves_unknown_null_and_is_idempotent() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let bob = user(&t, "bob").await;
    let a = create_ok(&t, &project("a", alice.namespace_id, alice.user_id)).await;
    let b = create_ok(&t, &project("b", bob.namespace_id, bob.user_id)).await;
    let (a, b) = (a.project_id.get(), b.project_id.get());
    let mut su = t.superuser().await;
    // Rows from before the column: the creator was never written.
    audited_su(
        &mut su,
        &format!("UPDATE forge.projects SET created_by = NULL WHERE id IN ({a}, {b})"),
    )
    .await;
    // A project with no project.created audit row at all, and one whose
    // row names a user that does not exist: neither can be resolved.
    let (none, ghost) = (990_001_i64, 990_002_i64);
    let mut tx = su.begin().await.expect("begin");
    sqlx::raw_sql(&format!(
        "INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, resource_id, \
         request_id) VALUES ('system', 'probe.changed', 'succeeded', 'project', {none}, 'r-1'); \
         INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, resource_id, \
         request_id, details) VALUES ('system', 'project.created', 'succeeded', 'project', \
         {ghost}, 'r-2', '{{\"owner_id\": 987654321}}'); \
         INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, resource_id, \
         request_id, details) VALUES ('system', 'project.created', 'failed', 'project', \
         {none}, 'r-3', '{{\"owner_id\": {bob}}}'); \
         INSERT INTO forge.projects (id, display_name) OVERRIDING SYSTEM VALUE \
         VALUES ({none}, 'none'), ({ghost}, 'ghost')",
        bob = bob.user_id.get()
    ))
    .execute(&mut *tx)
    .await
    .expect("legacy rows");
    tx.commit().await.expect("commit");
    for p in [a, b, none, ghost] {
        assert_eq!(created_by(&mut su, p).await, None, "before: {p}");
    }

    assert_eq!(audited_su(&mut su, backfill_sql()).await, 2, "a and b");
    assert_eq!(created_by(&mut su, a).await, Some(alice.user_id.get()));
    assert_eq!(created_by(&mut su, b).await, Some(bob.user_id.get()));
    assert_eq!(
        created_by(&mut su, none).await,
        None,
        "no succeeded audit row"
    );
    assert_eq!(created_by(&mut su, ghost).await, None, "unknown user");

    // Idempotent: a second run changes nothing, and never overwrites a
    // value already there.
    assert_eq!(audited_su(&mut su, backfill_sql()).await, 0);
    audited_su(
        &mut su,
        &format!(
            "UPDATE forge.projects SET created_by = {} WHERE id = {b}",
            alice.user_id.get()
        ),
    )
    .await;
    assert_eq!(audited_su(&mut su, backfill_sql()).await, 0);
    assert_eq!(created_by(&mut su, b).await, Some(alice.user_id.get()));
    t.drop_now().await;
}

/// The column is audited like every other projects column: changing it
/// without a `project` audit row fails at COMMIT.
#[tokio::test]
async fn created_by_cannot_change_without_an_audit_row() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let bob = user(&t, "bob").await;
    let a = create_ok(&t, &project("a", alice.namespace_id, alice.user_id)).await;
    let err = crate::audited_tests::commit(
        &t,
        &[format!(
            "UPDATE forge.projects SET created_by = {} WHERE id = {}",
            bob.user_id.get(),
            a.project_id.get()
        )],
    )
    .await
    .expect_err("unaudited change");
    crate::audited_tests::assert_refused(&err, "projects");
    t.drop_now().await;
}
