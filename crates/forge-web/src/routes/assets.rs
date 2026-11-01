//! `GET|HEAD /-/assets/{file}` (`WebShell`, public; ADR-0015 Amendment M2,
//! ADR-0022 §2): the embedded UI's files, by exact name. A name the build
//! does not have is `FG-WEB-0001` (`reason=unknown_route`, `part=instance`),
//! never the shell. Names carry a content hash, so an answer is cached for
//! a year (`immutable`). The br / gzip variants were made at build time
//! (`web/scripts/compress.mjs`); nothing is compressed at run time.

use std::borrow::Cow;

use axum::body::Body;
use axum::http::header::{
    HeaderValue, ACCEPT_ENCODING, CACHE_CONTROL, CONTENT_ENCODING, CONTENT_TYPE, VARY,
};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use forge_auth::InstanceCtx;
use forge_errors::{ErrorCode, ForgeError};

use crate::app::AppState;
use crate::registry::Dispatched;
use crate::security_headers::ResponseClass;
use crate::ui::UiStatus;

pub const CACHE_IMMUTABLE: &str = "public, max-age=31536000, immutable";

/// One embedded file, or `FG-WEB-0001`.
pub async fn asset(_state: AppState, request: Dispatched<InstanceCtx>) -> Response {
    // The parser admits exactly one segment for `assets`.
    let found = request
        .tail()
        .iter()
        .next()
        .and_then(|name| crate::ui::asset(name));
    let Some(found) = found else {
        return request
            .ctx()
            .fail(
                ForgeError::new(ErrorCode::RouteNotFound)
                    .with_context("reason", "unknown_route")
                    .with_context("part", "instance")
                    .with_context("rule", "unknown_asset")
                    .with_context("ui", ui_kind()),
            )
            .into_response();
    };
    let (bytes, encoding): (Cow<'static, [u8]>, Option<&'static str>) =
        match (accepts(request.headers()), found.br, found.gzip) {
            (Accepts { br: true, .. }, Some(br), _) => (br, Some("br")),
            (Accepts { gzip: true, .. }, _, Some(gz)) => (gz, Some("gzip")),
            _ => (found.identity, None),
        };
    let mut res = Body::from(bytes).into_response();
    let headers = res.headers_mut();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static(found.content_type));
    headers.insert(CACHE_CONTROL, HeaderValue::from_static(CACHE_IMMUTABLE));
    headers.insert(VARY, HeaderValue::from_static("Accept-Encoding"));
    if let Some(encoding) = encoding {
        headers.insert(CONTENT_ENCODING, HeaderValue::from_static(encoding));
    }
    res.extensions_mut().insert(ResponseClass::Asset);
    res
}

/// For the log of an unknown asset: a placeholder build has none at all.
fn ui_kind() -> &'static str {
    match crate::ui::status() {
        UiStatus::Placeholder => "placeholder",
        UiStatus::Embedded { .. } => "embedded",
    }
}

/// What `Accept-Encoding` allows of the two precompressed forms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Accepts {
    br: bool,
    gzip: bool,
}

/// RFC 9110 §12.5.3, as far as two codings need: a coding is acceptable
/// when listed (or `*` is) with a `q` above 0; an explicit entry wins over
/// `*`. Unparsable `q` values count as 0 (never guess a coding).
fn accepts(headers: &HeaderMap) -> Accepts {
    let mut br: Option<bool> = None;
    let mut gzip: Option<bool> = None;
    let mut any: Option<bool> = None;
    for value in headers.get_all(ACCEPT_ENCODING) {
        let Ok(value) = value.to_str() else {
            continue;
        };
        for item in value.split(',') {
            let mut parts = item.split(';');
            let coding = parts.next().unwrap_or("").trim().to_ascii_lowercase();
            let mut ok = true;
            for param in parts {
                if let Some((k, v)) = param.split_once('=') {
                    if k.trim().eq_ignore_ascii_case("q") {
                        ok = v.trim().parse::<f32>().is_ok_and(|q| q > 0.0);
                    }
                }
            }
            match coding.as_str() {
                "br" => br = Some(ok),
                "gzip" | "x-gzip" => gzip = Some(ok),
                "*" => any = Some(ok),
                _ => {}
            }
        }
    }
    Accepts {
        br: br.or(any).unwrap_or(false),
        gzip: gzip.or(any).unwrap_or(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn of(values: &[&str]) -> Accepts {
        let mut h = HeaderMap::new();
        for v in values {
            h.append(ACCEPT_ENCODING, HeaderValue::from_str(v).expect("header"));
        }
        accepts(&h)
    }

    #[test]
    fn accept_encoding_picks_what_the_client_allows() {
        assert_eq!(of(&[]), Accepts::default());
        assert_eq!(
            of(&["gzip, deflate, br, zstd"]),
            Accepts {
                br: true,
                gzip: true
            }
        );
        assert_eq!(
            of(&["gzip"]),
            Accepts {
                br: false,
                gzip: true
            }
        );
        assert_eq!(
            of(&["BR;q=0.5"]),
            Accepts {
                br: true,
                gzip: false
            }
        );
        assert_eq!(
            of(&["br;q=0, gzip"]),
            Accepts {
                br: false,
                gzip: true
            }
        );
        assert_eq!(
            of(&["*"]),
            Accepts {
                br: true,
                gzip: true
            }
        );
        assert_eq!(
            of(&["*, br;q=0"]),
            Accepts {
                br: false,
                gzip: true
            }
        );
        assert_eq!(of(&["identity"]), Accepts::default());
        assert_eq!(of(&["br;q=x"]), Accepts::default());
        assert_eq!(
            of(&["br", "gzip"]),
            Accepts {
                br: true,
                gzip: true
            }
        );
    }
}
