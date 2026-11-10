//! The permission matrix rows of the CI outputs (M4 C5, ADR-0029 decision
//! 5, plan 14 §4.2): a job's log (`CiLogRead`) and a pipeline's diagnosis
//! (`PipelineDiagnosisRead`).
//!
//! Two decisions, one rule each:
//! 1. the route's proof, on the project: as `RepoBrowse` (Reporter, any
//!    signed-in person on an internal project, `read_api`); an agent
//!    audience passes here (it is measured below);
//! 2. `authorize_ci_output` on `Resource::CiOutput`: the project read again,
//!    then an AI agent audience — a token issued with `is_agent`, a service
//!    account, a bot — only when the pipeline's mark (measured at its
//!    creation) **and** the default branch's tip are both `Open`; anything
//!    else is `FG-AUTH-0022` (`mark_of=pipeline|tip`). `Undecided`,
//!    `Unborn` and `NotMeasured` are closed (ADR-0029 obligation 4). A
//!    person is never refused by the marks.
//!
//! A runner and a job are refused both actions
//! (`matrix_rows_ci::generated_denies`); neither is performed acting for a
//! person (`matrix_rows_act_as`).

use forge_errors::ErrorCode as E;

use crate::action::Action as A;
use crate::matrix::{Actor as W, Expect, Fixture as F, Row};
use crate::model::CopyMark as M;

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

const fn out(pipeline_mark: M, tip_mark: M) -> F {
    F::CiOutput {
        internal: false,
        pipeline_mark,
        tip_mark,
    }
}

const OPEN: F = out(M::Open, M::Open);
const WITHHELD: E = E::RepositoryCopyWithheld;

/// The same rows for both actions.
macro_rules! ci_output_rows {
    ($a:expr) => {
        [
            // 1. The route's proof on the project.
            row($a, W::Reporter, F::PrivateProject, Allow),
            row($a, W::Developer, F::PrivateProject, Allow),
            row($a, W::Admin, F::PrivateProject, Allow),
            row($a, W::NonMember, F::InternalProject, Allow),
            row($a, W::AgentTokenReadApi, F::PrivateProject, Allow),
            row($a, W::ServiceAccountTokenReadApi, F::PrivateProject, Allow),
            row($a, W::OwnerTokenReadApi, F::PrivateProject, Allow),
            row($a, W::Guest, F::PrivateProject, deny(E::Forbidden)),
            row($a, W::GuestTokenReadApi, F::PrivateProject, deny(E::Forbidden)),
            row($a, W::DevTokenReadRepository, F::PrivateProject, deny(E::InsufficientScope)),
            row($a, W::NonMember, F::PrivateProject, deny(E::NotFound)),
            row($a, W::Anonymous, F::PrivateProject, deny(E::Unauthenticated)),
            row($a, W::Disabled, F::PrivateProject, deny(E::AccountDisabled)),
            row($a, W::Reporter, F::DeletedProject, deny(E::PendingDeletion)),
            // 2. People: never refused by the marks.
            row($a, W::Reporter, out(M::Confidential, M::NotMeasured), Allow),
            row($a, W::Developer, out(M::Undecided, M::Confidential), Allow),
            row($a, W::OwnerTokenReadApi, out(M::Confidential, M::Unborn), Allow),
            row($a, W::Admin, out(M::Undecided, M::Undecided), Allow),
            row($a, W::NonMember, F::CiOutput { internal: true, pipeline_mark: M::Confidential, tip_mark: M::NotMeasured }, Allow),
            // Agents: both marks open only.
            row($a, W::AgentTokenReadApi, OPEN, Allow),
            row($a, W::AgentTokenApi, OPEN, Allow),
            row($a, W::ServiceAccountTokenReadApi, OPEN, Allow),
            row($a, W::BotTokenApi, OPEN, Allow),
            row($a, W::AgentTokenReadApi, out(M::Confidential, M::Open), deny(WITHHELD)),
            row($a, W::AgentTokenReadApi, out(M::Undecided, M::Open), deny(WITHHELD)),
            row($a, W::AgentTokenReadApi, out(M::Open, M::Confidential), deny(WITHHELD)),
            row($a, W::AgentTokenReadApi, out(M::Open, M::Undecided), deny(WITHHELD)),
            row($a, W::AgentTokenReadApi, out(M::Open, M::Unborn), deny(WITHHELD)),
            row($a, W::AgentTokenReadApi, out(M::Open, M::NotMeasured), deny(WITHHELD)),
            row($a, W::AgentTokenApi, out(M::Confidential, M::Confidential), deny(WITHHELD)),
            row($a, W::ServiceAccountTokenReadApi, out(M::Undecided, M::Open), deny(WITHHELD)),
            row($a, W::ServiceAccountTokenReadApi, out(M::Open, M::Confidential), deny(WITHHELD)),
            row($a, W::BotTokenApi, out(M::Confidential, M::Open), deny(WITHHELD)),
            row($a, W::BotTokenApi, out(M::Open, M::Undecided), deny(WITHHELD)),
            // The project read comes first: no oracle on the marks for
            // who cannot read the project.
            row($a, W::Anonymous, OPEN, deny(E::Unauthenticated)),
            row($a, W::NonMember, out(M::Confidential, M::Open), deny(E::NotFound)),
            row($a, W::Guest, out(M::Confidential, M::Open), deny(E::Forbidden)),
            row($a, W::AgentTokenWriteRepository, out(M::Confidential, M::Open), deny(E::InsufficientScope)),
        ]
    };
}

#[rustfmt::skip]
const LOG: [Row; 38] = ci_output_rows!(A::CiLogRead);
#[rustfmt::skip]
const DIAGNOSIS: [Row; 38] = ci_output_rows!(A::PipelineDiagnosisRead);

/// Every row of the CI outputs (`matrix::ROWS` appends them).
pub(crate) static CI_OUTPUT_ROWS: std::sync::LazyLock<Vec<Row>> =
    std::sync::LazyLock::new(|| [LOG.as_slice(), &DIAGNOSIS].concat());
