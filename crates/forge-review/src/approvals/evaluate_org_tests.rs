//! ADR-0038 §3 (step C): approvers by organisation selector, named users
//! and groups mixed; delegation.

use forge_core::approver::{
    resolve, ApproverSelector, ChainUnit, OrgLevel, OrgSnapshot, PersonOrg, Unresolved,
};
use forge_db::review::DelegatorFacts;
use serde_json::json;

use super::*;

const ERIN: UserId = UserId::new(5);
const QA: GroupId = GroupId::new(9);

fn chain(primary: &str) -> Vec<ChainUnit> {
    let full = [
        ("T1", OrgLevel::Team),
        ("S1", OrgLevel::Section),
        ("D1", OrgLevel::Department),
    ];
    let start = full.iter().position(|u| u.0 == primary).expect("unit");
    full[start..]
        .iter()
        .map(|(c, l)| ChainUnit {
            code: (*c).to_owned(),
            level: *l,
        })
        .collect()
}

fn org(primary: &str, rank: i32) -> PersonOrg {
    PersonOrg::Known(OrgSnapshot {
        plant: "P1".to_owned(),
        chain: chain(primary),
        rank: Some(rank),
        positions: Vec::new(),
        digest: "d".repeat(64),
    })
}

fn sel(v: serde_json::Value) -> ApproverSelector {
    ApproverSelector::from_value(&v).expect("selector")
}

/// The author works in team T1 (section S1, department D1).
fn targets_for(
    selectors: &[ApproverSelector],
    base_has_candidates: Option<bool>,
) -> SelectorTargets {
    let author = org("T1", 15);
    SelectorTargets(
        selectors
            .iter()
            .map(|s| (s.clone(), resolve(s, &author, None, base_has_candidates)))
            .collect(),
    )
}

fn with_org(mut a: ApproverFacts, o: PersonOrg) -> ApproverFacts {
    a.approval.approver_org = Some(o);
    a
}

fn for_delegator(mut a: ApproverFacts, delegator: UserId, o: PersonOrg) -> ApproverFacts {
    a.approval.on_behalf_of = Some(delegator);
    a.approval.approver_org = Some(o);
    a.delegator = Some(DelegatorFacts {
        user: delegator,
        username: format!("user{}", delegator.get()),
        active: true,
        kind: UserKind::Human,
        role: Some(Role::Developer),
        delegation_holds: true,
        groups: BTreeSet::new(),
    });
    a
}

fn selector_rule(
    name: &str,
    required: i32,
    selectors: Vec<ApproverSelector>,
) -> StoredApprovalRule {
    StoredApprovalRule {
        selectors,
        ..rule(name, required, Vec::new())
    }
}

fn org_inputs(
    approvals: Vec<ApproverFacts>,
    rules: Vec<StoredApprovalRule>,
    targets: SelectorTargets,
) -> ApprovalInputs {
    ApprovalInputs {
        targets,
        ..inputs(approvals, rules)
    }
}

#[test]
fn a_selector_counts_the_section_manager_and_not_others() {
    let manager = sel(json!({"relation": "requester_section", "min_rank": 55}));
    let rules = vec![selector_rule("section-manager", 1, vec![manager.clone()])];
    let targets = targets_for(std::slice::from_ref(&manager), Some(true));
    // Bob is a manager of S1 (rank 55): counts.
    let ok = evaluate(&org_inputs(
        vec![with_org(approver(BOB, Role::Developer), org("S1", 55))],
        rules.clone(),
        targets.clone(),
    ));
    assert!(ok.blocks.is_empty(), "{:?}", ok.blocks);
    // Carol is staff of S1 (rank 15): not in the rule.
    let low = evaluate(&org_inputs(
        vec![with_org(approver(CAROL, Role::Developer), org("S1", 15))],
        rules.clone(),
        targets.clone(),
    ));
    assert_eq!(reason_of(&low, CAROL), Some(UncountedReason::NotInRule));
    assert_eq!(low.blocks.len(), 1);
    // An approval without a recorded snapshot (before step C) or with an
    // unknown one matches no selector.
    for o in [None, Some(PersonOrg::Unknown("stale_snapshot".to_owned()))] {
        let mut a = approver(BOB, Role::Developer);
        a.approval.approver_org = o;
        let r = evaluate(&org_inputs(vec![a], rules.clone(), targets.clone()));
        assert_eq!(reason_of(&r, BOB), Some(UncountedReason::NotInRule));
    }
}

#[test]
fn users_groups_and_selectors_mix_in_one_rule_with_a_required_count() {
    let manager = sel(json!({"relation": "requester_section", "min_rank": 55}));
    let mut mixed = selector_rule("mixed", 3, vec![manager.clone()]);
    mixed.users = vec![CAROL];
    mixed.groups = vec![QA];
    let targets = targets_for(std::slice::from_ref(&manager), Some(true));
    let mut qa = approver(DAVE, Role::Developer);
    qa.groups.insert(QA);
    let approvals = vec![
        with_org(approver(BOB, Role::Developer), org("S1", 60)), // selector
        approver(CAROL, Role::Developer),                        // named
        qa,                                                      // group
        approver(ERIN, Role::Developer),                         // none of them
    ];
    let r = evaluate(&org_inputs(
        approvals.clone(),
        vec![mixed.clone()],
        targets.clone(),
    ));
    assert!(r.blocks.is_empty(), "{:?}", r.blocks);
    assert_eq!(r.rules[0].counted, 3);
    assert_eq!(reason_of(&r, ERIN), Some(UncountedReason::NotInRule));
    // Four required: three of them count.
    mixed.required_approvals = 4;
    let short = evaluate(&org_inputs(approvals, vec![mixed], targets));
    assert_eq!(
        short.blocks,
        vec![BlockReason::ApprovalsMissing {
            rule: "mixed".to_owned(),
            required: 4,
            counted: 3,
        }]
    );
}

#[test]
fn department_relation_and_escalation_choose_the_unit() {
    let gm = sel(json!({"relation": "requester_section", "min_rank": 75, "escalate": "parent"}));
    let rules = vec![selector_rule("gm", 1, vec![gm.clone()])];
    // Someone of rank 75 sits in the department, none in the section.
    let dept_head = with_org(approver(BOB, Role::Developer), org("D1", 75));
    // Candidates in the section known: no escalation, the head does not count.
    let stay = evaluate(&org_inputs(
        vec![dept_head.clone()],
        rules.clone(),
        targets_for(std::slice::from_ref(&gm), Some(true)),
    ));
    assert_eq!(reason_of(&stay, BOB), Some(UncountedReason::NotInRule));
    // No one in the section: one level up, the head counts.
    let up = evaluate(&org_inputs(
        vec![dept_head.clone()],
        rules.clone(),
        targets_for(std::slice::from_ref(&gm), Some(false)),
    ));
    assert!(up.blocks.is_empty(), "{:?}", up.blocks);
    // Could not tell (too many people): never widened.
    let unknown = evaluate(&org_inputs(
        vec![dept_head],
        rules,
        targets_for(std::slice::from_ref(&gm), None),
    ));
    assert_eq!(unknown.blocks.len(), 1);
    // requester_department, exact: only the department's own people.
    let exact = sel(json!({"relation": "requester_department"}));
    let r = evaluate(&org_inputs(
        vec![
            with_org(approver(BOB, Role::Developer), org("D1", 75)),
            with_org(approver(CAROL, Role::Developer), org("S1", 75)),
        ],
        vec![selector_rule("dept", 1, vec![exact.clone()])],
        targets_for(std::slice::from_ref(&exact), Some(true)),
    ));
    assert!(r.blocks.is_empty());
    assert_eq!(r.rules[0].counted, 1);
}

#[test]
fn an_unresolvable_selector_counts_no_one() {
    let s = sel(json!({"relation": "requester_section"}));
    let targets = SelectorTargets(vec![(
        s.clone(),
        Err(Unresolved::RequesterUnknown("no_snapshot".to_owned())),
    )]);
    let r = evaluate(&org_inputs(
        vec![with_org(approver(BOB, Role::Developer), org("S1", 99))],
        vec![selector_rule("section", 1, vec![s])],
        targets,
    ));
    assert_eq!(reason_of(&r, BOB), Some(UncountedReason::NotInRule));
    assert_eq!(r.blocks.len(), 1);
    // A selector absent from the targets (the rule changed after the
    // read) counts no one either.
    let other = sel(json!({"relation": "requester_department"}));
    let r = evaluate(&org_inputs(
        vec![with_org(approver(BOB, Role::Developer), org("D1", 99))],
        vec![selector_rule("dept", 1, vec![other])],
        SelectorTargets::default(),
    ));
    assert_eq!(r.blocks.len(), 1);
}

#[test]
fn exclusions_come_before_any_selector() {
    let anyone = sel(json!({"relation": "requester_section"}));
    let rules = vec![selector_rule("section", 1, vec![anyone.clone()])];
    let targets = targets_for(std::slice::from_ref(&anyone), Some(true));
    let manager = org("S1", 99);
    // The author, however high.
    let own = evaluate(&org_inputs(
        vec![with_org(
            approver(AUTHOR, Role::Maintainer),
            manager.clone(),
        )],
        rules.clone(),
        targets.clone(),
    ));
    assert_eq!(reason_of(&own, AUTHOR), Some(UncountedReason::Author));
    // A stale approval.
    let mut stale = with_org(approver(BOB, Role::Developer), manager.clone());
    stale.approval.head_sha = oid('b');
    let r = evaluate(&org_inputs(vec![stale], rules.clone(), targets.clone()));
    assert_eq!(reason_of(&r, BOB), Some(UncountedReason::Stale));
    // A bot.
    let mut bot = with_org(approver(BOB, Role::Developer), manager.clone());
    bot.kind = UserKind::Bot;
    let r = evaluate(&org_inputs(vec![bot], rules.clone(), targets.clone()));
    assert_eq!(reason_of(&r, BOB), Some(UncountedReason::NotHuman));
    // A committer of the range.
    let mut i = org_inputs(
        vec![with_org(approver(BOB, Role::Developer), manager)],
        rules,
        targets,
    );
    i.committers = Committers::Known(BTreeMap::from([(BOB, 7)]));
    let r = evaluate(&i);
    assert_eq!(reason_of(&r, BOB), Some(UncountedReason::Committer));
}

#[test]
fn a_delegate_counts_for_the_delegator_once() {
    // The rule names Carol; Dave approves on her behalf.
    let rules = vec![rule("carol", 1, vec![CAROL])];
    let delegated = for_delegator(approver(DAVE, Role::Developer), CAROL, org("S1", 15));
    let r = evaluate(&inputs(vec![delegated.clone()], rules.clone()));
    assert!(r.blocks.is_empty(), "{:?}", r.blocks);
    assert_eq!(
        r.approvals[0].on_behalf_of,
        Some((CAROL, "user3".to_owned()))
    );
    // Carol and her delegate both approve: one person, counted once.
    let two = evaluate(&inputs(
        vec![delegated.clone(), approver(CAROL, Role::Developer)],
        vec![rule("carol", 2, vec![CAROL, BOB])],
    ));
    assert_eq!(two.rules[0].counted, 1);
    // A delegate's own name does not make them count for a rule naming
    // only them when they approve for someone else (it counts as Carol's).
    let r = evaluate(&inputs(vec![delegated], vec![rule("dave", 1, vec![DAVE])]));
    assert_eq!(reason_of(&r, DAVE), Some(UncountedReason::NotInRule));
}

#[test]
fn a_delegators_exclusions_apply_too() {
    let rules = vec![rule("any", 1, Vec::new())];
    // Approving for the author.
    let for_author = for_delegator(approver(DAVE, Role::Developer), AUTHOR, org("S1", 15));
    let r = evaluate(&inputs(vec![for_author], rules.clone()));
    assert_eq!(reason_of(&r, DAVE), Some(UncountedReason::Author));
    // The delegator has left (disabled).
    let mut gone = for_delegator(approver(DAVE, Role::Developer), CAROL, org("S1", 15));
    if let Some(d) = gone.delegator.as_mut() {
        d.active = false;
    }
    let r = evaluate(&inputs(vec![gone], rules.clone()));
    assert_eq!(reason_of(&r, DAVE), Some(UncountedReason::Inactive));
    // The delegation was removed or given to someone else: not counted,
    // even before the clean-up ends the approval.
    let mut removed = for_delegator(approver(DAVE, Role::Developer), CAROL, org("S1", 15));
    if let Some(d) = removed.delegator.as_mut() {
        d.delegation_holds = false;
    }
    let r = evaluate(&inputs(vec![removed], vec![rule("carol", 1, vec![CAROL])]));
    assert_eq!(reason_of(&r, DAVE), Some(UncountedReason::DelegationEnded));
    assert_eq!(r.blocks.len(), 1);
    // The delegator holds no role on the project (or lost it), or only
    // Reporter: the delegate does not count for them, even when the rule
    // names the delegator (review of ADR-0038 step C).
    for role in [None, Some(Role::Reporter), Some(Role::Guest)] {
        let mut weak = for_delegator(approver(DAVE, Role::Maintainer), CAROL, org("S1", 15));
        if let Some(d) = weak.delegator.as_mut() {
            d.role = role;
        }
        let r = evaluate(&inputs(vec![weak.clone()], rules.clone()));
        assert_eq!(
            reason_of(&r, DAVE),
            Some(UncountedReason::RoleInsufficient),
            "{role:?}"
        );
        assert_eq!(r.blocks.len(), 1);
        let named = evaluate(&inputs(vec![weak], vec![rule("carol", 1, vec![CAROL])]));
        assert_eq!(
            reason_of(&named, DAVE),
            Some(UncountedReason::RoleInsufficient)
        );
    }
    // The delegator pushed a commit of the range.
    let mut i = inputs(
        vec![for_delegator(
            approver(DAVE, Role::Developer),
            CAROL,
            org("S1", 15),
        )],
        rules,
    );
    i.committers = Committers::Known(BTreeMap::from([(CAROL, 11)]));
    let r = evaluate(&i);
    assert_eq!(reason_of(&r, DAVE), Some(UncountedReason::Committer));
}

#[test]
fn a_delegate_matches_a_selector_on_the_delegators_snapshot() {
    let manager = sel(json!({"relation": "requester_section", "min_rank": 55}));
    let rules = vec![selector_rule("section-manager", 1, vec![manager.clone()])];
    let targets = targets_for(std::slice::from_ref(&manager), Some(true));
    // Dave (staff) approves for Carol, the section manager: the recorded
    // snapshot is Carol's.
    let r = evaluate(&org_inputs(
        vec![for_delegator(
            approver(DAVE, Role::Developer),
            CAROL,
            org("S1", 55),
        )],
        rules,
        targets,
    ));
    assert!(r.blocks.is_empty(), "{:?}", r.blocks);
}
