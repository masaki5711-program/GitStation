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

// ── M3 X2: `forge/spec-drift` follows a reference only when every
// signed-in person may read it (the check run is shown to every reader of
// the referencing project). ──

/// The rule is `authorize()`'s own: for every visibility, deletion state
/// and role the loader might have read, the answer equals SpecRefRead for
/// a signed-in person who holds no role in the referenced project.
#[test]
fn every_person_follows_exactly_what_a_person_without_a_role_may_read() {
    use crate::prove_spec_ref_for_every_person;
    let nobody = reader(Credential::Session, AccountKind::Human);
    for visibility in [Visibility::Private, Visibility::Internal] {
        for pending_deletion in [false, true] {
            for role in [
                None,
                Some(Role::Guest),
                Some(Role::Reporter),
                Some(Role::Owner),
            ] {
                let loaded = LoadedProject {
                    id: ProjectId::new(2),
                    visibility,
                    role,
                    pending_deletion,
                };
                let expected = prove(
                    &nobody,
                    Action::SpecRefRead,
                    ProjectCtx::new(LoadedProject {
                        role: None,
                        ..loaded
                    }),
                )
                .err()
                .map(|e| e.code());
                let got = prove_spec_ref_for_every_person(loaded)
                    .err()
                    .map(|e| e.code());
                assert_eq!(
                    got, expected,
                    "{visibility:?} pending={pending_deletion} {role:?}"
                );
            }
        }
    }
}

#[test]
fn every_person_reads_an_internal_project_and_no_private_one() {
    use crate::prove_spec_ref_for_every_person;
    prove_spec_ref_for_every_person(project(2, Visibility::Internal, None))
        .expect("an internal project is read by every signed-in person");
    // A role the loader read (for whoever) lends nothing: the rule is the
    // least person's.
    let private =
        prove_spec_ref_for_every_person(project(3, Visibility::Private, Some(Role::Owner)));
    assert_eq!(private.err().map(|e| e.code()), Some(ErrorCode::NotFound));
    let mut deleting = project(4, Visibility::Internal, None);
    deleting.pending_deletion = true;
    assert!(prove_spec_ref_for_every_person(deleting).is_err());
}

/// M3 X4: the spec API (`SpecRead`) is decided by the same rule, so every
/// `RepoBrowse` row has its `SpecRead` twin too.
#[test]
fn the_spec_api_mirrors_every_browse_row() {
    use crate::matrix::ROWS;
    let twins = |action: Action| -> Vec<String> {
        ROWS.iter()
            .filter(|r| r.action == action)
            .map(|r| format!("{:?} {:?} {:?}", r.actor, r.fixture, r.expect))
            .collect()
    };
    let mut browse = twins(Action::RepoBrowse);
    let mut spec = twins(Action::SpecRead);
    browse.sort();
    spec.sort();
    assert!(!spec.is_empty(), "SpecRead has no rows");
    assert_eq!(spec, browse, "SpecRead rows must be RepoBrowse's twins");
}

/// M3 X4: the spec API's trace follows `.forge/repo.yml` references with
/// the reader's own permissions, decided as `SpecRead` on each referenced
/// project (the route's proof, never the caller's principal).
#[test]
fn a_spec_api_proof_follows_references_for_the_reader() {
    let me = reader(Credential::Session, AccountKind::Human);
    let proof = prove(
        &me,
        Action::SpecRead,
        ProjectCtx::new(project(1, Visibility::Private, Some(Role::Maintainer))),
    )
    .expect("a Maintainer reads its own project's specification");
    let ok = prove_spec_ref(
        &proof,
        project(2, Visibility::Private, Some(Role::Reporter)),
    )
    .expect("followed");
    assert_eq!(ok.action(), Action::SpecRead);
    assert_eq!(
        code(prove_spec_ref(
            &proof,
            project(3, Visibility::Private, None)
        )),
        Some(ErrorCode::NotFound)
    );
}
