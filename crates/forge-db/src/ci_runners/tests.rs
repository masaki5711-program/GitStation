//! CI runners against a migrated database (M4 C3): registration and its
//! audit, runner and job token authentication (the database's clock),
//! `SKIP LOCKED` claims under concurrency, tags, the lease (heartbeat,
//! cancel delivery, another runner's job), completion and the pipeline's
//! state, the reaper (exactly once, the third loss), the log limits, the
//! secret release rule, and the revocation with the project.

use forge_core::ci_token::{CiToken, CiTokenKind};
use forge_core::{CiIdentity, CiJobTokenState, ObjectId, RequestId, UserId};
use forge_errors::ErrorCode;

use super::lease::{self, Appended, Heartbeat, JobResult};
use super::*;
use crate::change::{Actor, ChangeContext};
use crate::namespaces::{Destination, MoveOptions, RateLimit};
use crate::pipelines::{self, CopyMark, NewCiJob, NewPipeline, PipelineRequest, Planned, Source};
use crate::projects::{self, CreatedProject, NewProject};
use crate::test_support::*;
use crate::users::{self, NewUser};

fn ctx(actor: Actor, request: &str) -> ChangeContext {
    ChangeContext::new(actor, RequestId::parse(request).expect("request id"))
}

fn rid() -> RequestId {
    RequestId::parse("req-ci-auth").expect("request id")
}

struct World {
    t: FreshDb,
    alice: UserId,
    web: CreatedProject,
}

async fn world() -> World {
    let t = FreshDb::migrated().await;
    let alice = users::create(
        &t.db,
        &ctx(Actor::System, "req-user-alice"),
        &NewUser::human("alice", "alice", "alice@example.test"),
    )
    .await
    .expect("alice");
    let web = projects::create(
        &t.db,
        &ctx(Actor::System, "req-project-web"),
        &NewProject::new("web", alice.namespace_id, "Web", alice.user_id),
    )
    .await
    .expect("web");
    World {
        t,
        alice: alice.user_id,
        web,
    }
}

fn job(name: &str, needs: &[&str], tags: &[&str], secrets: &[&str]) -> NewCiJob {
    NewCiJob {
        name: name.to_owned(),
        stage: "test".to_owned(),
        wave: i32::from(!needs.is_empty()),
        needs: needs.iter().map(|n| (*n).to_owned()).collect(),
        image: format!("registry.internal/rust@sha256:{}", "a".repeat(64)),
        timeout_s: 600,
        tags: tags.iter().map(|n| (*n).to_owned()).collect(),
        secrets: secrets.iter().map(|n| (*n).to_owned()).collect(),
        manual: false,
    }
}

impl World {
    async fn runner(&self, name: &str, tags: &[&str]) -> (i64, CiToken) {
        let r = register(
            &self.t.db,
            &ctx(Actor::User(self.alice), &format!("req-reg-{name}")),
            &NewRunner {
                name: name.to_owned(),
                description: String::new(),
                tags: tags.iter().map(|t| (*t).to_owned()).collect(),
            },
        )
        .await
        .expect("register");
        (r.runner.id, r.token)
    }

    async fn pipeline(&self, sha: char, protected: bool, jobs: Vec<NewCiJob>) -> i64 {
        let new = NewPipeline {
            ai_eval_job: None,
            request: PipelineRequest {
                project: self.web.project_id,
                git_ref: forge_core::RefName::parse("refs/heads/main").expect("ref"),
                sha: ObjectId::parse(&sha.to_string().repeat(40)).expect("sha"),
                source: Source::Push,
                merge_request: None,
                triggered_by: None,
            },
            request_id: "req-plan".to_owned(),
            definition_hash: Some(pipelines::definition_hash(b"version: 1")),
            protected,
            copy_mark: CopyMark::Open,
            planned: Planned::Jobs(jobs),
        };
        pipelines::create(&self.t.db, &new)
            .await
            .expect("pipeline")
            .pipeline_id
    }

    async fn exec(&self, sql: &str) {
        self.t.exec_superuser(sql).await;
    }

    async fn scalar(&self, sql: &str) -> String {
        let mut su = self.t.superuser().await;
        sqlx::query_scalar::<_, String>(sql)
            .fetch_one(&mut su)
            .await
            .expect("scalar")
    }
}

#[tokio::test]
async fn register_shows_the_token_once_and_audits_the_runner() {
    let w = world().await;
    let (id, token) = w.runner("linux-1", &["podman"]).await;
    let who = authenticate(&w.t.db, &rid(), &token.reveal())
        .await
        .expect("runner token");
    assert_eq!(who, CiIdentity::Runner { runner_id: id });
    assert_eq!(
        w.scalar(
            "SELECT action || ':' || resource_type || ':' || resource_id FROM forge.audit_log \
                  WHERE action LIKE 'ci_runner.%'"
        )
        .await,
        format!("ci_runner.registered:ci_runner:{id}")
    );
    // The hash is stored, never the secret; the audit names no token.
    let details = w
        .scalar("SELECT details::text FROM forge.audit_log WHERE action = 'ci_runner.registered'")
        .await;
    assert!(!details.contains(&token.reveal()[4..]), "{details}");
    // The name is unique.
    let err = register(
        &w.t.db,
        &ctx(Actor::User(w.alice), "req-reg-again"),
        &NewRunner {
            name: "linux-1".to_owned(),
            description: String::new(),
            tags: Vec::new(),
        },
    )
    .await
    .expect_err("taken");
    assert_reason(&err, ErrorCode::RequestInvalid, "runner_name_taken");
    // A wrong secret, an unknown id, a malformed token: FG-AUTH-0001.
    let other = CiToken::generate(CiTokenKind::Runner).expect("rng");
    let forged = format!(
        "fgr_{}_{}",
        token.public_id().as_str(),
        &other.reveal()[21..]
    );
    for (presented, reason) in [
        (forged.as_str(), "mismatch"),
        (other.reveal().as_str(), "unknown"),
        ("fgr_nothex_00", "malformed"),
    ] {
        let err = authenticate(&w.t.db, &rid(), presented)
            .await
            .expect_err("refused");
        assert_reason(&err, ErrorCode::Unauthenticated, reason);
    }
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_disabled_runner_is_refused_and_claims_nothing() {
    let w = world().await;
    let (id, token) = w.runner("linux-1", &[]).await;
    w.pipeline('b', false, vec![job("build", &[], &[], &[])])
        .await;
    let updated = update(
        &w.t.db,
        &ctx(Actor::User(w.alice), "req-disable"),
        id,
        &RunnerUpdate {
            enabled: Some(false),
            ..RunnerUpdate::default()
        },
    )
    .await
    .expect("update")
    .expect("runner");
    assert!(!updated.enabled);
    let err = authenticate(&w.t.db, &rid(), &token.reveal())
        .await
        .expect_err("disabled");
    assert_reason(&err, ErrorCode::Unauthenticated, "runner_disabled");
    assert!(lease::claim(&w.t.db, id).await.expect("claim").is_none());
    // An update that changes nothing writes no audit row.
    update(
        &w.t.db,
        &ctx(Actor::User(w.alice), "req-disable-again"),
        id,
        &RunnerUpdate {
            enabled: Some(false),
            ..RunnerUpdate::default()
        },
    )
    .await
    .expect("update")
    .expect("runner");
    assert_eq!(
        w.scalar("SELECT count(*)::text FROM forge.audit_log WHERE action = 'ci_runner.updated'")
            .await,
        "1"
    );
    assert!(update(
        &w.t.db,
        &ctx(Actor::User(w.alice), "req-absent"),
        id + 100,
        &RunnerUpdate::default()
    )
    .await
    .expect("update")
    .is_none());
    w.t.drop_now().await;
}

/// `FOR UPDATE SKIP LOCKED`: runners claiming at once never share a job.
#[tokio::test]
async fn two_runners_never_claim_the_same_job() {
    let w = world().await;
    let (r1, _) = w.runner("r1", &[]).await;
    let (r2, _) = w.runner("r2", &[]).await;
    let jobs: Vec<NewCiJob> = (0..6)
        .map(|i| job(&format!("j{i}"), &[], &[], &[]))
        .collect();
    w.pipeline('b', false, jobs).await;
    let mut seen = Vec::new();
    loop {
        let (a, b) = tokio::join!(lease::claim(&w.t.db, r1), lease::claim(&w.t.db, r2));
        let (a, b) = (a.expect("claim 1"), b.expect("claim 2"));
        if a.is_none() && b.is_none() {
            // Both may skip a pipeline the other held: try once more alone.
            match lease::claim(&w.t.db, r1).await.expect("claim") {
                Some(c) => seen.push(c.job_id),
                None => break,
            }
            continue;
        }
        seen.extend(a.map(|c| c.job_id));
        seen.extend(b.map(|c| c.job_id));
    }
    let mut unique = seen.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), 6, "{seen:?}");
    assert_eq!(seen.len(), 6, "a job was handed out twice: {seen:?}");
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_claim_needs_runner_tags_covering_the_jobs() {
    let w = world().await;
    let (plain, _) = w.runner("plain", &[]).await;
    let (linux, _) = w.runner("linux", &["linux", "podman"]).await;
    w.pipeline('b', false, vec![job("build", &[], &["linux"], &[])])
        .await;
    assert!(lease::claim(&w.t.db, plain).await.expect("claim").is_none());
    let c = lease::claim(&w.t.db, linux)
        .await
        .expect("claim")
        .expect("job");
    assert_eq!(c.name, "build");
    assert_eq!(c.tags, vec!["linux".to_owned()]);
    w.t.drop_now().await;
}

/// The GPU runner's policy allowing `project` (one running job at a time).
fn gpu_policy(project: i64) -> forge_core::config::GpuEvalPolicy {
    forge_core::config::GpuEvalPolicy {
        projects: vec![project],
        max_running_per_project: 1,
    }
}

/// Plan 17 P17-1: a runner tagged `gpu-eval` takes only jobs that ask for
/// it — never an untagged job, nor one with only its other tags — while an
/// untagged job still goes to any other runner, as before.
#[tokio::test]
async fn untagged_job_never_assigned_to_gpu_runner() {
    let w = world().await;
    let policy = gpu_policy(w.web.project_id.get());
    let (gpu, _) = w.runner("gpu", &["gpu-eval", "linux"]).await;
    let (plain, _) = w.runner("plain", &[]).await;
    w.pipeline(
        'b',
        false,
        vec![
            job("untagged", &[], &[], &[]),
            job("linux", &[], &["linux"], &[]),
            job("bench", &[], &["gpu-eval"], &[]),
        ],
    )
    .await;
    // The GPU runner passes over the two older jobs and takes the bench.
    let c = lease::claim_with(&w.t.db, gpu, &policy)
        .await
        .expect("claim")
        .expect("the bench");
    assert_eq!(c.name, "bench");
    assert!(lease::claim_with(&w.t.db, gpu, &policy)
        .await
        .expect("claim")
        .is_none());
    // The untagged job is the plain runner's, as before; the `linux` job
    // waits for a runner with that tag that is not exclusive.
    let c = lease::claim(&w.t.db, plain)
        .await
        .expect("claim")
        .expect("the untagged job");
    assert_eq!(c.name, "untagged");
    assert!(lease::claim(&w.t.db, plain).await.expect("claim").is_none());
    assert_eq!(lease::EXCLUSIVE_RUNNER_TAGS, ["gpu-eval"]);
    w.t.drop_now().await;
}

/// Plan 17 P17-1 (review): a `gpu-eval` job is claimed only for a project
/// of `ci.gpu_eval_projects` (none by default), and at most
/// `ci.gpu_eval_max_running` of one project run at once.
#[tokio::test]
async fn a_gpu_eval_job_needs_its_project_listed_and_a_free_slot() {
    let w = world().await;
    let project = w.web.project_id.get();
    let (gpu, _) = w.runner("gpu", &["gpu-eval"]).await;
    w.pipeline(
        'b',
        false,
        vec![
            job("bench-1", &[], &["gpu-eval"], &[]),
            job("bench-2", &[], &["gpu-eval"], &[]),
        ],
    )
    .await;
    // Not listed (the default, or another project): never claimed.
    assert!(lease::claim(&w.t.db, gpu).await.expect("claim").is_none());
    assert!(lease::claim_with(&w.t.db, gpu, &gpu_policy(project + 1000))
        .await
        .expect("claim")
        .is_none());
    // Listed: one at a time.
    let policy = gpu_policy(project);
    let first = lease::claim_with(&w.t.db, gpu, &policy)
        .await
        .expect("claim")
        .expect("the first bench");
    assert!(lease::claim_with(&w.t.db, gpu, &policy)
        .await
        .expect("claim")
        .is_none());
    let two = forge_core::config::GpuEvalPolicy {
        max_running_per_project: 2,
        ..policy.clone() // clone: a second policy
    };
    let second = lease::claim_with(&w.t.db, gpu, &two)
        .await
        .expect("claim")
        .expect("the second bench, with two slots");
    assert_ne!(first.job_id, second.job_id);
    // A finished job frees its slot.
    lease::complete(
        &w.t.db,
        gpu,
        first.job_id,
        first.attempt,
        JobResult::Succeeded,
    )
    .await
    .expect("complete");
    lease::complete(
        &w.t.db,
        gpu,
        second.job_id,
        second.attempt,
        JobResult::Succeeded,
    )
    .await
    .expect("complete");
    w.t.drop_now().await;
}

/// Plan 19 §1.1: while interactive LLM inference is queued or running, a
/// `gpu-eval` job's claim waits — at most `GPU_EVAL_MAX_DELAY` (under the
/// 10 minutes of FG-CI-0005) — and a stale interactive row (a dead
/// worker's) holds nothing.
#[tokio::test]
async fn gpu_eval_claim_waits_at_most_10_min() {
    let w = world().await;
    let project = w.web.project_id.get();
    let policy = gpu_policy(project);
    let (gpu, _) = w.runner("gpu", &["gpu-eval"]).await;
    let (plain, _) = w.runner("plain", &[]).await;
    w.pipeline(
        'b',
        false,
        vec![
            job("bench", &[], &["gpu-eval"], &[]),
            job("build", &[], &[], &[]),
        ],
    )
    .await;
    let interactive = |age: &str| {
        format!(
            "INSERT INTO forge.llm_requests (purpose, priority, subject_kind, model, prompt_id, \
               prompt_version, input_sha256, input_class, request_id, queued_at) \
             VALUES ('approval_summary', 'interactive', 'deployment', 'qwen3:27b', 'approval', \
               1, repeat('a', 64), 'internal', 'req-llm', now() - interval '{age}') RETURNING id"
        )
    };
    let live: i64 = sqlx::query_scalar(&interactive("10 seconds"))
        .fetch_one(&mut w.t.superuser().await)
        .await
        .expect("interactive row");
    assert!(
        lease::claim_with(&w.t.db, gpu, &policy)
            .await
            .expect("claim")
            .is_none(),
        "the bench waits for the interactive inference"
    );
    // Other jobs are not held.
    assert!(lease::claim(&w.t.db, plain).await.expect("claim").is_some());
    // Pending for the longest delay: handed out anyway.
    let delay = forge_core::llm::GPU_EVAL_MAX_DELAY.as_secs();
    assert!(delay <= 600);
    w.t.exec_superuser(&format!(
        "UPDATE forge.ci_jobs SET updated_at = now() - make_interval(secs => {delay}) \
         WHERE name = 'bench'"
    ))
    .await;
    let c = lease::claim_with(&w.t.db, gpu, &policy)
        .await
        .expect("claim")
        .expect("the bench after the delay");
    assert_eq!(c.name, "bench");
    lease::complete(&w.t.db, gpu, c.job_id, c.attempt, JobResult::Succeeded)
        .await
        .expect("complete");
    // A second bench: a stale interactive row (a dead worker's) holds nothing.
    w.t.exec_superuser(&format!(
        "UPDATE forge.llm_requests SET queued_at = now() - interval '20 minutes' WHERE id = {live}"
    ))
    .await;
    w.pipeline('c', false, vec![job("bench", &[], &["gpu-eval"], &[])])
        .await;
    assert!(lease::claim_with(&w.t.db, gpu, &policy)
        .await
        .expect("claim")
        .is_some());
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_job_token_lives_with_its_job_and_the_pipeline_follows_its_jobs() {
    let w = world().await;
    let (r, _) = w.runner("r", &[]).await;
    let p = w
        .pipeline(
            'b',
            false,
            vec![
                job("build", &[], &[], &[]),
                job("test", &["build"], &[], &[]),
                job("deploy", &["test"], &[], &[]),
            ],
        )
        .await;
    let build = lease::claim(&w.t.db, r).await.expect("claim").expect("job");
    assert_eq!(build.name, "build");
    assert_eq!(build.pipeline.id, p);
    // Nothing else is ready yet.
    assert!(lease::claim(&w.t.db, r).await.expect("claim").is_none());
    let who = authenticate(&w.t.db, &rid(), &build.token.reveal())
        .await
        .expect("job token");
    assert_eq!(
        who,
        CiIdentity::Job {
            job_id: build.job_id,
            runner_id: r,
            project: w.web.project_id,
            sha: ObjectId::parse(&"b".repeat(40)).expect("sha"),
            state: CiJobTokenState::Live,
        }
    );
    assert_eq!(
        w.scalar(&format!(
            "SELECT status FROM forge.ci_pipelines WHERE id = {p}"
        ))
        .await,
        "running"
    );
    let done = lease::complete(
        &w.t.db,
        r,
        build.job_id,
        build.attempt,
        JobResult::Succeeded,
    )
    .await
    .expect("complete");
    assert_eq!(
        (done.status, done.pipeline_status.as_str()),
        ("succeeded", "running")
    );
    // Revoked in the same transaction as the job's end.
    match authenticate(&w.t.db, &rid(), &build.token.reveal())
        .await
        .expect("job token")
    {
        CiIdentity::Job { state, .. } => assert_eq!(state, CiJobTokenState::Revoked),
        other => panic!("{other:?}"),
    }
    let test = lease::claim(&w.t.db, r).await.expect("claim").expect("job");
    assert_eq!(test.name, "test");
    let done = lease::complete(&w.t.db, r, test.job_id, test.attempt, JobResult::Failed)
        .await
        .expect("complete");
    // The job waiting on the failure is cancelled, the pipeline failed.
    assert_eq!(
        (done.status, done.pipeline_status.as_str()),
        ("failed", "failed")
    );
    assert_eq!(
        w.scalar(&format!(
            "SELECT status FROM forge.ci_jobs WHERE pipeline_id = {p} AND name = 'deploy'"
        ))
        .await,
        "canceled"
    );
    // A second completion is a lost lease, never a second change.
    let err = lease::complete(&w.t.db, r, test.job_id, test.attempt, JobResult::Succeeded)
        .await
        .expect_err("done");
    assert_eq!(err.code(), ErrorCode::JobLeaseLost);
    w.t.drop_now().await;
}

#[tokio::test]
async fn expiry_is_decided_by_the_database_clock() {
    let w = world().await;
    let (r, _) = w.runner("r", &[]).await;
    w.pipeline('b', false, vec![job("build", &[], &[], &[])])
        .await;
    let c = lease::claim(&w.t.db, r).await.expect("claim").expect("job");
    w.exec(&format!(
        "UPDATE forge.ci_job_tokens SET created_at = now() - interval '2 hours', \
         expires_at = now() - interval '1 second' WHERE job_id = {}",
        c.job_id
    ))
    .await;
    match authenticate(&w.t.db, &rid(), &c.token.reveal())
        .await
        .expect("job token")
    {
        CiIdentity::Job { state, .. } => assert_eq!(state, CiJobTokenState::Expired),
        other => panic!("{other:?}"),
    }
    w.t.drop_now().await;
}

#[tokio::test]
async fn an_expired_lease_is_returned_to_the_queue_exactly_once() {
    let w = world().await;
    let (r, _) = w.runner("r", &[]).await;
    let (other, _) = w.runner("other", &[]).await;
    w.pipeline('b', false, vec![job("build", &[], &[], &[])])
        .await;
    let c = lease::claim(&w.t.db, r).await.expect("claim").expect("job");
    // Another runner never acts on it.
    for err in [
        lease::heartbeat(&w.t.db, other, c.job_id, c.attempt)
            .await
            .expect_err("other"),
        lease::complete(&w.t.db, other, c.job_id, c.attempt, JobResult::Succeeded)
            .await
            .expect_err("other"),
        lease::append_log(&w.t.db, other, c.job_id, c.attempt, 0, "x")
            .await
            .expect_err("other"),
    ] {
        assert_eq!(err.code(), ErrorCode::JobLeaseLost);
    }
    assert!(matches!(
        lease::heartbeat(&w.t.db, r, c.job_id, c.attempt)
            .await
            .expect("heartbeat"),
        Heartbeat::Running { .. }
    ));
    w.exec(&format!(
        "UPDATE forge.ci_jobs SET lease_until = now() - interval '1 second' WHERE id = {}",
        c.job_id
    ))
    .await;
    let err = lease::heartbeat(&w.t.db, r, c.job_id, c.attempt)
        .await
        .expect_err("expired");
    assert_eq!(err.code(), ErrorCode::JobLeaseLost);
    let (a, b) = tokio::join!(lease::reap(&w.t.db), lease::reap(&w.t.db));
    let reaped: Vec<_> = a
        .expect("reap")
        .reaped
        .into_iter()
        .chain(b.expect("reap").reaped)
        .collect();
    assert_eq!(reaped.len(), 1, "{reaped:?}");
    assert_eq!(reaped[0].outcome, "requeued");
    assert_eq!(reaped[0].runner_id, r);
    assert!(lease::reap(&w.t.db).await.expect("reap").reaped.is_empty());
    assert_eq!(
        w.scalar(&format!(
            "SELECT status || ':' || attempt || ':' || lease_losses FROM forge.ci_jobs WHERE id = {}",
            c.job_id
        ))
        .await,
        "pending:2:1"
    );
    assert_eq!(
        w.scalar(&format!(
            "SELECT revoked_reason FROM forge.ci_job_tokens WHERE job_id = {}",
            c.job_id
        ))
        .await,
        "lease_lost"
    );
    // The old attempt changes nothing, even when the same runner holds
    // the job again.
    let again = lease::claim(&w.t.db, r).await.expect("claim").expect("job");
    assert_eq!((again.job_id, again.attempt), (c.job_id, 2));
    let err = lease::complete(&w.t.db, r, c.job_id, c.attempt, JobResult::Succeeded)
        .await
        .expect_err("stale attempt");
    assert_eq!(err.code(), ErrorCode::JobLeaseLost);
    w.t.drop_now().await;
}

#[tokio::test]
async fn the_third_lost_lease_fails_the_job() {
    let w = world().await;
    let (r, _) = w.runner("r", &[]).await;
    w.pipeline('b', false, vec![job("build", &[], &[], &[])])
        .await;
    let c = lease::claim(&w.t.db, r).await.expect("claim").expect("job");
    w.exec(&format!(
        "UPDATE forge.ci_jobs SET lease_losses = 2, lease_until = now() - interval '1 second' \
         WHERE id = {}",
        c.job_id
    ))
    .await;
    let reaped = lease::reap(&w.t.db).await.expect("reap").reaped;
    assert_eq!(reaped.len(), 1);
    assert_eq!((reaped[0].outcome, reaped[0].lease_losses), ("failed", 3));
    assert_eq!(
        w.scalar(&format!(
            "SELECT j.status || ':' || j.error_code || ':' || p.status FROM forge.ci_jobs j \
             JOIN forge.ci_pipelines p ON p.id = j.pipeline_id WHERE j.id = {}",
            c.job_id
        ))
        .await,
        "failed:FG-CI-0004:failed"
    );
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_cancel_reaches_the_runner_through_its_heartbeat() {
    let w = world().await;
    let (r, _) = w.runner("r", &[]).await;
    let p = w
        .pipeline('b', false, vec![job("build", &[], &[], &[])])
        .await;
    let c = lease::claim(&w.t.db, r).await.expect("claim").expect("job");
    pipelines::cancel_pipeline_decided(
        &w.t.db,
        &ctx(Actor::User(w.alice), "req-cancel"),
        (w.web.project_id, p),
        w.alice,
        |_| Ok(()),
    )
    .await
    .expect("cancel");
    assert_eq!(
        lease::heartbeat(&w.t.db, r, c.job_id, c.attempt)
            .await
            .expect("heartbeat"),
        Heartbeat::Canceled
    );
    match authenticate(&w.t.db, &rid(), &c.token.reveal())
        .await
        .expect("job token")
    {
        CiIdentity::Job { state, .. } => assert_eq!(state, CiJobTokenState::Revoked),
        other => panic!("{other:?}"),
    }
    // The runner's completion acknowledges the cancel and releases it.
    let done = lease::complete(&w.t.db, r, c.job_id, c.attempt, JobResult::Succeeded)
        .await
        .expect("ack");
    assert_eq!(
        (done.status, done.pipeline_status.as_str()),
        ("canceled", "canceled")
    );
    assert_eq!(
        w.scalar(&format!(
            "SELECT coalesce(runner_id::text, 'none') FROM forge.ci_jobs WHERE id = {}",
            c.job_id
        ))
        .await,
        "none"
    );
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_log_past_the_job_limit_is_refused() {
    let w = world().await;
    let (r, _) = w.runner("r", &[]).await;
    w.pipeline('b', false, vec![job("build", &[], &[], &[])])
        .await;
    let c = lease::claim(&w.t.db, r).await.expect("claim").expect("job");
    let append = |seq: i64, body: &'static str| {
        lease::append_log(&w.t.db, r, c.job_id, c.attempt, seq, body)
    };
    assert_eq!(append(0, "hello\n").await.expect("log"), Appended::Stored);
    assert_eq!(
        append(0, "hello\n").await.expect("resend"),
        Appended::Duplicate
    );
    for (seq, body) in [(1, ""), (-1, "x"), (4096, "x")] {
        let err = append(seq, body).await.expect_err("invalid");
        assert_eq!(err.code(), ErrorCode::RequestInvalid);
    }
    // Fill the attempt to the byte limit (256 chunks of 64 KiB).
    w.exec(&format!(
        "INSERT INTO forge.ci_log_chunks (job_id, attempt, seq, project_id, body) \
         SELECT {}, {}, s, {}, repeat('x', 65536) FROM generate_series(1, 256) AS s",
        c.job_id,
        c.attempt,
        w.web.project_id.get()
    ))
    .await;
    let err = append(300, "more").await.expect_err("limit");
    assert_eq!(err.code(), ErrorCode::JobLogTooLarge);
    // Append only: the application role may not change a stored chunk.
    let mut app = sqlx::query("UPDATE forge.ci_log_chunks SET body = 'y' WHERE seq = 0")
        .execute(w.t.db.write_pool())
        .await
        .expect_err("append only")
        .to_string();
    app.make_ascii_lowercase();
    assert!(app.contains("permission denied"), "{app}");
    w.t.drop_now().await;
}

/// ADR-0029 obligation 5: only `StoredPipeline::secrets_allowed()` decides,
/// from the record made at creation; an unprotected push never gets the
/// secret names (C6 adds the values on the same rule).
#[tokio::test]
async fn a_job_of_an_unprotected_pipeline_is_never_secret_eligible() {
    let w = world().await;
    let (r, _) = w.runner("r", &[]).await;
    w.pipeline('b', false, vec![job("deploy", &[], &[], &["DEPLOY_KEY"])])
        .await;
    let c = lease::claim(&w.t.db, r).await.expect("claim").expect("job");
    assert!(!c.pipeline.secrets_allowed());
    assert!(!c.secrets_allowed);
    assert!(c.secret_names.is_empty());
    w.pipeline('c', true, vec![job("deploy", &[], &[], &["DEPLOY_KEY"])])
        .await;
    let c = lease::claim(&w.t.db, r).await.expect("claim").expect("job");
    assert!(c.secrets_allowed);
    assert_eq!(c.secret_names, vec!["DEPLOY_KEY".to_owned()]);
    w.t.drop_now().await;
}

/// ADR-0029 obligation 1: the project's deletion and its move revoke its
/// job tokens in their own transaction.
#[tokio::test]
async fn deleting_or_moving_the_project_revokes_its_job_tokens() {
    let w = world().await;
    let (r, _) = w.runner("r", &[]).await;
    w.pipeline(
        'b',
        false,
        vec![job("a", &[], &[], &[]), job("b", &[], &[], &[])],
    )
    .await;
    let first = lease::claim(&w.t.db, r).await.expect("claim").expect("job");
    let parent: i64 = {
        let mut su = w.t.superuser().await;
        sqlx::query_scalar(
            "SELECT parent_id FROM forge.namespaces WHERE kind = 'project' AND owner_id = $1",
        )
        .bind(w.web.project_id.get())
        .fetch_one(&mut su)
        .await
        .expect("parent")
    };
    projects::move_decided(
        &w.t.db,
        &ctx(Actor::User(w.alice), "req-move"),
        w.web.project_id,
        w.alice,
        &Destination {
            parent: Some(forge_core::ids::NamespaceId::new(parent)),
            segment: "web2".to_owned(),
        },
        MoveOptions {
            reclaim: false,
            rate_limit: RateLimit::Enforce,
        },
        |_| Ok(()),
    )
    .await
    .expect("rename");
    assert_eq!(
        w.scalar(&format!(
            "SELECT revoked_reason FROM forge.ci_job_tokens WHERE job_id = {}",
            first.job_id
        ))
        .await,
        "project_moved"
    );
    let second = lease::claim(&w.t.db, r).await.expect("claim").expect("job");
    projects::delete_decided(
        &w.t.db,
        &ctx(Actor::User(w.alice), "req-delete"),
        w.web.project_id,
        w.alice,
        std::time::Duration::from_secs(3600),
        |_| Ok(()),
    )
    .await
    .expect("delete");
    assert_eq!(
        w.scalar(&format!(
            "SELECT revoked_reason FROM forge.ci_job_tokens WHERE job_id = {}",
            second.job_id
        ))
        .await,
        "project_deleted"
    );
    w.t.drop_now().await;
}

#[tokio::test]
async fn pending_jobs_without_a_live_runner_are_no_runner_available() {
    let w = world().await;
    let healthy = lease::health(&w.t.db).await.expect("health");
    assert!(!healthy.no_runner_available());
    let p = w
        .pipeline('b', false, vec![job("build", &[], &["gpu"], &[])])
        .await;
    w.exec(&format!(
        "UPDATE forge.ci_jobs SET updated_at = now() - interval '11 minutes' WHERE pipeline_id = {p}"
    ))
    .await;
    let (_, _) = w.runner("cpu", &[]).await;
    let h = lease::health(&w.t.db).await.expect("health");
    assert!(h.no_runner_available());
    assert_eq!((h.waiting, h.enabled_runners, h.live_runners), (1, 1, 0));
    assert!(h.oldest_wait_s.unwrap_or(0) >= 600);
    w.t.drop_now().await;
}

/// Wait (bounded, 10 s) until `task` finished or a backend waits on a lock
/// in a statement containing `fragment` — whichever the code under test
/// does — so a race test drives one fixed interleaving either way.
async fn until_done_or_waiting<T>(t: &FreshDb, task: &tokio::task::JoinHandle<T>, fragment: &str) {
    let mut su = t.superuser().await;
    for _ in 0..500 {
        if task.is_finished() {
            return;
        }
        let waiting: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity WHERE wait_event_type = 'Lock' \
             AND datname = current_database() AND strpos(query, $1) > 0",
        )
        .bind(fragment)
        .fetch_one(&mut su)
        .await
        .expect("pg_stat_activity");
        if waiting > 0 {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("the claim neither finished nor waited on a lock within 10 s");
}

/// Review fix (ADR-0029 obligation 1): a project deletion or move that
/// revokes the project's job tokens while a claim is between its pick and
/// its token INSERT never leaves a live token behind. The deletion / move
/// is held open right after its revocation (the moment of the race); the
/// claim runs, then the deletion / move commits.
#[tokio::test]
async fn a_delete_or_move_racing_a_claim_never_leaves_a_live_token() {
    let w = world().await;
    let (r, _) = w.runner("r", &[]).await;
    let project = w.web.project_id.get();
    let alice = w.alice.get();
    let racing = [
        (
            "project_deleted",
            vec![
                format!("SELECT 1 FROM forge.projects WHERE id = {project} FOR UPDATE"),
                format!(
                    "UPDATE forge.projects SET deleted_at = now(), \
                     purge_after = now() + interval '1 day', deleted_by = {alice} \
                     WHERE id = {project}"
                ),
            ],
        ),
        (
            "project_moved",
            // A group's move locks the subtree's namespace rows, not the
            // project row.
            vec![format!(
                "SELECT 1 FROM forge.namespaces WHERE kind = 'project' AND owner_id = {project} \
                 FOR NO KEY UPDATE"
            )],
        ),
    ];
    // The move first: after the deletion the project is pending deletion
    // and no claim would pick it anyway.
    for (why, mut statements) in racing.into_iter().rev() {
        w.pipeline(
            if why == "project_moved" { 'b' } else { 'c' },
            false,
            vec![job("build", &[], &[], &[])],
        )
        .await;
        statements.push(format!(
            "UPDATE forge.ci_job_tokens SET revoked_at = now(), revoked_reason = '{why}' \
             WHERE project_id = {project} AND revoked_at IS NULL"
        ));
        let held = w.t.hold_audited(("project", project), &statements).await;
        let db = w.t.db.clone();
        let claim = tokio::spawn(async move { lease::claim(&db, r).await });
        until_done_or_waiting(&w.t, &claim, "ci_job_tokens").await;
        held.commit().await;
        let claimed = claim.await.expect("join").expect("claim");
        assert_eq!(
            w.scalar(&format!(
                "SELECT count(*)::text FROM forge.ci_job_tokens \
                 WHERE project_id = {project} AND revoked_at IS NULL"
            ))
            .await,
            "0",
            "{why}: a live job token survived the race (claimed {:?})",
            claimed.map(|c| c.job_id)
        );
    }
    w.t.drop_now().await;
}

/// Review fix: a retry starts the lease-loss count again, so a job that
/// failed at its third lost lease survives its first loss after a retry.
#[tokio::test]
async fn a_retried_job_survives_its_first_lost_lease() {
    let w = world().await;
    let (r, _) = w.runner("r", &[]).await;
    let p = w
        .pipeline('b', false, vec![job("build", &[], &[], &[])])
        .await;
    let c = lease::claim(&w.t.db, r).await.expect("claim").expect("job");
    let expire = format!(
        "UPDATE forge.ci_jobs SET lease_until = now() - interval '1 second' WHERE id = {}",
        c.job_id
    );
    w.exec(&format!(
        "UPDATE forge.ci_jobs SET lease_losses = 2 WHERE id = {}",
        c.job_id
    ))
    .await;
    w.exec(&expire).await;
    let reaped = lease::reap(&w.t.db).await.expect("reap").reaped;
    assert_eq!(reaped[0].outcome, "failed");
    pipelines::retry_pipeline_decided(
        &w.t.db,
        &ctx(Actor::User(w.alice), "req-retry"),
        (w.web.project_id, p),
        w.alice,
        |_| Ok(()),
    )
    .await
    .expect("retry");
    let again = lease::claim(&w.t.db, r).await.expect("claim").expect("job");
    assert_eq!(again.job_id, c.job_id);
    w.exec(&expire).await;
    let reaped = lease::reap(&w.t.db).await.expect("reap").reaped;
    assert_eq!((reaped[0].outcome, reaped[0].lease_losses), ("requeued", 1));
    w.t.drop_now().await;
}

/// Review fix: disabling a runner stops the job tokens it holds too.
#[tokio::test]
async fn disabling_a_runner_refuses_its_job_tokens() {
    let w = world().await;
    let (r, _) = w.runner("r", &[]).await;
    w.pipeline('b', false, vec![job("build", &[], &[], &[])])
        .await;
    let c = lease::claim(&w.t.db, r).await.expect("claim").expect("job");
    update(
        &w.t.db,
        &ctx(Actor::User(w.alice), "req-disable"),
        r,
        &RunnerUpdate {
            enabled: Some(false),
            ..RunnerUpdate::default()
        },
    )
    .await
    .expect("update")
    .expect("runner");
    let err = authenticate(&w.t.db, &rid(), &c.token.reveal())
        .await
        .expect_err("a disabled runner's job token");
    assert_reason(&err, ErrorCode::Unauthenticated, "runner_disabled");
    w.t.drop_now().await;
}

/// Review fix: a NUL character (PostgreSQL text cannot hold it) is a bad
/// request, not a database error.
#[tokio::test]
async fn a_log_chunk_with_a_nul_is_a_bad_request() {
    let w = world().await;
    let (r, _) = w.runner("r", &[]).await;
    w.pipeline('b', false, vec![job("build", &[], &[], &[])])
        .await;
    let c = lease::claim(&w.t.db, r).await.expect("claim").expect("job");
    let err = lease::append_log(&w.t.db, r, c.job_id, c.attempt, 0, "a\0b")
        .await
        .expect_err("nul");
    assert_reason(&err, ErrorCode::RequestInvalid, "log_chunk_invalid");
    w.t.drop_now().await;
}

/// Review fix: one job the reaper cannot take back does not stop the pass;
/// the others are still returned to the queue.
#[tokio::test]
async fn one_failing_job_does_not_stop_the_reaper_pass() {
    let w = world().await;
    let (r, _) = w.runner("r", &[]).await;
    w.pipeline(
        'b',
        false,
        vec![job("a", &[], &[], &[]), job("b", &[], &[], &[])],
    )
    .await;
    let first = lease::claim(&w.t.db, r).await.expect("claim").expect("job");
    let second = lease::claim(&w.t.db, r).await.expect("claim").expect("job");
    // The first is reaped first (the older lease end) and its update fails.
    w.exec(&format!(
        "UPDATE forge.ci_jobs SET lease_until = now() - interval '2 seconds' WHERE id = {}; \
         UPDATE forge.ci_jobs SET lease_until = now() - interval '1 second' WHERE id = {}; \
         CREATE FUNCTION forge.test_refuse_reap() RETURNS trigger LANGUAGE plpgsql AS \
           $fn$BEGIN RAISE EXCEPTION 'test: refused'; END$fn$; \
         CREATE TRIGGER test_refuse_reap BEFORE UPDATE ON forge.ci_jobs FOR EACH ROW \
           WHEN (OLD.id = {}) EXECUTE FUNCTION forge.test_refuse_reap();",
        first.job_id, second.job_id, first.job_id
    ))
    .await;
    let pass = lease::reap(&w.t.db).await;
    assert_eq!(
        w.scalar(&format!(
            "SELECT status || ':' || attempt FROM forge.ci_jobs WHERE id = {}",
            second.job_id
        ))
        .await,
        "pending:2",
        "the pass stopped at the failing job"
    );
    let pass = pass.expect("only the scan fails a pass");
    assert_eq!(
        pass.reaped.iter().map(|r| r.job_id).collect::<Vec<_>>(),
        vec![second.job_id]
    );
    assert_eq!(pass.failed.len(), 1);
    assert_eq!(pass.failed[0].0, first.job_id);
    assert_eq!(pass.failed[0].1.code(), ErrorCode::DbQueryFailed);
    // The failed job's transaction was rolled back: still held, retried by
    // the next pass.
    assert_eq!(
        w.scalar(&format!(
            "SELECT status || ':' || attempt FROM forge.ci_jobs WHERE id = {}",
            first.job_id
        ))
        .await,
        "running:1"
    );
    w.t.drop_now().await;
}

/// M4 C4: the job fetch route's facts — the pipeline, its project and SHA,
/// the project's repository — in one read; no job, no row.
#[tokio::test]
async fn a_jobs_copy_names_its_pipelines_project_sha_and_repository() {
    let w = world().await;
    let (r, _) = w.runner("r", &[]).await;
    let p = w
        .pipeline('c', false, vec![job("build", &[], &[], &[])])
        .await;
    let claimed = lease::claim(&w.t.db, r).await.expect("claim").expect("job");
    let copy = lease::job_copy(&w.t.db, claimed.job_id)
        .await
        .expect("read")
        .expect("a job");
    assert_eq!(copy.job_id, claimed.job_id);
    assert_eq!(copy.pipeline_id, p);
    assert_eq!(copy.project, w.web.project_id);
    assert_eq!(copy.sha.as_str(), "c".repeat(40));
    let repo: i64 = w
        .scalar(&format!(
            "SELECT id::text FROM forge.repositories WHERE project_id = {}",
            w.web.project_id.get()
        ))
        .await
        .parse()
        .expect("id");
    assert_eq!(copy.repository.id.get(), repo);
    assert!(lease::job_copy(&w.t.db, claimed.job_id + 1000)
        .await
        .expect("read")
        .is_none());
    w.t.drop_now().await;
}

/// Plan 18 S2: every change that makes a CI job claimable tells the
/// runners waiting on the hub (`NOTIFY forge_jobs, 'ci.job'`), so a long
/// poll looks long before its safety-net period; a claim tells no one.
#[tokio::test]
async fn a_ci_job_made_pending_wakes_the_waiting_runner() {
    use std::time::{Duration, Instant};

    use crate::wake::{Channel, WakeHub, Woken};

    const SAFETY: Duration = Duration::from_secs(10);
    const WELL_BEFORE: Duration = Duration::from_secs(3);
    const QUIET: Duration = Duration::from_millis(300);
    const POLL: Duration = Duration::from_millis(50);
    /// How long a wait of up to the safety period took.
    async fn woken_after(idle: &mut JobWait) -> Duration {
        let started = Instant::now();
        idle.wait(SAFETY).await;
        started.elapsed()
    }

    let w = world().await;
    let (r, _) = w.runner("r", &[]).await;
    let hub = WakeHub::spawn(&w.t.db, &[Channel::Jobs]).expect("hub");
    let mut connected = hub.subscribe_unseen(Channel::Jobs);
    assert_eq!(
        connected.wait(Duration::from_secs(30)).await,
        Woken::Reconnected
    );
    // clone: the handle a server process attaches its hub to
    let db = w.t.db.clone().with_wake(hub.clone(), SAFETY);
    let mut idle = JobWait::new(&db, POLL);

    // A planned pipeline.
    let p = w
        .pipeline(
            'b',
            false,
            vec![
                job("build", &[], &[], &[]),
                job("test", &["build"], &[], &[]),
            ],
        )
        .await;
    assert!(woken_after(&mut idle).await < WELL_BEFORE);
    // The claim makes nothing claimable: the wait runs its whole period.
    let build = lease::claim(&db, r).await.expect("claim").expect("job");
    let started = Instant::now();
    idle.wait(QUIET).await;
    assert!(started.elapsed() >= QUIET);
    // A job whose needs succeeded.
    lease::complete(&db, r, build.job_id, build.attempt, JobResult::Succeeded)
        .await
        .expect("complete");
    assert!(woken_after(&mut idle).await < WELL_BEFORE);
    // A lease the reaper gives back.
    let test = lease::claim(&db, r).await.expect("claim").expect("job");
    assert_eq!((test.name.as_str(), test.pipeline.id), ("test", p));
    idle.wait(QUIET).await;
    w.exec(&format!(
        "UPDATE forge.ci_jobs SET lease_until = now() - interval '1 second' WHERE id = {}",
        test.job_id
    ))
    .await;
    lease::reap(&db).await.expect("reap");
    assert!(woken_after(&mut idle).await < WELL_BEFORE);
    // A job of the queue (another payload of the channel) is not for it.
    w.exec("SELECT pg_catalog.pg_notify('forge_jobs', 'test.a')")
        .await;
    let started = Instant::now();
    idle.wait(QUIET).await;
    assert!(started.elapsed() >= QUIET);

    // Without a hub the long poll is the plain poll.
    let mut plain = JobWait::new(&w.t.db, POLL);
    let started = Instant::now();
    plain.wait(SAFETY).await;
    assert!((POLL..WELL_BEFORE).contains(&started.elapsed()));
    hub.stop(Duration::from_secs(30)).await.expect("stop");
    w.t.drop_now().await;
}

/// The `FG-DB-0024` events of a capture.
fn missed_wakes(logs: &crate::jobs::runner_stop_tests::Captured) -> Vec<serde_json::Value> {
    logs.failures()
        .into_iter()
        .filter(|e| e["fields"]["error_code"] == "FG-DB-0024")
        .collect()
}

/// Wait until one wait of `idle` ran its whole safety-net period (a wake
/// still on its way ends an earlier one).
async fn a_whole_safety_net(idle: &mut JobWait, safety: std::time::Duration) {
    for _ in 0..20 {
        let started = std::time::Instant::now();
        idle.wait(safety).await;
        if started.elapsed() >= safety {
            return;
        }
    }
    panic!("no wait ran the whole safety-net period");
}

/// Review fix 2: a CI job found by a safety-net look is a missed wake
/// (`FG-DB-0024`) only when it had been claimable by this runner for the
/// whole safety period — pending that long (the database's clock) and the
/// runner unchanged that long. Then it is logged once, with the age.
#[tokio::test]
async fn a_safety_net_find_of_a_ci_job_is_a_miss_only_after_the_safety_period() {
    use std::time::Duration;

    use crate::jobs::runner_stop_tests::Captured;
    use crate::wake::{Channel, WakeHub, Woken};

    const SAFETY: Duration = Duration::from_secs(2);
    const POLL: Duration = Duration::from_millis(50);
    const LONG_AGO: &str = "now() - interval '60 seconds'";

    let logs = Captured::default();
    let _guard = logs.install();
    let w = world().await;
    let (r, _) = w.runner("r", &[]).await;
    let hub = WakeHub::spawn(&w.t.db, &[Channel::Jobs]).expect("hub");
    let mut connected = hub.subscribe_unseen(Channel::Jobs);
    assert_eq!(
        connected.wait(Duration::from_secs(30)).await,
        Woken::Reconnected
    );
    // clone: the handle a server process attaches its hub to
    let db = w.t.db.clone().with_wake(hub.clone(), SAFETY);
    let mut idle = JobWait::new(&db, POLL);
    let request_id = rid();
    let runner_changed =
        |at: &str| format!("UPDATE forge.ci_runners SET updated_at = {at} WHERE id = {r}");
    let job_pending_since = |pipeline: i64, at: &str| {
        format!("UPDATE forge.ci_jobs SET updated_at = {at} WHERE pipeline_id = {pipeline}")
    };

    // Pending since just now, the runner unchanged for long: found early.
    let p = w
        .pipeline('b', false, vec![job("build", &[], &[], &[])])
        .await;
    a_whole_safety_net(&mut idle, SAFETY).await;
    w.exec(&runner_changed(LONG_AGO)).await;
    w.exec(&job_pending_since(p, "now()")).await;
    let young = lease::claim(&db, r).await.expect("claim").expect("job");
    assert!(young.claimable_for < SAFETY, "{:?}", young.claimable_for);
    idle.claimed(r, &young, &request_id);
    assert_eq!(missed_wakes(&logs), Vec::<serde_json::Value>::new());

    // Pending for long, the runner changed just now (re-tagged, resumed).
    let p = w
        .pipeline('c', false, vec![job("build", &[], &[], &[])])
        .await;
    a_whole_safety_net(&mut idle, SAFETY).await;
    w.exec(&job_pending_since(p, LONG_AGO)).await;
    w.exec(&runner_changed("now()")).await;
    let retagged = lease::claim(&db, r).await.expect("claim").expect("job");
    assert!(
        retagged.claimable_for < SAFETY,
        "{:?}",
        retagged.claimable_for
    );
    idle.claimed(r, &retagged, &request_id);
    assert_eq!(missed_wakes(&logs), Vec::<serde_json::Value>::new());

    // Claimable for longer than the safety period: no wake announced it.
    let p = w
        .pipeline('d', false, vec![job("build", &[], &[], &[])])
        .await;
    a_whole_safety_net(&mut idle, SAFETY).await;
    w.exec(&job_pending_since(p, LONG_AGO)).await;
    w.exec(&runner_changed(LONG_AGO)).await;
    let old = lease::claim(&db, r).await.expect("claim").expect("job");
    assert!(
        old.claimable_for >= Duration::from_secs(60),
        "{:?}",
        old.claimable_for
    );
    idle.claimed(r, &old, &request_id);
    let missed = missed_wakes(&logs);
    assert_eq!(missed.len(), 1, "{missed:?}");
    assert_eq!(missed[0]["level"], "WARN");
    let context = missed[0]["fields"]["context"].as_str().unwrap_or_default();
    for part in [
        format!("oldest_ready_ms={}", old.claimable_for.as_millis()),
        format!("safety_poll_ms={}", SAFETY.as_millis()),
        "kind=ci.job".to_owned(),
        format!("job_id={}", old.job_id),
        format!("runner_id={r}"),
        "suppressed=0".to_owned(),
    ] {
        assert!(context.contains(&part), "{part} in {context}");
    }
    // The same claim without a safety-net wait before it says nothing
    // more (the flag is taken), and a job found without a hub never does.
    idle.claimed(r, &old, &request_id);
    let mut plain = JobWait::new(&w.t.db, POLL);
    plain.wait(SAFETY).await;
    plain.claimed(r, &old, &request_id);
    assert_eq!(missed_wakes(&logs).len(), 1);

    hub.stop(Duration::from_secs(30)).await.expect("stop");
    w.t.drop_now().await;
}

/// Review fix 7: the end of a job wakes the waiting runners only when
/// something can now be claimed — a job whose needs succeeded (above) or
/// a freed GPU place; the end of a job nothing waited for wakes no one.
/// A runner re-tagged or resumed wakes them too.
#[tokio::test]
async fn the_end_of_a_job_wakes_runners_only_when_a_job_can_be_claimed() {
    use std::time::{Duration, Instant};

    use crate::wake::{Channel, WakeHub, Woken};

    const SAFETY: Duration = Duration::from_secs(10);
    const WELL_BEFORE: Duration = Duration::from_secs(3);
    const QUIET: Duration = Duration::from_millis(300);
    const POLL: Duration = Duration::from_millis(50);
    /// How long a wait of up to `max` took.
    async fn waited(idle: &mut JobWait, max: Duration) -> Duration {
        let started = Instant::now();
        idle.wait(max).await;
        started.elapsed()
    }

    let w = world().await;
    let policy = gpu_policy(w.web.project_id.get());
    let (r, _) = w.runner("r", &[]).await;
    let (gpu, _) = w.runner("gpu", &["gpu-eval"]).await;
    let hub = WakeHub::spawn(&w.t.db, &[Channel::Jobs]).expect("hub");
    let mut connected = hub.subscribe_unseen(Channel::Jobs);
    assert_eq!(
        connected.wait(Duration::from_secs(30)).await,
        Woken::Reconnected
    );
    // clone: the handle a server process attaches its hub to
    let db = w.t.db.clone().with_wake(hub.clone(), SAFETY);
    let mut idle = JobWait::new(&db, POLL);

    // A job nothing waits for: neither its end …
    w.pipeline('b', false, vec![job("solo", &[], &[], &[])])
        .await;
    assert!(waited(&mut idle, SAFETY).await < WELL_BEFORE);
    let solo = lease::claim(&db, r).await.expect("claim").expect("job");
    lease::complete(&db, r, solo.job_id, solo.attempt, JobResult::Succeeded)
        .await
        .expect("complete");
    assert!(waited(&mut idle, QUIET).await >= QUIET);
    // … nor its failure by the reaper wakes anyone.
    w.pipeline('c', false, vec![job("lost", &[], &[], &[])])
        .await;
    assert!(waited(&mut idle, SAFETY).await < WELL_BEFORE);
    let lost = lease::claim(&db, r).await.expect("claim").expect("job");
    w.exec(&format!(
        "UPDATE forge.ci_jobs SET lease_losses = 2, lease_until = now() - interval '1 second' \
         WHERE id = {}",
        lost.job_id
    ))
    .await;
    let reaped = lease::reap(&db).await.expect("reap").reaped;
    assert_eq!(reaped[0].outcome, "failed");
    assert!(waited(&mut idle, QUIET).await >= QUIET);

    // A `gpu-eval` job that ends frees its GPU place: completed …
    w.pipeline('d', false, vec![job("bench", &[], &["gpu-eval"], &[])])
        .await;
    assert!(waited(&mut idle, SAFETY).await < WELL_BEFORE);
    let bench = lease::claim_with(&db, gpu, &policy)
        .await
        .expect("claim")
        .expect("the bench");
    assert!(waited(&mut idle, QUIET).await >= QUIET);
    lease::complete(&db, gpu, bench.job_id, bench.attempt, JobResult::Succeeded)
        .await
        .expect("complete");
    assert!(waited(&mut idle, SAFETY).await < WELL_BEFORE);
    // … or failed by the reaper.
    w.pipeline('e', false, vec![job("bench", &[], &["gpu-eval"], &[])])
        .await;
    assert!(waited(&mut idle, SAFETY).await < WELL_BEFORE);
    let bench = lease::claim_with(&db, gpu, &policy)
        .await
        .expect("claim")
        .expect("the bench");
    assert!(waited(&mut idle, QUIET).await >= QUIET);
    w.exec(&format!(
        "UPDATE forge.ci_jobs SET lease_losses = 2, lease_until = now() - interval '1 second' \
         WHERE id = {}",
        bench.job_id
    ))
    .await;
    let reaped = lease::reap(&db).await.expect("reap").reaped;
    assert_eq!(reaped[0].outcome, "failed");
    assert!(waited(&mut idle, SAFETY).await < WELL_BEFORE);

    // A runner re-tagged or resumed: jobs may be claimable by it now. A
    // change of its description only, or a pause, is not for the waiters.
    let change =
        |enabled: Option<bool>, tags: Option<&[&str]>, description: Option<&str>| RunnerUpdate {
            enabled,
            tags: tags.map(|t| t.iter().map(|t| (*t).to_owned()).collect()),
            description: description.map(str::to_owned),
        };
    let admin = ctx(Actor::User(w.alice), "req-runner-change");
    for (step, wakes) in [
        (change(None, Some(&["linux"]), None), true),
        (change(None, None, Some("the build host")), false),
        (change(Some(false), None, None), false),
        (change(Some(true), None, None), true),
    ] {
        update(&db, &admin, r, &step)
            .await
            .expect("update")
            .expect("runner");
        if wakes {
            assert!(waited(&mut idle, SAFETY).await < WELL_BEFORE, "{step:?}");
        } else {
            assert!(waited(&mut idle, QUIET).await >= QUIET, "{step:?}");
        }
    }

    hub.stop(Duration::from_secs(30)).await.expect("stop");
    w.t.drop_now().await;
}
