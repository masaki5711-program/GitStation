//! The LLM records against a migrated database (plan 19 L5-2).

use forge_core::llm::SubjectKind;
use forge_core::RequestId;
use forge_errors::ForgeError;

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
        subject_kind: SubjectKind::MergeRequest,
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
        subject_kind: SubjectKind::MergeRequest,
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

type Allowed = (SubjectKind, Option<i64>);

const PERSON: ReaderKind = ReaderKind::Person;
const AGENT: ReaderKind = ReaderKind::Agent;
const MR7: Allowed = (SubjectKind::MergeRequest, Some(7));
const MR8: Allowed = (SubjectKind::MergeRequest, Some(8));

/// A route's decision as L5-4 will make it: `authorize()` on the subject
/// the store loaded; the reader may see `allowed` only, anything else is
/// "not found" (FG-AUTH-0003 stands in for the route's denial).
fn decide(
    allowed: Allowed,
    reader: ReaderKind,
) -> impl FnOnce(&AdviceSubject) -> Result<ReaderKind, ForgeError> {
    move |s| {
        if (s.kind, s.id) == allowed {
            Ok(reader)
        } else {
            Err(ForgeError::new(ErrorCode::Forbidden).with_context("reason", "subject_not_visible"))
        }
    }
}

async fn read(
    db: &Db,
    id: i64,
    allowed: Allowed,
    reader: ReaderKind,
) -> Result<Option<AdviceView>, ForgeError> {
    advice_decided(db, id, decide(allowed, reader)).await
}

async fn record_feedback(
    db: &Db,
    id: i64,
    allowed: Allowed,
    finding: Option<u8>,
    user: UserId,
    reader: ReaderKind,
    verdict: Verdict,
) -> Result<FeedbackOutcome, ForgeError> {
    record_feedback_decided(db, id, finding, user, verdict, decide(allowed, reader)).await
}

/// A decision is always made on the advice's own subject: a reader allowed
/// on merge request 5 (or on deployment 7) who asks for advice of
/// (confidential) merge request 7 is refused by the decision on MR 7 — the
/// store never pairs one subject's decision with another's advice — for
/// the read and for the vote alike.
#[tokio::test]
async fn another_subjects_advice_is_not_found() {
    let w = world().await;
    let id = insert_advice(&w.t.db, &advice(&w, None, InputClass::Confidential))
        .await
        .expect("advice of MR 7");
    let mut seen = Vec::new();
    let got = advice_decided(&w.t.db, id, |s: &AdviceSubject| {
        seen.push(*s);
        Ok(PERSON)
    })
    .await
    .expect("read");
    assert!(got.is_some());
    assert_eq!(seen.len(), 1);
    assert_eq!(
        (seen[0].kind, seen[0].id),
        MR7,
        "decided on its own subject"
    );
    assert_eq!(seen[0].project, Some(w.web.project_id));
    assert_eq!(seen[0].input_class, InputClass::Confidential);
    for allowed in [
        (SubjectKind::MergeRequest, Some(5)),
        (SubjectKind::Deployment, Some(7)),
    ] {
        let err = read(&w.t.db, id, allowed, PERSON)
            .await
            .expect_err("refused");
        assert_eq!(err.code(), ErrorCode::Forbidden);
        let err = record_feedback(
            &w.t.db,
            id,
            allowed,
            Some(0),
            w.alice,
            PERSON,
            Verdict::Wrong,
        )
        .await
        .expect_err("refused");
        assert_eq!(err.code(), ErrorCode::Forbidden);
    }
    let rows =
        w.t.json_rows("SELECT count(*) AS n FROM forge.llm_advice_feedback")
            .await;
    assert_eq!(rows[0]["n"], 0, "no vote landed");
    assert!(read(&w.t.db, id, MR7, PERSON)
        .await
        .expect("read")
        .is_some());
    // The table takes the three kinds only.
    let mut bad = advice(&w, None, InputClass::Internal);
    bad.subject_id = Some(9);
    insert_advice(&w.t.db, &bad).await.expect("a known kind");
    let refused = sqlx::query(
        "INSERT INTO forge.llm_advice (purpose, subject_kind, subject_id, summary, input_class, \
           model, prompt_version) VALUES ('mr_review', 'wiki_page', 1, 's', 'internal', 'm', 1)",
    )
    .execute(&mut w.t.superuser().await)
    .await;
    assert!(refused.is_err(), "an unknown subject kind is refused");
}

/// Two votes of one person at once: one row, and exactly one of them is
/// reported as recorded (the xmax test of the single statement).
#[tokio::test]
async fn concurrent_votes_record_one_row() {
    let w = world().await;
    let id = insert_advice(&w.t.db, &advice(&w, None, InputClass::Internal))
        .await
        .expect("advice");
    let (a, b) = tokio::join!(
        record_feedback(&w.t.db, id, MR7, Some(0), w.alice, PERSON, Verdict::Useful),
        record_feedback(&w.t.db, id, MR7, Some(0), w.alice, PERSON, Verdict::Useful),
    );
    let outcomes = [a.expect("first"), b.expect("second")];
    assert_eq!(
        outcomes
            .iter()
            .filter(|o| **o == FeedbackOutcome::Recorded)
            .count(),
        1,
        "{outcomes:?}"
    );
    assert!(
        outcomes.contains(&FeedbackOutcome::Unchanged),
        "{outcomes:?}"
    );
    let rows =
        w.t.json_rows(&format!(
            "SELECT count(*) AS n FROM forge.llm_advice_feedback WHERE advice_id = {id}"
        ))
        .await;
    assert_eq!(rows[0]["n"], 1);
}

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
    let stored = shown(read(&w.t.db, id, MR7, PERSON).await.expect("read"));
    assert_eq!(stored.input_class, InputClass::Confidential);
    // An agent learns only that it exists; a reader of another subject
    // learns nothing; an agent cannot vote.
    assert_eq!(
        read(&w.t.db, id, MR7, AGENT).await.expect("read"),
        Some(AdviceView::Withheld {
            id,
            input_class: InputClass::Confidential
        })
    );
    // A refused decision ends the read: the words never leave the store.
    let refused = advice_decided(&w.t.db, id, |_| {
        Err::<ReaderKind, _>(ForgeError::new(ErrorCode::Forbidden))
    })
    .await;
    assert!(refused.is_err());
    // No such advice: none, and nothing is decided.
    assert_eq!(
        advice_decided(&w.t.db, id + 1000, |_| -> Result<ReaderKind, ForgeError> {
            panic!("no decision without an advice")
        })
        .await
        .expect("read"),
        None
    );
    assert_eq!(
        record_feedback(&w.t.db, id, MR7, None, w.alice, AGENT, Verdict::Useful)
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
    let stored = shown(read(&w.t.db, id, MR7, PERSON).await.expect("read"));
    assert_eq!(stored.superseded_by, Some(newer));
    // Superseded once, by a newer advice of its subject only.
    let mut su = w.t.superuser().await;
    let again = sqlx::query("UPDATE forge.llm_advice SET superseded_by = $1 WHERE id = $2")
        .bind(id)
        .bind(newer)
        .execute(&mut su)
        .await;
    assert!(
        again.is_err(),
        "an older advice never supersedes a newer one"
    );
    // A reference becomes NULL only when the referenced row is deleted.
    let cleared = sqlx::query("UPDATE forge.llm_advice SET superseded_by = NULL WHERE id = $1")
        .bind(id)
        .execute(&mut su)
        .await;
    assert!(
        cleared.is_err(),
        "superseded_by is not cleared by a statement"
    );
    record_feedback(&w.t.db, newer, MR7, None, w.alice, PERSON, Verdict::Useful)
        .await
        .expect("vote");
    let detached =
        sqlx::query("UPDATE forge.llm_advice_feedback SET advice_id = NULL WHERE advice_id = $1")
            .bind(newer)
            .execute(&mut su)
            .await;
    assert!(detached.is_err(), "advice_id is not cleared by a statement");
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
        record_feedback(db, id, MR7, Some(0), w.alice, PERSON, Verdict::Useful)
            .await
            .expect("first"),
        FeedbackOutcome::Recorded
    );
    assert_eq!(
        record_feedback(db, id, MR7, Some(0), w.alice, PERSON, Verdict::Useful)
            .await
            .expect("again"),
        FeedbackOutcome::Unchanged
    );
    assert_eq!(
        record_feedback(db, id, MR7, Some(0), w.alice, PERSON, Verdict::Wrong)
            .await
            .expect("changed"),
        FeedbackOutcome::Changed
    );
    // Another finding, another person, the advice as a whole: their own rows.
    for (finding, user) in [(Some(1), w.alice), (Some(0), w.bob), (None, w.alice)] {
        assert_eq!(
            record_feedback(db, id, MR7, finding, user, PERSON, Verdict::Useful)
                .await
                .expect("own row"),
            FeedbackOutcome::Recorded
        );
    }
    // A finding the advice does not have, an advice that does not exist.
    assert_eq!(
        record_feedback(db, id, MR7, Some(2), w.alice, PERSON, Verdict::Useful)
            .await
            .expect("none"),
        FeedbackOutcome::NoAdvice
    );
    assert_eq!(
        record_feedback(db, id + 1000, MR7, None, w.alice, PERSON, Verdict::Useful)
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
    // M5 L5-6: each person reads their own verdicts by finding (the
    // advice-as-a-whole row is not a finding's).
    assert_eq!(
        my_feedback(db, id, w.alice).await.expect("alice"),
        vec![(0, Verdict::Wrong), (1, Verdict::Useful)]
    );
    assert_eq!(
        my_feedback(db, id, w.bob).await.expect("bob"),
        vec![(0, Verdict::Useful)]
    );
    // The metric `forge_llm_advice_feedback_total{purpose,verdict}`.
    assert_eq!(
        ops::feedback_totals(db).await.expect("totals"),
        vec![
            ("mr_review".to_owned(), "useful".to_owned(), 3),
            ("mr_review".to_owned(), "wrong".to_owned(), 1),
        ]
    );
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
    record_feedback(&w.t.db, old, MR7, Some(0), w.alice, PERSON, Verdict::Wrong)
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
    assert!(read(&w.t.db, old, MR7, PERSON)
        .await
        .expect("read")
        .is_none());
    assert!(read(&w.t.db, fresh, MR8, PERSON)
        .await
        .expect("read")
        .is_some());
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

/// A live request of `priority` queued `age` ago (`queued` or `running`),
/// or one that gave up in the queue `age` ago (`abandoned` FG-LLM-0004).
async fn aged_request(w: &World, priority: &str, age: &str, state: &str) {
    let (code, started, finished) = match state {
        "queued" => ("NULL", "NULL", "NULL"),
        "running" => ("NULL", "now()", "NULL"),
        _ => ("'FG-LLM-0004'", "NULL", "now() - interval '1 minute'"),
    };
    w.t.exec_superuser(&format!(
        "INSERT INTO forge.llm_requests (purpose, priority, subject_kind, subject_id, project_id, \
           model, prompt_id, prompt_version, input_sha256, input_class, request_id, state, \
           error_code, queued_at, started_at, finished_at) \
         VALUES ('mr_review', '{priority}', 'merge_request', 7, {}, 'qwen3:27b', 'mr_review', 1, \
           repeat('b', 64), 'internal', 'req-llm-aged', '{state}', {code}, \
           now() - interval '{age}', {started}, {finished})",
        w.web.project_id.get()
    ))
    .await;
}

#[tokio::test]
async fn queue_health_reports_the_oldest_wait_per_tier_and_recent_deadline_misses() {
    let w = world().await;
    assert_eq!(
        queue_health(&w.t.db).await.expect("empty"),
        QueueHealth::default()
    );
    aged_request(&w, "batch", "2 hours", "queued").await;
    aged_request(&w, "review", "40 minutes", "queued").await;
    aged_request(&w, "review", "5 minutes", "queued").await;
    aged_request(&w, "interactive", "1 minute", "running").await;
    // Gave up in the queue: counted when it ended in the last hour.
    aged_request(&w, "review", "31 minutes", "abandoned").await;
    w.t.exec_superuser(&format!(
        "INSERT INTO forge.llm_requests (purpose, priority, subject_kind, project_id, model, \
           prompt_id, prompt_version, input_sha256, input_class, request_id, state, error_code, \
           queued_at, finished_at) \
         VALUES ('mr_review', 'review', 'merge_request', {}, 'qwen3:27b', 'mr_review', 1, \
           repeat('b', 64), 'internal', 'req-llm-old-miss', 'abandoned', 'FG-LLM-0004', \
           now() - interval '3 hours', now() - interval '2 hours')",
        w.web.project_id.get()
    ))
    .await;
    let h = queue_health(&w.t.db).await.expect("health");
    assert_eq!(h.queued, 3);
    assert_eq!(h.running, 1);
    assert_eq!(h.deadline_misses, 1, "only the miss of the last hour");
    let tiers: Vec<Priority> = h.oldest_queued_s.iter().map(|(p, _)| *p).collect();
    assert_eq!(tiers, [Priority::Review, Priority::Batch], "highest first");
    let review = h.oldest_queued_s[0].1;
    assert!((40 * 60..41 * 60).contains(&review), "{review}");
    let batch = h.oldest_queued_s[1].1;
    assert!((2 * 3600..2 * 3600 + 60).contains(&batch), "{batch}");
}
