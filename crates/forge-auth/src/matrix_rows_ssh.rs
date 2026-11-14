//! The permission matrix rows of the SSH git transport and SSH keys (M4
//! C13, ADR-0032 §2):
//!
//! - A person through an SSH key (`Principal::ssh_key`) is decided exactly
//!   like a token with `read_repository` + `write_repository`: clone /
//!   fetch by the read role, push by the push role and the ref
//!   protections (the same `authorize()` arms as Smart HTTP — over SSH the
//!   routes are the Smart HTTP routes), no administrator right, and no
//!   action of the API (`FG-AUTH-0002`).
//! - `SshKeyList` / `SshKeyDelete`: one's own keys only (another user's is
//!   not found, administrators included), `read_api` / `api`.
//! - `SshKeyAdd`: sessions only — a token never adds a key, an SSH key
//!   does not either.
//!
//! A runner and a job are refused every SSH key action
//! (`matrix_rows_ci::generated_denies`), and none is performed acting for
//! a person (`matrix_rows_act_as::generated_denies`).

use forge_errors::ErrorCode as E;

use crate::action::Action as A;
use crate::matrix::{Actor as W, Expect, Expect::Allow, Fixture as F, Row};
use crate::model::{CopyMark as M, RefKind, RefUpdate as U, Role as R};

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

const BR: RefKind = RefKind::Branch;
const TAG: RefKind = RefKind::Tag;
const RES: RefKind = RefKind::Reserved;

const fn r(kind: RefKind, update: U, protected: Option<R>) -> F {
    F::Ref {
        kind,
        update,
        protected,
    }
}

#[rustfmt::skip]
pub(crate) const SSH_ROWS: &[Row] = &[
    // --- clone / fetch over SSH (RepoRead) ---
    row(A::RepoRead, W::SshKeyDeveloper, F::PrivateProject, Allow),
    row(A::RepoRead, W::SshKeyReporter, F::PrivateProject, Allow),
    row(A::RepoRead, W::SshKeyDeveloper, F::RepoCopy(M::NotMeasured), Allow),
    row(A::RepoRead, W::SshKeyAdmin, F::InternalProject, Allow),
    // An administrator's key is an ordinary member's: no private project
    // they are not a member of (the invisible project's 404).
    row(A::RepoRead, W::SshKeyAdmin, F::PrivateProject, deny(E::NotFound)),
    row(A::RepoRead, W::SshKeyDeveloper, F::DeletedProject, deny(E::PendingDeletion)),
    row(A::RepoRead, W::SshKeyDeveloper, F::Absent, deny(E::NotFound)),
    // --- push over SSH (RepoPush, then each ref) ---
    row(A::RepoPush, W::SshKeyDeveloper, F::PrivateProject, Allow),
    row(A::RepoPush, W::SshKeyReporter, F::PrivateProject, deny(E::Forbidden)),
    row(A::RepoPush, W::SshKeyAdmin, F::InternalProject, deny(E::Forbidden)),
    row(A::RepoPush, W::SshKeyAdmin, F::PrivateProject, deny(E::NotFound)),
    row(A::RepoPush, W::SshKeyDeveloper, r(BR, U::FastForward, None), Allow),
    row(A::RepoPush, W::SshKeyDeveloper, r(BR, U::Create, None), Allow),
    row(A::RepoPush, W::SshKeyDeveloper, r(BR, U::FastForward, Some(R::Developer)), Allow),
    row(A::RepoPush, W::SshKeyDeveloper, r(BR, U::FastForward, Some(R::Maintainer)), deny(E::ProtectedRefPushNotAllowed)),
    row(A::RepoPush, W::SshKeyDeveloper, r(BR, U::Force, Some(R::Developer)), deny(E::ProtectedRefForcePush)),
    row(A::RepoPush, W::SshKeyDeveloper, r(BR, U::Delete, Some(R::Developer)), deny(E::ProtectedRefDeletion)),
    row(A::RepoPush, W::SshKeyDeveloper, r(TAG, U::Create, Some(R::Maintainer)), deny(E::ProtectedRefPushNotAllowed)),
    row(A::RepoPush, W::SshKeyDeveloper, r(RES, U::Create, None), deny(E::ReservedRefNamespace)),
    row(A::RepoPush, W::SshKeyDeveloper, F::RefNoDirectPush { update: U::FastForward }, deny(E::ProtectedRefDirectPushForbidden)),
    // --- nothing of the API through an SSH key ---
    row(A::ProjectRead, W::SshKeyDeveloper, F::PrivateProject, deny(E::InsufficientScope)),
    row(A::TokenList, W::SshKeyDeveloper, F::OwnToken, deny(E::InsufficientScope)),
    row(A::TokenCreate, W::SshKeyDeveloper, F::OwnTokenRequest, deny(E::InsufficientScope)),
    row(A::SshKeyList, W::SshKeyDeveloper, F::OwnToken, deny(E::InsufficientScope)),
    row(A::SshKeyAdd, W::SshKeyDeveloper, F::OwnToken, deny(E::InsufficientScope)),
    row(A::SshKeyDelete, W::SshKeyDeveloper, F::OwnToken, deny(E::InsufficientScope)),
    // --- the SSH key routes ---
    row(A::SshKeyList, W::Reporter, F::OwnToken, Allow),
    row(A::SshKeyList, W::OwnerTokenReadApi, F::OwnToken, Allow),
    row(A::SshKeyList, W::DevTokenReadRepository, F::OwnToken, deny(E::InsufficientScope)),
    row(A::SshKeyList, W::Reporter, F::ForeignToken, deny(E::NotFound)),
    row(A::SshKeyList, W::Admin, F::ForeignToken, deny(E::NotFound)),
    row(A::SshKeyList, W::Anonymous, F::OwnToken, deny(E::Unauthenticated)),
    row(A::SshKeyAdd, W::Reporter, F::OwnToken, Allow),
    row(A::SshKeyAdd, W::Admin, F::OwnToken, Allow),
    row(A::SshKeyAdd, W::DevTokenApi, F::OwnToken, deny(E::InsufficientScope)),
    row(A::SshKeyAdd, W::AdminTokenAdmin, F::OwnToken, deny(E::InsufficientScope)),
    row(A::SshKeyAdd, W::Admin, F::ForeignToken, deny(E::NotFound)),
    row(A::SshKeyAdd, W::Disabled, F::OwnToken, deny(E::AccountDisabled)),
    row(A::SshKeyDelete, W::Reporter, F::OwnToken, Allow),
    row(A::SshKeyDelete, W::DevTokenApi, F::OwnToken, Allow),
    row(A::SshKeyDelete, W::DevTokenReadRepository, F::OwnToken, deny(E::InsufficientScope)),
    row(A::SshKeyDelete, W::Reporter, F::ForeignToken, deny(E::NotFound)),
    row(A::SshKeyDelete, W::AdminTokenApi, F::ForeignToken, deny(E::NotFound)),
];
