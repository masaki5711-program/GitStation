//! What `forge doctor` sees: a weakened derived-event check (db.bootstrap),
//! a stuck subscriber with its lag (subscribers.lag) and quarantined events
//! (subscribers.dead_letters).

use std::sync::atomic::Ordering as Atomic;

use forge_errors::ErrorCode;

use super::tests::{published, step, Scripted};
use super::*;
use crate::bootstrap;
use crate::test_support::*;

#[tokio::test]
async fn a_tampered_derived_event_check_is_reported() {
    let _lock = ClusterLock::shared().await;
    let cases = [
        (
            "DROP TRIGGER events_lineage ON forge.events",
            "trigger=events_lineage",
        ),
        (
            "ALTER TABLE forge.events DISABLE TRIGGER events_derived_progress",
            "trigger=events_derived_progress",
        ),
        // The audit-presence check must exempt derived events only.
        (
            "DROP TRIGGER events_audit_present ON forge.events; \
             CREATE CONSTRAINT TRIGGER events_audit_present AFTER INSERT ON forge.events \
             DEFERRABLE INITIALLY DEFERRED FOR EACH ROW \
             EXECUTE FUNCTION forge.events_audit_present(); \
             ALTER TABLE forge.events ENABLE ALWAYS TRIGGER events_audit_present",
            "trigger=events_audit_present",
        ),
        (
            "CREATE OR REPLACE FUNCTION forge.events_derived_progress() RETURNS trigger \
             LANGUAGE plpgsql SET search_path = pg_catalog, pg_temp \
             AS $$ BEGIN RETURN NULL; END $$",
            "function=events_derived_progress",
        ),
    ];
    for (tamper, context) in cases {
        let t = FreshDb::migrated().await;
        bootstrap::check(&t.db, &[])
            .await
            .expect("the shipped checks pass");
        t.exec_superuser(tamper).await;
        let err = bootstrap::check(&t.db, &[]).await.expect_err(tamper);
        assert_reason(&err, ErrorCode::AppendOnlyViolation, "guard_missing");
        assert!(err.to_string().contains(context), "{tamper}\n=> {err}");
        t.drop_now().await;
    }
}

#[tokio::test]
async fn doctor_reports_a_stuck_subscriber_and_quarantined_events() {
    let t = FreshDb::migrated().await;
    assert_eq!(states(&t.db).await.expect("states"), Some(Vec::new()));
    let empty = dead_letters(&t.db)
        .await
        .expect("dead letters")
        .expect("migrated");
    assert_eq!((empty.total, empty.last_24h), (0, 0));
    empty.check().expect("nothing quarantined");
    let ordered = Scripted::new("stuck-one", Ordering::Ordered);
    let independent = Scripted::new("lossy", Ordering::Independent);
    for s in [&ordered, &independent] {
        register(&t.db, s.as_ref(), Start::Beginning)
            .await
            .expect("register");
        s.failing.store(true, Atomic::SeqCst);
    }
    published(&t, 3).await;
    for _ in 0..5 {
        step(&t, &ordered).await;
        step(&t, &independent).await;
    }
    let found = states(&t.db).await.expect("states").expect("migrated");
    let stuck = found.iter().find(|s| s.name == "stuck-one").expect("row");
    assert_eq!((stuck.lag_events, stuck.failures), (3, 5));
    assert_eq!(stuck.next_event_type.as_deref(), Some("repository.created"));
    let err = check_lag(&found).expect_err("stuck");
    assert_reason(&err, ErrorCode::SubscriberLag, "stuck");
    assert!(err.to_string().contains("subscriber=stuck-one"), "{err}");
    assert!(err.to_string().contains("lag_events=3"), "{err}");
    let letters = dead_letters(&t.db)
        .await
        .expect("dead letters")
        .expect("migrated");
    assert_eq!((letters.total, letters.last_24h), (1, 1));
    let err = letters.check().expect_err("quarantined");
    assert_reason(&err, ErrorCode::SubscriberLag, "quarantined");
    assert!(err.to_string().contains("subscriber=lossy"), "{err}");
    t.drop_now().await;
}
