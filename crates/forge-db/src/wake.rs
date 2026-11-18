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
//! (half-open TCP) is found by a bounded read every [`PING_EVERY`].
//! A lost link is reconnected with a backoff from [`RECONNECT_MIN`] to
//! [`RECONNECT_MAX`]. Notifications sent while it is down are lost, so
//! every (re)connect wakes every waiter once ([`Woken::Reconnected`]:
//! "look at once").
//!
//! **A dead listener is visible.** Each failure is logged with its code
//! (`FG-DB-0001` for the link, `FG-DB-0002` for a refused statement) and
//! `operation=wake_listen` — once when it starts, again when the reason
//! changes and every [`RELOG_EVERY`] while it lasts (a link that connects
//! and drops again is ONE lasting failure: it counts as recovered only
//! after it stayed up for [`RECONNECT_MAX`]). Meanwhile
//! [`WakeHub::is_live`] is false and waiters fall back to their own poll
//! (the job runners: one second, as before plan 18). A waiter never spins:
//! every wait ends by a notification, a (re)connect, the loss of the link
//! (once) or its own deadline.
//!
//! **A task that ends is visible too.** The listening task runs under a
//! supervisor: when it ends without a requested stop (a panic, or a return
//! nobody asked for) the supervisor marks the hub down, logs
//! `FG-SYS-0001` with `task=wake_hub` (`reason=task_panicked` /
//! `task_ended` / `task_cancelled`) at once — again every [`RELOG_EVERY`]
//! while it keeps ending — and starts it again after [`RECONNECT_MAX`].
//! Waiters poll meanwhile and are told to look when it is back.
//!
//! **The NOTIFY queue is watched.** The LISTEN connection (the primary, by
//! construction) reads `pg_notification_queue_usage()` every
//! [`PING_EVERY`] — that read is also its link check — and logs
//! `FG-DB-0023` while the queue is at least [`NOTIFY_QUEUE_ALERT`] full
//! (once, then every [`RELOG_EVERY`]): a full queue fails the COMMIT of
//! every transaction that notifies.
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
use std::future::Future;
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
use crate::{query_error, Db};

/// The longest NOTIFY payload the hub sends (PostgreSQL refuses 8000 bytes
/// and more; a kind or an id is far shorter).
pub const PAYLOAD_MAX: usize = 255;
/// Distinct payloads remembered per channel.
pub const PAYLOADS_MAX: usize = 256;
/// The LISTEN connection is checked — and the NOTIFY queue sampled — this
/// often.
pub const PING_EVERY: Duration = Duration::from_secs(5);
/// The first pause before a reconnect.
pub const RECONNECT_MIN: Duration = Duration::from_millis(250);
/// The longest pause between reconnects; also how long a link must hold
/// to count as recovered, and the pause before an ended task is started
/// again.
pub const RECONNECT_MAX: Duration = Duration::from_secs(5);
/// A lasting listener failure is logged again this often.
pub const RELOG_EVERY: Duration = Duration::from_secs(60);
/// `FG-DB-0024` is logged at most this often per channel and process.
pub const MISSED_LOG_EVERY: Duration = Duration::from_secs(60);
/// `FG-DB-0023`: the NOTIFY queue is this full (a fraction of its 8 GB).
pub const NOTIFY_QUEUE_ALERT: f64 = 0.5;

const OPERATION: &str = "wake_listen";
/// The `task` of the hub's `FG-SYS-0001` events.
const TASK: &str = "wake_hub";
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
    /// The `FG-DB-0024` limiter of each channel.
    missed: Mutex<[Missed; 4]>,
}

/// The `FG-DB-0024` limiter of one channel: when it was last logged, and
/// how many misses were not logged since.
#[derive(Debug, Clone, Copy, Default)]
struct Missed {
    logged: Option<Instant>,
    suppressed: u64,
}

impl Missed {
    /// A miss at `now`. `Some(n)`: it is logged, and stands for the `n`
    /// misses not logged since the last one that was; `None`: it is only
    /// counted (the last one was logged less than [`MISSED_LOG_EVERY`]
    /// ago).
    fn note(&mut self, now: Instant) -> Option<u64> {
        let due = self
            .logged
            .is_none_or(|at| now.saturating_duration_since(at) >= MISSED_LOG_EVERY);
        if !due {
            self.suppressed = self.suppressed.saturating_add(1);
            return None;
        }
        self.logged = Some(now);
        Some(std::mem::take(&mut self.suppressed))
    }
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
            missed: Mutex::new([Missed::default(); 4]),
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
    /// never a reason to fail the start. `Err` (`FG-SYS-0001`): no
    /// channel was given, or there is no tokio runtime.
    pub fn spawn(db: &Db, channels: &[Channel]) -> Result<Self, ForgeError> {
        Self::spawn_running(db, channels, run)
    }

    /// [`Self::spawn`] with the listening task given (`body`: what the
    /// supervisor starts, and starts again when it ends unasked).
    fn spawn_running<F, Fut>(db: &Db, channels: &[Channel], body: F) -> Result<Self, ForgeError>
    where
        F: FnMut(Arc<Shared>, Arc<Link>, watch::Receiver<bool>) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
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
        let link = Arc::new(Link {
            options: db.listen_options().clone(), // clone: the task owns its options
            round_trip: db.round_trip_timeout(),
            target: db.target().to_owned(),
            channels: Channel::ALL
                .into_iter()
                .filter(|c| channels.contains(c))
                .collect(),
            request_id: RequestId::generate(),
        });
        let task = runtime.spawn(supervise(Arc::clone(&shared), link, stopped, body));
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
    /// [`MISSED_LOG_EVERY`] per channel; whether it was logged. The logged
    /// event carries `suppressed`: how many misses of the channel were not
    /// logged since the last one that was.
    pub fn missed(&self, channel: Channel, error: ForgeError, request_id: &RequestId) -> bool {
        let suppressed = {
            let mut limiters = self
                .0
                .shared
                .missed
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            limiters[channel.index()].note(Instant::now())
        };
        let Some(suppressed) = suppressed else {
            return false;
        };
        let error = error
            .with_context("channel", channel.name())
            .with_context("suppressed", suppressed);
        telemetry::log_failure(&error, request_id);
        true
    }

    /// End the hub's task and wait for it, at most `wait`. `Err`
    /// (`FG-SYS-0001`): its supervisor panicked or did not end in time
    /// (it is aborted, and the listening task with it). A listening task
    /// that ended by itself earlier was logged when it did. A second call
    /// has nothing left to do.
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
                        // Not reachable while this waiter lives: it holds
                        // the `Shared` that owns the sender. Kept so that
                        // a closed channel could never become a spin.
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
/// [`RELOG_EVERY`]. The failure lasts until [`Relog::settled`]: a link
/// that comes up and drops again within [`RECONNECT_MAX`] is the same
/// failure, not a new one each time.
#[derive(Default)]
struct Relog {
    /// The failure logged last, and when.
    last: Option<(String, Instant)>,
    /// "Up again" was said for it.
    announced: bool,
}

impl Relog {
    /// Log `error` unless the same was logged less than [`RELOG_EVERY`]
    /// ago; whether it was logged.
    fn report(&mut self, error: ForgeError, request_id: &RequestId) -> bool {
        self.report_at(error, request_id, Instant::now())
    }

    /// [`Self::report`] at `now`.
    fn report_at(&mut self, error: ForgeError, request_id: &RequestId, now: Instant) -> bool {
        let signature = signature(&error);
        let repeat = self.last.as_ref().is_some_and(|(last, at)| {
            *last == signature && now.saturating_duration_since(*at) < RELOG_EVERY
        });
        if repeat {
            return false;
        }
        self.last = Some((signature, now));
        self.announced = false;
        telemetry::log_failure(&error.with_context("operation", OPERATION), request_id);
        true
    }

    /// It is up. `Some(recovered)`: worth a log line — the first time
    /// (`false`), and once after each logged failure (`true`); `None`: the
    /// failure it follows already had its line (a flapping link).
    fn up(&mut self) -> Option<bool> {
        let recovered = self.last.is_some();
        if recovered && self.announced {
            return None;
        }
        self.announced = recovered;
        Some(recovered)
    }

    /// It held: what fails next is a new failure, logged at once.
    fn settled(&mut self) {
        self.last = None;
        self.announced = false;
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

/// Ends its task when dropped (the supervisor's own end takes the
/// listening task with it).
struct AbortOnDrop(JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// A stop was asked for (or the last handle of the hub is gone).
fn stop_requested(stop: &watch::Receiver<bool>) -> bool {
    *stop.borrow() || stop.has_changed().is_err()
}

/// What an end of the listening task that nobody asked for is logged as.
fn task_ended(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::Internal)
        .with_context("task", TASK)
        .with_context("reason", reason)
}

/// The hub's supervisor: runs the listening task (`body`) and, when it
/// ends without a requested stop, says so at once (`FG-SYS-0001`
/// `task=wake_hub`; the same reason again only every [`RELOG_EVERY`]),
/// leaves the waiters to their poll and starts it again after
/// [`RECONNECT_MAX`].
async fn supervise<F, Fut>(
    shared: Arc<Shared>,
    link: Arc<Link>,
    mut stop: watch::Receiver<bool>,
    mut body: F,
) where
    F: FnMut(Arc<Shared>, Arc<Link>, watch::Receiver<bool>) -> Fut,
    Fut: Future<Output = ()> + Send + 'static,
{
    let mut relog = Relog::default();
    loop {
        // clone: each run of the task has its own handles
        let listening = body(Arc::clone(&shared), Arc::clone(&link), stop.clone());
        let mut task = AbortOnDrop(tokio::spawn(listening));
        let ended = (&mut task.0).await;
        // However it ended: no wake will come, the waiters poll.
        shared.disconnected();
        if stop_requested(&stop) {
            return;
        }
        let reason = match ended {
            Ok(()) => "task_ended",
            Err(e) if e.is_panic() => "task_panicked",
            Err(_) => "task_cancelled",
        };
        relog.report(task_ended(reason), &link.request_id);
        tokio::select! {
            () = tokio::time::sleep(RECONNECT_MAX) => {}
            () = stopped(&mut stop) => return,
        }
    }
}

/// The listening task: connect, receive until the link fails, back off,
/// again — until a stop is asked for.
async fn run(shared: Arc<Shared>, link: Arc<Link>, mut stop: watch::Receiver<bool>) {
    let _down = Down(Arc::clone(&shared));
    let mut relog = Relog::default();
    let mut backlog = Relog::default();
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
                if let Some(recovered) = relog.up() {
                    tracing::info!(
                        target: "forge_db",
                        event = "forge.wake.listening",
                        request_id = %link.request_id,
                        db_target = %link.target,
                        backend_pid,
                        recovered,
                        channels = link.channels.len(),
                        "wake hub listening"
                    );
                }
                let lost = tokio::select! {
                    lost = receive(&shared, &link, listener, &mut backlog) => lost,
                    () = stopped(&mut stop) => return,
                };
                shared.disconnected();
                if since.elapsed() >= RECONNECT_MAX {
                    // It held: back to the short pause, and its loss is a
                    // new failure. A link that drops sooner is the failure
                    // already logged (a flapping link is logged once a
                    // minute, not every cycle).
                    pause = RECONNECT_MIN;
                    relog.settled();
                }
                relog.report(lost, &link.request_id);
            }
            Err(e) => {
                relog.report(e, &link.request_id);
            }
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

/// Deliver notifications until the link fails; the failure. Every
/// [`PING_EVERY`] — idle or between two batches of a busy link — the
/// connection is checked by reading the NOTIFY queue's usage on it
/// ([`sampled`]).
async fn receive(
    shared: &Shared,
    link: &Link,
    mut listener: PgListener,
    backlog: &mut Relog,
) -> ForgeError {
    let deliver = |channel: &str, payload: &str| {
        if let Some(channel) = Channel::of(channel) {
            shared.deliver(channel, payload);
        }
    };
    let mut check_at = Instant::now() + PING_EVERY;
    loop {
        match tokio::time::timeout_at(check_at, listener.try_recv()).await {
            Ok(Ok(Some(first))) => {
                deliver(first.channel(), first.payload());
                while let Some(next) = listener.next_buffered() {
                    deliver(next.channel(), next.payload());
                }
                if Instant::now() < check_at {
                    continue;
                }
            }
            Ok(Ok(None)) => return unavailable(&link.target, "listen_connection_lost"),
            Ok(Err(e)) => return query_error("wake.listen_recv", e),
            Err(_) => {}
        }
        let usage = sqlx::query_scalar::<_, f64>(NOTIFY_QUEUE_USAGE).fetch_one(&mut listener);
        match tokio::time::timeout(link.round_trip, usage).await {
            Ok(Ok(usage)) => sampled(backlog, usage, &link.request_id),
            // A sample that fails is a link that fails: logged by the
            // caller, and the connection is replaced.
            Ok(Err(e)) => return query_error("wake.listen_ping", e),
            Err(_) => {
                return unavailable(&link.target, "listen_ping_timeout")
                    .with_context("timeout_ms", link.round_trip.as_millis())
                    .with_context("timeout_key", "database.acquire_timeout_ms")
            }
        }
        check_at = Instant::now() + PING_EVERY;
    }
}

/// One sample of the NOTIFY queue's usage read by the hub: `FG-DB-0023`
/// while it is at least [`NOTIFY_QUEUE_ALERT`] full — once, then every
/// [`RELOG_EVERY`]; a new backlog after it drained is logged at once.
fn sampled(backlog: &mut Relog, usage: f64, request_id: &RequestId) {
    match check_notify_queue(usage) {
        Ok(()) => backlog.settled(),
        Err(e) => {
            backlog.report(e, request_id);
        }
    }
}

/// Send `NOTIFY channel, payload` on `conn`: delivered when its
/// transaction commits, never when it rolls back. `payload` is a kind or
/// an id (`FG-SYS-0001` when it is longer than [`PAYLOAD_MAX`]).
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

const NOTIFY_QUEUE_USAGE: &str = "SELECT pg_catalog.pg_notification_queue_usage()";

/// How full the server's NOTIFY queue is (0.0–1.0). It fills when a
/// session that LISTENs stays in a transaction or does not read its
/// socket. Read on the write pool — the server the notifications are sent
/// on; on a standby the function answers 0 and the check would be blind.
/// `forge doctor` reads it here; a running process samples it on its
/// LISTEN connection (every [`PING_EVERY`]).
pub async fn notify_queue_usage(db: &Db) -> Result<f64, ForgeError> {
    db.within("wake.notify_queue_usage", async {
        sqlx::query_scalar(NOTIFY_QUEUE_USAGE)
            .fetch_one(db.write_pool())
            .await
            .map_err(|e| query_error("wake.notify_queue_usage", e))
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
