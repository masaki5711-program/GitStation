//! The permission matrix rows of the document edit claims (plan 13 §7.5
//! stage 4 part A, ADR-0027 §3, §7): taking, extending and releasing one's
//! own claim is a Developer's (whoever may push may edit), revoking
//! another person's is a Maintainer's — both people only (an agent's token
//! here; service accounts and bots in `matrix_rows_people.rs`), `api`,
//! never acting for a person (the generated act-as rows). Reading the
//! claims and the open revisions is `RepoBrowse` (its rows are the base
//! matrix's).
//!
//! Whose claim it is (the holder's own release, `FG-DOC-0007` for another
//! person's live claim) is not a decision of `authorize()`: forge-db
//! decides it on the locked row after the decision (forge-web's
//! `document_revision_routes.rs`).

use forge_errors::ErrorCode as E;

use crate::action::Action as A;
use crate::matrix::{Actor as W, Expect, Expect::Allow, Fixture as F, Row};

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

#[rustfmt::skip]
const CLAIM: [Row; 14] = [
    row(A::DocumentEditClaim, W::Developer, F::PrivateProject, Allow),
    row(A::DocumentEditClaim, W::Maintainer, F::PrivateProject, Allow),
    row(A::DocumentEditClaim, W::Owner, F::PrivateProject, Allow),
    row(A::DocumentEditClaim, W::Admin, F::PrivateProject, Allow),
    row(A::DocumentEditClaim, W::DevTokenApi, F::PrivateProject, Allow),
    row(A::DocumentEditClaim, W::Reporter, F::PrivateProject, deny(E::Forbidden)),
    row(A::DocumentEditClaim, W::Guest, F::PrivateProject, deny(E::Forbidden)),
    row(A::DocumentEditClaim, W::AgentMaintainerTokenApi, F::PrivateProject, deny(E::Forbidden)),
    row(A::DocumentEditClaim, W::MaintainerTokenReadApi, F::PrivateProject, deny(E::InsufficientScope)),
    row(A::DocumentEditClaim, W::NonMember, F::PrivateProject, deny(E::NotFound)),
    row(A::DocumentEditClaim, W::NonMember, F::InternalProject, deny(E::Forbidden)),
    row(A::DocumentEditClaim, W::Anonymous, F::PrivateProject, deny(E::Unauthenticated)),
    row(A::DocumentEditClaim, W::Disabled, F::PrivateProject, deny(E::AccountDisabled)),
    row(A::DocumentEditClaim, W::Developer, F::DeletedProject, deny(E::PendingDeletion)),
];

#[rustfmt::skip]
const REVOKE: [Row; 14] = [
    row(A::DocumentEditClaimRevoke, W::Maintainer, F::PrivateProject, Allow),
    row(A::DocumentEditClaimRevoke, W::Owner, F::PrivateProject, Allow),
    row(A::DocumentEditClaimRevoke, W::Admin, F::PrivateProject, Allow),
    row(A::DocumentEditClaimRevoke, W::MaintainerTokenApi, F::PrivateProject, Allow),
    row(A::DocumentEditClaimRevoke, W::Developer, F::PrivateProject, deny(E::Forbidden)),
    row(A::DocumentEditClaimRevoke, W::Reporter, F::PrivateProject, deny(E::Forbidden)),
    row(A::DocumentEditClaimRevoke, W::Guest, F::PrivateProject, deny(E::Forbidden)),
    row(A::DocumentEditClaimRevoke, W::AgentMaintainerTokenApi, F::PrivateProject, deny(E::Forbidden)),
    row(A::DocumentEditClaimRevoke, W::MaintainerTokenReadApi, F::PrivateProject, deny(E::InsufficientScope)),
    row(A::DocumentEditClaimRevoke, W::NonMember, F::PrivateProject, deny(E::NotFound)),
    row(A::DocumentEditClaimRevoke, W::NonMember, F::InternalProject, deny(E::Forbidden)),
    row(A::DocumentEditClaimRevoke, W::Anonymous, F::PrivateProject, deny(E::Unauthenticated)),
    row(A::DocumentEditClaimRevoke, W::Disabled, F::PrivateProject, deny(E::AccountDisabled)),
    row(A::DocumentEditClaimRevoke, W::Maintainer, F::DeletedProject, deny(E::PendingDeletion)),
];

/// Every row of the edit claims (`matrix::ROWS` appends them).
pub(crate) static DOC_REVISION_ROWS: std::sync::LazyLock<Vec<Row>> =
    std::sync::LazyLock::new(|| [CLAIM.as_slice(), REVOKE.as_slice()].concat());
