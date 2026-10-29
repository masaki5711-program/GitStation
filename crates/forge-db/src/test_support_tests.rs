//! `FreshDb` cleans up after itself: its database is dropped when the value
//! goes away, whether or not the test called `drop_now` and also when the
//! test panics (so `forge_t_*` databases no longer pile up).

use crate::test_support::*;

async fn exists(name: &str) -> bool {
    let mut admin = superuser(&maintenance_db()).await;
    sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_database WHERE datname = $1)")
        .bind(name)
        .fetch_one(&mut admin)
        .await
        .expect("pg_database")
}

#[tokio::test]
async fn a_fresh_db_is_dropped_without_drop_now() {
    let t = FreshDb::new().await;
    let name = t.name.clone();
    assert!(exists(&name).await, "created");
    drop(t);
    assert!(!exists(&name).await, "{name} must be gone");
}

#[tokio::test]
async fn drop_now_drops_the_database_before_it_returns() {
    let t = FreshDb::new().await;
    let name = t.name.clone();
    t.drop_now().await;
    assert!(!exists(&name).await, "{name} must be gone");
}

#[tokio::test]
async fn a_panicking_test_leaves_no_database_behind() {
    // The failing "test" runs on its own thread and runtime, like a test of
    // its own; its panic unwinds through the FreshDb.
    let (tx, rx) = std::sync::mpsc::channel();
    let test = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        rt.block_on(async {
            let t = FreshDb::new().await;
            tx.send(t.name.clone()).expect("send name");
            panic!("a failing test");
        })
    });
    assert!(test.join().is_err(), "the test panicked");
    let name = rx.recv().expect("the test created its database");
    assert!(!exists(&name).await, "{name} must be gone");
}
