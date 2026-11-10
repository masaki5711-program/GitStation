//! `change_context` never audits a CI principal as a user (ADR-0029 §8).
#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly

use forge_auth::{JobCredential, JobTokenState, Principal, RunnerCredential};
use forge_core::{push::ObjectId, ProjectId, RequestId, UserId};
use forge_errors::ErrorCode;

use crate::{change_context, ReviewRequest};

#[test]
fn a_ci_principal_is_an_internal_error_not_a_user() {
    let request = ReviewRequest {
        request_id: RequestId::generate(),
        credential_id: None,
    };
    let job = Principal::job(JobCredential {
        job_id: 1,
        project: ProjectId::new(7),
        sha: ObjectId::parse("1111111111111111111111111111111111111111").expect("sha"),
        state: JobTokenState::Live,
    });
    let runner = Principal::runner(RunnerCredential { runner_id: 3 });
    for p in [job, runner] {
        let err = change_context(&request, &p, UserId::new(1)).expect_err("ci principal");
        assert_eq!(err.code(), ErrorCode::Internal);
        assert!(err
            .context()
            .iter()
            .any(|(k, v)| *k == "reason" && v == "ci_principal_in_review"));
    }
}
