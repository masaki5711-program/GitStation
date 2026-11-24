//! `.forge/repo.yml` and the train lint's strictness (plan 18 S1 review):
//! a base whose file cannot be read as a valid one (`FG-REPO-0024`) never
//! wedges the repository — the train that repairs the file decides with
//! its own value and lands; while both sides are invalid nothing lands on
//! old problems, and the removal names the config, not the documents. A
//! valid `doc_lint_strict: true` on the base is never weakened by a member
//! of the train.

use forge_core::{PushRecord, PushUpdateKind, PushedRef, RefName};
use forge_db::change::{Actor, ChangeContext};
use forge_db::review::ApproveOutcome;

use super::*;

/// `merge_queue.doc_lint_strict` is neither `true` nor `false` (line 2).
const INVALID: &str = "merge_queue:\n  doc_lint_strict: maybe\n";
const NOT_STRICT: &str = "merge_queue:\n  doc_lint_strict: false\n";

/// [`OLD_BASE`] with a `.forge/repo.yml` that is not valid.
const INVALID_BASE: &[(&str, &str)] = &[
    ("README.md", "base\n"),
    (".forge/doctypes/qms.manual.yml", MANUAL_T),
    (".forge/repo.yml", INVALID),
    ("qms/QM-001.md", QM_001_CITING_404),
];

impl World {
    /// [`World::doc_mr`] approved by olga, the owner: a change under
    /// `.forge/` enters the queue only with a Maintainer's approval
    /// (ADR-0017 §2.7). The approval counts only when the committers are
    /// known, so dave's push of the branch is on record.
    async fn config_mr(
        &mut self,
        name: &str,
        changes: &[(&str, Option<&str>)],
    ) -> StoredMergeRequest {
        let mr = self.doc_mr(name, changes).await;
        let head = oid(mr.head_sha.as_deref().expect("head"));
        let push = PushRecord {
            repository_id: self.repo,
            updates: vec![PushedRef {
                name: RefName::parse(&format!("refs/heads/{name}")).expect("ref"),
                old: oid(&"0".repeat(40)),
                new: head.clone(), // clone: the one commit is also the listed one
                kind: PushUpdateKind::Create,
                commits: vec![head],
                total_commits: 1,
                total_commits_partial: false,
            }],
        };
        let ctx = ChangeContext::new(
            Actor::User(self.dev),
            RequestId::parse(&format!("req-push-{name}")).expect("id"),
        );
        let recorded = repositories::record_push(&self.t.db, &ctx, &push)
            .await
            .expect("push record");
        assert_eq!(recorded, forge_core::PushRecorded::Recorded);
        let who = session(self.owner);
        let project = loaded(&self.t, &who, self.project).await;
        let proof = prove(
            &who,
            Action::MergeRequestApprove,
            MergeRequestCtx::new(project, forge_review::loaded_merge_request(&mr)),
        )
        .expect("proof");
        let answer = self
            .review
            .approve(
                &request(&format!("req-approve-{name}")),
                &proof,
                ready(self.repo),
                &oid(mr.head_sha.as_deref().expect("head")),
                None,
            )
            .await
            .expect("approve");
        assert!(
            matches!(
                answer.outcome,
                ApproveOutcome::Approved { counted: true, .. }
            ),
            "the Maintainer's approval counts: {:?}",
            answer.outcome
        );
        mr
    }
}

#[tokio::test]
async fn a_train_that_repairs_an_invalid_repo_yml_lands() {
    let mut w = world_with(OLD_BASE).await;
    w.restart_main(INVALID_BASE);
    assert_eq!(
        w.problems_at(&w.base).await,
        ["qms/QM-001.md:ref_broken:QM-404"]
    );
    // The repair changes only the file: the base's old problem stays.
    let repairs = w
        .config_mr("fa", &[(".forge/repo.yml", Some(NOT_STRICT))])
        .await;
    w.enqueue(&repairs).await.expect("enqueue");
    let worker = w.lint_worker("w1");
    let logs = Captured::default();
    let sizes = {
        let _guard = logs.install();
        w.drive(&worker, &BTreeSet::new()).await
    };
    assert_eq!(sizes, [1]);
    // The tip's valid file decides (not strict): the repair lands.
    assert_merged(&w, &repairs).await;
    assert_eq!(w.main_file(".forge/repo.yml"), NOT_STRICT);
    assert!(logs.named("forge.merge_queue.entry_removed").is_empty());

    // Deleting the file is a repair too: without one the repository is
    // not strict.
    w.restart_main(INVALID_BASE);
    let deletes = w.config_mr("fb", &[(".forge/repo.yml", None)]).await;
    w.enqueue(&deletes).await.expect("enqueue");
    let sizes = w.drive(&worker, &BTreeSet::new()).await;
    assert_eq!(sizes, [1]);
    assert_merged(&w, &deletes).await;
    assert!(!w.main_has(".forge/repo.yml"));

    // The tip's value is the one that counts: a repair that says "strict"
    // and leaves the old problem is told so — the documents, not the file.
    w.restart_main(INVALID_BASE);
    let base = w.base.clone(); // clone: compared after the world is borrowed again
    let strict = w
        .config_mr("fc", &[(".forge/repo.yml", Some(STRICT))])
        .await;
    w.enqueue(&strict).await.expect("enqueue");
    let sizes = w.drive(&worker, &BTreeSet::new()).await;
    assert_eq!(sizes, [1]);
    assert_removed_by_the_lint(&w, &strict, "doc_lint_base_not_clean").await;
    assert_eq!(w.tip("main"), base, "main did not move");
}

#[tokio::test]
async fn an_invalid_repo_yml_on_both_sides_does_not_land_and_names_the_config() {
    assert_eq!(ErrorCode::RepoConfigInvalid.id(), "FG-REPO-0024");
    let mut w = world_with(OLD_BASE).await;
    w.restart_main(INVALID_BASE);
    let base = w.base.clone(); // clone: compared after the world is borrowed again
    let adds = w.doc_mr("fa", &[("qms/QM-003.md", Some(QM_003))]).await;
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

    // Both leave, and what they are told is the config — not "your
    // documents are broken".
    for mr in [&adds, &code] {
        let entry = w.entry_of(mr).await;
        assert_eq!(entry.state, EntryState::Removed, "!{}", mr.iid);
        assert_eq!(
            entry.reason.as_deref(),
            Some("doc_lint_repo_config_invalid"),
            "!{}",
            mr.iid
        );
        assert_eq!(
            entry.error_code.as_deref(),
            Some("FG-REPO-0024"),
            "!{}",
            mr.iid
        );
        assert_eq!(w.state_of(mr).await, forge_core::MergeRequestState::Open);
    }
    let batches = w.batches().await;
    assert_eq!(batches.len(), 1, "{batches:?}");
    assert_eq!(
        (
            batches[0].0.as_str(),
            batches[0].1.as_str(),
            batches[0].2.as_str()
        ),
        ("failed", "doc_lint_repo_config_invalid", "FG-REPO-0024")
    );
    let removed = logs.named("forge.merge_queue.entry_removed");
    assert_eq!(removed.len(), 2, "{removed:?}");
    for line in &removed {
        assert_eq!(line["reason"], "doc_lint_repo_config_invalid", "{line}");
        assert_eq!(line["error_code"], "FG-REPO-0024", "{line}");
        assert_eq!(line["config_line"], 2, "{line}");
        assert_eq!(line["config_reason"], "doc_lint_strict_invalid", "{line}");
        assert_eq!(line["base_sha"], base.as_str(), "{line}");
        assert_eq!(
            line["problems"], "ref_broken:qms/QM-001.md:QM-404",
            "{line}"
        );
        assert!(line.get("step").is_none(), "no step is the cause: {line}");
    }
    // The one removal that is no expected outcome of a member's change.
    assert_eq!(
        logs.levels("forge.merge_queue.entry_removed"),
        ["WARN", "WARN"]
    );

    // A train that leaves no problem lands even so: the file only makes
    // the repository strict, it does not close the queue.
    let cleans = w.doc_mr("fc", &[("qms/QM-001.md", Some(QM_001))]).await;
    w.enqueue(&cleans).await.expect("enqueue");
    let sizes = w.drive(&worker, &BTreeSet::new()).await;
    assert_eq!(sizes, [1]);
    assert_merged(&w, &cleans).await;
    assert_eq!(w.main_file(".forge/repo.yml"), INVALID);
}

#[tokio::test]
async fn a_member_cannot_turn_a_strict_base_non_strict() {
    let mut w = world_with(OLD_BASE).await;
    w.restart_main(STRICT_BASE);
    let base = w.base.clone(); // clone: compared after the world is borrowed again

    // One member relaxes the rule, the next rides on it — in one train.
    let relaxes = w
        .config_mr("fa", &[(".forge/repo.yml", Some(NOT_STRICT))])
        .await;
    let adds = w.doc_mr("fb", &[("qms/QM-003.md", Some(QM_003))]).await;
    w.enqueue(&relaxes).await.expect("enqueue");
    w.enqueue(&adds).await.expect("enqueue");
    let worker = w.lint_worker("w1");
    let sizes = w.drive(&worker, &BTreeSet::new()).await;
    assert_eq!(sizes, [2]);
    // The base's valid value decides: strict, and the base is not clean.
    for mr in [&relaxes, &adds] {
        assert_removed_by_the_lint(&w, mr, "doc_lint_base_not_clean").await;
    }
    assert_eq!(w.tip("main"), base, "main did not move");
    assert_eq!(w.main_file(".forge/repo.yml"), STRICT);

    // Nor does deleting the file, or breaking it, in the train.
    let deletes = w.config_mr("fc", &[(".forge/repo.yml", None)]).await;
    let breaks = w
        .config_mr("fd", &[(".forge/repo.yml", Some(INVALID))])
        .await;
    for mr in [&deletes, &breaks] {
        w.enqueue(mr).await.expect("enqueue");
        let sizes = w.drive(&worker, &BTreeSet::new()).await;
        assert_eq!(sizes, [1]);
        assert_removed_by_the_lint(&w, mr, "doc_lint_base_not_clean").await;
    }
    assert_eq!(w.tip("main"), base, "main did not move");
}
