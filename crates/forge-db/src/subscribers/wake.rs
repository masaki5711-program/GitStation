//! [`Wake`]: when subscribers should look for work (ADR-0016 §7) — a
//! `NOTIFY forge_events_sequenced` from the sequencer, or at the latest
//! after the poll interval.
//!
//! Since plan 18 S2 this is a waiter of the process's wake hub
//! ([`crate::wake::WakeHub`], its `Events` channel): the hub holds the one
//! LISTEN connection of the process, reconnects it and logs its failures
//! (`FG-DB-0001` / `FG-DB-0002` with `operation=wake_listen`). On a [`Db`]
//! without a hub (tests, one-shot commands) the first wait starts a hub of
//! its own for this channel; it ends when the `Wake` is dropped.
//!
//! Notifications sent while the connection is down are lost, so every
//! (re)connect reports [`Woken::Connected`] and the caller checks the state
//! at once. While the connection is down a wait ends at the poll interval
//! ([`Woken::Polled`]): the subscribers keep working, only slower.

use std::time::Duration;

use forge_errors::ForgeError;

use crate::wake::{self, Channel, WakeHub, WakeRx};
use crate::Db;

/// Why [`Wake::wait`] returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Woken {
    /// The sequencer numbered events.
    Notified,
    /// The poll interval passed without a notification.
    Polled,
    /// The LISTEN connection was (re)established: notifications may have
    /// been missed, look now.
    Connected,
}

/// The subscription, once the first wait made it.
#[derive(Debug)]
struct Listening {
    rx: WakeRx,
    /// The hub started for this `Wake` alone (no hub on the `Db`): kept so
    /// that it lives as long as the subscription.
    _own: Option<WakeHub>,
}

#[derive(Debug, Default)]
pub(crate) struct Wake {
    listening: Option<Listening>,
}

impl Wake {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Wait at most `max` for a notification. The first call subscribes
    /// and waits for the hub's connection — at most the connect bound,
    /// then [`Woken::Connected`]; with the database down it ends as
    /// [`Woken::Polled`] (the hub logs the failure and keeps trying).
    /// `Err` (`FG-SYS-0001`): no hub could be started.
    pub(crate) async fn wait(&mut self, db: &Db, max: Duration) -> Result<Woken, ForgeError> {
        let (listening, max) = match &mut self.listening {
            Some(listening) => (listening, max),
            None => {
                let (hub, own) = match db.wake() {
                    // clone: a handle on the shared hub
                    Some(hub) if hub.listens(Channel::Events) => (hub.clone(), None),
                    _ => {
                        let hub = WakeHub::spawn(db, &[Channel::Events])?;
                        (hub.clone(), Some(hub)) // clone: the handle kept below
                    }
                };
                let listening = self.listening.insert(Listening {
                    rx: hub.subscribe_unseen(Channel::Events),
                    _own: own,
                });
                let connect = db.round_trip_timeout().saturating_mul(2);
                (listening, max.max(connect))
            }
        };
        Ok(match listening.rx.wait(max).await {
            wake::Woken::Notified(_) => Woken::Notified,
            wake::Woken::SafetyNet => Woken::Polled,
            wake::Woken::Reconnected => Woken::Connected,
        })
    }
}
