//! Path-aware carry-forward against a migrated database (plan 18 S4,
//! ADR-0037 (c)): the SQL twin of `may_interact`, what a landing carries
//! and what it checks again, and the orders in which a landing, a new
//! head and a refresh can meet.

use forge_core::ids::{MergeRequestId, UserId};
use forge_core::merge_paths::{may_interact, ChangedPath, LandedPaths, PathSet};
use forge_core::{
    MergeMethod, MergeabilityStatus, PushRecord, PushRecorder, PushUpdateKind, PushedRef, Pusher,
    RequestId,
};
use proptest::prelude::*;
use proptest::strategy::ValueTree;
use proptest::test_runner::TestRunner;

use super::tests::{as_user, branch, drain, oid, open_at_a, queued_checks, result, system, web};
use super::{carry_forward_on, settled_on};
use crate::projects::CreatedProject;
use crate::review::*;
use crate::subscribers::{self, Start};
use crate::test_support::*;

fn rid(text: &str) -> RequestId {
    RequestId::parse(text).expect("request id")
}

/// The complete set of `(path, deleted)` entries.
fn set(entries: &[(&str, bool)]) -> PathSet {
    PathSet::build(
        entries.iter().map(|(path, deleted)| ChangedPath {
            path: (*path).to_owned(),
            deleted: *deleted,
            special: false,
        }),
        5000,
    )
    .expect("a complete set")
}

/// The complete set of changed (not deleted) `files`.
fn files(files: &[&str]) -> PathSet {
    set(&files.iter().map(|f| (*f, false)).collect::<Vec<_>>())
}

/// One finished check of `mr` (head `a`) against the target tip `target`,
/// stored with `paths`.
async fn checked(
    t: &FreshDb,
    mr: MergeRequestId,
    status: MergeabilityStatus,
    target: char,
    paths: Option<PathSet>,
) {
    let seq = request_check(&t.db, mr, rid("req-r"))
        .await
        .expect("request")
        .expect("open");
    let start = begin_check(&t.db, mr, seq).await.expect("begin");
    assert!(matches!(start, CheckStart::Check(_)), "{start:?}");
    let mut check = result(status, 'a', target);
    check.paths = paths;
    let done = record_check(
        &t.db,
        &system(&format!("req-check-{}-{seq}", mr.get())),
        mr,
        seq,
        &check,
    )
    .await
    .expect("record");
    assert!(matches!(done, Recorded::Stored { .. }), "{done:?}");
}

async fn row(t: &FreshDb, mr: MergeRequestId) -> StoredMergeability {
    mergeability_written(&t.db, mr)
        .await
        .expect("read")
        .expect("row")
}

/// A merge lands on `main` (`old` → `new`) through the merge's own record
/// (`finish_merge`, the transaction that writes `repository.pushed`), with
/// `paths` as its landed block (`None`: no block).
async fn land(
    t: &FreshDb,
    alice: UserId,
    web: &CreatedProject,
    source: &str,
    (old, new): (char, char),
    paths: Option<PathSet>,
) {
    let mr = open_at_a(t, alice, web.project_id, source, "main").await;
    let ask = merges::NewMerge {
        method: MergeMethod::MergeCommit,
        head_sha: oid('a'),
        message: None,
        delete_source_branch: false,
        credential: merges::CredentialRef::Session(1),
    };
    let (attempt, ()) = merges::request_merge_mr_decided(
        &t.db,
        &as_user(alice, &format!("req-merge-{source}")),
        (web.project_id, mr.iid),
        alice,
        &ask,
        |_| Ok(()),
        |_| "synthetic".to_owned(),
    )
    .await
    .expect("ask");
    let step = merges::GateStep::Prepare {
        expected_old: oid(old),
        new_sha: oid(new),
    };
    let prepared = merges::merge_gate_decided(
        &t.db,
        &system(&format!("req-gate-{source}")),
        attempt.id,
        step,
        |_| Ok(()),
    )
    .await
    .expect("prepare");
    assert!(matches!(prepared, merges::GateOutcome::Decided(())));
    assert!(merges::fence_merge(&t.db, attempt.id).await.expect("fence"));
    let attempt = merges::merge_attempt(&t.db, attempt.id)
        .await
        .expect("read")
        .expect("row");
    let push = PushRecord {
        repository_id: web.repository_id,
        updates: vec![PushedRef {
            name: branch("main"),
            old: oid(old),
            new: oid(new),
            kind: PushUpdateKind::FastForward,
            commits: Vec::new(),
            total_commits: 0,
            total_commits_partial: true,
        }],
    };
    let landed = paths.map(|paths| LandedPaths {
        target: branch("main"),
        old: oid(old),
        new: oid(new),
        paths,
    });
    let done = merges::finish_merge(
        &t.db,
        &as_user(alice, &format!("req-finish-{source}")),
        &attempt,
        &push,
        Some(&[]),
        landed.as_ref(),
    )
    .await
    .expect("finish");
    assert!(
        matches!(done, merges::MergeFinished::Recorded(_)),
        "{done:?}"
    );
}

struct World {
    t: FreshDb,
    alice: UserId,
    web: CreatedProject,
    subscriber: MergeRequestRefresh,
}

async fn world(path_filter: bool) -> World {
    let t = FreshDb::migrated().await;
    let (alice, web) = web(&t).await;
    let subscriber = MergeRequestRefresh::new(50).with_path_filter(path_filter);
    subscribers::register(&t.db, &subscriber, Start::Head)
        .await
        .expect("register");
    World {
        t,
        alice,
        web,
        subscriber,
    }
}

impl World {
    async fn open(&self, source: &str) -> StoredMergeRequest {
        open_at_a(&self.t, self.alice, self.web.project_id, source, "main").await
    }

    async fn land(&self, source: &str, tips: (char, char), paths: Option<PathSet>) {
        land(&self.t, self.alice, &self.web, source, tips, paths).await;
    }

    async fn drain(&self) {
        drain(&self.t, &self.subscriber).await;
    }
}

/// The result is kept and re-pointed: same status, reason, conflicts and
/// seq, `checked_target_sha` at `tip`, `computed_target_sha` where it was
/// computed.
fn assert_carried(before: &StoredMergeability, after: &StoredMergeability, tip: char, from: char) {
    assert_eq!(after.status, before.status);
    assert_eq!(after.reason, before.reason);
    assert_eq!(after.conflicts, before.conflicts);
    assert_eq!(after.check_seq, before.check_seq, "no check was requested");
    assert_eq!(after.checked_head_sha, before.checked_head_sha);
    assert_eq!(after.checked_target_sha, Some(oid(tip)));
    assert_eq!(after.computed_target_sha, Some(oid(from)));
    assert_eq!(after.carried_from(), Some(&oid(from)));
}

/// The result is not kept: a check was requested (`unchecked`, a new seq),
/// the checked pair is what the last check saw. A row waiting for its
/// check is not shown as carried.
fn assert_rechecked(before: &StoredMergeability, after: &StoredMergeability) {
    assert_eq!(after.status, MergeabilityStatus::Unchecked);
    assert_eq!(
        after.check_seq,
        before.check_seq + 1,
        "a check was requested"
    );
    assert_eq!(after.checked_target_sha, before.checked_target_sha);
    assert_eq!(after.carried_from(), None);
}

/// Plan 18 S4: a landing whose paths cannot interact with a merge
/// request's keeps its decided result — mergeable or conflicting — and
/// re-points it at the new tip in the refresh, without a check;
/// `computed_target_sha` stays the tip the result was computed against,
/// over any number of landings. The merge request the landing does touch
/// is checked again, as before.
#[tokio::test]
async fn disjoint_mrs_are_carried_forward_with_their_computed_tip() {
    let w = world(true).await;
    let t = &w.t;
    let x = w.open("x").await;
    let y = w.open("y").await;
    let z = w.open("z").await;
    let touched = w.open("touched").await;
    let can = MergeabilityStatus::CanBeMerged;
    checked(t, x.id, can, 'b', Some(files(&["src/x.rs"]))).await;
    checked(t, y.id, can, 'b', Some(files(&["docs/y.md"]))).await;
    let cannot = MergeabilityStatus::CannotBeMerged;
    checked(t, z.id, cannot, 'b', Some(files(&["lib/z.rs"]))).await;
    checked(t, touched.id, can, 'b', Some(files(&["src/l.rs"]))).await;
    let before = [
        row(t, x.id).await,
        row(t, y.id).await,
        row(t, z.id).await,
        row(t, touched.id).await,
    ];
    assert_eq!(before[0].computed_target_sha, Some(oid('b')));
    assert_eq!(before[0].carried_from(), None, "a computed result");
    assert_eq!(
        stored_paths(&t.db, x.id).await.expect("paths"),
        Some(files(&["src/x.rs"])),
        "the set is stored with the result"
    );

    // `src/l.rs` lands: the same directory as x's file, another file.
    w.land("l1", ('b', 'c'), Some(files(&["src/l.rs"]))).await;
    w.drain().await;
    let after = [
        row(t, x.id).await,
        row(t, y.id).await,
        row(t, z.id).await,
        row(t, touched.id).await,
    ];
    for i in 0..3 {
        assert_carried(&before[i], &after[i], 'c', 'b');
    }
    assert_eq!(after[2].conflicts, ["README"], "the conflict list is kept");
    assert_rechecked(&before[3], &after[3]);
    let queued = queued_checks(t).await;
    assert!(
        queued.contains(&(touched.id.get(), after[3].check_seq)),
        "{queued:?}"
    );
    for (mr, kept) in [(&x, &after[0]), (&y, &after[1]), (&z, &after[2])] {
        assert!(
            !queued
                .iter()
                .any(|(id, seq)| *id == mr.id.get() && *seq > kept.check_seq),
            "no new job for a carried merge request: {queued:?}"
        );
    }

    // A second landing: carried again, the computed tip does not move.
    w.land("l2", ('c', 'd'), Some(files(&["other/q.rs"]))).await;
    w.drain().await;
    for (i, mr) in [&x, &y, &z].into_iter().enumerate() {
        assert_carried(&before[i], &row(t, mr.id).await, 'd', 'b');
    }
    // The touched one was not decided at `c`: nothing to carry.
    let again = row(t, touched.id).await;
    assert_eq!(again.status, MergeabilityStatus::Unchecked);
    assert_eq!(again.check_seq, after[3].check_seq + 1);

    // Its check runs at the new tip: a computed result again.
    checked(t, touched.id, can, 'd', Some(files(&["src/l.rs"]))).await;
    let computed = row(t, touched.id).await;
    assert_eq!(computed.computed_target_sha, Some(oid('d')));
    assert_eq!(computed.carried_from(), None);

    // A merge that found a conflict the carried answer did not show asks
    // for a computed one; a computed row is left alone.
    let asked = recheck_if_carried(&t.db, x.id, rid("req-train"))
        .await
        .expect("recheck");
    let x_before = &before[0];
    assert_eq!(asked, Some(x_before.check_seq + 1));
    assert_eq!(row(t, x.id).await.status, MergeabilityStatus::Unchecked);
    assert_eq!(
        recheck_if_carried(&t.db, touched.id, rid("req-train"))
            .await
            .expect("recheck"),
        None
    );
    assert_eq!(row(t, touched.id).await.check_seq, computed.check_seq);
    w.t.drop_now().await;
}

/// Plan 18 S4: an incomplete or unknown path set on either side is never
/// "does not interact" — the merge request is checked again.
#[tokio::test]
async fn an_incomplete_path_set_is_rechecked() {
    let w = world(true).await;
    let t = &w.t;
    let incomplete = w.open("incomplete").await;
    let unknown = w.open("unknown").await;
    let known = w.open("known").await;
    let can = MergeabilityStatus::CanBeMerged;
    checked(t, incomplete.id, can, 'b', Some(PathSet::incomplete())).await;
    checked(t, unknown.id, can, 'b', None).await;
    checked(t, known.id, can, 'b', Some(files(&["src/k.rs"]))).await;
    assert_eq!(
        stored_paths(&t.db, incomplete.id).await.expect("paths"),
        Some(PathSet::incomplete())
    );
    assert_eq!(stored_paths(&t.db, unknown.id).await.expect("paths"), None);
    let before = [
        row(t, incomplete.id).await,
        row(t, unknown.id).await,
        row(t, known.id).await,
    ];

    w.land("l1", ('b', 'c'), Some(files(&["docs/far.md"])))
        .await;
    w.drain().await;
    assert_rechecked(&before[0], &row(t, incomplete.id).await);
    assert_rechecked(&before[1], &row(t, unknown.id).await);
    let carried = row(t, known.id).await;
    assert_carried(&before[2], &carried, 'c', 'b');

    // The landing's own set is incomplete (too many paths, a
    // `.gitattributes`, …): everything is checked again.
    w.land("l2", ('c', 'd'), Some(PathSet::incomplete())).await;
    w.drain().await;
    let after = row(t, known.id).await;
    assert_eq!(after.status, MergeabilityStatus::Unchecked);
    assert_eq!(after.check_seq, carried.check_seq + 1);
    assert_eq!(after.checked_target_sha, Some(oid('c')), "not moved");
    w.t.drop_now().await;
}

/// Plan 18 S4: a `repository.pushed` payload without the landed block — a
/// direct push, a merge recorded before the block existed or without its
/// paths — checks every open merge request of the target again, exactly as
/// before the filter (a guard: it holds with and without the filter).
#[tokio::test]
async fn old_payloads_without_paths_recheck_like_today() {
    let w = world(true).await;
    let t = &w.t;
    let x = w.open("x").await;
    let y = w.open("y").await;
    let can = MergeabilityStatus::CanBeMerged;
    checked(t, x.id, can, 'b', Some(files(&["src/x.rs"]))).await;
    checked(t, y.id, can, 'b', Some(files(&["docs/y.md"]))).await;
    let before = [row(t, x.id).await, row(t, y.id).await];

    // A direct push to the target.
    let who = Pusher {
        request_id: rid("req-push-main"),
        user: w.alice,
        kind: forge_core::UserKind::Human,
        credential_id: None,
        source_ip: None,
        user_agent: None,
    };
    let push = PushRecord {
        repository_id: w.web.repository_id,
        updates: vec![PushedRef {
            name: branch("main"),
            old: oid('b'),
            new: oid('c'),
            kind: PushUpdateKind::FastForward,
            commits: Vec::new(),
            total_commits: 0,
            total_commits_partial: false,
        }],
    };
    let recorder: &dyn PushRecorder = &t.db;
    let done = recorder.record_push(&who, &push).await.expect("push");
    assert_eq!(done, forge_core::PushRecorded::Recorded);
    w.drain().await;
    assert_rechecked(&before[0], &row(t, x.id).await);
    assert_rechecked(&before[1], &row(t, y.id).await);

    // A merge whose record carries no paths.
    checked(t, x.id, can, 'c', Some(files(&["src/x.rs"]))).await;
    checked(t, y.id, can, 'c', Some(files(&["docs/y.md"]))).await;
    let before = [row(t, x.id).await, row(t, y.id).await];
    w.land("l1", ('c', 'd'), None).await;
    w.drain().await;
    assert_rechecked(&before[0], &row(t, x.id).await);
    assert_rechecked(&before[1], &row(t, y.id).await);
    let landed: Vec<bool> = sqlx::query_scalar(
        "SELECT payload ? 'landed' FROM forge.events WHERE event_type = 'repository.pushed'",
    )
    .fetch_all(&mut t.superuser().await)
    .await
    .expect("payloads");
    assert_eq!(landed, [false, false], "no payload has the block");
    w.t.drop_now().await;
}

/// Plan 18 S4, the kill switch: with `review.path_filter = false` a
/// landing that says its paths still checks every merge request again (a
/// guard: it holds with and without the filter).
#[tokio::test]
async fn the_filter_switched_off_rechecks_every_merge_request() {
    let w = world(false).await;
    let t = &w.t;
    let x = w.open("x").await;
    let can = MergeabilityStatus::CanBeMerged;
    checked(t, x.id, can, 'b', Some(files(&["src/x.rs"]))).await;
    let before = row(t, x.id).await;
    w.land("l1", ('b', 'c'), Some(files(&["docs/far.md"])))
        .await;
    w.drain().await;
    assert_rechecked(&before, &row(t, x.id).await);
    w.t.drop_now().await;
}

/// Plan 18 S4: the stored set is the checked head's. A new head between
/// the landing and its refresh is never carried — the version marks the
/// row `unchecked`, and even a row that still said "mergeable" would not
/// be: the statement takes only rows whose checked head is the merge
/// request's head.
#[tokio::test]
async fn a_new_head_between_the_landing_and_the_refresh_is_never_carried() {
    let w = world(true).await;
    let t = &w.t;
    let x = w.open("x").await;
    let can = MergeabilityStatus::CanBeMerged;
    checked(t, x.id, can, 'b', Some(files(&["src/x.rs"]))).await;
    let before = row(t, x.id).await;
    w.land("l1", ('b', 'c'), Some(files(&["docs/far.md"])))
        .await;
    // The source is pushed before the refresh handles the landing.
    let done = record_mr_version(&t.db, &system("req-v2-x"), x.id, &oid('e'), None)
        .await
        .expect("version");
    assert_eq!(done, VersionOutcome::Added { version: 2 });
    w.drain().await;
    let after = row(t, x.id).await;
    assert_eq!(after.status, MergeabilityStatus::Unchecked);
    assert_eq!(after.checked_target_sha, Some(oid('b')), "not carried");
    assert!(after.check_seq > before.check_seq, "a check was requested");

    // A writer that lost the race: the row says "mergeable" for the old
    // head at the current tip. The next landing must not carry it.
    t.exec_superuser(&format!(
        "UPDATE forge.merge_request_mergeability SET status = 'can_be_merged', reason = NULL, \
         checked_target_sha = '{}', computed_target_sha = '{}' WHERE merge_request_id = {}",
        oid('c').as_str(),
        oid('c').as_str(),
        x.id.get()
    ))
    .await;
    let stale = row(t, x.id).await;
    assert_eq!(stale.status, can);
    assert_eq!(stale.checked_head_sha, Some(oid('a')), "the old head");
    w.land("l2", ('c', 'd'), Some(files(&["docs/far.md"])))
        .await;
    w.drain().await;
    let after = row(t, x.id).await;
    assert_eq!(after.checked_target_sha, Some(oid('c')), "not carried");
    assert_eq!(after.status, MergeabilityStatus::Unchecked);
    assert_eq!(after.check_seq, stale.check_seq + 1);
    w.t.drop_now().await;
}

/// Plan 18 S4: two landings whose events are handled in one go. A merge
/// request is carried over each landing it cannot interact with, checked
/// again from the first one it may interact with, and never carried from
/// a tip its result does not answer for.
#[tokio::test]
async fn two_landings_before_one_refresh_carry_only_what_answers_each_tip() {
    let w = world(true).await;
    let t = &w.t;
    let apart = w.open("apart").await;
    let first = w.open("first").await;
    let second = w.open("second").await;
    let can = MergeabilityStatus::CanBeMerged;
    checked(t, apart.id, can, 'b', Some(files(&["src/apart.rs"]))).await;
    checked(t, first.id, can, 'b', Some(files(&["one/f.rs"]))).await;
    checked(t, second.id, can, 'b', Some(files(&["two/s.rs"]))).await;
    let before = [
        row(t, apart.id).await,
        row(t, first.id).await,
        row(t, second.id).await,
    ];
    w.land("l1", ('b', 'c'), Some(files(&["one/f.rs"]))).await;
    w.land("l2", ('c', 'd'), Some(files(&["two/s.rs"]))).await;
    w.drain().await;
    // Carried over both.
    assert_carried(&before[0], &row(t, apart.id).await, 'd', 'b');
    // Checked again from the first landing; the second finds no decision
    // at `c` and asks again.
    let first_after = row(t, first.id).await;
    assert_eq!(first_after.status, MergeabilityStatus::Unchecked);
    assert_eq!(first_after.checked_target_sha, Some(oid('b')));
    assert_eq!(first_after.check_seq, before[1].check_seq + 2);
    // Carried over the first (to `c`), checked again by the second.
    let second_after = row(t, second.id).await;
    assert_eq!(second_after.status, MergeabilityStatus::Unchecked);
    assert_eq!(second_after.checked_target_sha, Some(oid('c')));
    assert_eq!(second_after.carried_from(), None, "waiting for its check");
    assert_eq!(second_after.check_seq, before[2].check_seq + 1);
    w.t.drop_now().await;
}

/// Plan 18 S4: the carry-forward statement run twice for one landing (an
/// event delivered again, a queue batch's second event) moves a row once;
/// the second run finds it settled at the new tip and requests nothing.
/// A landing that arrives late (its old tip is not the row's) carries
/// nothing.
#[tokio::test]
async fn a_refresh_that_runs_twice_carries_once() {
    let w = world(true).await;
    let t = &w.t;
    let p = w.web.project_id;
    let x = w.open("x").await;
    let can = MergeabilityStatus::CanBeMerged;
    checked(t, x.id, can, 'b', Some(files(&["src/x.rs"]))).await;
    let before = row(t, x.id).await;
    let landing = |old: char, new: char| LandedPaths {
        target: branch("main"),
        old: oid(old),
        new: oid(new),
        paths: files(&["docs/far.md"]),
    };
    let mut conn = t.db.write_pool().acquire().await.expect("conn");
    let once = carry_forward_on(&mut conn, p, &landing('b', 'c'))
        .await
        .expect("carry");
    assert_eq!(once, [x.id.get()]);
    let twice = carry_forward_on(&mut conn, p, &landing('b', 'c'))
        .await
        .expect("carry again");
    assert_eq!(twice, Vec::<i64>::new());
    let settled = settled_on(&mut conn, p, "refs/heads/main", &oid('c'))
        .await
        .expect("settled");
    assert_eq!(settled, [x.id.get()]);
    assert_carried(&before, &row(t, x.id).await, 'c', 'b');
    // Out of order: a landing from a tip the row does not answer for.
    let late = carry_forward_on(&mut conn, p, &landing('e', 'f'))
        .await
        .expect("late");
    assert_eq!(late, Vec::<i64>::new());
    assert_carried(&before, &row(t, x.id).await, 'c', 'b');
    // Another branch's landing with the same tips moves nothing either.
    let mut other = landing('c', 'd');
    other.target = branch("release");
    let none = carry_forward_on(&mut conn, p, &other).await.expect("other");
    assert_eq!(none, Vec::<i64>::new());
    drop(conn);
    w.t.drop_now().await;
}

/// A finished check of `mr` stored the way the binary before the path
/// columns stores it (its `record_check` statement, column for column):
/// the result and the checked pair, nothing of the set, its stamp or the
/// computed tip.
async fn checked_the_old_way(t: &FreshDb, mr: MergeRequestId, head: char, target: char) {
    let seq = request_check(&t.db, mr, rid("req-old"))
        .await
        .expect("request")
        .expect("open");
    let start = begin_check(&t.db, mr, seq).await.expect("begin");
    assert!(matches!(start, CheckStart::Check(_)), "{start:?}");
    t.exec_superuser(&format!(
        "UPDATE forge.merge_request_mergeability SET status = 'can_be_merged', reason = NULL, \
         checked_head_sha = '{}', checked_target_sha = '{}', checked_at = now(), \
         conflicts = '{{}}', conflicts_truncated = false, confirmed_at = now(), \
         last_decided = COALESCE('can_be_merged', last_decided), updated_at = now() \
         WHERE merge_request_id = {}",
        oid(head).as_str(),
        oid(target).as_str(),
        mr.get()
    ))
    .await;
}

/// Review (HIGH): a stored path set belongs to the check that wrote it,
/// not to whatever the row says now. A worker that does not know the path
/// columns (the binary before them, while serve and worker are restarted
/// one after the other, or after a rollback) stores a result for a new
/// head — or for the same head at a later tip — and leaves the set, its
/// stamp and `computed_target_sha` of the earlier check in the row. Such
/// a row is neither carried on that set nor shown as carried: the next
/// landing checks it again.
#[tokio::test]
async fn a_result_stored_without_the_path_columns_is_rechecked_not_carried() {
    let w = world(true).await;
    let t = &w.t;
    let moved = w.open("moved").await;
    let same = w.open("same").await;
    let own = w.open("own").await;
    let can = MergeabilityStatus::CanBeMerged;
    checked(t, moved.id, can, 'b', Some(files(&["src/moved.rs"]))).await;
    checked(t, same.id, can, 'b', Some(files(&["src/same.rs"]))).await;
    checked(t, own.id, can, 'b', Some(files(&["src/own.rs"]))).await;
    let own_before = row(t, own.id).await;

    // A landing touches `same`: it is checked again, the others carried.
    w.land("l0", ('b', 'c'), Some(files(&["src/same.rs"])))
        .await;
    w.drain().await;
    assert_carried(&own_before, &row(t, own.id).await, 'c', 'b');
    // The old worker runs that check: head `a` at the new tip `c`. The
    // set and the computed tip `b` of the earlier check stay in the row.
    checked_the_old_way(t, same.id, 'a', 'c').await;
    // `moved` gets a new head, checked by the old worker as well: the
    // set in the row is the old head's.
    let done = record_mr_version(&t.db, &system("req-v2-moved"), moved.id, &oid('e'), None)
        .await
        .expect("version");
    assert_eq!(done, VersionOutcome::Added { version: 2 });
    checked_the_old_way(t, moved.id, 'e', 'c').await;

    let before = [row(t, moved.id).await, row(t, same.id).await];
    for (stale, head) in before.iter().zip(['e', 'a']) {
        assert_eq!(stale.status, can);
        assert_eq!(stale.checked_head_sha, Some(oid(head)));
        assert_eq!(stale.checked_target_sha, Some(oid('c')));
        assert_eq!(
            stale.carried_from(),
            None,
            "computed at `c` by a writer that left the earlier tip behind"
        );
        assert_eq!(stale.computed_target_sha, None);
    }
    for mr in [&moved, &same] {
        assert_eq!(
            stored_paths(&t.db, mr.id).await.expect("paths"),
            None,
            "the set of an earlier check is unknown"
        );
        assert_eq!(
            recheck_if_carried(&t.db, mr.id, rid("req-not-carried"))
                .await
                .expect("recheck"),
            None,
            "not a carried row"
        );
    }

    // A landing far from every stored set: only the row that is its own
    // is carried.
    w.land("l1", ('c', 'd'), Some(files(&["docs/far.md"])))
        .await;
    w.drain().await;
    assert_carried(&own_before, &row(t, own.id).await, 'd', 'b');
    assert_rechecked(&before[0], &row(t, moved.id).await);
    assert_rechecked(&before[1], &row(t, same.id).await);

    // The new binary's check makes the row its own again.
    checked(t, same.id, can, 'd', Some(files(&["src/same.rs"]))).await;
    let again = row(t, same.id).await;
    assert_eq!(again.computed_target_sha, Some(oid('d')));
    assert_eq!(
        stored_paths(&t.db, same.id).await.expect("paths"),
        Some(files(&["src/same.rs"]))
    );
    w.land("l2", ('d', 'f'), Some(files(&["docs/far.md"])))
        .await;
    w.drain().await;
    assert_carried(&again, &row(t, same.id).await, 'f', 'd');
    w.t.drop_now().await;
}

/// Review (MEDIUM): a stored set says which version of the rule it was
/// built for. A set of another version is unknown — the merge request is
/// checked again, as the module documentation promises.
#[tokio::test]
async fn a_set_of_another_rule_version_is_rechecked() {
    let w = world(true).await;
    let t = &w.t;
    let other = w.open("other").await;
    let current = w.open("current").await;
    let can = MergeabilityStatus::CanBeMerged;
    checked(t, other.id, can, 'b', Some(files(&["src/other.rs"]))).await;
    checked(t, current.id, can, 'b', Some(files(&["src/current.rs"]))).await;
    let version: i32 = sqlx::query_scalar(&format!(
        "SELECT paths_rule FROM forge.merge_request_mergeability WHERE merge_request_id = {}",
        other.id.get()
    ))
    .fetch_one(&mut t.superuser().await)
    .await
    .expect("rule");
    assert_eq!(
        u32::try_from(version).ok(),
        Some(forge_core::merge_paths::RULE_VERSION),
        "the check stores the version with the set"
    );
    t.exec_superuser(&format!(
        "UPDATE forge.merge_request_mergeability SET paths_rule = {} WHERE merge_request_id = {}",
        version + 1,
        other.id.get()
    ))
    .await;
    let before = [row(t, other.id).await, row(t, current.id).await];
    assert_eq!(stored_paths(&t.db, other.id).await.expect("paths"), None);

    w.land("l1", ('b', 'c'), Some(files(&["docs/far.md"])))
        .await;
    w.drain().await;
    assert_rechecked(&before[0], &row(t, other.id).await);
    assert_carried(&before[1], &row(t, current.id).await, 'c', 'b');
    w.t.drop_now().await;
}

// --- the SQL twin -------------------------------------------------------

/// Few segments, so that sets collide: prefixes that are no directory
/// boundary (`a` / `ab`), case pairs, Unicode with and without a case
/// mapping, a decomposed form, and — rarely — global paths.
fn segment() -> impl Strategy<Value = String> {
    prop_oneof![
        8 => Just("a"),
        8 => Just("ab"),
        8 => Just("A"),
        8 => Just("b"),
        4 => Just("a b"),
        4 => Just("a."),
        4 => Just("\u{e9}"),
        4 => Just("\u{c9}"),
        4 => Just("e\u{301}"),
        4 => Just("\u{df}"),
        4 => Just("\u{131}"),
        4 => Just("\u{130}"),
        4 => Just("\u{65e5}\u{672c}"),
        4 => Just("x.rs"),
        1 => Just(".gitattributes"),
        1 => Just(".forge"),
    ]
    .prop_map(str::to_owned)
}

fn path() -> impl Strategy<Value = String> {
    prop::collection::vec(segment(), 1..4).prop_map(|parts| parts.join("/"))
}

type Columns = (
    Option<Vec<String>>,
    Option<Vec<String>>,
    Option<Vec<String>>,
    Option<bool>,
);

fn columns_of(set: &PathSet) -> Columns {
    (
        Some(set.files().to_vec()),
        Some(set.dirs().to_vec()),
        Some(set.deleted_dirs().to_vec()),
        Some(set.is_complete()),
    )
}

/// A set as the product builds it (complete, or incomplete with empty
/// lists: the cap, a special entry, a global path).
fn built() -> impl Strategy<Value = Columns> {
    (
        prop::collection::vec((path(), any::<bool>(), prop::bool::weighted(0.02)), 0..6),
        prop_oneof![Just(2_usize), Just(5000_usize)],
    )
        .prop_map(|(entries, max)| {
            let entries = entries
                .into_iter()
                .map(|(path, deleted, special)| ChangedPath {
                    path,
                    deleted,
                    special,
                });
            columns_of(&PathSet::build(entries, max).unwrap_or_else(|_| PathSet::incomplete()))
        })
}

/// Keys nothing in the product writes: trailing slashes, the empty string,
/// loose text.
fn raw_key() -> impl Strategy<Value = String> {
    prop_oneof![
        4 => path(),
        2 => path().prop_map(|p| format!("{p}/")),
        1 => Just(String::new()),
        2 => "[a-c/]{0,4}",
    ]
}

/// Columns as a row or a payload could hold them, nulls included.
fn raw() -> impl Strategy<Value = Columns> {
    let list = || prop::option::weighted(0.9, prop::collection::vec(raw_key(), 0..4));
    (
        list(),
        list(),
        list(),
        prop::option::weighted(0.9, prop::bool::weighted(0.85)),
    )
}

/// The names of the git-backed property
/// (`forge-git/src/mergeability_path_rule_tests.rs`): prefixes that are
/// no directory boundary and a case pair.
fn name() -> impl Strategy<Value = &'static str> {
    prop_oneof![
        4 => Just("a"),
        3 => Just("b"),
        2 => Just("ab"),
        2 => Just("A"),
        1 => Just("c"),
    ]
}

fn short_path() -> impl Strategy<Value = String> {
    prop::collection::vec(name(), 1..4).prop_map(|parts| parts.join("/"))
}

/// One change as git lists it (`(path, deleted)` entries): a file
/// written, a file deleted, a file replaced by a directory, a directory
/// deleted, a rename.
fn listed_change() -> impl Strategy<Value = Vec<(String, bool)>> {
    prop_oneof![
        3 => short_path().prop_map(|p| vec![(p, false)]),
        2 => short_path().prop_map(|p| vec![(p, true)]),
        2 => (short_path(), name()).prop_map(|(p, n)| vec![(format!("{p}/{n}"), false), (p, true)]),
        2 => (short_path(), prop::collection::vec(name(), 1..4)).prop_map(|(dir, names)| {
            names
                .into_iter()
                .map(|n| (format!("{dir}/{n}"), true))
                .collect()
        }),
        2 => (short_path(), short_path()).prop_map(|(from, to)| vec![(from, true), (to, false)]),
    ]
}

/// A set built from a few such changes — the sets the git-backed
/// property decides on, here against the SQL twin.
fn changed() -> impl Strategy<Value = Columns> {
    prop::collection::vec(listed_change(), 0..4).prop_map(|changes| {
        let entries = changes
            .into_iter()
            .flatten()
            .map(|(path, deleted)| ChangedPath {
                path,
                deleted,
                special: false,
            });
        columns_of(&PathSet::build(entries, 5000).unwrap_or_else(|_| PathSet::incomplete()))
    })
}

fn side() -> impl Strategy<Value = Columns> {
    prop_oneof![
        1 => Just((None, None, None, None)),
        6 => built(),
        4 => raw(),
        5 => changed(),
    ]
}

/// Plan 18 S4: the carry-forward statement decides with SQL what
/// `may_interact` decides in Rust. For every generated pair of sets —
/// built ones, raw ones, change sets as git lists them (deletions, a
/// file replaced by a directory, a directory deleted, a rename), empty
/// lists, nulls, non-boundary prefixes, trailing slashes, Unicode — the
/// two answers are the same, and the SQL answer is never NULL.
#[tokio::test]
async fn may_interact_matches_its_sql_twin() {
    let t = FreshDb::migrated().await;
    let mut conn = t.db.write_pool().acquire().await.expect("conn");
    let sql = format!(
        "SELECT {} FROM (SELECT $1::text[] AS head_files, $2::text[] AS head_dirs, \
         $3::text[] AS head_deleted_dirs, $4::boolean AS paths_complete) g",
        interacts_sql("$5", "$6", "$7", "$8")
    );
    let mut runner = TestRunner::default();
    let strategy = (side(), side());
    let (mut interacting, mut apart) = (0_u32, 0_u32);
    for case in 0..600 {
        let (mr, landed) = strategy.new_tree(&mut runner).expect("a case").current();
        let rust = may_interact(
            PathSet::from_columns(mr.0.clone(), mr.1.clone(), mr.2.clone(), mr.3).as_ref(),
            PathSet::from_columns(
                landed.0.clone(),
                landed.1.clone(),
                landed.2.clone(),
                landed.3,
            )
            .as_ref(),
        );
        let twin: Option<bool> = sqlx::query_scalar(&sql)
            .bind(&mr.0)
            .bind(&mr.1)
            .bind(&mr.2)
            .bind(mr.3)
            .bind(&landed.0)
            .bind(&landed.1)
            .bind(&landed.2)
            .bind(landed.3)
            .fetch_one(&mut *conn)
            .await
            .expect("twin");
        assert_eq!(
            twin,
            Some(rust),
            "case {case}: merge request {mr:?}, landed {landed:?}"
        );
        if rust {
            interacting += 1;
        } else {
            apart += 1;
        }
    }
    assert!(
        interacting > 50 && apart > 50,
        "the generator covers both answers: {interacting} / {apart}"
    );
    drop(conn);
    t.drop_now().await;
}

/// The twin's fixed points, by name (what the property samples): a
/// non-boundary prefix is no interaction, a file under a landed
/// directory deletion is, and an unknown or incomplete side always is.
#[tokio::test]
async fn the_sql_twin_decides_the_named_cases() {
    let t = FreshDb::migrated().await;
    let mut conn = t.db.write_pool().acquire().await.expect("conn");
    let sql = format!(
        "SELECT {} FROM (SELECT $1::text[] AS head_files, $2::text[] AS head_dirs, \
         $3::text[] AS head_deleted_dirs, $4::boolean AS paths_complete) g",
        interacts_sql("$5", "$6", "$7", "$8")
    );
    let cases: [(&str, Option<PathSet>, Option<PathSet>, bool); 9] = [
        ("apart", Some(files(&["a/x"])), Some(files(&["b/y"])), false),
        (
            "non-boundary prefix",
            Some(files(&["a"])),
            Some(files(&["ab"])),
            false,
        ),
        (
            "same directory",
            Some(files(&["d/x"])),
            Some(files(&["d/y"])),
            false,
        ),
        (
            "same file",
            Some(files(&["d/x"])),
            Some(files(&["d/x"])),
            true,
        ),
        (
            "case only",
            Some(files(&["d/X"])),
            Some(files(&["D/x"])),
            true,
        ),
        (
            "file and directory",
            Some(files(&["a"])),
            Some(files(&["a/b"])),
            true,
        ),
        (
            "added under a deleted directory",
            Some(files(&["d/new"])),
            Some(set(&[("d/old", true)])),
            true,
        ),
        (
            "incomplete",
            Some(PathSet::incomplete()),
            Some(files(&["b"])),
            true,
        ),
        ("unknown", Some(files(&["a"])), None, true),
    ];
    for (name, mr, landed, expected) in cases {
        assert_eq!(
            may_interact(mr.as_ref(), landed.as_ref()),
            expected,
            "{name}"
        );
        let twin: Option<bool> = sqlx::query_scalar(&sql)
            .bind(mr.as_ref().map(PathSet::files))
            .bind(mr.as_ref().map(PathSet::dirs))
            .bind(mr.as_ref().map(PathSet::deleted_dirs))
            .bind(mr.as_ref().map(PathSet::is_complete))
            .bind(landed.as_ref().map(PathSet::files))
            .bind(landed.as_ref().map(PathSet::dirs))
            .bind(landed.as_ref().map(PathSet::deleted_dirs))
            .bind(landed.as_ref().map(PathSet::is_complete))
            .fetch_one(&mut *conn)
            .await
            .expect("twin");
        assert_eq!(twin, Some(expected), "{name}");
    }
    drop(conn);
    t.drop_now().await;
}
