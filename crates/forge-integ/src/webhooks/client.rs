//! The one HTTP client of webhook deliveries (ADR-0016 §8): the URL is
//! checked against the target policy of the moment, its name resolved
//! now (within the connect budget) and every address checked; the
//! connection is pinned to the checked address (reqwest's resolve
//! override: the name is never looked up again), TLS is verified against
//! the name (SNI) with `webhook.ca_file`'s roots only. No proxy from the
//! environment, no redirect followed (a 3xx is a failure), 3 s to connect,
//! 10 s in all, at most 4 KiB of the answer read; nothing of the body
//! leaves this module but [`response_head`]'s bounded, redacted head.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Instant;

use forge_core::webhook::{
    response_head, TargetHost, TargetPolicy, WebhookUrl, CONNECT_TIMEOUT, MAX_RESPONSE_BYTES,
    TOTAL_TIMEOUT,
};
use forge_errors::{ErrorCode, ForgeError};

/// How one POST ended.
#[derive(Debug)]
pub enum Sent {
    /// The receiver answered (any status; a 2xx is the success).
    Answered {
        status: u16,
        /// [`response_head`] of at most 4 KiB read.
        head: String,
        duration_ms: i32,
    },
    /// Refused before anything was sent: `FG-INT-0010` with `reason`
    /// (the policy of the moment, the resolved addresses).
    Refused(ForgeError),
    /// No answer: `reason` (`dns`, `connect`, `connect_timeout`, `timeout`,
    /// `tls`, `body`, `request`).
    Failed {
        reason: &'static str,
        duration_ms: i32,
    },
}

/// The delivery's transport: the policy and the trusted roots.
#[derive(Clone)]
pub struct Transport {
    policy: TargetPolicy,
    roots: Arc<Vec<reqwest::Certificate>>,
}

impl std::fmt::Debug for Transport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Transport")
            .field("roots", &self.roots.len())
            .finish_non_exhaustive()
    }
}

fn elapsed_ms(started: Instant) -> i32 {
    i32::try_from(started.elapsed().as_millis()).unwrap_or(i32::MAX)
}

impl Transport {
    /// `ca_pem`: `webhook.ca_file`'s content (`None`: no root is trusted,
    /// every https delivery fails TLS). A bundle without a usable
    /// certificate is `FG-SYS-0002 config_key=webhook.ca_file`.
    pub fn new(policy: TargetPolicy, ca_pem: Option<&[u8]>) -> Result<Self, ForgeError> {
        let roots = match ca_pem {
            None => Vec::new(),
            Some(pem) => {
                let invalid = |reason: &'static str| {
                    ForgeError::new(ErrorCode::ConfigInvalid)
                        .with_context("config_key", "webhook.ca_file")
                        .with_context("reason", reason)
                };
                let certs = reqwest::Certificate::from_pem_bundle(pem)
                    .map_err(|e| invalid("invalid_pem").with_source(e))?;
                if certs.is_empty() {
                    return Err(invalid("no_certificate"));
                }
                certs
            }
        };
        Ok(Self {
            policy,
            roots: Arc::new(roots),
        })
    }

    pub fn policy(&self) -> &TargetPolicy {
        &self.policy
    }

    /// The addresses `url`'s name resolves to now (a literal as is).
    async fn resolve(url: &WebhookUrl) -> Result<Vec<IpAddr>, &'static str> {
        let name = match url.host() {
            TargetHost::Ip(ip) => return Ok(vec![*ip]),
            TargetHost::Name(name) => name.clone(), // clone: the lookup owns its host
        };
        let port = url.port();
        match tokio::time::timeout(CONNECT_TIMEOUT, tokio::net::lookup_host((name, port))).await {
            Ok(Ok(addrs)) => Ok(addrs.map(|a| a.ip()).collect()),
            Ok(Err(_)) | Err(_) => Err("dns"),
        }
    }

    /// POST `body` to `raw_url` with `headers` (module docs).
    pub async fn post(
        &self,
        raw_url: &str,
        headers: &[(&'static str, String)],
        body: Vec<u8>,
    ) -> Sent {
        let started = Instant::now();
        let url = match WebhookUrl::parse(raw_url, self.policy.allow_http())
            .and_then(|u| self.policy.check_url(&u).map(|()| u))
        {
            Ok(url) => url,
            Err(e) => return Sent::Refused(e),
        };
        let addrs = match Self::resolve(&url).await {
            Ok(addrs) => addrs,
            Err(reason) => {
                return Sent::Failed {
                    reason,
                    duration_ms: elapsed_ms(started),
                }
            }
        };
        let pinned = match self.policy.check_resolved(&url, &addrs) {
            Ok(ip) => ip,
            Err(e) => return Sent::Refused(e),
        };
        let client = match self.client(&url, pinned) {
            Ok(c) => c,
            Err(_) => {
                return Sent::Failed {
                    reason: "request",
                    duration_ms: elapsed_ms(started),
                }
            }
        };
        let mut request = client
            .post(url.as_str())
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body);
        for (name, value) in headers {
            request = request.header(*name, value.as_str());
        }
        let whole = async {
            let mut response = request.send().await.map_err(|e| classify(&e))?;
            let status = response.status().as_u16();
            let mut read = Vec::new();
            // Read at most MAX_RESPONSE_BYTES; the rest is never read.
            while read.len() < MAX_RESPONSE_BYTES {
                match response.chunk().await {
                    Ok(Some(chunk)) => {
                        let room = MAX_RESPONSE_BYTES - read.len();
                        read.extend_from_slice(
                            chunk.get(..room.min(chunk.len())).unwrap_or(&chunk),
                        );
                    }
                    Ok(None) => break,
                    // The status arrived: a broken body does not undo it.
                    Err(_) => break,
                }
            }
            Ok::<_, &'static str>((status, response_head(&read)))
        };
        match tokio::time::timeout(TOTAL_TIMEOUT, whole).await {
            Ok(Ok((status, head))) => Sent::Answered {
                status,
                head,
                duration_ms: elapsed_ms(started),
            },
            Ok(Err(reason)) => Sent::Failed {
                reason,
                duration_ms: elapsed_ms(started),
            },
            Err(_) => Sent::Failed {
                reason: "timeout",
                duration_ms: elapsed_ms(started),
            },
        }
    }

    /// A client for this one delivery, its connections pinned to `pinned`.
    fn client(&self, url: &WebhookUrl, pinned: IpAddr) -> Result<reqwest::Client, reqwest::Error> {
        let mut builder = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(TOTAL_TIMEOUT)
            .pool_max_idle_per_host(0)
            .http1_only()
            .tls_built_in_root_certs(false)
            .user_agent(concat!("gitstation-webhook/", env!("CARGO_PKG_VERSION")));
        for cert in self.roots.iter() {
            builder = builder.add_root_certificate(cert.clone()); // clone: the builder owns its roots
        }
        if let TargetHost::Name(name) = url.host() {
            builder = builder.resolve(name, SocketAddr::new(pinned, url.port()));
        }
        builder.build()
    }
}

/// A transport error as the reason it means (never its text: it may name
/// the URL with its query).
fn classify(e: &reqwest::Error) -> &'static str {
    if e.is_connect() && e.is_timeout() {
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
    }
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
