//! The diagnosis read (plan 19 L5-7b) on a migrated database.

use forge_auth::{prove, AccountKind, Credential, Principal, Scope};
use forge_core::UserId;
use forge_db::llm::NewAdvice;
use forge_db::test_support::FreshDb;

use super::*;

fn advice(purpose: Purpose, kind: SubjectKind, id: Option<i64>, class: InputClass) -> NewAdvice {
    NewAdvice {
        llm_request_id: None,
        purpose,
        subject_kind: kind,
        subject_id: id,
        project: None,
        head_sha: None,
        summary: "書き込みのプールが時間切れになった。".to_owned(),
        findings: serde_json::json!([]),
        deep_review: None,
        dropped_findings: 0,
        degraded: false,
        input_class: class,
        model: "qwen3:27b".to_owned(),
        prompt_version: 1,
    }
}

fn admin_session() -> Principal {
    Principal::authenticated(
        UserId::new(1),
        true,
        true,
        Credential::Session,
        AccountKind::Human,
    )
}

fn admin_agent_token() -> Principal {
    Principal::authenticated(
        UserId::new(1),
        true,
        true,
        Credential::Token(vec![Scope::ReadApi, Scope::Admin]),
        AccountKind::Human,
    )
    .as_agent(true)
}

async fn insert(db: &Db, a: &NewAdvice) -> i64 {
    forge_db::llm::insert_advice(db, a).await.expect("advice")
}

#[tokio::test]
async fn an_administrator_reads_an_incident_diagnosis_with_its_label() {
    let t = FreshDb::migrated().await;
    let id = insert(
        &t.db,
        &advice(
            Purpose::IncidentDiagnosis,
            SubjectKind::Incident,
            None,
            InputClass::Internal,
        ),
    )
    .await;
    let proof = prove(&admin_session(), Action::OpsDiagnosisRead, InstanceCtx).expect("admin");
    let got = Diagnoses::new(t.db.clone())
        .read(&proof, id, false)
        .await
        .expect("read");
    let Diagnosis::Shown(a) = got else {
        panic!("shown to an administrator");
    };
    assert_eq!(a.id, id);
    assert_eq!(a.model, "qwen3:27b");
    assert_eq!(a.prompt_version, 1);
    assert!(a.created_at.ends_with('Z'));
}

#[tokio::test]
async fn confidential_diagnosis_withheld_from_an_admin_agent() {
    let t = FreshDb::migrated().await;
    let service = Diagnoses::new(t.db.clone());
    let internal = insert(
        &t.db,
        &advice(
            Purpose::IncidentDiagnosis,
            SubjectKind::Incident,
            None,
            InputClass::Internal,
        ),
    )
    .await;
    let confidential = insert(
        &t.db,
        &advice(
            Purpose::IncidentDiagnosis,
            SubjectKind::Incident,
            None,
            InputClass::Confidential,
        ),
    )
    .await;
    let proof =
        prove(&admin_agent_token(), Action::OpsDiagnosisRead, InstanceCtx).expect("admin agent");
    assert!(matches!(
        service.read(&proof, internal, false).await.expect("read"),
        Diagnosis::Shown(_)
    ));
    assert_eq!(
        service
            .read(&proof, confidential, false)
            .await
            .expect("read"),
        Diagnosis::Withheld {
            id: confidential,
            input_class: InputClass::Confidential
        }
    );
    // A person who declares an agent audience is treated as one.
    let person = prove(&admin_session(), Action::OpsDiagnosisRead, InstanceCtx).expect("admin");
    assert!(matches!(
        service
            .read(&person, confidential, true)
            .await
            .expect("read"),
        Diagnosis::Withheld { .. }
    ));
}

#[tokio::test]
async fn only_incident_diagnoses_are_served_here() {
    let t = FreshDb::migrated().await;
    let service = Diagnoses::new(t.db.clone());
    let other = insert(
        &t.db,
        &advice(
            Purpose::ApprovalSummary,
            SubjectKind::Deployment,
            Some(5),
            InputClass::Internal,
        ),
    )
    .await;
    let merge_request = insert(
        &t.db,
        &advice(
            Purpose::MrReview,
            SubjectKind::MergeRequest,
            Some(3),
            InputClass::Confidential,
        ),
    )
    .await;
    // An incident subject with another purpose: never "withheld", not found.
    let wrong_purpose = insert(
        &t.db,
        &advice(
            Purpose::MrReview,
            SubjectKind::Incident,
            None,
            InputClass::Confidential,
        ),
    )
    .await;
    let proof = prove(&admin_session(), Action::OpsDiagnosisRead, InstanceCtx).expect("admin");
    let agent =
        prove(&admin_agent_token(), Action::OpsDiagnosisRead, InstanceCtx).expect("admin agent");
    for id in [other, merge_request, wrong_purpose, other + 1000] {
        for (who, declared) in [(&proof, false), (&agent, false), (&proof, true)] {
            let e = service
                .read(who, id, declared)
                .await
                .expect_err("not found");
            assert_eq!(e.code(), ErrorCode::NotFound, "{id}");
        }
    }
}

#[test]
fn a_non_administrator_has_no_proof() {
    let developer = Principal::authenticated(
        UserId::new(2),
        false,
        true,
        Credential::Session,
        AccountKind::Human,
    );
    let e = prove(&developer, Action::OpsDiagnosisRead, InstanceCtx).expect_err("denied");
    assert_eq!(e.code(), ErrorCode::NotFound);
}
