//! The train lint's edges (plan 18 S1 review): a bad step in the middle of
//! a train, the gate off, a train the lint does not apply to, and a tree
//! past a bound of the train's reading.

use super::*;

#[tokio::test]
async fn a_bad_step_in_the_middle_of_three_is_removed_and_the_others_land() {
    let mut w = world_with(CLEAN_BASE).await;
    let deletes = w.doc_mr("fa", &[("qms/QM-002.md", None)]).await;
    let cites = w
        .doc_mr("fb", &[("qms/QM-001.md", Some(QM_001_CITING_002))])
        .await;
    let last = w.doc_mr("fc", &[("qms/QM-004.md", Some(QM_004))]).await;
    for mr in [&deletes, &cites, &last] {
        w.enqueue(mr).await.expect("enqueue");
    }
    let worker = w.lint_worker("w1");
    let logs = Captured::default();
    let sizes = {
        let _guard = logs.install();
        w.drive(&worker, &BTreeSet::new()).await
    };
    // One train of three; its second step is the bad one; the first and
    // the third are one train again and land in their order.
    assert_eq!(sizes, [3, 2]);
    assert_removed_by_the_lint(&w, &cites, "doc_lint_failed").await;
    let removed = logs.named("forge.merge_queue.entry_removed");
    assert_eq!(removed.len(), 1, "{removed:?}");
    assert_eq!(removed[0]["step"], 2, "{}", removed[0]);
    assert_eq!(
        removed[0]["merge_request_id"],
        cites.id.get(),
        "{}",
        removed[0]
    );
    for mr in [&deletes, &last] {
        assert_merged(&w, mr).await;
    }
    let chain = w.first_parents();
    assert_eq!(chain.len(), 3, "{chain:?}");
    assert!(!w.has(&chain[1], "qms/QM-002.md"), "QM-002 deleted first");
    assert!(!w.has(&chain[1], "qms/QM-004.md"));
    assert!(w.has(&chain[2], "qms/QM-004.md"), "QM-004 added after it");
    assert_eq!(w.main_file("qms/QM-001.md"), QM_001);
    for commit in &chain {
        let problems = w.problems_at(commit).await;
        assert!(problems.is_empty(), "{commit}: {problems:?}");
    }
}

/// `merge_queue.train_lint_budget_ms = 0`: `forge serve` injects no lint
/// (its start line says `train_lint=off`, forge-cli `serve_merge_queue`),
/// and trains land as before the gate existed.
#[tokio::test]
async fn without_the_gate_a_train_that_would_fail_the_lint_lands() {
    let mut w = world_with(CLEAN_BASE).await;
    let deletes = w.doc_mr("fa", &[("qms/QM-002.md", None)]).await;
    let cites = w
        .doc_mr("fb", &[("qms/QM-001.md", Some(QM_001_CITING_002))])
        .await;
    w.enqueue(&deletes).await.expect("enqueue");
    w.enqueue(&cites).await.expect("enqueue");
    let off = w.worker("w1");
    assert!(!off.lints_trains());
    assert!(w.lint_worker("w2").lints_trains());
    let logs = Captured::default();
    let sizes = {
        let _guard = logs.install();
        w.drive(&off, &BTreeSet::new()).await
    };
    assert_eq!(sizes, [2]);
    for mr in [&deletes, &cites] {
        assert_merged(&w, mr).await;
    }
    // What the switch gives up: the target now has the broken reference
    // the gate would have kept out.
    let tip = w.tip("main");
    assert_eq!(
        w.problems_at(&tip).await,
        ["qms/QM-001.md:ref_broken:QM-002"]
    );
    assert!(w.doc_lint_run(&tip).await.is_none(), "nothing was linted");
    let landed = logs.named("forge.merge_queue.batch_landed");
    assert_eq!(landed.len(), 1, "{landed:?}");
    assert_eq!(landed[0]["lint_ms"], 0, "{}", landed[0]);
    // Off is silent per train: no verdict, no skip.
    assert!(logs.named("forge.merge_queue.entry_removed").is_empty());
    assert!(logs
        .named("forge.merge_queue.train_lint_skipped")
        .is_empty());
}

#[tokio::test]
async fn a_train_that_changes_no_document_is_not_linted_and_lands() {
    // A target with an old problem: a lint of this train's tree would
    // record a failed run on it.
    let mut w = world_with(OLD_BASE).await;
    let code = w
        .doc_mr("fa", &[("src/main.rs", Some("fn main() {}\n"))])
        .await;
    let more = w
        .doc_mr("fb", &[("src/lib.rs", Some("pub fn f() {}\n"))])
        .await;
    w.enqueue(&code).await.expect("enqueue");
    w.enqueue(&more).await.expect("enqueue");
    let worker = w.lint_worker("w1");
    assert!(worker.lints_trains());
    let logs = Captured::default();
    let sizes = {
        let _guard = logs.install();
        w.drive(&worker, &BTreeSet::new()).await
    };
    assert_eq!(sizes, [2]);
    for mr in [&code, &more] {
        assert_merged(&w, mr).await;
    }
    assert!(w.main_has("src/main.rs") && w.main_has("src/lib.rs"));
    // Not linted: no tree was read, so no run is on the landed tip.
    assert!(w.doc_lint_run(&w.tip("main")).await.is_none());
    assert!(logs.named("forge.merge_queue.entry_removed").is_empty());
    // The skip is told apart from "the gate is off" by its debug line.
    let skipped = logs.named("forge.merge_queue.train_lint_skipped");
    assert_eq!(skipped.len(), 1, "{skipped:?}");
    assert_eq!(
        logs.levels("forge.merge_queue.train_lint_skipped"),
        ["DEBUG"]
    );
    assert_eq!(skipped[0]["base_sha"], w.base.as_str(), "{}", skipped[0]);
    assert!(skipped[0]["batch_id"].as_i64().is_some(), "{}", skipped[0]);
    assert!(
        skipped[0]["elapsed_ms"].as_u64().is_some(),
        "{}",
        skipped[0]
    );
}

#[tokio::test]
async fn a_tree_past_a_bound_leaves_with_the_bound_named() {
    let mut w = world_with(CLEAN_BASE).await;
    let a = w.doc_mr("fa", &[("qms/QM-003.md", Some(QM_003))]).await;
    w.enqueue(&a).await.expect("enqueue");
    let worker = w
        .worker("w1")
        .with_train_lint(Unlintable::new(Some("doc_files")));
    let logs = Captured::default();
    {
        let _guard = logs.install();
        for round in 1..=3 {
            // Plan 18 S7: linted in the tick that built the batch.
            let ended = worker.tick().await.expect("tick").expect("due");
            assert_eq!(ended.outcome, "reaped", "round {round}");
        }
    }
    assert_eq!(w.tip("main"), w.base, "never a pass");
    let entry = w.entry_of(&a).await;
    assert_eq!(entry.state, EntryState::Removed);
    assert_eq!(entry.reason.as_deref(), Some("doc_lint_unavailable"));
    assert_eq!(entry.error_code.as_deref(), Some("FG-CI-0024"));

    let unavailable = logs.named("forge.merge_queue.train_lint_unavailable");
    assert_eq!(unavailable.len(), 3, "{unavailable:?}");
    for line in &unavailable {
        assert_eq!(line["reason"], "read_failed", "{line}");
        assert_eq!(line["bound"], "doc_files", "{line}");
        assert!(line.get("cause").is_none(), "{line}");
    }
    let removed = logs.named("forge.merge_queue.entry_removed");
    assert_eq!(removed.len(), 1, "{removed:?}");
    assert_eq!(removed[0]["reason"], "doc_lint_unavailable");
    assert_eq!(removed[0]["error_code"], "FG-CI-0024");
    assert_eq!(removed[0]["lint_reason"], "read_failed", "{}", removed[0]);
    assert_eq!(removed[0]["bound"], "doc_files", "{}", removed[0]);
    assert!(removed[0].get("cause").is_none(), "{}", removed[0]);
}
