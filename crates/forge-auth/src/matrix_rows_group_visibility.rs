//! The permission matrix rows of group visibility (plan 15 MR 3, §4.1;
//! ADR-0010 Amendment "グループの可視性"):
//!
//! - a group's effective visibility is the narrowest on its chain (the
//!   loaders fold it: an internal group below a private one is
//!   `Fixture::PrivateGroup` here, and a project below a private group is
//!   `Fixture::PrivateProject` — its rows in `matrix_rows.rs` are the
//!   project side of §4.1);
//! - a private group is read by the roles on its chain and administrators;
//!   everyone else gets the invisible project's 404 (`FG-REPO-0005`), a
//!   service account too — and on an internal group a service account
//!   without a role as well (it sees what it is a member of, M3 S0);
//! - a caller with a role strictly below sees it as a pass-through
//!   (its identity without its description): `GroupRead` and resolving
//!   its path only, never its members, never a write (404);
//! - creating in, changing, auditing, moving or moving into a group the caller
//!   cannot see is the same 404, never a 403;
//! - `GroupVisibilityChange`: the group's Owner (administrators pass), a
//!   person in a web session (a token is `FG-AUTH-0002
//!   reason=session_required`, a service account or bot `FG-AUTH-0003`).
//!   A value wider than the chain above is `FG-REPO-0033`, decided in the
//!   write transaction (forge-db tests), not here.

use forge_errors::ErrorCode as E;

use crate::action::Action as A;
use crate::matrix::{Actor as W, Expect, Fixture as F, Row};
use crate::model::{MembershipChange, Role};

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

const PRIVATE: F = F::PrivateGroup;
const PASS: F = F::PrivateGroupPassThrough;
const ADD_GUEST: MembershipChange = MembershipChange::Add(Role::Guest);

/// Every group visibility row (`matrix::ROWS` appends them).
#[rustfmt::skip]
pub(crate) const GROUP_VISIBILITY_ROWS: &[Row] = &[
    // --- GroupRead × private: the chain's roles, administrators ---
    row(A::GroupRead, W::Guest, PRIVATE, Allow),
    row(A::GroupRead, W::Owner, PRIVATE, Allow),
    row(A::GroupRead, W::Admin, PRIVATE, Allow),
    row(A::GroupRead, W::AgentTokenReadApi, PRIVATE, Allow),
    row(A::GroupRead, W::GuestTokenReadApi, PRIVATE, Allow),
    row(A::GroupRead, W::NonMember, PRIVATE, deny(E::NotFound)),
    row(A::GroupRead, W::NonMemberTokenReadApi, PRIVATE, deny(E::NotFound)),
    row(A::GroupRead, W::ServiceAccountNonMemberTokenReadApi, PRIVATE, deny(E::NotFound)),
    // An administrator's token without `admin` acts as a normal user.
    row(A::GroupRead, W::AdminTokenApi, PRIVATE, deny(E::NotFound)),
    row(A::GroupRead, W::Anonymous, PRIVATE, deny(E::Unauthenticated)),
    // A wrong-scope token learns nothing about a hidden group either.
    row(A::GroupRead, W::NonMember, PASS, Allow),
    row(A::GroupRead, W::NonMemberTokenReadApi, PASS, Allow),
    // --- GroupRead × internal: a service account by its roles only ---
    row(A::GroupRead, W::ServiceAccountNonMemberTokenReadApi, F::Group, deny(E::NotFound)),
    row(A::GroupRead, W::NonMemberTokenReadApi, F::Group, Allow),
    // --- resolving a private group's path ---
    row(A::PathResolve, W::NonMember, F::ResolvedPrivateGroup { descendant: false }, deny(E::NotFound)),
    row(A::PathResolve, W::NonMember, F::ResolvedPrivateGroup { descendant: true }, Allow),
    row(A::PathResolve, W::Guest, F::ResolvedPrivateGroup { descendant: false }, Allow),
    row(A::PathResolve, W::Admin, F::ResolvedPrivateGroup { descendant: false }, Allow),
    // --- members: whoever sees the group fully ---
    row(A::MemberList, W::Guest, PRIVATE, Allow),
    row(A::MemberList, W::Admin, PRIVATE, Allow),
    row(A::MemberList, W::NonMember, PRIVATE, deny(E::NotFound)),
    row(A::MemberList, W::NonMember, PASS, deny(E::NotFound)),
    row(A::MemberManage, W::Owner, F::PrivateGroupMember(ADD_GUEST), Allow),
    row(A::MemberManage, W::NonMember, F::PrivateGroupMember(ADD_GUEST), deny(E::NotFound)),
    // --- creating in, changing, auditing a group nobody showed the caller ---
    row(A::SubgroupCreate, W::Owner, PRIVATE, Allow),
    row(A::SubgroupCreate, W::Maintainer, PRIVATE, deny(E::Forbidden)),
    row(A::SubgroupCreate, W::NonMember, PRIVATE, deny(E::NotFound)),
    row(A::SubgroupCreate, W::NonMember, PASS, deny(E::NotFound)),
    row(A::ProjectCreate, W::Maintainer, PRIVATE, Allow),
    row(A::ProjectCreate, W::Developer, PRIVATE, deny(E::Forbidden)),
    row(A::ProjectCreate, W::NonMember, PRIVATE, deny(E::NotFound)),
    row(A::ProjectCreate, W::NonMember, PASS, deny(E::NotFound)),
    row(A::ProjectCreate, W::Admin, PRIVATE, Allow),
    row(A::GroupUpdate, W::Owner, PRIVATE, Allow),
    row(A::GroupUpdate, W::NonMember, PRIVATE, deny(E::NotFound)),
    row(A::AuditRead, W::Owner, PRIVATE, Allow),
    row(A::AuditRead, W::NonMember, PASS, deny(E::NotFound)),
    // --- moving a project into a private group ---
    row(A::ProjectTransfer, W::Owner, F::TransferPrivateTarget { target: Some(Role::Maintainer), descendant: false }, Allow),
    row(A::ProjectTransfer, W::Owner, F::TransferPrivateTarget { target: None, descendant: false }, deny(E::NotFound)),
    row(A::ProjectTransfer, W::Owner, F::TransferPrivateTarget { target: None, descendant: true }, deny(E::NotFound)),
    row(A::ProjectTransfer, W::Admin, F::TransferPrivateTarget { target: None, descendant: false }, Allow),
    // --- renaming / moving a private group ---
    row(A::GroupTransfer, W::Owner, F::GroupTransferPrivateSource, Allow),
    row(A::GroupTransfer, W::NonMember, F::GroupTransferPrivateSource, deny(E::NotFound)),
    // --- GroupVisibilityChange: the Owner, a person, a session ---
    row(A::GroupVisibilityChange, W::Owner, F::Group, Allow),
    row(A::GroupVisibilityChange, W::Owner, PRIVATE, Allow),
    row(A::GroupVisibilityChange, W::Admin, F::Group, Allow),
    row(A::GroupVisibilityChange, W::Admin, PRIVATE, Allow),
    row(A::GroupVisibilityChange, W::Maintainer, F::Group, deny(E::Forbidden)),
    row(A::GroupVisibilityChange, W::Maintainer, PRIVATE, deny(E::Forbidden)),
    row(A::GroupVisibilityChange, W::Guest, F::Group, deny(E::Forbidden)),
    row(A::GroupVisibilityChange, W::Guest, PRIVATE, deny(E::Forbidden)),
    // A token is refused before the group is looked at, even a hidden one.
    row(A::GroupVisibilityChange, W::NonMemberTokenReadApi, PASS, deny(E::InsufficientScope)),
    row(A::GroupVisibilityChange, W::NonMember, F::Group, deny(E::Forbidden)),
    row(A::GroupVisibilityChange, W::NonMember, PRIVATE, deny(E::NotFound)),
    row(A::GroupVisibilityChange, W::NonMember, PASS, deny(E::NotFound)),
    row(A::GroupVisibilityChange, W::Anonymous, F::Group, deny(E::Unauthenticated)),
    row(A::GroupVisibilityChange, W::Disabled, F::Group, deny(E::AccountDisabled)),
    row(A::GroupVisibilityChange, W::OwnerTokenApi, F::Group, deny(E::InsufficientScope)),
    row(A::GroupVisibilityChange, W::AgentOwnerTokenApi, F::Group, deny(E::InsufficientScope)),
    row(A::GroupVisibilityChange, W::AdminTokenAdmin, F::Group, deny(E::InsufficientScope)),
    row(A::GroupVisibilityChange, W::ServiceAccountAdminSession, F::Group, deny(E::Forbidden)),
    row(A::GroupVisibilityChange, W::BotAdminSession, F::Group, deny(E::Forbidden)),
];
