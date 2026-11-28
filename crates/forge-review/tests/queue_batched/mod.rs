//! Plan 18 S10 (ADR-0037 (d) 5): the landing section without a process per
//! member. Under the merge lock a batch reads every source tip with one
//! `for-each-ref`, uses the approval git facts its build read (checked
//! again by sha), decides every member in ONE transaction and records
//! every merge in ONE transaction. What it writes is what landing the
//! members one by one wrote: per merge request, in train order, the
//! `merge_request.merged` and `repository.pushed` audit rows and events of
//! the requester. A member refused under the lock leaves nothing written
//! (the decisions are one transaction); the batch is rebuilt without it.

use super::*;

impl World {
    /// A second web session of dave (a credential that can be revoked
    /// apart from the one the other entries use).
    async fn second_session(&self) -> i64 {
        forge_db::sessions::issue(
            &self.t.db,
            &system("req-session-2"),
            &forge_db::sessions::NewSession {
                user: self.dev,
                idle: Duration::from_secs(3600),
                absolute: Duration::from_secs(8 * 3600),
                oidc: None,
            },
        )
        .await
        .expect("session")
        .session_id
    }

    /// dave puts `mr` in the queue with web session `session`.
    async fn enqueue_with(&self, mr: &StoredMergeRequest, session_id: i64) {
        let who = session(self.dev);
        let proof = self
            .mr_proof(&who, Action::MergeQueueEnqueue, mr)
            .await
            .expect("allowed");
        self.review
            .enqueue(
                &request(&format!("req-enqueue-{}", mr.iid)),
                CredentialRef::Session(session_id),
                &proof,
                ready(self.repo),
                &QueueAsk {
                    sha: oid(mr.head_sha.as_deref().expect("head")),
                    method: None,
                },
            )
            .await
            .expect("enqueue");
    }

    /// Move branch `name` one commit ahead without telling the review side
    /// (no refresh: the merge request's recorded head stays).
    fn move_source(&self, name: &str) {
        let tip = self.tip(name);
        let mut all = self.base_files.to_vec();
        all.push(("moved.txt", "moved\n"));
        let moved = commit(&self.path, &all, Some(&tip));
        plumb(
            &self.path,
            &["update-ref", &format!("refs/heads/{name}"), &moved],
            None,
        );
        self.browser.invalidate(self.repo);
    }

    async fn attempts(&self) -> i64 {
        count(
            &self.t,
            "SELECT count(*) AS n FROM forge.merge_request_merges",
        )
        .await
    }
}

/// The golden record of a per-member landing, from the attempts: for each
/// merge request in train order its `merge_request.merged` row then the
/// target's `repository.pushed` row (actor dave, the attempt's request id
/// and credential), and the two events deduplicated by the merge id.
#[tokio::test]
async fn a_batched_landing_writes_the_same_audit_rows_and_events_as_per_member() {
    let mut w = world().await;
    let mut mrs = Vec::new();
    for name in ["ga", "gb", "gc"] {
        mrs.push(w.mr(name, &[(&format!("{name}.txt"), "x\n")]).await);
    }
    for mr in &mrs {
        w.enqueue(mr).await.expect("enqueue");
    }
    let worker = w.worker("w1");
    let logs = Captured::default();
    let sizes = {
        let _guard = logs.install();
        w.drive(&worker, &BTreeSet::new()).await
    };
    assert_eq!(sizes, [3]);
    let mut expected_audit = Vec::new();
    let mut expected_events = Vec::new();
    let mut old = w.base.clone();
    let mut reqs = BTreeSet::new();
    for mr in &mrs {
        let entry = w.entry_of(mr).await;
        assert_eq!(entry.state, EntryState::Merged);
        let id = entry.merge_id.expect("merge");
        let attempt = merges::merge_attempt(&w.t.db, id)
            .await
            .expect("read")
            .expect("attempt");
        assert_eq!(attempt.state, MergeState::Applied);
        let new = attempt.new_sha.as_ref().expect("new").as_str().to_owned();
        assert_eq!(
            attempt.expected_old.as_ref().map(ObjectId::as_str),
            Some(old.as_str())
        );
        let req = attempt.request_id.as_str().to_owned();
        reqs.insert(req.clone()); // clone: the expected rows own it too
        expected_audit.push(serde_json::json!({
            "action": "merge_request.merged", "actor_kind": "user",
            "actor_user_id": w.dev.get(), "credential_id": w.session,
            "request_id": req, "merge_id": id, "target_sha": old, "merged_sha": new,
        }));
        expected_audit.push(serde_json::json!({
            "action": "repository.pushed", "actor_kind": "user",
            "actor_user_id": w.dev.get(), "credential_id": w.session,
            "request_id": req, "merge_id": null, "target_sha": null, "merged_sha": null,
        }));
        expected_events.push(serde_json::json!({
            "event_type": "merge_request.merged", "aggregate_id": mr.id.get(),
            "actor_user_id": w.dev.get(), "request_id": req,
            "dedup_key": format!("merge_request_merged:{id}"),
        }));
        expected_events.push(serde_json::json!({
            "event_type": "repository.pushed", "aggregate_id": w.repo.get(),
            "actor_user_id": w.dev.get(), "request_id": req,
            "dedup_key": format!("merge_pushed:{id}"),
        }));
        old = new;
    }
    assert_eq!(old, w.tip("main"), "the target is the last step");
    let reqs: Vec<String> = reqs.into_iter().map(|r| format!("'{r}'")).collect();
    let audit =
        w.t.json_rows(&format!(
            "SELECT action, actor_kind, actor_user_id, credential_id, request_id, \
             (details->>'merge_id')::bigint AS merge_id, details->>'target_sha' AS target_sha, \
             details->>'merged_sha' AS merged_sha FROM forge.audit_log \
             WHERE action IN ('merge_request.merged', 'repository.pushed') \
             AND request_id IN ({}) ORDER BY id",
            reqs.join(",")
        ))
        .await;
    assert_eq!(audit, expected_audit);
    let events =
        w.t.json_rows(
            "SELECT event_type, aggregate_id, actor_user_id, request_id, dedup_key \
             FROM forge.events WHERE dedup_key LIKE 'merge_request_merged:%' \
             OR dedup_key LIKE 'merge_pushed:%' ORDER BY id",
        )
        .await;
    assert_eq!(events, expected_events);
    // Plan 18 S10: under the lock, no process per member for the source
    // tips or the approval facts — the target's tip, the one
    // `for-each-ref`, the compare-and-swap, the landed paths, and per
    // member only the record's commit list and attribution.
    let landed = logs.named("forge.merge_queue.batch_landed");
    assert_eq!(landed.len(), 1, "{landed:?}");
    let spawns = landed[0]["git_spawns"].as_u64().expect("git_spawns");
    assert!(
        spawns <= 4 + 3 * 3,
        "git_spawns under the lock: {}",
        landed[0]
    );
}

#[tokio::test]
async fn a_source_moved_after_build_is_refused_under_the_lock() {
    let mut w = world().await;
    let first = w.mr("ma", &[("ma.txt", "x\n")]).await;
    let second = w.mr("mb", &[("mb.txt", "x\n")]).await;
    w.enqueue(&first).await.expect("enqueue");
    w.enqueue(&second).await.expect("enqueue");
    let worker = w.worker("w1");
    let built = worker.tick().await.expect("tick").expect("due");
    assert_eq!(built.outcome, "built");
    let queue = w.queue_id().await;
    // After the build (and its approval facts) the source moves in git
    // only: the merge request still records the tested head.
    w.move_source("ma");
    w.finish_pipeline(queue, &BTreeSet::new()).await;
    let landing = worker.tick().await.expect("tick").expect("due");
    assert_eq!(landing.outcome, "abandoned", "{landing:?}");
    assert_eq!(w.tip("main"), w.base, "nothing landed");
    assert_eq!(w.attempts().await, 0, "the refused decisions wrote nothing");
    let removed = w.entry_of(&first).await;
    assert_eq!(removed.state, EntryState::Removed);
    assert_eq!(removed.reason.as_deref(), Some("source_moved"));
    assert_eq!(
        removed.error_code.as_deref(),
        Some(ErrorCode::HeadChanged.id())
    );
    w.drive(&worker, &BTreeSet::new()).await;
    assert_eq!(
        w.state_of(&first).await,
        forge_core::MergeRequestState::Open
    );
    assert_eq!(
        w.state_of(&second).await,
        forge_core::MergeRequestState::Merged
    );
    assert!(!w.main_has("ma.txt"));
    assert!(w.main_has("mb.txt"));
}

#[tokio::test]
async fn one_refusal_in_a_batched_decision_rebuilds_without_it() {
    let mut w = world().await;
    let mut mrs = Vec::new();
    for name in ["ra", "rb", "rc"] {
        mrs.push(w.mr(name, &[(&format!("{name}.txt"), "x\n")]).await);
    }
    let other = w.second_session().await;
    w.enqueue(&mrs[0]).await.expect("enqueue");
    w.enqueue_with(&mrs[1], other).await;
    w.enqueue(&mrs[2]).await.expect("enqueue");
    let worker = w.worker("w1");
    let built = worker.tick().await.expect("tick").expect("due");
    assert_eq!(built.outcome, "built");
    let queue = w.queue_id().await;
    let refused_batch = built.batch_id.expect("batch");
    // The middle member's credential is revoked while the batch tests.
    let _revoked = forge_db::sessions::revoke(
        &w.t.db,
        &system("req-revoke"),
        other,
        forge_core::SessionRevokeReason::Logout,
    )
    .await
    .expect("revoke");
    w.finish_pipeline(queue, &BTreeSet::new()).await;
    let landing = worker.tick().await.expect("tick").expect("due");
    assert_eq!(landing.outcome, "abandoned", "{landing:?}");
    assert_eq!(landing.batch_id, Some(refused_batch));
    // One transaction: the first member's decision is not left behind.
    assert_eq!(w.attempts().await, 0, "nothing of the refused batch stays");
    assert_eq!(w.tip("main"), w.base);
    let removed = w.entry_of(&mrs[1]).await;
    assert_eq!(removed.state, EntryState::Removed);
    assert_eq!(removed.reason.as_deref(), Some("credential_revoked"));
    for mr in [&mrs[0], &mrs[2]] {
        assert_eq!(
            w.entry_of(mr).await.state,
            EntryState::Queued,
            "back in order"
        );
    }
    let sizes = w.drive(&worker, &BTreeSet::new()).await;
    assert_eq!(sizes, [2], "rebuilt without the refused member");
    for (mr, file, merged) in [
        (&mrs[0], "ra.txt", true),
        (&mrs[1], "rb.txt", false),
        (&mrs[2], "rc.txt", true),
    ] {
        assert_eq!(w.main_has(file), merged, "{file}");
        let want = if merged {
            forge_core::MergeRequestState::Merged
        } else {
            forge_core::MergeRequestState::Open
        };
        assert_eq!(w.state_of(mr).await, want);
    }
}
