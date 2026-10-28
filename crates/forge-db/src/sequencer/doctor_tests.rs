//! What `forge doctor` reports about the sequencer: a weakened
//! forge.assign_seq (db.bootstrap) and the sequencer lag (FG-DB-0005).

use forge_errors::ErrorCode;

use super::tests::insert_events;
use super::*;
use crate::bootstrap;
use crate::test_support::*;

const SIGNATURE: &str = "forge.assign_seq(integer, boolean)";

/// The function's definition with `from` replaced by `to`, re-created by
/// the superuser (CREATE OR REPLACE keeps the owner).
async fn replaced_body(t: &FreshDb, from: &str, to: &str) -> String {
    let def: String = sqlx::query_scalar(
        "SELECT pg_get_functiondef('forge.assign_seq(integer,boolean)'::regprocedure)",
    )
    .fetch_one(&mut t.superuser().await)
    .await
    .expect("definition");
    assert!(def.contains(from), "{def}");
    def.replacen(from, to, 1)
}

#[tokio::test]
async fn a_weakened_assign_seq_is_reported() {
    let _lock = ClusterLock::shared().await;
    let cases: Vec<(String, &str)> = vec![
        ("SELECT 1".to_owned(), ""),
        (
            format!("ALTER FUNCTION {SIGNATURE} OWNER TO forge_owner"),
            "guard=function_owner",
        ),
        (
            format!("ALTER FUNCTION {SIGNATURE} SECURITY INVOKER"),
            "guard=function_body",
        ),
        (
            format!("ALTER FUNCTION {SIGNATURE} SET search_path = forge, pg_catalog, pg_temp"),
            "guard=function_body",
        ),
        (
            format!("GRANT EXECUTE ON FUNCTION {SIGNATURE} TO forge_ro"),
            "guard=function_privilege",
        ),
        (
            format!("GRANT EXECUTE ON FUNCTION {SIGNATURE} TO PUBLIC"),
            "guard=function_privilege",
        ),
        (
            format!("REVOKE EXECUTE ON FUNCTION {SIGNATURE} FROM forge_app"),
            "guard=function_privilege",
        ),
        (
            "CREATE FUNCTION forge.assign_seq(integer) RETURNS void LANGUAGE sql AS 'SELECT'"
                .to_owned(),
            "guard=function_owner",
        ),
    ];
    for (tamper, guard) in cases {
        let t = FreshDb::migrated().await;
        t.exec_superuser(&tamper).await;
        let result = bootstrap::check(&t.db, &[]).await;
        if guard.is_empty() {
            result.expect("an untouched assign_seq passes");
        } else {
            let text = result.expect_err(&tamper).to_string();
            assert!(text.contains(guard), "{tamper}\n=> {text}");
            assert!(text.contains("function=assign_seq"), "{tamper}\n=> {text}");
        }
        t.drop_now().await;
    }
    // A changed body (a larger batch bound) with everything else intact.
    let t = FreshDb::migrated().await;
    let tampered = replaced_body(&t, "max_batch > 1000", "max_batch > 5000").await;
    t.exec_superuser(&tampered).await;
    let text = bootstrap::check(&t.db, &[])
        .await
        .expect_err("tampered body")
        .to_string();
    assert!(text.contains("guard=function_body"), "{text}");
    t.drop_now().await;
}

#[tokio::test]
async fn before_the_migration_there_is_no_backlog_to_report() {
    let t = FreshDb::new().await;
    assert_eq!(backlog(&t.db).await.expect("backlog"), None);
    t.drop_now().await;
}

#[tokio::test]
async fn a_stale_sequencer_with_a_backlog_is_outbox_sequencer_lag() {
    let t = FreshDb::migrated().await;
    // No sequencer has ever run: events wait and there is no heartbeat.
    insert_events(&t, 2).await;
    let b = backlog(&t.db).await.expect("backlog").expect("migrated");
    assert_eq!(b.unsequenced_events, 2);
    assert_eq!(b.heartbeat_age_ms, None);
    let err = b.check_lag().expect_err("never ran");
    assert_eq!(err.code(), ErrorCode::OutboxSequencerLag);
    assert!(err.to_string().contains("unsequenced_count=2"), "{err}");
    // It runs: nothing waits.
    run_once(&t.db, BatchSize::MAX, AuditSequencing::Skip)
        .await
        .expect("run");
    let b = backlog(&t.db).await.expect("backlog").expect("migrated");
    assert_eq!(b.unsequenced_events, 0);
    b.check_lag().expect("caught up");
    // A backlog with a fresh heartbeat is a sequencer at work, not lag.
    insert_events(&t, 1).await;
    let b = backlog(&t.db).await.expect("backlog").expect("migrated");
    assert_eq!(b.unsequenced_events, 1);
    b.check_lag().expect("fresh heartbeat");
    // It stopped 1 minute ago.
    t.exec_superuser(
        "UPDATE forge.event_feed_state SET sequencer_heartbeat = now() - interval '1 minute'",
    )
    .await;
    let b = backlog(&t.db).await.expect("backlog").expect("migrated");
    let err = b.check_lag().expect_err("stale");
    assert_eq!(err.code(), ErrorCode::OutboxSequencerLag);
    let text = err.to_string();
    assert!(text.contains("unsequenced_count=1"), "{text}");
    let age: i64 = text
        .split("heartbeat_age_ms=")
        .nth(1)
        .and_then(|rest| rest.split(|c: char| !c.is_ascii_digit()).next())
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("no heartbeat_age_ms in {text}"));
    assert!(age >= 60_000, "{text}");
    assert_eq!(b.heartbeat_age_ms, Some(age));
    t.drop_now().await;
}

#[tokio::test]
async fn lock_holder_pid_names_the_session_holding_the_sequencer_lock() {
    let t = FreshDb::migrated().await;
    let b = backlog(&t.db).await.expect("backlog").expect("migrated");
    assert_eq!(b.lock_holder_pid, None);
    let mut holder = t.superuser().await;
    let mut tx = holder.begin().await.expect("begin");
    let pid: i32 = sqlx::query_scalar(
        "SELECT pg_backend_pid() FROM (SELECT pg_advisory_xact_lock(1718579815, 1)) AS l",
    )
    .fetch_one(&mut *tx)
    .await
    .expect("hold the sequencer lock");
    let b = backlog(&t.db).await.expect("backlog").expect("migrated");
    assert_eq!(b.lock_holder_pid, Some(pid));
    // Another advisory lock (same first key, other second key) is not it.
    tx.commit().await.expect("release");
    let mut tx = holder.begin().await.expect("begin");
    sqlx::query("SELECT pg_advisory_xact_lock(1718579815, 2)")
        .execute(&mut *tx)
        .await
        .expect("another lock");
    let b = backlog(&t.db).await.expect("backlog").expect("migrated");
    assert_eq!(b.lock_holder_pid, None);
    tx.commit().await.expect("release");
    let b = backlog(&t.db).await.expect("backlog").expect("migrated");
    assert_eq!(b.lock_holder_pid, None);
    // It reaches the lag error too.
    insert_events(&t, 1).await;
    let mut tx = holder.begin().await.expect("begin");
    sqlx::query("SELECT pg_advisory_xact_lock(1718579815, 1)")
        .execute(&mut *tx)
        .await
        .expect("hold again");
    let err = backlog(&t.db)
        .await
        .expect("backlog")
        .expect("migrated")
        .check_lag()
        .expect_err("never ran, rows wait");
    assert!(
        err.to_string().contains(&format!("lock_holder_pid={pid}")),
        "{err}"
    );
    tx.commit().await.expect("release");
    t.drop_now().await;
}

#[test]
fn counts_above_the_cap_are_shown_as_a_floor() {
    assert_eq!(shown(5), "5");
    assert_eq!(shown(COUNT_CAP), "100000");
    assert_eq!(shown(COUNT_CAP + 1), "100000+");
}

#[tokio::test]
async fn audit_rows_count_only_while_audit_sequencing_is_on() {
    let t = FreshDb::migrated().await;
    run_once(&t.db, BatchSize::MAX, AuditSequencing::Skip)
        .await
        .expect("run");
    t.exec_superuser(
        "UPDATE forge.event_feed_state SET sequencer_heartbeat = now() - interval '1 minute'",
    )
    .await;
    // The migrations' audit rows wait, but audit sequencing is off on purpose.
    let b = backlog(&t.db).await.expect("backlog").expect("migrated");
    assert_eq!(b.unsequenced_audit, None);
    assert_eq!(b.unsequenced(), 0);
    b.check_lag().expect("audit sequencing is off");
    t.exec_superuser("UPDATE forge.event_feed_state SET audit_sequencing_enabled = true")
        .await;
    let b = backlog(&t.db).await.expect("backlog").expect("migrated");
    assert!(b.unsequenced_audit.is_some_and(|n| n > 0), "{b:?}");
    let err = b.check_lag().expect_err("audit rows wait");
    assert_eq!(err.code(), ErrorCode::OutboxSequencerLag);
    t.drop_now().await;
}
