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
use crate::prove::ResourceCtx;
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
    // 5. The runner API (M4 C3): a runner claims on the instance and
    // works on the jobs it holds; people, tokens and service accounts
    // never reach it (`runner_only`), a job neither (generated rows).
    row(A::RunnerJobRequest, W::Runner, F::Instance, Allow),
    row(A::RunnerJobRequest, W::Admin, F::Instance, deny(E::Forbidden)),
    row(A::RunnerJobRequest, W::Developer, F::Instance, deny(E::Forbidden)),
    row(A::RunnerJobRequest, W::AdminTokenAdmin, F::Instance, deny(E::Forbidden)),
    row(A::RunnerJobRequest, W::AgentTokenApi, F::Instance, deny(E::Forbidden)),
    row(A::RunnerJobRequest, W::ServiceAccountTokenApi, F::Instance, deny(E::Forbidden)),
    row(A::RunnerJobRequest, W::Anonymous, F::Instance, deny(E::Unauthenticated)),
    row(A::RunnerJobRequest, W::Job(J::Live), F::Instance, deny(E::Forbidden)),
    // A runner action on the wrong resource is refused, never decided.
    row(A::RunnerJobRequest, W::Runner, LEASED, deny(E::Forbidden)),
    row(A::RunnerJobHeartbeat, W::Runner, F::Instance, deny(E::Forbidden)),
];

/// Who holds a job's lease in a [`F::RunnerJob`] row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Holder {
    /// The row's runner (`runner()`).
    Me,
    /// Another runner.
    Other,
    /// No runner (waiting, or ended).
    Nobody,
}

/// A running job leased to the row's runner.
const LEASED: F = F::RunnerJob {
    holder: Holder::Me,
    finished: false,
};

/// The lease rows of one runner action on a job.
macro_rules! lease_rows {
    ($action:expr) => {
        [
            row($action, W::Runner, LEASED, Allow),
            // A job cancelled while this runner held it: still its own
            // (the heartbeat delivers the cancel, the completion acks it).
            row(
                $action,
                W::Runner,
                F::RunnerJob {
                    holder: Holder::Me,
                    finished: true,
                },
                Allow,
            ),
            row(
                $action,
                W::Runner,
                F::RunnerJob {
                    holder: Holder::Other,
                    finished: false,
                },
                deny(E::Forbidden),
            ),
            row(
                $action,
                W::Runner,
                F::RunnerJob {
                    holder: Holder::Nobody,
                    finished: false,
                },
                deny(E::Forbidden),
            ),
            row(
                $action,
                W::Runner,
                F::RunnerJob {
                    holder: Holder::Nobody,
                    finished: true,
                },
                deny(E::Forbidden),
            ),
            row($action, W::Job(J::Live), LEASED, deny(E::Forbidden)),
            row($action, W::Admin, LEASED, deny(E::Forbidden)),
            row($action, W::AdminTokenAdmin, LEASED, deny(E::Forbidden)),
            row($action, W::AgentTokenApi, LEASED, deny(E::Forbidden)),
            row(
                $action,
                W::ServiceAccountTokenApi,
                LEASED,
                deny(E::Forbidden),
            ),
            row($action, W::Anonymous, LEASED, deny(E::Unauthenticated)),
        ]
    };
}

#[rustfmt::skip]
const HEARTBEAT: [Row; 11] = lease_rows!(A::RunnerJobHeartbeat);
#[rustfmt::skip]
const LOG: [Row; 11] = lease_rows!(A::RunnerLogAppend);
#[rustfmt::skip]
const COMPLETE: [Row; 11] = lease_rows!(A::RunnerJobComplete);

/// Registering and managing runners (M4 C3): an administrator's web
/// session, people only (the service account and bot twins are in
/// `matrix_rows_people.rs`).
#[rustfmt::skip]
const ADMIN: [Row; 12] = [
    row(A::RunnerRegister, W::Admin, F::Instance, Allow),
    row(A::RunnerRegister, W::AdminTokenAdmin, F::Instance, deny(E::InsufficientScope)),
    row(A::RunnerRegister, W::AgentAdminTokenAdmin, F::Instance, deny(E::InsufficientScope)),
    row(A::RunnerRegister, W::Developer, F::Instance, deny(E::Forbidden)),
    row(A::RunnerRegister, W::DisabledAdmin, F::Instance, deny(E::AccountDisabled)),
    row(A::RunnerRegister, W::Anonymous, F::Instance, deny(E::Unauthenticated)),
    row(A::RunnerManage, W::Admin, F::Instance, Allow),
    row(A::RunnerManage, W::AdminTokenAdmin, F::Instance, deny(E::InsufficientScope)),
    row(A::RunnerManage, W::AgentAdminTokenAdmin, F::Instance, deny(E::InsufficientScope)),
    row(A::RunnerManage, W::Developer, F::Instance, deny(E::Forbidden)),
    row(A::RunnerManage, W::DisabledAdmin, F::Instance, deny(E::AccountDisabled)),
    row(A::RunnerManage, W::Anonymous, F::Instance, deny(E::Unauthenticated)),
];

/// The runner API's and the runner administration's rows (`matrix::ROWS`
/// appends them).
pub(crate) static RUNNER_ROWS: std::sync::LazyLock<Vec<Row>> =
    std::sync::LazyLock::new(|| [HEARTBEAT.as_slice(), &LOG, &COMPLETE, &ADMIN].concat());

/// The job's own artifacts (M4 C6): a live job's token, its own job of its
/// own project only; another job or project is the invisible project's
/// 404; an ended job's token is unauthenticated; a runner, people and
/// agents never.
macro_rules! job_artifact_rows {
    ($action:expr) => {
        [
            row($action, W::Job(J::Live), MINE, Allow),
            row(
                $action,
                W::Job(J::Live),
                F::JobSelf {
                    other_project: false,
                    other_job: true,
                },
                deny(E::NotFound),
            ),
            row(
                $action,
                W::Job(J::Live),
                F::JobSelf {
                    other_project: true,
                    other_job: false,
                },
                deny(E::NotFound),
            ),
            row($action, W::Job(J::Revoked), MINE, deny(E::Unauthenticated)),
            row($action, W::Job(J::Expired), MINE, deny(E::Unauthenticated)),
            row($action, W::Runner, MINE, deny(E::Forbidden)),
            row($action, W::Admin, MINE, deny(E::Forbidden)),
            row($action, W::Developer, MINE, deny(E::Forbidden)),
            row($action, W::AgentTokenApi, MINE, deny(E::Forbidden)),
            row($action, W::ServiceAccountTokenApi, MINE, deny(E::Forbidden)),
            row($action, W::Anonymous, MINE, deny(E::Unauthenticated)),
        ]
    };
}

/// The row's own job of its own project.
const MINE: F = F::JobSelf {
    other_project: false,
    other_job: false,
};

#[rustfmt::skip]
const UPLOAD: [Row; 11] = job_artifact_rows!(A::JobArtifactUpload);
#[rustfmt::skip]
const DOWNLOAD: [Row; 11] = job_artifact_rows!(A::JobArtifactDownload);

/// A job's own-artifact rows (`matrix::ROWS` appends them); a job's
/// pinned copy is not an artifact action (and the reverse).
pub(crate) static JOB_ARTIFACT_ROWS: std::sync::LazyLock<Vec<Row>> =
    std::sync::LazyLock::new(|| {
        let crossed = [
            row(A::RepoRead, W::Job(J::Live), MINE, deny(E::NotFound)),
            row(
                A::JobArtifactUpload,
                W::Job(J::Live),
                OWN,
                deny(E::NotFound),
            ),
        ];
        [UPLOAD.as_slice(), &DOWNLOAD, &crossed].concat()
    });

/// The job's own job (41) or another (42) of its project or another.
#[cfg(test)]
pub(crate) fn job_self(other_project: bool, other_job: bool) -> Resource {
    crate::contexts::JobSelfCtx::new(
        if other_job { 42 } else { 41 },
        if other_project {
            ProjectId::new(8)
        } else {
            JOB_PROJECT
        },
    )
    .resource()
}

/// The runner of every runner row.
#[cfg(test)]
const RUNNER_ID: i64 = 3;

/// A job's lease as the runner API loads it.
#[cfg(test)]
pub(crate) fn runner_job(holder: Holder, finished: bool) -> Resource {
    let holder = match holder {
        Holder::Me => Some(RUNNER_ID),
        Holder::Other => Some(RUNNER_ID + 1),
        Holder::Nobody => None,
    };
    crate::contexts::RunnerJobCtx::new(41, holder, finished).resource()
}

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
    Principal::runner(RunnerCredential {
        runner_id: RUNNER_ID,
    })
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
