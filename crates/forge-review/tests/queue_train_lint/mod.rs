//! The merge queue's train lint (plan 18 S1, ADR-0037 (b)) against a real
//! bare repository, the development database and forge-checks' real
//! `TrainLint`, as `forge serve` wires it: two merge requests that each
//! pass `forge/doc-lint` alone and break a reference together never both
//! land; the first bad step leaves with `FG-CI-0032` and the rest land in
//! order; a `main` with old problems still moves; a strict repository does
//! not; a train that cannot be linted never lands (`FG-DOC-0002
//! scope=train`, the third time `FG-CI-0024 reason=doc_lint_unavailable`).
//! `repo_config`: an invalid `.forge/repo.yml` (`FG-REPO-0024`) never
//! wedges the repository. `edges`: a bad step in the middle, the gate off,
//! the skip, a bound.
//!
//! The documents are in a repository without a CI definition, so a train
//! is green at once and the lint is what decides.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};

use forge_checks::train_lint::{lint_commit, TrainLint};
use forge_core::checks::DOC_LINT_CHECK;
use forge_core::train_lint::{TrainDocLint, TrainLintFuture, TrainLintInput};
use forge_db::checks::{check_runs, StoredCheckRun};
use forge_errors::ForgeError;

use super::*;

mod edges;
mod repo_config;

const MANUAL_T: &str = "name: qms.manual\nid_pattern: '^QM-\\d{3}$'\n";
const QM_001: &str =
    "---\ndoctype: qms.manual\nid: QM-001\ntitle: 品質マニュアル\nowner: qa-team\n---\n\n本文。\n";
/// QM-001 citing QM-002 (line 8).
const QM_001_CITING_002: &str = "---\ndoctype: qms.manual\nid: QM-001\ntitle: 品質マニュアル\nowner: qa-team\n---\n\n本文。[[QM-002]]\n";
/// QM-001 citing a document nobody has (line 8).
const QM_001_CITING_404: &str = "---\ndoctype: qms.manual\nid: QM-001\ntitle: 品質マニュアル\nowner: qa-team\n---\n\n本文。[[QM-404]]\n";
const QM_002: &str =
    "---\ndoctype: qms.manual\nid: QM-002\ntitle: 検査手順\nowner: qa-team\n---\n\n本文。\n";
const QM_003: &str =
    "---\ndoctype: qms.manual\nid: QM-003\ntitle: 是正処置\nowner: qa-team\n---\n\n本文。\n";
const QM_004: &str =
    "---\ndoctype: qms.manual\nid: QM-004\ntitle: 文書管理\nowner: qa-team\n---\n\n本文。\n";
const STRICT: &str = "merge_queue:\n  doc_lint_strict: true\n";

/// A clean `main`: two documents of one type, no CI definition.
const CLEAN_BASE: &[(&str, &str)] = &[
    ("README.md", "base\n"),
    (".forge/doctypes/qms.manual.yml", MANUAL_T),
    ("qms/QM-001.md", QM_001),
    ("qms/QM-002.md", QM_002),
];
/// A `main` that already has a problem: QM-001 cites QM-404.
const OLD_BASE: &[(&str, &str)] = &[
    ("README.md", "base\n"),
    (".forge/doctypes/qms.manual.yml", MANUAL_T),
    ("qms/QM-001.md", QM_001_CITING_404),
];
/// [`OLD_BASE`] in a repository that says it must be clean.
const STRICT_BASE: &[(&str, &str)] = &[
    ("README.md", "base\n"),
    (".forge/doctypes/qms.manual.yml", MANUAL_T),
    (".forge/repo.yml", STRICT),
    ("qms/QM-001.md", QM_001_CITING_404),
];

/// A commit holding exactly `files` (paths of any depth) on `parent`.
fn nested_commit(repo: &Path, files: &[(&str, &str)], parent: Option<&str>) -> String {
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

impl World {
    /// The queue's worker with forge-checks' train lint, as `forge serve`
    /// builds it.
    fn lint_worker(&self, name: &str) -> MergeQueue {
        self.worker(name).with_train_lint(Arc::new(TrainLint::new(
            self.t.db.clone(),
            self.git.clone(),
            self.storage.clone(),
            self.browser.clone(),
        )))
    }

    /// Branch `name` one commit ahead of the base — the base's files with
    /// `changes` (`None`: the file is deleted) — and its merge request.
    async fn doc_mr(&mut self, name: &str, changes: &[(&str, Option<&str>)]) -> StoredMergeRequest {
        let mut all: BTreeMap<&str, &str> = self.base_files.iter().copied().collect();
        for &(path, content) in changes {
            match content {
                Some(text) => all.insert(path, text),
                None => all.remove(path),
            };
        }
        let files: Vec<(&str, &str)> = all.into_iter().collect();
        let head = nested_commit(&self.path, &files, Some(&self.base));
        self.mr_at(name, &head).await
    }

    /// `main` restarts at a commit of `files` (a tree the base fixture's
    /// builder cannot write: two paths under `.forge/`).
    fn restart_main(&mut self, files: &'static [(&'static str, &'static str)]) {
        let base = nested_commit(&self.path, files, None);
        plumb(&self.path, &["update-ref", "refs/heads/main", &base], None);
        self.browser.invalidate(self.repo);
        self.base = base;
        self.base_files = files;
    }

    /// `main`'s first-parent commits, oldest first.
    fn first_parents(&self) -> Vec<String> {
        let chain = plumb(
            &self.path,
            &["rev-list", "--first-parent", "--reverse", "refs/heads/main"],
            None,
        );
        chain.lines().map(str::to_owned).collect()
    }

    /// Whether `file` is in the tree of `commit`.
    fn has(&self, commit: &str, file: &str) -> bool {
        Command::new("git")
            .arg("--git-dir")
            .arg(&self.path)
            .args(["cat-file", "-e", &format!("{commit}:{file}")])
            .output()
            .expect("cat-file")
            .status
            .success()
    }

    /// `file` of `main`.
    fn main_file(&self, file: &str) -> String {
        let out = Command::new("git")
            .arg("--git-dir")
            .arg(&self.path)
            .args(["cat-file", "blob", &format!("refs/heads/main:{file}")])
            .output()
            .expect("cat-file");
        assert!(out.status.success(), "{file} on main");
        String::from_utf8(out.stdout).expect("utf8")
    }

    /// The document problems of `commit`'s tree, as `path:rule:id`.
    async fn problems_at(&self, commit: &str) -> Vec<String> {
        let reports = lint_commit(&self.browser, self.repo, &oid(commit))
            .await
            .expect("lint");
        reports
            .iter()
            .flat_map(|r| {
                r.problems.iter().map(|p| {
                    format!(
                        "{}:{}:{}",
                        r.path,
                        p.rule,
                        p.subject.as_deref().unwrap_or("")
                    )
                })
            })
            .collect()
    }

    /// The `forge/doc-lint` run recorded on `commit`.
    async fn doc_lint_run(&self, commit: &str) -> Option<StoredCheckRun> {
        check_runs(&self.t.db, self.project, &oid(commit))
            .await
            .expect("runs")
            .into_iter()
            .find(|r| r.name.as_str() == DOC_LINT_CHECK)
    }

    /// The queue's batches in the order they were built:
    /// `(state, reason, error_code, train_sha)`.
    async fn batches(&self) -> Vec<(String, String, String, String)> {
        let text = |v: &serde_json::Value| v.as_str().unwrap_or("").to_owned();
        self.t
            .json_rows(
                "SELECT state, reason, error_code, train_sha FROM forge.merge_queue_batches \
                 ORDER BY id",
            )
            .await
            .iter()
            .map(|r| {
                (
                    text(&r["state"]),
                    text(&r["reason"]),
                    text(&r["error_code"]),
                    text(&r["train_sha"]),
                )
            })
            .collect()
    }
}

/// `mr`'s entry left the queue with `FG-CI-0032` and `reason`; the merge
/// request is still open.
async fn assert_removed_by_the_lint(w: &World, mr: &StoredMergeRequest, reason: &str) {
    let entry = w.entry_of(mr).await;
    assert_eq!(entry.state, EntryState::Removed, "!{}", mr.iid);
    assert_eq!(entry.reason.as_deref(), Some(reason), "!{}", mr.iid);
    assert_eq!(
        entry.error_code.as_deref(),
        Some(ErrorCode::MergeQueueDocLintFailed.id()),
        "!{}",
        mr.iid
    );
    assert_eq!(w.state_of(mr).await, forge_core::MergeRequestState::Open);
}

async fn assert_merged(w: &World, mr: &StoredMergeRequest) {
    assert_eq!(
        w.entry_of(mr).await.state,
        EntryState::Merged,
        "!{}",
        mr.iid
    );
    assert_eq!(w.state_of(mr).await, forge_core::MergeRequestState::Merged);
}

/// Load test 2026-11-19 (scenario a-xref): a person's entry the train
/// lint removed (`doc_lint_failed`, FG-CI-0032) keeps that head away from
/// auto-merge too — the head and the target prove it, not who enqueued.
/// Before, auto-merge enqueued the same head again, the train was built
/// and linted a second time, and only then did auto-merge stop.
#[tokio::test]
async fn a_person_entry_removed_by_the_train_lint_stops_auto_merge_on_that_head() {
    let mut w = world_with(CLEAN_BASE).await;
    let deletes = w.doc_mr("fa", &[("qms/QM-002.md", None)]).await;
    let cites = w
        .doc_mr("fb", &[("qms/QM-001.md", Some(QM_001_CITING_002))])
        .await;
    w.enqueue(&deletes).await.expect("enqueue");
    w.enqueue(&cites).await.expect("enqueue");
    w.drive(&w.lint_worker("w1"), &BTreeSet::new()).await;
    assert_merged(&w, &deletes).await;
    assert_removed_by_the_lint(&w, &cites, "doc_lint_failed").await;
    // On its own the head is clear and mergeable into the new tip, as
    // auto-merge sees it: forge/risk and forge/doc-lint succeed.
    w.checked(cites.id).await;
    w.risk(&cites, CheckConclusion::Success, "No risk found")
        .await;

    let report = w.auto().tick().await.expect("tick");
    assert_eq!(report.queued, 0, "{report:?}");
    assert!(w.live_entry(&cites).await.is_none(), "not enqueued again");
    assert_eq!(
        w.auto_state(&cites).await,
        Some((
            "stopped".to_owned(),
            Some("program_breakage".to_owned()),
            "merge_queue_doc_lint_failed".to_owned(),
            Some(DOC_LINT_CHECK.to_owned()),
        ))
    );
    let removals = count(
        &w.t,
        &format!(
            "SELECT count(*) AS n FROM forge.merge_queue_entries WHERE merge_request_id = {} \
             AND state = 'removed' AND error_code = 'FG-CI-0032'",
            cites.id.get()
        ),
    )
    .await;
    assert_eq!(removals, 1, "one FG-CI-0032 removal per not-merged head");
}

#[tokio::test]
async fn a_train_whose_members_break_a_reference_together_never_lands() {
    assert_eq!(ErrorCode::MergeQueueDocLintFailed.id(), "FG-CI-0032");
    let mut w = world_with(CLEAN_BASE).await;
    // Each is fine alone: one deletes QM-002, the other starts to cite it.
    let deletes = w.doc_mr("fa", &[("qms/QM-002.md", None)]).await;
    let cites = w
        .doc_mr("fb", &[("qms/QM-001.md", Some(QM_001_CITING_002))])
        .await;
    for head in [&deletes, &cites] {
        let alone = w.problems_at(head.head_sha.as_deref().expect("head")).await;
        assert!(alone.is_empty(), "green alone: {alone:?}");
    }
    w.enqueue(&deletes).await.expect("enqueue");
    w.enqueue(&cites).await.expect("enqueue");
    let worker = w.lint_worker("w1");
    let logs = Captured::default();
    let sizes = {
        let _guard = logs.install();
        w.drive(&worker, &BTreeSet::new()).await
    };
    assert_eq!(sizes, [2, 1], "the train of both, then the one left");

    // The train of both never landed: the one that cites is out, with the
    // code, and the other landed alone.
    assert_removed_by_the_lint(&w, &cites, "doc_lint_failed").await;
    assert_merged(&w, &deletes).await;
    assert!(!w.main_has("qms/QM-002.md"));
    assert_eq!(w.main_file("qms/QM-001.md"), QM_001);
    // No commit `main` ever pointed at has a broken reference.
    let chain = w.first_parents();
    assert_eq!(chain.len(), 2, "{chain:?}");
    for commit in &chain {
        let problems = w.problems_at(commit).await;
        assert!(problems.is_empty(), "{commit}: {problems:?}");
    }

    let batches = w.batches().await;
    assert_eq!(batches.len(), 2, "{batches:?}");
    let (state, reason, code, train) = &batches[0];
    assert_eq!(
        (state.as_str(), reason.as_str(), code.as_str()),
        ("failed", "doc_lint_failed", "FG-CI-0032")
    );
    assert_eq!(batches[1].0, "landed");
    // The train ref of the failed batch is gone.
    let refs = plumb(&w.path, &["for-each-ref", "refs/merge-queue/"], None);
    assert!(refs.is_empty(), "{refs}");

    // One removal, logged once: who, which step, what.
    let removed = logs.named("forge.merge_queue.entry_removed");
    assert_eq!(removed.len(), 1, "{removed:?}");
    let removed = &removed[0];
    assert_eq!(removed["error_code"], "FG-CI-0032", "{removed}");
    assert_eq!(removed["reason"], "doc_lint_failed", "{removed}");
    assert_eq!(removed["merge_request_id"], cites.id.get(), "{removed}");
    assert_eq!(removed["step"], 2, "{removed}");
    assert_eq!(removed["train_sha"], train.as_str(), "{removed}");
    assert_eq!(removed["base_sha"], w.base.as_str(), "{removed}");
    assert!(removed["elapsed_ms"].as_u64().is_some(), "{removed}");
    assert_eq!(
        removed["problems"], "ref_broken:qms/QM-001.md:QM-002",
        "{removed}"
    );
    assert!(removed["request_id"].as_str().is_some(), "{removed}");
    // An expected outcome, as the catalog says (`severity = "info"`).
    assert_eq!(logs.levels("forge.merge_queue.entry_removed"), ["INFO"]);

    // The evidence: the train's `forge/doc-lint` run, with the file and
    // the line.
    let run = w.doc_lint_run(train).await.expect("the train's run");
    assert_eq!(run.conclusion, Some(CheckConclusion::Failure));
    assert!(
        run.summary
            .contains("qms/QM-001.md:8 ref_broken — QM-002 [FG-DOC-0001]"),
        "{}",
        run.summary
    );
    // The landed tip has its run too, and it is green.
    let landed = w
        .doc_lint_run(&w.tip("main"))
        .await
        .expect("the landed tip's run");
    assert_eq!(landed.conclusion, Some(CheckConclusion::Success));
}

#[tokio::test]
async fn the_first_bad_step_is_removed_and_the_rest_land_in_order() {
    let mut w = world_with(CLEAN_BASE).await;
    let first = w.doc_mr("fa", &[("qms/QM-003.md", Some(QM_003))]).await;
    let deletes = w.doc_mr("fb", &[("qms/QM-002.md", None)]).await;
    let cites = w
        .doc_mr("fc", &[("qms/QM-001.md", Some(QM_001_CITING_002))])
        .await;
    let last = w.doc_mr("fd", &[("qms/QM-004.md", Some(QM_004))]).await;
    for mr in [&first, &deletes, &cites, &last] {
        w.enqueue(mr).await.expect("enqueue");
    }
    let worker = w.lint_worker("w1");
    let logs = Captured::default();
    let sizes = {
        let _guard = logs.install();
        w.drive(&worker, &BTreeSet::new()).await
    };
    // One train of four; the third step is the first bad one; the three
    // left are one train again (no bisection of the queue's batch size).
    assert_eq!(sizes, [4, 3]);
    assert_removed_by_the_lint(&w, &cites, "doc_lint_failed").await;
    let removed = logs.named("forge.merge_queue.entry_removed");
    assert_eq!(removed.len(), 1, "{removed:?}");
    assert_eq!(removed[0]["step"], 3, "{}", removed[0]);
    assert_eq!(
        removed[0]["merge_request_id"],
        cites.id.get(),
        "{}",
        removed[0]
    );
    for mr in [&first, &deletes, &last] {
        assert_merged(&w, mr).await;
    }

    // In their order: the base, then one commit per merge request left.
    let chain = w.first_parents();
    assert_eq!(chain.len(), 4, "{chain:?}");
    assert_eq!(chain[0], w.base);
    let state = |commit: &str| {
        (
            w.has(commit, "qms/QM-003.md"),
            w.has(commit, "qms/QM-002.md"),
            w.has(commit, "qms/QM-004.md"),
        )
    };
    assert_eq!(state(&chain[1]), (true, true, false), "QM-003 added");
    assert_eq!(state(&chain[2]), (true, false, false), "QM-002 deleted");
    assert_eq!(state(&chain[3]), (true, false, true), "QM-004 added");
    assert_eq!(w.main_file("qms/QM-001.md"), QM_001);
    for commit in &chain {
        let problems = w.problems_at(commit).await;
        assert!(problems.is_empty(), "{commit}: {problems:?}");
    }
}

#[tokio::test]
async fn a_main_with_old_problems_still_moves_when_nothing_new_breaks() {
    let mut w = world_with(OLD_BASE).await;
    assert_eq!(
        w.problems_at(&w.base).await,
        ["qms/QM-001.md:ref_broken:QM-404"]
    );
    let adds = w.doc_mr("fa", &[("qms/QM-003.md", Some(QM_003))]).await;
    w.enqueue(&adds).await.expect("enqueue");
    let worker = w.lint_worker("w1");
    let logs = Captured::default();
    let sizes = {
        let _guard = logs.install();
        w.drive(&worker, &BTreeSet::new()).await
    };
    assert_eq!(sizes, [1]);
    assert_merged(&w, &adds).await;
    assert!(w.main_has("qms/QM-003.md"));
    assert!(logs.named("forge.merge_queue.entry_removed").is_empty());

    // The lint ran and its time is the landing's `lint_ms` (plan 18 S0's
    // constant 0 is now measured: trees were read by git).
    let landed = logs.named("forge.merge_queue.batch_landed");
    assert_eq!(landed.len(), 1, "{landed:?}");
    assert!(
        landed[0]["lint_ms"].as_u64().is_some_and(|ms| ms >= 1),
        "lint_ms measured: {}",
        landed[0]
    );
    // The landed tip's run is the tree's own assessment — the old problem
    // is still listed and fails it — and says what the queue judged.
    let tip = w.tip("main");
    let run = w.doc_lint_run(&tip).await.expect("the landed tip's run");
    assert_eq!(run.conclusion, Some(CheckConclusion::Failure));
    assert!(
        run.summary.contains(&format!(
            "Merge queue: the tree of a train built on {} — 0 problem(s) the base does not have",
            w.base
        )),
        "{}",
        run.summary
    );
    assert!(
        run.summary
            .contains("qms/QM-001.md:8 ref_broken — QM-404 [FG-DOC-0001]"),
        "{}",
        run.summary
    );

    // A second problem of the same kind in another file is new: that
    // merge request does not land on the old problem's back.
    let cites = w
        .doc_mr(
            "fb",
            &[(
                "qms/QM-005.md",
                Some("---\ndoctype: qms.manual\nid: QM-005\ntitle: 追加\nowner: qa-team\n---\n\n[[QM-404]]\n"),
            )],
        )
        .await;
    w.enqueue(&cites).await.expect("enqueue");
    let sizes = w.drive(&worker, &BTreeSet::new()).await;
    assert_eq!(sizes, [1]);
    assert_removed_by_the_lint(&w, &cites, "doc_lint_failed").await;
    assert_eq!(w.tip("main"), tip, "main did not move");

    // A train that changes no document is not linted: nothing is read
    // and no run is recorded on its tip.
    let code = w
        .doc_mr("fc", &[("src/main.rs", Some("fn main() {}\n"))])
        .await;
    w.enqueue(&code).await.expect("enqueue");
    let sizes = w.drive(&worker, &BTreeSet::new()).await;
    assert_eq!(sizes, [1]);
    assert_merged(&w, &code).await;
    assert!(w.doc_lint_run(&w.tip("main")).await.is_none());
}

#[tokio::test]
async fn a_strict_repository_does_not_land_while_the_base_has_problems() {
    let mut w = world_with(OLD_BASE).await;
    w.restart_main(STRICT_BASE);
    let base = w.base.clone(); // clone: compared after the world is borrowed again
                               // Clean by itself, and it adds no problem: in a repository that is not
                               // strict it lands (`a_main_with_old_problems_still_moves…`).
    let adds = w.doc_mr("fa", &[("qms/QM-003.md", Some(QM_003))]).await;
    // Not even a change of no document lands: strict is always linted.
    let code = w
        .doc_mr("fb", &[("src/main.rs", Some("fn main() {}\n"))])
        .await;
    w.enqueue(&adds).await.expect("enqueue");
    w.enqueue(&code).await.expect("enqueue");
    let worker = w.lint_worker("w1");
    let logs = Captured::default();
    let sizes = {
        let _guard = logs.install();
        w.drive(&worker, &BTreeSet::new()).await
    };
    assert_eq!(sizes, [2]);
    assert_eq!(w.tip("main"), base, "main did not move");
    // Nobody is the cause: both leave, with the reason that says so.
    for mr in [&adds, &code] {
        assert_removed_by_the_lint(&w, mr, "doc_lint_base_not_clean").await;
    }
    let batches = w.batches().await;
    assert_eq!(batches.len(), 1, "{batches:?}");
    assert_eq!(
        (
            batches[0].0.as_str(),
            batches[0].1.as_str(),
            batches[0].2.as_str()
        ),
        ("failed", "doc_lint_failed", "FG-CI-0032")
    );
    let removed = logs.named("forge.merge_queue.entry_removed");
    assert_eq!(removed.len(), 2, "{removed:?}");
    for line in &removed {
        assert_eq!(line["reason"], "doc_lint_base_not_clean", "{line}");
        assert_eq!(line["error_code"], "FG-CI-0032", "{line}");
        assert_eq!(
            line["problems"], "ref_broken:qms/QM-001.md:QM-404",
            "{line}"
        );
        assert!(line.get("step").is_none(), "no step is the cause: {line}");
    }
    assert_eq!(
        logs.levels("forge.merge_queue.entry_removed"),
        ["INFO", "INFO"]
    );

    // The way out: one merge request that leaves the target clean lands,
    // and the next one lands after it.
    let repairs = w.doc_mr("fc", &[("qms/QM-001.md", Some(QM_001))]).await;
    w.enqueue(&repairs).await.expect("enqueue");
    let sizes = w.drive(&worker, &BTreeSet::new()).await;
    assert_eq!(sizes, [1]);
    assert_merged(&w, &repairs).await;
    assert_eq!(w.main_file("qms/QM-001.md"), QM_001);
    let next = w.doc_mr("fd", &[("qms/QM-004.md", Some(QM_004))]).await;
    w.enqueue(&next).await.expect("enqueue");
    let sizes = w.drive(&worker, &BTreeSet::new()).await;
    assert_eq!(sizes, [1]);
    assert_merged(&w, &next).await;
    assert!(w.problems_at(&w.tip("main")).await.is_empty());
}

/// A train lint that cannot answer: every call fails — a reading that
/// timed out, or (`bound`) a tree past a bound of the train's reading, as
/// forge-checks' `TrainLint` says it.
struct Unlintable {
    calls: AtomicUsize,
    bound: Option<&'static str>,
}

impl Unlintable {
    fn new(bound: Option<&'static str>) -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            bound,
        })
    }
}

impl TrainDocLint for Unlintable {
    fn lint_train<'a>(&'a self, _input: TrainLintInput<'a>) -> TrainLintFuture<'a> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let error = match self.bound {
            None => {
                ForgeError::new(ErrorCode::RequestTimeout).with_context("operation", "synthetic")
            }
            Some(bound) => ForgeError::new(ErrorCode::DocIndexBounded)
                .with_context("scope", "train")
                .with_context("reason", "read_failed")
                .with_context("bound", bound),
        };
        Box::pin(async move { Err(error) })
    }
}

#[tokio::test]
async fn an_unlintable_train_is_abandoned_and_reaped_on_the_third_time() {
    let mut w = world_with(CLEAN_BASE).await;
    let a = w.doc_mr("fa", &[("qms/QM-003.md", Some(QM_003))]).await;
    w.enqueue(&a).await.expect("enqueue");
    let lint = Unlintable::new(None);
    let worker = w.worker("w1").with_train_lint(lint.clone()); // clone: the test counts its calls
    let logs = Captured::default();
    {
        let _guard = logs.install();
        for round in 1..=3 {
            let built = worker.tick().await.expect("tick").expect("due");
            assert_eq!(built.outcome, "built", "round {round}");
            let ended = worker.tick().await.expect("tick").expect("due");
            assert_eq!(ended.outcome, "reaped", "round {round}");
            let batch = qdb::batch(&w.t.db, built.batch_id.expect("id"))
                .await
                .expect("read")
                .expect("batch");
            assert_eq!(batch.state, BatchState::Abandoned, "round {round}");
            assert_eq!(batch.reason.as_deref(), Some("doc_lint_unavailable"));
            assert_eq!(
                batch.error_code.as_deref(),
                Some(ErrorCode::DocIndexBounded.id())
            );
            // Never a pass: the target did not move.
            assert_eq!(w.tip("main"), w.base, "round {round}");
        }
    }
    assert_eq!(lint.calls.load(Ordering::SeqCst), 3, "asked once per train");
    // The third time the entry leaves for good, as after three reaps — and
    // says what could not be done (never `stalled`: no test was late).
    let entry = w.entry_of(&a).await;
    assert_eq!(entry.state, EntryState::Removed);
    assert_eq!(entry.reason.as_deref(), Some("doc_lint_unavailable"));
    assert_eq!(
        entry.error_code.as_deref(),
        Some(ErrorCode::MergeQueueStalled.id())
    );
    assert_eq!(ErrorCode::MergeQueueStalled.id(), "FG-CI-0024");
    assert_eq!(w.state_of(&a).await, forge_core::MergeRequestState::Open);
    let refs = plumb(&w.path, &["for-each-ref", "refs/merge-queue/"], None);
    assert!(refs.is_empty(), "{refs}");

    // Each failure is logged once, with the code, the scope and the cause.
    let unavailable = logs.named("forge.merge_queue.train_lint_unavailable");
    assert_eq!(unavailable.len(), 3, "{unavailable:?}");
    for line in &unavailable {
        assert_eq!(line["error_code"], "FG-DOC-0002", "{line}");
        assert_eq!(line["scope"], "train", "{line}");
        assert_eq!(line["reason"], "read_failed", "{line}");
        assert_eq!(line["cause"], "FG-WEB-0002", "{line}");
        assert!(line["request_id"].as_str().is_some(), "{line}");
        assert!(line["batch_id"].as_i64().is_some(), "{line}");
        assert_eq!(line["base_sha"], w.base.as_str(), "{line}");
        assert!(line["elapsed_ms"].as_u64().is_some(), "{line}");
        // What is not known is not logged (never an empty field).
        assert!(line.get("bound").is_none(), "{line}");
        assert!(line.get("budget_ms").is_none(), "{line}");
    }
    // The removal names the document check and carries the lint's cause.
    let removed = logs.named("forge.merge_queue.entry_removed");
    assert_eq!(removed.len(), 1, "{removed:?}");
    let removed = &removed[0];
    assert_eq!(removed["reason"], "doc_lint_unavailable", "{removed}");
    assert_eq!(removed["error_code"], "FG-CI-0024", "{removed}");
    assert_eq!(removed["merge_request_id"], a.id.get(), "{removed}");
    assert_eq!(removed["lint_reason"], "read_failed", "{removed}");
    assert_eq!(removed["cause"], "FG-WEB-0002", "{removed}");
    assert!(removed.get("bound").is_none(), "{removed}");
    assert_eq!(removed["base_sha"], w.base.as_str(), "{removed}");

    // The real lint past its budget is the same stop, with its reason.
    let b = w.doc_mr("fb", &[("qms/QM-004.md", Some(QM_004))]).await;
    w.enqueue(&b).await.expect("enqueue");
    let hurried = w.lint_worker("w1").with_limits(QueueLimits {
        poll: Duration::ZERO,
        idle: Duration::ZERO,
        train_lint_budget: Duration::from_nanos(1),
        ..QueueLimits::default()
    });
    let logs = Captured::default();
    {
        let _guard = logs.install();
        let built = hurried.tick().await.expect("tick").expect("due");
        assert_eq!(built.outcome, "built");
        let ended = hurried.tick().await.expect("tick").expect("due");
        assert_eq!(ended.outcome, "reaped");
    }
    let unavailable = logs.named("forge.merge_queue.train_lint_unavailable");
    assert_eq!(unavailable.len(), 1, "{unavailable:?}");
    assert_eq!(unavailable[0]["error_code"], "FG-DOC-0002");
    assert_eq!(
        unavailable[0]["reason"], "train_budget",
        "{}",
        unavailable[0]
    );
    assert_eq!(unavailable[0]["budget_ms"], 0, "{}", unavailable[0]);
    assert!(unavailable[0].get("cause").is_none(), "{}", unavailable[0]);
    assert_eq!(w.tip("main"), w.base);
    assert_eq!(w.entry_of(&b).await.state, EntryState::Queued);
}
