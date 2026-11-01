//! The JWKS cache (ADR-0021 §11): the set is kept 10 minutes; a token
//! naming a `kid` the fresh set does not have triggers one refetch, at most
//! once per 60 seconds — within that minute an unknown `kid` is
//! `FG-AUTH-0012 unknown_kid` without a call, so a flood of forged tokens
//! never turns into a flood of requests to the identity provider.

use std::sync::Arc;
use std::time::Duration;

use forge_errors::ForgeError;
use tokio::sync::Mutex;
use tokio::time::Instant;

use crate::error::unavailable;
use crate::http::IdpClient;
use crate::keys::KeySet;

/// The cache's timing (the defaults are ADR-0021's).
#[derive(Debug, Clone, Copy)]
pub struct JwksTiming {
    /// How long a fetched set is used (10 minutes).
    pub ttl: Duration,
    /// The shortest time between two refetches for an unknown `kid` (60 s).
    pub refetch_every: Duration,
}

impl Default for JwksTiming {
    fn default() -> Self {
        Self {
            ttl: Duration::from_secs(600),
            refetch_every: Duration::from_secs(60),
        }
    }
}

#[derive(Debug, Default)]
struct Held {
    keys: Option<(Arc<KeySet>, Instant)>,
    last_refetch: Option<Instant>,
}

#[derive(Debug)]
pub(crate) struct JwksCache {
    held: Mutex<Held>,
    timing: JwksTiming,
}

impl JwksCache {
    pub(crate) fn new(timing: JwksTiming) -> Self {
        Self {
            held: Mutex::new(Held::default()),
            timing,
        }
    }

    /// A set that holds `kid` if the provider has it: the cached set when
    /// fresh and holding it, otherwise a fetch (expired or absent set, or an
    /// unknown `kid` and no refetch in the last minute). The returned set
    /// may still lack `kid` (the verifier answers `unknown_kid`).
    pub(crate) async fn for_kid(
        &self,
        http: &IdpClient,
        jwks_uri: &str,
        kid: &str,
        deadline: Duration,
    ) -> Result<Arc<KeySet>, ForgeError> {
        let mut held = self.held.lock().await;
        let now = Instant::now();
        if let Some((keys, at)) = &held.keys {
            let fresh = now.duration_since(*at) < self.timing.ttl;
            if fresh && keys.get(kid).is_some() {
                return Ok(Arc::clone(keys));
            }
            if fresh {
                let recently = held
                    .last_refetch
                    .is_some_and(|t| now.duration_since(t) < self.timing.refetch_every);
                if recently {
                    return Ok(Arc::clone(keys));
                }
                held.last_refetch = Some(now);
            }
        }
        let keys = Arc::new(fetch(http, jwks_uri, deadline).await?);
        held.keys = Some((Arc::clone(&keys), Instant::now()));
        Ok(keys)
    }
}

/// One fetch of the set.
pub(crate) async fn fetch(
    http: &IdpClient,
    jwks_uri: &str,
    deadline: Duration,
) -> Result<KeySet, ForgeError> {
    let reply = http.get("jwks", jwks_uri, deadline).await?;
    if reply.status != 200 {
        return Err(unavailable("jwks", "status").with_context("status", reply.status));
    }
    KeySet::parse(&reply.body)
        .map_err(|detail| unavailable("jwks", "invalid_json").with_context("detail", detail))
}
