#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! The conflict scan (M3 X6, plan 04 C-2) against a real bare repository
//! and the development database. Merge requests are opened through
//! forge-review and their versions recorded by the version job
//! (`forge_review::refresh`), which also requests the scan; the scan then
//! runs `merge-base`, `diff-tree` and `merge-tree` with the real git:
//! the same lines changed on both sides are predicted (drafts included,
//! one event per side), disjoint paths never reach a trial merge, the same
//! file in other hunks is clean, a conflict with the target alone is not
//! the pair's, and the caps and timeouts make a scan `incomplete`.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use common::*;
use forge_core::{ProjectId, RepoId, RequestId, UserId};
use forge_db::repositories;
use forge_db::review::{
    conflict_predictions, conflict_scan, ConflictScanRequest, PairOutcome, RefreshRequest,
    ScanStatus,
};
use forge_db::test_support::FreshDb;
use forge_errors::ErrorCode;
use forge_git::browse::{BrowseLimits, Browser};
use forge_git::mergeability::MergeTreeLimits;
use forge_git::{Git, GitConfig, StorageRoot};
use forge_review::{
    refresh, scan, ConflictPredictor, NewFields, Review, ScanLimits, StoredMergeRequest,
    VersionRefs,
};

fn plumb(repo: &Path, args: &[&str], input: Option<&[u8]>) -> String {
    use std::io::Write as _;
    let mut child = Command::new("git")
        .arg("--git-dir")
        .arg(repo)
        .args(args)
        .env("GIT_AUTHOR_NAME", "forge-test")
        .env("GIT_AUTHOR_EMAIL", "forge-test@example.invalid")
        .env("GIT_COMMITTER_NAME", "forge-test")
        .env("GIT_COMMITTER_EMAIL", "forge-test@example.invalid")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("git");
    let mut stdin = child.stdin.take().expect("stdin");
    if let Some(input) = input {
        stdin.write_all(input).expect("write");
    }
    drop(stdin);
    let out = child.wait_with_output().expect("wait");
    assert!(out.status.success(), "git {args:?}");
    String::from_utf8(out.stdout)
        .expect("utf8")
        .trim()
        .to_owned()
}

/// Twenty numbered lines (the base content of `a.txt` and `b.txt`).
fn lines() -> Vec<String> {
    (1..=20).map(|n| format!("line {n}")).collect()
}

fn with_line(n: usize, text: &str) -> String {
    let mut l = lines();
    l[n - 1] = text.to_owned();
    l.join("\n") + "\n"
}

/// A commit of the flat `files` on top of `parent`.
fn commit(repo: &Path, parent: Option<&str>, files: &[(&str, String)]) -> String {
    let mut sorted: Vec<&(&str, String)> = files.iter().collect();
    sorted.sort_by_key(|(name, _)| *name);
    let mut listing = String::new();
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

struct World {
    t: FreshDb,
    _home: tempfile::TempDir,
    _root: tempfile::TempDir,
    path: PathBuf,
    git: Git,
    storage: StorageRoot,
    owner: UserId,
    project: ProjectId,
    repo: RepoId,
    review: Review,
    refs: VersionRefs,
    /// The base files on `main`.
    base: Vec<(&'static str, String)>,
    base_commit: String,
}

async fn world() -> World {
    let t = FreshDb::migrated().await;
    let owner = forge_db::users::create(
        &t.db,
        &system("req-user-olga"),
        &forge_db::users::NewUser::human("olga", "olga", "olga@example.test"),
    )
    .await
    .expect("user")
    .user_id;
    let web = project(&t, owner, &[]).await;
    let _marked = repositories::mark_ready(&t.db, &system("req-ready"), web.repository_id)
        .await
        .expect("ready");
    let home = tempfile::tempdir().expect("home");
    let root = tempfile::tempdir().expect("root");
    let git = Git::new(GitConfig::new("git", home.path()));
    let storage = StorageRoot::open(root.path()).expect("root");
    let path = storage
        .init_bare(&git, web.repository_id, uuid::Uuid::new_v4(), None)
        .await
        .expect("init");
    let base = vec![
        ("a.txt", lines().join("\n") + "\n"),
        ("b.txt", lines().join("\n") + "\n"),
    ];
    let base_commit = commit(&path, None, &base);
    plumb(
        &path,
        &["update-ref", "refs/heads/main", &base_commit],
        None,
    );
    let browser = Browser::new(git.clone(), storage.clone(), BrowseLimits::default());
    let refs = VersionRefs::new(git.clone(), storage.clone()).with_browser(browser.clone());
    let review = Review::new(t.db.clone(), Arc::new(browser)).with_version_refs(refs.clone());
    World {
        t,
        _home: home,
        _root: root,
        path,
        git,
        storage,
        owner,
        project: web.project_id,
        repo: web.repository_id,
        review,
        refs,
        base,
        base_commit,
    }
}

impl World {
    /// The base files with `changes` applied (a new file is added).
    fn files(&self, changes: &[(&'static str, String)]) -> Vec<(&'static str, String)> {
        let mut files = self.base.clone();
        for (name, content) in changes {
            match files.iter_mut().find(|(n, _)| n == name) {
                Some(f) => f.1.clone_from(content),
                None => files.push((name, content.clone())),
            }
        }
        files
    }

    /// Branch `name` at one commit on `parent` with `changes`, then a merge
    /// request onto `main` (`draft` as given) and its first version; the
    /// scan the version job requested, if any.
    async fn mr(
        &self,
        name: &str,
        parent: &str,
        changes: &[(&'static str, String)],
        draft: bool,
    ) -> (StoredMergeRequest, Option<i64>) {
        let head = commit(&self.path, Some(parent), &self.files(changes));
        plumb(
            &self.path,
            &["update-ref", &format!("refs/heads/{name}"), &head],
            None,
        );
        let proof = open_proof(
            &self.t,
            &session(self.owner),
            self.project,
            (name, "main"),
            draft,
        )
        .await
        .expect("open proof");
        let mr = self
            .review
            .open(
                &request(&format!("req-open-{name}")),
                &proof,
                ready(self.repo),
                &NewFields {
                    title: format!("Change {name}"),
                    description: String::new(),
                },
            )
            .await
            .expect("open");
        let report = refresh(
            &self.t.db,
            &self.refs,
            RefreshRequest {
                merge_request: mr.id,
                pushed_by: None,
            },
            &RequestId::parse(&format!("req-v-{name}")).expect("id"),
        )
        .await
        .expect("version");
        assert_eq!(report.outcome, "added");
        (mr, report.scan_requested)
    }

    fn predictor(&self, limits: ScanLimits) -> ConflictPredictor {
        // clone: the predictor owns its handles
        ConflictPredictor::new(self.git.clone(), self.storage.clone()).with_limits(limits)
    }

    async fn run(
        &self,
        mr: &StoredMergeRequest,
        seq: Option<i64>,
        limits: ScanLimits,
    ) -> Result<forge_review::ScanReport, forge_errors::ForgeError> {
        scan(
            &self.t.db,
            &self.predictor(limits),
            ConflictScanRequest {
                merge_request: mr.id,
                scan_seq: seq.expect("the version job requested a scan"),
            },
            &RequestId::parse(&format!("req-scan-{}", mr.iid)).expect("id"),
        )
        .await
    }

    async fn pair(&self, a: &StoredMergeRequest, b: &StoredMergeRequest) -> (String, Vec<String>) {
        let lo = a.id.get().min(b.id.get());
        let hi = a.id.get().max(b.id.get());
        let rows = self
            .t
            .json_rows(&format!(
                "SELECT outcome, conflicts FROM forge.merge_request_conflict_pairs \
                 WHERE low_mr_id = {lo} AND high_mr_id = {hi}"
            ))
            .await;
        let row = rows.first().expect("pair row");
        let conflicts = row["conflicts"]
            .as_array()
            .expect("conflicts")
            .iter()
            .map(|c| c.as_str().expect("path").to_owned())
            .collect();
        (
            row["outcome"].as_str().expect("outcome").to_owned(),
            conflicts,
        )
    }

    async fn predicted_events(&self) -> Vec<(i64, serde_json::Value)> {
        self.t
            .json_rows(
                "SELECT aggregate_id, payload FROM forge.events \
                 WHERE event_type = 'merge_request.conflict_predicted' ORDER BY id",
            )
            .await
            .into_iter()
            .map(|r| {
                (
                    r["aggregate_id"].as_i64().expect("id"),
                    r["payload"].clone(),
                )
            })
            .collect()
    }
}

/// C-2: two merge requests (one a draft) changing the same line of the same
/// file are predicted to conflict; both sides get the event and read it.
#[tokio::test]
async fn the_same_lines_changed_on_two_merge_requests_are_predicted_to_conflict() {
    let w = world().await;
    let base = w.base_commit.clone();
    let (one, seq1) = w
        .mr("one", &base, &[("a.txt", with_line(2, "one"))], false)
        .await;
    // Alone on the target: scanned, nothing to predict.
    let report = w
        .run(&one, seq1, ScanLimits::default())
        .await
        .expect("scan");
    assert_eq!(report.status, Some(ScanStatus::Scanned));
    assert_eq!(report.pairs_tried, 0);
    let (two, seq2) = w
        .mr("two", &base, &[("a.txt", with_line(2, "two"))], true)
        .await;
    let report = w
        .run(&two, seq2, ScanLimits::default())
        .await
        .expect("scan");
    assert_eq!(report.status, Some(ScanStatus::Scanned));
    assert_eq!(report.pairs_tried, 1);
    assert_eq!(report.predicted, vec![one.id]);
    assert_eq!(
        w.pair(&one, &two).await,
        ("conflict".to_owned(), vec!["a.txt".to_owned()])
    );
    let events = w.predicted_events().await;
    let sides: Vec<i64> = events.iter().map(|e| e.0).collect();
    assert_eq!(sides, vec![two.id.get(), one.id.get()]);
    assert_eq!(events[0].1["other_iid"], one.iid);
    assert_eq!(events[1].1["other_iid"], two.iid);
    // The draft is predicted against too, and both read the prediction.
    for (me, other) in [(&one, &two), (&two, &one)] {
        let read = conflict_predictions(&w.t.db, me.id).await.expect("read");
        assert_eq!(read.len(), 1, "{read:?}");
        assert_eq!(read[0].other, other.id);
        assert_eq!(read[0].conflicts, ["a.txt"]);
    }
    let (scan_row, preds) = w.review.conflicts(two.id).await.expect("conflicts");
    assert_eq!(scan_row.expect("row").status, ScanStatus::Scanned);
    assert_eq!((preds.len(), preds[0].other_draft), (1, false));
    let (_, preds) = w.review.conflicts(one.id).await.expect("conflicts");
    assert!(preds[0].other_draft, "the draft is the other side");
    // The same head again: the version job does not ask for another scan.
    let again = refresh(
        &w.t.db,
        &w.refs,
        RefreshRequest {
            merge_request: two.id,
            pushed_by: None,
        },
        &RequestId::parse("req-v-two-again").expect("id"),
    )
    .await
    .expect("refresh");
    assert_eq!(again.scan_requested, None);
    w.t.drop_now().await;
}

/// Narrow first: merge requests changing different files are `no_overlap`
/// without a trial merge; the same file in hunks far apart is `clean`.
#[tokio::test]
async fn disjoint_paths_are_no_overlap_and_other_hunks_are_clean() {
    let w = world().await;
    let base = w.base_commit.clone();
    let (one, _) = w
        .mr("one", &base, &[("a.txt", with_line(1, "one"))], false)
        .await;
    let (two, _) = w
        .mr("two", &base, &[("c.txt", "new\n".to_owned())], false)
        .await;
    let (three, seq3) = w
        .mr("three", &base, &[("a.txt", with_line(20, "three"))], false)
        .await;
    let report = w
        .run(&three, seq3, ScanLimits::default())
        .await
        .expect("scan");
    assert_eq!(report.status, Some(ScanStatus::Scanned));
    assert_eq!(report.pairs_tried, 1, "only the pair sharing a.txt");
    assert!(report.predicted.is_empty());
    assert_eq!(w.pair(&three, &two).await.0, "no_overlap");
    assert_eq!(w.pair(&three, &one).await.0, "clean");
    assert!(w.predicted_events().await.is_empty());
    w.t.drop_now().await;
}

/// A conflict that comes from the target alone (one side is based on a
/// newer target that changed the line the other changes) is that side's
/// conflict with the target (C-1), not a conflict between the two.
#[tokio::test]
async fn a_conflict_with_the_target_alone_is_not_predicted_between_the_two() {
    let w = world().await;
    let base = w.base_commit.clone();
    let (one, _) = w
        .mr(
            "one",
            &base,
            &[
                ("a.txt", with_line(2, "one")),
                ("b.txt", with_line(1, "one")),
            ],
            false,
        )
        .await;
    // The target moves: line 2 changes on main.
    let moved = commit(
        &w.path,
        Some(&base),
        &w.files(&[("a.txt", with_line(2, "main"))]),
    );
    plumb(&w.path, &["update-ref", "refs/heads/main", &moved], None);
    // Two is based on the new target and changes b.txt only, elsewhere.
    let two_files = w.files(&[
        ("a.txt", with_line(2, "main")),
        ("b.txt", with_line(20, "two")),
    ]);
    let head = commit(&w.path, Some(&moved), &two_files);
    plumb(&w.path, &["update-ref", "refs/heads/two", &head], None);
    let proof = open_proof(&w.t, &session(w.owner), w.project, ("two", "main"), false)
        .await
        .expect("proof");
    let two = w
        .review
        .open(
            &request("req-open-two"),
            &proof,
            ready(w.repo),
            &NewFields {
                title: "Two".to_owned(),
                description: String::new(),
            },
        )
        .await
        .expect("open");
    let report = refresh(
        &w.t.db,
        &w.refs,
        RefreshRequest {
            merge_request: two.id,
            pushed_by: None,
        },
        &RequestId::parse("req-v-two").expect("id"),
    )
    .await
    .expect("version");
    let report = w
        .run(&two, report.scan_requested, ScanLimits::default())
        .await
        .expect("scan");
    assert_eq!(report.pairs_tried, 1, "both change b.txt");
    assert!(report.predicted.is_empty(), "{report:?}");
    assert_eq!(w.pair(&one, &two).await.0, "clean");
    w.t.drop_now().await;
}

/// Bounds: more candidates than `pairs_max` make the scan `incomplete
/// pairs_capped` (the pair tried still predicts); FG-REVIEW-0022 is the
/// code of the log line.
#[tokio::test]
async fn a_scan_with_more_candidates_than_the_cap_is_incomplete() {
    let w = world().await;
    let base = w.base_commit.clone();
    let (one, _) = w
        .mr("one", &base, &[("a.txt", with_line(2, "one"))], false)
        .await;
    let (two, _) = w
        .mr("two", &base, &[("a.txt", with_line(2, "two"))], false)
        .await;
    let (three, seq3) = w
        .mr("three", &base, &[("a.txt", with_line(2, "three"))], false)
        .await;
    let limits = ScanLimits {
        pairs_max: 1,
        ..ScanLimits::default()
    };
    let report = w.run(&three, seq3, limits).await.expect("scan");
    assert_eq!(
        (report.status, report.reason),
        (Some(ScanStatus::Incomplete), Some("pairs_capped"))
    );
    assert_eq!((report.pairs_tried, report.pairs_skipped), (1, 1));
    assert_eq!(report.predicted.len(), 1);
    let row = conflict_scan(&w.t.db, three.id)
        .await
        .expect("read")
        .expect("row");
    assert_eq!(
        (row.status, row.reason.as_deref(), row.pairs_skipped),
        (ScanStatus::Incomplete, Some("pairs_capped"), 1)
    );
    // peers_max below the number of peers: incomplete peers_capped.
    let (four, seq4) = w
        .mr("four", &base, &[("c.txt", "four\n".to_owned())], false)
        .await;
    let limits = ScanLimits {
        peers_max: 1,
        ..ScanLimits::default()
    };
    let report = w.run(&four, seq4, limits).await.expect("scan");
    assert_eq!(report.reason, Some("peers_capped"));
    let _ = (one, two);
    w.t.drop_now().await;
}

/// A git timeout on a pair is stored on the pair (`failed git_timeout`),
/// the scan is `incomplete pair_failed`, and the error is returned for the
/// worker's retry — never "no conflict".
#[tokio::test]
async fn a_git_timeout_on_a_pair_is_stored_and_returned() {
    let w = world().await;
    let base = w.base_commit.clone();
    let (one, _) = w
        .mr("one", &base, &[("a.txt", with_line(2, "one"))], false)
        .await;
    let (two, seq2) = w
        .mr("two", &base, &[("a.txt", with_line(2, "two"))], false)
        .await;
    let limits = ScanLimits {
        merge_tree: MergeTreeLimits {
            timeout: Duration::ZERO,
            ..MergeTreeLimits::default()
        },
        ..ScanLimits::default()
    };
    let err = w.run(&two, seq2, limits).await.expect_err("timeout");
    assert_eq!(err.code(), ErrorCode::GitTimeout, "{err}");
    let row = conflict_scan(&w.t.db, two.id)
        .await
        .expect("read")
        .expect("row");
    assert_eq!(
        (row.status, row.reason.as_deref()),
        (ScanStatus::Incomplete, Some("pair_failed"))
    );
    let lo = one.id.get().min(two.id.get());
    let rows =
        w.t.json_rows(&format!(
            "SELECT outcome, reason FROM forge.merge_request_conflict_pairs WHERE low_mr_id = {lo}"
        ))
        .await;
    let outcome = rows[0]["outcome"].as_str().expect("outcome").to_owned();
    assert_eq!(
        (outcome.as_str(), rows[0]["reason"].as_str()),
        ("failed", Some("git_timeout"))
    );
    assert_eq!(
        PairOutcome::parse(&outcome),
        Some(PairOutcome::Failed),
        "a failed pair is shown, never as no conflict"
    );
    w.t.drop_now().await;
}
