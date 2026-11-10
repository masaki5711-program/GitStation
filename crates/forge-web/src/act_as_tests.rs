//! The act-as step without a database (review of ADR-0025, 2026-11-09,
//! security LOW a): act-as headers without a credential are refused
//! `FG-AUTH-0001`, never stripped to continue anonymously.

use axum::body::Body;
use axum::extract::Request;
use axum::http::StatusCode;
use forge_auth::Action;
use forge_core::RequestId;
use forge_errors::Lang;
use http_body_util::BodyExt;
use serde_json::Value;

use super::admit;
use crate::auth::Surface;

fn request(headers: &[(&str, &str)]) -> Request {
    let mut b = axum::http::Request::builder()
        .method("GET")
        .uri("/api/v1/projects/1");
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    b.body(Body::empty()).expect("request")
}

async fn code(res: axum::response::Response) -> (StatusCode, String) {
    let status = res.status();
    let bytes = res.into_body().collect().await.expect("body").to_bytes();
    let v: Value = serde_json::from_slice(&bytes).expect("json");
    (status, v["error_code"].as_str().unwrap_or("").to_owned())
}

#[tokio::test]
async fn act_as_headers_without_a_credential_are_refused_never_stripped() {
    let assertion = format!("fga_{}_{}", "0".repeat(16), "1".repeat(64));
    for headers in [
        vec![("forge-act-as", "user:1")],
        vec![("forge-assertion", assertion.as_str())],
        vec![("forge-act-as", "user:1"), ("forge-assertion", &assertion)],
    ] {
        for action in [
            Some(Action::ProjectRead),
            Some(Action::MergeRequestApprove),
            None,
        ] {
            let mut req = request(&headers);
            let id = RequestId::generate();
            let res = match admit(&mut req, Surface::Api, action, &id, Lang::En).await {
                Ok(_) => panic!("{headers:?} {action:?}: admitted without a credential"),
                Err(res) => *res,
            };
            assert_eq!(
                code(res).await,
                (StatusCode::UNAUTHORIZED, "FG-AUTH-0001".to_owned()),
                "{headers:?} {action:?}"
            );
        }
    }
    // Without the headers nothing changes: no principal, no refusal here
    // (the guard answers FG-AUTH-0001 on a guarded route).
    let mut req = request(&[]);
    let admitted = admit(
        &mut req,
        Surface::Api,
        Some(Action::ProjectRead),
        &RequestId::generate(),
        Lang::En,
    )
    .await;
    assert!(matches!(admitted, Ok(None)));
}
