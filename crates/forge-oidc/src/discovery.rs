//! The provider's discovery document (`<issuer>/.well-known/openid-configuration`,
//! ADR-0021 §2, §11): fetched once and kept for the process. Its `issuer`
//! must equal `auth.oidc.issuer` byte for byte (`issuer_mismatch`), and
//! every endpoint forge calls or sends the browser to must have the
//! issuer's origin (`endpoint_origin`): a tampered or misconfigured
//! document never makes forge send the client secret or a code elsewhere.

use std::sync::Arc;
use std::time::Duration;

use forge_errors::ForgeError;
use serde_json::Value;
use tokio::sync::Mutex;

use crate::error::unavailable;
use crate::http::IdpClient;

/// The endpoints the sign-in uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discovery {
    pub issuer: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub jwks_uri: String,
    /// For the RP logout (MR5).
    pub end_session_endpoint: Option<String>,
    /// The provider's `Date` header when the document was fetched (doctor).
    pub date: Option<String>,
}

fn origin(url: &str) -> Option<String> {
    url::Url::parse(url)
        .ok()
        .map(|u| u.origin().ascii_serialization())
        .filter(|o| o != "null")
}

impl Discovery {
    /// Read and check a discovery document for `issuer`.
    pub fn parse(issuer: &str, body: &[u8], date: Option<String>) -> Result<Self, ForgeError> {
        const CALL: &str = "discovery";
        let doc: Value = serde_json::from_slice(body)
            .map_err(|_| unavailable(CALL, "invalid_json").with_context("detail", "json"))?;
        let field = |name: &'static str| -> Result<String, ForgeError> {
            doc.get(name)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .ok_or_else(|| unavailable(CALL, "invalid_json").with_context("detail", name))
        };
        let stated = field("issuer")?;
        if stated != issuer {
            return Err(unavailable(CALL, "issuer_mismatch"));
        }
        let expected = origin(issuer)
            .ok_or_else(|| unavailable(CALL, "issuer_mismatch").with_context("detail", "origin"))?;
        let same_origin = |name: &'static str, url: &str| -> Result<(), ForgeError> {
            if origin(url).as_deref() == Some(expected.as_str()) {
                Ok(())
            } else {
                Err(unavailable(CALL, "endpoint_origin").with_context("endpoint", name))
            }
        };
        let authorization_endpoint = field("authorization_endpoint")?;
        let token_endpoint = field("token_endpoint")?;
        let jwks_uri = field("jwks_uri")?;
        same_origin("authorization_endpoint", &authorization_endpoint)?;
        same_origin("token_endpoint", &token_endpoint)?;
        same_origin("jwks_uri", &jwks_uri)?;
        let end_session_endpoint = doc
            .get("end_session_endpoint")
            .and_then(Value::as_str)
            .map(str::to_owned);
        if let Some(url) = &end_session_endpoint {
            same_origin("end_session_endpoint", url)?;
        }
        Ok(Self {
            issuer: stated,
            authorization_endpoint,
            token_endpoint,
            jwks_uri,
            end_session_endpoint,
            date,
        })
    }
}

/// The document, fetched on first use and kept; concurrent first uses wait
/// for one fetch (the lock is held across it). A failed fetch is not
/// cached: the next sign-in tries once more.
#[derive(Debug, Default)]
pub(crate) struct DiscoveryCache {
    doc: Mutex<Option<Arc<Discovery>>>,
}

impl DiscoveryCache {
    pub(crate) async fn get(
        &self,
        http: &IdpClient,
        issuer: &str,
        deadline: Duration,
    ) -> Result<Arc<Discovery>, ForgeError> {
        let mut held = self.doc.lock().await;
        if let Some(doc) = held.as_ref() {
            return Ok(Arc::clone(doc));
        }
        let doc = Arc::new(fetch(http, issuer, deadline).await?);
        *held = Some(Arc::clone(&doc));
        Ok(doc)
    }
}

/// One fetch of the document (no cache).
pub(crate) async fn fetch(
    http: &IdpClient,
    issuer: &str,
    deadline: Duration,
) -> Result<Discovery, ForgeError> {
    let url = format!(
        "{}/.well-known/openid-configuration",
        issuer.trim_end_matches('/')
    );
    let reply = http.get("discovery", &url, deadline).await?;
    if reply.status != 200 {
        return Err(unavailable("discovery", "status").with_context("status", reply.status));
    }
    Discovery::parse(issuer, &reply.body, reply.date)
}
