#![allow(clippy::expect_used, clippy::unwrap_used)] // tests may fail loudly

use std::collections::{BTreeMap, BTreeSet};

use forge_core::ids::{ApprovalId, ApprovalRuleId, GroupId, MergeRequestId, UserId};
use forge_core::{ObjectId, RefName, RefPattern, Role, UncountedReason, UserKind};
use forge_db::review::{ApprovalFacts, ApproverFacts, StoredApproval, StoredApprovalRule};

use super::*;
use crate::approvals::codeowners::parse;
use crate::merges::BlockReason;

const AUTHOR: UserId = UserId::new(1);
const BOB: UserId = UserId::new(2);
const CAROL: UserId = UserId::new(3);
const DAVE: UserId = UserId::new(4);

fn oid(c: char) -> ObjectId {
    ObjectId::parse(&c.to_string().repeat(40)).expect("oid")
}

fn main() -> RefName {
    RefName::parse("refs/heads/main").expect("ref")
}

fn approver(user: UserId, role: Role) -> ApproverFacts {
    ApproverFacts {
        approval: StoredApproval {
            id: ApprovalId::new(user.get() * 10),
            merge_request: MergeRequestId::new(1),
            approver: user,
            head_sha: oid('a'),
            target_branch: main(),
            created_at: "2026-11-03T00:00:00Z".to_owned(),
            revoked_at: None,
            revoked_reason: None,
        },
        username: format!("user{}", user.get()),
        active: true,
        kind: UserKind::Human,
        role: Some(role),
        groups: BTreeSet::new(),
    }
}

fn rule(name: &str, required: i32, users: Vec<UserId>) -> StoredApprovalRule {
    StoredApprovalRule {
        id: ApprovalRuleId::new(1),
        name: name.to_owned(),
        target_pattern: None,
        required_approvals: required,
        users,
        groups: Vec::new(),
    }
}

fn inputs(approvals: Vec<ApproverFacts>, rules: Vec<StoredApprovalRule>) -> ApprovalInputs {
    ApprovalInputs {
        author: AUTHOR,
        head: oid('a'),
        target: main(),
        facts: ApprovalFacts {
            approvals,
            rules,
            ..ApprovalFacts::default()
        },
        committers: Committers::Known(BTreeMap::new()),
        changed: ChangedPathsFact::Known(vec!["src/lib.rs".to_owned()]),
        codeowners: CodeownersFact::Absent,
        committer_approvals_excluded: true,
        codeowners_enforced: true,
        minimum: 0,
    }
}

fn reason_of(report: &ApprovalReport, user: UserId) -> Option<UncountedReason> {
    report
        .approvals
        .iter()
        .find(|a| a.approver == user)
        .expect("listed")
        .uncounted_reason
}

/// ADR-0024 §8.6: a rule change changes the result (and nothing else does
/// it to the approvals).
#[test]
fn a_rule_change_changes_the_evaluation() {
    let approvals = vec![approver(BOB, Role::Developer)];
    let none = evaluate(&inputs(approvals.clone(), Vec::new()));
    assert!(none.blocks.is_empty(), "no rule: nothing required");
    assert!(none.approvals[0].counted);
    let two = evaluate(&inputs(
        approvals.clone(),
        vec![rule("backend", 2, Vec::new())],
    ));
    assert_eq!(
        two.blocks,
        vec![BlockReason::ApprovalsMissing {
            rule: "backend".to_owned(),
            required: 2,
            counted: 1
        }]
    );
    let one = evaluate(&inputs(
        approvals.clone(),
        vec![rule("backend", 1, Vec::new())],
    ));
    assert!(one.blocks.is_empty());
    // A rule naming only Carol: Bob's approval counts for nothing.
    let carol_only = evaluate(&inputs(approvals, vec![rule("backend", 1, vec![CAROL])]));
    assert_eq!(carol_only.blocks.len(), 1);
    assert_eq!(
        reason_of(&carol_only, BOB),
        Some(UncountedReason::NotInRule)
    );
    // A rule for another target does not apply.
    let mut other = rule("release", 3, Vec::new());
    other.target_pattern = Some(RefPattern::parse("refs/heads/release/*").expect("pattern"));
    let report = evaluate(&inputs(vec![approver(BOB, Role::Developer)], vec![other]));
    assert!(report.blocks.is_empty());
    assert!(!report.rules[0].applies);
}

/// A rule's groups: a direct member counts.
#[test]
fn a_rule_group_member_counts() {
    let mut bob = approver(BOB, Role::Developer);
    bob.groups.insert(GroupId::new(7));
    let mut r = rule("team", 1, Vec::new());
    r.groups = vec![GroupId::new(7)];
    assert!(evaluate(&inputs(vec![bob], vec![r.clone()]))
        .blocks
        .is_empty());
    let report = evaluate(&inputs(vec![approver(CAROL, Role::Developer)], vec![r]));
    assert_eq!(report.blocks.len(), 1);
}

/// ADR-0024 §8.1, §8.3, §8.6: the reasons an approval does not count, each
/// decided at evaluation time.
#[test]
fn an_approval_counts_only_when_valid_now() {
    let mut stale = approver(BOB, Role::Developer);
    stale.approval.head_sha = oid('b');
    let mut retargeted = approver(CAROL, Role::Developer);
    retargeted.approval.target_branch = RefName::parse("refs/heads/dev").expect("ref");
    let report = evaluate(&inputs(
        vec![stale, retargeted],
        vec![rule("r", 1, Vec::new())],
    ));
    assert_eq!(reason_of(&report, BOB), Some(UncountedReason::Stale));
    assert_eq!(reason_of(&report, CAROL), Some(UncountedReason::Stale));
    let mut inactive = approver(BOB, Role::Developer);
    inactive.active = false;
    let mut bot = approver(CAROL, Role::Developer);
    bot.kind = UserKind::Bot;
    let reporter = approver(DAVE, Role::Reporter);
    let author = approver(AUTHOR, Role::Owner);
    let report = evaluate(&inputs(
        vec![inactive, bot, reporter, author],
        vec![rule("r", 1, Vec::new())],
    ));
    assert_eq!(reason_of(&report, BOB), Some(UncountedReason::Inactive));
    assert_eq!(reason_of(&report, CAROL), Some(UncountedReason::NotHuman));
    assert_eq!(
        reason_of(&report, DAVE),
        Some(UncountedReason::RoleInsufficient)
    );
    assert_eq!(reason_of(&report, AUTHOR), Some(UncountedReason::Author));
    assert_eq!(report.blocks.len(), 1, "none of them counts");
}

/// ADR-0024 §8.3: a committer's approval does not count, with the push as
/// evidence; unknown committers count nobody (fail closed).
#[test]
fn committers_do_not_count_and_unknown_committers_fail_closed() {
    let mut i = inputs(
        vec![
            approver(BOB, Role::Developer),
            approver(CAROL, Role::Developer),
        ],
        vec![rule("r", 2, Vec::new())],
    );
    i.committers = Committers::Known(BTreeMap::from([(BOB, 42)]));
    let report = evaluate(&i);
    assert_eq!(reason_of(&report, BOB), Some(UncountedReason::Committer));
    assert_eq!(report.approvals[0].committer_push, Some(42));
    assert_eq!(
        report.blocks,
        vec![BlockReason::ApprovalsMissing {
            rule: "r".to_owned(),
            required: 2,
            counted: 1
        }]
    );
    i.committers = Committers::Unknown("push_record_missing");
    let report = evaluate(&i);
    assert_eq!(
        reason_of(&report, BOB),
        Some(UncountedReason::CommittersUnknown)
    );
    assert_eq!(
        reason_of(&report, CAROL),
        Some(UncountedReason::CommittersUnknown)
    );
}

fn owned(text: &str, owners: &[(&str, OwnerRef)], paths: &ChangedPathsFact) -> CodeownersFact {
    let file = parse(text.as_bytes()).expect("valid");
    let matched = match paths {
        ChangedPathsFact::Known(p) => file.owned_lines(p, None).expect("no deadline"),
        ChangedPathsFact::Conflict | ChangedPathsFact::Unknown(_) => BTreeMap::new(),
    };
    CodeownersFact::Valid {
        path: "CODEOWNERS",
        file,
        matched,
        owners: owners
            .iter()
            .map(|(name, r)| {
                let owner = if name.contains('/') {
                    Owner::Group((*name).to_owned())
                } else {
                    Owner::Name((*name).to_owned())
                };
                (owner, *r)
            })
            .collect(),
    }
}

/// ADR-0024 §9: each changed path's last rule needs one of its owners.
#[test]
fn codeowners_need_an_owner_per_changed_rule() {
    let mut i = inputs(vec![approver(BOB, Role::Developer)], Vec::new());
    i.changed = ChangedPathsFact::Known(vec!["src/lib.rs".to_owned(), "docs/a.md".to_owned()]);
    i.codeowners = owned(
        "*.rs @bob\ndocs/ @org/writers\n",
        &[
            ("bob", OwnerRef::User(BOB)),
            ("org/writers", OwnerRef::Group(GroupId::new(9))),
        ],
        &i.changed,
    );
    let report = evaluate(&i);
    assert_eq!(
        report.blocks,
        vec![BlockReason::CodeownerApprovalMissing {
            pattern: "docs/".to_owned(),
            owners: vec!["@org/writers".to_owned()]
        }]
    );
    assert!(report.approvals[0].counted, "Bob owns *.rs");
    let mut carol = approver(CAROL, Role::Developer);
    carol.groups.insert(GroupId::new(9));
    i.facts.approvals.push(carol);
    assert!(evaluate(&i).blocks.is_empty());
}

/// An unusable CODEOWNERS file blocks with its line and reason.
#[test]
fn an_invalid_codeowners_file_blocks() {
    let mut i = inputs(vec![approver(BOB, Role::Developer)], Vec::new());
    i.codeowners = CodeownersFact::Invalid(CodeownersError {
        line: 3,
        reason: "unknown_owner",
    });
    assert_eq!(
        evaluate(&i).blocks,
        vec![BlockReason::CodeownersInvalid {
            line: 3,
            reason: "unknown_owner"
        }]
    );
}

/// ADR-0024 §8.7 / ADR-0017 §2.7: a configuration path needs a Maintainer.
#[test]
fn a_config_change_needs_a_maintainer_approval() {
    let mut i = inputs(vec![approver(BOB, Role::Developer)], Vec::new());
    i.changed = ChangedPathsFact::Known(vec![".forge/CODEOWNERS".to_owned()]);
    let report = evaluate(&i);
    assert_eq!(
        report.blocks,
        vec![BlockReason::ConfigChangeNeedsMaintainerApproval]
    );
    assert_eq!(reason_of(&report, BOB), Some(UncountedReason::NotInRule));
    i.facts.approvals.push(approver(CAROL, Role::Maintainer));
    assert!(evaluate(&i).blocks.is_empty());
    // The author's own Maintainer approval never counts.
    let mut author = inputs(vec![approver(AUTHOR, Role::Maintainer)], Vec::new());
    author.changed = ChangedPathsFact::Known(vec!["CODEOWNERS".to_owned()]);
    assert_eq!(
        evaluate(&author).blocks,
        vec![BlockReason::ConfigChangeNeedsMaintainerApproval]
    );
}

#[test]
fn is_config_path_holds_no_protected_paths_or_ci_includes_until_m4() {
    for path in [
        ".forge/ci.yml",
        ".forge/CODEOWNERS",
        ".forge-ci.yml",
        "CODEOWNERS",
        // R5 review: ASCII case is ignored.
        ".Forge/ci.yml",
        "codeowners",
        ".FORGE-CI.YML",
    ] {
        assert!(is_config_path(path), "{path}");
    }
    for path in [
        "src/CODEOWNERS",
        "forge/x",
        ".forge",
        "ci/build.sh",
        "Makefile",
    ] {
        assert!(!is_config_path(path), "{path}");
    }
}

/// Changed paths that could not be read block, and CODEOWNERS is not
/// evaluated as "no owners".
#[test]
fn unknown_changed_paths_block_and_never_mean_no_owners() {
    let mut i = inputs(vec![approver(BOB, Role::Developer)], Vec::new());
    i.changed = ChangedPathsFact::Unknown("too_many_paths");
    i.codeowners = owned("* @bob\n", &[("bob", OwnerRef::User(BOB))], &i.changed);
    assert_eq!(
        evaluate(&i).blocks,
        vec![BlockReason::ChangedPathsUnknown {
            reason: "too_many_paths"
        }]
    );
}

/// A trial merge that conflicts cannot land: the conflict blocks it (the
/// mergeability check, the job's own trial merge: FG-REVIEW-0006), so the
/// approval half adds no `changed_paths_unknown` of its own and the job
/// keeps its conflict code (R5 review round 1 regression).
#[test]
fn a_conflicting_merge_adds_no_changed_paths_reason() {
    let mut i = inputs(vec![approver(BOB, Role::Developer)], Vec::new());
    i.changed = ChangedPathsFact::Conflict;
    assert!(evaluate(&i).blocks.is_empty());
}

/// M3 R6 (ADR-0024 §8.3): a project that turns the committer rule off
/// counts its committers' approvals — and then unknown committers do not
/// fail the approvals either (the rule is not consulted).
#[test]
fn a_project_may_count_its_committers_approvals() {
    let mut i = inputs(
        vec![
            approver(BOB, Role::Developer),
            approver(CAROL, Role::Developer),
        ],
        vec![rule("r", 2, Vec::new())],
    );
    i.committers = Committers::Known(BTreeMap::from([(BOB, 42)]));
    i.committer_approvals_excluded = false;
    let report = evaluate(&i);
    assert_eq!(reason_of(&report, BOB), None);
    assert!(report.blocks.is_empty());
    i.committers = Committers::Unknown("push_record_missing");
    assert!(evaluate(&i).blocks.is_empty());
}

/// M3 R6 (ADR-0024 §9): while CODEOWNERS enforcement is suspended, neither
/// a missing owner nor an unusable file blocks — the configuration rule
/// still does (it is not CODEOWNERS'); enforcing again brings both back.
#[test]
fn a_suspended_codeowners_enforcement_drops_only_the_codeowners_requirements() {
    let mut i = inputs(vec![approver(BOB, Role::Developer)], Vec::new());
    i.changed = ChangedPathsFact::Known(vec!["docs/a.md".to_owned()]);
    i.codeowners = owned(
        "docs/ @org/writers\n",
        &[("org/writers", OwnerRef::Group(GroupId::new(9)))],
        &i.changed,
    );
    assert_eq!(evaluate(&i).blocks.len(), 1);
    i.codeowners_enforced = false;
    let report = evaluate(&i);
    assert!(report.blocks.is_empty(), "{:?}", report.blocks);
    assert!(report.codeowners.is_empty());
    assert!(!report.codeowners_enforced);
    i.codeowners = CodeownersFact::Invalid(CodeownersError {
        line: 1,
        reason: "unknown_owner",
    });
    assert!(evaluate(&i).blocks.is_empty());
    i.changed = ChangedPathsFact::Known(vec!["CODEOWNERS".to_owned()]);
    assert_eq!(
        evaluate(&i).blocks,
        vec![BlockReason::ConfigChangeNeedsMaintainerApproval],
        "the fix of CODEOWNERS still needs a Maintainer"
    );
}

/// M3 R6 (ADR-0024 §8.6): the protected branch's minimum counts every
/// valid approval (any eligible person), on top of the rules, and makes
/// such an approval "needed" (never `not_in_rule`).
#[test]
fn the_protected_minimum_counts_any_valid_approval() {
    let mut i = inputs(vec![approver(BOB, Role::Developer)], Vec::new());
    i.minimum = 2;
    let report = evaluate(&i);
    assert_eq!(
        report.blocks,
        vec![BlockReason::ApprovalsMissing {
            rule: PROTECTED_BRANCH_RULE.to_owned(),
            required: 2,
            counted: 1
        }]
    );
    assert_eq!(reason_of(&report, BOB), None);
    assert!(report
        .rules
        .iter()
        .any(|r| r.name == PROTECTED_BRANCH_RULE && r.applies));
    i.facts.approvals.push(approver(CAROL, Role::Developer));
    assert!(evaluate(&i).blocks.is_empty());
    // An invalid approval (the author's) does not count toward it.
    i.facts.approvals = vec![
        approver(AUTHOR, Role::Maintainer),
        approver(BOB, Role::Developer),
    ];
    assert_eq!(evaluate(&i).blocks.len(), 1);
}
