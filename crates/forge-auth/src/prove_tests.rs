use forge_core::{NamespaceId, ProjectId};
use forge_errors::ErrorCode;

use crate::matrix::{Actor, Fixture, ME, ROWS};
use crate::{
    authorize, prove, Action, Credential, InstanceCtx, NamespaceView, Principal, ProjectCtx,
    ProjectView, ResolvedCtx, ResolvedVia, Resource, ResourceCtx, Role, Scope, UnresolvedCtx,
    UnresolvedReason, Visibility,
};

/// Any matrix resource as a context (the id is irrelevant to the decision).
#[derive(Debug)]
struct RowCtx(Resource);

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

#[test]
fn prove_returns_the_action_and_id_it_decided() {
    let ctx = ProjectCtx {
        id: ProjectId::new(42),
        view: private(Some(Role::Reporter)),
    };
    let proof = prove(&session(), Action::ProjectRead, ctx).expect("reporter reads");
    assert_eq!(proof.action(), Action::ProjectRead);
    assert_eq!(proof.id(), &ProjectId::new(42));
    assert_eq!(proof.ctx(), &ctx);

    let resolved = ResolvedCtx {
        namespace: NamespaceId::new(7),
        via: ResolvedVia::Live,
        view: NamespaceView::Project(private(Some(Role::Guest))),
    };
    let proof = prove(&session(), Action::PathResolve, resolved).expect("guest resolves");
    assert_eq!(proof.action(), Action::PathResolve);
    assert_eq!(proof.id(), &NamespaceId::new(7));
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
    let resolved = ResolvedCtx {
        namespace: NamespaceId::new(1),
        via: ResolvedVia::Live,
        view: NamespaceView::Project(guest),
    };
    assert!(prove(&session(), Action::PathResolve, resolved).is_ok());
    let err = prove(
        &session(),
        Action::ProjectRead,
        ProjectCtx {
            id: ProjectId::new(1),
            view: guest,
        },
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

#[test]
fn unresolved_is_not_found_with_its_reason_for_every_active_caller() {
    let admin = Principal::authenticated(ME, true, true, Credential::Session);
    let repo_token = Principal::authenticated(
        ME,
        false,
        true,
        Credential::Token(vec![Scope::ReadRepository]),
    );
    for reason in [UnresolvedReason::Absent, UnresolvedReason::TombstoneDeleted] {
        for p in [session(), admin.clone(), repo_token.clone()] {
            let err = prove(&p, Action::PathResolve, UnresolvedCtx(reason)).expect_err("absent");
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
        UnresolvedCtx(UnresolvedReason::Absent),
    )
    .expect_err("anonymous");
    assert_eq!(err.code(), ErrorCode::Unauthenticated);
}

#[test]
fn an_invisible_target_behind_a_tombstone_is_logged_as_such() {
    let moved = ResolvedCtx {
        namespace: NamespaceId::new(1),
        via: ResolvedVia::Moved,
        view: NamespaceView::Project(private(None)),
    };
    let err = prove(&session(), Action::PathResolve, moved).expect_err("not visible");
    assert_eq!(err.code(), ErrorCode::NotFound);
    assert!(
        err.to_string().contains("reason=tombstone_not_visible"),
        "{err}"
    );
    let live = ResolvedCtx {
        via: ResolvedVia::Live,
        ..moved
    };
    let err = prove(&session(), Action::PathResolve, live).expect_err("not visible");
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
        let ctx = ResolvedCtx {
            namespace: NamespaceId::new(3),
            via: ResolvedVia::Live,
            view,
        };
        assert!(
            prove(&session(), Action::PathResolve, ctx).is_ok(),
            "{view:?}"
        );
    }
}

#[test]
fn an_administrator_resolves_an_invisible_project() {
    let admin = Principal::authenticated(ME, true, true, Credential::Session);
    let ctx = ResolvedCtx {
        namespace: NamespaceId::new(3),
        via: ResolvedVia::Moved,
        view: NamespaceView::Project(private(None)),
    };
    assert!(prove(&admin, Action::PathResolve, ctx).is_ok());
}

#[test]
fn resolution_resources_fit_only_path_resolve() {
    let admin = Principal::authenticated(ME, true, true, Credential::Session);
    let resources = [
        Resource::Unresolved(UnresolvedReason::Absent),
        Resource::Resolved {
            via: ResolvedVia::Live,
            view: NamespaceView::User,
        },
    ];
    for &action in Action::ALL
        .iter()
        .filter(|a| !a.is_public() && **a != Action::PathResolve)
    {
        for resource in &resources {
            let err = authorize(&admin, action, resource).expect_err("wrong shape");
            assert_eq!(err.code(), ErrorCode::Internal, "{action:?} {resource:?}");
        }
    }
}

#[test]
fn namespace_view_names_its_kind() {
    assert_eq!(NamespaceView::User.kind(), "user");
    assert_eq!(NamespaceView::Group { role: None }.kind(), "group");
    assert_eq!(NamespaceView::Project(private(None)).kind(), "project");
}
