//! The permission matrix rows of `pfs-ai-bot` (plan 17 P17-5, §3,
//! ADR-0035 Amendment P17-5): PFS's integration, a service account whose
//! only membership is the `pfs-ai` group (Developer), holding an
//! `improve_draft` token (`crate::improve_draft`).
//!
//! - It sees its member projects only: an internal project outside the
//!   group is the invisible project's 404, like a private one.
//! - It creates new `refs/heads/improve/*` branches only (never moves
//!   one: a revision is a new branch and a new draft), and
//!   opens draft merge requests from such a branch only; it comments.
//! - It never marks a merge request ready, approves, merges, closes,
//!   resolves, releases, deploys or administers anything — a Maintainer
//!   membership included ([`generated_denies`] for every action it may
//!   not ask for).
//! - The scope is a service account's: a person's or a bot's token with it
//!   is refused, and a disabled account is disabled first.
//! - Which paths a push changes is the pre-receive hook's
//!   (`bot_cannot_touch_protected_path`, forge-git, FG-GIT-0039).

use forge_errors::ErrorCode as E;

use crate::action::Action as A;
use crate::matrix::{Actor as W, Expect, Expect::Allow, Fixture as F, Row};
use crate::model::RefUpdate as U;
#[cfg(test)]
use crate::model::Role;

/// The improvement-draft token's holder in a row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Bot {
    /// `pfs-ai-bot`: a service account, Developer by its `pfs-ai`
    /// membership.
    Developer,
    /// The same with a Maintainer membership (the role lends nothing the
    /// token does not allow).
    Maintainer,
    /// The same with a Reporter membership (below a push).
    Reporter,
    /// The same, no membership on the row's project (outside `pfs-ai`).
    NonMember,
    /// The same, its token issued with `is_agent`.
    Agent,
    /// The same, the account disabled.
    Disabled,
    /// A person's token with the scope (forge-db never issues one).
    Person,
    /// A bot account's token with the scope (forge-db never issues one).
    NoPerson,
}

/// What a row of this file decides on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Target {
    /// `refs/heads/improve/req-1` of a private project.
    ImproveRef(U),
    /// `refs/heads/main` of a private project.
    MainRef(U),
    /// `refs/heads/feature` of a private project.
    FeatureRef,
    /// `refs/tags/improve/v1` of a private project (a create).
    ImproveTag,
    /// `refs/heads/improve/req-1` of an internal project.
    InternalImproveRef,
    /// Opening a merge request: draft or not, from `improve/req-1` or from
    /// `feature`, into `main`, by the caller.
    Open { draft: bool, improve: bool },
    /// The caller's own open draft `improve/req-1` → `main`, the update
    /// asking for this draft flag (`None`: title / description only).
    OwnDraft { set_draft: Option<bool> },
    /// Another person's open, ready merge request `feature` → `main`.
    OtherReady,
    /// A one-step revert's merge request (`revert/mr-1` → `main`, ready).
    Revert,
}

#[cfg(test)]
impl Bot {
    pub(crate) fn principal(self) -> crate::model::Principal {
        use crate::model::{AccountKind, Credential, Principal, Scope};
        let kind = match self {
            Bot::Person => AccountKind::Human,
            Bot::NoPerson => AccountKind::Bot,
            Bot::Developer
            | Bot::Maintainer
            | Bot::Reporter
            | Bot::NonMember
            | Bot::Agent
            | Bot::Disabled => AccountKind::ServiceAccount,
        };
        Principal::authenticated(
            crate::matrix::ME,
            false,
            self != Bot::Disabled,
            Credential::Token(vec![Scope::ImproveDraft]),
            kind,
        )
        .as_agent(self == Bot::Agent)
    }

    pub(crate) fn role(self) -> Option<Role> {
        match self {
            Bot::NonMember => None,
            Bot::Reporter => Some(Role::Reporter),
            Bot::Maintainer => Some(Role::Maintainer),
            Bot::Developer | Bot::Agent | Bot::Disabled | Bot::Person | Bot::NoPerson => {
                Some(Role::Developer)
            }
        }
    }
}

#[cfg(test)]
impl Target {
    pub(crate) fn resource(self, role: Option<Role>) -> crate::model::Resource {
        use crate::model::{
            MergeRequestEdit, MergeRequestState, ProjectView, Resource, Visibility,
        };
        let name =
            |raw: &str| forge_core::RefName::parse(raw).expect("fixture ref names are valid");
        let private = ProjectView {
            visibility: Visibility::Private,
            role,
            pending_deletion: false,
        };
        let push = |project, raw: &str, update| Resource::Ref {
            project,
            name: name(raw),
            update,
            protection: None,
        };
        let mr = |own: bool, draft, source: &str, set_draft| Resource::MergeRequest {
            project: private,
            author: if own {
                crate::matrix::ME
            } else {
                crate::matrix::OTHER
            },
            state: MergeRequestState::Open,
            draft,
            source_branch: name(source),
            target_branch: name("refs/heads/main"),
            edit: MergeRequestEdit {
                target_branch: None,
                draft: set_draft,
            },
            target_protection: None,
        };
        match self {
            Target::ImproveRef(u) => push(private, "refs/heads/improve/req-1", u),
            Target::MainRef(u) => push(private, "refs/heads/main", u),
            Target::FeatureRef => push(private, "refs/heads/feature", U::Create),
            Target::ImproveTag => push(private, "refs/tags/improve/v1", U::Create),
            Target::InternalImproveRef => push(
                ProjectView {
                    visibility: Visibility::Internal,
                    ..private
                },
                "refs/heads/improve/req-1",
                U::FastForward,
            ),
            Target::Open { draft, improve } => mr(
                true,
                draft,
                if improve {
                    "refs/heads/improve/req-1"
                } else {
                    "refs/heads/feature"
                },
                None,
            ),
            Target::OwnDraft { set_draft } => mr(true, true, "refs/heads/improve/req-1", set_draft),
            Target::OtherReady => mr(false, false, "refs/heads/feature", None),
            Target::Revert => mr(true, false, "refs/heads/revert/mr-1", None),
        }
    }
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

const BOT: W = W::PfsAiBot(Bot::Developer);
const fn t(target: Target) -> F {
    F::PfsAiBot(target)
}
const DRAFT_IMPROVE: F = t(Target::Open {
    draft: true,
    improve: true,
});

#[rustfmt::skip]
pub(crate) const PFS_AI_BOT_ROWS: &[Row] = &[
    // What it does: read its member project, push improve/*, open a draft,
    // comment.
    row(A::PathResolve, BOT, F::ResolvedPrivate, Allow),
    row(A::ProjectRead, BOT, F::PrivateProject, Allow),
    row(A::RepoRead, BOT, F::PrivateProject, Allow),
    row(A::MergeRequestList, BOT, F::PrivateProject, Allow),
    row(A::RepoPush, BOT, F::PrivateProject, Allow),
    row(A::RepoPush, BOT, t(Target::ImproveRef(U::Create)), Allow),
    row(A::RepoPush, W::PfsAiBot(Bot::Agent), t(Target::ImproveRef(U::Create)), Allow),
    row(A::MergeRequestCreate, BOT, DRAFT_IMPROVE, Allow),
    row(A::MergeRequestRead, BOT, t(Target::OwnDraft { set_draft: None }), Allow),
    row(A::MergeRequestComment, BOT, t(Target::OtherReady), Allow),
    // bot_denied_outside_pfs_ai_group: no membership is the invisible
    // project's 404, an internal project included.
    row(A::ProjectRead, W::PfsAiBot(Bot::NonMember), F::PrivateProject, deny(E::NotFound)),
    row(A::PathResolve, W::PfsAiBot(Bot::NonMember), F::ResolvedInternal, deny(E::NotFound)),
    row(A::ProjectRead, W::PfsAiBot(Bot::NonMember), F::InternalProject, deny(E::NotFound)),
    row(A::RepoRead, W::PfsAiBot(Bot::NonMember), F::InternalProject, deny(E::NotFound)),
    row(A::MergeRequestList, W::PfsAiBot(Bot::NonMember), F::InternalProject, deny(E::NotFound)),
    row(A::RepoPush, W::PfsAiBot(Bot::NonMember), t(Target::InternalImproveRef), deny(E::NotFound)),
    row(A::MergeRequestCreate, W::PfsAiBot(Bot::NonMember), DRAFT_IMPROVE, deny(E::NotFound)),
    row(A::MergeRequestRead, W::PfsAiBot(Bot::NonMember), t(Target::OwnDraft { set_draft: None }), deny(E::NotFound)),
    row(A::MergeRequestComment, W::PfsAiBot(Bot::NonMember), t(Target::OtherReady), deny(E::NotFound)),
    row(A::RepoPush, W::PfsAiBot(Bot::Reporter), F::PrivateProject, deny(E::Forbidden)),
    row(A::MergeRequestCreate, BOT, t(Target::Revert), deny(E::Forbidden)),
    // bot_branch_outside_improve_denied: no other branch, no tag, no
    // rewrite of its own.
    row(A::RepoPush, BOT, t(Target::MainRef(U::FastForward)), deny(E::Forbidden)),
    row(A::RepoPush, BOT, t(Target::MainRef(U::Create)), deny(E::Forbidden)),
    row(A::RepoPush, BOT, t(Target::FeatureRef), deny(E::Forbidden)),
    row(A::RepoPush, BOT, t(Target::ImproveTag), deny(E::Forbidden)),
    row(A::RepoPush, BOT, t(Target::ImproveRef(U::FastForward)), deny(E::Forbidden)),
    row(A::RepoPush, BOT, t(Target::ImproveRef(U::Force)), deny(E::Forbidden)),
    row(A::RepoPush, BOT, t(Target::ImproveRef(U::Delete)), deny(E::Forbidden)),
    row(A::RepoPush, W::PfsAiBot(Bot::Maintainer), t(Target::MainRef(U::FastForward)), deny(E::Forbidden)),
    // Below a push by membership: the role still decides.
    row(A::RepoPush, W::PfsAiBot(Bot::Reporter), t(Target::ImproveRef(U::Create)), deny(E::Forbidden)),
    row(A::MergeRequestCreate, W::PfsAiBot(Bot::Reporter), DRAFT_IMPROVE, deny(E::Forbidden)),
    // Drafts only, from improve/* only.
    row(A::MergeRequestCreate, BOT, t(Target::Open { draft: false, improve: true }), deny(E::Forbidden)),
    row(A::MergeRequestCreate, BOT, t(Target::Open { draft: true, improve: false }), deny(E::Forbidden)),
    row(A::MergeRequestCreate, W::PfsAiBot(Bot::Maintainer), t(Target::Open { draft: false, improve: true }), deny(E::Forbidden)),
    // bot_cannot_mark_ready: no update at all — the ready flag, a title —
    // nor approve, merge, close, resolve; a Maintainer membership too.
    row(A::MergeRequestUpdate, BOT, t(Target::OwnDraft { set_draft: Some(false) }), deny(E::InsufficientScope)),
    row(A::MergeRequestUpdate, W::PfsAiBot(Bot::Maintainer), t(Target::OwnDraft { set_draft: Some(false) }), deny(E::InsufficientScope)),
    row(A::MergeRequestUpdate, BOT, t(Target::OwnDraft { set_draft: None }), deny(E::InsufficientScope)),
    row(A::MergeRequestApprove, BOT, t(Target::OtherReady), deny(E::InsufficientScope)),
    row(A::MergeRequestMerge, W::PfsAiBot(Bot::Maintainer), t(Target::OtherReady), deny(E::InsufficientScope)),
    row(A::MergeRequestClose, BOT, t(Target::OwnDraft { set_draft: None }), deny(E::InsufficientScope)),
    // The scope is a service account's only.
    row(A::ProjectRead, W::PfsAiBot(Bot::Person), F::PrivateProject, deny(E::Forbidden)),
    row(A::RepoPush, W::PfsAiBot(Bot::Person), t(Target::ImproveRef(U::Create)), deny(E::Forbidden)),
    row(A::ProjectRead, W::PfsAiBot(Bot::NoPerson), F::PrivateProject, deny(E::Forbidden)),
    // A disabled account, before anything else.
    row(A::ProjectRead, W::PfsAiBot(Bot::Disabled), F::PrivateProject, deny(E::AccountDisabled)),
    row(A::RepoPush, W::PfsAiBot(Bot::Disabled), t(Target::ImproveRef(U::Create)), deny(E::AccountDisabled)),
    // A project pending deletion: as for everyone.
    row(A::ProjectRead, BOT, F::DeletedProject, deny(E::PendingDeletion)),
];

/// The repository browse API and the reads decided like it (spec refs,
/// specs, context): the same rows for each, so they stay twins.
pub(crate) fn browse_rows() -> Vec<Row> {
    [A::RepoBrowse, A::SpecRefRead, A::SpecRead, A::ContextRead]
        .into_iter()
        .flat_map(|a| {
            [
                row(a, BOT, F::PrivateProject, Allow),
                row(
                    a,
                    W::PfsAiBot(Bot::NonMember),
                    F::InternalProject,
                    deny(E::NotFound),
                ),
            ]
        })
        .collect()
}

/// Every action the token may not ask for, refused on its own member
/// project (`improve_draft_not_allowed`; the act-as confirmations answer
/// `FG-REPO-0005` as for everyone they refuse). Public actions pass for
/// everyone.
pub(crate) fn generated_denies() -> Vec<Row> {
    A::ALL
        .iter()
        .filter(|a| !a.is_public() && !crate::improve_draft::allowed(**a))
        .map(|&a| {
            let code = if matches!(a, A::ActAsConfirmView | A::ActAsAssertionIssue) {
                E::NotFound
            } else {
                E::InsufficientScope
            };
            row(
                a,
                W::PfsAiBot(Bot::Maintainer),
                F::PrivateProject,
                deny(code),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decide(actor: W, action: A, fixture: F) -> Result<(), forge_errors::ForgeError> {
        crate::authorize(&actor.principal(), action, &fixture.resource(actor))
    }

    fn reason(e: &forge_errors::ForgeError) -> String {
        e.context()
            .iter()
            .find(|(k, _)| *k == "reason")
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    }

    #[test]
    fn bot_denied_outside_pfs_ai_group() {
        let outside = W::PfsAiBot(Bot::NonMember);
        for (action, fixture) in [
            (A::ProjectRead, F::InternalProject),
            (A::RepoRead, F::InternalProject),
            (A::RepoBrowse, F::InternalProject),
            (A::ProjectRead, F::PrivateProject),
            (A::RepoPush, t(Target::InternalImproveRef)),
        ] {
            let e = decide(outside, action, fixture).expect_err("outside pfs-ai");
            assert_eq!(e.code(), E::NotFound, "{action:?} {fixture:?}");
            assert_eq!(reason(&e), "improve_draft_not_member");
        }
        // A person reads the same internal project: the 404 is the bot's.
        assert!(decide(W::NonMember, A::ProjectRead, F::InternalProject).is_ok());
        assert!(decide(BOT, A::ProjectRead, F::PrivateProject).is_ok());
    }

    #[test]
    fn bot_cannot_mark_ready() {
        for bot in [BOT, W::PfsAiBot(Bot::Maintainer), W::PfsAiBot(Bot::Agent)] {
            let ready = t(Target::OwnDraft {
                set_draft: Some(false),
            });
            let e = decide(bot, A::MergeRequestUpdate, ready).expect_err("ready");
            assert_eq!(e.code(), E::InsufficientScope, "{bot:?}");
            assert_eq!(reason(&e), "improve_draft_not_allowed");
            // Opening one ready is refused too.
            let open_ready = t(Target::Open {
                draft: false,
                improve: true,
            });
            let e = decide(bot, A::MergeRequestCreate, open_ready).expect_err("open ready");
            assert_eq!(reason(&e), "draft_only", "{bot:?}");
            for action in [A::MergeRequestApprove, A::MergeRequestMerge] {
                assert!(decide(bot, action, t(Target::OtherReady)).is_err());
            }
        }
        // The same person's API token marks its own draft ready.
        let person = crate::model::Principal::authenticated(
            crate::matrix::ME,
            false,
            true,
            crate::model::Credential::Token(vec![crate::model::Scope::Api]),
            crate::model::AccountKind::Human,
        );
        let ready = Target::OwnDraft {
            set_draft: Some(false),
        }
        .resource(Some(Role::Developer));
        assert!(crate::authorize(&person, A::MergeRequestUpdate, &ready).is_ok());
    }

    #[test]
    fn bot_branch_outside_improve_denied() {
        for (target, why) in [
            (Target::MainRef(U::FastForward), "improve_branch_only"),
            (Target::FeatureRef, "improve_branch_only"),
            (Target::ImproveTag, "improve_branch_only"),
            (
                Target::ImproveRef(U::FastForward),
                "improve_draft_create_only",
            ),
            (Target::ImproveRef(U::Force), "improve_draft_create_only"),
            (Target::ImproveRef(U::Delete), "improve_draft_create_only"),
        ] {
            let e = decide(BOT, A::RepoPush, t(target)).expect_err("outside improve/*");
            assert_eq!(e.code(), E::Forbidden, "{target:?}");
            assert_eq!(reason(&e), why, "{target:?}");
        }
        let from_feature = t(Target::Open {
            draft: true,
            improve: false,
        });
        let e = decide(BOT, A::MergeRequestCreate, from_feature).expect_err("feature");
        assert_eq!(reason(&e), "improve_branch_only");
        assert!(decide(BOT, A::RepoPush, t(Target::ImproveRef(U::Create))).is_ok());
    }

    #[test]
    fn the_scope_is_a_service_accounts_only() {
        for bot in [Bot::Person, Bot::NoPerson] {
            let e = decide(W::PfsAiBot(bot), A::ProjectRead, F::PrivateProject)
                .expect_err("not a service account");
            assert_eq!(reason(&e), "improve_draft_not_service_account", "{bot:?}");
        }
    }

    /// Every allowed action has an allowed row, so the allow list and the
    /// rows cannot drift apart.
    #[test]
    fn every_allowed_action_has_an_allowed_bot_row() {
        for &a in A::ALL.iter().filter(|a| crate::improve_draft::allowed(**a)) {
            assert!(
                PFS_AI_BOT_ROWS
                    .iter()
                    .chain(browse_rows().iter())
                    .any(|r| r.action == a
                        && r.expect == Allow
                        && matches!(r.actor, W::PfsAiBot(_))),
                "{a:?}"
            );
        }
    }
}
