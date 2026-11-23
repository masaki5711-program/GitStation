use serde_json::json;

use super::*;

fn unit(code: &str, level: OrgLevel) -> ChainUnit {
    ChainUnit {
        code: code.to_owned(),
        level,
    }
}

/// A person in team T1 of section S1 of department D1 of division V1.
fn person(primary: &str, rank: Option<i32>, positions: &[&str]) -> OrgSnapshot {
    let full = [
        unit("T1", OrgLevel::Team),
        unit("S1", OrgLevel::Section),
        unit("D1", OrgLevel::Department),
        unit("V1", OrgLevel::Division),
    ];
    let start = full.iter().position(|u| u.code == primary).expect("known unit");
    OrgSnapshot {
        plant: "P1".to_owned(),
        chain: full[start..].to_vec(),
        rank,
        positions: positions.iter().map(|p| (*p).to_owned()).collect(),
        digest: "d".repeat(64),
    }
}

fn sel(v: serde_json::Value) -> ApproverSelector {
    ApproverSelector::from_value(&v).expect("valid selector")
}

#[test]
fn selector_parses_and_round_trips_its_canonical_form() {
    let s = sel(json!({"relation": "requester_department", "include_descendants": true,
        "min_rank": 55, "escalate": "parent"}));
    assert_eq!(s.relation, Relation::RequesterDepartment);
    assert_eq!(s.qualifier, Qualifier::MinRank(55));
    assert_eq!(s.escalate, Escalate::Parent);
    assert_eq!(ApproverSelector::from_value(&s.to_value()), Ok(s));
    let p = sel(json!({"relation": "unit", "unit": "QA-01", "positions": ["manager", "staff"]}));
    assert_eq!(ApproverSelector::from_value(&p.to_value()), Ok(p));
}

#[test]
fn selector_refuses_what_it_cannot_decide() {
    for (v, why) in [
        (json!("x"), "not_an_object"),
        (json!({"relation": "boss"}), "relation"),
        (json!({"relation": "unit"}), "unit_required"),
        (json!({"relation": "requester_section", "unit": "S1"}), "unit_not_allowed"),
        (json!({"relation": "unit", "unit": "lower"}), "unit_format"),
        (json!({"relation": "requester_section", "include_descendants": true}), "include_descendants"),
        (json!({"relation": "requester_section", "min_rank": 1000}), "min_rank"),
        (json!({"relation": "requester_section", "min_rank": 5, "positions": ["x"]}), "min_rank_or_positions"),
        (json!({"relation": "requester_section", "positions": []}), "positions"),
        (json!({"relation": "requester_section", "positions": ["Bad"]}), "positions"),
        (json!({"relation": "requester_section", "head_only": true}), "head_only_unavailable"),
        (json!({"relation": "requester_section", "escalate": "up"}), "escalate"),
        (json!({"relation": "requester_section", "extra": 1}), "unknown_field"),
    ] {
        assert_eq!(ApproverSelector::from_value(&v), Err(why), "{v}");
    }
}

#[test]
fn person_org_round_trips_and_refuses_broken_stored_forms() {
    let known = PersonOrg::Known(person("T1", Some(55), &["manager"]));
    assert_eq!(PersonOrg::from_value(&known.to_value()), Some(known));
    let unknown = PersonOrg::Unknown("stale_snapshot".to_owned());
    assert_eq!(PersonOrg::from_value(&unknown.to_value()), Some(unknown));
    assert_eq!(PersonOrg::from_value(&json!({"plant": "P1", "chain": []})), None);
    assert_eq!(PersonOrg::from_value(&json!(null)), None);
}

#[test]
fn requester_section_matches_people_of_the_section_with_the_rank() {
    let s = sel(json!({"relation": "requester_section", "min_rank": 55}));
    let requester = PersonOrg::Known(person("T1", Some(15), &[]));
    let t = resolve(&s, &requester, None, Some(true)).expect("resolves");
    assert_eq!((t.unit.as_str(), t.exact, t.escalated), ("S1", false, false));
    assert!(matches(&s.qualifier, &t, &person("S1", Some(55), &[])));
    assert!(matches(&s.qualifier, &t, &person("T1", Some(60), &[])));
    assert!(!matches(&s.qualifier, &t, &person("S1", Some(54), &[])));
    assert!(!matches(&s.qualifier, &t, &person("S1", None, &[])), "no rank never qualifies");
    assert!(!matches(&s.qualifier, &t, &person("D1", Some(99), &[])), "above the section");
    let mut other_plant = person("S1", Some(99), &[]);
    other_plant.plant = "P2".to_owned();
    assert!(!matches(&s.qualifier, &t, &other_plant));
}

#[test]
fn requester_department_exact_or_with_descendants() {
    let requester = PersonOrg::Known(person("T1", None, &[]));
    let exact = sel(json!({"relation": "requester_department", "positions": ["general_manager"]}));
    let t = resolve(&exact, &requester, None, Some(true)).expect("resolves");
    assert!(t.exact);
    assert!(matches(&exact.qualifier, &t, &person("D1", None, &["general_manager"])));
    assert!(!matches(&exact.qualifier, &t, &person("S1", None, &["general_manager"])));
    let wide = sel(json!({"relation": "requester_department", "include_descendants": true}));
    let t = resolve(&wide, &requester, None, Some(true)).expect("resolves");
    assert!(matches(&wide.qualifier, &t, &person("S1", None, &[])));
}

#[test]
fn unit_relation_needs_the_unit_in_the_master() {
    let requester = PersonOrg::Known(person("T1", None, &[]));
    let s = sel(json!({"relation": "unit", "unit": "S1"}));
    assert_eq!(
        resolve(&s, &requester, None, Some(true)),
        Err(Unresolved::UnitNotInMaster)
    );
    let chain = [unit("S1", OrgLevel::Section), unit("D1", OrgLevel::Department)];
    let t = resolve(&s, &requester, Some(&chain), Some(true)).expect("resolves");
    assert_eq!((t.unit.as_str(), t.exact), ("S1", true));
    assert!(matches(&s.qualifier, &t, &person("S1", None, &[])));
    assert!(!matches(&s.qualifier, &t, &person("T1", None, &[])), "exact: not the team");
}

#[test]
fn escalate_parent_only_on_a_known_empty_base() {
    let requester = PersonOrg::Known(person("T1", None, &[]));
    let s = sel(json!({"relation": "requester_section", "min_rank": 75, "escalate": "parent"}));
    let up = resolve(&s, &requester, None, Some(false)).expect("resolves");
    assert_eq!((up.unit.as_str(), up.exact, up.escalated), ("D1", false, true));
    assert!(matches(&s.qualifier, &up, &person("D1", Some(75), &[])));
    // Unknown candidates or people found: never widened.
    assert_eq!(resolve(&s, &requester, None, None).map(|t| t.unit), Ok("S1".to_owned()));
    assert_eq!(resolve(&s, &requester, None, Some(true)).map(|t| t.unit), Ok("S1".to_owned()));
    // No escalation asked: stays.
    let stay = sel(json!({"relation": "requester_section", "min_rank": 75}));
    assert_eq!(resolve(&stay, &requester, None, Some(false)).map(|t| t.unit), Ok("S1".to_owned()));
}

#[test]
fn an_unknown_requester_resolves_nothing() {
    let s = sel(json!({"relation": "requester_section"}));
    let r = resolve(&s, &PersonOrg::Unknown("no_snapshot".to_owned()), None, Some(false));
    assert_eq!(r, Err(Unresolved::RequesterUnknown("no_snapshot".to_owned())));
    assert_eq!(r.unwrap_err().detail(), Some("no_snapshot"));
    // A requester in a department with no section.
    let r = resolve(&s, &PersonOrg::Known(person("D1", None, &[])), None, Some(true));
    assert_eq!(r, Err(Unresolved::RequesterNoSection));
}

#[test]
fn codes_follow_the_master_rules() {
    assert!(unit_code_ok("QA-01_X"));
    assert!(!unit_code_ok("-QA"));
    assert!(!unit_code_ok(&"A".repeat(21)));
    assert!(position_code_ok("general_manager"));
    assert!(!position_code_ok("1x"));
}
