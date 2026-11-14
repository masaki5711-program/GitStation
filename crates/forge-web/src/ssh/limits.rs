//! Connection admission of the SSH transport (M4 C13, ADR-0032 §4): a cap
//! in total and per peer, new connections per peer per minute, and a
//! minute's lockout of a peer whose connections keep failing to
//! authenticate. A refused connection is closed before the SSH handshake
//! (`FG-GIT-0035`); refusals are logged at most once per
//! [`REFUSAL_LOG_INTERVAL`] with the count they stand for.
//!
//! Peers are keyed like the HTTP server's (`server::peer_key`: an IPv6
//! peer counts as its /64).

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use forge_core::{telemetry, RequestId};
use forge_errors::{ErrorCode, ForgeError};

/// The window of the per-minute counts.
pub const WINDOW: Duration = Duration::from_secs(60);
/// At most one refusal line per this interval.
pub const REFUSAL_LOG_INTERVAL: Duration = Duration::from_secs(10);
/// Past this many tracked peers, idle entries are dropped on the next
/// admission (bounds the table under a scan).
const SWEEP_AT: usize = 4096;

/// The bounds (`git.ssh.*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdmissionLimits {
    pub max_connections: usize,
    pub max_connections_per_peer: usize,
    pub connections_per_minute_per_peer: u32,
    pub auth_failures_per_minute_per_peer: u32,
}

#[derive(Debug, Default)]
struct PeerState {
    open: usize,
    window_start: Option<Instant>,
    opened: u32,
    failures: u32,
}

impl PeerState {
    /// Start a new window when the old one is over.
    fn roll(&mut self, now: Instant) {
        if self
            .window_start
            .is_none_or(|start| now.saturating_duration_since(start) >= WINDOW)
        {
            self.window_start = Some(now);
            self.opened = 0;
            self.failures = 0;
        }
    }

    fn idle(&self, now: Instant) -> bool {
        self.open == 0
            && self
                .window_start
                .is_none_or(|start| now.saturating_duration_since(start) >= WINDOW)
    }
}

#[derive(Debug, Default)]
struct Table {
    open: usize,
    peers: HashMap<IpAddr, PeerState>,
}

/// Why a connection was refused (the log's fields).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Refusal {
    pub reason: &'static str,
    pub limit: u64,
    pub open: u64,
}

impl Refusal {
    pub fn error(&self, peer: IpAddr, suppressed: u64) -> ForgeError {
        ForgeError::new(ErrorCode::SshConnectionRefused)
            .with_context("reason", self.reason)
            .with_context("peer", peer)
            .with_context("limit", self.limit)
            .with_context("open", self.open)
            .with_context("suppressed", suppressed)
    }
}

/// The admission table, shared by every connection.
#[derive(Debug)]
pub struct Admission {
    limits: AdmissionLimits,
    table: Mutex<Table>,
    log: Mutex<RefusalLog>,
}

/// One admitted connection; released on drop.
#[derive(Debug)]
pub struct Admitted {
    admission: Arc<Admission>,
    peer: IpAddr,
}

impl Drop for Admitted {
    fn drop(&mut self) {
        let mut table = self.admission.lock();
        table.open = table.open.saturating_sub(1);
        let now = Instant::now();
        if let Some(state) = table.peers.get_mut(&self.peer) {
            state.open = state.open.saturating_sub(1);
            if state.idle(now) {
                table.peers.remove(&self.peer);
            }
        }
    }
}

impl Admitted {
    /// The connection ended without a successful authentication: one
    /// failure for its peer.
    pub fn auth_failed(&self) {
        let mut table = self.admission.lock();
        let state = table.peers.entry(self.peer).or_default();
        state.roll(Instant::now());
        state.failures = state.failures.saturating_add(1);
    }
}

impl Admission {
    pub fn new(limits: AdmissionLimits) -> Arc<Self> {
        Arc::new(Self {
            limits,
            table: Mutex::new(Table::default()),
            log: Mutex::new(RefusalLog::default()),
        })
    }

    /// The critical sections are map updates: a poisoned lock holds
    /// consistent data and is used as is.
    fn lock(&self) -> std::sync::MutexGuard<'_, Table> {
        self.table.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Admit one connection of `peer` (a peer key) at `now`.
    pub fn admit(self: &Arc<Self>, peer: IpAddr, now: Instant) -> Result<Admitted, Refusal> {
        let l = self.limits;
        let mut table = self.lock();
        if table.peers.len() >= SWEEP_AT {
            table.peers.retain(|_, s| !s.idle(now));
        }
        if table.open >= l.max_connections {
            return Err(Refusal {
                reason: "connection_limit",
                limit: l.max_connections as u64,
                open: table.open as u64,
            });
        }
        let state = table.peers.entry(peer).or_default();
        state.roll(now);
        if state.failures >= l.auth_failures_per_minute_per_peer {
            return Err(Refusal {
                reason: "auth_failures",
                limit: u64::from(l.auth_failures_per_minute_per_peer),
                open: state.open as u64,
            });
        }
        if state.opened >= l.connections_per_minute_per_peer {
            return Err(Refusal {
                reason: "rate_limit",
                limit: u64::from(l.connections_per_minute_per_peer),
                open: state.open as u64,
            });
        }
        if state.open >= l.max_connections_per_peer {
            return Err(Refusal {
                reason: "peer_limit",
                limit: l.max_connections_per_peer as u64,
                open: state.open as u64,
            });
        }
        state.open += 1;
        state.opened = state.opened.saturating_add(1);
        table.open += 1;
        Ok(Admitted {
            admission: Arc::clone(self),
            peer,
        })
    }

    /// Log a refusal unless one was logged within the interval (then it is
    /// counted into the next line).
    pub fn log_refusal(&self, refusal: &Refusal, peer: IpAddr, now: Instant) {
        let due = self
            .log
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .due(now);
        if let Some(suppressed) = due {
            telemetry::log_failure(&refusal.error(peer, suppressed), &RequestId::generate());
        }
    }

    /// Open connections now (doctor / tests).
    pub fn open(&self) -> usize {
        self.lock().open
    }
}

#[derive(Debug, Default)]
struct RefusalLog {
    last: Option<Instant>,
    suppressed: u64,
}

impl RefusalLog {
    /// `Some(suppressed)` when a line is due now.
    fn due(&mut self, now: Instant) -> Option<u64> {
        match self.last {
            Some(last) if now.saturating_duration_since(last) < REFUSAL_LOG_INTERVAL => {
                self.suppressed += 1;
                None
            }
            _ => {
                self.last = Some(now);
                Some(std::mem::take(&mut self.suppressed))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    const LIMITS: AdmissionLimits = AdmissionLimits {
        max_connections: 4,
        max_connections_per_peer: 2,
        connections_per_minute_per_peer: 5,
        auth_failures_per_minute_per_peer: 3,
    };

    fn ip(last: u8) -> IpAddr {
        IpAddr::from([10, 0, 0, last])
    }

    /// The errors.toml test of FG-GIT-0035: every bound refuses with its
    /// reason, and a released connection frees its slot.
    #[test]
    fn ssh_connections_over_the_cap_are_refused() {
        let a = Admission::new(LIMITS);
        let t0 = Instant::now();
        let first = a.admit(ip(1), t0).unwrap();
        let second = a.admit(ip(1), t0).unwrap();
        let peer = a.admit(ip(1), t0).unwrap_err();
        assert_eq!(peer.reason, "peer_limit");
        assert_eq!(peer.error(ip(1), 0).code(), ErrorCode::SshConnectionRefused);
        let _c = a.admit(ip(2), t0).unwrap();
        let _d = a.admit(ip(3), t0).unwrap();
        assert_eq!(a.open(), 4);
        assert_eq!(a.admit(ip(4), t0).unwrap_err().reason, "connection_limit");
        drop(first);
        assert_eq!(a.open(), 3);
        let _again = a.admit(ip(4), t0).unwrap();
        drop(second);
        // Peer 1 opened 2 (and was refused once): 3 more fit the minute.
        let b = Admission::new(LIMITS);
        for _ in 0..5 {
            drop(b.admit(ip(9), t0).unwrap());
        }
        assert_eq!(b.admit(ip(9), t0).unwrap_err().reason, "rate_limit");
        // The next minute starts afresh.
        drop(b.admit(ip(9), t0 + WINDOW).unwrap());
    }

    #[test]
    fn a_peer_failing_authentication_is_locked_out_for_the_minute() {
        let a = Admission::new(LIMITS);
        let t0 = Instant::now();
        for _ in 0..3 {
            let c = a.admit(ip(7), t0).unwrap();
            c.auth_failed();
        }
        let refused = a.admit(ip(7), t0 + Duration::from_secs(30)).unwrap_err();
        assert_eq!(refused.reason, "auth_failures");
        // Another peer is not affected.
        drop(a.admit(ip(8), t0).unwrap());
        // After the window: admitted again.
        drop(
            a.admit(ip(7), t0 + WINDOW + Duration::from_secs(1))
                .unwrap(),
        );
    }

    #[test]
    fn idle_peers_leave_the_table() {
        let a = Admission::new(LIMITS);
        let Some(t0) = Instant::now().checked_sub(WINDOW + Duration::from_secs(1)) else {
            return; // a clock this young cannot express the past window
        };
        drop(a.admit(ip(5), t0).unwrap());
        assert!(
            a.lock().peers.is_empty(),
            "an idle peer past its window is dropped"
        );
    }

    #[test]
    fn refusal_lines_are_throttled_and_count_what_they_stand_for() {
        let mut log = RefusalLog::default();
        let t0 = Instant::now();
        assert_eq!(log.due(t0), Some(0));
        assert_eq!(log.due(t0 + Duration::from_secs(1)), None);
        assert_eq!(log.due(t0 + Duration::from_secs(2)), None);
        assert_eq!(log.due(t0 + REFUSAL_LOG_INTERVAL), Some(2));
    }
}
