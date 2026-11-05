//! The permission matrix rows of the spec references (M3 X1,
//! `SpecRefRead`: `GET /api/v1/projects/{id}/repository/spec-refs`, plan 03
//! §4.4, ADR-0023 Amendment X1).
//!
//! One action, decided twice with the same rule — `RepoBrowse`'s (Reporter
//! by role, every signed-in person on an internal project, a service account
//! by membership only, `read_api`):
//!
//! - on the referencing project, by the route (its `.forge/repo.yml` is
//!   repository content);
//! - on every referenced project, by `prove_spec_ref` with the **reader's**
//!   principal (the proof's): a reference never lends the referencing
//!   project's access to the referenced one.
//!
//! So every `RepoBrowse` row has its twin here with the same actor, fixture
//! and outcome (`the_spec_refs_mirror_every_browse_row`).

use forge_errors::ErrorCode as E;

use crate::action::Action as A;
use crate::matrix::{Actor as W, Expect, Fixture as F, Row};

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

use Expect::Allow;

#[rustfmt::skip]
pub(crate) const SPEC_REF_ROWS: &[Row] = &[
    row(A::SpecRefRead, W::Reporter, F::PrivateProject, Allow),
    row(A::SpecRefRead, W::Developer, F::PrivateProject, Allow),
    row(A::SpecRefRead, W::Maintainer, F::PrivateProject, Allow),
    row(A::SpecRefRead, W::Owner, F::PrivateProject, Allow),
    row(A::SpecRefRead, W::Admin, F::PrivateProject, Allow),
    row(A::SpecRefRead, W::NonMember, F::InternalProject, Allow),
    row(A::SpecRefRead, W::Guest, F::InternalProject, Allow),
    row(A::SpecRefRead, W::DevTokenApi, F::PrivateProject, Allow),
    row(A::SpecRefRead, W::OwnerTokenReadApi, F::PrivateProject, Allow),
    row(A::SpecRefRead, W::AgentTokenReadApi, F::PrivateProject, Allow),
    row(A::SpecRefRead, W::AdminTokenAdmin, F::PrivateProject, Allow),
    row(A::SpecRefRead, W::Anonymous, F::PrivateProject, deny(E::Unauthenticated)),
    row(A::SpecRefRead, W::Anonymous, F::InternalProject, deny(E::Unauthenticated)),
    // a Guest of a private project resolves it but reads no content
    row(A::SpecRefRead, W::Guest, F::PrivateProject, deny(E::Forbidden)),
    // a private project the reader is no member of: the same 404 as absent
    row(A::SpecRefRead, W::NonMember, F::PrivateProject, deny(E::NotFound)),
    row(A::SpecRefRead, W::NonMemberTokenReadApi, F::PrivateProject, deny(E::NotFound)),
    row(A::SpecRefRead, W::NonMemberTokenReadApi, F::InternalProject, Allow),
    row(A::SpecRefRead, W::GuestTokenReadApi, F::PrivateProject, deny(E::Forbidden)),
    row(A::SpecRefRead, W::GuestTokenReadApi, F::InternalProject, Allow),
    row(A::SpecRefRead, W::Disabled, F::PrivateProject, deny(E::AccountDisabled)),
    row(A::SpecRefRead, W::Disabled, F::InternalProject, deny(E::AccountDisabled)),
    row(A::SpecRefRead, W::DisabledAdmin, F::PrivateProject, deny(E::AccountDisabled)),
    // a git-only token reads no JSON API
    row(A::SpecRefRead, W::DevTokenReadRepository, F::PrivateProject, deny(E::InsufficientScope)),
    row(A::SpecRefRead, W::DevTokenWriteRepository, F::PrivateProject, deny(E::InsufficientScope)),
    row(A::SpecRefRead, W::DevTokenReadRepository, F::InternalProject, deny(E::InsufficientScope)),
    // an administrator's token without the admin scope has no admin rights
    row(A::SpecRefRead, W::AdminTokenApi, F::PrivateProject, deny(E::NotFound)),
    row(A::SpecRefRead, W::Developer, F::DeletedProject, deny(E::PendingDeletion)),
    row(A::SpecRefRead, W::Admin, F::DeletedProject, deny(E::PendingDeletion)),
    row(A::SpecRefRead, W::NonMember, F::DeletedProject, deny(E::NotFound)),
    row(A::SpecRefRead, W::NonMember, F::InternalDeletedProject, deny(E::PendingDeletion)),
    row(A::SpecRefRead, W::Developer, F::Absent, deny(E::NotFound)),
    row(A::SpecRefRead, W::Admin, F::Absent, deny(E::NotFound)),
    // a service account reads member projects only (M3 S0), a bot like a person
    row(A::SpecRefRead, W::ServiceAccountNonMemberTokenReadApi, F::InternalProject, deny(E::NotFound)),
    row(A::SpecRefRead, W::ServiceAccountGuestTokenRead, F::PrivateProject, deny(E::Forbidden)),
    row(A::SpecRefRead, W::BotNonMemberTokenReadApi, F::InternalProject, Allow),
];

/// M3 X4 (`SpecRead`: `GET /api/v1/projects/{id}/specs/…`): the same
/// rule again, so every `RepoBrowse` row has its twin here too
/// (`the_spec_api_mirrors_every_browse_row`).
#[rustfmt::skip]
pub(crate) const SPEC_READ_ROWS: &[Row] = &[
    row(A::SpecRead, W::Reporter, F::PrivateProject, Allow),
    row(A::SpecRead, W::Developer, F::PrivateProject, Allow),
    row(A::SpecRead, W::Maintainer, F::PrivateProject, Allow),
    row(A::SpecRead, W::Owner, F::PrivateProject, Allow),
    row(A::SpecRead, W::Admin, F::PrivateProject, Allow),
    row(A::SpecRead, W::NonMember, F::InternalProject, Allow),
    row(A::SpecRead, W::Guest, F::InternalProject, Allow),
    row(A::SpecRead, W::DevTokenApi, F::PrivateProject, Allow),
    row(A::SpecRead, W::OwnerTokenReadApi, F::PrivateProject, Allow),
    row(A::SpecRead, W::AgentTokenReadApi, F::PrivateProject, Allow),
    row(A::SpecRead, W::AdminTokenAdmin, F::PrivateProject, Allow),
    row(A::SpecRead, W::Anonymous, F::PrivateProject, deny(E::Unauthenticated)),
    row(A::SpecRead, W::Anonymous, F::InternalProject, deny(E::Unauthenticated)),
    // a Guest of a private project resolves it but reads no content
    row(A::SpecRead, W::Guest, F::PrivateProject, deny(E::Forbidden)),
    // a private project the reader is no member of: the same 404 as absent
    row(A::SpecRead, W::NonMember, F::PrivateProject, deny(E::NotFound)),
    row(A::SpecRead, W::NonMemberTokenReadApi, F::PrivateProject, deny(E::NotFound)),
    row(A::SpecRead, W::NonMemberTokenReadApi, F::InternalProject, Allow),
    row(A::SpecRead, W::GuestTokenReadApi, F::PrivateProject, deny(E::Forbidden)),
    row(A::SpecRead, W::GuestTokenReadApi, F::InternalProject, Allow),
    row(A::SpecRead, W::Disabled, F::PrivateProject, deny(E::AccountDisabled)),
    row(A::SpecRead, W::Disabled, F::InternalProject, deny(E::AccountDisabled)),
    row(A::SpecRead, W::DisabledAdmin, F::PrivateProject, deny(E::AccountDisabled)),
    // a git-only token reads no JSON API
    row(A::SpecRead, W::DevTokenReadRepository, F::PrivateProject, deny(E::InsufficientScope)),
    row(A::SpecRead, W::DevTokenWriteRepository, F::PrivateProject, deny(E::InsufficientScope)),
    row(A::SpecRead, W::DevTokenReadRepository, F::InternalProject, deny(E::InsufficientScope)),
    // an administrator's token without the admin scope has no admin rights
    row(A::SpecRead, W::AdminTokenApi, F::PrivateProject, deny(E::NotFound)),
    row(A::SpecRead, W::Developer, F::DeletedProject, deny(E::PendingDeletion)),
    row(A::SpecRead, W::Admin, F::DeletedProject, deny(E::PendingDeletion)),
    row(A::SpecRead, W::NonMember, F::DeletedProject, deny(E::NotFound)),
    row(A::SpecRead, W::NonMember, F::InternalDeletedProject, deny(E::PendingDeletion)),
    row(A::SpecRead, W::Developer, F::Absent, deny(E::NotFound)),
    row(A::SpecRead, W::Admin, F::Absent, deny(E::NotFound)),
    // a service account reads member projects only (M3 S0), a bot like a person
    row(A::SpecRead, W::ServiceAccountNonMemberTokenReadApi, F::InternalProject, deny(E::NotFound)),
    row(A::SpecRead, W::ServiceAccountGuestTokenRead, F::PrivateProject, deny(E::Forbidden)),
    row(A::SpecRead, W::BotNonMemberTokenReadApi, F::InternalProject, Allow),
];
