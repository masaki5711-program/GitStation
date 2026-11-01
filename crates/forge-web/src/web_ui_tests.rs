//! M2 MR2 (ADR-0022 §2 / §3, ADR-0015 Amendment M2): the shell is the same
//! bytes for every path, `/-/assets/{file}` serves only embedded files, and
//! every response class carries its row of the security-header table. The
//! tests run with and without `embed-ui` (the asset-serving ones only with
//! it: `cargo nextest run -p forge-web --features embed-ui` after
//! `npm run build`).

use std::time::Duration;

use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use forge_errors::ErrorCode;
use http_body_util::BodyExt;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tower::ServiceExt;

use crate::app::{build, AppConfig, AppState};
use crate::context::REQUEST_ID_HEADER;
use crate::tests::{no_lookups, no_tokens};

const SHELL_CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self'; \
img-src 'self' data:; font-src 'self'; connect-src 'self'; object-src 'none'; \
base-uri 'none'; frame-ancestors 'none'; form-action 'self'";
const NONE_CSP: &str = "default-src 'none'; frame-ancestors 'none'";

struct Answer {
    status: StatusCode,
    headers: HeaderMap,
    bytes: Vec<u8>,
}

impl Answer {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }

    fn code(&self) -> String {
        let v: Value = serde_json::from_slice(&self.bytes).expect("JSON error body");
        v["error_code"].as_str().expect("error_code").to_owned()
    }
}

fn app(config: AppConfig) -> axum::Router {
    build(
        crate::app::registry(),
        AppState::new("test", Vec::new(), no_tokens(), no_lookups()),
        config,
    )
    .expect("router")
}

async fn send_with(config: AppConfig, method: &str, uri: &str, headers: &[(&str, &str)]) -> Answer {
    let mut req = Request::builder().method(method).uri(uri);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let res = app(config)
        .oneshot(req.body(Body::empty()).expect("request"))
        .await
        .expect("infallible");
    let status = res.status();
    let headers = res.headers().clone();
    let bytes = res
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes()
        .to_vec();
    Answer {
        status,
        headers,
        bytes,
    }
}

async fn get(uri: &str) -> Answer {
    send_with(AppConfig::default(), "GET", uri, &[]).await
}

fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn without_request_id(h: &HeaderMap) -> Vec<(String, Vec<u8>)> {
    let mut v: Vec<_> = h
        .iter()
        .filter(|(k, _)| *k != REQUEST_ID_HEADER)
        .map(|(k, v)| (k.as_str().to_owned(), v.as_bytes().to_vec()))
        .collect();
    v.sort();
    v
}

/// The common row of the table (every class but Smart HTTP).
fn assert_common(a: &Answer, what: &str) {
    assert_eq!(
        a.header("x-content-type-options"),
        Some("nosniff"),
        "{what}"
    );
    assert_eq!(a.header("referrer-policy"), Some("same-origin"), "{what}");
    assert_eq!(
        a.header("cross-origin-opener-policy"),
        Some("same-origin"),
        "{what}"
    );
    assert_eq!(
        a.header("permissions-policy"),
        Some("camera=(), microphone=(), geolocation=(), usb=(), payment=()"),
        "{what}"
    );
    assert_eq!(
        a.header("strict-transport-security"),
        None,
        "{what}: HSTS is off by default"
    );
}

// ── the shell ────────────────────────────────────────────────────────────

const SHELL_PATHS: &[&str] = &[
    "/acme",
    "/acme/platform/forge-web",
    "/does/not/exist",
    "/acme/x/-/blob/main/src/%E3%81%82.rs",
    "/acme/x/-/tree/main",
    "/acme/x/-/settings",
    "/acme/-/members",
    "/-/projects/42",
    "/-/projects/42/-/commits/main",
];

#[tokio::test]
async fn the_shell_is_byte_identical_for_every_path_by_hash() {
    let expected = sha256(crate::ui::shell_html());
    let first = get(SHELL_PATHS[0]).await;
    for path in SHELL_PATHS {
        for (method, query) in [("GET", ""), ("GET", "?tab=1&x=%3Cscript%3E")] {
            let a = send_with(
                AppConfig::default(),
                method,
                &format!("{path}{query}"),
                &[("accept-language", "en"), ("cookie", "a=b")],
            )
            .await;
            assert_eq!(a.status, StatusCode::OK, "{path}");
            assert_eq!(sha256(&a.bytes), expected, "{path}{query}");
            assert_eq!(
                without_request_id(&a.headers),
                without_request_id(&first.headers),
                "{path}{query}: headers differ"
            );
        }
    }
    assert_eq!(
        first.header("content-type"),
        Some("text/html; charset=utf-8")
    );
    assert_eq!(first.header("cache-control"), Some("no-cache"));
    assert_eq!(first.header("content-security-policy"), Some(SHELL_CSP));
    assert_eq!(first.header("cross-origin-resource-policy"), None);
    assert_common(&first, "shell");
}

#[cfg(not(feature = "embed-ui"))]
#[tokio::test]
async fn without_embed_ui_the_shell_is_the_placeholder() {
    let a = get("/acme").await;
    assert_eq!(a.bytes, crate::ui::PLACEHOLDER_HTML.as_bytes());
    assert_eq!(crate::ui::status(), crate::ui::UiStatus::Placeholder);
}

#[cfg(feature = "embed-ui")]
#[tokio::test]
async fn with_embed_ui_the_shell_is_the_built_index_html() {
    let a = get("/acme").await;
    let built = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../web/dist/index.html"
    ))
    .expect("web/dist/index.html");
    assert_eq!(sha256(&a.bytes), sha256(&built));
    let html = String::from_utf8(a.bytes).expect("utf-8");
    // No inline script or style: the CSP allows 'self' only.
    assert!(!html.contains("<script>"), "{html}");
    assert!(!html.contains("<style"), "{html}");
    assert!(html.contains("src=\"/-/assets/"), "{html}");
}

// ── assets ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn an_unknown_asset_is_fg_web_0001_with_the_strict_policy_never_the_shell() {
    for (method, uri) in [
        ("GET", "/-/assets/index-0000.js"),
        ("GET", "/-/assets/index.html"),
        ("GET", "/-/assets/missing.js.br"),
        ("GET", "/-/assets/missing.exe"),
        ("HEAD", "/-/assets/index-0000.js"),
        ("GET", "/-/assets"),
        ("GET", "/-/assets/brand/icon.svg"),
        ("GET", "/-/assets/%2e%2e"),
        ("POST", "/-/assets/index-0000.js"),
    ] {
        let a = send_with(AppConfig::default(), method, uri, &[]).await;
        assert_eq!(a.status, StatusCode::NOT_FOUND, "{method} {uri}");
        if method != "HEAD" {
            assert_eq!(a.code(), ErrorCode::RouteNotFound.id(), "{method} {uri}");
        }
        assert_ne!(a.bytes, crate::ui::shell_html(), "{uri}");
        assert_eq!(a.header("content-security-policy"), Some(NONE_CSP), "{uri}");
        assert_ne!(
            a.header("cache-control"),
            Some(crate::routes::assets::CACHE_IMMUTABLE),
            "{uri}: an error is never cached for a year"
        );
        assert_common(&a, uri);
    }
}

#[cfg(feature = "embed-ui")]
mod embedded {
    use super::*;

    /// Every file of the build (originals only).
    fn built_assets() -> Vec<String> {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../web/dist");
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .expect("web/dist")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .filter(|n| n != "index.html" && !n.ends_with(".br") && !n.ends_with(".gz"))
            .collect();
        names.sort();
        names
    }

    fn on_disk(name: &str) -> Option<Vec<u8>> {
        std::fs::read(format!(
            "{}/../../web/dist/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .ok()
    }

    #[tokio::test]
    async fn every_built_asset_is_served_immutable_with_its_type_and_policy() {
        let names = built_assets();
        assert!(names.iter().any(|n| n.ends_with(".js")), "{names:?}");
        for name in &names {
            let a = get(&format!("/-/assets/{name}")).await;
            assert_eq!(a.status, StatusCode::OK, "{name}");
            assert_eq!(
                Some(a.bytes.clone()),
                on_disk(name),
                "{name}: identity bytes"
            );
            assert_eq!(a.header("content-encoding"), None, "{name}");
            assert_eq!(
                a.header("cache-control"),
                Some(crate::routes::assets::CACHE_IMMUTABLE),
                "{name}"
            );
            assert_eq!(a.header("vary"), Some("Accept-Encoding"), "{name}");
            assert_eq!(
                a.header("content-security-policy"),
                Some(SHELL_CSP),
                "{name}"
            );
            assert_eq!(
                a.header("cross-origin-resource-policy"),
                Some("same-origin"),
                "{name}"
            );
            let ext = name.rsplit_once('.').map(|(_, e)| e).expect("extension");
            let expected = crate::ui::EXTENSIONS
                .iter()
                .find(|(e, _)| *e == ext)
                .map(|(_, t)| *t);
            assert_eq!(a.header("content-type"), expected, "{name}");
            assert_common(&a, name);
        }
    }

    #[tokio::test]
    async fn precompressed_variants_follow_accept_encoding() {
        let name = built_assets()
            .into_iter()
            .find(|n| n.ends_with(".js"))
            .expect("a script");
        let uri = format!("/-/assets/{name}");
        let br = on_disk(&format!("{name}.br")).expect("the build writes .br");
        let gz = on_disk(&format!("{name}.gz")).expect("the build writes .gz");
        for (accept, encoding, bytes) in [
            ("gzip, deflate, br", Some("br"), &br),
            ("br;q=0, gzip", Some("gzip"), &gz),
            ("gzip", Some("gzip"), &gz),
        ] {
            let a = send_with(
                AppConfig::default(),
                "GET",
                &uri,
                &[("accept-encoding", accept)],
            )
            .await;
            assert_eq!(a.status, StatusCode::OK, "{accept}");
            assert_eq!(a.header("content-encoding"), encoding, "{accept}");
            assert_eq!(&a.bytes, bytes, "{accept}");
            assert_eq!(a.header("vary"), Some("Accept-Encoding"), "{accept}");
        }
        let plain = send_with(
            AppConfig::default(),
            "GET",
            &uri,
            &[("accept-encoding", "identity")],
        )
        .await;
        assert_eq!(plain.header("content-encoding"), None);
        assert_eq!(Some(plain.bytes), on_disk(&name));
        let head = send_with(AppConfig::default(), "HEAD", &uri, &[]).await;
        assert_eq!(head.status, StatusCode::OK);
        assert!(head.bytes.is_empty());
    }
}

// ── the other classes ────────────────────────────────────────────────────

#[tokio::test]
async fn api_answers_carry_the_api_policy_and_no_store() {
    // Unauthenticated (401) and unknown API paths (404) alike.
    for uri in ["/api/v1/projects", "/api/v1/user", "/api/v1/nothing-here"] {
        let a = get(uri).await;
        assert!(a.status.is_client_error(), "{uri}: {}", a.status);
        assert_eq!(a.header("content-security-policy"), Some(NONE_CSP), "{uri}");
        assert_eq!(
            a.header("cache-control"),
            Some("private, no-store"),
            "{uri}"
        );
        assert_eq!(a.header("vary"), Some("Authorization, Cookie"), "{uri}");
        assert_eq!(a.header("cross-origin-resource-policy"), None, "{uri}");
        assert_common(&a, uri);
    }
}

#[tokio::test]
async fn raw_and_archive_paths_are_sandboxed_even_before_they_are_routed() {
    for uri in ["/acme/x/-/raw/main/a.html", "/acme/x/-/archive/main.zip"] {
        let a = get(uri).await;
        assert_eq!(
            a.header("content-security-policy"),
            Some("sandbox; default-src 'none'"),
            "{uri}"
        );
        assert_eq!(
            a.header("cross-origin-resource-policy"),
            Some("same-origin"),
            "{uri}"
        );
        assert_common(&a, uri);
    }
}

#[tokio::test]
async fn login_paths_are_never_stored() {
    let a = get("/-/auth/login").await;
    assert_eq!(a.header("content-security-policy"), Some(NONE_CSP));
    assert_eq!(a.header("cache-control"), Some("no-store"));
    assert_common(&a, "/-/auth/login");
}

#[tokio::test]
async fn health_and_other_answers_get_the_strict_policy() {
    for uri in ["/healthz", "/-/unknown", "/acme/x/-/nothing"] {
        let a = get(uri).await;
        assert_eq!(a.header("content-security-policy"), Some(NONE_CSP), "{uri}");
        assert_common(&a, uri);
    }
}

#[tokio::test]
async fn smart_http_answers_are_left_as_they_were() {
    let a = get("/acme/x.git/info/refs?service=git-upload-pack").await;
    assert_eq!(a.status, StatusCode::UNAUTHORIZED);
    for name in [
        "content-security-policy",
        "referrer-policy",
        "cross-origin-opener-policy",
        "permissions-policy",
        "cross-origin-resource-policy",
    ] {
        assert_eq!(a.header(name), None, "{name}");
    }
}

#[tokio::test]
async fn hsts_is_sent_everywhere_but_git_when_configured() {
    let config = AppConfig {
        hsts: Some(Duration::from_secs(15_552_000)),
        ..AppConfig::default()
    };
    for uri in ["/acme", "/api/v1/projects", "/healthz", "/-/assets/x.js"] {
        let a = send_with(config, "GET", uri, &[]).await;
        assert_eq!(
            a.header("strict-transport-security"),
            Some("max-age=15552000"),
            "{uri}"
        );
    }
    let git = send_with(
        config,
        "GET",
        "/acme/x.git/info/refs?service=git-upload-pack",
        &[],
    )
    .await;
    assert_eq!(git.header("strict-transport-security"), None);
}
