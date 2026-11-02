use forge_core::{GroupId, NamespaceId, ProjectId, UserId};
use forge_errors::ErrorCode;

use crate::matrix::{Actor, Fixture, ME, ROWS};
use crate::{
    authorize, prove, Action, Credential, InstanceCtx, LoadedNamespace, LoadedProject,
    LoadedTarget, NamespaceView, Principal, ProjectCtx, ProjectView, ResolvedCtx, ResolvedVia,
    Resource, ResourceCtx, Role, Scope, UnresolvedCtx, UnresolvedReason, Visibility,
};

/// Any matrix resource as a context (the id is irrelevant to the decision).
/// Only possible inside this crate: `ResourceCtx` is sealed.
#[derive(Debug)]
struct RowCtx(Resource);

impl crate::prove::sealed::Sealed for RowCtx {}

impl ResourceCtx for RowCtx {
    type Id = ();
    fn resource(&self) -> Resource {
        self.0.clone()
    }
    fn id(&self) {}
}

fn session() -> Principal {
    Principal::authenticated(ME, false, true, Credential::Session)
}

fn private(role: Option<Role>) -> ProjectView {
    ProjectView {
        visibility: Visibility::Private,
        role,
        pending_deletion: false,
    }
}

fn project(id: i64, role: Option<Role>) -> ProjectCtx {
    ProjectCtx::new(LoadedProject {
        id: ProjectId::new(id),
        visibility: Visibility::Private,
        role,
        pending_deletion: false,
    })
}

/// A resolved context whose target has `view` (owner ids are fixed per
/// kind: user 1, group 3, project 9).
fn resolved(id: i64, via: ResolvedVia, view: NamespaceView) -> ResolvedCtx {
    let target = match view {
        NamespaceView::User => LoadedTarget::User(UserId::new(1)),
        NamespaceView::Group { role } => LoadedTarget::Group {
            id: GroupId::new(3),
            role,
        },
        NamespaceView::Project(p) => LoadedTarget::Project(LoadedProject {
            id: ProjectId::new(9),
            visibility: p.visibility,
            role: p.role,
            pending_deletion: p.pending_deletion,
        }),
    };
    ResolvedCtx::new(LoadedNamespace {
        namespace: NamespaceId::new(id),
        via,
        target,
    })
}

#[test]
fn prove_returns_the_action_and_id_it_decided() {
    let ctx = project(42, Some(Role::Reporter));
    let proof = prove(&session(), Action::ProjectRead, ctx.clone()).expect("reporter reads");
    assert_eq!(proof.action(), Action::ProjectRead);
    assert_eq!(proof.id(), &ProjectId::new(42));
    assert_eq!(proof.ctx(), &ctx);
    assert_eq!(proof.ctx().view(), private(Some(Role::Reporter)));

    let ctx = resolved(
        7,
        ResolvedVia::Live,
        NamespaceView::Project(private(Some(Role::Guest))),
    );
    let proof = prove(&session(), Action::PathResolve, ctx).expect("guest resolves");
    assert_eq!(proof.action(), Action::PathResolve);
    assert_eq!(proof.id(), &NamespaceId::new(7));
    assert_eq!(proof.ctx().via(), ResolvedVia::Live);
}

#[test]
fn a_context_decides_on_the_row_its_id_comes_from() {
    // Two loaded projects: the proof for one carries that one's id and view.
    let visible = project(1, Some(Role::Reporter));
    let invisible = project(2, None);
    assert_eq!(visible.id(), ProjectId::new(1));
    assert_eq!(visible.resource(), Resource::Project(visible.view()));
    let err = prove(&session(), Action::ProjectRead, invisible).expect_err("not visible");
    assert_eq!(err.code(), ErrorCode::NotFound);
    let proof = prove(&session(), Action::ProjectRead, visible).expect("visible");
    assert_eq!(proof.id(), &ProjectId::new(1));
}

/// Property over the whole matrix: for every action × every actor × every
/// fixture that appears in it, `prove()` allows exactly when `authorize()`
/// does and denies with the same code and the same context.
#[test]
fn prove_denies_exactly_like_authorize() {
    let mut seen_actors: Vec<Actor> = Vec::new();
    let mut seen_fixtures: Vec<Fixture> = Vec::new();
    for r in ROWS {
        if !seen_actors.contains(&r.actor) {
            seen_actors.push(r.actor);
        }
        if !seen_fixtures.contains(&r.fixture) {
            seen_fixtures.push(r.fixture);
        }
    }

    let mut checked = 0usize;
    let mut failures = Vec::new();
    for &action in Action::ALL {
        for &actor in &seen_actors {
            for &fixture in &seen_fixtures {
                let principal = actor.principal();
                let resource = fixture.resource(actor);
                let decided = authorize(&principal, action, &resource);
                let proved = prove(&principal, action, RowCtx(resource));
                let same = match (&decided, &proved) {
                    (Ok(()), Ok(proof)) => proof.action() == action,
                    (Err(a), Err(b)) => a.code() == b.code() && a.context() == b.context(),
                    (Ok(()), Err(_)) | (Err(_), Ok(_)) => false,
                };
                if !same {
                    failures.push(format!(
                        "{action:?} {actor:?} {fixture:?}: authorize {decided:?}, prove {:?}",
                        proved.map(|p| p.action())
                    ));
                }
                checked += 1;
            }
        }
    }
    assert!(
        failures.is_empty(),
        "prove diverged:\n{}",
        failures.join("\n")
    );
    assert!(checked > 1000, "only {checked} combinations checked");
}

#[test]
fn guest_on_private_may_resolve_but_not_project_read() {
    let guest = private(Some(Role::Guest));
    let ctx = resolved(1, ResolvedVia::Live, NamespaceView::Project(guest));
    assert!(prove(&session(), Action::PathResolve, ctx).is_ok());
    let err = prove(
        &session(),
        Action::ProjectRead,
        project(1, Some(Role::Guest)),
    )
    .expect_err("guest cannot read the project");
    assert_eq!(err.code(), ErrorCode::Forbidden);
}

#[test]
fn webshell_is_public_and_needs_no_scope() {
    assert!(Action::WebShell.is_public());
    assert_eq!(Action::WebShell.required_scope(), None);
    assert!(!Action::WebShell.is_git_operation());
    let empty_token = Principal::authenticated(ME, false, true, Credential::Token(vec![]));
    let disabled = Principal::authenticated(ME, false, false, Credential::Session);
    for p in [Principal::anonymous(), empty_token, disabled] {
        let proof = prove(&p, Action::WebShell, InstanceCtx).expect("public");
        assert_eq!(proof.action(), Action::WebShell);
    }
}

#[test]
fn path_resolve_needs_read_api() {
    assert_eq!(Action::PathResolve.required_scope(), Some(Scope::ReadApi));
    assert!(!Action::PathResolve.is_public());
    assert!(!Action::PathResolve.is_git_operation());
}

fn repo_token() -> Principal {
    Principal::authenticated(
        ME,
        false,
        true,
        Credential::Token(vec![Scope::ReadRepository]),
    )
}

#[test]
fn unresolved_is_not_found_with_its_reason_for_every_active_caller() {
    let admin = Principal::authenticated(ME, true, true, Credential::Session);
    let api_token = Principal::authenticated(ME, false, true, Credential::Token(vec![Scope::Api]));
    for &reason in UnresolvedReason::ALL {
        for p in [session(), admin.clone(), api_token.clone()] {
            let err =
                prove(&p, Action::PathResolve, UnresolvedCtx::new(reason)).expect_err("absent");
            assert_eq!(err.code(), ErrorCode::NotFound);
            assert!(
                err.to_string()
                    .contains(&format!("reason={}", reason.as_str())),
                "{err}"
            );
        }
    }
    let err = prove(
        &Principal::anonymous(),
        Action::PathResolve,
        UnresolvedCtx::new(UnresolvedReason::Absent),
    )
    .expect_err("anonymous");
    assert_eq!(err.code(), ErrorCode::Unauthenticated);
}

#[test]
fn a_wrong_scope_token_gets_the_same_403_for_every_path() {
    let paths = [
        Resource::Unresolved(UnresolvedReason::Absent),
        Resource::Unresolved(UnresolvedReason::TombstoneDeleted),
        Resource::Resolved {
            via: ResolvedVia::Live,
            view: NamespaceView::Project(private(None)),
        },
        Resource::Resolved {
            via: ResolvedVia::Moved,
            view: NamespaceView::Project(private(None)),
        },
        Resource::Resolved {
            via: ResolvedVia::Live,
            view: NamespaceView::Project(ProjectView {
                pending_deletion: true,
                ..private(Some(Role::Owner))
            }),
        },
        Resource::Resolved {
            via: ResolvedVia::Live,
            view: NamespaceView::Project(private(Some(Role::Developer))),
        },
        Resource::Resolved {
            via: ResolvedVia::Live,
            view: NamespaceView::User,
        },
    ];
    let errors: Vec<String> = paths
        .iter()
        .map(|r| {
            let err = authorize(&repo_token(), Action::PathResolve, r).expect_err("wrong scope");
            assert_eq!(err.code(), ErrorCode::InsufficientScope, "{r:?}");
            err.to_string()
        })
        .collect();
    assert!(errors.iter().all(|e| *e == errors[0]), "{errors:?}");
}

#[test]
fn unresolved_is_not_found_for_every_non_public_action() {
    // No existence oracle: an absent path gets the same 404 as an invisible
    // project, whatever action a route decides it with.
    let admin = Principal::authenticated(ME, true, true, Credential::Session);
    for &action in Action::ALL.iter().filter(|a| !a.is_public()) {
        for &reason in UnresolvedReason::ALL {
            for p in [session(), admin.clone()] {
                let err = authorize(&p, action, &Resource::Unresolved(reason))
                    .expect_err("nothing there");
                assert_eq!(err.code(), ErrorCode::NotFound, "{action:?} {reason:?}");
            }
        }
    }
}

#[test]
fn an_invisible_target_behind_a_tombstone_is_logged_as_such() {
    let view = NamespaceView::Project(private(None));
    let err = prove(
        &session(),
        Action::PathResolve,
        resolved(1, ResolvedVia::Moved, view),
    )
    .expect_err("not visible");
    assert_eq!(err.code(), ErrorCode::NotFound);
    assert!(
        err.to_string().contains("reason=tombstone_not_visible"),
        "{err}"
    );
    let err = prove(
        &session(),
        Action::PathResolve,
        resolved(1, ResolvedVia::Live, view),
    )
    .expect_err("not visible");
    assert!(err.to_string().contains("reason=not_visible"), "{err}");
}

#[test]
fn groups_and_personal_namespaces_resolve_for_every_signed_in_user() {
    for view in [
        NamespaceView::User,
        NamespaceView::Group { role: None },
        NamespaceView::Group {
            role: Some(Role::Guest),
        },
    ] {
        let ctx = resolved(3, ResolvedVia::Live, view);
        assert!(
            prove(&session(), Action::PathResolve, ctx).is_ok(),
            "{view:?}"
        );
    }
}

#[test]
fn an_administrator_resolves_an_invisible_project() {
    let admin = Principal::authenticated(ME, true, true, Credential::Session);
    let ctx = resolved(3, ResolvedVia::Moved, NamespaceView::Project(private(None)));
    assert!(prove(&admin, Action::PathResolve, ctx).is_ok());
}

#[test]
fn resolved_fits_only_path_resolve() {
    let admin = Principal::authenticated(ME, true, true, Credential::Session);
    let resource = Resource::Resolved {
        via: ResolvedVia::Live,
        view: NamespaceView::User,
    };
    for &action in Action::ALL
        .iter()
        .filter(|a| !a.is_public() && **a != Action::PathResolve)
    {
        let err = authorize(&admin, action, &resource).expect_err("wrong shape");
        assert_eq!(err.code(), ErrorCode::Internal, "{action:?}");
    }
}

#[test]
fn namespace_view_names_its_kind() {
    assert_eq!(NamespaceView::User.kind(), "user");
    assert_eq!(NamespaceView::Group { role: None }.kind(), "group");
    assert_eq!(NamespaceView::Project(private(None)).kind(), "project");
}

#[test]
fn unresolved_ctx_keeps_its_reason() {
    let ctx = UnresolvedCtx::new(UnresolvedReason::TombstoneDeleted);
    assert_eq!(ctx.reason(), UnresolvedReason::TombstoneDeleted);
    assert_eq!(
        ctx.resource(),
        Resource::Unresolved(UnresolvedReason::TombstoneDeleted)
    );
}

// ── M1 phase 8b MR7a: group, personal namespace, parent and visibility
// contexts; the re-decision inside a write transaction ──

fn group_ctx(id: i64, role: Option<Role>) -> crate::GroupCtx {
    crate::GroupCtx::new(crate::LoadedGroup {
        id: GroupId::new(id),
        role,
    })
}

#[test]
fn group_and_namespace_contexts_decide_on_their_own_row() {
    let proof = prove(
        &session(),
        Action::GroupUpdate,
        group_ctx(4, Some(Role::Owner)),
    )
    .expect("an Owner updates the group");
    assert_eq!(proof.id(), &GroupId::new(4));
    assert_eq!(proof.ctx().role(), Some(Role::Owner));
    let err = prove(
        &session(),
        Action::GroupUpdate,
        group_ctx(4, Some(Role::Maintainer)),
    )
    .expect_err("a Maintainer does not");
    assert_eq!(err.code(), ErrorCode::Forbidden);

    let own = crate::UserNamespaceCtx::new(crate::LoadedUserNamespace {
        namespace: NamespaceId::new(7),
        owner: ME,
    });
    assert_eq!(own.resource(), Resource::Namespace { owner: ME });
    let proof = prove(&session(), Action::ProjectCreate, own).expect("own namespace");
    assert_eq!(proof.id(), &NamespaceId::new(7));
    let foreign = crate::UserNamespaceCtx::new(crate::LoadedUserNamespace {
        namespace: NamespaceId::new(8),
        owner: UserId::new(2),
    });
    let err = prove(&session(), Action::ProjectCreate, foreign).expect_err("foreign");
    assert_eq!(err.code(), ErrorCode::Forbidden);
}

#[test]
fn a_parent_context_is_the_namespace_id_and_the_parents_resource() {
    let in_group = crate::ParentCtx::new(crate::LoadedParent::Group {
        namespace: NamespaceId::new(5),
        group: crate::LoadedGroup {
            id: GroupId::new(3),
            role: Some(Role::Maintainer),
        },
    });
    assert_eq!(in_group.id(), NamespaceId::new(5));
    assert_eq!(
        in_group.resource(),
        Resource::Group {
            role: Some(Role::Maintainer)
        }
    );
    assert!(prove(&session(), Action::ProjectCreate, in_group).is_ok());
    let developer = crate::ParentCtx::new(crate::LoadedParent::Group {
        namespace: NamespaceId::new(5),
        group: crate::LoadedGroup {
            id: GroupId::new(3),
            role: Some(Role::Developer),
        },
    });
    let err = prove(&session(), Action::ProjectCreate, developer).expect_err("developer");
    assert_eq!(err.code(), ErrorCode::Forbidden);
    let personal = crate::ParentCtx::new(crate::LoadedParent::User(crate::LoadedUserNamespace {
        namespace: NamespaceId::new(6),
        owner: ME,
    }));
    assert_eq!(personal.id(), NamespaceId::new(6));
    assert_eq!(personal.resource(), Resource::Namespace { owner: ME });
}

#[test]
fn a_visibility_change_context_carries_the_target_visibility() {
    let ctx = crate::VisibilityChangeCtx::new(
        LoadedProject {
            id: ProjectId::new(9),
            visibility: Visibility::Private,
            role: Some(Role::Owner),
            pending_deletion: false,
        },
        Visibility::Internal,
    );
    assert_eq!(ctx.to(), Visibility::Internal);
    assert_eq!(
        ctx.resource(),
        Resource::VisibilityChange {
            project: private(Some(Role::Owner)),
            to: Visibility::Internal,
        }
    );
    let proof = prove(&session(), Action::ProjectVisibilityChange, ctx).expect("owner");
    assert_eq!(proof.id(), &ProjectId::new(9));
}

#[test]
fn reconfirm_decides_again_on_the_fresh_row_with_the_fresh_account() {
    let active = crate::AccountFacts {
        active: true,
        is_admin: false,
    };
    let proof = prove(
        &session(),
        Action::ProjectUpdate,
        project(2, Some(Role::Maintainer)),
    )
    .expect("maintainer");
    // The role is still there: the same decision, a new proof.
    let again = proof
        .reconfirm(active, project(2, Some(Role::Maintainer)))
        .expect("still a maintainer");
    assert_eq!(again.action(), Action::ProjectUpdate);
    assert_eq!(again.id(), &ProjectId::new(2));
    // The role was removed in between: denied (the private project is now
    // invisible to the caller).
    let err = proof
        .reconfirm(active, project(2, None))
        .expect_err("no role any more");
    assert_eq!(err.code(), ErrorCode::NotFound);
    // Demoted: forbidden.
    let err = proof
        .reconfirm(active, project(2, Some(Role::Developer)))
        .expect_err("demoted");
    assert_eq!(err.code(), ErrorCode::Forbidden);
    // The account was disabled in between.
    let disabled = crate::AccountFacts {
        active: false,
        is_admin: false,
    };
    let err = proof
        .reconfirm(disabled, project(2, Some(Role::Maintainer)))
        .expect_err("disabled");
    assert_eq!(err.code(), ErrorCode::AccountDisabled);
    // Another row than the one decided: a programming error, never a pass.
    let err = proof
        .reconfirm(active, project(3, Some(Role::Owner)))
        .expect_err("another project");
    assert_eq!(err.code(), ErrorCode::Internal);
    assert!(
        err.to_string().contains("reason=reconfirm_target_mismatch"),
        "{err}"
    );
}

#[test]
fn reconfirm_uses_the_fresh_admin_flag_and_keeps_the_credential() {
    let admin = Principal::authenticated(ME, true, true, Credential::Session);
    let proof = prove(&admin, Action::GroupCreateTopLevel, InstanceCtx).expect("admin");
    let demoted = crate::AccountFacts {
        active: true,
        is_admin: false,
    };
    let err = proof
        .reconfirm(demoted, InstanceCtx)
        .expect_err("no longer an administrator");
    assert_eq!(err.code(), ErrorCode::Forbidden);
    // A token keeps its scopes: an admin flag alone grants nothing to an
    // `api` token without the `admin` scope.
    let token = Principal::authenticated(ME, true, true, Credential::Token(vec![Scope::Api]));
    let proof = prove(
        &token,
        Action::SubgroupCreate,
        group_ctx(1, Some(Role::Owner)),
    )
    .expect("owner token");
    let still_admin = crate::AccountFacts {
        active: true,
        is_admin: true,
    };
    let err = proof
        .reconfirm(still_admin, group_ctx(1, None))
        .expect_err("the token carries no admin scope");
    assert_eq!(err.code(), ErrorCode::Forbidden);
}

#[test]
fn reconfirm_keeps_the_agent_flag() {
    let agent = Principal::authenticated(ME, false, true, Credential::Token(vec![Scope::Api]))
        .as_agent(true);
    let proof = prove(&agent, Action::ProjectUpdate, project(2, Some(Role::Owner)))
        .expect("agents update projects");
    let again = proof
        .reconfirm(
            crate::AccountFacts {
                active: true,
                is_admin: false,
            },
            project(2, Some(Role::Owner)),
        )
        .expect("still allowed");
    assert!(again.principal().is_agent());
}

/// D5: a move's proof binds the source and the target parent together,
/// and a hidden target never yields one; the rename and tombstone
/// contexts carry their own row's id.
#[test]
fn d5_contexts_bind_source_and_target() {
    let owned = LoadedProject {
        id: ProjectId::new(9),
        visibility: Visibility::Private,
        role: Some(Role::Owner),
        pending_deletion: false,
    };
    let group = crate::LoadedTransferTarget::Group {
        namespace: NamespaceId::new(5),
        group: crate::LoadedGroup {
            id: GroupId::new(3),
            role: Some(Role::Maintainer),
        },
    };
    let proof = prove(
        &session(),
        Action::ProjectTransfer,
        crate::TransferCtx::new(owned, group),
    )
    .expect("owner to a group they maintain");
    assert_eq!(proof.id(), &(ProjectId::new(9), Some(NamespaceId::new(5))));
    let hidden = crate::LoadedTransferTarget::NotVisible;
    let admin = Principal::authenticated(ME, true, true, Credential::Session);
    let err = prove(
        &admin,
        Action::ProjectTransfer,
        crate::TransferCtx::new(owned, hidden),
    )
    .expect_err("hidden target");
    assert_eq!(err.code(), ErrorCode::NotFound);
    let err = prove(
        &admin,
        Action::GroupTransfer,
        crate::GroupTransferCtx::new(
            crate::LoadedGroup {
                id: GroupId::new(4),
                role: Some(Role::Owner),
            },
            hidden,
        ),
    )
    .expect_err("hidden target");
    assert_eq!(err.code(), ErrorCode::NotFound);
    let top = prove(
        &admin,
        Action::GroupTransfer,
        crate::GroupTransferCtx::new(
            crate::LoadedGroup {
                id: GroupId::new(4),
                role: None,
            },
            crate::LoadedTransferTarget::TopLevel,
        ),
    )
    .expect("admin to the top level");
    assert_eq!(top.id(), &(GroupId::new(4), None));
    let rename = prove(
        &admin,
        Action::UserRename,
        crate::UserRenameCtx::new(UserId::new(2)),
    )
    .expect("admin renames");
    assert_eq!(rename.id(), &UserId::new(2));
    let tombstone = crate::TombstoneCtx::new(crate::LoadedTombstone {
        id: forge_core::TombstoneId::new(6),
        release: crate::TombstoneRelease::Moved {
            target_owner_role: Some(Role::Owner),
            target_visible: true,
        },
    });
    let proof = prove(&session(), Action::NamespaceTombstoneRelease, tombstone)
        .expect("the target's owner");
    assert_eq!(proof.id(), &forge_core::TombstoneId::new(6));
}
