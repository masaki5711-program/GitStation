//! The Ollama provider (plan 19 §2): `POST {endpoint}/api/chat` (one
//! answer, no streaming, `format: "json"`, the configured `num_ctx`) and
//! `GET {endpoint}/api/tags` (the model's digest).
//!
//! Transport: rustls with `llm.ca_file`'s roots only (no public roots), no
//! proxy, no redirect, `llm.timeout_ms` per call (connect at most 3 s), at
//! most [`MAX_BODY_BYTES`] read. The endpoint was checked to be loopback or
//! an allowed in-house host when the configuration was loaded.
//!
//! Every failure is `FG-LLM-0001` with `reason` (`connect`,
//! `connect_timeout`, `timeout`, `redirect`, `request`, `status_<code>`,
//! `body_too_large`, `body_invalid`); the provider's error text is never
//! passed on (it may quote the prompt).

use std::time::Duration;

use forge_core::llm::{ChatFuture, ChatReply, ChatRequest, LlmProvider};
use forge_errors::{ErrorCode, ForgeError};
use serde_json::{json, Value};

use crate::DigestFuture;

/// The largest answer read.
pub const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;

/// An HTTP client with the gateway's rules (module docs).
pub(crate) fn client(
    ca_pem: Option<&[u8]>,
    timeout: Duration,
) -> Result<reqwest::Client, ForgeError> {
    let invalid = |reason: &'static str| {
        ForgeError::new(ErrorCode::ConfigInvalid)
            .with_context("config_key", "llm.ca_file")
            .with_context("reason", reason)
    };
    let mut builder = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(timeout.min(Duration::from_secs(3)))
        .timeout(timeout)
        .tls_built_in_root_certs(false)
        .user_agent(concat!("gitstation/", env!("CARGO_PKG_VERSION")));
    if let Some(pem) = ca_pem {
        let certs = reqwest::Certificate::from_pem_bundle(pem)
            .map_err(|e| invalid("invalid_pem").with_source(e))?;
        if certs.is_empty() {
            return Err(invalid("no_certificate"));
        }
        for cert in certs {
            builder = builder.add_root_certificate(cert);
        }
    }
    builder.build().map_err(|e| {
        ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "http_client")
            .with_source(e)
    })
}

/// A transport error as the reason it means (never its text).
pub(crate) fn classify(e: &reqwest::Error) -> &'static str {
    if e.is_connect() && e.is_timeout() {
        "connect_timeout"
    } else if e.is_timeout() {
        "timeout"
    } else if e.is_connect() {
        "connect"
    } else if e.is_redirect() {
        "redirect"
    } else {
        "request"
    }
}

pub(crate) fn unavailable(reason: impl std::fmt::Display) -> ForgeError {
    ForgeError::new(ErrorCode::LlmUnavailable).with_context("reason", reason)
}

/// Read at most `max` bytes of a 2xx body.
pub(crate) async fn read_body(
    sent: Result<reqwest::Response, reqwest::Error>,
    max: usize,
) -> Result<Vec<u8>, ForgeError> {
    let mut response = sent.map_err(|e| unavailable(classify(&e)))?;
    let status = response.status();
    if !status.is_success() {
        return Err(unavailable(format_args!("status_{}", status.as_u16())));
    }
    let mut body = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                if body.len() + chunk.len() > max {
                    return Err(unavailable("body_too_large"));
                }
                body.extend_from_slice(&chunk);
            }
            Ok(None) => return Ok(body),
            Err(e) => return Err(unavailable(classify(&e))),
        }
    }
}

/// The Ollama HTTP API.
pub struct OllamaProvider {
    client: reqwest::Client,
    endpoint: String,
}

impl std::fmt::Debug for OllamaProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OllamaProvider")
            .field("endpoint", &self.endpoint)
            .finish_non_exhaustive()
    }
}

impl OllamaProvider {
    /// `endpoint`: the checked origin (`llm.endpoint`).
    pub fn new(
        endpoint: &str,
        ca_pem: Option<&[u8]>,
        timeout: Duration,
    ) -> Result<Self, ForgeError> {
        Ok(Self {
            client: client(ca_pem, timeout)?,
            endpoint: endpoint.trim_end_matches('/').to_owned(),
        })
    }

    async fn chat_once(&self, request: &ChatRequest) -> Result<ChatReply, ForgeError> {
        let body = json!({
            "model": request.model,
            "stream": false,
            "format": "json",
            "options": {"num_ctx": request.num_ctx},
            "messages": [
                {"role": "system", "content": request.system},
                {"role": "user", "content": request.user},
            ],
        });
        let sent = self
            .client
            .post(format!("{}/api/chat", self.endpoint))
            .header("content-type", "application/json")
            .body(body.to_string())
            .send()
            .await;
        let bytes = read_body(sent, MAX_BODY_BYTES).await?;
        let value: Value =
            serde_json::from_slice(&bytes).map_err(|_| unavailable("body_invalid"))?;
        let content = value
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(Value::as_str)
            .ok_or_else(|| unavailable("body_invalid"))?;
        let tokens = |key: &str| {
            value
                .get(key)
                .and_then(Value::as_u64)
                .map_or(0, |n| u32::try_from(n).unwrap_or(u32::MAX))
        };
        Ok(ChatReply {
            content: content.to_owned(),
            input_tokens: tokens("prompt_eval_count"),
            output_tokens: tokens("eval_count"),
        })
    }

    async fn digest_once(&self, model: &str) -> Result<Option<String>, ForgeError> {
        let sent = self
            .client
            .get(format!("{}/api/tags", self.endpoint))
            .send()
            .await;
        let bytes = read_body(sent, MAX_BODY_BYTES).await?;
        let value: Value =
            serde_json::from_slice(&bytes).map_err(|_| unavailable("body_invalid"))?;
        let models = value
            .get("models")
            .and_then(Value::as_array)
            .ok_or_else(|| unavailable("body_invalid"))?;
        Ok(models
            .iter()
            .find(|m| {
                m.get("name").and_then(Value::as_str) == Some(model)
                    || m.get("model").and_then(Value::as_str) == Some(model)
            })
            .and_then(|m| m.get("digest"))
            .and_then(Value::as_str)
            .map(str::to_owned))
    }
}

impl LlmProvider for OllamaProvider {
    fn chat<'a>(&'a self, request: &'a ChatRequest) -> ChatFuture<'a> {
        Box::pin(self.chat_once(request))
    }
}

impl crate::ModelCatalog for OllamaProvider {
    fn digest<'a>(&'a self, model: &'a str) -> DigestFuture<'a> {
        Box::pin(self.digest_once(model))
    }
}
