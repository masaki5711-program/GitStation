#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! The pure part: the protected patterns and the plan for each trigger.

use forge_core::{MergeRule, ProtectionRule, RefPattern, Role};

use super::*;

fn rules(patterns: &[&str]) -> ProtectionRules {
    ProtectionRules::new(
        patterns
            .iter()
            .map(|p| ProtectionRule {
                pattern: RefPattern::parse(p).expect("pattern"),
                push_min_role: Role::Maintainer,
                merge: MergeRule::default(),
            })
            .collect(),
    )
}

fn sha() -> ObjectId {
    ObjectId::parse(&"b".repeat(40)).expect("sha")
}

fn request(branch: &str, source: Source) -> PipelineRequest {
    PipelineRequest {
        project: ProjectId::new(1),
        git_ref: RefName::parse(&format!("refs/heads/{branch}")).expect("branch"),
        sha: sha(),
        source,
        merge_request: (source == Source::MergeRequest).then(|| forge_core::MergeRequestId::new(4)),
        triggered_by: None,
    }
}

fn image() -> String {
    format!("registry.internal/rust@sha256:{}", "a".repeat(64))
}

fn definition(with_deploy: bool) -> Vec<u8> {
    let mut text = format!(
        "version: 1\nstages: [build, deploy]\njobs:\n  build:\n    stage: build\n    image: {}\n    timeout: 10m\n    script: [cargo build]\n",
        image()
    );
    if with_deploy {
        text.push_str(&format!(
            "  deploy:\n    stage: deploy\n    image: {}\n    timeout: 10m\n    script: [./deploy.sh]\n    secrets: [DEPLOY_KEY]\n    rules:\n      branches: [main]\n      merge_requests: false\n",
            image()
        ));
    }
    text.into_bytes()
}

fn jobs((planned, _): (db::Planned, Option<String>)) -> Vec<(String, i32, Vec<String>)> {
    match planned {
        db::Planned::Jobs(jobs) => jobs
            .into_iter()
            .map(|j| (j.name, j.wave, j.needs))
            .collect(),
        other @ (db::Planned::Invalid(_) | db::Planned::Error) => panic!("not planned: {other:?}"),
    }
}

fn reasons((planned, _): (db::Planned, Option<String>)) -> Vec<String> {
    match planned {
        db::Planned::Invalid(problems) => problems.into_iter().map(|p| p.reason).collect(),
        other @ (db::Planned::Jobs(_) | db::Planned::Error) => panic!("not invalid: {other:?}"),
    }
}

#[test]
fn the_protected_patterns_are_branches_only_with_a_trailing_star() {
    let r = rules(&[
        "refs/heads/main",
        "refs/heads/release/*",
        "refs/tags/*",
        "refs/heads/a*b",
    ]);
    assert_eq!(branch_patterns(&r), ["main", "release/*"]);
}

#[test]
fn a_push_to_a_protected_branch_plans_its_secret_job() {
    let planned = Pipelines::plan(
        &request("main", Source::Push),
        &definition(true),
        &rules(&["refs/heads/main"]),
    )
    .expect("plan");
    assert_eq!(
        jobs(planned),
        [
            ("build".to_owned(), 0, vec![]),
            ("deploy".to_owned(), 1, vec!["build".to_owned()]),
        ]
    );
}

#[test]
fn an_unprotected_repository_makes_a_secret_job_invalid() {
    // The lint with the real (empty) protected list refuses it.
    let planned = Pipelines::plan(
        &request("main", Source::Push),
        &definition(true),
        &rules(&[]),
    )
    .expect("plan");
    assert_eq!(reasons(planned), ["secret_on_unprotected_ref"]);
}

#[test]
fn a_merge_request_pipeline_is_never_protected() {
    // MR pipelines run with protected = false: the deploy job (secrets,
    // main only, no MRs) is not selected, the build is.
    let planned = Pipelines::plan(
        &request("main", Source::MergeRequest),
        &definition(true),
        &rules(&["refs/heads/main"]),
    )
    .expect("plan");
    assert_eq!(jobs(planned), [("build".to_owned(), 0, vec![])]);
    // A job with secrets that does run for merge requests is invalid.
    let text = String::from_utf8(definition(true))
        .expect("utf8")
        .replace("merge_requests: false", "merge_requests: true");
    let planned = Pipelines::plan(
        &request("main", Source::MergeRequest),
        text.as_bytes(),
        &rules(&["refs/heads/main"]),
    )
    .expect("plan");
    assert!(reasons(planned).contains(&"secret_on_unprotected_ref".to_owned()));
}

#[test]
fn a_manual_run_on_a_protected_branch_is_not_protected() {
    let planned = Pipelines::plan(
        &request("main", Source::Manual),
        &definition(true),
        &rules(&["refs/heads/main"]),
    )
    .expect("plan");
    assert_eq!(reasons(planned), ["secret_on_unprotected_ref"]);
}

#[test]
fn a_branch_no_job_selects_plans_nothing() {
    let text = String::from_utf8(definition(false)).expect("utf8").replace(
        "script: [cargo build]\n",
        "script: [cargo build]\n    rules:\n      branches: [main]\n",
    );
    let planned = Pipelines::plan(&request("docs", Source::Push), text.as_bytes(), &rules(&[]))
        .expect("plan");
    assert_eq!(jobs(planned), []);
}

#[test]
fn a_definition_with_a_cycle_is_invalid() {
    let text = format!(
        "version: 1\nstages: [test]\njobs:\n  a:\n    stage: test\n    image: {i}\n    timeout: 1m\n    script: [x]\n    needs: [b]\n  b:\n    stage: test\n    image: {i}\n    timeout: 1m\n    script: [x]\n    needs: [a]\n",
        i = image()
    );
    let planned = Pipelines::plan(&request("main", Source::Push), text.as_bytes(), &rules(&[]))
        .expect("plan");
    assert!(reasons(planned).iter().all(|r| r == "needs_cycle"));
}

/// Plan 17 P17-2 (ADR-0034): the job that declares `eval-report.json` is
/// recorded with the pipeline; a definition without it records none (no
/// `forge/ai-eval` for that pipeline).
#[test]
fn the_job_declaring_the_eval_report_is_planned_with_the_pipeline() {
    let (_, none) = Pipelines::plan(
        &request("feature", Source::MergeRequest),
        &definition(false),
        &rules(&[]),
    )
    .expect("plan");
    assert_eq!(none, None);
    let text = String::from_utf8(definition(false)).expect("utf8").replace(
        "script: [cargo build]\n",
        "script: [cargo build]\n    artifacts:\n      paths: [eval-report.json]\n",
    );
    let (planned, job) = Pipelines::plan(
        &request("feature", Source::MergeRequest),
        text.as_bytes(),
        &rules(&[]),
    )
    .expect("plan");
    assert_eq!(job.as_deref(), Some("build"));
    assert_eq!(jobs((planned, job)), [("build".to_owned(), 0, vec![])]);
}
