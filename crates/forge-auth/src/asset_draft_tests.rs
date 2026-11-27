use forge_core::RefName;
use forge_errors::ErrorCode;

use crate::action::Action;
use crate::model::{
    AssetDraftTarget, Principal, ProjectView, RefProtection, RefUpdate, Resource, Role, Visibility,
};

fn name(s: &str) -> RefName {
    RefName::parse(s).expect("valid ref")
}

fn target() -> AssetDraftTarget {
    AssetDraftTarget {
        project: ProjectView {
            visibility: Visibility::Private,
            role: None,
            pending_deletion: false,
        },
        branch: name("refs/heads/ai-draft/ir-1"),
        source_branch: name("refs/heads/improve/ir-1"),
        update: RefUpdate::Create,
        protection: None,
        mr_open: true,
        mr_draft: true,
        ai_assets_enabled: true,
    }
}

fn reason(e: &forge_errors::ForgeError) -> String {
    e.context()
        .iter()
        .find(|(k, _)| *k == "reason")
        .map(|(_, v)| v.clone())
        .unwrap_or_default()
}

fn decide(t: AssetDraftTarget) -> Result<(), forge_errors::ForgeError> {
    crate::authorize(
        &Principal::asset_draft(),
        Action::RepoPush,
        &Resource::AssetDraft(t),
    )
}

#[test]
fn the_writer_creates_the_draft_branch_of_a_draft_improvement() {
    decide(target()).expect("allowed");
}

#[test]
fn draft_never_marks_ready() {
    // Marking ready is `MergeRequestUpdate`; nothing but the branch
    // creation is ever allowed, on the merge request or its draft target.
    for action in Action::ALL
        .iter()
        .copied()
        .filter(|a| *a != Action::RepoPush)
        // The act-as confirmations answer FG-REPO-0005 for everyone they
        // refuse, before any arm (as in the matrix's generated denies).
        .filter(|a| !matches!(a, Action::ActAsConfirmView | Action::ActAsAssertionIssue))
    {
        let e = crate::authorize(
            &Principal::asset_draft(),
            action,
            &Resource::AssetDraft(target()),
        )
        .expect_err("refused");
        assert_eq!(e.code(), ErrorCode::Forbidden, "{action:?}");
        assert_eq!(reason(&e), "asset_draft_not_allowed", "{action:?}");
    }
    let e = crate::authorize(
        &Principal::asset_draft(),
        Action::MergeRequestUpdate,
        &Resource::AssetDraft(target()),
    )
    .expect_err("refused");
    assert_eq!(reason(&e), "asset_draft_not_allowed");
}

/// One condition of the target broken.
type Break = fn(&mut AssetDraftTarget);

#[test]
fn each_condition_of_the_target_refuses() {
    let cases: [(Break, &str); 8] = [
        (
            |t| t.ai_assets_enabled = false,
            "asset_draft_ai_assets_disabled",
        ),
        (
            |t| t.branch = name("refs/heads/improve/ir-1"),
            "asset_draft_branch_only",
        ),
        (
            |t| t.branch = name("refs/heads/ai-draft/other"),
            "asset_draft_branch_only",
        ),
        (
            |t| t.source_branch = name("refs/heads/feature/x"),
            "asset_draft_branch_only",
        ),
        (
            |t| t.update = RefUpdate::FastForward,
            "asset_draft_create_only",
        ),
        (
            |t| t.protection = RefProtection::effective(Some(Role::Developer)),
            "asset_draft_protected_branch",
        ),
        (|t| t.mr_open = false, "asset_draft_merge_request_not_open"),
        (|t| t.mr_draft = false, "asset_draft_not_draft"),
    ];
    for (break_it, want) in cases {
        let mut t = target();
        break_it(&mut t);
        let e = decide(t).expect_err(want);
        assert_eq!(e.code(), ErrorCode::Forbidden, "{want}");
        assert_eq!(reason(&e), want);
    }
    let mut t = target();
    t.project.pending_deletion = true;
    assert_eq!(
        decide(t).expect_err("deleted").code(),
        ErrorCode::PendingDeletion
    );
}

#[test]
fn nobody_else_reaches_the_draft_target() {
    for p in [Principal::auto_merge(), Principal::anonymous()] {
        assert!(crate::authorize(&p, Action::RepoPush, &Resource::AssetDraft(target())).is_err());
    }
}
