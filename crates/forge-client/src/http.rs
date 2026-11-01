//! The HTTP client: base URL, bearer token, timeouts, error mapping, paging.
//!
//! Safety rules, each with a test:
//! - the token is sent only to the configured host: a path that would leave it
//!   (`//other`, `http://other/..`) is a usage error before any request;
//! - redirects are not followed (a redirect would carry the token elsewhere);
//! - the response body is read up to a limit;
//! - every request has a connect and a total timeout.

use crate::config::Config;
use crate::error::{ClientError, Result};
use crate::store::Token;
use forge_errors::ErrorCode;
use serde_json::Value;
use std::io::Read;
use url::Url;

pub(crate) const MAX_BODY_BYTES: u64 = 16 * 1024 * 1024;
/// A cursor loop that never ends (a broken server) stops here.
const MAX_PAGES: usize = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
    Put,
    Patch,
    Delete,
}

impl Method {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_uppercase().as_str() {
            "GET" => Some(Self::Get),
            "POST" => Some(Self::Post),
            "PUT" => Some(Self::Put),
            "PATCH" => Some(Self::Patch),
            "DELETE" => Some(Self::Delete),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Patch => "PATCH",
            Self::Delete => "DELETE",
        }
    }

    fn to_reqwest(self) -> reqwest::Method {
        match self {
            Self::Get => reqwest::Method::GET,
            Self::Post => reqwest::Method::POST,
            Self::Put => reqwest::Method::PUT,
            Self::Patch => reqwest::Method::PATCH,
            Self::Delete => reqwest::Method::DELETE,
        }
    }
}

#[derive(Debug)]
pub struct ApiResponse {
    pub status: u16,
    pub body: Value,
    pub request_id: Option<String>,
    /// Size of the response body on the wire.
    pub bytes: usize,
}

/// Bounds of `--paginate`: a hostile or broken server cannot grow memory
/// without limit.
#[derive(Debug, Clone, Copy)]
pub struct PageLimits {
    pub max_pages: usize,
    pub max_items: usize,
    pub max_bytes: usize,
}

impl Default for PageLimits {
    fn default() -> Self {
        Self {
            max_pages: MAX_PAGES,
            max_items: 200_000,
            max_bytes: 64 * 1024 * 1024,
        }
    }
}

pub struct ApiClient {
    http: reqwest::blocking::Client,
    base: Url,
    token: Option<Token>,
}

impl ApiClient {
    pub fn new(cfg: &Config, token: Option<Token>) -> Result<Self> {
        let mut b = reqwest::blocking::Client::builder()
            .timeout(cfg.timeout)
            .connect_timeout(cfg.connect_timeout)
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("gstn/", env!("CARGO_PKG_VERSION")));
        if let Some(path) = &cfg.ca_file {
            let pem = std::fs::read(path).map_err(|_| {
                ClientError::local(
                    ErrorCode::CliConfigInvalid,
                    format!(
                        "config_key=ca_file reason=unreadable path={}",
                        path.display()
                    ),
                )
            })?;
            let certs = reqwest::Certificate::from_pem_bundle(&pem).map_err(|_| {
                ClientError::local(
                    ErrorCode::CliConfigInvalid,
                    format!("config_key=ca_file reason=invalid path={}", path.display()),
                )
            })?;
            for c in certs {
                b = b.add_root_certificate(c);
            }
        }
        let http = b.build().map_err(|e| {
            ClientError::local(
                ErrorCode::CliConfigInvalid,
                format!("reason=client_build ({})", kind(&e)),
            )
        })?;
        Ok(Self {
            http,
            base: cfg.host.clone(),
            token,
        })
    }

    /// Resolve `path` against the configured host and refuse anything else.
    pub fn url_for(&self, path: &str, query: &[(String, String)]) -> Result<Url> {
        let usage = |why: &str| {
            ClientError::local(
                ErrorCode::CliUsageInvalid,
                format!("reason=path_invalid ({why})"),
            )
        };
        if !path.starts_with('/')
            || path.starts_with("//")
            || path.contains("://")
            || path.contains('\\')
        {
            return Err(usage(
                "a path on the configured host starting with a single /",
            ));
        }
        if path.chars().any(|c| c.is_control() || c == ' ') {
            return Err(usage("no spaces or control characters"));
        }
        let mut url = self
            .base
            .join(path)
            .map_err(|_| usage("not a valid path"))?;
        if url.origin() != self.base.origin() {
            return Err(usage("it leaves the configured host"));
        }
        if !query.is_empty() {
            let mut q = url.query_pairs_mut();
            for (k, v) in query {
                q.append_pair(k, v);
            }
        }
        Ok(url)
    }

    pub fn request(
        &self,
        method: Method,
        path: &str,
        query: &[(String, String)],
        body: Option<&Value>,
    ) -> Result<ApiResponse> {
        let url = self.url_for(path, query)?;
        let mut req = self
            .http
            .request(method.to_reqwest(), url)
            .header(reqwest::header::ACCEPT, "application/json");
        if let Some(t) = &self.token {
            req = req.bearer_auth(t.expose());
        }
        if let Some(b) = body {
            req = req.json(b);
        }
        let res = req.send().map_err(unreachable)?;
        let status = res.status().as_u16();
        let header_id = res
            .headers()
            .get("x-request-id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let mut bytes = Vec::new();
        res.take(MAX_BODY_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| {
                ClientError::local(
                    ErrorCode::CliServerUnreachable,
                    format!(
                        "reason={}",
                        if e.kind() == std::io::ErrorKind::TimedOut {
                            "timeout"
                        } else {
                            "other"
                        }
                    ),
                )
            })?;
        if bytes.len() as u64 > MAX_BODY_BYTES {
            return Err(invalid(status, "body_too_large"));
        }
        let parsed: std::result::Result<Value, _> = if bytes.is_empty() {
            Ok(Value::Null)
        } else {
            serde_json::from_slice(&bytes)
        };
        if (200..300).contains(&status) {
            let body = parsed.map_err(|_| invalid(status, "not_json"))?;
            return Ok(ApiResponse {
                status,
                body,
                request_id: header_id,
                bytes: bytes.len(),
            });
        }
        if (300..400).contains(&status) {
            return Err(invalid(status, "unexpected_redirect"));
        }
        // An error: the API v1 error body, or something else (a proxy page).
        let v = parsed.map_err(|_| invalid(status, "error_body_not_json"))?;
        let code = v.get("error_code").and_then(Value::as_str);
        let msg = v.get("user_message").and_then(Value::as_str);
        match (code, msg) {
            (Some(code), Some(msg)) => {
                let rid = v
                    .get("request_id")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or(header_id);
                Err(ClientError::from_server(status, code, msg, rid.as_deref()))
            }
            _ => Err(invalid(status, "error_body_unrecognized")),
        }
    }

    /// GET every page of a list route and merge the items. The result has the
    /// list shape of API v1 (`items`, `next_cursor: null`, `has_more: false`).
    pub fn get_all_pages(&self, path: &str, query: &[(String, String)]) -> Result<ApiResponse> {
        self.get_all_pages_limited(path, query, PageLimits::default())
    }

    pub fn get_all_pages_limited(
        &self,
        path: &str,
        query: &[(String, String)],
        limits: PageLimits,
    ) -> Result<ApiResponse> {
        let mut items: Vec<Value> = Vec::new();
        let mut cursor: Option<String> = None;
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut total_bytes = 0usize;
        for _ in 0..limits.max_pages {
            let mut q: Vec<(String, String)> = query
                .iter()
                .filter(|(k, _)| k != "cursor")
                .cloned()
                .collect();
            if let Some(c) = &cursor {
                q.push(("cursor".to_string(), c.clone()));
            }
            let page = self.request(Method::Get, path, &q, None)?;
            let Some(arr) = page.body.get("items").and_then(Value::as_array) else {
                return Err(invalid(page.status, "not_a_list_response"));
            };
            total_bytes = total_bytes.saturating_add(page.bytes);
            if total_bytes > limits.max_bytes {
                return Err(invalid(page.status, "paginate_bytes_limit"));
            }
            if items.len().saturating_add(arr.len()) > limits.max_items {
                return Err(invalid(page.status, "paginate_items_limit"));
            }
            items.extend(arr.iter().cloned());
            match page.body.get("next_cursor") {
                Some(Value::String(next)) => {
                    if !seen.insert(next.clone()) {
                        return Err(invalid(page.status, "cursor_repeated"));
                    }
                    cursor = Some(next.clone());
                }
                Some(Value::Null) | None => {
                    return Ok(ApiResponse {
                        status: page.status,
                        body: serde_json::json!({"items": items, "next_cursor": null, "has_more": false}),
                        request_id: page.request_id,
                        bytes: total_bytes,
                    })
                }
                Some(_) => return Err(invalid(page.status, "next_cursor_malformed")),
            }
        }
        Err(invalid(200, "too_many_pages"))
    }
}

fn kind(e: &reqwest::Error) -> &'static str {
    if e.is_timeout() {
        "timeout"
    } else if e.is_connect() {
        "connect"
    } else {
        "other"
    }
}

fn unreachable(e: reqwest::Error) -> ClientError {
    // reqwest's Display can include the URL with its query; only the class
    // goes into the message. A TLS failure shows up as a connect error whose
    // source chain mentions the certificate.
    let mut chain = String::new();
    let mut src: Option<&dyn std::error::Error> = Some(&e);
    while let Some(s) = src {
        chain.push_str(&s.to_string().to_ascii_lowercase());
        chain.push(' ');
        src = s.source();
    }
    let reason =
        if chain.contains("certificate") || chain.contains("tls") || chain.contains("handshake") {
            "tls"
        } else {
            kind(&e)
        };
    ClientError::local(ErrorCode::CliServerUnreachable, format!("reason={reason}"))
}

fn invalid(status: u16, reason: &str) -> ClientError {
    ClientError::local(
        ErrorCode::CliResponseInvalid,
        format!("reason={reason} http_status={status}"),
    )
}
