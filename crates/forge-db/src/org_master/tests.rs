//! The org master's copy against a migrated database (ADR-0038 §1, §4):
//! a sync is one transaction with one audit row when it changed anything,
//! rows no longer listed go, and the derivation walks parents by level and
//! fails closed. Synthetic codes and names only.

use forge_core::RequestId;

use super::*;
use crate::change::Actor;
use crate::org_profiles::{record, Observed, OrgSnapshot, OrgUnit};
use crate::test_support::*;
use crate::users::{self, NewUser};

const ISSUER: &str = "http://127.0.0.1:58080/realms/synthetic";
const DAY: Duration = Duration::from_secs(24 * 3600);
const AGES: MaxAges = MaxAges {
    profile: DAY,
    master: DAY,
};

fn ctx() -> ChangeContext {
    ChangeContext::new(Actor::System, RequestId::generate())
}

fn name(ja: &str) -> LocalizedName {
    LocalizedName {
        ja: ja.to_owned(),
        en: Some(format!("{ja} (en)")),
        id: None,
    }
}

fn unit(code: &str, level: UnitLevel, parent: Option<&str>) -> MasterUnit {
    MasterUnit {
        plant_code: "P001".to_owned(),
        code: code.to_owned(),
        level,
        parent_code: parent.map(str::to_owned),
        name: name(&format!("名前 {code}")),
        cost_center: Some("CC-1".to_owned()),
        head_staff_id: Some("SYN0009".to_owned()),
        valid_from: Some("2026-01-01".to_owned()),
        version: 1,
    }
}

fn position(code: &str, rank: i32) -> MasterPosition {
    MasterPosition {
        plant_code: "P001".to_owned(),
        code: code.to_owned(),
        name: name(code),
        rank,
        sort_order: rank,
        active: true,
        version: 1,
    }
}

/// division DIV-1 → department DEP-1 → section SEC-1 → team TEAM-1.
fn tree() -> Vec<MasterUnit> {
    vec![
        unit("DIV-1", UnitLevel::Division, None),
        unit("DEP-1", UnitLevel::Department, Some("DIV-1")),
        unit("SEC-1", UnitLevel::Section, Some("DEP-1")),
        unit("TEAM-1", UnitLevel::Team, Some("SEC-1")),
    ]
}

async fn synced_audits(t: &FreshDb) -> Vec<serde_json::Value> {
    t.json_rows(
        "SELECT actor_kind, resource_type, details FROM forge.audit_log \
         WHERE action = 'org.master_synced' ORDER BY id",
    )
    .await
}

async fn person(t: &FreshDb, name: &str, primary: &str, plant: Option<&str>) -> UserId {
    let new = NewUser::human(name, name, &format!("{name}@example.test"));
    let user = users::create(&t.db, &ctx(), &new)
        .await
        .expect("create user")
        .user_id;
    let snapshot = OrgSnapshot {
        org_units: vec![OrgUnit {
            code: primary.to_owned(),
            primary: true,
        }],
        positions: vec!["manager".to_owned()],
        position_rank: Some(55),
        plants: plant.iter().map(|p| (*p).to_owned()).collect(),
        plant_code: plant.map(str::to_owned),
    };
    let _ = record(&t.db, &ctx(), user, ISSUER, Observed::Valid(&snapshot))
        .await
        .expect("snapshot");
    user
}

fn known(d: Derived) -> DerivedOrg {
    match d {
        Derived::Known(org) => *org,
        Derived::Unknown(reason) => panic!("unknown: {}", reason.as_str()),
    }
}

#[tokio::test]
async fn a_sync_replaces_the_copy_and_is_audited_only_when_it_changed() {
    let t = FreshDb::migrated().await;
    let first = replace(&t.db, &ctx(), &tree(), &[position("manager", 55)])
        .await
        .expect("first sync");
    assert_eq!(first.units.added, 4);
    assert_eq!(first.units.total, 4);
    assert_eq!(first.positions.added, 1);
    let audits = synced_audits(&t).await;
    assert_eq!(audits.len(), 1, "{audits:?}");
    assert_eq!(audits[0]["actor_kind"], "system");
    assert_eq!(audits[0]["resource_type"], "org_master");
    assert_eq!(audits[0]["details"]["units"]["added"], 4);
    // Counts only: never a name or a staff id.
    let text = audits[0]["details"].to_string();
    assert!(
        !text.contains("SYN0009") && !text.contains("名前"),
        "{text}"
    );

    // The same lists: synced_at moves, no audit row.
    t.exec_superuser("UPDATE forge.org_units_master SET synced_at = now() - interval '2 days'")
        .await;
    let same = replace(&t.db, &ctx(), &tree(), &[position("manager", 55)])
        .await
        .expect("same sync");
    assert!(!same.units.touched() && !same.positions.touched());
    assert_eq!(synced_audits(&t).await.len(), 1);
    let health = health(&t.db).await.expect("health");
    assert_eq!((health.units, health.positions), (4, 1));
    assert!(health.age_secs.is_some_and(|a| a < 60), "{health:?}");

    // A renamed unit, a closed one (no longer listed) and a new position.
    let mut next = tree();
    next.pop();
    next[2].name = name("改名した課");
    let changed = replace(
        &t.db,
        &ctx(),
        &next,
        &[position("manager", 55), position("staff", 15)],
    )
    .await
    .expect("changed sync");
    assert_eq!(
        changed.units,
        Counts {
            added: 0,
            changed: 1,
            removed: 1,
            total: 3
        }
    );
    assert_eq!(changed.positions.added, 1);
    assert_eq!(synced_audits(&t).await.len(), 2);
    let rows = t
        .json_rows("SELECT code, name_ja FROM forge.org_units_master ORDER BY code")
        .await;
    assert_eq!(rows.len(), 3);
    assert!(rows.iter().all(|r| r["code"] != "TEAM-1"));
}

#[tokio::test]
async fn an_empty_or_duplicated_unit_list_writes_nothing() {
    let t = FreshDb::migrated().await;
    replace(&t.db, &ctx(), &tree(), &[])
        .await
        .expect("first sync");
    let empty = replace(&t.db, &ctx(), &[], &[]).await.expect_err("empty");
    assert!(empty.to_string().contains("org_master_empty"), "{empty}");
    let mut twice = tree();
    twice.push(unit("SEC-1", UnitLevel::Section, Some("DEP-1")));
    let dup = replace(&t.db, &ctx(), &twice, &[])
        .await
        .expect_err("duplicate");
    assert!(dup.to_string().contains("org_master_duplicate"), "{dup}");
    // A row the table CHECKs refuse rolls the whole sync back.
    let mut bad = tree();
    bad[0].name = name("x");
    bad[0].plant_code = "p1".to_owned();
    replace(&t.db, &ctx(), &bad, &[])
        .await
        .expect_err("check refused");
    assert_eq!(health(&t.db).await.expect("health").units, 4);
    let names = t
        .json_rows("SELECT count(*) AS n FROM forge.org_units_master WHERE plant_code = 'P001'")
        .await;
    assert_eq!(names[0]["n"], 4);
}

#[tokio::test]
async fn a_team_member_derives_the_section_and_the_department_above() {
    let t = FreshDb::migrated().await;
    replace(&t.db, &ctx(), &tree(), &[position("manager", 55)])
        .await
        .expect("sync");
    let ana = person(&t, "ana", "TEAM-1", Some("P001")).await;
    let org = known(derive(&t.db, ana, AGES).await.expect("derive"));
    assert_eq!(org.plant_code, "P001");
    assert_eq!(org.primary.unit.code, "TEAM-1");
    assert_eq!(org.primary.unit.level, UnitLevel::Team);
    assert_eq!(
        org.primary.section.as_ref().map(|s| s.code.as_str()),
        Some("SEC-1")
    );
    assert_eq!(
        org.primary.department.as_ref().map(|d| d.name.ja.as_str()),
        Some("名前 DEP-1")
    );
    assert_eq!(org.position_rank, Some(55));

    // A section member: the section is the unit itself.
    let sam = person(&t, "sam", "SEC-1", Some("P001")).await;
    let org = known(derive(&t.db, sam, AGES).await.expect("derive"));
    assert_eq!(
        org.primary.section.map(|s| s.code),
        Some("SEC-1".to_owned())
    );
    assert_eq!(
        org.primary.department.map(|d| d.code),
        Some("DEP-1".to_owned())
    );

    // The display reads.
    let paths = primary_paths(&t.db, &[ana, sam]).await.expect("paths");
    assert_eq!(paths.len(), 2);
    let by_code = unit_paths(
        &t.db,
        Some("P001"),
        &["TEAM-1".to_owned(), "NOPE".to_owned()],
    )
    .await
    .expect("unit paths");
    assert_eq!(by_code.len(), 1);
    assert!(unit_paths(&t.db, None, &["TEAM-1".to_owned()])
        .await
        .expect("no plant")
        .is_empty());
}

#[tokio::test]
async fn the_derivation_fails_closed() {
    let t = FreshDb::migrated().await;
    let unknown = |d: Derived| match d {
        Derived::Unknown(reason) => reason,
        Derived::Known(org) => panic!("known: {org:?}"),
    };
    // No master yet.
    let ana = person(&t, "ana", "TEAM-1", Some("P001")).await;
    assert_eq!(
        unknown(derive(&t.db, ana, AGES).await.expect("derive")),
        UnknownReason::MasterMissing
    );
    // A team whose parent section is not in the master.
    let mut broken = tree();
    broken.remove(2);
    replace(&t.db, &ctx(), &broken, &[]).await.expect("sync");
    assert_eq!(
        unknown(derive(&t.db, ana, AGES).await.expect("derive")),
        UnknownReason::MissingParent
    );
    replace(&t.db, &ctx(), &tree(), &[]).await.expect("sync");
    known(derive(&t.db, ana, AGES).await.expect("derive"));

    // A code not in the master, no plant, no snapshot.
    let gone = person(&t, "gone", "CLOSED-9", Some("P001")).await;
    assert_eq!(
        unknown(derive(&t.db, gone, AGES).await.expect("derive")),
        UnknownReason::NotInMaster
    );
    let nowhere = person(&t, "nowhere", "TEAM-1", None).await;
    assert_eq!(
        unknown(derive(&t.db, nowhere, AGES).await.expect("derive")),
        UnknownReason::NoPlant
    );
    let new = NewUser::human("nobody", "nobody", "nobody@example.test");
    let nobody = users::create(&t.db, &ctx(), &new)
        .await
        .expect("user")
        .user_id;
    assert_eq!(
        unknown(derive(&t.db, nobody, AGES).await.expect("derive")),
        UnknownReason::NoSnapshot
    );

    // A stale snapshot, then a stale master.
    t.exec_superuser(&format!(
        "UPDATE forge.user_org_profiles SET confirmed_at = now() - interval '25 hours' \
         WHERE user_id = {}",
        ana.get()
    ))
    .await;
    assert_eq!(
        unknown(derive(&t.db, ana, AGES).await.expect("derive")),
        UnknownReason::StaleSnapshot
    );
    t.exec_superuser(&format!(
        "UPDATE forge.user_org_profiles SET confirmed_at = now() WHERE user_id = {}",
        ana.get()
    ))
    .await;
    t.exec_superuser("UPDATE forge.org_units_master SET synced_at = now() - interval '25 hours'")
        .await;
    assert_eq!(
        unknown(derive(&t.db, ana, AGES).await.expect("derive")),
        UnknownReason::MasterStale
    );
    // An invalid snapshot.
    let _ = record(&t.db, &ctx(), ana, ISSUER, Observed::Invalid)
        .await
        .expect("invalid");
    assert_eq!(
        unknown(derive(&t.db, ana, AGES).await.expect("derive")),
        UnknownReason::InvalidSnapshot
    );
}

#[tokio::test]
async fn the_read_pool_never_sees_the_head_staff_id() {
    let t = FreshDb::migrated().await;
    replace(&t.db, &ctx(), &tree(), &[]).await.expect("sync");
    let mut conn = t.db.acquire().await.expect("read pool");
    let refused = sqlx::query_scalar::<_, Option<String>>(
        "SELECT head_staff_id FROM forge.org_units_master LIMIT 1",
    )
    .fetch_one(&mut *conn)
    .await
    .expect_err("no column grant");
    assert!(
        refused.to_string().contains("permission denied"),
        "{refused}"
    );
    let names: Vec<String> = sqlx::query_scalar("SELECT name_ja FROM forge.org_units_master")
        .fetch_all(&mut *conn)
        .await
        .expect("names are readable");
    assert_eq!(names.len(), 4);
}

// ------------------------------------------------------------ pure path_of

fn row(code: &str, level: &str, parent: Option<&str>) -> ChainRow {
    ChainRow {
        code: code.to_owned(),
        level: level.to_owned(),
        parent_code: parent.map(str::to_owned),
        name: name(code),
        valid: true,
    }
}

#[test]
fn a_path_is_found_by_level_never_by_the_code() {
    let path = path_of(&[
        row("X9", "team", Some("A1")),
        row("A1", "section", Some("Z5")),
        row("Z5", "department", Some("Q0")),
        row("Q0", "division", None),
    ])
    .expect("path");
    assert_eq!(path.section.map(|s| s.code), Some("A1".to_owned()));
    assert_eq!(path.department.map(|d| d.code), Some("Z5".to_owned()));
    // A team directly under a department: no section.
    let flat = path_of(&[row("T", "team", Some("D")), row("D", "department", None)]).expect("flat");
    assert!(flat.section.is_none());
    assert_eq!(flat.department.map(|d| d.code), Some("D".to_owned()));
    // A division: neither.
    let top = path_of(&[row("V", "division", None)]).expect("top");
    assert!(top.section.is_none() && top.department.is_none());
}

#[test]
fn a_broken_chain_is_unknown() {
    assert_eq!(path_of(&[]), Err(UnknownReason::NotInMaster));
    let mut future = row("T", "team", None);
    future.valid = false;
    assert_eq!(path_of(&[future]), Err(UnknownReason::NotYetValid));
    assert_eq!(
        path_of(&[row("T", "team", Some("S"))]),
        Err(UnknownReason::MissingParent)
    );
    let mut parent_later = row("S", "section", None);
    parent_later.valid = false;
    assert_eq!(
        path_of(&[row("T", "team", Some("S")), parent_later]),
        Err(UnknownReason::MissingParent)
    );
    // A parent at the same or a lower level.
    assert_eq!(
        path_of(&[row("S1", "section", Some("S2")), row("S2", "section", None)]),
        Err(UnknownReason::BrokenHierarchy)
    );
    assert_eq!(
        path_of(&[row("S", "section", Some("T")), row("T", "team", None)]),
        Err(UnknownReason::BrokenHierarchy)
    );
    // An unknown level.
    assert_eq!(
        path_of(&[row("U", "unit", None)]),
        Err(UnknownReason::BrokenHierarchy)
    );
}
