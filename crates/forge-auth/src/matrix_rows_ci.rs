//! The permission matrix rows of the CI principals (M4 C0, ADR-0029): a
//! registered runner's token and a job's short-lived token, decided in
//! `job.rs` before every other rule of `authorize()`.
//!
//! 1. generated: every action whose `ci_rule()` is not the principal's
//!    own is refused (`FG-AUTH-0003 reason=ci_not_allowed`) for a live job
//!    (`ci_denied_for_<action>`, the job's column) and for a runner (the
//!    runner's column; no runner action exists before M4 C3) —
//!    [`generated_denies`];
//! 2. a job's copy (`RepoRead` on `Resource::PinnedCopy`): its own project
//!    at its pinned SHA is allowed whatever the confidential marks (the
//!    copy is never measured); another project or another SHA is the
//!    invisible project's 404; an expired or revoked token is
//!    unauthenticated; a plain project read or a measured copy, visible or
//!    hidden, is the same 404 (`job_requires_pinned_copy` in the log);
//! 3. a runner reads no repository (a pinned copy, a measured copy);
//! 4. a pinned copy is the job's alone: people, administrators, agents'
//!    tokens and service accounts are refused (`pinned_copy_without_job`).
//!    Their measured copies (`RepoCopy`, `FG-AUTH-0022`) keep their rows in
//!    `matrix_rows_repo_copy.rs`, unchanged.

use forge_errors::ErrorCode as E;

use crate::action::{Action as A, CiRule};
use crate::matrix::{Actor as W, Expect, Fixture as F, Row};
use crate::model::{CopyMark as M, JobTokenState as J};

#[cfg(test)]
use crate::model::{JobCredential, Principal, Resource, RunnerCredential};
#[cfg(test)]
use forge_core::{push::ObjectId, ProjectId};

/// The job's project and pinned SHA in every job row.
#[cfg(test)]
pub(crate) const JOB_PROJECT: ProjectId = ProjectId::new(7);
#[cfg(test)]
pub(crate) const JOB_SHA: &str = "1111111111111111111111111111111111111111";
#[cfg(test)]
const OTHER_SHA: &str = "2222222222222222222222222222222222222222";

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

const OWN: F = F::PinnedCopy {
    other_project: false,
    other_sha: false,
};

#[rustfmt::skip]
pub(crate) const CI_ROWS: &[Row] = &[
    // 2. A job's own project at its pinned SHA: allowed (no mark looked at).
    row(A::RepoRead, W::Job(J::Live), OWN, Allow),
    row(A::RepoRead, W::Job(J::Live), F::PinnedCopy { other_project: true, other_sha: false }, deny(E::NotFound)),
    row(A::RepoRead, W::Job(J::Live), F::PinnedCopy { other_project: false, other_sha: true }, deny(E::NotFound)),
    row(A::RepoRead, W::Job(J::Live), F::PinnedCopy { other_project: true, other_sha: true }, deny(E::NotFound)),
    row(A::RepoRead, W::Job(J::Expired), OWN, deny(E::Unauthenticated)),
    row(A::RepoRead, W::Job(J::Revoked), OWN, deny(E::Unauthenticated)),
    // Anything but its pinned copy: the same 404, a hidden (private) or a
    // visible (internal) project alike.
    row(A::RepoRead, W::Job(J::Live), F::PrivateProject, deny(E::NotFound)),
    row(A::RepoRead, W::Job(J::Live), F::InternalProject, deny(E::NotFound)),
    row(A::RepoRead, W::Job(J::Live), F::RepoCopy(M::Open), deny(E::NotFound)),
    row(A::RepoRead, W::Job(J::Live), F::InternalRepoCopy(M::Confidential), deny(E::NotFound)),
    // 3. A runner reads no repository.
    row(A::RepoRead, W::Runner, OWN, deny(E::Forbidden)),
    row(A::RepoRead, W::Runner, F::RepoCopy(M::Open), deny(E::Forbidden)),
    // 4. A pinned copy is the job's alone.
    row(A::RepoRead, W::Reporter, OWN, deny(E::Forbidden)),
    row(A::RepoRead, W::Admin, OWN, deny(E::Forbidden)),
    row(A::RepoRead, W::DevTokenReadRepository, OWN, deny(E::Forbidden)),
    row(A::RepoRead, W::AgentTokenReadRepository, OWN, deny(E::Forbidden)),
    row(A::RepoRead, W::ServiceAccountTokenReadRepository, OWN, deny(E::Forbidden)),
];

/// Rows 1: every action a live job or a runner never performs
/// (`ci_denied_for_<action>`, ADR-0029 §2), on a private project. The
/// act-as confirmation answers every refusal with the invisible project's
/// 404 (ADR-0025, decision of 2026-11-10), a CI principal's too.
pub(crate) fn generated_denies() -> Vec<Row> {
    let refusal = |a: A| {
        if matches!(a, A::ActAsConfirmView | A::ActAsAssertionIssue) {
            deny(E::NotFound)
        } else {
            deny(E::Forbidden)
        }
    };
    let job = A::ALL
        .iter()
        .filter(|a| a.ci_rule() != CiRule::Job)
        .map(|&a| row(a, W::Job(J::Live), F::PrivateProject, refusal(a)));
    let runner = A::ALL
        .iter()
        .filter(|a| a.ci_rule() != CiRule::Runner)
        .map(|&a| row(a, W::Runner, F::PrivateProject, refusal(a)));
    job.chain(runner).collect()
}

/// The runner of every runner row.
#[cfg(test)]
pub(crate) fn runner() -> Principal {
    Principal::runner(RunnerCredential { runner_id: 3 })
}

/// The job of every job row, its token in `state`.
#[cfg(test)]
pub(crate) fn job(state: J) -> Principal {
    Principal::job(JobCredential {
        job_id: 41,
        project: JOB_PROJECT,
        sha: sha(JOB_SHA),
        state,
    })
}

/// A pinned copy of the job's project (or another) at its SHA (or another).
#[cfg(test)]
pub(crate) fn pinned_copy(other_project: bool, other_sha: bool) -> Resource {
    Resource::PinnedCopy(crate::model::PinnedCopy::new(
        if other_project {
            ProjectId::new(8)
        } else {
            JOB_PROJECT
        },
        sha(if other_sha { OTHER_SHA } else { JOB_SHA }),
    ))
}

#[cfg(test)]
#[allow(clippy::expect_used)] // fixture SHAs are valid
pub(crate) fn sha(raw: &str) -> ObjectId {
    ObjectId::parse(raw).expect("fixture SHAs are valid")
}
