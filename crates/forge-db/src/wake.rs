//! [`WakeHub`]: ONE LISTEN connection per process, fanned out to every
//! waiter of the process (plan 18 S2, ADR-0037 (a)).
//!
//! | [`Channel`] | NOTIFY channel | payload | sent by |
//! |---|---|---|---|
//! | `Jobs` | `forge_jobs` | the job kind (`ci.job` for a CI job a runner may take) | `jobs::enqueue*` / `release` / `fail` / `reap`, the CI job state changes |
//! | `MergeQueue` | `forge_merge_queue` | the queue id | nobody yet (plan 18 S3) |
//! | `Refs` | `forge_refs` | the repository id | nobody yet (see `forge_git::browse::Browser::invalidate`) |
//! | `Events` | `forge_events_sequenced` | the last event seq | the sequencer |
//!
//! **A wake is a hint, never the truth.** Every waiter reads the database
//! again when it wakes and looks by itself after its safety-net period at
//! the latest, so a lost, duplicated or late notification costs time —
//! never correctness. Payloads carry a kind or an id only (never content,
//! never a secret), at most [`PAYLOAD_MAX`] bytes.
//!
//! **The connection.** The hub's task holds the dedicated LISTEN
//! connection ([`Db::listen_options`](crate::Db), ADR-0018 §2): a
//! one-connection pool of its own, outside the write and read pools — one
//! server connection per process on top of them (the connection budget
//! counts it, `bootstrap::ConnectionBudget`). Connecting is bounded
//! (twice `database.acquire_timeout_ms`); a link that fires no error
//! (half-open TCP) is found by a bounded `SELECT 1` every [`PING_EVERY`].
//! A lost link is reconnected with a backoff from [`RECONNECT_MIN`] to
//! [`RECONNECT_MAX`]. Notifications sent while it is down are lost, so
//! every (re)connect wakes every waiter once ([`Woken::Reconnected`]:
//! "look at once").
//!
//! **A dead listener is visible.** Each failure is logged with its code
//! (`FG-DB-0001` for the link, `FG-DB-0002` for a refused statement) and
//! `operation=wake_listen` — once when it starts, again when the reason
//! changes and every [`RELOG_EVERY`] while it lasts. Meanwhile
//! [`WakeHub::is_live`] is false and waiters fall back to their own poll
//! (the job runners: one second, as before plan 18). A waiter never spins:
//! every wait ends by a notification, a (re)connect, the loss of the link
//! (once) or its own deadline.
//!
//! **Bursts coalesce.** A waiter that was not waiting when notifications
//! arrived gets them all in ONE wake: the hub keeps, per channel, the last
//! sequence number of each payload (at most [`PAYLOADS_MAX`] distinct
//! ones; a waiter further behind is told to look at everything).
//!
//! **Shutdown.** [`WakeHub::stop`] ends the task and waits for it; it also
//! ends when the last [`WakeHub`] is dropped. Waiters of a stopped hub
//! wait out their deadline (their callers watch the shutdown signal).

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use forge_core::{telemetry, RequestId};
use forge_errors::{ErrorCode, ForgeError};
use sqlx::postgres::{PgConnectOptions, PgConnection, PgListener, PgPoolOptions};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::error_map::{connect_error, unavailable};
use crate::{query_error, read_query_error, Db};

/// The longest NOTIFY payload the hub sends (PostgreSQL refuses 8000 bytes
/// and more; a kind or an id is far shorter).
pub const PAYLOAD_MAX: usize = 255;
/// Distinct payloads remembered per channel.
pub const PAYLOADS_MAX: usize = 256;
/// The idle LISTEN connection is checked this often.
pub const PING_EVERY: Duration = Duration::from_secs(5);
/// The first pause before a reconnect.
pub const RECONNECT_MIN: Duration = Duration::from_millis(250);
/// The longest pause between reconnects.
pub const RECONNECT_MAX: Duration = Duration::from_secs(5);
/// A lasting listener failure is logged again this often.
pub const RELOG_EVERY: Duration = Duration::from_secs(60);
/// `FG-DB-0024` is logged at most this often per channel and process.
pub const MISSED_LOG_EVERY: Duration = Duration::from_secs(60);
/// `FG-DB-0023`: the NOTIFY queue is this full (a fraction of its 8 GB).
pub const NOTIFY_QUEUE_ALERT: f64 = 0.5;

const OPERATION: &str = "wake_listen";
/// The `Jobs` payload for a CI job a runner may take (`forge.ci_jobs`, not
/// a kind of `forge.jobs`: no job runner has a handler for it).
pub const CI_JOB: &str = "ci.job";

/// What a waiter waits for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Channel {
    /// `forge_jobs`: a job became claimable (payload: its kind).
    Jobs,
    /// `forge_merge_queue`: a merge queue changed (payload: the queue id).
    MergeQueue,
    /// `forge_refs`: refs of a repository changed (payload: its id).
    Refs,
    /// `forge_events_sequenced`: the sequencer numbered events.
    Events,
}

impl Channel {
    pub const ALL: [Self; 4] = [Self::Jobs, Self::MergeQueue, Self::Refs, Self::Events];

    /// The NOTIFY channel (a plain lowercase identifier under 63 bytes).
    pub const fn name(self) -> &'static str {
        match self {
            Self::Jobs => "forge_jobs",
            Self::MergeQueue => "forge_merge_queue",
            Self::Refs => "forge_refs",
            Self::Events => crate::sequencer::CHANNEL,
        }
    }

    const fn index(self) -> usize {
        match self {
            Self::Jobs => 0,
            Self::MergeQueue => 1,
            Self::Refs => 2,
            Self::Events => 3,
        }
    }

    fn of(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.name() == name)
    }
}

/// Why a wait ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Woken {
    /// Notifications arrived: their distinct payloads, oldest first.
    Notified(Vec<String>),
    /// No notification within the safety-net period — or the listener was
    /// just lost: look by yourself.
    SafetyNet,
    /// The LISTEN connection was (re)established, or the waiter fell too
    /// far behind: notifications may have been missed — look at everything.
    Reconnected,
}

/// The state of one channel as its waiters see it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Signal {
    /// Raised by every (re)connect.
    epoch: u64,
    /// Raised by every notification.
    seq: u64,
    /// The LISTEN connection is up and listens on this channel.
    live: bool,
}

/// The last sequence number of each payload of one channel.
#[derive(Debug, Default)]
struct Payloads {
    seq: u64,
    /// Payloads at or below this were forgotten.
    floor: u64,
    last: BTreeMap<String, u64>,
}

impl Payloads {
    fn push(&mut self, payload: &str) -> u64 {
        self.seq = self.seq.saturating_add(1);
        self.last.insert(payload.to_owned(), self.seq);
        while self.last.len() > PAYLOADS_MAX {
            let Some((oldest, seq)) = self
                .last
                .iter()
                .min_by_key(|(_, seq)| **seq)
                .map(|(payload, seq)| (payload.clone(), *seq))
            else {
                break;
            };
            self.last.remove(&oldest);
            self.floor = self.floor.max(seq);
        }
        self.seq
    }

    /// The payloads after `seen`, oldest first, and the sequence number
    /// they reach; `None` when some were forgotten.
    fn since(&self, seen: u64) -> (u64, Option<Vec<String>>) {
        if seen < self.floor {
            return (self.seq, None);
        }
        let mut newer: Vec<(u64, &String)> = self
            .last
            .iter()
            .filter(|(_, seq)| **seq > seen)
            .map(|(payload, seq)| (*seq, payload))
            .collect();
        newer.sort_unstable();
        let payloads = newer.into_iter().map(|(_, p)| p.clone()).collect();
        (self.seq, Some(payloads))
    }
}

#[derive(Debug)]
struct Slot {
    /// The hub listens on this channel.
    listened: bool,
    signal: watch::Sender<Signal>,
    payloads: Mutex<Payloads>,
}

/// What the hub's task and every [`WakeRx`] share.
#[derive(Debug)]
struct Shared {
    slots: [Slot; 4],
    /// The server process of the LISTEN connection (0: not connected).
    backend_pid: AtomicI32,
    /// When `FG-DB-0024` was last logged, per channel.
    missed: Mutex<[Option<Instant>; 4]>,
}

impl Shared {
    fn new(channels: &[Channel]) -> Self {
        let slot = |channel: Channel| Slot {
            listened: channels.contains(&channel),
            signal: watch::Sender::new(Signal::default()),
            payloads: Mutex::new(Payloads::default()),
        };
        Self {
            slots: Channel::ALL.map(slot),
            backend_pid: AtomicI32::new(0),
            missed: Mutex::new([None; 4]),
        }
    }

    fn slot(&self, channel: Channel) -> &Slot {
        &self.slots[channel.index()]
    }

    fn deliver(&self, channel: Channel, payload: &str) {
        let slot = self.slot(channel);
        let seq = {
            // A poisoned lock still holds a consistent map: every change
            // above is completed before the lock is released.
            let mut payloads = slot.payloads.lock().unwrap_or_else(PoisonError::into_inner);
            payloads.push(payload)
        };
        slot.signal.send_modify(|s| s.seq = s.seq.max(seq));
    }

    fn connected(&self, backend_pid: i32) {
        self.backend_pid.store(backend_pid, Ordering::Relaxed);
        for slot in self.slots.iter().filter(|s| s.listened) {
            slot.signal.send_modify(|s| {
                s.epoch = s.epoch.saturating_add(1);
                s.live = true;
            });
        }
    }

    fn disconnected(&self) {
        self.backend_pid.store(0, Ordering::Relaxed);
        for slot in &self.slots {
            slot.signal
                .send_if_modified(|s| std::mem::replace(&mut s.live, false));
        }
    }
}

/// Marks the hub down when its task ends, however it ends.
struct Down(Arc<Shared>);

impl Drop for Down {
    fn drop(&mut self) {
        self.0.disconnected();
    }
}

#[derive(Debug)]
struct Handle {
    shared: Arc<Shared>,
    stop: watch::Sender<bool>,
    task: Mutex<Option<JoinHandle<()>>>,
}

/// The wake hub of a process. Cloning shares it.
#[derive(Debug, Clone)]
pub struct WakeHub(Arc<Handle>);

/// What the task needs of the [`Db`] (it holds no pool of it).
struct Link {
    options: PgConnectOptions,
    round_trip: Duration,
    target: String,
    channels: Vec<Channel>,
    /// One id for every log line of this hub.
    request_id: RequestId,
}

impl WakeHub {
    /// Start the hub of this process, listening on `channels`. It connects
    /// in the background: a database that is down is logged and retried,
    /// never a reason to fail the start. `Err` (`FG-INTERNAL-0001`): no
    /// channel was given, or there is no tokio runtime.
    pub fn spawn(db: &Db, channels: &[Channel]) -> Result<Self, ForgeError> {
        let refused = |reason: &'static str| {
            ForgeError::new(ErrorCode::Internal)
                .with_context("operation", OPERATION)
                .with_context("reason", reason)
        };
        if channels.is_empty() {
            return Err(refused("no_channels"));
        }
        let runtime = tokio::runtime::Handle::try_current().map_err(|_| refused("no_runtime"))?;
        let shared = Arc::new(Shared::new(channels));
        let (stop, stopped) = watch::channel(false);
        let link = Link {
            options: db.listen_options().clone(), // clone: the task owns its options
            round_trip: db.round_trip_timeout(),
            target: db.target().to_owned(),
            channels: Channel::ALL
                .into_iter()
                .filter(|c| channels.contains(c))
                .collect(),
            request_id: RequestId::generate(),
        };
        let task = runtime.spawn(run(Arc::clone(&shared), link, stopped));
        Ok(Self(Arc::new(Handle {
            shared,
            stop,
            task: Mutex::new(Some(task)),
        })))
    }

    /// A waiter of `channel` that has seen everything up to now.
    pub fn subscribe(&self, channel: Channel) -> WakeRx {
        let rx = self.0.shared.slot(channel).signal.subscribe();
        let seen = *rx.borrow();
        WakeRx {
            shared: Arc::clone(&self.0.shared),
            channel,
            rx,
            seen,
        }
    }

    /// A waiter of `channel` that has seen nothing: its first wait returns
    /// [`Woken::Reconnected`] as soon as the hub is (or gets) connected.
    pub(crate) fn subscribe_unseen(&self, channel: Channel) -> WakeRx {
        let mut rx = self.subscribe(channel);
        rx.seen = Signal::default();
        rx
    }

    /// The hub listens on `channel`.
    pub fn listens(&self, channel: Channel) -> bool {
        self.0.shared.slot(channel).listened
    }

    /// The LISTEN connection is up and listens on `channel`: a wake will
    /// come. False: the waiter must poll by itself.
    pub fn is_live(&self, channel: Channel) -> bool {
        self.0.shared.slot(channel).signal.borrow().live
    }

    /// Wake the waiters of `channel` of THIS process, as a notification
    /// with `payload` would (no database round trip).
    pub fn nudge(&self, channel: Channel, payload: &str) {
        self.0.shared.deliver(channel, payload);
    }

    /// The server process of the LISTEN connection, while connected.
    pub fn backend_pid(&self) -> Option<i32> {
        match self.0.shared.backend_pid.load(Ordering::Relaxed) {
            0 => None,
            pid => Some(pid),
        }
    }

    /// Log `FG-DB-0024` (`error` with the `channel` added): a safety-net
    /// look found work no wake announced. At most once per
    /// [`MISSED_LOG_EVERY`] per channel; whether it was logged.
    pub fn missed(&self, channel: Channel, error: ForgeError, request_id: &RequestId) -> bool {
        {
            let mut last = self
                .0
                .shared
                .missed
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let now = Instant::now();
            if !may_log_missed(last[channel.index()], now) {
                return false;
            }
            last[channel.index()] = Some(now);
        }
        telemetry::log_failure(&error.with_context("channel", channel.name()), request_id);
        true
    }

    /// End the hub's task and wait for it, at most `wait`. `Err`
    /// (`FG-INTERNAL-0001`): the task panicked or did not end in time (it
    /// is aborted). A second call has nothing left to do.
    pub async fn stop(&self, wait: Duration) -> Result<(), ForgeError> {
        self.0.stop.send_replace(true);
        let task = self
            .0
            .task
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        let Some(mut task) = task else {
            return Ok(());
        };
        let failed = |reason: &'static str| {
            ForgeError::new(ErrorCode::Internal)
                .with_context("operation", OPERATION)
                .with_context("reason", reason)
        };
        match tokio::time::timeout(wait, &mut task).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) if e.is_panic() => Err(failed("task_panicked")),
            Ok(Err(_)) => Err(failed("task_cancelled")),
            Err(_) => {
                task.abort();
                Err(failed("stop_timeout").with_context("timeout_ms", wait.as_millis()))
            }
        }
    }
}

/// Whether `FG-DB-0024` may be logged at `now`, last logged at `last`.
pub(crate) fn may_log_missed(last: Option<Instant>, now: Instant) -> bool {
    last.is_none_or(|at| now.saturating_duration_since(at) >= MISSED_LOG_EVERY)
}

/// One waiter of one channel. Not shared: each waiter has its own.
#[derive(Debug)]
pub struct WakeRx {
    shared: Arc<Shared>,
    channel: Channel,
    rx: watch::Receiver<Signal>,
    seen: Signal,
}

impl WakeRx {
    /// Wait at most `safety_net` for a wake of this channel.
    pub async fn wait(&mut self, safety_net: Duration) -> Woken {
        self.wait_matching(safety_net, |_| true).await
    }

    /// [`Self::wait`], ignoring the notifications whose payload `wanted`
    /// refuses. Cancel-safe: a dropped wait loses no wanted wake.
    pub async fn wait_matching(
        &mut self,
        safety_net: Duration,
        wanted: impl Fn(&str) -> bool,
    ) -> Woken {
        let deadline = Instant::now() + safety_net;
        loop {
            let now = *self.rx.borrow_and_update();
            if now.epoch != self.seen.epoch {
                self.seen = now;
                return Woken::Reconnected;
            }
            if now.seq > self.seen.seq {
                let (reached, newer) = {
                    let payloads = self
                        .shared
                        .slot(self.channel)
                        .payloads
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner);
                    payloads.since(self.seen.seq)
                };
                self.seen.seq = self.seen.seq.max(now.seq).max(reached);
                let Some(newer) = newer else {
                    self.seen.live = now.live;
                    return Woken::Reconnected;
                };
                let hits: Vec<String> = newer.into_iter().filter(|p| wanted(p)).collect();
                if !hits.is_empty() {
                    self.seen.live = now.live;
                    return Woken::Notified(hits);
                }
            }
            let lost = self.seen.live && !now.live;
            self.seen.live = now.live;
            if lost {
                return Woken::SafetyNet;
            }
            tokio::select! {
                changed = self.rx.changed() => {
                    if changed.is_err() {
                        // The hub is gone: nothing will wake this waiter.
                        tokio::time::sleep_until(deadline).await;
                        return Woken::SafetyNet;
                    }
                }
                () = tokio::time::sleep_until(deadline) => return Woken::SafetyNet,
            }
        }
    }
}

/// Resolves when the hub is told to stop (or its last handle is dropped).
async fn stopped(stop: &mut watch::Receiver<bool>) {
    loop {
        if *stop.borrow_and_update() {
            return;
        }
        if stop.changed().await.is_err() {
            return;
        }
    }
}

/// Logs a lasting failure once, again when it changes and every
/// [`RELOG_EVERY`].
#[derive(Default)]
struct Relog {
    last: Option<(String, Instant)>,
}

impl Relog {
    fn report(&mut self, error: ForgeError, request_id: &RequestId) {
        let signature = signature(&error);
        let now = Instant::now();
        let repeat = self.last.as_ref().is_some_and(|(last, at)| {
            *last == signature && now.saturating_duration_since(*at) < RELOG_EVERY
        });
        if repeat {
            return;
        }
        self.last = Some((signature, now));
        telemetry::log_failure(&error.with_context("operation", OPERATION), request_id);
    }

    fn recovered(&mut self) -> bool {
        self.last.take().is_some()
    }
}

/// What makes two failures "the same" for [`Relog`].
fn signature(error: &ForgeError) -> String {
    let mut signature = error.code().id().to_owned();
    for (key, value) in error.context() {
        if matches!(*key, "reason" | "query" | "sqlstate") {
            signature.push(' ');
            signature.push_str(value);
        }
    }
    signature
}

/// The hub's task: connect, receive until the link fails, back off, again.
async fn run(shared: Arc<Shared>, link: Link, mut stop: watch::Receiver<bool>) {
    let _down = Down(Arc::clone(&shared));
    let mut relog = Relog::default();
    let mut pause = RECONNECT_MIN;
    loop {
        let attempt = tokio::select! {
            attempt = connect(&link) => attempt,
            () = stopped(&mut stop) => return,
        };
        match attempt {
            Ok((listener, backend_pid)) => {
                let since = Instant::now();
                shared.connected(backend_pid);
                tracing::info!(
                    target: "forge_db",
                    event = "forge.wake.listening",
                    request_id = %link.request_id,
                    db_target = %link.target,
                    backend_pid,
                    recovered = relog.recovered(),
                    channels = link.channels.len(),
                    "wake hub listening"
                );
                let lost = tokio::select! {
                    lost = receive(&shared, &link, listener) => lost,
                    () = stopped(&mut stop) => return,
                };
                shared.disconnected();
                if since.elapsed() >= RECONNECT_MAX {
                    pause = RECONNECT_MIN;
                }
                relog.report(lost, &link.request_id);
            }
            Err(e) => relog.report(e, &link.request_id),
        }
        tokio::select! {
            () = tokio::time::sleep(pause) => {}
            () = stopped(&mut stop) => return,
        }
        pause = pause.saturating_mul(2).min(RECONNECT_MAX);
    }
}

/// Open the LISTEN connection, bounded; with the server process behind it.
async fn connect(link: &Link) -> Result<(PgListener, i32), ForgeError> {
    let round_trip = link.round_trip;
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .min_connections(0)
        .acquire_timeout(round_trip)
        .idle_timeout(None)
        .max_lifetime(None)
        // clone: the listener's own pool keeps its options
        .connect_lazy_with(link.options.clone());
    let listen = async {
        let mut listener = PgListener::connect_with(&pool)
            .await
            .map_err(|e| connect_error(&link.target, e))?;
        // A lost link is reported (`try_recv` → `None`) and reconnected by
        // the hub, with its backoff and its "look at once".
        listener.eager_reconnect(false);
        listener
            .listen_all(link.channels.iter().map(|c| c.name()))
            .await
            .map_err(|e| query_error("wake.listen", e))?;
        let backend_pid: i32 = sqlx::query_scalar("SELECT pg_catalog.pg_backend_pid()")
            .fetch_one(&mut listener)
            .await
            .map_err(|e| query_error("wake.listen_pid", e))?;
        Ok((listener, backend_pid))
    };
    let bound = round_trip.saturating_mul(2);
    match tokio::time::timeout(bound, listen).await {
        Ok(result) => result,
        Err(_) => Err(unavailable(&link.target, "listen_timeout")
            .with_context("timeout_ms", bound.as_millis())
            .with_context("timeout_key", "database.acquire_timeout_ms")),
    }
}

/// Deliver notifications until the link fails; the failure.
async fn receive(shared: &Shared, link: &Link, mut listener: PgListener) -> ForgeError {
    let deliver = |channel: &str, payload: &str| {
        if let Some(channel) = Channel::of(channel) {
            shared.deliver(channel, payload);
        }
    };
    loop {
        match tokio::time::timeout(PING_EVERY, listener.try_recv()).await {
            Ok(Ok(Some(first))) => {
                deliver(first.channel(), first.payload());
                while let Some(next) = listener.next_buffered() {
                    deliver(next.channel(), next.payload());
                }
            }
            Ok(Ok(None)) => return unavailable(&link.target, "listen_connection_lost"),
            Ok(Err(e)) => return query_error("wake.listen_recv", e),
            Err(_) => {
                let ping = sqlx::query("SELECT 1").execute(&mut listener);
                match tokio::time::timeout(link.round_trip, ping).await {
                    Ok(Ok(_)) => {}
                    Ok(Err(e)) => return query_error("wake.listen_ping", e),
                    Err(_) => {
                        return unavailable(&link.target, "listen_ping_timeout")
                            .with_context("timeout_ms", link.round_trip.as_millis())
                            .with_context("timeout_key", "database.acquire_timeout_ms")
                    }
                }
            }
        }
    }
}

/// Send `NOTIFY channel, payload` on `conn`: delivered when its
/// transaction commits, never when it rolls back. `payload` is a kind or
/// an id (`FG-INTERNAL-0001` when it is longer than [`PAYLOAD_MAX`]).
pub(crate) async fn notify_on(
    conn: &mut PgConnection,
    channel: Channel,
    payload: &str,
) -> Result<(), ForgeError> {
    if payload.len() > PAYLOAD_MAX {
        return Err(ForgeError::new(ErrorCode::Internal)
            .with_context("operation", "wake.notify")
            .with_context("reason", "payload_too_long")
            .with_context("channel", channel.name())
            .with_context("bytes", payload.len()));
    }
    sqlx::query("SELECT pg_catalog.pg_notify($1, $2)")
        .bind(channel.name())
        .bind(payload)
        .execute(&mut *conn)
        .await
        .map_err(|e| query_error("wake.notify", e))?;
    Ok(())
}

/// How full the server's NOTIFY queue is (0.0–1.0), read on the read
/// pool. It fills when a session that LISTENs stays in a transaction.
pub async fn notify_queue_usage(db: &Db) -> Result<f64, ForgeError> {
    db.within("wake.notify_queue_usage", async {
        sqlx::query_scalar("SELECT pg_catalog.pg_notification_queue_usage()")
            .fetch_one(db.pool().executor())
            .await
            .map_err(|e| read_query_error("wake.notify_queue_usage", e))
    })
    .await
}

/// `FG-DB-0023` when the NOTIFY queue is at least [`NOTIFY_QUEUE_ALERT`]
/// full (at 1.0 every transaction that notifies fails at its commit).
pub fn check_notify_queue(usage: f64) -> Result<(), ForgeError> {
    if usage < NOTIFY_QUEUE_ALERT {
        return Ok(());
    }
    Err(ForgeError::new(ErrorCode::NotifyQueueBacklog)
        .with_context("usage", format!("{usage:.3}"))
        .with_context("threshold", NOTIFY_QUEUE_ALERT))
}

#[cfg(test)]
#[path = "wake_tests.rs"]
mod tests;
