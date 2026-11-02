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
        forge_auth::Principal::authenticated(self.alice, false, true, Credential::Session)
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

#[tokio::test]
async fn the_worker_keeps_mergeability_current_through_every_trigger() {
    let mut w = World::new().await;
    let base = commit_on(&w.path, None, &[("README", "base\n")]);
    // The head adds `feature.txt`; the conflicting target adds it too.
    let c1 = commit_on(
        &w.path,
        Some(&base),
        &[("README", "base\n"), ("feature.txt", "head\n")],
    );
    let clash = commit_on(
        &w.path,
        Some(&base),
        &[("README", "base\n"), ("feature.txt", "target\n")],
    );
    plumb(&w.path, &["update-ref", "refs/heads/main", &base], None);
    plumb(&w.path, &["update-ref", "refs/heads/feature", &c1], None);

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
    assert_eq!(s.reason.as_deref(), Some("conflict"));
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
    assert_eq!(s.reason.as_deref(), Some("unrelated_histories"));

    // The target is deleted: checked again, `target_missing`.
    w.push("release", None).await;
    let s = w.status(&mr).await;
    assert_eq!(s.status, MergeabilityStatus::CheckFailed, "{s:?}");
    assert_eq!(s.reason.as_deref(), Some("target_missing"));
    // Undecided statuses never flip.
    assert_eq!(w.flips().await.len(), 4);
    w.t.drop_now().await;
}
