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
        .into_iter()
        .chain(b.expect("reap"))
        .collect();
    assert_eq!(reaped.len(), 1, "{reaped:?}");
    assert_eq!(reaped[0].outcome, "requeued");
    assert_eq!(reaped[0].runner_id, r);
    assert!(lease::reap(&w.t.db).await.expect("reap").is_empty());
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
    let reaped = lease::reap(&w.t.db).await.expect("reap");
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

/// ADR-0029 obligation 4 / plan 14 C5: before C5 decides the CI outputs
/// on their marks (FG-AUTH-0022 for an agent audience), **no code reads a
/// job log back**: the read pool may not select `body`, and outside tests
/// the only statements on `ci_log_chunks` are the runner's append (with
/// its size count) and the project purge. A read path added before the
/// C5 decision turns this red.
#[tokio::test]
async fn ci_logs_have_no_read_path_before_c5() {
    let t = FreshDb::migrated().await;
    let err = sqlx::query("SELECT body FROM forge.ci_log_chunks LIMIT 1")
        .execute(t.db.pool().executor())
        .await
        .expect_err("the read pool never reads a log body")
        .to_string()
        .to_ascii_lowercase();
    assert!(err.contains("permission denied"), "{err}");
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates")
        .to_owned();
    let mut offenders = Vec::new();
    let mut stack = vec![root];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path.to_string_lossy().replace('\\', "/");
            if path.is_dir() {
                if !name.ends_with("/target") && !name.ends_with("/tests") {
                    stack.push(path);
                }
                continue;
            }
            if !name.ends_with(".rs") || name.contains("test") {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            let code: String = text
                .lines()
                .filter(|l| !l.trim_start().starts_with("//"))
                .collect::<Vec<_>>()
                .join(
                    "
",
                );
            if !code.contains("ci_log_chunks") {
                continue;
            }
            let allowed = name.ends_with("forge-db/src/ci_runners/lease.rs")
                || name.ends_with("forge-db/src/projects/purge.rs")
                || name.ends_with("forge-db/src/bootstrap/schema.rs");
            if !allowed {
                offenders.push(name);
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "a CI log read path before C5 (ADR-0029 obligation 4): {offenders:?}"
    );
    // The append's only read is the size count, never the body's text.
    let lease = include_str!("lease.rs");
    assert!(!lease.contains("SELECT body"));
    t.drop_now().await;
}
