//! The CI outputs against a migrated database (M4 C5): the log page and
//! its masking context, the decision before any body is read, the
//! fingerprint sweep (exactly once per attempt, folded per project and
//! fingerprint), the diagnosis, the `ci/pipeline` check through the
//! pipeline's transitions, and the known fix of a merge request (P-7).

use forge_core::checks::{CheckConclusion, CheckStatus, CI_PIPELINE_CHECK};
use forge_core::{CheckName, MergeRequestId, ObjectId, RefName, RequestId, UserId};
use forge_errors::{ErrorCode, ForgeError};

use super::*;
use crate::change::{Actor, ChangeContext};
use crate::ci_runners::lease::{self, JobResult};
use crate::ci_runners::{register, NewRunner};
use crate::pipelines::{self, NewCiJob, NewPipeline, PipelineRequest, Planned};
use crate::projects::{self, CreatedProject, NewProject};
use crate::review::{open_mr_decided, NewMergeRequest};
use crate::test_support::*;
use crate::users::{self, NewUser};

fn ctx(actor: Actor, request: &str) -> ChangeContext {
    ChangeContext::new(actor, RequestId::parse(request).expect("request id"))
}

fn system() -> ChangeContext {
    let mut c = ctx(Actor::System, "req-ci-checks");
    c.actor_label = Some("forge-ci".to_owned());
    c
}

struct World {
    t: FreshDb,
    alice: UserId,
    web: CreatedProject,
    runner: i64,
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
    let runner = register(
        &t.db,
        &ctx(Actor::User(alice.user_id), "req-reg"),
        &NewRunner {
            name: "linux-1".to_owned(),
            description: String::new(),
            tags: Vec::new(),
        },
    )
    .await
    .expect("runner")
    .runner
    .id;
    World {
        t,
        alice: alice.user_id,
        web,
        runner,
    }
}

fn job(name: &str) -> NewCiJob {
    NewCiJob {
        name: name.to_owned(),
        stage: "test".to_owned(),
        wave: 0,
        needs: Vec::new(),
        image: format!("registry.internal/rust@sha256:{}", "a".repeat(64)),
        timeout_s: 600,
        tags: Vec::new(),
        secrets: Vec::new(),
    }
}

fn sha(c: char) -> ObjectId {
    ObjectId::parse(&c.to_string().repeat(40)).expect("sha")
}

impl World {
    async fn pipeline_of(
        &self,
        c: char,
        source: Source,
        merge_request: Option<MergeRequestId>,
        jobs: &[&str],
        mark: CopyMark,
    ) -> i64 {
        let new = NewPipeline {
            request: PipelineRequest {
                project: self.web.project_id,
                git_ref: RefName::parse("refs/heads/main").expect("ref"),
                sha: sha(c),
                source,
                merge_request,
                triggered_by: None,
            },
            request_id: "req-plan".to_owned(),
            definition_hash: Some(pipelines::definition_hash(b"version: 1")),
            protected: false,
            copy_mark: mark,
            planned: Planned::Jobs(jobs.iter().map(|n| job(n)).collect()),
        };
        pipelines::create(&self.t.db, &new)
            .await
            .expect("pipeline")
            .pipeline_id
    }

    async fn pipeline(&self, c: char, jobs: &[&str]) -> i64 {
        self.pipeline_of(c, Source::Push, None, jobs, CopyMark::Open)
            .await
    }

    /// Claim the next job, append `log` in `chunks`, finish it.
    async fn run(&self, chunks: &[&str], result: JobResult) -> (i64, i32) {
        let claimed = lease::claim(&self.t.db, self.runner)
            .await
            .expect("claim")
            .expect("a job");
        let (job, attempt) = (claimed.job_id, claimed.attempt);
        for (i, body) in chunks.iter().enumerate() {
            lease::append_log(
                &self.t.db,
                self.runner,
                job,
                attempt,
                i64::try_from(i).expect("seq"),
                body,
            )
            .await
            .expect("append");
        }
        lease::complete(&self.t.db, self.runner, job, attempt, result)
            .await
            .expect("complete");
        (job, attempt)
    }

    async fn scalar(&self, sql: &str) -> String {
        let mut su = self.t.superuser().await;
        sqlx::query_scalar::<_, String>(sql)
            .fetch_one(&mut su)
            .await
            .expect("scalar")
    }

    async fn sweep_checks(&self) -> Vec<PipelineCheck> {
        let mut out = Vec::new();
        for id in pipelines_due_for_check(&self.t.db, 100).await.expect("due") {
            out.push(
                record_pipeline_check(&self.t.db, &system(), id, build)
                    .await
                    .expect("record"),
            );
        }
        out
    }

    async fn check_of(&self, c: char) -> String {
        self.scalar(&format!(
            "SELECT status || ':' || coalesce(conclusion, '-') || ':' || \
               coalesce(reported_by::text, 'system') FROM forge.check_runs \
             WHERE project_id = {} AND head_sha = '{}' AND name = 'ci/pipeline'",
            self.web.project_id.get(),
            sha(c).as_str()
        ))
        .await
    }
}

/// The mapping of forge-pipelines, reduced for the store's tests.
fn build(f: &PipelineCheckFacts) -> Result<NewCheckRun, ForgeError> {
    let (status, conclusion) = match f.status {
        PipelineStatus::Pending => (CheckStatus::Queued, None),
        PipelineStatus::Running => (CheckStatus::InProgress, None),
        PipelineStatus::Succeeded | PipelineStatus::Skipped => {
            (CheckStatus::Completed, Some(CheckConclusion::Success))
        }
        PipelineStatus::Canceled => (CheckStatus::Completed, Some(CheckConclusion::Neutral)),
        PipelineStatus::Failed | PipelineStatus::Invalid => {
            (CheckStatus::Completed, Some(CheckConclusion::Failure))
        }
        PipelineStatus::Error => (CheckStatus::Completed, Some(CheckConclusion::Error)),
    };
    Ok(NewCheckRun {
        name: CheckName::parse(CI_PIPELINE_CHECK).expect("name"),
        status,
        conclusion,
        summary: format!("pipeline {} {}", f.pipeline_id, f.status.as_str()),
        details_url: None,
        annotations: Vec::new(),
    })
}

/// A digest that keys on the log's last line with digits removed.
fn digest(log: &FailureLog) -> FailureDigest {
    let last = log
        .tail
        .lines()
        .rfind(|l| !l.trim().is_empty())
        .unwrap_or("no log");
    let key: String = last.chars().filter(|c| !c.is_ascii_digit()).collect();
    let n = log.tail.lines().count();
    let line = i32::try_from(log.first_line + i64::try_from(n).expect("n") - 1).expect("line");
    FailureDigest {
        fingerprint: format!(
            "fp_{:016x}",
            key.len() as u64 * 7919 + u64::from(key.as_bytes()[0])
        ),
        error_codes: if last.contains("FG-") {
            vec!["FG-DB-0003".to_owned()]
        } else {
            Vec::new()
        },
        excerpt: last.to_owned(),
        lines: (n > 0).then_some((line, line)),
    }
}

fn allow(_: &LogTarget) -> Result<(), ForgeError> {
    Ok(())
}

#[tokio::test]
async fn a_log_page_is_whole_chunks_with_masking_context_and_a_cursor() {
    let w = world().await;
    let p = w.pipeline('a', &["test"]).await;
    let big = "x".repeat(600);
    let (job, attempt) = w
        .run(&["line 1\n", &big, "tail\n", "end\n"], JobResult::Succeeded)
        .await;
    let project = w.web.project_id;
    let (page, ()) = job_log_decided(&w.t.db, project, (p, job), None, 1, allow)
        .await
        .expect("read")
        .expect("found");
    assert_eq!(page.attempt, attempt);
    assert_eq!(page.target.job_name, "test");
    assert_eq!(page.target.copy_mark, CopyMark::Open);
    assert_eq!(
        page.chunks.iter().map(|c| c.0).collect::<Vec<_>>(),
        [1, 2, 3]
    );
    assert_eq!(page.before, "line 1\n");
    assert_eq!(page.after, "");
    assert_eq!(page.next_seq, None);
    // The context before the page is bounded and ends where the page starts.
    let (page, ()) = job_log_decided(&w.t.db, project, (p, job), None, 2, allow)
        .await
        .expect("read")
        .expect("found");
    assert!(page.before.len() <= 512 && page.before.ends_with('x'));
    // Another pipeline, another project's view, an attempt not run: not found.
    for (pipeline, attempt) in [(p + 1, None), (p, Some(attempt + 1)), (p, Some(0))] {
        let got = job_log_decided(&w.t.db, project, (pipeline, job), attempt, 0, allow)
            .await
            .expect("read");
        assert!(got.is_none(), "{pipeline} {attempt:?}");
    }
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_refused_decision_reads_nothing_and_answers_its_error() {
    let w = world().await;
    let p = w
        .pipeline_of('a', Source::Push, None, &["test"], CopyMark::Undecided)
        .await;
    let (job, _) = w.run(&["secret output\n"], JobResult::Failed).await;
    let mut seen = None;
    let err = job_log_decided(&w.t.db, w.web.project_id, (p, job), None, 0, |t| {
        seen = Some(t.copy_mark);
        Err::<(), _>(ForgeError::new(ErrorCode::RepositoryCopyWithheld))
    })
    .await
    .expect_err("refused");
    assert_eq!(err.code(), ErrorCode::RepositoryCopyWithheld);
    assert_eq!(seen, Some(CopyMark::Undecided));
    let err = diagnosis_decided(&w.t.db, w.web.project_id, p, |t| {
        assert_eq!(t.copy_mark, CopyMark::Undecided);
        Err::<(), _>(ForgeError::new(ErrorCode::RepositoryCopyWithheld))
    })
    .await
    .expect_err("refused");
    assert_eq!(err.code(), ErrorCode::RepositoryCopyWithheld);
    w.t.drop_now().await;
}

/// P-6: one failure seen again folds into one fingerprint; each attempt is
/// recorded exactly once; a retry's new attempt is a new occurrence.
#[tokio::test]
async fn fingerprints_fold_the_same_failure_and_record_each_attempt_once() {
    let w = world().await;
    let p1 = w.pipeline('a', &["test"]).await;
    let (j1, _) = w
        .run(
            &["build ok\n", "test x FAILED after 10s\n"],
            JobResult::Failed,
        )
        .await;
    let p2 = w.pipeline('b', &["test"]).await;
    let (j2, _) = w
        .run(&["test x FAILED after 12s\n"], JobResult::Failed)
        .await;
    let done = fingerprint_failed_jobs(&w.t.db, 10, digest)
        .await
        .expect("sweep");
    assert_eq!(done.len(), 2);
    assert_eq!(done[0].fingerprint, done[1].fingerprint);
    assert_eq!(
        (done[0].occurrences, done[1].occurrences),
        (1, 2),
        "{done:?}"
    );
    assert_eq!((done[0].job_id, done[1].job_id), (j1, j2));
    assert_eq!((done[0].pipeline_id, done[1].pipeline_id), (p1, p2));
    // Nothing left: a second pass records nothing.
    assert!(fingerprint_failed_jobs(&w.t.db, 10, digest)
        .await
        .expect("again")
        .is_empty());
    assert_eq!(
        w.scalar(
            "SELECT count(*)::text || ':' || max(occurrences)::text || ':' || \
               max(last_job_id)::text FROM forge.failure_fingerprints"
        )
        .await,
        format!("1:2:{j2}")
    );
    // A retry fails again: a new attempt, a new occurrence.
    pipelines::retry_pipeline_decided(
        &w.t.db,
        &ctx(Actor::User(w.alice), "req-retry"),
        (w.web.project_id, p2),
        w.alice,
        |_| Ok(()),
    )
    .await
    .expect("retry");
    let (_, attempt) = w
        .run(&["test x FAILED after 9s\n"], JobResult::Failed)
        .await;
    assert_eq!(attempt, 2);
    let again = fingerprint_failed_jobs(&w.t.db, 10, digest)
        .await
        .expect("retry sweep");
    assert_eq!(again.len(), 1);
    assert_eq!((again[0].attempt, again[0].occurrences), (2, 3));
    assert_eq!(
        w.scalar("SELECT count(*)::text FROM forge.ci_job_failures")
            .await,
        "3"
    );
    // The diagnosis of the retried pipeline names the current attempt.
    let (target, failures, ()) = diagnosis_decided(&w.t.db, w.web.project_id, p2, |_| Ok(()))
        .await
        .expect("diagnosis")
        .expect("found");
    assert_eq!(target.status, PipelineStatus::Failed);
    assert_eq!(failures.len(), 1);
    let found = failures[0].found.as_ref().expect("fingerprinted");
    assert_eq!(failures[0].attempt, 2);
    assert_eq!(found.occurrences, 3);
    assert_eq!(found.excerpt, "test x FAILED after 9s");
    assert_eq!(found.lines, Some((1, 1)));
    assert!(found.known_fix.is_none());
    w.t.drop_now().await;
}

/// A digest out of shape is a caller bug: refused, nothing stored, the job
/// stays to fingerprint.
#[tokio::test]
async fn a_digest_out_of_shape_stores_nothing() {
    let w = world().await;
    w.pipeline('a', &["test"]).await;
    w.run(&["boom\n"], JobResult::Failed).await;
    let err = fingerprint_failed_jobs(&w.t.db, 10, |l| FailureDigest {
        fingerprint: "fp_NOTHEX".to_owned(),
        ..digest(l)
    })
    .await
    .expect_err("bad digest");
    assert_eq!(err.code(), ErrorCode::Internal);
    assert_eq!(
        w.scalar("SELECT count(*)::text FROM forge.failure_fingerprints")
            .await,
        "0"
    );
    assert_eq!(
        fingerprint_failed_jobs(&w.t.db, 10, digest)
            .await
            .expect("sweep")
            .len(),
        1
    );
    w.t.drop_now().await;
}

/// ADR-0029 decision 7: `ci/pipeline` follows the pipeline — queued,
/// in progress, then its conclusion — written by the system (no
/// reporter), once per status.
#[tokio::test]
async fn the_pipeline_check_follows_the_pipeline_through_its_transitions() {
    let w = world().await;
    let p = w.pipeline('a', &["build", "test"]).await;
    let first = w.sweep_checks().await;
    assert!(matches!(first.as_slice(), [PipelineCheck::Written { .. }]));
    assert_eq!(w.check_of('a').await, "queued:-:system");
    assert!(w.sweep_checks().await.is_empty(), "once per status");
    w.run(&["ok\n"], JobResult::Succeeded).await;
    w.sweep_checks().await;
    assert_eq!(w.check_of('a').await, "in_progress:-:system");
    w.run(&["boom\n"], JobResult::Failed).await;
    w.sweep_checks().await;
    assert_eq!(w.check_of('a').await, "completed:failure:system");
    // A retry runs again: the check is queued again, then succeeds.
    pipelines::retry_pipeline_decided(
        &w.t.db,
        &ctx(Actor::User(w.alice), "req-retry"),
        (w.web.project_id, p),
        w.alice,
        |_| Ok(()),
    )
    .await
    .expect("retry");
    w.sweep_checks().await;
    assert_eq!(w.check_of('a').await, "queued:-:system");
    w.run(&["ok\n"], JobResult::Succeeded).await;
    w.sweep_checks().await;
    assert_eq!(w.check_of('a').await, "completed:success:system");
    // check.completed was published on each change of the conclusion; the
    // system's runs are audited as the system with the label forge-ci.
    assert_eq!(
        w.scalar("SELECT count(*)::text FROM forge.events WHERE event_type = 'check.completed'")
            .await,
        "2"
    );
    assert_eq!(
        w.scalar(
            "SELECT DISTINCT actor_kind || ':' || coalesce(actor_label, '') FROM forge.audit_log \
             WHERE action LIKE 'check%'"
        )
        .await,
        "system:forge-ci"
    );
    w.t.drop_now().await;
}

/// Only the newest pipeline of a commit writes its check: an older one
/// finishing later never overwrites it.
#[tokio::test]
async fn only_the_newest_pipeline_of_a_commit_writes_its_check() {
    let w = world().await;
    let old = w.pipeline('a', &["test"]).await;
    w.sweep_checks().await;
    let (job, attempt) = {
        let c = lease::claim(&w.t.db, w.runner)
            .await
            .expect("claim")
            .expect("job");
        (c.job_id, c.attempt)
    };
    // A second pipeline of the same commit (a manual run).
    let newer = w
        .pipeline_of('a', Source::Manual, None, &["test"], CopyMark::Open)
        .await;
    assert!(newer > old);
    w.sweep_checks().await;
    lease::complete(&w.t.db, w.runner, job, attempt, JobResult::Failed)
        .await
        .expect("complete old");
    let swept = w.sweep_checks().await;
    assert!(
        matches!(swept.as_slice(), [PipelineCheck::Superseded]),
        "{swept:?}"
    );
    assert_eq!(w.check_of('a').await, "queued:-:system");
    w.t.drop_now().await;
}

/// P-7: a fingerprint that failed in a merge request's pipeline is fixed by
/// that merge request when its later pipeline succeeds; the diagnosis of
/// the same failure elsewhere names the fix.
#[tokio::test]
async fn a_merge_request_that_turns_green_is_the_known_fix() {
    let w = world().await;
    let (mr, ()) = open_mr_decided(
        &w.t.db,
        &ctx(Actor::User(w.alice), "req-open"),
        w.web.project_id,
        w.alice,
        &NewMergeRequest {
            title: "Raise the pool limit".to_owned(),
            description: String::new(),
            source_branch: RefName::parse("refs/heads/fix").expect("ref"),
            target_branch: RefName::parse("refs/heads/main").expect("ref"),
            draft: false,
        },
        |_| Ok(()),
    )
    .await
    .expect("open");
    w.pipeline_of(
        'a',
        Source::MergeRequest,
        Some(mr.id),
        &["test"],
        CopyMark::Open,
    )
    .await;
    w.run(&["pool exhausted FG-DB-0003\n"], JobResult::Failed)
        .await;
    fingerprint_failed_jobs(&w.t.db, 10, digest)
        .await
        .expect("sweep");
    w.sweep_checks().await;
    w.pipeline_of(
        'b',
        Source::MergeRequest,
        Some(mr.id),
        &["test"],
        CopyMark::Open,
    )
    .await;
    w.run(&["ok\n"], JobResult::Succeeded).await;
    let swept = w.sweep_checks().await;
    assert!(
        swept
            .iter()
            .any(|c| matches!(c, PipelineCheck::Written { fixed: 1, .. })),
        "{swept:?}"
    );
    // The same failure on main later: its diagnosis names the fix.
    let main = w.pipeline('c', &["test"]).await;
    w.run(&["pool exhausted FG-DB-0003\n"], JobResult::Failed)
        .await;
    fingerprint_failed_jobs(&w.t.db, 10, digest)
        .await
        .expect("sweep");
    let (_, failures, ()) = diagnosis_decided(&w.t.db, w.web.project_id, main, |_| Ok(()))
        .await
        .expect("diagnosis")
        .expect("found");
    let found = failures[0].found.as_ref().expect("found");
    assert_eq!(found.error_codes, ["FG-DB-0003"]);
    let fix = found.known_fix.as_ref().expect("known fix");
    assert_eq!((fix.merge_request_id, fix.iid), (mr.id.get(), mr.iid));
    assert_eq!(fix.title, "Raise the pool limit");
    w.t.drop_now().await;
}

/// ADR-0029 decision 5 / obligation 4 (replaces C3's
/// `ci_logs_have_no_read_path_before_c5`): CI log text — a chunk's `body`
/// and a failure's `excerpt` — is never readable by the read pool, and in
/// production code it is read only in this module: the page after the
/// caller's decision, the failure log by the system's sweep. Any other
/// file naming the log table, or a body read moved before the decision,
/// turns this red.
#[tokio::test]
async fn ci_log_text_is_read_only_after_a_decision() {
    let t = FreshDb::migrated().await;
    for sql in [
        "SELECT body FROM forge.ci_log_chunks LIMIT 1",
        "SELECT excerpt FROM forge.ci_job_failures LIMIT 1",
    ] {
        let err = sqlx::query(sql)
            .execute(t.db.pool().executor())
            .await
            .expect_err("the read pool never reads CI log text")
            .to_string()
            .to_ascii_lowercase();
        assert!(err.contains("permission denied"), "{sql}: {err}");
    }
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
                .join("\n");
            if !code.contains("ci_log_chunks") && !code.contains("ci_job_failures") {
                continue;
            }
            let allowed = name.ends_with("forge-db/src/ci_runners/lease.rs")
                || name.ends_with("forge-db/src/projects/purge.rs")
                || name.ends_with("forge-db/src/bootstrap/schema.rs")
                || name.ends_with("forge-db/src/ci_outputs/mod.rs");
            if !allowed {
                offenders.push(name);
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "CI log text read outside forge_db::ci_outputs (ADR-0029 decision 5): {offenders:?}"
    );
    // The runner's append reads only the size, never the text.
    assert!(!include_str!("../ci_runners/lease.rs").contains("SELECT body"));
    // In this module: the page is read once, after the decision; the
    // excerpt after the diagnosis' decision; the failure log only by the
    // sweep.
    let module = include_str!("mod.rs");
    let log_fn = &module[module
        .find("pub async fn job_log_decided")
        .expect("job_log_decided")..];
    let log_fn = &log_fn[..log_fn.find("\ntype Page").expect("end")];
    let decided = log_fn.find("decide(&target)?").expect("decision");
    let read = log_fn.find("read_page(").expect("read");
    assert!(decided < read, "the page is read after the decision");
    assert_eq!(module.matches("read_page(").count(), 2, "one call, one fn");
    let diag = &module[module
        .find("pub async fn diagnosis_decided")
        .expect("diagnosis_decided")..];
    let decided = diag.find("decide(&target)?").expect("decision");
    let excerpt = diag.find("jf.excerpt").expect("excerpt");
    assert!(decided < excerpt, "the excerpt is read after the decision");
    assert_eq!(
        module.matches("failure_log(").count(),
        2,
        "one call, one fn"
    );
    assert_eq!(module.matches("jf.excerpt").count(), 1);
    t.drop_now().await;
}

/// Migration 20261206200000 on a database a service account had written
/// `ci/pipeline` into before C5 (no rule stopped it then): those runs and
/// their annotations are removed (audited on the project), so a stale
/// success can never satisfy a required `ci/pipeline`; every other run is
/// kept, and afterwards no account can hold a `ci/pipeline` run
/// (check_runs_reporter).
#[tokio::test]
async fn the_migration_removes_ci_pipeline_runs_a_service_account_reported() {
    const C5: i64 = 20261206200000;
    let t = FreshDb::new().await;
    let before = sqlx::migrate::Migrator {
        migrations: std::borrow::Cow::Owned(
            crate::MIGRATOR
                .iter()
                .filter(|m| m.version < C5)
                .cloned()
                .collect(),
        ),
        ..sqlx::migrate::Migrator::DEFAULT
    };
    for m in [&before, &crate::MIGRATOR] {
        let mut mc = Db::connect_migrator(&t.migrate_config(&[]).db)
            .await
            .expect("migrator");
        let applied = crate::migrate(&mut mc, m, std::time::Duration::from_secs(60))
            .await
            .expect("migrate");
        mc.close().await;
        if std::ptr::eq(m, &before) {
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
            let (p, a) = (web.project_id.get(), alice.user_id.get());
            let run = |name: &str, c: char| {
                format!(
                    "INSERT INTO forge.check_runs (project_id, head_sha, name, status, \
                     conclusion, completed_at, reported_by, credential_id, request_id) VALUES \
                     ({p}, repeat('{c}', 40), '{name}', 'completed', 'success', now(), {a}, 7, \
                     'req-old')"
                )
            };
            t.commit_audited(
                ("project", p),
                &[
                    run(CI_PIPELINE_CHECK, 'a'),
                    run(CI_PIPELINE_CHECK, 'b'),
                    run("ci/gates", 'a'),
                    format!(
                        "INSERT INTO forge.check_annotations (check_run_id, project_id, \
                         position, path, start_line, end_line, level, message) SELECT id, {p}, \
                         0, 'src/lib.rs', 1, 1, 'failure', 'stale' FROM forge.check_runs \
                         WHERE name = '{CI_PIPELINE_CHECK}' AND head_sha = repeat('a', 40)"
                    ),
                ],
            )
            .await;
        } else {
            assert_eq!(applied, vec![C5]);
        }
    }
    let rows = t
        .json_rows(
            "SELECT name, reported_by IS NOT NULL AS reported FROM forge.check_runs ORDER BY name",
        )
        .await;
    assert_eq!(
        rows,
        vec![serde_json::json!({"name": "ci/gates", "reported": true})]
    );
    let left = t
        .json_rows("SELECT count(*) AS n FROM forge.check_annotations")
        .await;
    assert_eq!(left, vec![serde_json::json!({"n": 0})]);
    let audit = t
        .json_rows(
            "SELECT action, resource_type, details -> 'runs' AS runs, \
             details -> 'annotations' AS annotations FROM forge.audit_log \
             WHERE action = 'check_run.reserved_removed'",
        )
        .await;
    assert_eq!(
        audit,
        vec![serde_json::json!({
            "action": "check_run.reserved_removed",
            "resource_type": "project",
            "runs": 2,
            "annotations": 1,
        })]
    );
    let mut su = t.superuser().await;
    let p: i64 = sqlx::query_scalar("SELECT id FROM forge.projects LIMIT 1")
        .fetch_one(&mut su)
        .await
        .expect("project");
    let a: i64 = sqlx::query_scalar("SELECT id FROM forge.users WHERE username = 'alice'")
        .fetch_one(&mut su)
        .await
        .expect("alice");
    let sql = format!(
        "INSERT INTO forge.check_runs (project_id, head_sha, name, status, reported_by, \
         request_id) VALUES ({p}, repeat('c', 40), '{CI_PIPELINE_CHECK}', 'queued', {a}, 'req-x')"
    );
    let constraint = match sqlx::query(&sql).execute(&mut su).await {
        Err(sqlx::Error::Database(e)) => e.constraint().map(str::to_owned),
        _ => None,
    };
    assert_eq!(constraint.as_deref(), Some("check_runs_reporter"));
    drop(su);
    t.drop_now().await;
}
