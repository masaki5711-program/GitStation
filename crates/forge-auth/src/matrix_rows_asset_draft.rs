//! The permission matrix rows of the AI asset draft (plan 17 P17-10,
//! ADR-0039 Amendment P17-10):
//!
//! - The writer (`W::AssetDraft`) may create (`RepoPush`) the draft branch
//!   `ai-draft/<id>` of an open draft improvement merge request
//!   (`improve/<id>`) of a project whose `ai_assets.enabled` is on — on
//!   `Resource::AssetDraft` only, and only as a creation of an unprotected
//!   branch. Each condition has its deny row.
//! - It is refused every other action ([`generated_denies`]): it never
//!   marks a merge request ready, approves, merges, comments, reads or
//!   changes a setting.
//! - Nobody else reaches `Resource::AssetDraft` (`asset_draft_resource`).

use forge_errors::ErrorCode as E;

use crate::action::Action as A;
use crate::matrix::{Actor as W, Expect, Expect::Allow, Fixture as F, Row};

/// The cases of the writer's target (each breaks one condition but `Ok`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Case {
    Ok,
    AiAssetsOff,
    NotTheDraftBranch,
    SourceNotImprove,
    NotACreation,
    Protected,
    Closed,
    Ready,
    Deleted,
}

const fn row(action: A, actor: W, fixture: F, expect: Expect) -> Row {
    Row {
        action,
        actor,
        fixture,
        expect,
    }
}

const fn deny(code: E) -> Expect {
    Expect::Deny(code)
}

const OK: F = F::AssetDraft(Case::Ok);

/// Every explicit row of the AI asset draft (`matrix::ROWS` appends them).
#[rustfmt::skip]
pub(crate) const ASSET_DRAFT_ROWS: &[Row] = &[
    row(A::RepoPush, W::AssetDraft, OK, Allow),
    row(A::RepoPush, W::AssetDraft, F::AssetDraft(Case::AiAssetsOff), deny(E::Forbidden)),
    row(A::RepoPush, W::AssetDraft, F::AssetDraft(Case::NotTheDraftBranch), deny(E::Forbidden)),
    row(A::RepoPush, W::AssetDraft, F::AssetDraft(Case::SourceNotImprove), deny(E::Forbidden)),
    row(A::RepoPush, W::AssetDraft, F::AssetDraft(Case::NotACreation), deny(E::Forbidden)),
    row(A::RepoPush, W::AssetDraft, F::AssetDraft(Case::Protected), deny(E::Forbidden)),
    row(A::RepoPush, W::AssetDraft, F::AssetDraft(Case::Closed), deny(E::Forbidden)),
    row(A::RepoPush, W::AssetDraft, F::AssetDraft(Case::Ready), deny(E::Forbidden)),
    row(A::RepoPush, W::AssetDraft, F::AssetDraft(Case::Deleted), deny(E::PendingDeletion)),
    // A plain project push is no draft target: the writer pushes nothing else.
    row(A::RepoPush, W::AssetDraft, F::PrivateProject, deny(E::Forbidden)),
    // Nobody else reaches the draft target, an administrator included.
    row(A::RepoPush, W::Admin, OK, deny(E::Forbidden)),
    row(A::RepoPush, W::Maintainer, OK, deny(E::Forbidden)),
    row(A::RepoPush, W::Owner, OK, deny(E::Forbidden)),
    row(A::RepoPush, W::Developer, OK, deny(E::Forbidden)),
    row(A::RepoPush, W::MaintainerTokenApi, OK, deny(E::Forbidden)),
    row(A::RepoPush, W::AgentMaintainerTokenApi, OK, deny(E::Forbidden)),
    row(A::RepoPush, W::Autofix, OK, deny(E::Forbidden)),
    row(A::RepoPush, W::AutoMerge, OK, deny(E::Forbidden)),
];

/// The writer is refused every action it may not perform, on a project and
/// on its own target alike (`reason=asset_draft_not_allowed`: marking
/// ready, approving, merging, commenting, reading, settings; the act-as
/// confirmations answer `FG-REPO-0005` as for everyone they refuse).
pub(crate) fn generated_denies() -> Vec<Row> {
    A::ALL
        .iter()
        .filter(|a| !crate::asset_draft::asset_draft_may(**a))
        .flat_map(|&a| {
            let code = if matches!(a, A::ActAsConfirmView | A::ActAsAssertionIssue) {
                E::NotFound
            } else {
                E::Forbidden
            };
            [
                row(a, W::AssetDraft, F::PrivateProject, deny(code)),
                row(a, W::AssetDraft, OK, deny(code)),
            ]
        })
        .collect()
}

/// The resource of a [`F::AssetDraft`] row.
#[cfg(test)]
pub(crate) fn target_resource(case: Case) -> crate::model::Resource {
    use crate::model::{
        AssetDraftTarget, ProjectView, RefProtection, RefUpdate, Resource, Role, Visibility,
    };
    let name = |s: &str| forge_core::RefName::parse(s).expect("fixture ref names are valid");
    let mut t = AssetDraftTarget {
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
    };
    match case {
        Case::Ok => {}
        Case::AiAssetsOff => t.ai_assets_enabled = false,
        Case::NotTheDraftBranch => t.branch = name("refs/heads/ai-draft/ir-2"),
        Case::SourceNotImprove => t.source_branch = name("refs/heads/feature/ir-1"),
        Case::NotACreation => t.update = RefUpdate::FastForward,
        Case::Protected => t.protection = RefProtection::effective(Some(Role::Developer)),
        Case::Closed => t.mr_open = false,
        Case::Ready => t.mr_draft = false,
        Case::Deleted => t.project.pending_deletion = true,
    }
    Resource::AssetDraft(t)
}
