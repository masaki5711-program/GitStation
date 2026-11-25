//! Kanflow's GPU gate (plan 19 §1.1, Kanflow `remote-gate.ts`): `GET
//! {llm.kanflow_busy_url}` with `x-internal-token` answers
//! `{"busyHigh": <n>, "idleForMs": <ms|null>}`. A user waits when
//! `busyHigh > 0` or the last user call ended less than [`QUIET`] ago.
//! No answer, another status or another body is [`Kanflow::Unreachable`]
//! (treated as idle, throttled: never blocks forge).

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use forge_core::config::Secret;
use forge_errors::ForgeError;
use serde_json::Value;

use crate::ollama::{client, read_body};
use crate::sched::Kanflow;

/// How long after a user's call Kanflow still counts as busy.
pub const QUIET: Duration = Duration::from_secs(5);

/// The boxed future of [`BusySource::poll`].
pub type BusyFuture<'a> = Pin<Box<dyn Future<Output = Kanflow> + Send + 'a>>;

/// Where the gateway learns whether a Kanflow user waits.
pub trait BusySource: Send + Sync {
    fn poll(&self) -> BusyFuture<'_>;
}

/// No Kanflow gate configured: always idle.
#[derive(Debug, Default)]
pub struct NoGate;

impl BusySource for NoGate {
    fn poll(&self) -> BusyFuture<'_> {
        Box::pin(async { Kanflow::Idle })
    }
}

/// The HTTP gate.
pub struct KanflowGate {
    client: reqwest::Client,
    url: String,
    token: Secret,
}

impl std::fmt::Debug for KanflowGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KanflowGate")
            .field("url", &self.url)
            .finish_non_exhaustive()
    }
}

impl KanflowGate {
    pub fn new(
        url: &str,
        token: Secret,
        ca_pem: Option<&[u8]>,
        timeout: Duration,
    ) -> Result<Self, ForgeError> {
        Ok(Self {
            client: client(ca_pem, timeout)?,
            url: url.to_owned(),
            token,
        })
    }
}

/// Read Kanflow's answer.
pub fn parse_busy(body: &[u8]) -> Kanflow {
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        return Kanflow::Unreachable;
    };
    let Some(high) = value.get("busyHigh").and_then(Value::as_u64) else {
        return Kanflow::Unreachable;
    };
    let quiet_ms = u64::try_from(QUIET.as_millis()).unwrap_or(u64::MAX);
    let recent = value
        .get("idleForMs")
        .and_then(Value::as_f64)
        .is_some_and(|ms| ms >= 0.0 && ms < quiet_ms as f64);
    if high > 0 || recent {
        Kanflow::Busy
    } else {
        Kanflow::Idle
    }
}

impl KanflowGate {
    /// Ask Kanflow once. `Err` when no answer came (the error's `reason`:
    /// `connect`, `timeout`, `status_<code>`, …; never its text);
    /// `Ok(Kanflow::Unreachable)` when the answer was not understood.
    /// `forge doctor`'s `llm.kanflow_gate` shows the reason; the gateway
    /// treats both as unreachable.
    pub async fn probe(&self) -> Result<Kanflow, ForgeError> {
        let sent = self
            .client
            .get(&self.url)
            .header("x-internal-token", self.token.expose())
            .send()
            .await;
        read_body(sent, 64 * 1024)
            .await
            .map(|body| parse_busy(&body))
    }
}

impl BusySource for KanflowGate {
    fn poll(&self) -> BusyFuture<'_> {
        Box::pin(async move { self.probe().await.unwrap_or(Kanflow::Unreachable) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn busy_when_a_user_waits_or_just_did() {
        assert_eq!(
            parse_busy(br#"{"busyHigh": 1, "idleForMs": null}"#),
            Kanflow::Busy
        );
        assert_eq!(
            parse_busy(br#"{"busyHigh": 0, "idleForMs": 1200}"#),
            Kanflow::Busy
        );
        assert_eq!(
            parse_busy(br#"{"busyHigh": 0, "idleForMs": 60000}"#),
            Kanflow::Idle
        );
        assert_eq!(
            parse_busy(br#"{"busyHigh": 0, "idleForMs": null}"#),
            Kanflow::Idle
        );
        assert_eq!(
            parse_busy(br#"{"error": "forbidden"}"#),
            Kanflow::Unreachable
        );
        assert_eq!(parse_busy(b"<html>"), Kanflow::Unreachable);
    }
}
