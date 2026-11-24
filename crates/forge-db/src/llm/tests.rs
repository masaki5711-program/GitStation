//! The LLM records against a migrated database (plan 19 L5-2).

use forge_core::RequestId;

use super::*;
use crate::change::{Actor, ChangeContext};
use crate::projects::{self, CreatedProject, NewProject};
use crate::test_support::*;
use crate::users::{self, NewUser};

fn system(request: &str) -> ChangeContext {
    ChangeContext::new(Actor::System, RequestId::parse(request).expect("id"))
}

struct World {
    t: FreshDb,
    alice: UserId,
    bob: UserId,
    web: CreatedProject,
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
    let bob = users::create(
        &t.db,
        &system("req-user-bob"),
        &NewUser::human("bob", "bob", "bob@example.test"),
    )
    .await
    .expect("bob");
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
        bob: bob.user_id,
        web,
    }
}

fn request(w: &World, class: InputClass) -> NewRequest {
    NewRequest {
        purpose: Purpose::MrReview,
        priority: Priority::Review,
        subject_kind: "merge_request".to_owned(),
        subject_id: Some(7),
        project: Some(w.web.project_id),
        requested_by: Some(w.alice),
        model: "qwen3:27b".to_owned(),
        model_digest: Some("a".repeat(64)),
        prompt_id: "mr_review".to_owned(),
        prompt_version: 1,
        input_sha256: "b".repeat(64),
        input_class: class,
        request_id: RequestId::parse("req-llm-1").expect("id"),
    }
}

fn advice(w: &World, request: Option<i64>, class: InputClass) -> NewAdvice {
    NewAdvice {
        llm_request_id: request,
        purpose: Purpose::MrReview,
        subject_kind: "merge_request".to_owned(),
        subject_id: Some(7),
        project: Some(w.web.project_id),
        head_sha: Some("c".repeat(40)),
        summary: "Adds a retry.".to_owned(),
        findings: serde_json::json!([
            {"severity": "high", "path": "src/a.rs", "line": 3, "quote": "x.unwrap()", "message": "m", "confidence": 0.9},
            {"severity": "low", "path": "src/b.rs", "line": 1, "quote": "y", "message": "n", "confidence": 0.7}
        ]),
        deep_review: None,
        dropped_findings: 0,
        degraded: false,
        input_class: class,
        model: "qwen3:27b".to_owned(),
        prompt_version: 1,
    }
}

const PERSON: Reader = Reader {
    is_agent: false,
    can_read_subject: true,
};
const AGENT: Reader = Reader {
    is_agent: true,
    can_read_subject: true,
};

/// A request queued `age` ago, in `state` (as written then).
async fn old_request(w: &World, age: &str, state: &str) -> i64 {
    let ended = if matches!(state, "succeeded" | "failed" | "abandoned") {
        format!("now() - interval '{age}'")
    } else {
        "NULL".to_owned()
    };
    let code = if state == "succeeded" || state == "running" {
        "NULL"
    } else {
        "'FG-SYS-0001'"
    };
    sqlx::query_scalar(&format!(
        "INSERT INTO forge.llm_requests (purpose, priority, subject_kind, subject_id, project_id, \
           model, prompt_id, prompt_version, input_sha256, input_class, request_id, state, \
           error_code, queued_at, started_at, finished_at) \
         VALUES ('mr_review', 'review', 'merge_request', 7, $1, 'qwen3:27b', 'mr_review', 1, \
           repeat('b', 64), 'internal', 'req-llm-old', '{state}', {code}, \
           now() - interval '{age}', now() - interval '{age}', {ended}) RETURNING id"
    ))
    .bind(w.web.project_id.get())
    .fetch_one(&mut w.t.superuser().await)
    .await
    .expect("old request")
}

fn shown(view: Option<AdviceView>) -> StoredAdvice {
    match view {
        Some(AdviceView::Shown(a)) => *a,
        other => panic!("not shown: {other:?}"),
    }
}

#[test]
fn a_stale_live_row_is_older_than_any_live_request() {
    // The longest deadline plus the longest checked call (two attempts of
    // llm.timeout_ms ≤ 600 s, and 5 s).
    let longest = Priority::Batch.deadline().as_secs() + 2 * 600 + 5;
    assert!(u64::try_from(STALE_LIVE_SECS).unwrap_or(0) > longest);
}

#[tokio::test]
async fn a_request_moves_forward_and_records_its_code() {
    let w = world().await;
    let id = insert_request(&w.t.db, &request(&w, InputClass::Internal))
        .await
        .expect("queued");
    assert!(interactive_active(&w.t.db).await.is_ok_and(|a| !a));
    assert!(mark_running(&w.t.db, id).await.expect("running"));
    assert!(!mark_running(&w.t.db, id).await.expect("only once"));
    let usage = Usage {
        attempts: 2,
        input_tokens: Some(1200),
        output_tokens: Some(80),
    };
    assert!(finish(
        &w.t.db,
        id,
        Ending::Failed(ErrorCode::LlmOutputInvalid),
        usage
    )
    .await
    .expect("failed"));
    assert!(!finish(&w.t.db, id, Ending::Succeeded, usage)
        .await
        .expect("ended once"));
    let rows =
        w.t.json_rows(&format!(
            "SELECT state, error_code, attempts FROM forge.llm_requests WHERE id = {id}"
        ))
        .await;
    assert_eq!(rows[0]["state"], "failed");
    assert_eq!(rows[0]["error_code"], "FG-LLM-0002");
    assert_eq!(rows[0]["attempts"], 2);

    // A dead worker's live rows are abandoned once they are stale; a
    // running worker's (younger) rows stay.
    let mut interactive = request(&w, InputClass::Internal);
    interactive.priority = Priority::Interactive;
    let live = old_request(&w, "8 hours", "running").await;
    let young = insert_request(&w.t.db, &interactive).await.expect("queued");
    assert!(interactive_active(&w.t.db).await.expect("active"));
    // The identity of a request (its queue time too) never changes.
    let moved = sqlx::query(
        "UPDATE forge.llm_requests SET queued_at = now() - interval '1 day' WHERE id = $1",
    )
    .bind(young)
    .execute(&mut w.t.superuser().await)
    .await;
    assert!(moved.is_err(), "the guard keeps queued_at");
    assert_eq!(
        abandon_stale(&w.t.db, ErrorCode::Internal, STALE_LIVE_SECS)
            .await
            .expect("abandoned"),
        1
    );
    assert!(interactive_active(&w.t.db).await.expect("the young one"));
    finish(
        &w.t.db,
        young,
        Ending::Abandoned(ErrorCode::LlmInputRefused),
        Usage::default(),
    )
    .await
    .expect("ended");
    assert!(!interactive_active(&w.t.db).await.expect("idle"));
    let rows =
        w.t.json_rows(&format!(
            "SELECT state FROM forge.llm_requests WHERE id = {live}"
        ))
        .await;
    assert_eq!(rows[0]["state"], "abandoned");
}

#[tokio::test]
async fn advice_keeps_input_class() {
    let w = world().await;
    let req = insert_request(&w.t.db, &request(&w, InputClass::Confidential))
        .await
        .expect("queued");
    // Lower than its request's: refused by the table.
    let err = insert_advice(&w.t.db, &advice(&w, Some(req), InputClass::Internal))
        .await
        .expect_err("lowered");
    assert!(chain(&err).contains("input_class"), "{}", chain(&err));
    let id = insert_advice(&w.t.db, &advice(&w, Some(req), InputClass::Confidential))
        .await
        .expect("kept");
    let stored = shown(super::advice(&w.t.db, id, PERSON).await.expect("read"));
    assert_eq!(stored.input_class, InputClass::Confidential);
    // An agent learns only that it exists; a reader of another subject
    // learns nothing; an agent cannot vote.
    assert_eq!(
        super::advice(&w.t.db, id, AGENT).await.expect("read"),
        Some(AdviceView::Withheld {
            id,
            input_class: InputClass::Confidential
        })
    );
    let outsider = Reader {
        is_agent: false,
        can_read_subject: false,
    };
    assert_eq!(super::advice(&w.t.db, id, outsider).await.expect("read"), None);
    assert_eq!(
        record_feedback(&w.t.db, id, None, w.alice, AGENT, Verdict::Useful)
            .await
            .expect("refused"),
        FeedbackOutcome::NotAllowed
    );
    // Never lowered afterwards, even by hand.
    let mut su = w.t.superuser().await;
    let lowered = sqlx::query("UPDATE forge.llm_advice SET input_class = 'internal' WHERE id = $1")
        .bind(id)
        .execute(&mut su)
        .await;
    assert!(lowered.is_err(), "the guard refuses a lower class");
    // A newer advice of the same subject supersedes it.
    let newer = insert_advice(&w.t.db, &advice(&w, None, InputClass::Confidential))
        .await
        .expect("newer");
    let stored = shown(super::advice(&w.t.db, id, PERSON).await.expect("read"));
    assert_eq!(stored.superseded_by, Some(newer));
    // Superseded once, by a newer advice of its subject only.
    let mut su = w.t.superuser().await;
    let again = sqlx::query("UPDATE forge.llm_advice SET superseded_by = $1 WHERE id = $2")
        .bind(id)
        .bind(newer)
        .execute(&mut su)
        .await;
    assert!(again.is_err(), "an older advice never supersedes a newer one");
}

#[tokio::test]
async fn input_text_never_stored() {
    let w = world().await;
    // No column of the records can hold the input: only its hash and class.
    let columns =
        w.t.json_rows(
            "SELECT table_name, column_name FROM information_schema.columns \
             WHERE table_schema = 'forge' AND table_name IN ('llm_requests', 'llm_advice') \
               AND column_name LIKE '%input%' ORDER BY 1, 2",
        )
        .await;
    let names: Vec<String> = columns
        .iter()
        .map(|c| {
            format!(
                "{}.{}",
                c["table_name"].as_str().unwrap_or(""),
                c["column_name"].as_str().unwrap_or("")
            )
        })
        .collect();
    assert_eq!(
        names,
        [
            "llm_advice.input_class",
            "llm_requests.input_class",
            "llm_requests.input_sha256",
            "llm_requests.input_tokens",
        ]
    );
    // The hash column takes a hash only.
    let mut bad = request(&w, InputClass::Internal);
    bad.input_sha256 = "diff --git a/src/a.rs b/src/a.rs".to_owned();
    insert_request(&w.t.db, &bad)
        .await
        .expect_err("text is not a hash");
}

#[tokio::test]
async fn feedback_one_per_user_and_finding() {
    let w = world().await;
    let id = insert_advice(&w.t.db, &advice(&w, None, InputClass::Internal))
        .await
        .expect("advice");
    let db = &w.t.db;
    assert_eq!(
        record_feedback(db, id, Some(0), w.alice, PERSON, Verdict::Useful)
            .await
            .expect("first"),
        FeedbackOutcome::Recorded
    );
    assert_eq!(
        record_feedback(db, id, Some(0), w.alice, PERSON, Verdict::Useful)
            .await
            .expect("again"),
        FeedbackOutcome::Unchanged
    );
    assert_eq!(
        record_feedback(db, id, Some(0), w.alice, PERSON, Verdict::Wrong)
            .await
            .expect("changed"),
        FeedbackOutcome::Changed
    );
    // Another finding, another person, the advice as a whole: their own rows.
    for (finding, user) in [(Some(1), w.alice), (Some(0), w.bob), (None, w.alice)] {
        assert_eq!(
            record_feedback(db, id, finding, user, PERSON, Verdict::Useful)
                .await
                .expect("own row"),
            FeedbackOutcome::Recorded
        );
    }
    // A finding the advice does not have, an advice that does not exist.
    assert_eq!(
        record_feedback(db, id, Some(2), w.alice, PERSON, Verdict::Useful)
            .await
            .expect("none"),
        FeedbackOutcome::NoAdvice
    );
    assert_eq!(
        record_feedback(db, id + 1000, None, w.alice, PERSON, Verdict::Useful)
            .await
            .expect("none"),
        FeedbackOutcome::NoAdvice
    );
    let rows =
        w.t.json_rows(&format!(
            "SELECT count(*) AS n FROM forge.llm_advice_feedback WHERE advice_id = {id}"
        ))
        .await;
    assert_eq!(rows[0]["n"], 4);
    // The database refuses a second row for the same triple.
    let mut su = w.t.superuser().await;
    let dup = sqlx::query(
        "INSERT INTO forge.llm_advice_feedback (advice_id, purpose, input_class, finding_index, \
           user_id, verdict) VALUES ($1, 'mr_review', 'internal', 0, $2, 'useful')",
    )
    .bind(id)
    .bind(w.alice.get())
    .execute(&mut su)
    .await;
    assert!(dup.is_err());
}

#[tokio::test]
async fn retention_sweep_keeps_feedback() {
    let w = world().await;
    // A request that ended 91 days ago (written as it was then: the guard
    // keeps its identity and its end).
    let req = old_request(&w, "91 days", "succeeded").await;
    // An advice of 181 days ago (written as it was then: the guard lets
    // only superseded_by change afterwards).
    let old: i64 = sqlx::query_scalar(
        "INSERT INTO forge.llm_advice (llm_request_id, purpose, subject_kind, subject_id, \
           project_id, summary, findings, input_class, model, prompt_version, created_at) \
         VALUES ($1, 'mr_review', 'merge_request', 7, $2, 's', \
           '[{\"severity\": \"low\"}]', 'internal', 'qwen3:27b', 1, \
           now() - interval '181 days') RETURNING id",
    )
    .bind(req)
    .bind(w.web.project_id.get())
    .fetch_one(&mut w.t.superuser().await)
    .await
    .expect("old advice");
    record_feedback(&w.t.db, old, Some(0), w.alice, PERSON, Verdict::Wrong)
        .await
        .expect("feedback");
    // A fresh request and advice stay.
    let fresh_req = insert_request(&w.t.db, &request(&w, InputClass::Internal))
        .await
        .expect("fresh");
    let mut a = advice(&w, None, InputClass::Internal);
    a.subject_id = Some(8);
    let fresh = insert_advice(&w.t.db, &a).await.expect("fresh advice");
    let swept = sweep_retention(&w.t.db).await.expect("swept");
    assert_eq!(
        swept,
        Swept {
            advice: 1,
            requests: 1
        }
    );
    assert!(super::advice(&w.t.db, old, PERSON).await.expect("read").is_none());
    assert!(super::advice(&w.t.db, fresh, PERSON).await.expect("read").is_some());
    let rows = w
        .t
        .json_rows("SELECT advice_id, purpose, verdict FROM forge.llm_advice_feedback ORDER BY id")
        .await;
    assert_eq!(rows.len(), 1, "feedback is never deleted");
    assert!(rows[0]["advice_id"].is_null());
    assert_eq!(rows[0]["purpose"], "mr_review");
    assert_eq!(rows[0]["verdict"], "wrong");
    let live =
        w.t.json_rows(&format!(
            "SELECT count(*) AS n FROM forge.llm_requests WHERE id = {fresh_req}"
        ))
        .await;
    assert_eq!(live[0]["n"], 1);
}

#[tokio::test]
async fn review_mode_is_off_without_a_row() {
    let w = world().await;
    assert_eq!(
        review_mode(&w.t.db, w.web.project_id).await.expect("read"),
        ReviewMode::Off
    );
    w.t.exec_superuser(&format!(
        "INSERT INTO forge.project_llm (project_id, review, changed_by) VALUES ({}, 'advisory', {})",
        w.web.project_id.get(),
        w.alice.get()
    ))
    .await;
    assert_eq!(
        review_mode(&w.t.db, w.web.project_id).await.expect("read"),
        ReviewMode::Advisory
    );
}
