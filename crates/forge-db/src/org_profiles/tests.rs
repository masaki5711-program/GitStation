//! The org claims' snapshot against a migrated database (ADR-0021
//! amendment 2026-11-20): the first snapshot and a change are audited once,
//! an equal one moves `confirmed_at` only, invalid claims keep the last
//! valid values and set `invalid_since` once, and the reads. Synthetic
//! codes only.

use forge_core::RequestId;

use super::*;
use crate::change::Actor;
use crate::test_support::*;
use crate::users::{self, NewUser};

const ISSUER: &str = "http://127.0.0.1:58080/realms/synthetic";

fn ctx(actor: Actor) -> ChangeContext {
    ChangeContext::new(actor, RequestId::generate())
}

async fn person(t: &FreshDb, name: &str) -> UserId {
    let new = NewUser::human(name, name, &format!("{name}@example.test"));
    users::create(&t.db, &ctx(Actor::System), &new)
        .await
        .expect("create user")
        .user_id
}

fn snapshot(primary: &str, other: Option<&str>) -> OrgSnapshot {
    let mut org_units = vec![OrgUnit {
        code: primary.to_owned(),
        primary: true,
    }];
    if let Some(o) = other {
        org_units.push(OrgUnit {
            code: o.to_owned(),
            primary: false,
        });
    }
    OrgSnapshot {
        org_units,
        positions: vec!["member".to_owned()],
        position_rank: Some(3),
        plants: vec!["P001".to_owned()],
        plant_code: Some("P001".to_owned()),
    }
}

async fn changes(t: &FreshDb, user: UserId) -> Vec<serde_json::Value> {
    t.json_rows(&format!(
        "SELECT actor_kind, details FROM forge.audit_log \
         WHERE action = 'user.org_profile_changed' AND resource_id = {} ORDER BY id",
        user.get()
    ))
    .await
}

async fn row(t: &FreshDb, user: UserId) -> serde_json::Value {
    let rows = t
        .json_rows(&format!(
            "SELECT org_units, positions, position_rank, plants, plant_code, digest, \
             confirmed_at, changed_at, invalid_since FROM forge.user_org_profiles \
             WHERE user_id = {}",
            user.get()
        ))
        .await;
    rows.into_iter().next().expect("a profile row")
}

#[tokio::test]
async fn the_first_snapshot_and_a_change_are_audited_and_an_equal_one_is_not() {
    let t = FreshDb::migrated().await;
    let alice = person(&t, "alice").await;
    let first = snapshot("SYN-A-0001", Some("SYN-B-0002"));
    let done = record(
        &t.db,
        &ctx(Actor::User(alice)),
        alice,
        ISSUER,
        Observed::Valid(&first),
    )
    .await
    .expect("recorded");
    assert_eq!(done, Recorded::Created);
    let audits = changes(&t, alice).await;
    assert_eq!(audits.len(), 1, "{audits:?}");
    assert_eq!(audits[0]["actor_kind"], "user");
    assert_eq!(audits[0]["details"]["before"], serde_json::Value::Null);
    assert_eq!(
        audits[0]["details"]["after"]["org_units"][0]["code"],
        "SYN-A-0001"
    );
    let stored = row(&t, alice).await;
    assert_eq!(stored["positions"][0], "member");
    assert_eq!(stored["digest"], first.digest().expect("digest"));
    assert_eq!(stored["invalid_since"], serde_json::Value::Null);

    // The same values: confirmed_at only, no audit row.
    t.exec_superuser(&format!(
        "UPDATE forge.user_org_profiles SET confirmed_at = now() - interval '1 hour' \
         WHERE user_id = {}",
        alice.get()
    ))
    .await;
    let done = record(
        &t.db,
        &ctx(Actor::System),
        alice,
        ISSUER,
        Observed::Valid(&first),
    )
    .await
    .expect("confirmed");
    assert_eq!(done, Recorded::Unchanged);
    assert_eq!(changes(&t, alice).await.len(), 1);
    let fresh = t
        .json_rows(&format!(
            "SELECT confirmed_at > now() - interval '1 minute' AS fresh \
             FROM forge.user_org_profiles WHERE user_id = {}",
            alice.get()
        ))
        .await;
    assert_eq!(fresh[0]["fresh"], true);

    // A change: one audit row with the changed field only.
    let moved = snapshot("SYN-C-0003", Some("SYN-B-0002"));
    let done = record(
        &t.db,
        &ctx(Actor::System),
        alice,
        ISSUER,
        Observed::Valid(&moved),
    )
    .await
    .expect("changed");
    assert_eq!(done, Recorded::Changed);
    let audits = changes(&t, alice).await;
    assert_eq!(audits.len(), 2);
    let details = &audits[1]["details"];
    assert_eq!(details["fields"], serde_json::json!(["org_units"]));
    assert_eq!(details["before"]["org_units"][0]["code"], "SYN-A-0001");
    assert_eq!(details["after"]["org_units"][0]["code"], "SYN-C-0003");
    assert!(details["before"].get("positions").is_none(), "{details}");
    t.drop_now().await;
}

#[tokio::test]
async fn invalid_claims_keep_the_last_valid_values_and_mark_once() {
    let t = FreshDb::migrated().await;
    let bob = person(&t, "bob").await;
    let good = snapshot("SYN-A-0001", None);
    let _created = record(
        &t.db,
        &ctx(Actor::User(bob)),
        bob,
        ISSUER,
        Observed::Valid(&good),
    )
    .await
    .expect("recorded");
    let first = record(&t.db, &ctx(Actor::System), bob, ISSUER, Observed::Invalid)
        .await
        .expect("invalid");
    assert_eq!(first, Recorded::Invalid { newly: true });
    let again = record(&t.db, &ctx(Actor::System), bob, ISSUER, Observed::Invalid)
        .await
        .expect("still invalid");
    assert_eq!(again, Recorded::Invalid { newly: false });
    let stored = row(&t, bob).await;
    assert_eq!(stored["org_units"][0]["code"], "SYN-A-0001", "kept");
    assert_ne!(stored["invalid_since"], serde_json::Value::Null);
    assert_eq!(changes(&t, bob).await.len(), 1, "invalid is not audited");
    let health = health(&t.db).await.expect("health");
    assert_eq!((health.profiles, health.invalid), (1, 1));
    assert!(health.oldest_confirmed_at.is_some());

    // Valid again: invalid_since clears (the same values: no audit row).
    let done = record(
        &t.db,
        &ctx(Actor::System),
        bob,
        ISSUER,
        Observed::Valid(&good),
    )
    .await
    .expect("valid again");
    assert_eq!(done, Recorded::Unchanged);
    assert_eq!(row(&t, bob).await["invalid_since"], serde_json::Value::Null);
    t.drop_now().await;
}

#[tokio::test]
async fn invalid_claims_without_a_snapshot_write_an_empty_one() {
    let t = FreshDb::migrated().await;
    let carol = person(&t, "carol").await;
    let done = record(
        &t.db,
        &ctx(Actor::User(carol)),
        carol,
        ISSUER,
        Observed::Invalid,
    )
    .await
    .expect("recorded");
    assert_eq!(done, Recorded::Invalid { newly: true });
    let stored = row(&t, carol).await;
    assert_eq!(stored["org_units"], serde_json::json!([]));
    assert_ne!(stored["invalid_since"], serde_json::Value::Null);
    let audits = changes(&t, carol).await;
    assert_eq!(audits.len(), 1);
    assert_eq!(audits[0]["details"]["invalid"], true);
    let profile = read(&t.db, carol).await.expect("read").expect("a profile");
    assert!(profile.invalid_since.is_some());
    assert_eq!(profile.snapshot, OrgSnapshot::default());
    t.drop_now().await;
}

#[tokio::test]
async fn the_reads_show_the_whole_snapshot_and_the_primary_codes() {
    let t = FreshDb::migrated().await;
    let alice = person(&t, "alice").await;
    let bob = person(&t, "bob").await;
    let dave = person(&t, "dave").await;
    let s = snapshot("SYN-A-0001", Some("SYN-B-0002"));
    let _a = record(
        &t.db,
        &ctx(Actor::User(alice)),
        alice,
        ISSUER,
        Observed::Valid(&s),
    )
    .await
    .expect("alice");
    let no_primary = OrgSnapshot {
        org_units: vec![OrgUnit {
            code: "SYN-B-0002".to_owned(),
            primary: false,
        }],
        ..OrgSnapshot::default()
    };
    let _b = record(
        &t.db,
        &ctx(Actor::User(bob)),
        bob,
        ISSUER,
        Observed::Valid(&no_primary),
    )
    .await
    .expect("bob");
    let profile = read(&t.db, alice).await.expect("read").expect("alice's");
    assert_eq!(profile.snapshot, s);
    assert_eq!(profile.snapshot.primary_code(), Some("SYN-A-0001"));
    assert!(read(&t.db, dave).await.expect("read").is_none());
    let codes = primary_codes(&t.db, &[alice, bob, dave])
        .await
        .expect("codes");
    assert_eq!(codes.len(), 1, "{codes:?}");
    assert_eq!(codes.get(&alice).map(String::as_str), Some("SYN-A-0001"));
    assert!(primary_codes(&t.db, &[]).await.expect("none").is_empty());
    t.drop_now().await;
}

#[tokio::test]
async fn a_large_change_keeps_the_digests_only() {
    let t = FreshDb::migrated().await;
    let erin = person(&t, "erin").await;
    let small = snapshot("SYN-A-0001", None);
    let _first = record(
        &t.db,
        &ctx(Actor::User(erin)),
        erin,
        ISSUER,
        Observed::Valid(&small),
    )
    .await
    .expect("first");
    let big = OrgSnapshot {
        org_units: (0..MAX_ENTRIES)
            .map(|i| OrgUnit {
                code: format!("SYN-{i:02}-{}", "9".repeat(13)),
                primary: i == 0,
            })
            .collect(),
        positions: (0..MAX_ENTRIES)
            .map(|i| format!("p{i:02}{}", "x".repeat(60)))
            .collect(),
        ..small.clone()
    };
    let done = record(
        &t.db,
        &ctx(Actor::System),
        erin,
        ISSUER,
        Observed::Valid(&big),
    )
    .await
    .expect("changed");
    assert_eq!(done, Recorded::Changed);
    let audits = changes(&t, erin).await;
    let details = &audits[1]["details"];
    assert_eq!(details["values_omitted"], true, "{details}");
    assert!(details.get("after").is_none());
    assert_eq!(details["digest_after"], big.digest().expect("digest"));
    t.drop_now().await;
}

#[test]
fn the_digest_follows_the_values() {
    let a = snapshot("SYN-A-0001", None);
    let b = snapshot("SYN-A-0002", None);
    let da = a.digest().expect("a");
    assert_eq!(da.len(), 64);
    assert_eq!(da, a.clone().digest().expect("again"));
    assert_ne!(da, b.digest().expect("b"));
}
