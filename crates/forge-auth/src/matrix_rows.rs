//! The rows of the permission matrix (see `matrix.rs`). Grouped by action;
//! denied rows are first-class. Each row: action, caller, resource, expected.

use forge_errors::ErrorCode as E;

use crate::action::Action as A;
use crate::matrix::{Actor as W, Expect, Expect::Allow, Fixture as F, Row};
use crate::model::{MembershipChange as M, RefKind, RefUpdate as U, Role as R};

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

const fn pc(from: Option<R>, to: Option<R>) -> F {
    F::ProtectionChange { from, to }
}

#[rustfmt::skip]
pub(crate) const ROWS: &[Row] = &[
    // --- RepoRead (git clone / fetch) ---
    row(A::RepoRead, W::Anonymous, F::PrivateProject, deny(E::Unauthenticated)),
    row(A::RepoRead, W::Disabled, F::PrivateProject, deny(E::AccountDisabled)),
    row(A::RepoRead, W::NonMember, F::PrivateProject, deny(E::NotFound)),
    row(A::RepoRead, W::Guest, F::PrivateProject, deny(E::Forbidden)),
    row(A::RepoRead, W::Reporter, F::PrivateProject, Allow),
    row(A::RepoRead, W::Developer, F::PrivateProject, Allow),
    row(A::RepoRead, W::Admin, F::PrivateProject, Allow),
    row(A::RepoRead, W::DevTokenReadRepository, F::PrivateProject, Allow),
    row(A::RepoRead, W::DevTokenWriteRepository, F::PrivateProject, Allow),
    row(A::RepoRead, W::DevTokenApi, F::PrivateProject, deny(E::InsufficientScope)),
    row(A::RepoRead, W::AdminTokenApi, F::PrivateProject, deny(E::NotFound)),
    row(A::RepoRead, W::NonMember, F::InternalProject, Allow),
    row(A::RepoRead, W::Guest, F::InternalProject, Allow),
    row(A::RepoRead, W::Developer, F::DeletedProject, deny(E::PendingDeletion)),
    row(A::RepoRead, W::NonMember, F::DeletedProject, deny(E::NotFound)),
    // --- RepoPush, project level (before receive-pack starts) ---
    row(A::RepoPush, W::Developer, F::PrivateProject, Allow),
    row(A::RepoPush, W::DevTokenWriteRepository, F::PrivateProject, Allow),
    row(A::RepoPush, W::Reporter, F::PrivateProject, deny(E::Forbidden)),
    row(A::RepoPush, W::NonMember, F::InternalProject, deny(E::Forbidden)),
    row(A::RepoPush, W::NonMember, F::PrivateProject, deny(E::NotFound)),
    row(A::RepoPush, W::DevTokenReadRepository, F::PrivateProject, deny(E::InsufficientScope)),
    // --- RepoPush (one ref update) ---
    row(A::RepoPush, W::Anonymous, r(BR, U::FastForward, None), deny(E::Unauthenticated)),
    row(A::RepoPush, W::NonMember, r(BR, U::FastForward, None), deny(E::NotFound)),
    row(A::RepoPush, W::NonMember, F::InternalRef, deny(E::Forbidden)),
    row(A::RepoPush, W::Reporter, r(BR, U::FastForward, None), deny(E::Forbidden)),
    row(A::RepoPush, W::Developer, r(BR, U::FastForward, None), Allow),
    row(A::RepoPush, W::Developer, r(BR, U::Create, None), Allow),
    row(A::RepoPush, W::Developer, r(BR, U::Force, None), Allow),
    row(A::RepoPush, W::Developer, r(BR, U::Delete, None), Allow),
    row(A::RepoPush, W::Developer, r(TAG, U::Create, None), Allow),
    row(A::RepoPush, W::Developer, F::DeletedRef, deny(E::PendingDeletion)),
    row(A::RepoPush, W::DevTokenReadRepository, r(BR, U::FastForward, None), deny(E::InsufficientScope)),
    row(A::RepoPush, W::DevTokenApi, r(BR, U::FastForward, None), deny(E::InsufficientScope)),
    row(A::RepoPush, W::DevTokenWriteRepository, r(BR, U::FastForward, None), Allow),
    // reserved namespaces (refs/merge-requests, refs/replace, …): nobody
    row(A::RepoPush, W::Owner, r(RES, U::Create, None), deny(E::ReservedRefNamespace)),
    row(A::RepoPush, W::Admin, r(RES, U::Create, None), deny(E::ReservedRefNamespace)),
    row(A::RepoPush, W::Reporter, r(RES, U::Create, None), deny(E::Forbidden)),
    // protected refs
    row(A::RepoPush, W::Developer, r(BR, U::FastForward, Some(R::Maintainer)), deny(E::ProtectedRefPushNotAllowed)),
    row(A::RepoPush, W::Developer, r(BR, U::Create, Some(R::Maintainer)), deny(E::ProtectedRefPushNotAllowed)),
    row(A::RepoPush, W::Developer, r(BR, U::FastForward, Some(R::Developer)), Allow),
    row(A::RepoPush, W::Maintainer, r(BR, U::FastForward, Some(R::Maintainer)), Allow),
    row(A::RepoPush, W::Maintainer, r(TAG, U::Create, Some(R::Maintainer)), Allow),
    row(A::RepoPush, W::Admin, r(BR, U::FastForward, Some(R::Owner)), Allow),
    row(A::RepoPush, W::Reporter, r(BR, U::Force, Some(R::Developer)), deny(E::Forbidden)),
    row(A::RepoPush, W::Developer, r(BR, U::Force, Some(R::Developer)), deny(E::ProtectedRefForcePush)),
    row(A::RepoPush, W::Owner, r(BR, U::Force, Some(R::Maintainer)), deny(E::ProtectedRefForcePush)),
    row(A::RepoPush, W::Admin, r(BR, U::Force, Some(R::Maintainer)), deny(E::ProtectedRefForcePush)),
    row(A::RepoPush, W::Developer, r(BR, U::Delete, Some(R::Developer)), deny(E::ProtectedRefDeletion)),
    row(A::RepoPush, W::Owner, r(TAG, U::Delete, Some(R::Maintainer)), deny(E::ProtectedRefDeletion)),
    row(A::RepoPush, W::Admin, r(BR, U::Delete, Some(R::Maintainer)), deny(E::ProtectedRefDeletion)),
    // --- ProjectRead (API) ---
    row(A::ProjectRead, W::Reporter, F::PrivateProject, Allow),
    row(A::ProjectRead, W::Guest, F::PrivateProject, deny(E::Forbidden)),
    row(A::ProjectRead, W::NonMember, F::PrivateProject, deny(E::NotFound)),
    row(A::ProjectRead, W::Disabled, F::PrivateProject, deny(E::AccountDisabled)),
    row(A::ProjectRead, W::NonMember, F::InternalProject, Allow),
    row(A::ProjectRead, W::DevTokenReadRepository, F::PrivateProject, deny(E::InsufficientScope)),
    row(A::ProjectRead, W::DevTokenApi, F::PrivateProject, Allow),
    row(A::ProjectRead, W::OwnerTokenReadApi, F::PrivateProject, Allow),
    // --- ProjectCreate ---
    row(A::ProjectCreate, W::Maintainer, F::Group, Allow),
    row(A::ProjectCreate, W::Developer, F::Group, deny(E::Forbidden)),
    row(A::ProjectCreate, W::NonMember, F::Group, deny(E::Forbidden)),
    row(A::ProjectCreate, W::Admin, F::Group, Allow),
    row(A::ProjectCreate, W::NonMember, F::OwnNamespace, Allow),
    row(A::ProjectCreate, W::Developer, F::ForeignNamespace, deny(E::Forbidden)),
    row(A::ProjectCreate, W::Admin, F::ForeignNamespace, Allow),
    // --- ProjectUpdate ---
    row(A::ProjectUpdate, W::Maintainer, F::PrivateProject, Allow),
    row(A::ProjectUpdate, W::Developer, F::PrivateProject, deny(E::Forbidden)),
    row(A::ProjectUpdate, W::NonMember, F::PrivateProject, deny(E::NotFound)),
    row(A::ProjectUpdate, W::NonMember, F::InternalProject, deny(E::Forbidden)),
    row(A::ProjectUpdate, W::OwnerTokenReadApi, F::PrivateProject, deny(E::InsufficientScope)),
    row(A::ProjectUpdate, W::AdminTokenApi, F::InternalProject, deny(E::Forbidden)),
    row(A::ProjectUpdate, W::AdminTokenAdmin, F::PrivateProject, Allow),
    // --- ProjectVisibilityChange (Owner only) ---
    row(A::ProjectVisibilityChange, W::Owner, F::MakeInternal, Allow),
    row(A::ProjectVisibilityChange, W::Maintainer, F::MakeInternal, deny(E::Forbidden)),
    row(A::ProjectVisibilityChange, W::NonMember, F::MakeInternal, deny(E::NotFound)),
    row(A::ProjectVisibilityChange, W::Admin, F::MakeInternal, Allow),
    // --- ProtectedRefManage (protection levels at or below the caller's role) ---
    row(A::ProtectedRefManage, W::Maintainer, pc(None, Some(R::Maintainer)), Allow),
    row(A::ProtectedRefManage, W::Maintainer, pc(Some(R::Maintainer), None), Allow),
    row(A::ProtectedRefManage, W::Maintainer, pc(Some(R::Owner), None), deny(E::Forbidden)),
    row(A::ProtectedRefManage, W::Maintainer, pc(Some(R::Owner), Some(R::Developer)), deny(E::Forbidden)),
    row(A::ProtectedRefManage, W::Maintainer, pc(None, Some(R::Owner)), deny(E::Forbidden)),
    row(A::ProtectedRefManage, W::Owner, pc(Some(R::Owner), None), Allow),
    row(A::ProtectedRefManage, W::Developer, pc(None, Some(R::Developer)), deny(E::Forbidden)),
    row(A::ProtectedRefManage, W::NonMember, pc(None, Some(R::Developer)), deny(E::NotFound)),
    row(A::ProtectedRefManage, W::Admin, pc(Some(R::Owner), None), Allow),
    // --- ProjectTransfer ---
    row(A::ProjectTransfer, W::Owner, F::Transfer { target: Some(R::Maintainer) }, Allow),
    row(A::ProjectTransfer, W::Owner, F::Transfer { target: Some(R::Developer) }, deny(E::Forbidden)),
    row(A::ProjectTransfer, W::Owner, F::Transfer { target: None }, deny(E::Forbidden)),
    row(A::ProjectTransfer, W::Maintainer, F::Transfer { target: Some(R::Owner) }, deny(E::Forbidden)),
    row(A::ProjectTransfer, W::NonMember, F::Transfer { target: Some(R::Owner) }, deny(E::NotFound)),
    row(A::ProjectTransfer, W::Admin, F::Transfer { target: None }, Allow),
    // --- ProjectDelete / ProjectRestore ---
    row(A::ProjectDelete, W::Owner, F::PrivateProject, Allow),
    row(A::ProjectDelete, W::Maintainer, F::PrivateProject, deny(E::Forbidden)),
    row(A::ProjectDelete, W::NonMember, F::PrivateProject, deny(E::NotFound)),
    row(A::ProjectDelete, W::AdminTokenApi, F::PrivateProject, deny(E::NotFound)),
    row(A::ProjectDelete, W::Owner, F::DeletedProject, deny(E::PendingDeletion)),
    row(A::ProjectDelete, W::Admin, F::DeletedProject, deny(E::PendingDeletion)),
    row(A::ProjectRestore, W::Owner, F::DeletedProject, Allow),
    row(A::ProjectRestore, W::Admin, F::DeletedProject, Allow),
    row(A::ProjectRestore, W::Maintainer, F::DeletedProject, deny(E::Forbidden)),
    row(A::ProjectRestore, W::NonMember, F::DeletedProject, deny(E::NotFound)),
    row(A::ProjectRestore, W::OwnerTokenReadApi, F::DeletedProject, deny(E::InsufficientScope)),
    // --- MemberManage ---
    row(A::MemberManage, W::Maintainer, F::ProjectMember(M::Add(R::Developer)), Allow),
    row(A::MemberManage, W::Maintainer, F::ProjectMember(M::Add(R::Maintainer)), Allow),
    row(A::MemberManage, W::Maintainer, F::ProjectMember(M::Add(R::Owner)), deny(E::Forbidden)),
    row(A::MemberManage, W::Maintainer, F::ProjectMember(M::Change { from: R::Owner, to: R::Guest }), deny(E::Forbidden)),
    row(A::MemberManage, W::Maintainer, F::ProjectMember(M::Remove { from: R::Owner }), deny(E::Forbidden)),
    row(A::MemberManage, W::Maintainer, F::ProjectMember(M::Change { from: R::Maintainer, to: R::Developer }), Allow),
    row(A::MemberManage, W::Owner, F::ProjectMember(M::Add(R::Owner)), Allow),
    row(A::MemberManage, W::Owner, F::ProjectMember(M::Remove { from: R::Owner }), Allow),
    row(A::MemberManage, W::Owner, F::LastOwnerRemoval, deny(E::Forbidden)),
    row(A::MemberManage, W::Admin, F::LastOwnerRemoval, deny(E::Forbidden)),
    row(A::MemberManage, W::Owner, F::GroupLastOwnerRemoval, deny(E::Forbidden)),
    row(A::MemberManage, W::Admin, F::GroupLastOwnerRemoval, deny(E::Forbidden)),
    row(A::MemberManage, W::Developer, F::ProjectMember(M::Add(R::Guest)), deny(E::Forbidden)),
    row(A::MemberManage, W::NonMember, F::ProjectMember(M::Add(R::Guest)), deny(E::NotFound)),
    row(A::MemberManage, W::NonMember, F::InternalProjectMember(M::Add(R::Guest)), deny(E::Forbidden)),
    row(A::MemberManage, W::Owner, F::DeletedProjectMember(M::Add(R::Guest)), deny(E::PendingDeletion)),
    row(A::MemberManage, W::OwnerTokenReadApi, F::ProjectMember(M::Add(R::Guest)), deny(E::InsufficientScope)),
    row(A::MemberManage, W::Maintainer, F::GroupMember(M::Add(R::Developer)), Allow),
    row(A::MemberManage, W::Maintainer, F::GroupMember(M::Remove { from: R::Owner }), deny(E::Forbidden)),
    row(A::MemberManage, W::Developer, F::GroupMember(M::Add(R::Guest)), deny(E::Forbidden)),
    row(A::MemberManage, W::NonMember, F::GroupMember(M::Add(R::Guest)), deny(E::Forbidden)),
    row(A::MemberManage, W::AdminTokenApi, F::GroupMember(M::Add(R::Guest)), deny(E::Forbidden)),
    row(A::MemberManage, W::Admin, F::GroupMember(M::Add(R::Owner)), Allow),
    // --- Groups ---
    row(A::GroupRead, W::NonMember, F::Group, Allow),
    row(A::GroupRead, W::Anonymous, F::Group, deny(E::Unauthenticated)),
    row(A::GroupCreateTopLevel, W::Admin, F::Instance, Allow),
    row(A::GroupCreateTopLevel, W::Owner, F::Instance, deny(E::Forbidden)),
    row(A::GroupCreateTopLevel, W::AdminTokenApi, F::Instance, deny(E::Forbidden)),
    row(A::SubgroupCreate, W::Owner, F::Group, Allow),
    row(A::SubgroupCreate, W::Maintainer, F::Group, deny(E::Forbidden)),
    row(A::SubgroupCreate, W::Admin, F::Group, Allow),
    row(A::GroupUpdate, W::Owner, F::Group, Allow),
    row(A::GroupUpdate, W::Maintainer, F::Group, deny(E::Forbidden)),
    // --- Tokens ---
    row(A::TokenCreate, W::Reporter, F::OwnTokenRequest, Allow),
    row(A::TokenCreate, W::Reporter, F::OwnAdminTokenRequest, deny(E::Forbidden)),
    row(A::TokenCreate, W::Admin, F::OwnAdminTokenRequest, Allow),
    row(A::TokenCreate, W::DevTokenApi, F::OwnTokenRequest, deny(E::Forbidden)),
    row(A::TokenCreate, W::AdminTokenAdmin, F::OwnTokenRequest, deny(E::Forbidden)),
    row(A::TokenCreate, W::Admin, F::ForeignTokenRequest, deny(E::NotFound)),
    row(A::TokenList, W::Reporter, F::OwnToken, Allow),
    row(A::TokenList, W::OwnerTokenReadApi, F::OwnToken, Allow),
    row(A::TokenList, W::DevTokenReadRepository, F::OwnToken, deny(E::InsufficientScope)),
    row(A::TokenList, W::Reporter, F::ForeignToken, deny(E::NotFound)),
    row(A::TokenList, W::Admin, F::ForeignToken, deny(E::NotFound)),
    row(A::TokenRevoke, W::Reporter, F::OwnToken, Allow),
    row(A::TokenRevoke, W::DevTokenApi, F::OwnToken, Allow),
    row(A::TokenRevoke, W::DevTokenReadRepository, F::OwnToken, deny(E::InsufficientScope)),
    row(A::TokenRevoke, W::Reporter, F::ForeignToken, deny(E::NotFound)),
    row(A::TokenRevoke, W::AdminTokenApi, F::ForeignToken, deny(E::NotFound)),
    row(A::TokenRevoke, W::Admin, F::ForeignToken, Allow),
    // --- Users and audit ---
    row(A::UserCreate, W::Admin, F::Instance, Allow),
    row(A::UserCreate, W::Owner, F::Instance, deny(E::Forbidden)),
    row(A::UserCreate, W::DisabledAdmin, F::Instance, deny(E::AccountDisabled)),
    row(A::UserCreate, W::AdminTokenApi, F::Instance, deny(E::Forbidden)),
    row(A::UserCreate, W::AdminTokenAdmin, F::Instance, Allow),
    row(A::UserDisable, W::Admin, F::Instance, Allow),
    row(A::UserDisable, W::Maintainer, F::Instance, deny(E::Forbidden)),
    row(A::AuditRead, W::Admin, F::Instance, Allow),
    row(A::AuditRead, W::Owner, F::Instance, deny(E::Forbidden)),
    row(A::AuditRead, W::Owner, F::Group, Allow),
    row(A::AuditRead, W::Maintainer, F::Group, deny(E::Forbidden)),
    row(A::AuditRead, W::Owner, F::PrivateProject, Allow),
    row(A::AuditRead, W::Maintainer, F::PrivateProject, deny(E::Forbidden)),
    row(A::AuditRead, W::NonMember, F::PrivateProject, deny(E::NotFound)),
    // --- Operations ---
    row(A::Health, W::Anonymous, F::Instance, Allow),
    row(A::Metrics, W::Admin, F::Instance, Allow),
    row(A::Metrics, W::Owner, F::Instance, deny(E::Forbidden)),
];
