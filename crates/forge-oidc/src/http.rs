//! The one HTTP client to the identity provider (ADR-0021 §2, §11): no
//! environment proxy, no redirects (a 3xx is `reason=redirect`), only the
//! roots of `auth.oidc.ca_file` (no public CA), a 2 s connect timeout, a
//! per-call deadline around send *and* body, and at most 1 MiB of body.
//! Every failure is `FG-AUTH-0015` with `call` and `reason`; a response
//! body never enters an error or a log.

use std::time::Duration;

use forge_errors::ForgeError;

use crate::error::{internal, unavailable};

/// The largest response body read from the identity provider.
pub(crate) const MAX_BODY: usize = 1024 * 1024;

/// A response that arrived whole.
pub(crate) struct Reply {
    pub status: u16,
    pub body: Vec<u8>,
    /// The `Date` header (doctor's clock skew), if any.
    pub date: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct IdpClient {
    client: reqwest::Client,
}

impl IdpClient {
    /// Build the client; `ca_pem` is the content of `auth.oidc.ca_file`
    /// (`None` for an `http` loopback issuer). A CA file without a usable
    /// certificate or a client that cannot be built is `FG-SYS-0002`.
    pub(crate) fn new(ca_pem: Option<&[u8]>, connect: Duration) -> Result<Self, ForgeError> {
        let mut builder = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(connect)
            .tls_built_in_root_certs(false)
            .user_agent(concat!("gitstation/", env!("CARGO_PKG_VERSION")));
        if let Some(pem) = ca_pem {
            let certs = reqwest::Certificate::from_pem_bundle(pem).map_err(|e| {
                forge_errors::ForgeError::new(forge_errors::ErrorCode::ConfigInvalid)
                    .with_context("config_key", "auth.oidc.ca_file")
                    .with_context("reason", "invalid_pem")
                    .with_source(e)
            })?;
            if certs.is_empty() {
                return Err(
                    forge_errors::ForgeError::new(forge_errors::ErrorCode::ConfigInvalid)
                        .with_context("config_key", "auth.oidc.ca_file")
                        .with_context("reason", "no_certificate"),
                );
            }
            for cert in certs {
                builder = builder.add_root_certificate(cert);
            }
        }
        let client = builder
            .build()
            .map_err(|e| internal("http_client").with_source(e))?;
        Ok(Self { client })
    }

    /// `GET url` within `deadline`.
    pub(crate) async fn get(
        &self,
        call: &'static str,
        url: &str,
        deadline: Duration,
    ) -> Result<Reply, ForgeError> {
        let request = self
            .client
            .get(url)
            .header(reqwest::header::ACCEPT, "application/json");
        send(call, request, deadline).await
    }

    /// `POST url` with a form body and HTTP Basic client authentication
    /// (`client_secret_basic`, RFC 6749 §2.3.1: both parts form-encoded).
    pub(crate) async fn post_form(
        &self,
        call: &'static str,
        url: &str,
        (client_id, secret): (&str, &str),
        form: &[(&str, &str)],
        deadline: Duration,
    ) -> Result<Reply, ForgeError> {
        let enc = |s: &str| url::form_urlencoded::byte_serialize(s.as_bytes()).collect::<String>();
        let request = self
            .client
            .post(url)
            .basic_auth(enc(client_id), Some(enc(secret)))
            .header(reqwest::header::ACCEPT, "application/json")
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .body(form_body(form));
        send(call, request, deadline).await
    }
}

/// `application/x-www-form-urlencoded` of `form` (not held across an
/// await: the serializer is not `Send`).
fn form_body(form: &[(&str, &str)]) -> String {
    let mut body = url::form_urlencoded::Serializer::new(String::new());
    for (k, v) in form {
        body.append_pair(k, v);
    }
    body.finish()
}

async fn send(
    call: &'static str,
    request: reqwest::RequestBuilder,
    deadline: Duration,
) -> Result<Reply, ForgeError> {
    let whole = async {
        let mut response = request.send().await.map_err(|e| classify(call, &e))?;
        let status = response.status().as_u16();
        if (300..400).contains(&status) {
            return Err(unavailable(call, "redirect").with_context("status", status));
        }
        let date = response
            .headers()
            .get(reqwest::header::DATE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        if response
            .content_length()
            .is_some_and(|n| n > MAX_BODY as u64)
        {
            return Err(unavailable(call, "body_too_large"));
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|e| classify(call, &e))? {
            if body.len() + chunk.len() > MAX_BODY {
                return Err(unavailable(call, "body_too_large"));
            }
            body.extend_from_slice(&chunk);
        }
        Ok(Reply { status, body, date })
    };
    match tokio::time::timeout(deadline, whole).await {
        Ok(result) => result,
        Err(_) => {
            Err(unavailable(call, "timeout").with_context("timeout_ms", deadline.as_millis()))
        }
    }
}

/// A transport error as the reason it means (never its text: it may name
/// the URL with a query).
fn classify(call: &'static str, e: &reqwest::Error) -> ForgeError {
    let reason = if e.is_connect() && e.is_timeout() {
        "connect_timeout"
    } else if e.is_timeout() {
        "timeout"
    } else if e.is_connect() && is_tls(e) {
        "tls"
    } else if e.is_connect() {
        "connect"
    } else if e.is_redirect() {
        "redirect"
    } else if e.is_body() || e.is_decode() {
        "body"
    } else {
        "request"
    };
    unavailable(call, reason)
}

/// rustls reports a handshake failure as an `io::Error` of kind
/// `InvalidData` in the source chain.
fn is_tls(e: &reqwest::Error) -> bool {
    let mut source = std::error::Error::source(e);
    while let Some(s) = source {
        if let Some(io) = s.downcast_ref::<std::io::Error>() {
            if io.kind() == std::io::ErrorKind::InvalidData {
                return true;
            }
        }
        source = s.source();
    }
    false
}
