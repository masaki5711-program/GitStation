//! The mergeability job end to end (M3 R2, ADR-0024 §3): the production
//! handlers and subscribers against a migrated database and real git,
//! driven step by step. A merge request is checked when opened, when its
//! target moves (also back to an earlier tip: A → B → A), when its source
//! gets a version, when it is retargeted and when its target is deleted;
//! the conflict events come once per flip; a version keeps the bases it
//! was recorded with.

use std::path::Path;
use std::sync::Arc;

use forge_auth::{prove, Action, Credential, LoadedProject, MergeRequestCtx, MergeRequestEdit};
use forge_core::config::ReviewConfig;
use forge_core::{
    MergeRequestState, MergeabilityStatus, ObjectId, ProjectAccessLoader, ProjectRepository,
    PushRecord, PushRecorder, PushUpdateKind, PushedRef, Pusher, RefName, RepositoryState,
    RequestId, UserId,
};
use forge_db::jobs::{JobRunner, WorkerId};
use forge_db::review::{list_versions, mergeability_written, StoredMergeability};
use forge_db::subscribers::{register, Registry, Start};
use forge_db::test_support::FreshDb;
use forge_git::browse::{BrowseLimits, Browser};
use forge_review::{EditFields, NewFields, Review, ReviewRequest, StoredMergeRequest};

use super::tests::{drain, plumb};
use crate::worker::jobs::handlers;
use crate::worker::provision::tests::{project, storage};
use crate::worker::registry;

/// A commit with `files` (flat names) on top of `parent`.
fn commit_on(repo: &Path, parent: Option<&str>, files: &[(&str, &str)]) -> String {
    let mut listing = String::new();
    let mut sorted: Vec<&(&str, &str)> = files.iter().collect();
    sorted.sort_by_key(|(name, _)| *name);
    for (name, content) in sorted {
        let blob = plumb(
            repo,
            &["hash-object", "-w", "--stdin"],
            Some(content.as_bytes()),
        );
        listing.push_str(&format!("100644 blob {blob}\t{name}\n"));
    }
    let tree = plumb(repo, &["mktree"], Some(listing.as_bytes()));
    let mut args = vec!["commit-tree", tree.as_str(), "-m", "synthetic"];
    if let Some(p) = parent {
        args.extend(["-p", p]);
    }
    plumb(repo, &args, None)
}

fn oid(raw: &str) -> ObjectId {
    ObjectId::parse(raw).expect("oid")
}

fn name(raw: &str) -> RefName {
    RefName::parse(raw).expect("ref")
}

struct World {
    t: FreshDb,
    _dir: tempfile::TempDir,
    runner: JobRunner,
    registry: Registry,
    review: Review,
    /// The review's browser (its ref cache is dropped after a git write).
    browser: Browser,
    path: std::path::PathBuf,
    repo: forge_core::RepoId,
    project: forge_core::ProjectId,
    alice: UserId,
    pushes: usize,
    storage: crate::worker::provision::Storage,
}

impl World {
    async fn new() -> Self {
        let t = FreshDb::migrated().await;
        let (dir, storage) = storage();
        let web = project(&t, "alice").await;
        let mut runner = JobRunner::new(t.db.clone(), WorkerId::generate()); // clone: pool handles
        for handler in handlers(&t.db, &storage).expect("handlers") {
            runner.register(handler).expect("register");
        }
        let registry = registry(&ReviewConfig::default()).expect("registry");
        for subscriber in registry.iter() {
            register(&t.db, subscriber.as_ref(), Start::Head)
                .await
                .expect("register subscriber");
        }
        drain(&t, &runner, &registry).await;
        let path = storage
            .root
            .existing(web.repository_id)
            .expect("provisioned");
        let rows = t
            .json_rows("SELECT id FROM forge.users WHERE username = 'alice'")
            .await;
        let alice = UserId::new(
            rows.first()
                .and_then(|r| r.get("id"))
                .and_then(serde_json::Value::as_i64)
                .expect("alice"),
        );
        let browser = Browser::new(
            storage.git.clone(),  // clone: the browser owns its handles
            storage.root.clone(), // clone: as git
            BrowseLimits::default(),
        );
        // clone: shares the caches with the review's copy
        let review = Review::new(t.db.clone(), Arc::new(browser.clone()));
        Self {
            t,
            _dir: dir,
            runner,
            registry,
            review,
            browser,
            // clone: the sweep's own handles
            storage: storage.clone(),
            path,
            repo: web.repository_id,
            project: web.project_id,
            alice,
            pushes: 0,
        }
    }

    async fn drain(&self) {
        drain(&self.t, &self.runner, &self.registry).await;
    }

    async fn loaded(&self) -> LoadedProject {
        let record = self
            .t
            .db
            .load_project(self.project, Some(self.alice))
            .await
            .expect("load")
            .expect("project");
        LoadedProject {
            id: record.id,
            visibility: record.access.visibility,
            role: record.access.role,
            pending_deletion: record.access.pending_deletion,
        }
    }

    fn principal(&self) -> forge_auth::Principal {
        forge_auth::Principal::authenticated(
            self.alice,
            false,
            true,
            Credential::Session,
            forge_auth::AccountKind::Human,
        )
    }

    fn request(n: &str) -> ReviewRequest {
        ReviewRequest {
            request_id: RequestId::parse(n).expect("id"),
            credential_id: None,
        }
    }

    fn repository(&self) -> ProjectRepository {
        ProjectRepository {
            id: self.repo,
            state: RepositoryState::Ready,
        }
    }

    async fn open(&self, source: &str, target: &str) -> StoredMergeRequest {
        let proof = prove(
            &self.principal(),
            Action::MergeRequestCreate,
            MergeRequestCtx::new(
                self.loaded().await,
                forge_auth::LoadedMergeRequest {
                    iid: None,
                    author: self.alice,
                    state: MergeRequestState::Open,
                    draft: false,
                    source_branch: name(&format!("refs/heads/{source}")),
                    target_branch: name(&format!("refs/heads/{target}")),
                },
            ),
        )
        .expect("proof");
        self.review
            .open(
                &Self::request("req-open"),
                &proof,
                self.repository(),
                &NewFields {
                    title: "Feature".to_owned(),
                    description: String::new(),
                },
            )
            .await
            .expect("open")
    }

    async fn retarget(&self, mr: &StoredMergeRequest, target: &str) {
        let proof = prove(
            &self.principal(),
            Action::MergeRequestUpdate,
            MergeRequestCtx::edited(
                self.loaded().await,
                forge_review::loaded_merge_request(mr),
                MergeRequestEdit {
                    target_branch: Some(name(&format!("refs/heads/{target}"))),
                    draft: None,
                },
            ),
        )
        .expect("proof");
        let _edited = self
            .review
            .edit(
                &Self::request("req-retarget"),
                &proof,
                self.repository(),
                &EditFields {
                    title: None,
                    description: None,
                },
            )
            .await
            .expect("retarget");
    }

    /// Move (or delete: `None`) `branch` in git and record the push like
    /// forge-web does.
    async fn push(&mut self, branch: &str, new: Option<&str>) {
        let full = format!("refs/heads/{branch}");
        let old = plumb(
            &self.path,
            &["for-each-ref", "--format=%(objectname)", &full],
            None,
        );
        let zero = "0".repeat(40);
        let (kind, new_id) = match (old.is_empty(), new) {
            (_, None) => {
                plumb(&self.path, &["update-ref", "-d", &full], None);
                (PushUpdateKind::Delete, zero.clone())
            }
            (true, Some(n)) => {
                plumb(&self.path, &["update-ref", &full, n], None);
                (PushUpdateKind::Create, n.to_owned())
            }
            (false, Some(n)) => {
                plumb(&self.path, &["update-ref", &full, n], None);
                (PushUpdateKind::Force, n.to_owned())
            }
        };
        self.pushes += 1;
        let pusher = Pusher {
            request_id: RequestId::parse(&format!("req-push-{}", self.pushes)).expect("id"),
            user: self.alice,
            kind: forge_core::UserKind::Human,
            credential_id: None,
            source_ip: None,
            user_agent: None,
        };
        let old_id = if old.is_empty() { zero } else { old };
        let recorder: &dyn PushRecorder = &self.t.db;
        let recorded = recorder
            .record_push(
                &pusher,
                &PushRecord {
                    repository_id: self.repo,
                    updates: vec![PushedRef {
                        name: name(&full),
                        old: oid(&old_id),
                        new: oid(&new_id),
                        kind,
                        commits: Vec::new(),
                        total_commits: 0,
                        total_commits_partial: false,
                    }],
                },
            )
            .await
            .expect("record push");
        assert_eq!(recorded, forge_core::PushRecorded::Recorded);
        self.drain().await;
    }

    async fn status(&self, mr: &StoredMergeRequest) -> StoredMergeability {
        mergeability_written(&self.t.db, mr.id)
            .await
            .expect("read")
            .expect("a check was requested")
    }

    async fn flips(&self) -> Vec<String> {
        self.t
            .json_rows(
                "SELECT event_type FROM forge.events \
                 WHERE event_type LIKE 'merge_request.conflict_%' ORDER BY id",
            )
            .await
            .iter()
            .filter_map(|r| {
                r.get("event_type")
                    .and_then(|v| v.as_str())
                    .map(str::to_owned)
            })
            .collect()
    }
}

impl World {
    /// `main` at a base, `feature` adding `feature.txt` on it; `clash` (not
    /// on a branch) adds the same file differently.
    async fn histories(&self) -> (String, String, String) {
        let base = commit_on(&self.path, None, &[("README", "base\n")]);
        // The head adds `feature.txt`; the conflicting target adds it too.
        let c1 = commit_on(
            &self.path,
            Some(&base),
            &[("README", "base\n"), ("feature.txt", "head\n")],
        );
        let clash = commit_on(
            &self.path,
            Some(&base),
            &[("README", "base\n"), ("feature.txt", "target\n")],
        );
        plumb(&self.path, &["update-ref", "refs/heads/main", &base], None);
        plumb(&self.path, &["update-ref", "refs/heads/feature", &c1], None);

        (base, c1, clash)
    }
}

#[tokio::test]
async fn target_moves_and_new_versions_are_checked_and_flip_once_each() {
    let mut w = World::new().await;
    let (base, c1, clash) = w.histories().await;
    // Open: version 1 with its bases, then checked: mergeable. The first
    // decision is not a flip.
    let mr = w.open("feature", "main").await;
    w.drain().await;
    let s = w.status(&mr).await;
    assert_eq!(s.status, MergeabilityStatus::CanBeMerged, "{s:?}");
    assert_eq!(s.checked_head_sha, Some(oid(&c1)));
    assert_eq!(s.checked_target_sha, Some(oid(&base)));
    let versions = list_versions(&w.t.db, mr.id).await.expect("versions");
    assert_eq!(versions[0].base_sha, Some(oid(&base)));
    assert_eq!(versions[0].merge_base_sha, Some(oid(&base)));
    assert!(w.flips().await.is_empty());

    // The target moves to a clashing tip: re-checked, a conflict (A → B).
    w.push("main", Some(&clash)).await;
    let s = w.status(&mr).await;
    assert_eq!(s.status, MergeabilityStatus::CannotBeMerged, "{s:?}");
    assert_eq!(
        s.reason.map(forge_core::CheckReason::as_str),
        Some("conflict")
    );
    assert_eq!(s.conflicts, ["feature.txt"]);
    assert_eq!(s.checked_target_sha, Some(oid(&clash)));
    // Version 1 keeps the bases it was recorded with.
    let versions = list_versions(&w.t.db, mr.id).await.expect("versions");
    assert_eq!(versions[0].base_sha, Some(oid(&base)));

    // Back to the first tip (B → A): checked again although that pair was
    // checked before (a fresh seq, a fresh job).
    w.push("main", Some(&base)).await;
    let s = w.status(&mr).await;
    assert_eq!(s.status, MergeabilityStatus::CanBeMerged, "{s:?}");
    assert_eq!(
        w.flips().await,
        [
            "merge_request.conflict_detected",
            "merge_request.conflict_resolved"
        ]
    );

    // The target clashes again, then the source gets a version that takes
    // the target's content: version 2 records the clashing tip as its
    // base, and the conflict is resolved.
    w.push("main", Some(&clash)).await;
    let c2 = commit_on(
        &w.path,
        Some(&c1),
        &[("README", "base\n"), ("feature.txt", "target\n")],
    );
    w.push("feature", Some(&c2)).await;
    let s = w.status(&mr).await;
    assert_eq!(s.status, MergeabilityStatus::CanBeMerged, "{s:?}");
    assert_eq!(s.checked_head_sha, Some(oid(&c2)));
    let versions = list_versions(&w.t.db, mr.id).await.expect("versions");
    assert_eq!(versions.len(), 2);
    assert_eq!(versions[0].base_sha, Some(oid(&base)), "v1 unchanged");
    assert_eq!(versions[1].base_sha, Some(oid(&clash)));
    assert_eq!(versions[1].merge_base_sha, Some(oid(&base)));
    assert_eq!(
        w.flips().await.len(),
        4,
        "detected, resolved, detected, resolved"
    );

    w.t.drop_now().await;
}

/// A retarget re-checks (`unchecked` at once, then decided against the new
/// target), a deleted target is `target_missing`; undecided statuses never
/// flip.
#[tokio::test]
async fn a_retarget_and_a_deleted_target_are_checked_without_flips() {
    let mut w = World::new().await;
    let (_base, _c1, _clash) = w.histories().await;
    let mr = w.open("feature", "main").await;
    w.drain().await;
    assert_eq!(w.status(&mr).await.status, MergeabilityStatus::CanBeMerged);
    // Retarget to a branch whose tip has no common history: `unchecked`
    // in the edit's transaction, then `check_failed unrelated_histories`.
    let orphan = commit_on(&w.path, None, &[("elsewhere", "x\n")]);
    plumb(
        &w.path,
        &["update-ref", "refs/heads/release", &orphan],
        None,
    );
    w.browser.invalidate(w.repo);
    let reloaded = w
        .review
        .merge_request(w.project, mr.iid)
        .await
        .expect("read")
        .expect("mr");
    w.retarget(&reloaded, "release").await;
    assert_eq!(w.status(&mr).await.status, MergeabilityStatus::Unchecked);
    w.drain().await;
    let s = w.status(&mr).await;
    assert_eq!(s.status, MergeabilityStatus::CheckFailed, "{s:?}");
    assert_eq!(
        s.reason.map(forge_core::CheckReason::as_str),
        Some("unrelated_histories")
    );

    // The target is deleted: checked again, `target_missing`.
    w.push("release", None).await;
    let s = w.status(&mr).await;
    assert_eq!(s.status, MergeabilityStatus::CheckFailed, "{s:?}");
    assert_eq!(
        s.reason.map(forge_core::CheckReason::as_str),
        Some("target_missing")
    );
    // Undecided statuses never flip.
    assert!(w.flips().await.is_empty());
    w.t.drop_now().await;
}

/// Review (silent-failure HIGH, dead letter): a run that fails on its last
/// attempt (here: the repository is still provisioning, a retryable
/// failure) ends its request as `check_failed reason=job_failed`; an
/// earlier attempt leaves it to the retry.
#[tokio::test]
async fn a_last_failed_attempt_ends_the_request_as_job_failed() {
    use forge_core::CheckReason;
    use forge_db::jobs::{ClaimedJob, JobHandler, JobId, JobKind, JobTimeout, Lease};
    use forge_db::review::{request_check, MERGEABILITY_JOB, MERGEABILITY_TIMEOUT};

    let t = FreshDb::migrated().await;
    let (_dir, storage) = storage();
    // Not provisioned: no job ran, the repository is not ready.
    let web = project(&t, "alice").await;
    let alice = t
        .json_rows("SELECT id FROM forge.users WHERE username = 'alice'")
        .await
        .first()
        .and_then(|r| r["id"].as_i64())
        .expect("alice");
    let p = web.project_id.get();
    t.commit_audited(
        ("project", p),
        &[
            format!(
                "INSERT INTO forge.merge_request_counters (project_id, last_iid) VALUES ({p}, 1)"
            ),
            format!(
                "INSERT INTO forge.merge_requests (project_id, iid, title, author_id, \
                 source_branch, target_branch) VALUES ({p}, 1, 'synthetic', {alice}, \
                 'refs/heads/feature', 'refs/heads/main')"
            ),
        ],
    )
    .await;
    let mr = forge_core::ids::MergeRequestId::new(
        t.json_rows("SELECT id FROM forge.merge_requests")
            .await
            .first()
            .and_then(|r| r["id"].as_i64())
            .expect("mr"),
    );
    let seq = request_check(&t.db, mr, RequestId::parse("req-q").expect("id"))
        .await
        .expect("request")
        .expect("open");
    let handler = super::MergeabilityHandler::new(&t.db, &storage).expect("handler");
    let claimed = |attempt: i32| ClaimedJob {
        lease: Lease {
            job_id: JobId::new(1),
            worker_id: WorkerId::generate(),
            attempt,
        },
        kind: JobKind::parse(MERGEABILITY_JOB).expect("kind"),
        payload: serde_json::json!({"merge_request_id": mr.get(), "check_seq": seq}),
        max_attempts: 2,
        timeout: JobTimeout::new(MERGEABILITY_TIMEOUT).expect("timeout"),
        origin_request_id: "req-job".to_owned(),
        origin_event_seq: None,
    };
    assert!(handler.run(claimed(1)).await.is_err(), "not ready");
    let s = mergeability_written(&t.db, mr)
        .await
        .expect("read")
        .expect("row");
    assert_eq!(s.status, MergeabilityStatus::Checking, "left to the retry");
    assert!(handler.run(claimed(2)).await.is_err(), "still not ready");
    let s = mergeability_written(&t.db, mr)
        .await
        .expect("read")
        .expect("row");
    assert_eq!(
        (s.status, s.reason),
        (
            MergeabilityStatus::CheckFailed,
            Some(CheckReason::JobFailed)
        )
    );
    t.drop_now().await;
}

/// The sweep task ends a row left `checking` with no live job and
/// re-requests an `unchecked` one whose job died.
#[tokio::test]
async fn the_sweep_task_ends_stale_runs_and_requests_deferred_checks() {
    use forge_core::CheckReason;
    let w = World::new().await;
    let _histories = w.histories().await;
    let mr = w.open("feature", "main").await;
    w.drain().await;
    // A run that crashed mid-check: `checking`, its job dead, long ago.
    w.t.exec_superuser(&format!(
        "UPDATE forge.merge_request_mergeability SET status = 'checking', \
         updated_at = now() - interval '1 hour' WHERE merge_request_id = {}",
        mr.id.get()
    ))
    .await;
    let next = super::super::mergeability_sweep::run_mergeability_sweep(&w.t.db, Some(&w.storage))
        .await
        .expect("sweep");
    assert!(matches!(next, crate::worker::supervise::Next::AfterPeriod));
    let s = w.status(&mr).await;
    assert_eq!(
        (s.status, s.reason),
        (MergeabilityStatus::CheckFailed, Some(CheckReason::JobStale))
    );
    // Deferred: `unchecked`, no live job (its job died) → swept and checked.
    w.t.exec_superuser(&format!(
        "UPDATE forge.merge_request_mergeability SET status = 'unchecked' \
         WHERE merge_request_id = {}",
        mr.id.get()
    ))
    .await;
    super::super::mergeability_sweep::run_mergeability_sweep(&w.t.db, Some(&w.storage))
        .await
        .expect("sweep");
    w.drain().await;
    assert_eq!(w.status(&mr).await.status, MergeabilityStatus::CanBeMerged);
    w.t.drop_now().await;
}

/// Re-review (MEDIUM): a decided status whose target moved without a push
/// event reaching the subscriber (dead-lettered, or past the deferral
/// bound) is verified by the sweep against the live tip and checked again;
/// a pair that is still live is only marked verified.
#[tokio::test]
async fn the_sweep_rechecks_a_decided_status_whose_target_moved_silently() {
    let w = World::new().await;
    let (_base, _c1, clash) = w.histories().await;
    let mr = w.open("feature", "main").await;
    w.drain().await;
    assert_eq!(w.status(&mr).await.status, MergeabilityStatus::CanBeMerged);
    // Still live: verified, nothing requested.
    let jobs_before =
        w.t.json_rows("SELECT id FROM forge.jobs WHERE kind = 'merge_request.mergeability'")
            .await
            .len();
    super::super::mergeability_sweep::run_mergeability_sweep(&w.t.db, Some(&w.storage))
        .await
        .expect("sweep");
    let confirmed =
        w.t.json_rows("SELECT confirmed_at IS NOT NULL AS c FROM forge.merge_request_mergeability")
            .await;
    assert_eq!(confirmed[0]["c"], true);
    let jobs_after =
        w.t.json_rows("SELECT id FROM forge.jobs WHERE kind = 'merge_request.mergeability'")
            .await
            .len();
    assert_eq!(jobs_after, jobs_before, "a live pair is not re-requested");
    // The target moves behind the subscriber's back (no push event).
    plumb(&w.path, &["update-ref", "refs/heads/main", &clash], None);
    w.t.exec_superuser(
        "UPDATE forge.merge_request_mergeability SET confirmed_at = now() - interval '2 hours'",
    )
    .await;
    super::super::mergeability_sweep::run_mergeability_sweep(&w.t.db, Some(&w.storage))
        .await
        .expect("sweep");
    w.drain().await;
    let s = w.status(&mr).await;
    assert_eq!(s.status, MergeabilityStatus::CannotBeMerged, "{s:?}");
    assert_eq!(s.checked_target_sha, Some(oid(&clash)));
    w.t.drop_now().await;
}

// ── M3 X2: `forge/spec-drift` ──

/// A commit whose tree is `.forge/repo.yml` (`yml`) and `README`.
fn commit_with_repo_yml(repo: &Path, yml: &str) -> String {
    let blob = plumb(
        repo,
        &["hash-object", "-w", "--stdin"],
        Some(yml.as_bytes()),
    );
    let forge = plumb(
        repo,
        &["mktree"],
        Some(format!("100644 blob {blob}\trepo.yml\n").as_bytes()),
    );
    let readme = plumb(repo, &["hash-object", "-w", "--stdin"], Some(b"base\n"));
    let tree = plumb(
        repo,
        &["mktree"],
        Some(format!("040000 tree {forge}\t.forge\n100644 blob {readme}\tREADME\n").as_bytes()),
    );
    plumb(repo, &["commit-tree", &tree, "-m", "synthetic"], None)
}

/// Every merge request version gets its `forge/spec-drift` run through
/// the production subscriber (`spec-drift`) and job handler
/// (`check.spec_drift`): written by forge's worker, without a reporter.
#[tokio::test]
async fn a_merge_request_version_gets_its_spec_drift_run() {
    let mut w = World::new().await;
    let base = commit_on(&w.path, None, &[("README", "base\n")]);
    w.push("main", Some(&base)).await;
    let head = commit_with_repo_yml(
        &w.path,
        "specs:\n  - repo: nobody/specs\n    version: v1\n  - path: spec/\n",
    );
    w.push("feature", Some(&head)).await;
    let _mr = w.open("feature", "main").await;
    w.drain().await;
    let runs = forge_db::checks::check_runs(&w.t.db, w.project, &oid(&head))
        .await
        .expect("runs");
    // The version also gets its `forge/risk` run (M3 X7).
    let run = runs
        .iter()
        .find(|r| r.name.as_str() == forge_core::checks::SPEC_DRIFT_CHECK)
        .expect("a forge/spec-drift run");
    assert_eq!(
        run.conclusion,
        Some(forge_core::CheckConclusion::Neutral),
        "{}",
        run.summary
    );
    assert_eq!(run.reported_by, None);
    assert!(run.summary.contains("FG-REPO-0026"), "{}", run.summary);
    w.t.drop_now().await;
}

/// The handler's last failure records `error` on the head (the job dies;
/// the check never stays without an answer).
#[tokio::test]
async fn a_spec_drift_job_that_dies_records_error() {
    let w = World::new().await;
    let missing = oid(&"e".repeat(40));
    let mut job = forge_db::checks::spec_drift::spec_drift_job(
        &forge_db::checks::spec_drift::SpecDriftRequest {
            project: w.project,
            head: missing.clone(),
        },
        RequestId::parse("req-drift-dies").expect("id"),
        None,
    )
    .expect("job");
    job.max_attempts = forge_db::jobs::MaxAttempts::new(1).expect("one attempt");
    forge_db::jobs::enqueue(&w.t.db, &job)
        .await
        .expect("enqueue");
    // The head is no commit of the repository: git fails (FG-GIT-0003),
    // the one attempt is the last, the job dies after answering `error`.
    let ran = w
        .runner
        .run_once()
        .await
        .expect("runner")
        .expect("a job ran");
    assert!(matches!(ran, forge_db::jobs::Ran::Dead { .. }), "{ran:?}");
    let runs = forge_db::checks::check_runs(&w.t.db, w.project, &missing)
        .await
        .expect("runs");
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert_eq!(runs[0].conclusion, Some(forge_core::CheckConclusion::Error));
    assert!(
        runs[0].summary.contains("req-drift-dies"),
        "{}",
        runs[0].summary
    );
    let dead =
        w.t.json_rows("SELECT state FROM forge.jobs WHERE kind = 'check.spec_drift'")
            .await;
    assert_eq!(dead[0]["state"], "dead", "{dead:?}");
    w.t.drop_now().await;
}

fn missing_head_job(w: &World, attempts: u8, request: &str) -> forge_db::jobs::NewJob {
    let mut job = forge_db::checks::spec_drift::spec_drift_job(
        &forge_db::checks::spec_drift::SpecDriftRequest {
            project: w.project,
            head: oid(&"d".repeat(40)),
        },
        RequestId::parse(request).expect("id"),
        None,
    )
    .expect("job");
    job.max_attempts = forge_db::jobs::MaxAttempts::new(attempts).expect("attempts");
    job
}

async fn drift_runs(w: &World) -> Vec<forge_db::checks::StoredCheckRun> {
    forge_db::checks::check_runs(&w.t.db, w.project, &oid(&"d".repeat(40)))
        .await
        .expect("runs")
}

/// Review X2 (rust HIGH): an attempt that will be retried writes nothing;
/// the last one answers `error`.
#[tokio::test]
async fn only_the_last_attempt_of_a_failing_drift_job_answers_error() {
    let w = World::new().await;
    let job = missing_head_job(&w, 2, "req-drift-two");
    forge_db::jobs::enqueue(&w.t.db, &job)
        .await
        .expect("enqueue");
    let first = w.runner.run_once().await.expect("runner").expect("ran");
    assert!(
        matches!(first, forge_db::jobs::Ran::Retrying { .. }),
        "{first:?}"
    );
    assert!(
        drift_runs(&w).await.is_empty(),
        "no answer while it is retried"
    );
    // Skip the backoff.
    w.t.exec_superuser("UPDATE forge.jobs SET run_after = now() WHERE kind = 'check.spec_drift'")
        .await;
    let second = w.runner.run_once().await.expect("runner").expect("ran");
    assert!(
        matches!(second, forge_db::jobs::Ran::Dead { .. }),
        "{second:?}"
    );
    let runs = drift_runs(&w).await;
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].conclusion, Some(forge_core::CheckConclusion::Error));
    w.t.drop_now().await;
}

/// Review X2 (rust HIGH): a job the handler never finished (its worker
/// crashed, the runner's timeout cut it) dies through the reaper without
/// an answer; the sweep answers `error` for it once.
#[tokio::test]
async fn the_sweep_answers_a_reaped_drift_job_with_error_once() {
    let w = World::new().await;
    let job = missing_head_job(&w, 1, "req-drift-crash");
    forge_db::jobs::enqueue(&w.t.db, &job)
        .await
        .expect("enqueue");
    let kind = forge_db::jobs::JobKind::parse("check.spec_drift").expect("kind");
    let worker = forge_db::jobs::WorkerId::generate();
    let claimed = forge_db::jobs::claim(&w.t.db, &worker, &[kind])
        .await
        .expect("claim")
        .expect("a job");
    // The worker dies holding it: its lease runs out.
    w.t.exec_superuser(&format!(
        "UPDATE forge.jobs SET lease_until = now() - interval '1 second' WHERE id = {}",
        claimed.lease.job_id.get()
    ))
    .await;
    let reaped = forge_db::jobs::reap(&w.t.db).await.expect("reap");
    assert_eq!(reaped.len(), 1);
    assert!(drift_runs(&w).await.is_empty(), "the reaper writes no run");
    let answered = crate::worker::spec_drift_sweep::run_spec_drift_sweep(&w.t.db)
        .await
        .expect("sweep");
    assert_eq!(answered, crate::worker::supervise::Next::AfterPeriod);
    let runs = drift_runs(&w).await;
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert_eq!(runs[0].conclusion, Some(forge_core::CheckConclusion::Error));
    assert!(
        runs[0].summary.contains("FG-DB-0007"),
        "{}",
        runs[0].summary
    );
    assert!(
        runs[0].summary.contains("req-drift-crash"),
        "{}",
        runs[0].summary
    );
    // Answered: the next sweep writes nothing more.
    crate::worker::spec_drift_sweep::run_spec_drift_sweep(&w.t.db)
        .await
        .expect("sweep again");
    assert_eq!(drift_runs(&w).await[0].revision, 1);
    w.t.drop_now().await;
}

/// Every merge request version gets its `forge/risk` run through the
/// production subscriber (`risk`) and job handler (`check.risk`): the head
/// is compared with the merge request's target, and the run is written by
/// forge's worker without a reporter.
#[tokio::test]
async fn a_merge_request_version_gets_its_risk_run() {
    let mut w = World::new().await;
    let base = commit_on(&w.path, None, &[("README", "base\n")]);
    w.push("main", Some(&base)).await;
    let head = commit_on(
        &w.path,
        Some(&base),
        &[("README", "base\n"), (".env", "A=1\n")],
    );
    w.push("feature", Some(&head)).await;
    let _mr = w.open("feature", "main").await;
    w.drain().await;
    let runs = forge_db::checks::check_runs(&w.t.db, w.project, &oid(&head))
        .await
        .expect("runs");
    let run = runs
        .iter()
        .find(|r| r.name.as_str() == forge_core::checks::RISK_CHECK)
        .expect("a forge/risk run");
    assert_eq!(
        run.conclusion,
        Some(forge_core::CheckConclusion::Failure),
        "{}",
        run.summary
    );
    assert_eq!(run.reported_by, None);
    assert!(run.summary.contains("secret_file"), "{}", run.summary);
    assert!(run.summary.contains("FG-REVIEW-0023"), "{}", run.summary);
    w.t.drop_now().await;
}

fn missing_head_risk_job(w: &World, attempts: u8, request: &str) -> forge_db::jobs::NewJob {
    let mut job = forge_db::checks::risk::risk_job(
        &forge_db::checks::risk::RiskRequest {
            project: w.project,
            head: oid(&"c".repeat(40)),
        },
        "mr1",
        RequestId::parse(request).expect("id"),
        None,
    )
    .expect("job");
    job.max_attempts = forge_db::jobs::MaxAttempts::new(attempts).expect("attempts");
    job
}

async fn risk_runs(w: &World) -> Vec<forge_db::checks::StoredCheckRun> {
    forge_db::checks::check_runs(&w.t.db, w.project, &oid(&"c".repeat(40)))
        .await
        .expect("runs")
}

/// A job that fails is retried without writing anything; its last attempt
/// answers `error` on its head and then the job dies. The head is a
/// version of an open merge request but no commit of the repository, so
/// git fails (the target is compared with it).
#[tokio::test]
async fn a_risk_job_that_dies_records_error_on_its_last_attempt_only() {
    let mut w = World::new().await;
    let base = commit_on(
        &w.path,
        None,
        &[(
            "README", "base
",
        )],
    );
    w.push("main", Some(&base)).await;
    let tip = commit_on(
        &w.path,
        Some(&base),
        &[(
            "README", "feature
",
        )],
    );
    w.push("feature", Some(&tip)).await;
    let mr = w.open("feature", "main").await;
    // The open and the real version run through (their risk run is fine).
    w.drain().await;
    let missing = oid(&"c".repeat(40));
    forge_db::review::record_mr_version(
        &w.t.db,
        &forge_db::change::ChangeContext::new(
            forge_db::change::Actor::System,
            RequestId::parse("req-version").expect("id"),
        ),
        mr.id,
        &missing,
        Some(w.alice),
    )
    .await
    .expect("version");
    let job = missing_head_risk_job(&w, 2, "req-risk-dies");
    forge_db::jobs::enqueue(&w.t.db, &job)
        .await
        .expect("enqueue");
    let first = w.runner.run_once().await.expect("runner").expect("ran");
    assert!(
        matches!(first, forge_db::jobs::Ran::Retrying { .. }),
        "{first:?}"
    );
    assert!(
        risk_runs(&w).await.is_empty(),
        "no answer while it is retried"
    );
    // Skip the backoff.
    w.t.exec_superuser("UPDATE forge.jobs SET run_after = now() WHERE kind = 'check.risk'")
        .await;
    let second = w.runner.run_once().await.expect("runner").expect("ran");
    assert!(
        matches!(second, forge_db::jobs::Ran::Dead { .. }),
        "{second:?}"
    );
    let runs = risk_runs(&w).await;
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert_eq!(runs[0].conclusion, Some(forge_core::CheckConclusion::Error));
    assert!(
        runs[0].summary.contains("req-risk-dies"),
        "{}",
        runs[0].summary
    );
    w.t.drop_now().await;
}

/// A job the handler never finished (its worker crashed, the runner's
/// timeout cut it) dies through the reaper without an answer; the sweep
/// answers `error` for it once.
#[tokio::test]
async fn the_risk_sweep_answers_a_reaped_job_with_error_once() {
    let w = World::new().await;
    let job = missing_head_risk_job(&w, 1, "req-risk-crash");
    forge_db::jobs::enqueue(&w.t.db, &job)
        .await
        .expect("enqueue");
    let kind = forge_db::jobs::JobKind::parse("check.risk").expect("kind");
    let worker = forge_db::jobs::WorkerId::generate();
    let claimed = forge_db::jobs::claim(&w.t.db, &worker, &[kind])
        .await
        .expect("claim")
        .expect("a job");
    // The worker dies holding it: its lease runs out.
    w.t.exec_superuser(&format!(
        "UPDATE forge.jobs SET lease_until = now() - interval '1 second' WHERE id = {}",
        claimed.lease.job_id.get()
    ))
    .await;
    let reaped = forge_db::jobs::reap(&w.t.db).await.expect("reap");
    assert_eq!(reaped.len(), 1);
    assert!(risk_runs(&w).await.is_empty(), "the reaper writes no run");
    let answered =
        crate::worker::risk_sweep::sweep(&w.t.db, &std::sync::atomic::AtomicI64::new(0), 50)
            .await
            .expect("sweep");
    assert_eq!(answered, crate::worker::supervise::Next::AfterPeriod);
    let runs = risk_runs(&w).await;
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert_eq!(runs[0].conclusion, Some(forge_core::CheckConclusion::Error));
    assert!(
        runs[0].summary.contains("FG-DB-0007"),
        "{}",
        runs[0].summary
    );
    assert!(
        runs[0].summary.contains("req-risk-crash"),
        "{}",
        runs[0].summary
    );
    // Answered: the next sweep writes nothing more.
    crate::worker::risk_sweep::sweep(&w.t.db, &std::sync::atomic::AtomicI64::new(0), 50)
        .await
        .expect("sweep again");
    assert_eq!(risk_runs(&w).await[0].revision, 1);
    w.t.drop_now().await;
}

/// Review X7 (MEDIUM): the sweep walks the dead jobs by id with a cursor, a
/// batch at a time, and wraps at the end; a job it cannot answer does not
/// keep the later ones waiting.
#[tokio::test]
async fn the_risk_sweep_walks_its_cursor_through_more_jobs_than_a_batch() {
    use std::sync::atomic::{AtomicI64, Ordering};
    let w = World::new().await;
    for (i, head) in ['1', '2', '3'].iter().enumerate() {
        let mut job = forge_db::checks::risk::risk_job(
            &forge_db::checks::risk::RiskRequest {
                project: w.project,
                head: oid(&head.to_string().repeat(40)),
            },
            "mr1",
            RequestId::parse(&format!("req-walk-{i}")).expect("id"),
            None,
        )
        .expect("job");
        job.max_attempts = forge_db::jobs::MaxAttempts::new(1).expect("attempts");
        forge_db::jobs::enqueue(&w.t.db, &job)
            .await
            .expect("enqueue");
    }
    w.t.exec_superuser(
        "UPDATE forge.jobs SET state = 'dead', finished_at = now(), \
         last_error_code = 'FG-DB-0007' WHERE kind = 'check.risk'",
    )
    .await;
    let cursor = AtomicI64::new(0);
    let first = crate::worker::risk_sweep::sweep(&w.t.db, &cursor, 2)
        .await
        .expect("sweep");
    assert_eq!(first, crate::worker::supervise::Next::Immediately);
    assert!(cursor.load(Ordering::Relaxed) > 0, "the cursor moved on");
    let second = crate::worker::risk_sweep::sweep(&w.t.db, &cursor, 2)
        .await
        .expect("sweep");
    assert_eq!(second, crate::worker::supervise::Next::AfterPeriod);
    assert_eq!(cursor.load(Ordering::Relaxed), 0, "wrapped");
    let answered =
        w.t.json_rows("SELECT count(*) AS n FROM forge.check_runs WHERE name = 'forge/risk'")
            .await;
    assert_eq!(answered[0]["n"], 3, "{answered:?}");
    w.t.drop_now().await;
}

// ── M3 X4: `forge/spec-lint` and `forge/spec-trace` ──

/// A commit of nested `files` (`dir/name` paths) on top of `parent`.
fn commit_files(repo: &Path, parent: Option<&str>, files: &[(&str, &str)]) -> String {
    use std::collections::BTreeMap;
    #[derive(Default)]
    struct Dir {
        files: BTreeMap<String, String>,
        dirs: BTreeMap<String, Dir>,
    }
    fn write(repo: &Path, d: &Dir) -> String {
        let mut listing = String::new();
        for (name, sub) in &d.dirs {
            listing.push_str(&format!("040000 tree {}\t{name}\n", write(repo, sub)));
        }
        for (name, blob) in &d.files {
            listing.push_str(&format!("100644 blob {blob}\t{name}\n"));
        }
        plumb(repo, &["mktree"], Some(listing.as_bytes()))
    }
    let mut root = Dir::default();
    for (path, content) in files {
        let blob = plumb(
            repo,
            &["hash-object", "-w", "--stdin"],
            Some(content.as_bytes()),
        );
        let mut segs: Vec<&str> = path.split('/').collect();
        let name = segs.pop().expect("name");
        let mut d = &mut root;
        for s in segs {
            d = d.dirs.entry(s.to_owned()).or_default();
        }
        d.files.insert(name.to_owned(), blob);
    }
    let tree = write(repo, &root);
    let mut args = vec!["commit-tree", tree.as_str(), "-m", "synthetic"];
    if let Some(p) = parent {
        args.extend(["-p", p]);
    }
    plumb(repo, &args, None)
}

const SPEC_HEAD: &str = "---\nspec: config\ntitle: Config\nstatus: approved\n---\n";

async fn spec_runs(w: &World, head: &str) -> Vec<forge_db::checks::StoredCheckRun> {
    forge_db::checks::check_runs(&w.t.db, w.project, &oid(head))
        .await
        .expect("runs")
        .into_iter()
        .filter(|r| {
            [
                forge_core::checks::SPEC_LINT_CHECK,
                forge_core::checks::SPEC_TRACE_CHECK,
            ]
            .contains(&r.name.as_str())
        })
        .collect()
}

/// Every merge request version gets its `forge/spec-lint` and
/// `forge/spec-trace` runs through the production subscriber
/// (`spec-index`) and job handler (`check.spec_index`), compared with the
/// merge base: a changed requirement without a test is `untested`
/// (advisory: `neutral`), a new one with a tagged test is `tested`.
#[tokio::test]
async fn a_merge_request_version_gets_its_spec_lint_and_trace_runs() {
    let mut w = World::new().await;
    let base = commit_files(
        &w.path,
        None,
        &[(
            "spec/config.md",
            &format!("{SPEC_HEAD}::: req ZEVA-CFG-1\nold\n:::\n"),
        )],
    );
    w.push("main", Some(&base)).await;
    let head = commit_files(
        &w.path,
        Some(&base),
        &[
            (
                "spec/config.md",
                &format!(
                    "{SPEC_HEAD}::: req ZEVA-CFG-1\nnew\n:::\n::: req ZEVA-CFG-2\nadded\n:::\n"
                ),
            ),
            ("tests/reload.rs", "// @req ZEVA-CFG-2\nfn t() {}\n"),
        ],
    );
    w.push("feature", Some(&head)).await;
    let _mr = w.open("feature", "main").await;
    w.drain().await;
    let runs = spec_runs(&w, &head).await;
    assert_eq!(runs.len(), 2, "{runs:?}");
    let lint = &runs[0];
    assert_eq!(lint.name.as_str(), forge_core::checks::SPEC_LINT_CHECK);
    assert_eq!(
        lint.conclusion,
        Some(forge_core::CheckConclusion::Success),
        "{}",
        lint.summary
    );
    assert_eq!(lint.reported_by, None);
    let trace = &runs[1];
    assert_eq!(
        trace.conclusion,
        Some(forge_core::CheckConclusion::Neutral),
        "{}",
        trace.summary
    );
    assert!(
        trace
            .summary
            .contains("untested ZEVA-CFG-1 (spec_changed) spec/config.md:6"),
        "{}",
        trace.summary
    );
    assert!(
        trace.summary.contains("tested ZEVA-CFG-2"),
        "{}",
        trace.summary
    );
    assert!(
        trace.summary.contains(&format!("merge base {base}")),
        "{}",
        trace.summary
    );
    w.t.drop_now().await;
}

/// A broken specification fails spec-lint on the head.
#[tokio::test]
async fn a_broken_specification_fails_spec_lint() {
    let mut w = World::new().await;
    let base = commit_files(&w.path, None, &[("README", "base\n")]);
    w.push("main", Some(&base)).await;
    let head = commit_files(
        &w.path,
        Some(&base),
        &[(
            "spec/config.md",
            &format!("{SPEC_HEAD}::: req ZEVA-CFG-1\nx\n:::\n::: req ZEVA-CFG-1\ny\n:::\n"),
        )],
    );
    w.push("feature", Some(&head)).await;
    let _mr = w.open("feature", "main").await;
    w.drain().await;
    let runs = spec_runs(&w, &head).await;
    assert_eq!(
        runs[0].conclusion,
        Some(forge_core::CheckConclusion::Failure),
        "{}",
        runs[0].summary
    );
    assert!(
        runs[0].summary.contains("duplicate_id"),
        "{}",
        runs[0].summary
    );
    assert_eq!(runs[0].annotations.len(), 1);
    w.t.drop_now().await;
}

fn missing_spec_index_job(w: &World, attempts: u8, request: &str) -> forge_db::jobs::NewJob {
    let mut job = forge_db::checks::spec_index::spec_index_job(
        &forge_db::checks::spec_index::SpecIndexRequest {
            project: w.project,
            iid: 1,
            head: oid(&"d".repeat(40)),
        },
        None,
        RequestId::parse(request).expect("id"),
        None,
    )
    .expect("job");
    job.max_attempts = forge_db::jobs::MaxAttempts::new(attempts).expect("attempts");
    job
}

/// The handler's last failure records `error` for both checks; a retried
/// attempt writes nothing.
#[tokio::test]
async fn a_spec_index_job_that_dies_answers_error_for_both_checks() {
    let mut w = World::new().await;
    let base = commit_files(&w.path, None, &[("README", "base\n")]);
    w.push("main", Some(&base)).await;
    w.push("feature", Some(&base)).await;
    let _mr = w.open("feature", "main").await;
    w.drain().await;
    let job = missing_spec_index_job(&w, 2, "req-index-dies");
    forge_db::jobs::enqueue(&w.t.db, &job)
        .await
        .expect("enqueue");
    let first = w.runner.run_once().await.expect("runner").expect("ran");
    assert!(
        matches!(first, forge_db::jobs::Ran::Retrying { .. }),
        "{first:?}"
    );
    assert!(spec_runs(&w, &"d".repeat(40)).await.is_empty());
    w.t.exec_superuser("UPDATE forge.jobs SET run_after = now() WHERE kind = 'check.spec_index'")
        .await;
    let second = w.runner.run_once().await.expect("runner").expect("ran");
    assert!(
        matches!(second, forge_db::jobs::Ran::Dead { .. }),
        "{second:?}"
    );
    let runs = spec_runs(&w, &"d".repeat(40)).await;
    assert_eq!(runs.len(), 2, "{runs:?}");
    for run in &runs {
        assert_eq!(run.conclusion, Some(forge_core::CheckConclusion::Error));
        assert!(run.summary.contains("req-index-dies"), "{}", run.summary);
    }
    w.t.drop_now().await;
}

/// A job the reaper ended has no answer; the sweep writes `error` for both
/// checks once.
#[tokio::test]
async fn the_sweep_answers_a_reaped_spec_index_job_once() {
    let w = World::new().await;
    let job = missing_spec_index_job(&w, 1, "req-index-crash");
    forge_db::jobs::enqueue(&w.t.db, &job)
        .await
        .expect("enqueue");
    let kind = forge_db::jobs::JobKind::parse("check.spec_index").expect("kind");
    let worker = forge_db::jobs::WorkerId::generate();
    let claimed = forge_db::jobs::claim(&w.t.db, &worker, &[kind])
        .await
        .expect("claim")
        .expect("a job");
    w.t.exec_superuser(&format!(
        "UPDATE forge.jobs SET lease_until = now() - interval '1 second' WHERE id = {}",
        claimed.lease.job_id.get()
    ))
    .await;
    let reaped = forge_db::jobs::reap(&w.t.db).await.expect("reap");
    assert_eq!(reaped.len(), 1);
    assert!(spec_runs(&w, &"d".repeat(40)).await.is_empty());
    crate::worker::spec_index_sweep::run_spec_index_sweep(&w.t.db)
        .await
        .expect("sweep");
    let runs = spec_runs(&w, &"d".repeat(40)).await;
    assert_eq!(runs.len(), 2, "{runs:?}");
    for run in &runs {
        assert_eq!(run.conclusion, Some(forge_core::CheckConclusion::Error));
        assert!(run.summary.contains("FG-DB-0007"), "{}", run.summary);
        assert!(run.summary.contains("req-index-crash"), "{}", run.summary);
    }
    crate::worker::spec_index_sweep::run_spec_index_sweep(&w.t.db)
        .await
        .expect("sweep again");
    assert!(spec_runs(&w, &"d".repeat(40))
        .await
        .iter()
        .all(|r| r.revision == 1));
    w.t.drop_now().await;
}
