#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! `GET /api/v1/admin/diagnoses/{advice_id}` (plan 19 L5-7b) through the
//! product router against the development database: an administrator reads
//! an incident diagnosis with its label; anyone else gets 404; an admin
//! agent token reads an internal one but not a confidential one's words;
//! any other advice is 404. Synthetic data only.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use forge_auth::{AccountKind, Credential, Principal, Scope};
use forge_core::llm::{InputClass, Purpose, SubjectKind};
use forge_core::UserId;
use forge_db::llm::NewAdvice;
use forge_db::test_support::FreshDb;
use forge_llm::diagnoses::Diagnoses;
use forge_web::{AppConfig, AppState, Lookups};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

async fn get(t: &FreshDb, uri: &str, who: Principal, agent_header: bool) -> (StatusCode, Value) {
    let mut builder = Request::builder().method("GET").uri(uri);
    if agent_header {
        builder = builder.header("forge-context-audience", "agent");
    }
    let mut req = builder.body(Body::empty()).expect("request");
    req.extensions_mut().insert(who);
    let state = AppState::new(
        "test",
        Vec::new(),
        Arc::new(t.db.clone()),
        Lookups {
            namespaces: Arc::new(t.db.clone()),
            projects: Arc::new(t.db.clone()),
        },
    )
    .with_diagnoses(Diagnoses::new(t.db.clone()));
    let res = forge_web::app(state, AppConfig::default())
        .expect("router")
        .oneshot(req)
        .await
        .expect("infallible");
    let status = res.status();
    let raw = res
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes()
        .to_vec();
    (status, serde_json::from_slice(&raw).unwrap_or(Value::Null))
}

async fn advice(t: &FreshDb, purpose: Purpose, kind: SubjectKind, class: InputClass) -> i64 {
    forge_db::llm::insert_advice(
        &t.db,
        &NewAdvice {
            llm_request_id: None,
            purpose,
            subject_kind: kind,
            subject_id: (kind != SubjectKind::Incident).then_some(5),
            project: None,
            head_sha: None,
            summary: "書き込みのプールが時間切れになった。".to_owned(),
            findings: json!([{"severity": "high", "path": "log", "line": 2,
                "quote": "reason=acquire_timeout", "message": "プールが尽きた。次に見るもの: forge doctor --json",
                "confidence": 0.8}]),
            deep_review: None,
            dropped_findings: 0,
            degraded: false,
            input_class: class,
            model: "qwen3:27b".to_owned(),
            prompt_version: 1,
        },
    )
    .await
    .expect("advice")
}

fn person(admin: bool, credential: Credential) -> Principal {
    Principal::authenticated(UserId::new(1), admin, true, credential, AccountKind::Human)
}

fn uri(id: i64) -> String {
    format!("/api/v1/admin/diagnoses/{id}")
}

#[tokio::test]
async fn an_administrator_reads_a_diagnosis_and_others_get_404() {
    let t = FreshDb::migrated().await;
    let id = advice(
        &t,
        Purpose::IncidentDiagnosis,
        SubjectKind::Incident,
        InputClass::Internal,
    )
    .await;
    let (status, body) = get(&t, &uri(id), person(true, Credential::Session), false).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "shown");
    assert_eq!(body["label"], "AI の助言（提案）");
    assert_eq!(body["model"], "qwen3:27b");
    assert_eq!(body["candidates"][0]["line"], 2);
    // diagnosis_read_denied_for_non_admin: a non-administrator, and an
    // administrator's token without the admin scope.
    for who in [
        person(false, Credential::Session),
        person(true, Credential::Token(vec![Scope::Api])),
    ] {
        let (status, body) = get(&t, &uri(id), who, false).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        assert_eq!(body["error_code"], "FG-REPO-0005");
    }
}

#[tokio::test]
async fn confidential_diagnosis_withheld_from_agents() {
    let t = FreshDb::migrated().await;
    let internal = advice(
        &t,
        Purpose::IncidentDiagnosis,
        SubjectKind::Incident,
        InputClass::Internal,
    )
    .await;
    let confidential = advice(
        &t,
        Purpose::IncidentDiagnosis,
        SubjectKind::Incident,
        InputClass::Confidential,
    )
    .await;
    let agent =
        || person(true, Credential::Token(vec![Scope::ReadApi, Scope::Admin])).as_agent(true);
    let (status, body) = get(&t, &uri(internal), agent(), false).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "shown");
    let (status, body) = get(&t, &uri(confidential), agent(), false).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "withheld");
    assert_eq!(body["summary"], Value::Null);
    assert_eq!(body["candidates"], json!([]));
    // A person declaring an agent audience gets the same.
    let (_, body) = get(
        &t,
        &uri(confidential),
        person(true, Credential::Session),
        true,
    )
    .await;
    assert_eq!(body["state"], "withheld");
    let (_, body) = get(
        &t,
        &uri(confidential),
        person(true, Credential::Session),
        false,
    )
    .await;
    assert_eq!(body["state"], "shown");
}

#[tokio::test]
async fn other_advice_and_bad_ids_are_not_served() {
    let t = FreshDb::migrated().await;
    let other = advice(
        &t,
        Purpose::ApprovalSummary,
        SubjectKind::Deployment,
        InputClass::Internal,
    )
    .await;
    let admin = || person(true, Credential::Session);
    let (status, body) = get(&t, &uri(other), admin(), false).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    let (status, _) = get(&t, &uri(other + 1000), admin(), false).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, body) = get(&t, "/api/v1/admin/diagnoses/0", admin(), false).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}
