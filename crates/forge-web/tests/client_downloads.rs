#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! The `gstn` CLI distribution (`ClientDownload`): `GET /api/v1/clients`
//! and `GET /api/v1/clients/gstn/{target}/binary` through the product
//! router with a fake authenticator (synthetic identities) and a scratch
//! client directory: the listing omits absent targets, the checksum header
//! is the manifest's, an unknown or absent target is `404 FG-WEB-0012`, a
//! broken file is `500 FG-WEB-0013`, and the matrix rows hold over HTTP.

use std::path::Path;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use forge_core::authenticator::{AuthFuture, Authenticator, TokenIdentity};
use forge_core::namespace_resolver::LookupFuture;
use forge_core::{LookupPath, ProjectId, ProjectRecord, RequestId, Resolution, Scope, UserId};
use forge_errors::{ErrorCode, ForgeError};
use forge_web::routes::clients::ClientDist;
use forge_web::{AppConfig, AppState, Lookups};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tower::ServiceExt;

const HUMAN: &str =
    "fgp_00000000000000b1_1111111111111111111111111111111111111111111111111111111111111111";
const AGENT: &str =
    "fgp_00000000000000b3_3333333333333333333333333333333333333333333333333333333333333333";
const REPO_ONLY: &str =
    "fgp_00000000000000b4_4444444444444444444444444444444444444444444444444444444444444444";

struct Fake;

fn identity(user: i64, is_agent: bool, scopes: &[Scope]) -> TokenIdentity {
    TokenIdentity {
        token_id: user * 10,
        user_id: UserId::new(user),
        username: format!("user{user}"),
        display_name: format!("Synthetic User {user}"),
        is_admin: false,
        scopes: scopes.to_vec(),
        is_agent,
        kind: forge_core::UserKind::Human,
    }
}

impl Authenticator for Fake {
    fn authenticate<'a>(&'a self, _: &'a RequestId, presented: &'a str) -> AuthFuture<'a> {
        let result = match presented {
            HUMAN => Ok(identity(1, false, &[Scope::ReadApi])),
            AGENT => Ok(identity(3, true, &[Scope::ReadApi])),
            REPO_ONLY => Ok(identity(4, false, &[Scope::ReadRepository])),
            _ => Err(ForgeError::new(ErrorCode::Unauthenticated).with_context("reason", "unknown")),
        };
        Box::pin(async move { result })
    }
}

struct NoLookups;

impl forge_core::NamespaceResolver for NoLookups {
    fn resolve<'a>(&'a self, _: &'a LookupPath, _: Option<UserId>) -> LookupFuture<'a, Resolution> {
        Box::pin(async { Ok(Resolution::Absent) })
    }
}

impl forge_core::ProjectAccessLoader for NoLookups {
    fn load_project<'a>(
        &'a self,
        _: ProjectId,
        _: Option<UserId>,
    ) -> LookupFuture<'a, Option<ProjectRecord>> {
        Box::pin(async { Ok(None) })
    }
}

struct Got {
    status: StatusCode,
    headers: axum::http::HeaderMap,
    bytes: Vec<u8>,
}

impl Got {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.bytes).unwrap_or(Value::Null)
    }
}

async fn get(dir: &Path, token: Option<&str>, path: &str) -> Got {
    let lookups = Lookups {
        namespaces: Arc::new(NoLookups),
        projects: Arc::new(NoLookups),
    };
    let state = AppState::new("test", Vec::new(), Arc::new(Fake), lookups)
        .with_clients(ClientDist::new(dir.to_path_buf()));
    let app = forge_web::app(state, AppConfig::default()).expect("router");
    let mut req = Request::get(path);
    if let Some(t) = token {
        req = req.header("authorization", format!("Bearer {t}"));
    }
    let res = app
        .oneshot(req.body(Body::empty()).expect("request"))
        .await
        .expect("infallible");
    let status = res.status();
    let headers = res.headers().clone();
    let bytes = res.into_body().collect().await.expect("body").to_bytes();
    Got {
        status,
        headers,
        bytes: bytes.to_vec(),
    }
}

fn hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A directory shipping linux (and, in the manifest only, windows: its
/// file is missing).
fn linux_only() -> (tempfile::TempDir, Vec<u8>) {
    let dir = tempfile::tempdir().expect("dir");
    let linux = b"\x7fELF synthetic gstn".to_vec();
    std::fs::write(dir.path().join("gstn-linux-x86_64"), &linux).expect("binary");
    let manifest = json!({"clients": [
        {"name": "gstn", "target": "linux-x86_64", "version": "0.1.0+abc1234",
         "sha256": hex(&linux), "size": linux.len()},
        {"name": "gstn", "target": "windows-x86_64", "version": "0.1.0+abc1234",
         "sha256": hex(b"absent"), "size": 6},
    ]});
    std::fs::write(
        dir.path().join("manifest.json"),
        serde_json::to_vec(&manifest).expect("json"),
    )
    .expect("manifest");
    (dir, linux)
}

#[tokio::test]
async fn the_listing_omits_absent_targets() {
    let (dir, linux) = linux_only();
    let got = get(dir.path(), Some(HUMAN), "/api/v1/clients").await;
    assert_eq!(got.status, StatusCode::OK);
    assert_eq!(
        got.json(),
        json!({"items": [{
            "name": "gstn",
            "target": "linux-x86_64",
            "version": "0.1.0+abc1234",
            "sha256": hex(&linux),
            "size": linux.len(),
            "download_path": "/api/v1/clients/gstn/linux-x86_64/binary",
        }]})
    );
    // No directory at all: nothing shipped, not an error.
    let empty = tempfile::tempdir().expect("dir");
    let none = get(&empty.path().join("absent"), Some(HUMAN), "/api/v1/clients").await;
    assert_eq!(none.status, StatusCode::OK);
    assert_eq!(none.json(), json!({"items": []}));
}

#[tokio::test]
async fn the_checksum_header_matches_the_manifest() {
    let (dir, linux) = linux_only();
    let got = get(
        dir.path(),
        Some(AGENT),
        "/api/v1/clients/gstn/linux-x86_64/binary",
    )
    .await;
    assert_eq!(got.status, StatusCode::OK);
    assert_eq!(got.bytes, linux);
    let h = |name: &str| {
        got.headers
            .get(name)
            .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
    };
    assert_eq!(h("x-checksum-sha256"), Some(hex(&linux)));
    assert_eq!(
        h("content-type").as_deref(),
        Some("application/octet-stream")
    );
    assert_eq!(
        h("content-disposition").as_deref(),
        Some("attachment; filename=\"gstn\"")
    );
    assert_eq!(h("content-length"), Some(linux.len().to_string()));
}

#[tokio::test]
async fn the_windows_binary_is_saved_as_gstn_exe() {
    let dir = tempfile::tempdir().expect("dir");
    let exe = b"MZ synthetic gstn.exe".to_vec();
    std::fs::write(dir.path().join("gstn-windows-x86_64.exe"), &exe).expect("binary");
    let manifest = json!({"clients": [{"name": "gstn", "target": "windows-x86_64",
        "version": "0.1.0", "sha256": hex(&exe), "size": exe.len()}]});
    std::fs::write(
        dir.path().join("manifest.json"),
        serde_json::to_vec(&manifest).expect("json"),
    )
    .expect("manifest");
    let got = get(
        dir.path(),
        Some(HUMAN),
        "/api/v1/clients/gstn/windows-x86_64/binary",
    )
    .await;
    assert_eq!(got.status, StatusCode::OK);
    assert_eq!(got.bytes, exe);
    assert_eq!(
        got.headers.get("content-disposition").map(|v| v.as_bytes()),
        Some(&b"attachment; filename=\"gstn.exe\""[..])
    );
}

#[tokio::test]
async fn an_unknown_or_absent_target_is_404() {
    let (dir, _) = linux_only();
    for path in [
        // Not a target at all.
        "/api/v1/clients/gstn/darwin-arm64/binary",
        "/api/v1/clients/gstn/manifest.json/binary",
        // In the manifest, file missing.
        "/api/v1/clients/gstn/windows-x86_64/binary",
    ] {
        let got = get(dir.path(), Some(HUMAN), path).await;
        assert_eq!(got.status, StatusCode::NOT_FOUND, "{path}");
        assert_eq!(
            got.json()["error_code"],
            ErrorCode::ClientBinaryNotFound.id(),
            "{path}"
        );
    }
    // Nothing shipped at all.
    let empty = tempfile::tempdir().expect("dir");
    let got = get(
        empty.path(),
        Some(HUMAN),
        "/api/v1/clients/gstn/linux-x86_64/binary",
    )
    .await;
    assert_eq!(got.status, StatusCode::NOT_FOUND);
    assert_eq!(
        got.json()["error_code"],
        ErrorCode::ClientBinaryNotFound.id()
    );
}

#[tokio::test]
async fn a_size_mismatch_is_a_broken_distribution() {
    let (dir, _) = linux_only();
    std::fs::write(dir.path().join("gstn-linux-x86_64"), b"truncated").expect("replace");
    let got = get(
        dir.path(),
        Some(HUMAN),
        "/api/v1/clients/gstn/linux-x86_64/binary",
    )
    .await;
    assert_eq!(got.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        got.json()["error_code"],
        ErrorCode::ClientDistributionInvalid.id()
    );
    // Every manifest entry is broken (linux short, windows missing): the
    // listing says the distribution is broken instead of an empty 200.
    let list = get(dir.path(), Some(HUMAN), "/api/v1/clients").await;
    assert_eq!(list.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        list.json()["error_code"],
        ErrorCode::ClientDistributionInvalid.id()
    );
    // One servable entry is enough: the broken one is left out.
    let exe = b"MZ synthetic".to_vec();
    std::fs::write(dir.path().join("gstn-windows-x86_64.exe"), &exe).expect("exe");
    let manifest = json!({"clients": [
        {"name": "gstn", "target": "linux-x86_64", "version": "0.1.0",
         "sha256": hex(b"other"), "size": 999},
        {"name": "gstn", "target": "windows-x86_64", "version": "0.1.0",
         "sha256": hex(&exe), "size": exe.len()},
    ]});
    std::fs::write(
        dir.path().join("manifest.json"),
        serde_json::to_vec(&manifest).expect("json"),
    )
    .expect("manifest");
    let partial = get(dir.path(), Some(HUMAN), "/api/v1/clients").await;
    assert_eq!(partial.status, StatusCode::OK);
    let items = partial.json()["items"].clone();
    assert_eq!(items.as_array().map(Vec::len), Some(1));
    assert_eq!(items[0]["target"], "windows-x86_64");
    // A malformed manifest is the same code.
    std::fs::write(dir.path().join("manifest.json"), b"{\"clients\": 1}").expect("bad");
    let bad = get(dir.path(), Some(HUMAN), "/api/v1/clients").await;
    assert_eq!(bad.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        bad.json()["error_code"],
        ErrorCode::ClientDistributionInvalid.id()
    );
}

#[tokio::test]
async fn anonymous_and_tokens_without_read_api_are_refused() {
    let (dir, _) = linux_only();
    for path in [
        "/api/v1/clients",
        "/api/v1/clients/gstn/linux-x86_64/binary",
    ] {
        let got = get(dir.path(), None, path).await;
        assert_eq!(got.status, StatusCode::UNAUTHORIZED, "{path}");
        assert_eq!(got.json()["error_code"], ErrorCode::Unauthenticated.id());
        let got = get(dir.path(), Some(REPO_ONLY), path).await;
        assert_eq!(got.status, StatusCode::FORBIDDEN, "{path}");
        assert_eq!(got.json()["error_code"], ErrorCode::InsufficientScope.id());
    }
}
