//! [`Wake`]: when subscribers should look for work (ADR-0016 §7) — a
//! `NOTIFY forge_events_sequenced` from the sequencer, or at the latest
//! after the poll interval. LISTEN runs on its own connection
//! ([`Db::listen_options`](crate::Db), ADR-0018 §2): a one-connection pool
//! without lifetime or idle limits, used only by the listener.
//!
//! Notifications sent while the connection is down are lost, so every
//! (re)connect reports [`Woken::Connected`] and the caller checks the state
//! at once. A dead link that fires no error (half-open TCP) is found by a
//! bounded `SELECT 1` on the listener at every poll; the server drops a
//! dead client by its TCP keepalive.
//!
//! `wait` is meant to be awaited to completion; dropping it mid-wait (a
//! worker abort) drops the listener with it, which is harmless.

use std::time::Duration;

use forge_errors::ForgeError;
use sqlx::postgres::{PgListener, PgPoolOptions};

use crate::error_map::{connect_error, unavailable};
use crate::sequencer::CHANNEL;
use crate::{query_error, Db};

/// Why [`Wake::wait`] returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Woken {
    /// The sequencer numbered events.
    Notified,
    /// The poll interval passed without a notification.
    Polled,
    /// The LISTEN connection was (re)established: notifications may have
    /// been missed, look now.
    Connected,
}

#[derive(Default)]
pub struct Wake {
    listener: Option<PgListener>,
}

impl std::fmt::Debug for Wake {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Wake")
            .field("listening", &self.listener.is_some())
            .finish()
    }
}

impl Wake {
    pub fn new() -> Self {
        Self::default()
    }

    /// Wait at most `max` for a notification. Connecting (first call, or
    /// after a failure) returns [`Woken::Connected`] at once. A failure
    /// drops the connection (the next call reconnects) and is returned:
    /// `FG-DB-0001` for the link, `FG-DB-0002` for a refused statement.
    pub async fn wait(&mut self, db: &Db, max: Duration) -> Result<Woken, ForgeError> {
        let Some(listener) = self.listener.as_mut() else {
            self.listener = Some(connect(db).await?);
            return Ok(Woken::Connected);
        };
        let round_trip = db.round_trip_timeout();
        let woken = match tokio::time::timeout(max, listener.try_recv()).await {
            Ok(Ok(Some(_))) => {
                // One look covers every notification already received.
                while listener.next_buffered().is_some() {}
                Ok(Woken::Notified)
            }
            // The connection was lost and re-established (LISTEN included).
            Ok(Ok(None)) => Ok(Woken::Connected),
            Ok(Err(e)) => Err(query_error("subscribers.listen_recv", e)),
            Err(_) => {
                let ping = sqlx::query("SELECT 1").execute(&mut *listener);
                match tokio::time::timeout(round_trip, ping).await {
                    Ok(Ok(_)) => Ok(Woken::Polled),
                    Ok(Err(e)) => Err(query_error("subscribers.listen_ping", e)),
                    Err(_) => Err(unavailable(db.target(), "listen_ping_timeout")
                        .with_context("timeout_ms", round_trip.as_millis())
                        .with_context("timeout_key", "database.acquire_timeout_ms")),
                }
            }
        };
        if woken.is_err() {
            self.listener = None;
        }
        woken
    }
}

async fn connect(db: &Db) -> Result<PgListener, ForgeError> {
    let round_trip = db.round_trip_timeout();
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .min_connections(0)
        .acquire_timeout(round_trip)
        .idle_timeout(None)
        .max_lifetime(None)
        // clone: the listener's own pool keeps its options
        .connect_lazy_with(db.listen_options().clone());
    let listen = async {
        let mut listener = PgListener::connect_with(&pool)
            .await
            .map_err(|e| connect_error(db.target(), e))?;
        listener
            .listen(CHANNEL)
            .await
            .map_err(|e| query_error("subscribers.listen", e))?;
        Ok(listener)
    };
    match tokio::time::timeout(round_trip.saturating_mul(2), listen).await {
        Ok(result) => result,
        Err(_) => Err(unavailable(db.target(), "listen_timeout")
            .with_context("timeout_ms", round_trip.saturating_mul(2).as_millis())
            .with_context("timeout_key", "database.acquire_timeout_ms")),
    }
}
