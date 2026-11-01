#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // tests may fail loudly
//! raw and archive through the product router and real git (synthetic
//! content): types, ADR-0015 headers, option-like names, streaming.

use std::io::Read as _;

use axum::http::StatusCode;
use forge_auth::Scope;
use forge_errors::ErrorCode;
use forge_git::browse::BrowseLimits;

use super::*;
use crate::routes::browse::tests::{repo, router, send, session, token, Answer, Facts, PNG, SVG};

async fn get(path: &str) -> Answer {
    let r = repo().await;
    send(
        router(&r, Facts::default(), BrowseLimits::default()),
        "GET",
        path,
        Some(session()),
    )
    .await
}

/// ADR-0015 / ADR-0022 §3: every content answer.
fn assert_content_headers(a: &Answer) {
    assert_eq!(a.header("x-content-type-options"), "nosniff");
    assert_eq!(
        a.header("content-security-policy"),
        "sandbox; default-src 'none'"
    );
    assert_eq!(a.header("cross-origin-resource-policy"), "same-origin");
    assert_eq!(a.header("cache-control"), "private, no-store");
    assert_eq!(a.header("vary"), "Authorization, Cookie");
}

#[tokio::test]
async fn raw_text_is_text_plain_inline_with_the_content_headers() {
    let a = get("/acme/app/-/raw/main/README.md").await;
    assert_eq!(a.status, StatusCode::OK, "{:?}", a.json());
    assert_eq!(a.bytes, b"# synthetic\n");
    assert_eq!(a.header("content-type"), "text/plain; charset=utf-8");
    assert_eq!(
        a.header("content-disposition"),
        "inline; filename=\"README.md\"; filename*=UTF-8''README.md"
    );
    assert_eq!(a.header("content-length"), "12");
    assert_content_headers(&a);
    // The longest branch with a `/`, and a nested path.
    let a = get("/acme/app/-/raw/feature/x/src/lib.rs").await;
    assert_eq!(a.bytes, b"pub fn f() {}\n");
}

#[tokio::test]
async fn svg_is_only_a_download_and_images_need_matching_bytes() {
    let a = get("/acme/app/-/raw/main/pic.svg").await;
    assert_eq!(a.bytes, SVG);
    assert_eq!(a.header("content-type"), "application/octet-stream");
    assert!(a.header("content-disposition").starts_with("attachment;"));
    assert_content_headers(&a);

    let a = get("/acme/app/-/raw/main/logo.png").await;
    assert_eq!(a.bytes, PNG);
    assert_eq!(a.header("content-type"), "image/png");
    assert!(a.header("content-disposition").starts_with("inline;"));
    assert_content_headers(&a);

    // `.png` whose bytes are text: the extension alone never makes an image.
    let a = get("/acme/app/-/raw/main/fake.png").await;
    assert_eq!(a.header("content-type"), "text/plain; charset=utf-8");
    let a = get("/acme/app/-/raw/main/bin.dat").await;
    assert_eq!(a.header("content-type"), "application/octet-stream");
    assert!(a.header("content-disposition").starts_with("attachment;"));
}

#[test]
fn types_by_extension_and_magic() {
    assert_eq!(raw_type("a.png", PNG), ("image/png", true));
    assert_eq!(raw_type("a.PNG", PNG), ("image/png", true));
    // PNG bytes under `.jpg`: not that image, and NUL bytes are no text.
    assert_eq!(raw_type("a.jpg", PNG), (OCTET, false));
    assert_eq!(raw_type("a.gif", b"GIF89a.."), ("image/gif", true));
    assert_eq!(
        raw_type("a.jpeg", b"\xff\xd8\xff\xe0"),
        ("image/jpeg", true)
    );
    assert_eq!(
        raw_type("a.webp", b"RIFF\0\0\0\0WEBPVP8 "),
        ("image/webp", true)
    );
    assert_eq!(raw_type("a.webp", b"RIFF\0\0\0\0WAVE"), (OCTET, false));
    for name in [
        "x.svg", "x.SVG", "x.html", "x.htm", "x.pdf", "x.xhtml", "x.xml",
    ] {
        assert_eq!(raw_type(name, b"<svg/>"), (OCTET, false), "{name}");
    }
    assert_eq!(raw_type("Makefile", b"all:\n"), (TEXT, true));
    assert_eq!(
        raw_type(".png", PNG),
        (OCTET, false),
        "no stem: not an image"
    );
    assert_eq!(raw_type("a.txt", b"\xff\xfe"), (OCTET, false), "not UTF-8");
}

#[test]
fn dispositions_are_rfc_6266_with_an_ascii_fallback() {
    assert_eq!(
        disposition(false, "仕様 v1.md"),
        "attachment; filename=\"___v1.md\"; filename*=UTF-8''%E4%BB%95%E6%A7%98%20v1.md"
    );
    assert_eq!(
        disposition(true, "a\"b;c\r\n.txt"),
        "inline; filename=\"a_b_c__.txt\"; filename*=UTF-8''a%22b%3Bc%0D%0A.txt"
    );
    assert_eq!(
        disposition(false, "日本"),
        "attachment; filename=\"download\"; filename*=UTF-8''%E6%97%A5%E6%9C%AC"
    );
}

#[tokio::test]
async fn names_that_look_like_options_are_files() {
    let a = get("/acme/app/-/raw/main/--output=x").await;
    assert_eq!(a.bytes, b"option-like\n");
    let a = get("/acme/app/-/raw/main/-n").await;
    assert_eq!(a.bytes, b"dash-n\n");
    for path in [
        "/acme/app/-/raw/--output=x/README.md",
        "/acme/app/-/raw/-n/README.md",
        "/acme/app/-/raw/main/--output=written",
        "/acme/app/-/archive/--output=x.zip",
    ] {
        let a = get(path).await;
        assert_eq!(a.status, StatusCode::NOT_FOUND, "{path}");
        assert_eq!(
            a.code(),
            ErrorCode::RepositoryPathNotResolved.id(),
            "{path}"
        );
        assert_content_headers_on_errors(&a);
    }
    let a = get("/acme/app/-/raw/main/%E4%BB%95%E6%A7%98.md").await;
    assert_eq!(a.bytes, "日本語の仕様\n".as_bytes());
    assert!(a
        .header("content-disposition")
        .contains("filename*=UTF-8''%E4%BB%95"));
}

fn assert_content_headers_on_errors(a: &Answer) {
    assert_eq!(
        a.header("content-security-policy"),
        "sandbox; default-src 'none'"
    );
    assert_eq!(a.header("x-content-type-options"), "nosniff");
}

#[tokio::test]
async fn raw_head_answers_headers_only() {
    let r = repo().await;
    let a = send(
        router(&r, Facts::default(), BrowseLimits::default()),
        "HEAD",
        "/acme/app/-/raw/main/README.md",
        Some(session()),
    )
    .await;
    assert_eq!(a.status, StatusCode::OK);
    assert!(a.bytes.is_empty());
    assert_eq!(a.header("content-type"), "text/plain; charset=utf-8");
    // A large file: no stream is started for a HEAD.
    let small = BrowseLimits {
        raw_inline_max_bytes: 2,
        ..BrowseLimits::default()
    };
    let a = send(
        router(&r, Facts::default(), small),
        "HEAD",
        "/acme/app/-/raw/main/README.md",
        Some(session()),
    )
    .await;
    assert_eq!(a.status, StatusCode::OK);
    assert_eq!(a.header("content-type"), "application/octet-stream");
    assert_eq!(a.header("content-length"), "12");
}

#[tokio::test]
async fn large_raw_files_stream_as_downloads() {
    let r = repo().await;
    let small = BrowseLimits {
        raw_inline_max_bytes: 2,
        ..BrowseLimits::default()
    };
    let a = send(
        router(&r, Facts::default(), small),
        "GET",
        "/acme/app/-/raw/main/README.md",
        Some(session()),
    )
    .await;
    assert_eq!(a.status, StatusCode::OK);
    assert_eq!(a.bytes, b"# synthetic\n");
    assert_eq!(a.header("content-type"), "application/octet-stream");
    assert!(a.header("content-disposition").starts_with("attachment;"));
    assert_content_headers(&a);
}

#[tokio::test]
async fn archives_stream_zip_and_tar_gz() {
    let r = repo().await;
    let app = || router(&r, Facts::default(), BrowseLimits::default());
    let a = send(
        app(),
        "GET",
        "/acme/app/-/archive/main.zip",
        Some(session()),
    )
    .await;
    assert_eq!(a.status, StatusCode::OK, "{:?}", a.json());
    assert!(a.bytes.starts_with(b"PK"));
    assert_eq!(a.header("content-type"), "application/zip");
    assert_eq!(
        a.header("content-disposition"),
        "attachment; filename=\"app-main.zip\"; filename*=UTF-8''app-main.zip"
    );
    assert_content_headers(&a);
    let prefix = format!("app-{}/README.md", &r.commit[..12]);
    assert!(String::from_utf8_lossy(&a.bytes).contains(&prefix));

    let a = send(
        app(),
        "GET",
        "/acme/app/-/archive/feature/x.tar.gz",
        Some(session()),
    )
    .await;
    assert_eq!(a.header("content-type"), "application/gzip");
    assert!(a
        .header("content-disposition")
        .contains("app-feature-x.tar.gz"));
    let mut tar = Vec::new();
    flate2::read::GzDecoder::new(a.bytes.as_slice())
        .read_to_end(&mut tar)
        .expect("gzip");
    assert!(String::from_utf8_lossy(&tar).contains("README.md"));

    let a = send(app(), "HEAD", "/acme/app/-/archive/v1.zip", Some(session())).await;
    assert_eq!(a.status, StatusCode::OK);
    assert!(a.bytes.is_empty());

    // (the reasons, logged only: archive_format, path_remainder, ref_not_found)
    for path in [
        "/acme/app/-/archive/main.rar",
        "/acme/app/-/archive/main/README.md.zip",
        "/acme/app/-/archive/.zip",
    ] {
        let a = send(app(), "GET", path, Some(session())).await;
        assert_eq!(a.status, StatusCode::NOT_FOUND, "{path}");
        assert_eq!(
            a.code(),
            ErrorCode::RepositoryPathNotResolved.id(),
            "{path}"
        );
    }
}

#[tokio::test]
async fn content_routes_need_read_repository_and_a_reader() {
    let r = repo().await;
    let path = "/acme/app/-/raw/main/README.md";
    let a = send(
        router(&r, Facts::default(), BrowseLimits::default()),
        "GET",
        path,
        Some(token(&[Scope::ReadRepository])),
    )
    .await;
    assert_eq!(a.status, StatusCode::OK);
    let a = send(
        router(&r, Facts::default(), BrowseLimits::default()),
        "GET",
        path,
        Some(token(&[Scope::ReadApi])),
    )
    .await;
    assert_eq!(a.code(), ErrorCode::InsufficientScope.id());
    let hidden = Facts {
        role: None,
        ..Facts::default()
    };
    let a = send(
        router(&r, hidden, BrowseLimits::default()),
        "GET",
        path,
        Some(session()),
    )
    .await;
    assert_eq!(a.code(), ErrorCode::NotFound.id());
    let provisioning = Facts {
        state: forge_core::RepositoryState::Provisioning,
        ..Facts::default()
    };
    let a = send(
        router(&r, provisioning, BrowseLimits::default()),
        "GET",
        path,
        Some(session()),
    )
    .await;
    assert_eq!(a.code(), ErrorCode::RepositoryNotReady.id());
}
