//! `prove_spec_ref` (M3 X1, ADR-0023 Amendment X1): every referenced
//! project is decided with the reader's own permissions — the proof's
//! principal — never with the access the reader has to the referencing
//! project.

use forge_core::{ProjectId, UserId};
use forge_errors::ErrorCode;

use crate::{
    prove, prove_spec_ref, AccountKind, Action, Credential, LoadedProject, Principal, ProjectCtx,
    Role, Scope, Visibility,
};

const READER: UserId = UserId::new(5);

fn reader(credential: Credential, kind: AccountKind) -> Principal {
    Principal::authenticated(READER, false, true, credential, kind)
}

fn project(id: i64, visibility: Visibility, role: Option<Role>) -> LoadedProject {
    LoadedProject {
        id: ProjectId::new(id),
        visibility,
        role,
        pending_deletion: false,
    }
}

/// The reader is a Maintainer of the referencing project 1.
fn route_proof(p: &Principal) -> crate::Authorized<ProjectCtx> {
    prove(
        p,
        Action::SpecRefRead,
        ProjectCtx::new(project(1, Visibility::Private, Some(Role::Maintainer))),
    )
    .expect("a Maintainer reads its own project's spec references")
}

fn code(r: Result<crate::Authorized<ProjectCtx>, forge_errors::ForgeError>) -> Option<ErrorCode> {
    r.err().map(|e| e.code())
}

#[test]
fn a_referenced_project_is_decided_with_the_readers_own_role() {
    let me = reader(Credential::Session, AccountKind::Human);
    let proof = route_proof(&me);
    // A private spec repository the reader is a Reporter of: followed.
    let ok = prove_spec_ref(
        &proof,
        project(2, Visibility::Private, Some(Role::Reporter)),
    )
    .expect("a Reporter reads the referenced project");
    assert_eq!(ok.id(), &ProjectId::new(2));
    assert_eq!(ok.action(), Action::SpecRefRead);
    assert_eq!(ok.principal().user(), Some(READER));
    // No role there: the Maintainer role on project 1 lends nothing.
    assert_eq!(
        code(prove_spec_ref(
            &proof,
            project(3, Visibility::Private, None)
        )),
        Some(ErrorCode::NotFound)
    );
    // A Guest of a private project reads no content.
    assert_eq!(
        code(prove_spec_ref(
            &proof,
            project(4, Visibility::Private, Some(Role::Guest))
        )),
        Some(ErrorCode::Forbidden)
    );
    // An internal project: every signed-in person reads it.
    assert!(prove_spec_ref(&proof, project(5, Visibility::Internal, None)).is_ok());
    // Pending deletion: not followed.
    let mut deleted = project(6, Visibility::Private, Some(Role::Owner));
    deleted.pending_deletion = true;
    assert_eq!(
        code(prove_spec_ref(&proof, deleted)),
        Some(ErrorCode::PendingDeletion)
    );
}

#[test]
fn a_service_account_follows_only_projects_it_is_a_member_of() {
    let sa = reader(
        Credential::Token(vec![Scope::ReadApi]),
        AccountKind::ServiceAccount,
    );
    let proof = route_proof(&sa);
    assert_eq!(
        code(prove_spec_ref(
            &proof,
            project(5, Visibility::Internal, None)
        )),
        Some(ErrorCode::NotFound)
    );
    assert!(prove_spec_ref(
        &proof,
        project(5, Visibility::Internal, Some(Role::Reporter))
    )
    .is_ok());
}

#[test]
fn a_proof_for_another_action_follows_nothing() {
    let me = reader(Credential::Session, AccountKind::Human);
    let other = prove(
        &me,
        Action::RepoBrowse,
        ProjectCtx::new(project(1, Visibility::Private, Some(Role::Maintainer))),
    )
    .expect("browse");
    let refused = prove_spec_ref(&other, project(2, Visibility::Internal, None));
    let Err(err) = refused else {
        panic!("a proof for RepoBrowse must not follow a reference");
    };
    assert_eq!(err.code(), ErrorCode::Internal);
    assert!(err
        .context()
        .iter()
        .any(|(k, v)| *k == "reason" && v == "proof_action_mismatch"));
}

/// Both decisions are one rule: every `RepoBrowse` row has its
/// `SpecRefRead` twin with the same actor, fixture and outcome.
#[test]
fn the_spec_refs_mirror_every_browse_row() {
    use crate::matrix::ROWS;
    let missing: Vec<String> = ROWS
        .iter()
        .filter(|r| r.action == Action::RepoBrowse)
        .filter(|r| {
            !ROWS.iter().any(|c| {
                c.action == Action::SpecRefRead
                    && c.actor == r.actor
                    && c.fixture == r.fixture
                    && c.expect == r.expect
            })
        })
        .map(|r| format!("{:?} {:?} {:?}", r.actor, r.fixture, r.expect))
        .collect();
    assert!(
        missing.is_empty(),
        "rows without a spec-refs twin:\n{}",
        missing.join("\n")
    );
}
