//! Path-aware mergeability against a real bare repository and the
//! development database (plan 18 S4, ADR-0037 (c)), wired as `forge serve`
//! and the worker wire it: the check stores the head's changed paths, a
//! landing says what it changed, the `merge-request-refresh` subscriber
//! carries the results the landing cannot interact with — and the train
//! stays the authority: a carried "mergeable" that conflicts with what the
//! queue lands in front of it never lands (`FG-REVIEW-0006`), and its
//! carried answer is replaced by a computed one.
//!
//! The repository has no CI definition, so a train is green at once and
//! the merge is what decides — except in the tests of a required
//! `ci/pipeline` (`carried_green`): there `main` has a CI definition, a
//! train gets its pipeline, and what is shown is which decisions a carried
//! row's missing merge result holds up (a direct merge) and which it does
//! not (the merge queue, auto-merge).

use forge_core::MergeabilityStatus;
use forge_db::review::{mergeability, stored_paths, MergeRequestRefresh, StoredMergeability};
use forge_db::subscribers::{self, Progress};

use super::*;

/// `review.paths_max`'s default, as the worker and `forge serve` pass it.
const PATHS_MAX: usize = 5000;

/// A world whose landings say what they changed (`review.path_filter`).
async fn world_with_paths() -> World {
    let mut w = world_with(DOC_BASE).await;
    w.merger = w.merger.clone().with_paths(Some(PATHS_MAX)); // clone: a builder over the world's merger
    w
}

/// Check `mr` again as the worker does with the path filter on: the
/// result is stored with the head's changed-path set.
async fn check_with_paths(w: &World, mr: &StoredMergeRequest) {
    let seq = request_check(&w.t.db, mr.id, RequestId::parse("req-q-paths").expect("id"))
        .await
        .expect("request")
        .expect("open");
    let checker = Mergeability::new(w.git.clone(), w.storage.clone()).with_paths(Some(PATHS_MAX));
    check(
        &w.t.db,
        &checker,
        MergeabilityRequest {
            merge_request: mr.id,
            check_seq: seq,
        },
        &RequestId::parse("req-check-paths").expect("id"),
    )
    .await
    .expect("check");
}

/// Sequence the events written so far and let the refresh subscriber
/// handle every one (the worker's loop, here to the end).
async fn refresh_after_landing(w: &World, refresher: &MergeRequestRefresh) {
    forge_db::sequencer::run_once(
        &w.t.db,
        forge_db::sequencer::BatchSize::MAX,
        forge_db::sequencer::AuditSequencing::Skip,
    )
    .await
    .expect("sequencer");
    loop {
        match subscribers::run_once(&w.t.db, refresher)
            .await
            .expect("step")
        {
            Progress::Handled { .. } => {}
            Progress::CaughtUp => break,
            other @ (Progress::Busy
            | Progress::Disabled
            | Progress::Failed(_)
            | Progress::Stuck(_)
            | Progress::Quarantined(_)) => panic!("unexpected {other:?}"),
        }
    }
}

async fn stored(w: &World, mr: &StoredMergeRequest) -> StoredMergeability {
    mergeability(&w.t.db, mr.id)
        .await
        .expect("read")
        .expect("a row")
}

#[tokio::test]
async fn a_carried_mr_that_conflicts_in_the_train_is_removed_with_fg_review_0006() {
    let mut w = world_with_paths().await;
    // a and b both add `shared.txt`, differently: each merges cleanly into
    // `main` alone, never both. c touches another file.
    let a = w.mr("pa", &[("shared.txt", "from a\n")]).await;
    let b = w.mr("pb", &[("shared.txt", "from b\n")]).await;
    let c = w.mr("pc", &[("other.txt", "c\n")]).await;
    check_with_paths(&w, &a).await;
    check_with_paths(&w, &b).await;
    let base = oid(&w.base);
    for mr in [&a, &b] {
        let paths = stored_paths(&w.t.db, mr.id)
            .await
            .expect("read")
            .expect("a stored set");
        assert!(paths.is_complete(), "!{}", mr.iid);
        assert_eq!(paths.files(), ["shared.txt"], "!{}", mr.iid);
        let row = stored(&w, mr).await;
        assert_eq!(row.status, MergeabilityStatus::CanBeMerged);
        assert_eq!(row.computed_target_sha.as_ref(), Some(&base));
        assert_eq!(row.carried_from(), None, "computed, not carried yet");
    }

    // c lands through the queue; its record says it changed `other.txt`.
    let refresher = MergeRequestRefresh::new(50).with_path_filter(true);
    subscribers::register(&w.t.db, &refresher, subscribers::Start::Beginning)
        .await
        .expect("register");
    let worker = w.worker("w1");
    w.enqueue(&c).await.expect("enqueue c");
    w.drive(&worker, &BTreeSet::new()).await;
    assert_eq!(
        w.state_of(&c).await,
        forge_core::MergeRequestState::Merged,
        "c lands"
    );
    let tip = oid(&w.tip("main"));
    assert_ne!(tip, base);
    refresh_after_landing(&w, &refresher).await;

    // Neither a nor b can interact with `other.txt`: both results are kept
    // and answer for the new tip, saying where they were computed.
    let mut seqs = Vec::new();
    for mr in [&a, &b] {
        let row = stored(&w, mr).await;
        assert_eq!(row.status, MergeabilityStatus::CanBeMerged, "!{}", mr.iid);
        assert_eq!(row.checked_target_sha.as_ref(), Some(&tip), "!{}", mr.iid);
        assert_eq!(row.carried_from(), Some(&base), "!{} is carried", mr.iid);
        seqs.push(row.check_seq);
    }

    // The carried answers let both into the queue (the merge conditions
    // count them for the live tip) …
    w.enqueue(&a).await.expect("enqueue a");
    w.enqueue(&b).await.expect("enqueue b");
    w.drive(&worker, &BTreeSet::new()).await;

    // … and the train is the authority: a lands, b conflicts with it and
    // is removed with FG-REVIEW-0006. `main` holds a's file only.
    let entry_a = w.entry_of(&a).await;
    assert_eq!(entry_a.state, EntryState::Merged);
    assert_eq!(w.state_of(&a).await, forge_core::MergeRequestState::Merged);
    let entry_b = w.entry_of(&b).await;
    assert_eq!(entry_b.state, EntryState::Removed);
    assert_eq!(entry_b.reason.as_deref(), Some("conflict"));
    assert_eq!(
        entry_b.error_code.as_deref(),
        Some(ErrorCode::MergeRequestConflict.id())
    );
    assert_eq!(ErrorCode::MergeRequestConflict.id(), "FG-REVIEW-0006");
    assert_eq!(w.state_of(&b).await, forge_core::MergeRequestState::Open);
    assert_eq!(
        plumb(
            &w.path,
            &["cat-file", "-p", "refs/heads/main:shared.txt"],
            None
        ),
        "from a"
    );

    // The disproved carried answer is not left standing: a computed check
    // was requested (a new seq; the row no longer says "mergeable") …
    let after = stored(&w, &b).await;
    assert!(after.check_seq > seqs[1], "a new check was requested");
    assert_ne!(after.status, MergeabilityStatus::CanBeMerged);
    // … and that check says what the train found.
    check_with_paths(&w, &b).await;
    let computed = stored(&w, &b).await;
    assert_eq!(computed.status, MergeabilityStatus::CannotBeMerged);
    assert_eq!(computed.carried_from(), None);
    assert_eq!(
        computed.checked_target_sha.as_ref(),
        Some(&oid(&w.tip("main")))
    );
}

#[tokio::test]
async fn a_landing_on_the_same_file_rechecks_instead_of_carrying() {
    let mut w = world_with_paths().await;
    let a = w.mr("sa", &[("shared.txt", "from a\n")]).await;
    let b = w.mr("sb", &[("shared.txt", "from b\n")]).await;
    check_with_paths(&w, &a).await;
    check_with_paths(&w, &b).await;
    let before = stored(&w, &b).await;
    assert_eq!(before.status, MergeabilityStatus::CanBeMerged);

    let refresher = MergeRequestRefresh::new(50).with_path_filter(true);
    subscribers::register(&w.t.db, &refresher, subscribers::Start::Beginning)
        .await
        .expect("register");
    w.enqueue(&a).await.expect("enqueue a");
    w.drive(&w.worker("w1"), &BTreeSet::new()).await;
    assert_eq!(w.state_of(&a).await, forge_core::MergeRequestState::Merged);
    refresh_after_landing(&w, &refresher).await;

    // a's landing changed b's file: nothing is kept, a check is requested.
    let after = stored(&w, &b).await;
    assert_eq!(after.status, MergeabilityStatus::Unchecked);
    assert!(after.check_seq > before.check_seq);
    assert_eq!(after.carried_from(), None);
    // Not mergeable for the queue until the check has run.
    let refused = w
        .enqueue(&b)
        .await
        .expect_err("not checked for the new tip");
    assert_eq!(refused.code(), ErrorCode::MergeBlocked);
    assert!(refused
        .context()
        .iter()
        .any(|(k, v)| *k == "reason" && v == "mergeability_unknown"));
}

// --- review round: a carried result and what a merge needs from it ------

/// The check `mr`'s row is waiting for (its stored `check_seq`), run as
/// the worker's job runs it with the path filter on.
async fn run_requested_check(w: &World, mr: &StoredMergeRequest) -> forge_review::CheckReport {
    let asked = stored(w, mr).await;
    let checker = Mergeability::new(w.git.clone(), w.storage.clone()).with_paths(Some(PATHS_MAX));
    let report = check(
        &w.t.db,
        &checker,
        MergeabilityRequest {
            merge_request: mr.id,
            check_seq: asked.check_seq,
        },
        &RequestId::parse("req-check-asked").expect("id"),
    )
    .await
    .expect("check");
    w.browser.invalidate(w.repo);
    report
}

/// The live mergeability jobs queued for `mr` at `check_seq`.
async fn check_jobs(w: &World, mr: &StoredMergeRequest, check_seq: i64) -> i64 {
    count(
        &w.t,
        &format!(
            "SELECT count(*) AS n FROM forge.jobs WHERE kind = 'merge_request.mergeability' \
             AND state = 'queued' AND (payload->>'merge_request_id')::bigint = {} \
             AND (payload->>'check_seq')::bigint = {check_seq}",
            mr.id.get()
        ),
    )
    .await
}

/// The merge status dave reads (`GET …/merge-status`).
async fn status(w: &World, mr: &StoredMergeRequest) -> forge_review::MergeStatus {
    w.browser.invalidate(w.repo);
    let proof = w
        .mr_proof(&session(w.dev), Action::MergeRequestRead, mr)
        .await
        .expect("proof");
    w.review
        .merge_status(
            &RequestId::parse("req-status").expect("id"),
            &proof,
            ready(w.repo),
            None,
        )
        .await
        .expect("status")
}

fn reason_names(status: &forge_review::MergeStatus) -> Vec<&'static str> {
    status.reasons.iter().map(|r| r.as_str()).collect()
}

/// dave asks for a direct merge of `mr` (`POST …/merge`).
async fn ask(
    w: &World,
    mr: &StoredMergeRequest,
    id: &str,
) -> Result<forge_review::StoredMerge, forge_errors::ForgeError> {
    let proof = w
        .mr_proof(&session(w.dev), Action::MergeRequestMerge, mr)
        .await
        .expect("proof");
    w.review
        .request_merge(
            &request(id),
            CredentialRef::Session(w.session),
            &proof,
            ready(w.repo),
            &forge_review::MergeAsk {
                sha: oid(mr.head_sha.as_deref().expect("head")),
                method: None,
                delete_source_branch: None,
                message: None,
            },
        )
        .await
}

/// The commits the merge request pipeline was asked for (`ci.pipeline_create`
/// jobs of `mr`), oldest first.
async fn pipeline_jobs(w: &World, mr: &StoredMergeRequest) -> Vec<String> {
    let rows = w
        .t
        .json_rows("SELECT payload FROM forge.jobs WHERE kind = 'ci.pipeline_create' ORDER BY id")
        .await;
    rows.iter()
        .map(|r| &r["payload"])
        .filter(|p| p["merge_request_id"].as_i64() == Some(mr.id.get()))
        .map(|p| p["sha"].as_str().expect("sha").to_owned())
        .collect()
}

/// forge's `ci/pipeline` succeeded on `sha` (a merge result), as the
/// worker reports it.
async fn pipeline_green(w: &World, sha: &ObjectId) {
    w.t.commit_audited(
        ("project", w.project.get()),
        &[format!(
            "INSERT INTO forge.check_runs (project_id, head_sha, name, status, conclusion, \
             completed_at, request_id) VALUES ({p}, '{sha}', 'ci/pipeline', 'completed', \
             'success', now(), 'req-ci') ON CONFLICT (project_id, head_sha, name) DO UPDATE SET \
             conclusion = EXCLUDED.conclusion, revision = forge.check_runs.revision + 1",
            p = w.project.get(),
            sha = sha.as_str(),
        )],
    )
    .await;
}

/// A merge request whose "can be merged" was carried over a landing, on a
/// `main` that requires `ci/pipeline`.
struct CarriedGreen {
    w: World,
    a: StoredMergeRequest,
    head: ObjectId,
    /// The tip the result was computed against.
    base: ObjectId,
    /// The live tip the result was carried to.
    tip: ObjectId,
    /// The carried row's `check_seq`.
    seq: i64,
    worker: MergeQueue,
    /// The direct merge dave asked for while the result was still the
    /// computed one (`carried_green(true)`).
    early: Option<forge_review::StoredMerge>,
}

/// `main` requires `ci/pipeline` and has a CI definition (a train gets its
/// pipeline; `drive` finishes it green). `a` adds `a.txt`, is checked with
/// its paths and is green on its merge result — mergeable (with
/// `ask_early`, dave asks for the direct merge now). Then `c` (another
/// file) lands through the queue and the refresh carries `a`'s result to
/// the new tip.
async fn carried_green(ask_early: bool) -> CarriedGreen {
    let mut w = world_with(BASE).await;
    w.merger = w.merger.clone().with_paths(Some(PATHS_MAX)); // clone: a builder over the world's merger
    w.t.commit_audited(
        ("project", w.project.get()),
        &[
            format!(
                "INSERT INTO forge.protected_refs (project_id, pattern, push_min_role) \
                 VALUES ({}, 'refs/heads/main', 'developer')",
                w.project.get()
            ),
            format!(
                "INSERT INTO forge.protected_ref_merge_rules (protected_ref_id, project_id, \
                 required_approvals, required_checks, require_resolved_discussions) \
                 SELECT id, project_id, 0, ARRAY['ci/pipeline']::text[], false \
                 FROM forge.protected_refs WHERE project_id = {}",
                w.project.get()
            ),
        ],
    )
    .await;
    let a = w.mr("ra", &[("a.txt", "a\n")]).await;
    let c = w.mr("rc", &[("other.txt", "c\n")]).await;
    check_with_paths(&w, &a).await;
    let base = oid(&w.base);
    let head = oid(a.head_sha.as_deref().expect("head"));
    let r1 = forge_db::review::merge_result_of(&w.t.db, a.id, &head, &base)
        .await
        .expect("read")
        .expect("a merge result at the base");
    pipeline_green(&w, &r1).await;
    let first = status(&w, &a).await;
    assert!(first.mergeable, "{:?}", first.reasons);
    assert_eq!(first.merge_result_sha.as_ref(), Some(&r1));
    assert_eq!(first.carried_from_target_sha, None);
    let early = if ask_early {
        Some(ask(&w, &a, "req-merge-early").await.expect("queued"))
    } else {
        None
    };

    let refresher = MergeRequestRefresh::new(50).with_path_filter(true);
    subscribers::register(&w.t.db, &refresher, subscribers::Start::Beginning)
        .await
        .expect("register");
    let worker = w.worker("w1");
    w.enqueue(&c).await.expect("enqueue c");
    w.drive(&worker, &BTreeSet::new()).await;
    assert_eq!(
        w.state_of(&c).await,
        forge_core::MergeRequestState::Merged,
        "c lands on its train's pipeline"
    );
    let tip = oid(&w.tip("main"));
    assert_ne!(tip, base);
    refresh_after_landing(&w, &refresher).await;

    let row = stored(&w, &a).await;
    assert_eq!(row.status, MergeabilityStatus::CanBeMerged);
    assert_eq!(row.checked_target_sha.as_ref(), Some(&tip));
    assert_eq!(row.carried_from(), Some(&base), "a is carried");
    assert_eq!(
        forge_db::review::merge_result_of(&w.t.db, a.id, &head, &tip)
            .await
            .expect("read"),
        None,
        "a carried row has no merge result at the live tip"
    );
    let seq = row.check_seq;
    CarriedGreen {
        w,
        a,
        head,
        base,
        tip,
        seq,
        worker,
        early,
    }
}

/// What the status says about a carried row whose required `ci/pipeline`
/// ran on the merge result of the tip it was computed at: carried, the
/// run is stale for a direct merge, and the check is *not* "waiting for a
/// conflict-free merge result" (the merge is clean).
async fn assert_carried_status(c: &CarriedGreen) {
    let status = status(&c.w, &c.a).await;
    assert_eq!(status.carried_from_target_sha.as_ref(), Some(&c.base));
    assert_eq!(status.merge_result_sha, None);
    assert!(!status.mergeable);
    assert_eq!(reason_names(&status), ["required_check_stale"]);
    assert_eq!(
        status.reasons[0].detail().as_deref(),
        Some("name=ci/pipeline")
    );
    assert!(!status.waits_for_merge_result(&status.checks[0]));
}

/// Review (HIGH), decision (a): a person's enqueue is not held up by the
/// missing merge result of a carried row. `ci/pipeline` is the train's to
/// test (ADR-0030 §2), so the carried merge request enters the queue at
/// once — the row stays carried, nothing waits for the sweep — and lands
/// on the train's own pipeline.
#[tokio::test]
async fn a_carried_mr_with_a_required_pipeline_enters_the_queue_at_once() {
    let c = carried_green(false).await;
    assert_carried_status(&c).await;
    let entry = c.w.enqueue(&c.a).await.expect("the queue tests it");
    assert!(entry.state.is_live());
    let row = stored(&c.w, &c.a).await;
    assert_eq!(row.check_seq, c.seq, "no recompute was needed");
    assert_eq!(row.carried_from(), Some(&c.base));
    c.w.drive(&c.worker, &BTreeSet::new()).await;
    assert_eq!(
        c.w.state_of(&c.a).await,
        forge_core::MergeRequestState::Merged
    );
    assert!(c.w.main_has("a.txt"));
    assert!(c.w.main_has("other.txt"));
}

/// Review (HIGH), decision (b): auto-merge queues a carried merge request
/// as well — what `ci/pipeline` waits for is the queue's
/// (`auto::blockers`), and no fresh run is `PipelineState::None`, not a
/// stop.
#[tokio::test]
async fn auto_merge_queues_a_carried_mr_with_a_required_pipeline_at_once() {
    let c = carried_green(false).await;
    let auto = c.w.auto();
    c.w.risk(&c.a, CheckConclusion::Success, "No risk found")
        .await;
    let report = auto.tick().await.expect("tick");
    assert_eq!(report.queued, 1, "{report:?}");
    let entry = c.w.live_entry(&c.a).await.expect("queued");
    assert_eq!(entry.credential, CredentialRef::AutoMerge(c.w.project));
    let row = stored(&c.w, &c.a).await;
    assert_eq!(row.check_seq, c.seq, "no recompute was needed");
    assert_eq!(row.carried_from(), Some(&c.base));
    c.w.drive(&c.worker, &BTreeSet::new()).await;
    assert_eq!(
        c.w.state_of(&c.a).await,
        forge_core::MergeRequestState::Merged
    );
    assert!(c.w.main_has("a.txt"));
}

/// Review (HIGH), decision (c): a direct merge still needs `ci/pipeline`
/// on the merge result of the **live** tip, which a carried row does not
/// have — it is refused with FG-CI-0013, exactly as for a computed row.
/// What the refusal does now: it asks for the computed check at once
/// (the row is `unchecked` with a queued job), so the merge result and
/// its pipeline follow without the worker's sweep; once that pipeline is
/// green the same request merges.
#[tokio::test]
async fn a_direct_merge_of_a_carried_mr_asks_for_its_merge_result_at_once() {
    let c = carried_green(false).await;
    let (w, a) = (&c.w, &c.a);
    assert_carried_status(&c).await;
    let asked_before = pipeline_jobs(w, a).await;

    let refused = ask(w, a, "req-merge-carried").await.expect_err("stale");
    assert_eq!(refused.code(), ErrorCode::RequiredCheckStale);
    assert_eq!(ErrorCode::RequiredCheckStale.id(), "FG-CI-0013");
    assert!(refused
        .context()
        .iter()
        .any(|(k, v)| *k == "reasons" && v == "required_check_stale"));

    // The refusal requested the computed check: no sweep, no landing.
    let waiting = stored(w, a).await;
    assert_eq!(waiting.status, MergeabilityStatus::Unchecked);
    assert!(waiting.check_seq > c.seq, "a new check was requested");
    assert_eq!(waiting.carried_from(), None);
    assert_eq!(check_jobs(w, a, waiting.check_seq).await, 1);
    assert_eq!(w.tip("main"), c.tip.as_str(), "nothing merged");

    // The worker runs that check: computed at the live tip, with its merge
    // result, and the merge request pipeline is asked for on it.
    let report = run_requested_check(w, a).await;
    assert_eq!(report.outcome, "checked");
    let computed = stored(w, a).await;
    assert_eq!(computed.status, MergeabilityStatus::CanBeMerged);
    assert_eq!(computed.carried_from(), None);
    assert_eq!(computed.computed_target_sha.as_ref(), Some(&c.tip));
    let r2 = forge_db::review::merge_result_of(&w.t.db, a.id, &c.head, &c.tip)
        .await
        .expect("read")
        .expect("the live tip's merge result");
    let mut asked_after = pipeline_jobs(w, a).await;
    assert_eq!(asked_after.len(), asked_before.len() + 1);
    assert_eq!(asked_after.pop().as_deref(), Some(r2.as_str()));
    let now = status(w, a).await;
    assert_eq!(now.carried_from_target_sha, None);
    assert_eq!(now.merge_result_sha.as_ref(), Some(&r2));
    assert_eq!(reason_names(&now), ["required_check_stale"]);

    // Its pipeline reports: mergeable, and the direct merge lands.
    pipeline_green(w, &r2).await;
    let green = status(w, a).await;
    assert!(green.mergeable, "{:?}", green.reasons);
    let attempt = ask(w, a, "req-merge-computed").await.expect("queued");
    let merged = w.merger.execute(attempt.id).await.expect("merge");
    assert_eq!(merged.outcome, "applied");
    assert!(w.main_has("a.txt"));
}

/// Decision (c) in the merge job: an attempt requested while the result
/// was computed and fresh, executed after a landing carried the row. The
/// job's gate refuses it with FG-CI-0013 (no ref moves) and requests the
/// computed check itself.
#[tokio::test]
async fn the_merge_job_asks_for_the_merge_result_of_a_carried_mr() {
    let c = carried_green(true).await;
    let (w, a) = (&c.w, &c.a);
    let attempt = c.early.as_ref().expect("asked before the landing");
    let report = w.merger.execute(attempt.id).await.expect("job");
    assert_eq!(report.outcome, "superseded");
    assert_eq!(report.error_code, Some(ErrorCode::RequiredCheckStale));
    assert_eq!(w.tip("main"), c.tip.as_str(), "nothing merged");
    let waiting = stored(w, a).await;
    assert_eq!(waiting.status, MergeabilityStatus::Unchecked);
    assert!(waiting.check_seq > c.seq, "a new check was requested");
    assert_eq!(check_jobs(w, a, waiting.check_seq).await, 1);
}

/// Review (MEDIUM): with several merge bases the changed-path set is the
/// diff from one of them while the trial merge uses a virtual base, so it
/// is stored as incomplete and the merge request is checked again on
/// every landing. A criss-cross history: `main` and the head's parent are
/// two merges of the same two commits, in opposite order.
#[tokio::test]
async fn a_head_with_several_merge_bases_is_never_carried() {
    let mut w = world_with_paths().await;
    let readme = ("README.md", "base\n");
    let x = commit(&w.path, &[readme, ("x.txt", "x\n")], Some(&w.base));
    let y = commit(&w.path, &[readme, ("y.txt", "y\n")], Some(&w.base));
    let both = [readme, ("x.txt", "x\n"), ("y.txt", "y\n")];
    let tree = plumb(
        &w.path,
        &[
            "rev-parse",
            &format!("{}^{{tree}}", commit(&w.path, &both, None)),
        ],
        None,
    );
    let merge = |first: &str, second: &str| {
        plumb(
            &w.path,
            &[
                "commit-tree",
                &tree,
                "-m",
                "synthetic merge",
                "-p",
                first,
                "-p",
                second,
            ],
            None,
        )
    };
    let (left, right) = (merge(&x, &y), merge(&y, &x));
    assert_ne!(left, right);
    plumb(&w.path, &["update-ref", "refs/heads/main", &left], None);
    w.browser.invalidate(w.repo);
    assert_eq!(
        plumb(&w.path, &["merge-base", "--all", &left, &right], None)
            .lines()
            .count(),
        2,
        "a criss-cross: two merge bases"
    );

    let with = |file: &'static str| {
        let mut files = both.to_vec();
        files.push((file, "new\n"));
        files
    };
    // `cc` grows from the other merge: two merge bases with `main`.
    // `d` and `e` grow from `main`: one.
    let cc_head = commit(&w.path, &with("cc.txt"), Some(&right));
    let d_head = commit(&w.path, &with("d.txt"), Some(&left));
    let e_head = commit(&w.path, &with("e.txt"), Some(&left));
    let cc = w.mr_at("cc", &cc_head).await;
    let d = w.mr_at("dd", &d_head).await;
    let e = w.mr_at("ee", &e_head).await;
    check_with_paths(&w, &cc).await;
    check_with_paths(&w, &d).await;

    let criss = stored(&w, &cc).await;
    assert_eq!(criss.status, MergeabilityStatus::CanBeMerged);
    let set = stored_paths(&w.t.db, cc.id)
        .await
        .expect("read")
        .expect("a stored set");
    assert!(!set.is_complete(), "several merge bases: {set:?}");
    assert!(set.files().is_empty());
    let single = stored_paths(&w.t.db, d.id)
        .await
        .expect("read")
        .expect("a stored set");
    assert!(single.is_complete());
    assert_eq!(single.files(), ["d.txt"]);
    let d_before = stored(&w, &d).await;

    // `e` lands (another file): `d` is carried, `cc` is checked again.
    let refresher = MergeRequestRefresh::new(50).with_path_filter(true);
    subscribers::register(&w.t.db, &refresher, subscribers::Start::Beginning)
        .await
        .expect("register");
    w.enqueue(&e).await.expect("enqueue e");
    w.drive(&w.worker("w1"), &BTreeSet::new()).await;
    assert_eq!(w.state_of(&e).await, forge_core::MergeRequestState::Merged);
    let tip = oid(&w.tip("main"));
    refresh_after_landing(&w, &refresher).await;

    let d_after = stored(&w, &d).await;
    assert_eq!(d_after.status, MergeabilityStatus::CanBeMerged);
    assert_eq!(d_after.check_seq, d_before.check_seq);
    assert_eq!(d_after.checked_target_sha.as_ref(), Some(&tip));
    assert_eq!(d_after.carried_from(), Some(&oid(&left)));
    let cc_after = stored(&w, &cc).await;
    assert_eq!(cc_after.status, MergeabilityStatus::Unchecked);
    assert!(cc_after.check_seq > criss.check_seq);
    assert_eq!(cc_after.carried_from(), None);
}

/// Review (MEDIUM): a merge request carried over several landings that
/// then conflicts in a **direct** merge is refused with FG-REVIEW-0006 by
/// the job's own trial merge, and its carried row is replaced by a
/// computed one.
///
/// The rule cannot produce such a row (a landing on the merge request's
/// file is never carried — `a_landing_on_the_same_file_rechecks_instead_
/// of_carrying`, and the property tests against git). To show that the
/// merge is the authority even if it did, the stored set is replaced by
/// one that misses the conflicting file — what a defect of the rule, or
/// of the set, would amount to — before the conflicting change lands.
#[tokio::test]
async fn a_carried_mr_that_conflicts_in_a_direct_merge_is_refused_and_recomputed() {
    let mut w = world_with_paths().await;
    let a = w.mr("ma", &[("shared.txt", "from a\n")]).await;
    let b = w.mr("mb", &[("shared.txt", "from b\n")]).await;
    let c = w.mr("mc", &[("other.txt", "c\n")]).await;
    let d = w.mr("md", &[("more.txt", "d\n")]).await;
    check_with_paths(&w, &a).await;
    let base = oid(&w.base);
    let seq = stored(&w, &a).await.check_seq;
    let refresher = MergeRequestRefresh::new(50).with_path_filter(true);
    subscribers::register(&w.t.db, &refresher, subscribers::Start::Beginning)
        .await
        .expect("register");
    let worker = w.worker("w1");

    // Two landings on other files: a is carried twice, from the same tip.
    for (n, mr) in [&c, &d].into_iter().enumerate() {
        check_with_paths(&w, mr).await;
        w.enqueue(mr).await.expect("enqueue");
        w.drive(&worker, &BTreeSet::new()).await;
        assert_eq!(w.state_of(mr).await, forge_core::MergeRequestState::Merged);
        refresh_after_landing(&w, &refresher).await;
        let row = stored(&w, &a).await;
        assert_eq!(row.status, MergeabilityStatus::CanBeMerged, "landing {n}");
        assert_eq!(row.check_seq, seq, "landing {n}: not checked again");
        assert_eq!(row.carried_from(), Some(&base), "landing {n}");
        assert_eq!(
            row.checked_target_sha.as_ref(),
            Some(&oid(&w.tip("main"))),
            "landing {n}"
        );
    }

    // The stored set stops naming `shared.txt` (see the test's doc), and b
    // — the conflicting change — lands: a is carried a third time.
    w.t.exec_superuser(&format!(
        "UPDATE forge.merge_request_mergeability SET head_files = ARRAY['elsewhere.txt'] \
         WHERE merge_request_id = {}",
        a.id.get()
    ))
    .await;
    check_with_paths(&w, &b).await;
    w.enqueue(&b).await.expect("enqueue b");
    w.drive(&worker, &BTreeSet::new()).await;
    assert_eq!(w.state_of(&b).await, forge_core::MergeRequestState::Merged);
    let tip = w.tip("main");
    refresh_after_landing(&w, &refresher).await;
    let carried = stored(&w, &a).await;
    assert_eq!(carried.status, MergeabilityStatus::CanBeMerged);
    assert_eq!(carried.check_seq, seq);
    assert_eq!(carried.carried_from(), Some(&base));
    assert_eq!(carried.checked_target_sha.as_ref(), Some(&oid(&tip)));

    // The carried "mergeable" lets the request in; the job's trial merge
    // is the authority.
    let attempt = ask(&w, &a, "req-merge-conflict")
        .await
        .expect("queued on the carried result");
    let report = w.merger.execute(attempt.id).await.expect("job");
    assert_eq!(report.outcome, "superseded");
    assert_eq!(report.error_code, Some(ErrorCode::MergeRequestConflict));
    assert_eq!(ErrorCode::MergeRequestConflict.id(), "FG-REVIEW-0006");
    assert_eq!(w.tip("main"), tip, "nothing merged");
    assert_eq!(w.state_of(&a).await, forge_core::MergeRequestState::Open);

    // The disproved carried row is replaced: a check was requested, and it
    // says what the merge found, computed at the live tip.
    let asked = stored(&w, &a).await;
    assert_eq!(asked.status, MergeabilityStatus::Unchecked);
    assert!(asked.check_seq > seq, "a new check was requested");
    assert_eq!(asked.carried_from(), None);
    assert_eq!(check_jobs(&w, &a, asked.check_seq).await, 1);
    let report = run_requested_check(&w, &a).await;
    assert_eq!(report.outcome, "checked");
    let computed = stored(&w, &a).await;
    assert_eq!(computed.status, MergeabilityStatus::CannotBeMerged);
    assert_eq!(computed.carried_from(), None);
    assert_eq!(computed.checked_target_sha.as_ref(), Some(&oid(&tip)));
    assert_eq!(computed.computed_target_sha.as_ref(), Some(&oid(&tip)));
}
