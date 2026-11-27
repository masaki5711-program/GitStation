//! A person's LLM changes against a migrated database (plan 19 §8.2):
//! `llm.review` and the gateway's pause are audited once per change and
//! read back; a rerun is refused for a closed or off merge request, written
//! only for a completed head, and limited to 3 an hour per merge request.

use forge_core::checks::{CheckConclusion, CheckStatus, AI_REVIEW_CHECK};
use forge_core::{CheckName, ObjectId, RequestId};

use super::*;
use crate::change::{Actor, ChangeContext};
use crate::checks::NewCheckRun;
use crate::projects::{self, NewProject};
use crate::test_support::*;
use crate::users::{self, NewUser};

fn system(request: &str) -> ChangeContext {
    ChangeContext::new(Actor::System, RequestId::parse(request).expect("id"))
}

fn as_user(user: UserId, request: &str) -> ChangeContext {
    ChangeContext::new(Actor::User(user), RequestId::parse(request).expect("id"))
}

struct World {
    t: FreshDb,
    alice: UserId,
    project: ProjectId,
}

/// A project with merge request 1 (open, head `a…`) and 2 (closed).
async fn world() -> World {
    let t = FreshDb::migrated().await;
    let alice = users::create(
        &t.db,
        &system("req-user-alice"),
        &NewUser::human("alice", "alice", "alice@example.test"),
    )
    .await
    .expect("alice");
    let created = projects::create(
        &t.db,
        &system("req-p-web"),
        &NewProject::new("web", alice.namespace_id, "Web", alice.user_id),
    )
    .await
    .expect("project");
    let p = created.project_id.get();
    let a = alice.user_id.get();
    t.commit_audited(
        ("project", p),
        &[
            format!(
                "INSERT INTO forge.merge_request_counters (project_id, last_iid) VALUES ({p}, 2)"
            ),
            format!(
                "INSERT INTO forge.merge_requests (project_id, iid, title, author_id, \
                 source_branch, target_branch, head_sha) VALUES ({p}, 1, 'synthetic', {a}, \
                 'refs/heads/f1', 'refs/heads/main', '{}')",
                "a".repeat(40)
            ),
            format!(
                "INSERT INTO forge.merge_requests (project_id, iid, title, author_id, \
                 source_branch, target_branch, head_sha, state, closed_at, closed_by) VALUES \
                 ({p}, 2, 'synthetic', {a}, 'refs/heads/f2', 'refs/heads/main', '{}', 'closed', \
                 now(), {a})",
                "b".repeat(40)
            ),
        ],
    )
    .await;
    World {
        t,
        alice: alice.user_id,
        project: created.project_id,
    }
}

impl World {
    async fn audit(&self, like: &str) -> Vec<(String, serde_json::Value)> {
        sqlx::query_as(
            "SELECT action, details FROM forge.audit_log WHERE action LIKE $1 ORDER BY id",
        )
        .bind(like)
        .fetch_all(&mut self.t.superuser().await)
        .await
        .expect("audit")
    }

    async fn mode(&self, mode: ReviewMode, request: &str) -> (ChangeOutcome, ReviewSetting) {
        let (outcome, setting, ()) = set_review_mode_decided(
            &self.t.db,
            &as_user(self.alice, request),
            self.project,
            self.alice,
            mode,
            |_| Ok(()),
        )
        .await
        .expect("set");
        (outcome, setting)
    }

    async fn rerun(&self, iid: i64, request: &str) -> Result<Rerun, ForgeError> {
        request_rerun(
            &self.t.db,
            &as_user(self.alice, request),
            self.project,
            iid,
            self.alice,
            |facts| {
                // The decision sees the locked merge request.
                assert!(iid != 1 || facts.merge_request.is_some());
                Ok(())
            },
        )
        .await
        .map(|(rerun, ())| rerun)
    }

    async fn completed(&self) {
        record_forge_check(
            &self.t.db,
            self.project,
            &ObjectId::parse(&"a".repeat(40)).expect("sha"),
            &NewCheckRun {
                name: CheckName::parse(AI_REVIEW_CHECK).expect("name"),
                status: CheckStatus::Completed,
                conclusion: Some(CheckConclusion::Neutral),
                summary: "synthetic".to_owned(),
                details_url: None,
                annotations: Vec::new(),
            },
        )
        .await;
    }

    async fn reruns(&self) -> i64 {
        self.t
            .json_rows("SELECT count(*) AS n FROM forge.llm_review_reruns")
            .await[0]["n"]
            .as_i64()
            .expect("count")
    }
}

#[tokio::test]
async fn the_review_mode_is_off_until_changed_and_each_change_is_audited() {
    let w = world().await;
    let s = review_setting(&w.t.db, w.project).await.expect("read");
    assert_eq!((s.mode, s.changed_by), (ReviewMode::Off, None));
    // Off on a project without a row: nothing to write.
    assert_eq!(
        w.mode(ReviewMode::Off, "req-mode-0").await.0,
        ChangeOutcome::Unchanged
    );
    let (outcome, s) = w.mode(ReviewMode::Advisory, "req-mode-1").await;
    assert_eq!(outcome, ChangeOutcome::Changed);
    assert_eq!(
        (s.mode, s.changed_by),
        (ReviewMode::Advisory, Some(w.alice))
    );
    assert!(s.changed_at.is_some());
    assert_eq!(
        w.mode(ReviewMode::Advisory, "req-mode-2").await.0,
        ChangeOutcome::Unchanged
    );
    let _set = w.mode(ReviewMode::Required, "req-mode-3").await;
    assert_eq!(
        super::super::review_mode(&w.t.db, w.project)
            .await
            .expect("mode"),
        ReviewMode::Required
    );
    let audit = w.audit("llm.review%").await;
    assert_eq!(audit.len(), 2);
    assert_eq!(audit[0].0, "llm.review_changed");
    assert_eq!(audit[0].1["from"], "off");
    assert_eq!(audit[1].1["to"], "required");
    // A refused decision writes nothing.
    let refused = set_review_mode_decided(
        &w.t.db,
        &as_user(w.alice, "req-mode-4"),
        w.project,
        w.alice,
        ReviewMode::Off,
        |_| Err::<(), _>(ForgeError::new(ErrorCode::Forbidden)),
    )
    .await
    .expect_err("refused");
    assert_eq!(refused.code(), ErrorCode::Forbidden);
    assert_eq!(w.audit("llm.review%").await.len(), 2);
}

#[tokio::test]
async fn the_gateway_pause_is_stored_audited_and_read_back() {
    let w = world().await;
    assert_eq!(
        gateway_pause(&w.t.db).await.expect("read"),
        GatewayPause::default()
    );
    let set = |paused: bool, request: &'static str| {
        let db = w.t.db.clone();
        let ctx = as_user(w.alice, request);
        let alice = w.alice;
        async move {
            set_paused_decided(&db, &ctx, alice, paused, |_| Ok(()))
                .await
                .expect("set")
        }
    };
    let (outcome, state, ()) = set(true, "req-pause-1").await;
    assert_eq!(outcome, ChangeOutcome::Changed);
    assert!(state.paused);
    assert_eq!(state.changed_by, Some(w.alice));
    assert_eq!(gateway_pause(&w.t.db).await.expect("read"), state);
    assert_eq!(set(true, "req-pause-2").await.0, ChangeOutcome::Unchanged);
    let (_, resumed, ()) = set(false, "req-pause-3").await;
    assert!(!resumed.paused);
    let audit = w.audit("llm.%u_ed").await;
    let actions: Vec<&str> = audit.iter().map(|(a, _)| a.as_str()).collect();
    assert_eq!(actions, vec!["llm.paused", "llm.resumed"]);
}

#[tokio::test]
async fn a_rerun_of_a_closed_or_off_merge_request_is_refused() {
    let w = world().await;
    w.completed().await;
    let reason = |e: &ForgeError| {
        e.context()
            .iter()
            .find(|(k, _)| *k == "reason")
            .map(|(_, v)| v.to_string())
    };
    // Off (no row): nothing to repeat.
    let e = w.rerun(1, "req-rerun-1").await.expect_err("off");
    assert_eq!(e.code(), ErrorCode::AiReviewRerunRefused);
    assert_eq!(reason(&e).as_deref(), Some("review_off"));
    let _set = w.mode(ReviewMode::Advisory, "req-mode").await;
    let e = w.rerun(2, "req-rerun-2").await.expect_err("closed");
    assert_eq!(e.code(), ErrorCode::AiReviewRerunRefused);
    assert_eq!(reason(&e).as_deref(), Some("not_open"));
    let e = w.rerun(9, "req-rerun-3").await.expect_err("missing");
    assert_eq!(e.code(), ErrorCode::NotFound);
    assert_eq!(w.reruns().await, 0);
    assert!(w.audit("llm.review_rerun%").await.is_empty());
}

#[tokio::test]
async fn the_fourth_rerun_in_an_hour_is_429() {
    let w = world().await;
    let _set = w.mode(ReviewMode::Advisory, "req-mode").await;
    let head = "a".repeat(40);
    let mr =
        w.t.json_rows("SELECT id FROM forge.merge_requests WHERE iid = 1")
            .await[0]["id"]
            .as_i64()
            .expect("id");
    let mr = MergeRequestId::new(mr);
    // No run on the head yet: the worker reviews it anyway, nothing counted.
    assert_eq!(
        w.rerun(1, "req-rerun-0").await.expect("pending"),
        Rerun::Pending {
            head_sha: head.clone()
        }
    );
    assert_eq!(
        rerun_state(&w.t.db, w.project, mr, &head)
            .await
            .expect("hint"),
        RerunState::NotDone
    );
    for (n, remaining) in [(1, 2), (2, 1), (3, 0)] {
        w.completed().await;
        assert_eq!(
            rerun_state(&w.t.db, w.project, mr, &head)
                .await
                .expect("hint"),
            RerunState::Possible
        );
        assert_eq!(
            w.rerun(1, &format!("req-rerun-{n}")).await.expect("queued"),
            Rerun::Queued {
                head_sha: head.clone(),
                remaining
            }
        );
        // Until the worker writes the run again, a second press waits.
        assert_eq!(
            w.rerun(1, &format!("req-rerun-again-{n}"))
                .await
                .expect("pending"),
            Rerun::Pending {
                head_sha: head.clone()
            }
        );
        assert_eq!(
            rerun_state(&w.t.db, w.project, mr, &head)
                .await
                .expect("hint"),
            RerunState::Asked
        );
    }
    w.completed().await;
    let e = w.rerun(1, "req-rerun-4").await.expect_err("limited");
    assert_eq!(e.code(), ErrorCode::RateLimited);
    let ctx = e.context();
    let get = |key: &str| {
        ctx.iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v.to_string())
    };
    assert_eq!(get("route").as_deref(), Some("ai_review_rerun"));
    let retry: i64 = get("retry_after_s")
        .and_then(|v| v.parse().ok())
        .expect("retry");
    assert!((1..=3600).contains(&retry), "{retry}");
    assert_eq!(w.reruns().await, 3);
    let audit = w.audit("llm.review_rerun%").await;
    assert_eq!(audit.len(), 3);
    assert_eq!(audit[0].1["iid"], 1);
    assert_eq!(audit[0].1["head_sha"], head);
}

#[tokio::test]
async fn a_rerun_refused_on_the_locked_facts_writes_nothing() {
    let w = world().await;
    let _set = w.mode(ReviewMode::Advisory, "req-mode").await;
    w.completed().await;
    let e = request_rerun(
        &w.t.db,
        &as_user(w.alice, "req-rerun-demoted"),
        w.project,
        1,
        w.alice,
        |_| Err::<(), _>(ForgeError::new(ErrorCode::Forbidden)),
    )
    .await
    .expect_err("refused");
    assert_eq!(e.code(), ErrorCode::Forbidden);
    assert_eq!(w.reruns().await, 0);
    assert!(w.audit("llm.review_rerun%").await.is_empty());
}
