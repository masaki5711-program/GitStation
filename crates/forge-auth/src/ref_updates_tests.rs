#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)] // tests may fail loudly (rust/testing.md)

use forge_core::{ProjectId, ProtectionRule, ProtectionRules, RefName, RefPattern, UserId};
use forge_errors::ErrorCode;

use crate::{
    authorize, authorize_ref_updates, prove, Action, Credential, LoadedProject, Principal,
    ProjectCtx, RefProtection, RefUpdate, RefUpdateCtx, Resource, Role, Visibility,
};

fn user(admin: bool) -> Principal {
    Principal::authenticated(UserId::new(7), admin, true, Credential::Session)
}

fn project(role: Option<Role>) -> ProjectCtx {
    ProjectCtx::new(LoadedProject {
        id: ProjectId::new(3),
        visibility: Visibility::Private,
        role,
        pending_deletion: false,
    })
}

fn name(raw: &str) -> RefName {
    RefName::parse(raw).expect("ref")
}

/// `main` needs Maintainer; `release/*` needs Developer and Owner (the
/// strictest of the two applies).
fn rules() -> ProtectionRules {
    let rule = |pattern: &str, role| ProtectionRule {
        pattern: RefPattern::parse(pattern).expect("pattern"),
        push_min_role: role,
    };
    ProtectionRules::new(vec![
        rule("refs/heads/main", Role::Maintainer),
        rule("refs/heads/release/*", Role::Developer),
        rule("refs/heads/release/*", Role::Owner),
        rule("refs/tags/v*", Role::Maintainer),
    ])
}

#[test]
fn every_ref_is_decided_by_authorize_on_the_project_proof() {
    let me = user(false);
    let proof = prove(&me, Action::RepoPush, project(Some(Role::Developer))).expect("developer");
    let updates = [
        RefUpdateCtx::new(name("refs/heads/feature"), RefUpdate::Force, &rules()),
        RefUpdateCtx::new(name("refs/heads/main"), RefUpdate::FastForward, &rules()),
        RefUpdateCtx::new(name("refs/heads/MAIN"), RefUpdate::Delete, &rules()),
        RefUpdateCtx::new(name("refs/replace/abc"), RefUpdate::Create, &rules()),
        RefUpdateCtx::new(name("refs/heads/release/1"), RefUpdate::Create, &rules()),
    ];
    assert_eq!(
        updates[4].protection(),
        RefProtection::effective([Role::Owner]),
        "the strictest matching rule"
    );
    let got = authorize_ref_updates(&me, &proof, &updates).expect("decided");
    let codes: Vec<Option<ErrorCode>> = got
        .iter()
        .map(|r| r.as_ref().err().map(|e| e.code()))
        .collect();
    assert_eq!(
        codes,
        [
            None,
            Some(ErrorCode::ProtectedRefPushNotAllowed),
            Some(ErrorCode::ProtectedRefDeletion),
            Some(ErrorCode::ReservedRefNamespace),
            Some(ErrorCode::ProtectedRefPushNotAllowed),
        ]
    );
    // Exactly what authorize() answers for each ref (one decision point).
    for (u, r) in updates.iter().zip(&got) {
        let direct = authorize(
            &me,
            Action::RepoPush,
            &Resource::Ref {
                project: proof.ctx().view(),
                name: u.name().clone(),
                update: u.update(),
                protection: u.protection(),
            },
        );
        assert_eq!(
            direct.as_ref().err().map(|e| e.to_string()),
            r.as_ref().err().map(|e| e.to_string())
        );
    }
}

#[test]
fn force_on_a_protected_ref_is_denied_even_for_an_administrator() {
    let admin = user(true);
    let proof = prove(&admin, Action::RepoPush, project(None)).expect("admin");
    let updates = [
        RefUpdateCtx::new(name("refs/heads/main"), RefUpdate::Force, &rules()),
        RefUpdateCtx::new(name("refs/tags/v1"), RefUpdate::Force, &rules()),
        RefUpdateCtx::new(name("refs/heads/main"), RefUpdate::FastForward, &rules()),
    ];
    let got = authorize_ref_updates(&admin, &proof, &updates).expect("decided");
    assert_eq!(
        got[0].as_ref().expect_err("force").code(),
        ErrorCode::ProtectedRefForcePush
    );
    assert_eq!(
        got[1].as_ref().expect_err("tag move").code(),
        ErrorCode::ProtectedRefForcePush
    );
    assert!(got[2].is_ok(), "administrators pass the minimum role");
}

#[test]
fn a_proof_for_another_action_is_refused() {
    let me = user(false);
    let proof = prove(&me, Action::RepoRead, project(Some(Role::Developer))).expect("reader");
    let err = authorize_ref_updates(&me, &proof, &[]).expect_err("wrong proof");
    assert_eq!(err.code(), ErrorCode::Internal);
    assert!(err.to_string().contains("proof_action_mismatch"), "{err}");
}
