//! Org bindings against a migrated database (ADR-0038 §2, §4): the role
//! loaders raise a membership to a binding's role by the master's
//! hierarchy (team → section → department, never the code's shape),
//! `primary` vs `any`, `min_rank`, the Developer cap, and nothing at all
//! when the snapshot or the master's copy cannot be trusted — the hand-made
//! memberships unaffected. The commands are audited and refuse bad input
//! with FG-AUTH-0029 / 0030 / 0031. Synthetic codes and names only.

use forge_core::ids::{GroupId, ProjectId, UserId};
use forge_core::{RequestId, Role, Visibility};
use forge_errors::ErrorCode;

use super::*;
use crate::change::{Actor, ChangeContext};
use crate::groups::{self, CreatedGroup, NewGroup};
use crate::org_master::{replace, MasterPosition, MasterUnit, ReplacePolicy};
use crate::org_profiles::{record, Observed, OrgSnapshot, OrgUnit};
use crate::projects::{self, CreatedProject, NewProject};
use crate::roles::{load_group_roles, load_project_roles};
use crate::test_support::*;
use crate::users::{self, NewUser};

const ISSUER: &str = "http://127.0.0.1:58080/realms/synthetic";
const PLANT: &str = "P001";

fn ctx() -> ChangeContext {
    ChangeContext::new(Actor::System, RequestId::generate())
}

fn unit(code: &str, level: UnitLevel, parent: Option<&str>) -> MasterUnit {
    MasterUnit {
        plant_code: PLANT.to_owned(),
        code: code.to_owned(),
        level,
        parent_code: parent.map(str::to_owned),
        name: LocalizedName {
            ja: format!("名前 {code}"),
            en: Some(format!("Name {code}")),
            id: None,
        },
        cost_center: None,
        head_staff_id: None,
        valid_from: Some("2026-01-01".to_owned()),
        version: 1,
    }
}

/// DIV-1 → DEP-1 → SEC-1 → TEAM-1, DEP-1 → SEC-2; DIV-1 → DEP-2.
fn tree() -> Vec<MasterUnit> {
    vec![
        unit("DIV-1", UnitLevel::Division, None),
        unit("DEP-1", UnitLevel::Department, Some("DIV-1")),
        unit("SEC-1", UnitLevel::Section, Some("DEP-1")),
        unit("TEAM-1", UnitLevel::Team, Some("SEC-1")),
        unit("SEC-2", UnitLevel::Section, Some("DEP-1")),
        unit("DEP-2", UnitLevel::Department, Some("DIV-1")),
    ]
}

async fn sync(t: &FreshDb) {
    let positions = vec![MasterPosition {
        plant_code: PLANT.to_owned(),
        code: "manager".to_owned(),
        name: LocalizedName {
            ja: "課長".to_owned(),
            en: None,
            id: None,
        },
        rank: 55,
        sort_order: 1,
        active: true,
        version: 1,
    }];
    replace(
        &t.db,
        &ctx(),
        &tree(),
        &positions,
        ReplacePolicy {
            allow_large_shrink: false,
        },
    )
    .await
    .expect("sync");
}

async fn user(t: &FreshDb, name: &str) -> UserId {
    users::create(
        &t.db,
        &ctx(),
        &NewUser::human(name, name, &format!("{name}@example.test")),
    )
    .await
    .expect("user")
    .user_id
}

/// A snapshot: `units` as (code, primary), the rank.
async fn snapshot(t: &FreshDb, who: UserId, units: &[(&str, bool)], rank: Option<i32>) {
    let snap = OrgSnapshot {
        org_units: units
            .iter()
            .map(|(code, primary)| OrgUnit {
                code: (*code).to_owned(),
                primary: *primary,
            })
            .collect(),
        positions: Vec::new(),
        position_rank: rank,
        plants: vec![PLANT.to_owned()],
        plant_code: Some(PLANT.to_owned()),
    };
    let _ = record(&t.db, &ctx(), who, ISSUER, Observed::Valid(&snap))
        .await
        .expect("snapshot");
}

async fn person(t: &FreshDb, name: &str, units: &[(&str, bool)], rank: Option<i32>) -> UserId {
    let who = user(t, name).await;
    snapshot(t, who, units, rank).await;
    who
}

async fn group(t: &FreshDb, segment: &str, owner: UserId) -> CreatedGroup {
    let new = NewGroup {
        visibility: Visibility::Private,
        segment: segment.to_owned(),
        parent: None,
        display_name: format!("Group {segment}"),
        description: String::new(),
        owner,
    };
    groups::create(&t.db, &ctx(), &new).await.expect("group")
}

async fn project(t: &FreshDb, segment: &str, g: &CreatedGroup, creator: UserId) -> CreatedProject {
    let new = NewProject::new(
        segment,
        g.namespace_id,
        &format!("Project {segment}"),
        creator,
    );
    projects::create(&t.db, &ctx(), &new)
        .await
        .expect("project")
}

fn new_binding(org: &str, descendants: bool, role: Role) -> NewBinding {
    NewBinding {
        plant_code: PLANT.to_owned(),
        org_code: org.to_owned(),
        include_descendants: descendants,
        membership: Membership::Any,
        min_rank: None,
        role,
        reason: Some("合成の理由".to_owned()),
    }
}

async fn bind(
    t: &FreshDb,
    target: BindingTarget,
    owner: UserId,
    new: &NewBinding,
) -> StoredBinding {
    create_decided(&t.db, &ctx(), target, owner, new, |_| Ok(()))
        .await
        .expect("bind")
        .0
}

async fn group_role(t: &FreshDb, g: GroupId, who: UserId) -> Option<Role> {
    load_group_roles(&t.db, g, who).await.expect("roles").native
}

async fn project_role(t: &FreshDb, p: ProjectId, who: UserId) -> Option<Role> {
    load_project_roles(&t.db, p, who)
        .await
        .expect("roles")
        .native
}

struct World {
    t: FreshDb,
    owner: UserId,
    g: CreatedGroup,
    p: CreatedProject,
}

async fn world() -> World {
    let t = FreshDb::migrated().await;
    sync(&t).await;
    let owner = user(&t, "owner1").await;
    let g = group(&t, "pfs", owner).await;
    let p = project(&t, "spec", &g, owner).await;
    World { t, owner, g, p }
}

#[tokio::test]
async fn a_department_binding_with_descendants_reaches_its_sections_and_teams() {
    let w = world().await;
    let gid = BindingTarget::Group(w.g.group_id);
    bind(
        &w.t,
        gid,
        w.owner,
        &new_binding("DEP-1", true, Role::Developer),
    )
    .await;
    let team = person(&w.t, "team1", &[("TEAM-1", true)], None).await;
    let section = person(&w.t, "sec2", &[("SEC-2", true)], None).await;
    let dept = person(&w.t, "dep1", &[("DEP-1", true)], None).await;
    let other = person(&w.t, "dep2", &[("DEP-2", true)], None).await;
    let none = user(&w.t, "nosnap").await;
    for who in [team, section, dept] {
        assert_eq!(
            group_role(&w.t, w.g.group_id, who).await,
            Some(Role::Developer)
        );
        // Inherited by the projects below, as a group membership is.
        assert_eq!(
            project_role(&w.t, w.p.project_id, who).await,
            Some(Role::Developer)
        );
    }
    assert_eq!(group_role(&w.t, w.g.group_id, other).await, None);
    assert_eq!(group_role(&w.t, w.g.group_id, none).await, None);
    // The owner's own Owner is untouched.
    assert_eq!(
        group_role(&w.t, w.g.group_id, w.owner).await,
        Some(Role::Owner)
    );

    // Without descendants: the department's own people only.
    let pid = BindingTarget::Project(w.p.project_id);
    let w2 = person(&w.t, "team2", &[("TEAM-1", true)], None).await;
    let solo = person(&w.t, "dep2b", &[("DEP-2", true)], None).await;
    bind(
        &w.t,
        pid,
        w.owner,
        &new_binding("DEP-2", false, Role::Reporter),
    )
    .await;
    assert_eq!(
        project_role(&w.t, w.p.project_id, solo).await,
        Some(Role::Reporter)
    );
    let below = person(&w.t, "dep2team", &[("SEC-1", true)], None).await;
    assert_eq!(
        project_role(&w.t, w.p.project_id, below).await,
        Some(Role::Developer)
    );
    assert_eq!(
        project_role(&w.t, w.p.project_id, w2).await,
        Some(Role::Developer)
    );
    w.t.drop_now().await;
}

#[tokio::test]
async fn primary_bindings_ignore_concurrent_units_and_min_rank_filters() {
    let w = world().await;
    let pid = BindingTarget::Project(w.p.project_id);
    let mut primary_only = new_binding("SEC-1", false, Role::Reporter);
    primary_only.membership = Membership::Primary;
    bind(&w.t, pid, w.owner, &primary_only).await;
    // Primary DEP-2, concurrent SEC-1.
    let concurrent = person(&w.t, "conc", &[("DEP-2", true), ("SEC-1", false)], None).await;
    let primary = person(&w.t, "prim", &[("SEC-1", true)], None).await;
    assert_eq!(project_role(&w.t, w.p.project_id, concurrent).await, None);
    assert_eq!(
        project_role(&w.t, w.p.project_id, primary).await,
        Some(Role::Reporter)
    );
    // `any` takes the concurrent unit too.
    bind(
        &w.t,
        pid,
        w.owner,
        &new_binding("SEC-1", false, Role::Reporter),
    )
    .await;
    assert_eq!(
        project_role(&w.t, w.p.project_id, concurrent).await,
        Some(Role::Reporter)
    );

    // min_rank: managers (55) only are Developer through DEP-1.
    let mut managers = new_binding("DEP-1", true, Role::Developer);
    managers.min_rank = Some(55);
    bind(&w.t, pid, w.owner, &managers).await;
    let manager = person(&w.t, "mgr", &[("TEAM-1", true)], Some(55)).await;
    let staff = person(&w.t, "staff", &[("TEAM-1", true)], Some(15)).await;
    let unranked = person(&w.t, "norank", &[("SEC-2", true)], None).await;
    assert_eq!(
        project_role(&w.t, w.p.project_id, manager).await,
        Some(Role::Developer)
    );
    assert_eq!(project_role(&w.t, w.p.project_id, staff).await, None);
    assert_eq!(project_role(&w.t, w.p.project_id, unranked).await, None);
    w.t.drop_now().await;
}

#[tokio::test]
async fn stale_or_invalid_facts_give_no_role_and_leave_memberships_alone() {
    let w = world().await;
    let gid = BindingTarget::Group(w.g.group_id);
    bind(
        &w.t,
        gid,
        w.owner,
        &new_binding("DEP-1", true, Role::Developer),
    )
    .await;
    let who = person(&w.t, "member", &[("TEAM-1", true)], None).await;
    // A direct Reporter membership of the project, made by hand.
    w.t.commit_audited(
        ("project", w.p.project_id.get()),
        &[format!(
            "INSERT INTO forge.project_members (project_id, user_id, role) VALUES ({}, {}, 'reporter')",
            w.p.project_id.get(),
            who.get()
        )],
    )
    .await;
    assert_eq!(
        project_role(&w.t, w.p.project_id, who).await,
        Some(Role::Developer)
    );

    // The snapshot is older than org.max_profile_age (24 h).
    w.t.exec_superuser(&format!(
        "UPDATE forge.user_org_profiles SET confirmed_at = now() - interval '25 hours' \
         WHERE user_id = {}",
        who.get()
    ))
    .await;
    assert_eq!(group_role(&w.t, w.g.group_id, who).await, None);
    assert_eq!(
        project_role(&w.t, w.p.project_id, who).await,
        Some(Role::Reporter)
    );

    // Fresh again, then marked invalid.
    w.t.exec_superuser(&format!(
        "UPDATE forge.user_org_profiles SET confirmed_at = now(), invalid_since = now() \
         WHERE user_id = {}",
        who.get()
    ))
    .await;
    assert_eq!(group_role(&w.t, w.g.group_id, who).await, None);

    // Valid, but the master's copy is older than 24 h.
    w.t.exec_superuser(&format!(
        "UPDATE forge.user_org_profiles SET invalid_since = NULL WHERE user_id = {}; \
         UPDATE forge.org_master_state SET synced_at = now() - interval '25 hours'",
        who.get()
    ))
    .await;
    assert_eq!(group_role(&w.t, w.g.group_id, who).await, None);
    assert_eq!(
        project_role(&w.t, w.p.project_id, who).await,
        Some(Role::Reporter)
    );

    // A shorter policy (published from the configuration) closes earlier.
    w.t.exec_superuser("UPDATE forge.org_master_state SET synced_at = now()")
        .await;
    assert_eq!(
        group_role(&w.t, w.g.group_id, who).await,
        Some(Role::Developer)
    );
    w.t.exec_superuser(&format!(
        "UPDATE forge.user_org_profiles SET confirmed_at = now() - interval '2 hours' \
         WHERE user_id = {}",
        who.get()
    ))
    .await;
    let short = w.t.db.clone().with_org_ages(crate::org_master::MaxAges {
        profile: std::time::Duration::from_secs(3600),
        master: std::time::Duration::from_secs(3600),
    });
    publish_ages(&short).await.expect("publish");
    assert_eq!(group_role(&w.t, w.g.group_id, who).await, None);

    // A unit whose chain is broken (a parent not in the copy) counts for
    // nothing either.
    publish_ages(&w.t.db).await.expect("publish default");
    w.t.exec_superuser(&format!(
        "UPDATE forge.user_org_profiles SET confirmed_at = now() WHERE user_id = {}; \
         UPDATE forge.org_units_master SET parent_code = 'GONE-1' WHERE code = 'SEC-1'",
        who.get()
    ))
    .await;
    assert_eq!(group_role(&w.t, w.g.group_id, who).await, None);
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_binding_never_gives_more_than_developer() {
    let w = world().await;
    let pid = BindingTarget::Project(w.p.project_id);
    bind(
        &w.t,
        pid,
        w.owner,
        &new_binding("DEP-1", true, Role::Developer),
    )
    .await;
    let who = person(&w.t, "maint", &[("SEC-1", true)], None).await;
    // A hand-made Maintainer stays Maintainer (the higher of the two).
    w.t.commit_audited(
        ("project", w.p.project_id.get()),
        &[format!(
            "INSERT INTO forge.project_members (project_id, user_id, role) VALUES ({}, {}, 'maintainer')",
            w.p.project_id.get(),
            who.get()
        )],
    )
    .await;
    assert_eq!(
        project_role(&w.t, w.p.project_id, who).await,
        Some(Role::Maintainer)
    );
    // The table refuses a Maintainer binding written around the command.
    let mut conn = w.t.superuser().await;
    let refused = sqlx::query(&format!(
        "INSERT INTO forge.org_bindings (project_id, plant_code, org_code, role, created_by) \
         VALUES ({}, 'P001', 'DEP-1', 'maintainer', {})",
        w.p.project_id.get(),
        w.owner.get()
    ))
    .execute(&mut conn)
    .await
    .expect_err("role cap");
    assert!(
        refused.to_string().contains("org_bindings_role_cap"),
        "{refused}"
    );
    drop(conn);
    w.t.drop_now().await;
}

#[tokio::test]
async fn org_binding_role_above_developer_is_refused() {
    let w = world().await;
    for role in [Role::Maintainer, Role::Owner] {
        let err = create_decided(
            &w.t.db,
            &ctx(),
            BindingTarget::Group(w.g.group_id),
            w.owner,
            &new_binding("DEP-1", true, role),
            |_| Ok(()),
        )
        .await
        .expect_err("cap");
        assert_reason(&err, ErrorCode::OrgBindingRoleAboveCap, "role_above_cap");
    }
    assert!(list(&w.t.db, BindingTarget::Group(w.g.group_id))
        .await
        .expect("list")
        .is_empty());
    w.t.drop_now().await;
}

#[tokio::test]
async fn org_binding_invalid_input_is_refused() {
    let w = world().await;
    let target = BindingTarget::Project(w.p.project_id);
    let cases: Vec<(NewBinding, &str)> = vec![
        (new_binding("DEP-1", true, Role::Guest), "role_not_allowed"),
        (
            NewBinding {
                plant_code: "p1".to_owned(),
                ..new_binding("DEP-1", true, Role::Reporter)
            },
            "plant_code_invalid",
        ),
        (
            new_binding("dep-1", true, Role::Reporter),
            "org_code_invalid",
        ),
        (new_binding("", true, Role::Reporter), "org_code_invalid"),
        (
            NewBinding {
                min_rank: Some(1000),
                ..new_binding("DEP-1", true, Role::Reporter)
            },
            "min_rank_out_of_range",
        ),
        (
            NewBinding {
                reason: Some(" ".to_owned()),
                ..new_binding("DEP-1", true, Role::Reporter)
            },
            "reason_invalid",
        ),
        (
            NewBinding {
                reason: Some("x".repeat(501)),
                ..new_binding("DEP-1", true, Role::Reporter)
            },
            "reason_invalid",
        ),
    ];
    for (new, reason) in cases {
        let err = create_decided(&w.t.db, &ctx(), target, w.owner, &new, |_| Ok(()))
            .await
            .expect_err(reason);
        assert_reason(&err, ErrorCode::OrgBindingInvalid, reason);
    }
    // The same binding twice.
    bind(
        &w.t,
        target,
        w.owner,
        &new_binding("DEP-1", true, Role::Reporter),
    )
    .await;
    let err = create_decided(
        &w.t.db,
        &ctx(),
        target,
        w.owner,
        &new_binding("DEP-1", true, Role::Developer),
        |_| Ok(()),
    )
    .await
    .expect_err("duplicate");
    assert_reason(&err, ErrorCode::OrgBindingInvalid, "duplicate");
    w.t.drop_now().await;
}

#[tokio::test]
async fn org_binding_unit_not_in_master_is_refused() {
    let t = FreshDb::migrated().await;
    let owner = user(&t, "owner1").await;
    let g = group(&t, "pfs", owner).await;
    let target = BindingTarget::Group(g.group_id);
    let err = create_decided(
        &t.db,
        &ctx(),
        target,
        owner,
        &new_binding("DEP-1", true, Role::Reporter),
        |_| Ok(()),
    )
    .await
    .expect_err("never synced");
    assert_reason(&err, ErrorCode::OrgUnitNotInMaster, "master_missing");
    sync(&t).await;
    let err = create_decided(
        &t.db,
        &ctx(),
        target,
        owner,
        &new_binding("NOPE-9", true, Role::Reporter),
        |_| Ok(()),
    )
    .await
    .expect_err("absent");
    assert_reason(&err, ErrorCode::OrgUnitNotInMaster, "not_in_master");
    t.exec_superuser(
        "UPDATE forge.org_units_master SET valid_from = current_date + 3 WHERE code = 'SEC-2'",
    )
    .await;
    let err = create_decided(
        &t.db,
        &ctx(),
        target,
        owner,
        &new_binding("SEC-2", false, Role::Reporter),
        |_| Ok(()),
    )
    .await
    .expect_err("later");
    assert_reason(&err, ErrorCode::OrgUnitNotInMaster, "not_yet_valid");
    t.drop_now().await;
}

#[tokio::test]
async fn bindings_are_listed_audited_removed_and_their_people_shown() {
    let w = world().await;
    let target = BindingTarget::Project(w.p.project_id);
    let stored = bind(
        &w.t,
        target,
        w.owner,
        &new_binding("SEC-1", true, Role::Reporter),
    )
    .await;
    assert_eq!(stored.target, target);
    assert_eq!(stored.created_by_username, "owner1");
    assert_eq!(
        stored.unit.as_ref().map(|u| (u.level, u.name.ja.as_str())),
        Some((UnitLevel::Section, "名前 SEC-1"))
    );
    let team = person(&w.t, "team1", &[("TEAM-1", true)], None).await;
    let _other = person(&w.t, "dep2", &[("DEP-2", true)], None).await;
    let (people, truncated) = members(&w.t.db, target).await.expect("members");
    assert!(!truncated);
    assert_eq!(people.len(), 1, "{people:?}");
    assert_eq!(people[0].user_id, team);
    assert_eq!(people[0].role, Role::Reporter);
    assert_eq!(people[0].binding_id, stored.id);
    // The group has none.
    let (none, _) = members(&w.t.db, BindingTarget::Group(w.g.group_id))
        .await
        .expect("members");
    assert!(none.is_empty());

    let audits =
        w.t.json_rows(
            "SELECT action, resource_type, resource_id, project_id, details FROM forge.audit_log \
             WHERE action LIKE 'org_binding.%' ORDER BY id",
        )
        .await;
    assert_eq!(audits.len(), 1);
    assert_eq!(audits[0]["resource_type"], "project");
    assert_eq!(audits[0]["details"]["org_code"], "SEC-1");
    assert_eq!(audits[0]["details"]["reason_chars"], 5);
    assert!(!audits[0]["details"].to_string().contains("合成"));

    // Removing it on another target is not found; on its own it is gone.
    let err = delete_decided(
        &w.t.db,
        &ctx(),
        BindingTarget::Group(w.g.group_id),
        stored.id,
        w.owner,
        |_| Ok(()),
    )
    .await
    .expect_err("other target");
    assert_reason(&err, ErrorCode::NotFound, "absent");
    delete_decided(&w.t.db, &ctx(), target, stored.id, w.owner, |_| Ok(()))
        .await
        .expect("delete");
    assert!(list(&w.t.db, target).await.expect("list").is_empty());
    assert_eq!(project_role(&w.t, w.p.project_id, team).await, None);
    let n = w
        .t
        .json_rows("SELECT count(*) AS n FROM forge.audit_log WHERE action = 'org_binding.deleted'")
        .await;
    assert_eq!(n[0]["n"], 1);
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_refused_decision_writes_nothing() {
    let w = world().await;
    let target = BindingTarget::Group(w.g.group_id);
    let err = create_decided(
        &w.t.db,
        &ctx(),
        target,
        w.owner,
        &new_binding("DEP-1", true, Role::Reporter),
        |facts| match facts {
            TargetFacts::Group(f) if f.group.role == Some(Role::Owner) => {
                Err(ForgeError::new(ErrorCode::Forbidden))
            }
            TargetFacts::Group(_) | TargetFacts::Project(_) => Ok(()),
        },
    )
    .await
    .expect_err("refused");
    assert_eq!(err.code(), ErrorCode::Forbidden);
    assert!(list(&w.t.db, target).await.expect("list").is_empty());
    w.t.drop_now().await;
}

#[tokio::test]
async fn the_picker_finds_units_by_code_or_name() {
    let w = world().await;
    let by_code = units(&w.t.db, Some("sec"), None, 10).await.expect("units");
    let codes: Vec<&str> = by_code.iter().map(|u| u.code.as_str()).collect();
    assert_eq!(codes, ["SEC-1", "SEC-2"]);
    let by_name = units(&w.t.db, Some("name dep-2"), Some(PLANT), 10)
        .await
        .expect("units");
    assert_eq!(by_name.len(), 1);
    assert_eq!(by_name[0].level, UnitLevel::Department);
    assert!(units(&w.t.db, Some("%"), None, 10)
        .await
        .expect("units")
        .is_empty());
    assert_eq!(units(&w.t.db, None, None, 3).await.expect("units").len(), 3);
    w.t.drop_now().await;
}

#[test]
fn the_field_rules_match_the_table() {
    let ok = new_binding("A_1-B", false, Role::Developer);
    assert!(check(&ok).is_ok());
    assert!(check(&NewBinding {
        org_code: "A".repeat(20),
        ..ok.clone()
    })
    .is_ok());
    assert!(check(&NewBinding {
        org_code: "A".repeat(21),
        ..ok.clone()
    })
    .is_err());
    assert!(check(&NewBinding {
        plant_code: "ABCDE".to_owned(),
        ..ok.clone()
    })
    .is_err());
    assert!(check(&NewBinding {
        min_rank: Some(0),
        ..ok
    })
    .is_ok());
}
