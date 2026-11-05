#![allow(clippy::expect_used, clippy::unwrap_used)] // tests may fail loudly

use std::time::Duration;

use forge_core::ids::{ProjectId, UserId};
use forge_core::{ObjectId, RefName, RequestId};

use super::*;
use crate::change::{Actor, ChangeContext};
use crate::projects::{self, NewProject};
use crate::review::{
    close_mr_decided, edit_mr_decided, open_mr_decided, record_mr_version, reopen_mr_decided,
    MergeRequestPatch, NewMergeRequest, StoredMergeRequest,
};
use crate::test_support::FreshDb;
use crate::users::{self, NewUser};

#[test]
fn statuses_and_outcomes_round_trip() {
    for s in ScanStatus::ALL {
        assert_eq!(ScanStatus::parse(s.as_str()), Some(s));
    }
    for o in PairOutcome::ALL {
        assert_eq!(PairOutcome::parse(o.as_str()), Some(o));
    }
    assert_eq!(ScanStatus::parse("done"), None);
    assert!(ScanStatus::Incomplete.is_finished());
    assert!(!ScanStatus::Scanning.is_finished());
}

#[test]
fn the_job_payload_round_trips_and_refuses_anything_else() {
    let job = conflict_scan_job(
        MergeRequestId::new(7),
        3,
        RequestId::parse("req-j").expect("id"),
        None,
    )
    .expect("job");
    assert_eq!(job.kind.as_str(), CONFLICT_SCAN_JOB);
    assert_eq!(
        job.dedup_key.as_ref().map(crate::jobs::DedupKey::as_str),
        Some("mr_conflict_scan:7:3")
    );
    assert_eq!(
        conflict_scan_payload(&job.payload).expect("payload"),
        ConflictScanRequest {
            merge_request: MergeRequestId::new(7),
            scan_seq: 3
        }
    );
    for bad in [
        serde_json::json!({}),
        serde_json::json!({"merge_request_id": 7, "scan_seq": 0}),
        serde_json::json!({"merge_request_id": "7", "scan_seq": 1}),
    ] {
        assert!(conflict_scan_payload(&bad).is_err(), "{bad}");
    }
}

fn ctx(actor: Actor, request: &str) -> ChangeContext {
    ChangeContext::new(actor, RequestId::parse(request).expect("request id"))
}

fn system(request: &str) -> ChangeContext {
    ctx(Actor::System, request)
}

fn branch(name: &str) -> RefName {
    RefName::parse(&format!("refs/heads/{name}")).expect("branch")
}

fn oid(c: char) -> ObjectId {
    ObjectId::parse(&c.to_string().repeat(40)).expect("oid")
}

struct World {
    t: FreshDb,
    alice: UserId,
    project: ProjectId,
}

async fn world() -> World {
    let t = FreshDb::migrated().await;
    let alice = users::create(
        &t.db,
        &system("req-user-alice"),
        &NewUser::human("alice", "alice", "alice@example.test"),
    )
    .await
    .expect("alice");
    let web = projects::create(
        &t.db,
        &system("req-project-web"),
        &NewProject::new("web", alice.namespace_id, "Web", alice.user_id),
    )
    .await
    .expect("web");
    World {
        t,
        alice: alice.user_id,
        project: web.project_id,
    }
}

impl World {
    /// An open merge request `source` → `target` with version `head`.
    async fn mr(&self, source: &str, target: &str, head: char) -> StoredMergeRequest {
        let new = NewMergeRequest {
            title: format!("Merge {source}"),
            description: "synthetic".to_owned(),
            source_branch: branch(source),
            target_branch: branch(target),
            draft: false,
        };
        let (mr, ()) = open_mr_decided(
            &self.t.db,
            &ctx(Actor::User(self.alice), &format!("req-open-{source}")),
            self.project,
            self.alice,
            &new,
            |_| Ok(()),
        )
        .await
        .expect("open");
        self.push(&mr, head).await;
        mr
    }

    async fn push(&self, mr: &StoredMergeRequest, head: char) {
        record_mr_version(
            &self.t.db,
            &system(&format!("req-v-{}-{head}", mr.iid)),
            mr.id,
            &oid(head),
            None,
        )
        .await
        .expect("version");
    }

    /// Request, begin and record a scan of `mr` at `head` with `pairs`.
    async fn scan(
        &self,
        mr: &StoredMergeRequest,
        head: char,
        pairs: Vec<PairResult>,
        request: &str,
    ) -> ScanRecorded {
        let seq = request_scan(&self.t.db, mr.id, RequestId::parse("req-q").expect("id"))
            .await
            .expect("request")
            .expect("open");
        assert_eq!(
            begin_scan(&self.t.db, mr.id, seq).await.expect("begin"),
            ScanStart::Scan
        );
        let result = ScanResult {
            status: ScanStatus::Scanned,
            reason: None,
            head: Some(oid(head)),
            target_branch: mr.target_branch.clone(),
            peers: i32::try_from(pairs.len()).expect("small"),
            pairs_tried: i32::try_from(pairs.len()).expect("small"),
            pairs_skipped: 0,
            pairs,
        };
        record_scan(&self.t.db, &system(request), mr.id, seq, &result)
            .await
            .expect("record")
    }

    async fn predicted_events(&self) -> Vec<(i64, serde_json::Value, Option<String>)> {
        sqlx::query_as(
            "SELECT aggregate_id, payload, dedup_key FROM forge.events \
             WHERE event_type = 'merge_request.conflict_predicted' ORDER BY id",
        )
        .fetch_all(&mut self.t.superuser().await)
        .await
        .expect("events")
    }

    async fn scan_row(&self, mr: &StoredMergeRequest) -> StoredScan {
        conflict_scan(&self.t.db, mr.id)
            .await
            .expect("read")
            .expect("row")
    }
}

fn pair(other: &StoredMergeRequest, head: char, outcome: PairOutcome) -> PairResult {
    PairResult {
        other: other.id,
        other_iid: other.iid,
        other_head: oid(head),
        outcome,
        reason: None,
        conflicts: if outcome == PairOutcome::Conflict {
            vec!["README.md".to_owned()]
        } else {
            Vec::new()
        },
        conflicts_truncated: false,
    }
}

/// C-2: a pair that ENTERS conflict publishes one event per side (each
/// naming the other); staying in conflict publishes nothing; leaving and
/// entering again publishes with the next prediction_seq.
#[tokio::test]
async fn a_pair_that_enters_conflict_is_predicted_once_for_both_sides() {
    let w = world().await;
    let a = w.mr("a", "main", 'a').await;
    let b = w.mr("b", "main", 'b').await;
    let done = w
        .scan(
            &a,
            'a',
            vec![pair(&b, 'b', PairOutcome::Conflict)],
            "req-s1",
        )
        .await;
    assert_eq!(
        done,
        ScanRecorded::Stored {
            predicted: vec![b.id],
            skipped: 0
        }
    );
    let events = w.predicted_events().await;
    assert_eq!(events.len(), 2, "{events:?}");
    assert_eq!(events[0].0, a.id.get());
    assert_eq!(events[0].1["other_merge_request_id"], b.id.get());
    assert_eq!(events[0].1["other_iid"], b.iid);
    assert_eq!(events[0].1["conflict_files"], 1);
    assert_eq!(events[1].0, b.id.get());
    assert_eq!(events[1].1["other_merge_request_id"], a.id.get());
    assert_eq!(events[1].1["head_sha"], "b".repeat(40));
    assert_eq!(
        events[0].2.as_deref(),
        Some(
            format!(
                "merge_request_conflict_predicted:{}:{}:1",
                a.id.get(),
                b.id.get()
            )
            .as_str()
        )
    );
    // Both sides read it while both heads are current.
    for (me, other) in [(&a, &b), (&b, &a)] {
        let read = conflict_predictions(&w.t.db, me.id).await.expect("read");
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].other, other.id);
        assert_eq!(read[0].outcome, PairOutcome::Conflict);
        assert_eq!(read[0].conflicts, ["README.md"]);
    }
    // B's own scan finds the same conflict: no second prediction.
    let again = w
        .scan(
            &b,
            'b',
            vec![pair(&a, 'a', PairOutcome::Conflict)],
            "req-s2",
        )
        .await;
    assert_eq!(
        again,
        ScanRecorded::Stored {
            predicted: Vec::new(),
            skipped: 0
        }
    );
    assert_eq!(w.predicted_events().await.len(), 2);
    // Clean, then conflicting again: predicted with seq 2.
    w.scan(&a, 'a', vec![pair(&b, 'b', PairOutcome::Clean)], "req-s3")
        .await;
    assert!(conflict_predictions(&w.t.db, a.id)
        .await
        .expect("read")
        .is_empty());
    w.scan(
        &a,
        'a',
        vec![pair(&b, 'b', PairOutcome::Conflict)],
        "req-s4",
    )
    .await;
    let events = w.predicted_events().await;
    assert_eq!(events.len(), 4);
    assert_eq!(events[2].1["prediction_seq"], 2);
    // One audit row per side, actor system, on the project.
    let audits: Vec<(String, String)> = sqlx::query_as(
        "SELECT action, actor_kind FROM forge.audit_log WHERE request_id = 'req-s4' ORDER BY id",
    )
    .fetch_all(&mut w.t.superuser().await)
    .await
    .expect("audit");
    assert_eq!(
        audits,
        vec![
            (
                "merge_request.conflict_predicted".to_owned(),
                "system".to_owned()
            );
            2
        ]
    );
    w.t.drop_now().await;
}

/// A prediction is trusted only while both heads are current: a new head on
/// either side hides it until a scan of the new head stores the pair.
#[tokio::test]
async fn a_prediction_about_an_old_head_is_not_read() {
    let w = world().await;
    let a = w.mr("a", "main", 'a').await;
    let b = w.mr("b", "main", 'b').await;
    w.scan(
        &a,
        'a',
        vec![pair(&b, 'b', PairOutcome::Conflict)],
        "req-s1",
    )
    .await;
    w.push(&b, 'c').await;
    assert!(conflict_predictions(&w.t.db, a.id)
        .await
        .expect("read")
        .is_empty());
    // Back at the head the scan saw: current again.
    w.push(&b, 'b').await;
    assert_eq!(
        conflict_predictions(&w.t.db, a.id)
            .await
            .expect("read")
            .len(),
        1
    );
    w.t.drop_now().await;
}

/// The other side moved after the scan read it: the pair is not written
/// (no event about a head that is gone); its own scan writes it.
#[tokio::test]
async fn a_pair_whose_other_side_moved_is_skipped() {
    let w = world().await;
    let a = w.mr("a", "main", 'a').await;
    let b = w.mr("b", "main", 'b').await;
    w.push(&b, 'c').await;
    let done = w
        .scan(
            &a,
            'a',
            vec![pair(&b, 'b', PairOutcome::Conflict)],
            "req-s1",
        )
        .await;
    assert_eq!(
        done,
        ScanRecorded::Stored {
            predicted: Vec::new(),
            skipped: 1
        }
    );
    assert!(w.predicted_events().await.is_empty());
    w.t.drop_now().await;
}

/// A run whose seq is no longer the latest, or whose head moved, writes
/// nothing; `begin_scan` of an old seq is superseded.
#[tokio::test]
async fn a_superseded_or_moved_scan_writes_nothing() {
    let w = world().await;
    let a = w.mr("a", "main", 'a').await;
    let b = w.mr("b", "main", 'b').await;
    let origin = RequestId::parse("req-q").expect("id");
    let old = request_scan(&w.t.db, a.id, origin.clone())
        .await
        .expect("request")
        .expect("open");
    let new = request_scan(&w.t.db, a.id, origin)
        .await
        .expect("request")
        .expect("open");
    assert_eq!(new, old + 1);
    assert_eq!(
        begin_scan(&w.t.db, a.id, old).await.expect("begin"),
        ScanStart::Superseded
    );
    let result = |head| ScanResult {
        status: ScanStatus::Scanned,
        reason: None,
        head: Some(oid(head)),
        target_branch: branch("main"),
        peers: 1,
        pairs_tried: 1,
        pairs_skipped: 0,
        pairs: vec![pair(&b, 'b', PairOutcome::Conflict)],
    };
    let ctx = system("req-old");
    assert_eq!(
        record_scan(&w.t.db, &ctx, a.id, old, &result('a'))
            .await
            .expect("old"),
        ScanRecorded::Superseded
    );
    assert_eq!(
        record_scan(&w.t.db, &ctx, a.id, new, &result('d'))
            .await
            .expect("moved head"),
        ScanRecorded::Superseded
    );
    assert!(w.predicted_events().await.is_empty());
    assert_eq!(w.scan_row(&a).await.status, ScanStatus::Pending);
    w.t.drop_now().await;
}

/// A retarget and a reopen request a scan in their own transaction.
#[tokio::test]
async fn a_retarget_and_a_reopen_request_a_scan() {
    let w = world().await;
    let a = w.mr("a", "main", 'a').await;
    assert!(conflict_scan(&w.t.db, a.id).await.expect("read").is_none());
    let user = |r: &str| ctx(Actor::User(w.alice), r);
    let patch = MergeRequestPatch {
        target_branch: Some(branch("release")),
        ..MergeRequestPatch::default()
    };
    let _ = edit_mr_decided(
        &w.t.db,
        &user("req-retarget"),
        w.project,
        a.iid,
        w.alice,
        &patch,
        |_| Ok(()),
    )
    .await
    .expect("retarget");
    let first = w.scan_row(&a).await;
    assert_eq!(first.status, ScanStatus::Pending);
    let _ = close_mr_decided(
        &w.t.db,
        &user("req-close"),
        w.project,
        a.iid,
        w.alice,
        |_| Ok(()),
    )
    .await
    .expect("close");
    let _ = reopen_mr_decided(
        &w.t.db,
        &user("req-reopen"),
        w.project,
        a.iid,
        w.alice,
        None,
        |_| Ok(()),
    )
    .await
    .expect("reopen");
    let second = w.scan_row(&a).await;
    assert_eq!(second.scan_seq, first.scan_seq + 1);
    let jobs: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM forge.jobs WHERE kind = 'merge_request.conflict_scan'",
    )
    .fetch_one(&mut w.t.superuser().await)
    .await
    .expect("jobs");
    assert_eq!(jobs, 2);
    w.t.drop_now().await;
}

/// No silent failures: a run that failed for good ends as `job_failed`; a
/// row left unfinished with no live job is reaped as `job_stale`.
#[tokio::test]
async fn a_stale_or_failed_scan_is_ended_with_its_reason() {
    let w = world().await;
    let a = w.mr("a", "main", 'a').await;
    let seq = request_scan(&w.t.db, a.id, RequestId::parse("req-q").expect("id"))
        .await
        .expect("request")
        .expect("open");
    assert!(fail_scan(&w.t.db, a.id, seq).await.expect("fail"));
    let row = w.scan_row(&a).await;
    assert_eq!(
        (row.status, row.reason.as_deref()),
        (ScanStatus::Failed, Some("job_failed"))
    );
    assert!(!fail_scan(&w.t.db, a.id, seq).await.expect("again"));
    // A new request whose job is gone (deleted here, as a dead-lettered
    // and pruned job would be) is reaped.
    let seq = request_scan(&w.t.db, a.id, RequestId::parse("req-q2").expect("id"))
        .await
        .expect("request")
        .expect("open");
    let mut su = w.t.superuser().await;
    sqlx::query("DELETE FROM forge.jobs WHERE kind = 'merge_request.conflict_scan'")
        .execute(&mut su)
        .await
        .expect("drop jobs");
    let none = reap_stale_scans(&w.t.db, Duration::from_secs(3600), 10)
        .await
        .expect("young");
    assert!(none.is_empty(), "not old enough yet");
    let reaped = reap_stale_scans(&w.t.db, Duration::ZERO, 10)
        .await
        .expect("reap");
    assert_eq!(reaped, vec![(a.id, seq)]);
    let row = w.scan_row(&a).await;
    assert_eq!(
        (row.status, row.reason.as_deref()),
        (ScanStatus::Failed, Some("job_stale"))
    );
    w.t.drop_now().await;
}
