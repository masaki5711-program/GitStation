//! The org master's copy against a migrated database (ADR-0038 §1, §4):
//! a sync is one transaction with one audit row when it changed anything,
//! unchanged rows are not rewritten, a broken hierarchy or a sharp shrink
//! is refused (FG-INT-0027) unless allowed, one sync runs at a time, the
//! derivation walks parents by level and fails closed, and the display
//! reads mark names past a gate as stale. Synthetic codes and names only.

use forge_core::RequestId;
use forge_errors::ErrorCode;

use super::replace::check_hierarchy;
use super::*;
use crate::change::{Actor, ChangeContext};
use crate::org_profiles::{record, Observed, OrgSnapshot, OrgUnit};
use crate::test_support::*;
use crate::users::{self, NewUser};
use forge_core::ids::UserId;

const ISSUER: &str = "http://127.0.0.1:58080/realms/synthetic";

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

fn unit_in(plant: &str, code: &str, level: UnitLevel, parent: Option<&str>) -> MasterUnit {
    MasterUnit {
        plant_code: plant.to_owned(),
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

fn unit(code: &str, level: UnitLevel, parent: Option<&str>) -> MasterUnit {
    unit_in("P001", code, level, parent)
}

fn position_in(plant: &str, code: &str, rank: i32) -> MasterPosition {
    MasterPosition {
        plant_code: plant.to_owned(),
        code: code.to_owned(),
        name: name(code),
        rank,
        sort_order: rank,
        active: true,
        version: 1,
    }
}

fn position(code: &str, rank: i32) -> MasterPosition {
    position_in("P001", code, rank)
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

fn manager() -> Vec<MasterPosition> {
    vec![position("manager", 55)]
}

const STRICT: ReplacePolicy = ReplacePolicy {
    allow_large_shrink: false,
};

async fn sync_with(
    t: &FreshDb,
    units: &[MasterUnit],
    positions: &[MasterPosition],
    policy: ReplacePolicy,
) -> Synced {
    match replace(&t.db, &ctx(), units, positions, policy)
        .await
        .expect("sync")
    {
        Replaced::Synced(s) => s,
        Replaced::Busy => panic!("busy"),
    }
}

async fn sync(t: &FreshDb, units: &[MasterUnit], positions: &[MasterPosition]) -> Synced {
    sync_with(t, units, positions, STRICT).await
}

async fn refused(
    t: &FreshDb,
    units: &[MasterUnit],
    positions: &[MasterPosition],
    reason: &str,
) -> String {
    let err = replace(&t.db, &ctx(), units, positions, STRICT)
        .await
        .expect_err(reason);
    assert_eq!(err.code(), ErrorCode::OrgMasterInvalid, "{err}");
    let text = err.to_string();
    assert!(text.contains(&format!("reason={reason}")), "{text}");
    text
}

async fn synced_audits(t: &FreshDb) -> Vec<serde_json::Value> {
    t.json_rows(
        "SELECT actor_kind, resource_type, details FROM forge.audit_log \
         WHERE action = 'org.master_synced' ORDER BY id",
    )
    .await
}

async fn unit_count(t: &FreshDb) -> i64 {
    health(&t.db).await.expect("health").units
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

fn unknown(d: Derived) -> UnknownReason {
    match d {
        Derived::Unknown(reason) => reason,
        Derived::Known(org) => panic!("known: {org:?}"),
    }
}

#[tokio::test]
async fn a_sync_replaces_the_copy_and_is_audited_only_when_it_changed() {
    let t = FreshDb::migrated().await;
    let first = sync(&t, &tree(), &manager()).await;
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

    // The same lists: no row is rewritten, no audit row; the state's
    // synced_at moves.
    t.exec_superuser(
        "UPDATE forge.org_units_master SET changed_at = now() - interval '2 days'; \
         UPDATE forge.org_master_state SET synced_at = now() - interval '2 days'",
    )
    .await;
    let same = sync(&t, &tree(), &manager()).await;
    assert!(!same.units.touched() && !same.positions.touched());
    assert_eq!(synced_audits(&t).await.len(), 1);
    let untouched = t
        .json_rows(
            "SELECT count(*) AS n FROM forge.org_units_master \
             WHERE changed_at < now() - interval '1 day'",
        )
        .await;
    assert_eq!(untouched[0]["n"], 4, "unchanged rows are not rewritten");
    let h = health(&t.db).await.expect("health");
    assert_eq!((h.units, h.positions), (4, 1));
    assert!(h.age_secs.is_some_and(|a| a < 60), "{h:?}");

    // A renamed unit, a closed one (no longer listed) and a new position.
    let mut next = tree();
    next.pop();
    next[2].name = name("改名した課");
    let changed = sync(&t, &next, &[position("manager", 55), position("staff", 15)]).await;
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
        .json_rows(
            "SELECT code FROM forge.org_units_master \
             WHERE changed_at > now() - interval '1 minute'",
        )
        .await;
    assert_eq!(rows.len(), 1, "only the renamed row: {rows:?}");
    assert_eq!(unit_count(&t).await, 3);
}

#[tokio::test]
async fn an_empty_or_duplicated_unit_list_writes_nothing() {
    let t = FreshDb::migrated().await;
    sync(&t, &tree(), &manager()).await;
    let empty = replace(&t.db, &ctx(), &[], &manager(), STRICT)
        .await
        .expect_err("empty");
    assert!(empty.to_string().contains("org_master_empty"), "{empty}");
    let mut twice = tree();
    twice.push(unit("SEC-1", UnitLevel::Section, Some("DEP-1")));
    let dup = replace(&t.db, &ctx(), &twice, &manager(), STRICT)
        .await
        .expect_err("duplicate");
    assert!(dup.to_string().contains("org_master_duplicate"), "{dup}");
    // A row the table CHECKs refuse rolls the whole sync back.
    let mut bad = tree();
    bad[3].cost_center = Some("c".repeat(65));
    replace(&t.db, &ctx(), &bad, &manager(), STRICT)
        .await
        .expect_err("check refused");
    assert_eq!(unit_count(&t).await, 4);
}

#[tokio::test]
async fn a_broken_hierarchy_is_refused_before_anything_is_written() {
    let t = FreshDb::migrated().await;
    sync(&t, &tree(), &manager()).await;
    // A parent that is not listed (in this plant).
    let mut orphan = tree();
    orphan.push(unit("TEAM-2", UnitLevel::Team, Some("SEC-9")));
    let text = refused(&t, &orphan, &manager(), "missing_parent").await;
    assert!(
        text.contains("index=4") && text.contains("plant=P001"),
        "{text}"
    );
    let mut elsewhere = tree();
    elsewhere.push(unit_in("P002", "TEAM-2", UnitLevel::Team, Some("SEC-1")));
    refused(&t, &elsewhere, &manager(), "missing_parent").await;
    // A parent at the same or a lower level (which also rules out cycles).
    let mut flat = tree();
    flat.push(unit("SEC-2", UnitLevel::Section, Some("SEC-1")));
    refused(&t, &flat, &manager(), "level_order").await;
    let mut upside = tree();
    upside[1].parent_code = Some("TEAM-1".to_owned());
    refused(&t, &upside, &manager(), "level_order").await;
    assert_eq!(unit_count(&t).await, 4);
    // Pure: a valid tree passes.
    check_hierarchy(&tree()).expect("valid");
}

#[tokio::test]
async fn a_sharp_shrink_is_refused_unless_allowed() {
    let t = FreshDb::migrated().await;
    let mut both = tree();
    both.push(unit_in("P002", "DEP-9", UnitLevel::Department, None));
    let positions = [position("manager", 55), position_in("P002", "staff", 15)];
    sync(&t, &both, &positions).await;

    // More than half of the units removed.
    let text = refused(
        &t,
        &[unit("DIV-1", UnitLevel::Division, None), both[4].clone()],
        &positions,
        "shrink",
    )
    .await;
    assert!(
        text.contains("before=5") && text.contains("after=2") && text.contains("removed=3"),
        "{text}"
    );
    // A plant emptied (one unit of five removed).
    let text = refused(&t, &tree(), &positions, "plant_emptied").await;
    assert!(
        text.contains("plant=P002") && text.contains("list=units"),
        "{text}"
    );
    // No positions at all; a plant's positions emptied.
    refused(&t, &both, &[], "positions_empty").await;
    let text = refused(&t, &both, &manager(), "plant_emptied").await;
    assert!(
        text.contains("list=positions") && text.contains("plant=P002"),
        "{text}"
    );
    assert_eq!(unit_count(&t).await, 5);

    // The operator's opt-in lets the same sync through.
    let allowed = sync_with(
        &t,
        &tree(),
        &manager(),
        ReplacePolicy {
            allow_large_shrink: true,
        },
    )
    .await;
    assert_eq!(allowed.units.removed, 1);
    assert_eq!(allowed.positions.removed, 1);
    let audits = synced_audits(&t).await;
    assert_eq!(audits[1]["details"]["allow_large_shrink"], true);
}

#[tokio::test]
async fn a_second_sync_while_one_holds_the_lock_is_busy() {
    let t = FreshDb::migrated().await;
    let mut holder = t.db.write_pool().begin().await.expect("holder");
    let held: bool = sqlx::query_scalar(
        "SELECT pg_try_advisory_xact_lock(hashtextextended('org_master_sync', 0))",
    )
    .fetch_one(&mut *holder)
    .await
    .expect("lock");
    assert!(held);
    let busy = replace(&t.db, &ctx(), &tree(), &manager(), STRICT)
        .await
        .expect("busy is not an error");
    assert_eq!(busy, Replaced::Busy);
    assert_eq!(unit_count(&t).await, 0, "nothing written");
    holder.rollback().await.expect("rollback");
    sync(&t, &tree(), &manager()).await;
    assert_eq!(unit_count(&t).await, 4);
}

#[tokio::test]
async fn a_team_member_derives_the_section_and_the_department_above() {
    let t = FreshDb::migrated().await;
    sync(&t, &tree(), &manager()).await;
    let ana = person(&t, "ana", "TEAM-1", Some("P001")).await;
    let org = known(derive(&t.db, ana).await.expect("derive"));
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
    let org = known(derive(&t.db, sam).await.expect("derive"));
    assert_eq!(
        org.primary.section.map(|s| s.code),
        Some("SEC-1".to_owned())
    );
    assert_eq!(
        org.primary.department.map(|d| d.code),
        Some("DEP-1".to_owned())
    );

    // The display reads: fresh facts are not stale.
    let paths = primary_paths(&t.db, &[ana, sam]).await.expect("paths");
    assert_eq!(paths.len(), 2);
    assert!(paths.values().all(|p| !p.stale));
    let own = unit_paths(&t.db, ana).await.expect("unit paths");
    assert_eq!(own.len(), 1);
    assert!(!own["TEAM-1"].stale);
    let nowhere = person(&t, "nowhere", "TEAM-1", None).await;
    assert!(unit_paths(&t.db, nowhere)
        .await
        .expect("no plant")
        .is_empty());
}

#[tokio::test]
async fn the_derivation_fails_closed() {
    let t = FreshDb::migrated().await;
    // No master yet.
    let ana = person(&t, "ana", "TEAM-1", Some("P001")).await;
    assert_eq!(
        unknown(derive(&t.db, ana).await.expect("derive")),
        UnknownReason::MasterMissing
    );
    sync(&t, &tree(), &manager()).await;
    known(derive(&t.db, ana).await.expect("derive"));
    // A parent gone from the copy (the sync refuses that; a hand edit or a
    // later broken state must still fail closed).
    t.exec_superuser("DELETE FROM forge.org_units_master WHERE code = 'SEC-1'")
        .await;
    assert_eq!(
        unknown(derive(&t.db, ana).await.expect("derive")),
        UnknownReason::MissingParent
    );
    sync(&t, &tree(), &manager()).await;

    // A code not in the master, no plant, no snapshot.
    let gone = person(&t, "gone", "CLOSED-9", Some("P001")).await;
    assert_eq!(
        unknown(derive(&t.db, gone).await.expect("derive")),
        UnknownReason::NotInMaster
    );
    let nowhere = person(&t, "nowhere", "TEAM-1", None).await;
    assert_eq!(
        unknown(derive(&t.db, nowhere).await.expect("derive")),
        UnknownReason::NoPlant
    );
    let new = NewUser::human("nobody", "nobody", "nobody@example.test");
    let nobody = users::create(&t.db, &ctx(), &new)
        .await
        .expect("user")
        .user_id;
    assert_eq!(
        unknown(derive(&t.db, nobody).await.expect("derive")),
        UnknownReason::NoSnapshot
    );

    // A stale snapshot: unknown, and the display marks the names stale.
    t.exec_superuser(&format!(
        "UPDATE forge.user_org_profiles SET confirmed_at = now() - interval '25 hours' \
         WHERE user_id = {}",
        ana.get()
    ))
    .await;
    assert_eq!(
        unknown(derive(&t.db, ana).await.expect("derive")),
        UnknownReason::StaleSnapshot
    );
    let shown = primary_paths(&t.db, &[ana]).await.expect("paths");
    assert!(shown[&ana].stale, "last known names");
    assert!(unit_paths(&t.db, ana).await.expect("own")["TEAM-1"].stale);

    // A stale master.
    t.exec_superuser(&format!(
        "UPDATE forge.user_org_profiles SET confirmed_at = now() WHERE user_id = {}",
        ana.get()
    ))
    .await;
    assert!(!primary_paths(&t.db, &[ana]).await.expect("paths")[&ana].stale);
    t.exec_superuser("UPDATE forge.org_master_state SET synced_at = now() - interval '25 hours'")
        .await;
    assert_eq!(
        unknown(derive(&t.db, ana).await.expect("derive")),
        UnknownReason::MasterStale
    );
    assert!(primary_paths(&t.db, &[ana]).await.expect("paths")[&ana].stale);
    // An invalid snapshot.
    let _ = record(&t.db, &ctx(), ana, ISSUER, Observed::Invalid)
        .await
        .expect("invalid");
    assert_eq!(
        unknown(derive(&t.db, ana).await.expect("derive")),
        UnknownReason::InvalidSnapshot
    );
}

#[tokio::test]
async fn the_read_pool_never_sees_the_head_staff_id() {
    let t = FreshDb::migrated().await;
    sync(&t, &tree(), &manager()).await;
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
