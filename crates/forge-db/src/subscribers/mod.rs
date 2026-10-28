//! Internal event subscribers (ADR-0016 §7). A [`Subscriber`] reads the
//! published events (`forge.events` with a `seq`) in order through its own
//! cursor (`forge.event_subscribers.last_seq`), one event per transaction:
//!
//! 1. `SELECT … FOR UPDATE NOWAIT` on its row — `55P03` means another worker
//!    processes it right now: a normal path ([`Progress::Busy`], debug log,
//!    no FG code);
//! 2. the event `last_seq + 1` goes to [`Subscriber::handle`] with a
//!    [`DerivedTx`] (derived-table writes and derived events only);
//! 3. `last_seq` / `last_txid` advance in the same transaction
//!    (`… WHERE name = $1 AND last_seq = $prev`, monotonic) — the
//!    derived-event trigger checks exactly that (migration 20261027140000).
//!
//! A failed handler is rolled back (savepoint) and counted (so are an
//! undecodable event, a failed savepoint RELEASE and derived writes refused
//! at COMMIT — see `run`). An `ordered` subscriber stops after
//! [`FAILURES_BEFORE_STOP`] failures in a row (`FG-DB-0008 reason=stuck`)
//! and keeps retrying the same event — it never skips by itself
//! (`forge admin subscriber skip` is not yet implemented; its design is
//! step 4 of `ops/runbooks/subscriber-lag.md`). An `independent` one
//! quarantines the event into `forge.subscriber_dead_letters` and moves on
//! (`reason=quarantined`). Deriving more than [`MAX_DERIVED_PER_ROOT`]
//! events from one root, or deriving from an event at [`MAX_DEPTH`], is
//! `reason=fanout_limit` at once (stop / quarantine).
//!
//! [`run_once`] is one such step; [`Dispatcher`] runs every registered
//! subscriber for `forge worker` and waits on `wake::Wake` (NOTIFY
//! `forge_events_sequenced` on the dedicated LISTEN connection, or the
//! 5-second poll). [`states`] / [`dead_letters`] feed `forge doctor`
//! (`subscribers.lag`, `subscribers.dead_letters`).
//!
//! Logs of a subscriber step carry the source event's `request_id`, `seq`
//! and `event_id` ([`Incident`]); the caller (the worker) logs them once.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use forge_core::ids::ProjectId;
use forge_core::RequestId;
use forge_errors::{ErrorCode, ForgeError};

use crate::{query_error, Db};

mod count;
mod cursor;
mod derived;
mod dispatch;
mod lag;
mod run;
mod wake;

pub use derived::{DerivedTx, Limit};
pub use dispatch::{Dispatcher, Round, BATCH_PER_ROUND, POLL, ROUND_BUDGET};
pub use lag::{check_lag, dead_letters, states, DeadLetters, SubscriberState, BEHIND_AFTER};
pub use run::{run_once, Incident, Progress};

/// An `ordered` subscriber stops (and an `independent` one quarantines the
/// event) after this many failures in a row on one event.
pub const FAILURES_BEFORE_STOP: i32 = 5;
/// Derived events per root event (ADR-0016 §7; the trigger enforces it too).
pub const MAX_DERIVED_PER_ROOT: i64 = 100;
/// The deepest causation depth (the events CHECK and the trigger).
pub const MAX_DEPTH: i16 = 8;

/// How a subscriber treats an event it cannot process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ordering {
    /// Order matters (e.g. `work-item-sync`): stop and alert.
    Ordered,
    /// Each event stands alone (e.g. `notify`, `webhook-dispatch`):
    /// quarantine it and go on.
    Independent,
}

impl Ordering {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ordered => "ordered",
            Self::Independent => "independent",
        }
    }

    pub(crate) fn parse(text: &str) -> Option<Self> {
        match text {
            "ordered" => Some(Self::Ordered),
            "independent" => Some(Self::Independent),
            _ => None,
        }
    }
}

/// A published event as a subscriber sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct SourceEvent {
    pub seq: i64,
    pub event_id: uuid::Uuid,
    pub event_type: String,
    pub schema_version: i16,
    pub aggregate_type: String,
    pub aggregate_id: i64,
    pub project_id: Option<ProjectId>,
    pub actor_kind: String,
    pub actor_user_id: Option<i64>,
    /// The request that wrote the event (logs of this step carry it).
    pub request_id: String,
    /// The root of the derivation chain (`None` for a plain event).
    pub root_seq: Option<i64>,
    pub causation_depth: i16,
    pub payload: serde_json::Value,
}

impl SourceEvent {
    /// The request id for logs (`None` if the stored one is not log-safe).
    pub fn request_id(&self) -> Option<RequestId> {
        RequestId::parse(&self.request_id)
    }
}

/// What [`Subscriber::handle`] returns.
pub type HandleFuture<'a> = Pin<Box<dyn Future<Output = Result<(), ForgeError>> + Send + 'a>>;

/// An internal subscriber. `handle` runs inside the step's transaction: it
/// writes derived tables through [`DerivedTx::conn`] and derived events
/// through [`DerivedTx::emit`]; returning an error rolls both back. It must
/// be idempotent per event only in the sense that a failed attempt leaves
/// nothing behind (external calls belong in jobs, never in `handle`).
/// Match event types exhaustively (ADR-0016 §5) — no `_` arm.
///
/// **Trust boundary**: [`DerivedTx::conn`] is the step's raw connection
/// with forge_app's privileges. forge-db does not check what a handler
/// writes through it beyond the database's own guards (append-only
/// triggers, the derived-event checks at COMMIT): it must write derived
/// tables only — never audited state or forge.events directly — and never
/// issue transaction control (COMMIT, ROLLBACK, SAVEPOINT). A handler that
/// leaves its savepoint aborted (a swallowed SQL error) is counted as a
/// failure. Review every handler against this.
pub trait Subscriber: Send + Sync + 'static {
    /// `^[a-z][a-z0-9-]{0,62}$`, unique; the row in forge.event_subscribers.
    fn name(&self) -> &'static str;
    fn ordering(&self) -> Ordering;
    fn handle<'a>(&'a self, event: &'a SourceEvent, tx: &'a mut DerivedTx<'_>) -> HandleFuture<'a>;
}

/// The subscribers a worker runs.
#[derive(Clone, Default)]
pub struct Registry {
    subscribers: Vec<Arc<dyn Subscriber>>,
}

impl std::fmt::Debug for Registry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list()
            .entries(self.subscribers.iter().map(|s| s.name()))
            .finish()
    }
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add one; a bad or duplicate name is a programming error
    /// (`FG-SYS-0001 reason=invalid_subscriber_name / duplicate_subscriber`).
    pub fn with(mut self, subscriber: Arc<dyn Subscriber>) -> Result<Self, ForgeError> {
        let name = subscriber.name();
        let invalid = |reason: &'static str| {
            ForgeError::new(ErrorCode::Internal)
                .with_context("operation", "subscribers.registry")
                .with_context("reason", reason)
                .with_context("subscriber", name)
        };
        if !valid_name(name) {
            return Err(invalid("invalid_subscriber_name"));
        }
        if self.subscribers.iter().any(|s| s.name() == name) {
            return Err(invalid("duplicate_subscriber"));
        }
        self.subscribers.push(subscriber);
        Ok(self)
    }

    pub fn is_empty(&self) -> bool {
        self.subscribers.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Arc<dyn Subscriber>> {
        self.subscribers.iter()
    }
}

fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    name.len() <= 63
        && chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Where a newly registered subscriber starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Start {
    /// At the current `high_seq`: only events published from now on
    /// (ADR-0016 §1 — the default).
    Head,
    /// From the first event (only when asked for explicitly).
    Beginning,
}

/// Create the subscriber's row if it does not exist (an existing row keeps
/// its cursor). A row whose `ordering` differs from the code's is a
/// programming error (`FG-SYS-0001 reason=ordering_mismatch`): forge_app
/// cannot change it, and guessing which one is right could skip events.
pub async fn register(
    db: &Db,
    subscriber: &dyn Subscriber,
    start: Start,
) -> Result<(), ForgeError> {
    let name = subscriber.name();
    let ordering = subscriber.ordering();
    db.within("subscribers.register", async {
        let mut conn = db
            .write_pool()
            .acquire()
            .await
            .map_err(|e| query_error("subscribers.acquire", e))?;
        sqlx::query(
            "INSERT INTO forge.event_subscribers (name, ordering, last_seq, updated_at) \
             SELECT $1, $2, CASE WHEN $3 THEN coalesce((SELECT f.high_seq \
                 FROM forge.event_feed_state AS f WHERE f.singleton), 0) ELSE 0 END, \
                 pg_catalog.now() \
             ON CONFLICT (name) DO NOTHING",
        )
        .bind(name)
        .bind(ordering.as_str())
        .bind(start == Start::Head)
        .execute(&mut *conn)
        .await
        .map_err(|e| query_error("subscribers.register", e))?;
        let stored: String =
            sqlx::query_scalar("SELECT ordering FROM forge.event_subscribers WHERE name = $1")
                .bind(name)
                .fetch_one(&mut *conn)
                .await
                .map_err(|e| query_error("subscribers.register_check", e))?;
        if Ordering::parse(&stored) != Some(ordering) {
            return Err(ForgeError::new(ErrorCode::Internal)
                .with_context("operation", "subscribers.register")
                .with_context("reason", "ordering_mismatch")
                .with_context("subscriber", name)
                .with_context("stored", stored)
                .with_context("expected", ordering.as_str()));
        }
        Ok(())
    })
    .await
}

#[cfg(test)]
mod derived_tests;
#[cfg(test)]
mod doctor_tests;
#[cfg(test)]
mod poison_tests;
#[cfg(test)]
pub(crate) mod tests;
#[cfg(test)]
mod wake_tests;

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn subscriber_names_are_lowercase_slugs() {
        assert!(valid_name("webhook-dispatch"));
        assert!(valid_name("a1"));
        assert!(!valid_name(""));
        assert!(!valid_name("1st"));
        assert!(!valid_name("Notify"));
        assert!(!valid_name("work_item"));
        assert!(!valid_name(&"a".repeat(64)));
        assert_eq!(Ordering::parse("ordered"), Some(Ordering::Ordered));
        assert_eq!(Ordering::parse("other"), None);
    }
}
