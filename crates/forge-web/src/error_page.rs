//! The browser's error page of the sign-in flow (`/-/auth/…`).
//!
//! The browser navigates to `GET /-/auth/login`, `GET /-/auth/callback`
//! (the identity provider's redirect) and `POST /-/auth/logout` (a form);
//! a failure there used to answer the API's JSON body, which the browser
//! showed raw — the error code included (CLAUDE.md §3-3). A failure of a
//! browser **navigation** on those paths is answered as a small HTML page
//! instead: the localized `user_message`, the `request_id` (問い合わせ番号)
//! and the next step (sign in again / home). Everything else stays JSON:
//! a `fetch` (any `Sec-Fetch-Mode` but `navigate`), a client without fetch
//! metadata that does not accept `text/html`, and the back-channel logout
//! (server to server, never a browser).
//!
//! The page is built from what [`crate::error::ApiError`] rendered (the
//! [`ErrorView`] extension), so every failure on those paths — the
//! handler's, the guard's, the CSRF check's, a timeout, a panic — becomes
//! the same page; status, `Set-Cookie`, `Retry-After` and `Cache-Control`
//! stay. It has no script, no style and no subresource, so the auth
//! class's CSP (`default-src 'none'`, `crate::security_headers`) holds.
//! Every text is escaped. The error code never appears; it stays in the
//! log, written once by the error renderer.

use axum::body::Body;
use axum::extract::Request;
use axum::http::header::{HeaderMap, HeaderValue, ACCEPT, CONTENT_LENGTH, CONTENT_TYPE};
use axum::middleware::Next;
use axum::response::Response;
use forge_errors::Lang;

use crate::routes::auth::{BACKCHANNEL_LOGOUT_PATH, LOGIN_PATH};

/// What a failure showed its user, as a response extension set by the
/// error renderer (never sent).
#[derive(Debug, Clone)]
pub(crate) struct ErrorView {
    pub(crate) user_message: String,
    pub(crate) request_id: String,
    pub(crate) lang: Lang,
}

/// The paths whose browser navigations get the page.
fn page_path(path: &str) -> bool {
    path.starts_with("/-/auth/") && path != BACKCHANNEL_LOGOUT_PATH
}

/// A browser navigation: `Sec-Fetch-Mode: navigate`; without fetch
/// metadata (an older browser), an `Accept` that names `text/html`.
pub(crate) fn browser_navigation(headers: &HeaderMap) -> bool {
    match headers.get(crate::csrf::SEC_FETCH_MODE) {
        Some(mode) => mode.as_bytes() == b"navigate",
        None => headers.get_all(ACCEPT).iter().any(|v| {
            v.to_str().is_ok_and(|accept| {
                accept.split(',').any(|range| {
                    range
                        .split(';')
                        .next()
                        .unwrap_or_default()
                        .trim()
                        .eq_ignore_ascii_case("text/html")
                })
            })
        }),
    }
}

/// Middleware (`app::build`, inside the security headers): a failed
/// browser navigation on a sign-in path becomes the page.
pub(crate) async fn auth_error_page(req: Request, next: Next) -> Response {
    let page = page_path(req.uri().path()) && browser_navigation(req.headers());
    let res = next.run(req).await;
    if page {
        as_page(res)
    } else {
        res
    }
}

/// The response with its JSON error body replaced by the page; anything
/// that is not a rendered failure is returned as it is.
fn as_page(res: Response) -> Response {
    let Some(view) = res.extensions().get::<ErrorView>().cloned() else {
        return res;
    };
    let (mut parts, _json) = res.into_parts();
    parts.headers.remove(CONTENT_LENGTH);
    parts.headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    Response::from_parts(parts, Body::from(page(&view)))
}

struct Words {
    title: &'static str,
    request_id: &'static str,
    sign_in: &'static str,
    home: &'static str,
}

fn words(lang: Lang) -> Words {
    match lang {
        Lang::Ja => Words {
            title: "処理を完了できませんでした",
            request_id: "問い合わせ番号",
            sign_in: "もう一度サインインする",
            home: "ホームへ戻る",
        },
        Lang::En => Words {
            title: "The request could not be completed",
            request_id: "Request ID",
            sign_in: "Sign in again",
            home: "Back to home",
        },
        Lang::Id => Words {
            title: "Permintaan tidak dapat diselesaikan",
            request_id: "ID permintaan",
            sign_in: "Masuk lagi",
            home: "Kembali ke beranda",
        },
    }
}

/// `text` safe inside HTML text and quoted attribute values.
pub(crate) fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// The page: no script, no style, no subresource.
pub(crate) fn page(view: &ErrorView) -> String {
    let w = words(view.lang);
    format!(
        "<!doctype html>\n<html lang=\"{lang}\">\n<head>\n<meta charset=\"utf-8\">\n\
<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
<title>{title} - GitStation</title>\n</head>\n<body>\n<main>\n<h1>{title}</h1>\n\
<p>{message}</p>\n<p>{label}: <code>{request_id}</code></p>\n\
<ul>\n<li><a href=\"{login}\">{sign_in}</a></li>\n<li><a href=\"/\">{home}</a></li>\n</ul>\n\
</main>\n</body>\n</html>\n",
        lang = view.lang.as_str(),
        title = escape(w.title),
        message = escape(&view.user_message),
        label = escape(w.request_id),
        request_id = escape(&view.request_id),
        login = LOGIN_PATH,
        sign_in = escape(w.sign_in),
        home = escape(w.home),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            if let Ok(v) = HeaderValue::from_str(v) {
                h.append(*k, v);
            }
        }
        h
    }

    #[test]
    fn a_navigation_is_told_by_fetch_metadata_then_accept() {
        for (pairs, want) in [
            (vec![("sec-fetch-mode", "navigate")], true),
            (
                vec![
                    ("sec-fetch-mode", "navigate"),
                    ("accept", "application/json"),
                ],
                true,
            ),
            (vec![("sec-fetch-mode", "cors")], false),
            // The SPA's fetch with fetch metadata is never a page, whatever
            // it accepts.
            (
                vec![("sec-fetch-mode", "cors"), ("accept", "text/html")],
                false,
            ),
            (
                vec![(
                    "accept",
                    "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
                )],
                true,
            ),
            (vec![("accept", "TEXT/HTML; q=0.5")], true),
            (vec![("accept", "application/json")], false),
            (vec![("accept", "*/*")], false),
            (vec![], false),
        ] {
            assert_eq!(browser_navigation(&headers(&pairs)), want, "{pairs:?}");
        }
    }

    #[test]
    fn only_the_browser_sign_in_paths_get_the_page() {
        assert!(page_path("/-/auth/login"));
        assert!(page_path("/-/auth/callback"));
        assert!(page_path("/-/auth/logout"));
        assert!(!page_path(BACKCHANNEL_LOGOUT_PATH));
        assert!(!page_path("/api/v1/user"));
        assert!(!page_path("/acme"));
    }

    #[test]
    fn the_page_escapes_every_text_and_shows_no_error_code() {
        let view = ErrorView {
            user_message: "<script>alert('x')</script> & \"q\"".to_owned(),
            request_id: "req-<b>".to_owned(),
            lang: Lang::Ja,
        };
        let html = page(&view);
        assert!(!html.contains("<script"), "{html}");
        assert!(!html.contains("<b>"), "{html}");
        assert!(
            html.contains("&lt;script&gt;alert(&#39;x&#39;)&lt;/script&gt; &amp; &quot;q&quot;")
        );
        assert!(html.contains("問い合わせ番号: <code>req-&lt;b&gt;</code>"));
        assert!(html.contains("<html lang=\"ja\">"));
        assert!(html.contains("href=\"/-/auth/login\""));
        assert!(html.contains("href=\"/\""));
        assert!(!html.contains("FG-"));
        // CSP `default-src 'none'`: nothing inline, nothing loaded.
        for banned in ["<style", "style=", "<link", "<img", " on", "javascript:"] {
            assert!(!html.contains(banned), "{banned}: {html}");
        }
    }

    #[test]
    fn the_page_speaks_the_request_language() {
        for (lang, label) in [
            (Lang::Ja, "問い合わせ番号"),
            (Lang::En, "Request ID"),
            (Lang::Id, "ID permintaan"),
        ] {
            let html = page(&ErrorView {
                user_message: "m".to_owned(),
                request_id: "r".to_owned(),
                lang,
            });
            assert!(html.contains(&format!("<html lang=\"{}\">", lang.as_str())));
            assert!(html.contains(label), "{html}");
        }
    }
}
