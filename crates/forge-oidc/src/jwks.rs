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
    /// When the last fetch attempt started (the first fetch, a refetch,
    /// a failure): the 60 s limit counts from here.
    last_attempt: Option<Instant>,
    /// When the last attempt ended, and whether it failed.
    last_done: Option<Instant>,
    last_failed: bool,
}

/// What the cache answers without the provider.
enum Decision {
    Use(Arc<KeySet>),
    Fetch,
}

#[derive(Debug)]
pub(crate) struct JwksCache {
    /// Held only to read or write the state, never across a fetch.
    held: Mutex<Held>,
    /// One fetch at a time (single flight): callers that wait here use what
    /// the fetch before them got instead of fetching again.
    gate: Mutex<()>,
    timing: JwksTiming,
}

impl JwksCache {
    pub(crate) fn new(timing: JwksTiming) -> Self {
        Self {
            held: Mutex::new(Held::default()),
            gate: Mutex::new(()),
            timing,
        }
    }

    /// A set that holds `kid` if the provider has it: the cached set when
    /// fresh and holding it, otherwise a fetch (expired or absent set, or an
    /// unknown `kid`) — at most one attempt per `refetch_every` (60 s),
    /// failures included: within that minute an unknown `kid` gets the
    /// cached set (the verifier answers `unknown_kid`) and a missing or
    /// expired set after a failed attempt is `FG-AUTH-0015 call=jwks
    /// reason=backoff`, without a call. The returned set may still lack
    /// `kid`. Concurrent callers share one fetch; no lock is held across it.
    pub(crate) async fn for_kid(
        &self,
        http: &IdpClient,
        jwks_uri: &str,
        kid: &str,
        deadline: Duration,
    ) -> Result<Arc<KeySet>, ForgeError> {
        let asked = Instant::now();
        if let Decision::Use(keys) = self.decide(kid, asked).await? {
            return Ok(keys);
        }
        let _flight = self.gate.lock().await;
        // Another caller attempted while this one waited: take its outcome.
        if let Some(outcome) = self.attempted_since(asked).await {
            return outcome;
        }
        {
            let mut held = self.held.lock().await;
            held.last_attempt = Some(Instant::now());
            // In flight: callers arriving now wait at the gate for it.
            held.last_failed = false;
        }
        let fetched = fetch(http, jwks_uri, deadline).await;
        let mut held = self.held.lock().await;
        let done = Instant::now();
        held.last_done = Some(done);
        held.last_failed = fetched.is_err();
        let keys = Arc::new(fetched?);
        held.keys = Some((Arc::clone(&keys), done));
        Ok(keys)
    }

    async fn decide(&self, kid: &str, now: Instant) -> Result<Decision, ForgeError> {
        let held = self.held.lock().await;
        let fresh = held
            .keys
            .as_ref()
            .filter(|(_, at)| now.duration_since(*at) < self.timing.ttl)
            .map(|(keys, _)| Arc::clone(keys));
        if let Some(keys) = &fresh {
            if keys.get(kid).is_some() {
                return Ok(Decision::Use(Arc::clone(keys)));
            }
        }
        let recently = held
            .last_attempt
            .is_some_and(|t| now.duration_since(t) < self.timing.refetch_every);
        if !recently {
            return Ok(Decision::Fetch);
        }
        match fresh {
            Some(keys) => Ok(Decision::Use(keys)),
            None if held.last_failed => Err(backoff()),
            None => Ok(Decision::Fetch),
        }
    }

    /// The outcome of an attempt that ended after `asked`, if any.
    async fn attempted_since(&self, asked: Instant) -> Option<Result<Arc<KeySet>, ForgeError>> {
        let held = self.held.lock().await;
        if !held.last_done.is_some_and(|t| t >= asked) {
            return None;
        }
        Some(match (&held.keys, held.last_failed) {
            (Some((keys, _)), false) => Ok(Arc::clone(keys)),
            _ => Err(backoff()),
        })
    }
}

/// A fetch failed less than `refetch_every` ago: no new call.
fn backoff() -> ForgeError {
    unavailable("jwks", "backoff")
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
