//! The job queue (ADR-0016 §7): internal background work in `forge.jobs`
//! with fenced leases.
//!
//! - [`enqueue`] (or `ChangeTx::enqueue_job`, inside a change) inserts a
//!   `queued` job; a `dedup_key` already used by the same kind is
//!   [`Enqueued::Deduped`] (for as long as the old row exists, whatever its
//!   state).
//! - [`claim`] takes one ready job in ONE autocommit statement
//!   (`FOR UPDATE SKIP LOCKED`): `state = running`, `attempts + 1`,
//!   `worker_id`, `lease_until = now() + timeout + 30 s`. Concurrent
//!   claimers never get the same job.
//! - [`complete`] / [`fail`] are fenced by `(id, state = running, worker_id,
//!   attempts)`: a worker whose lease was reaped and re-claimed changes
//!   nothing ([`Completed::Fenced`] / [`Failed::Fenced`] carry `FG-DB-0007`).
//! - [`reap`] (every 30 s, ONE statement) ends expired leases: `dead` when
//!   `attempts >= max_attempts` (`last_error_code = FG-DB-0007`), otherwise
//!   `queued` again after the backoff.
//! - [`JobRunner`] runs one job at a time with a [`JobHandler`] per kind
//!   under `tokio::time::timeout(timeout_ms)` and reports dead jobs
//!   (`FG-DB-0006`, alert) and lost leases (`FG-DB-0007`, warn).
//! - [`health`] is what `forge doctor` reports (`jobs.dead`,
//!   `jobs.expired_leases`, `jobs.queue_depth`).
//!
//! Delivery is at-least-once: a job whose lease expires (a crashed worker, a
//! cancelled run, a lost COMMIT answer) runs again, so every handler must be
//! idempotent. Backoff after a failure or an expired lease:
//! 10 s × 2^(attempt − 1), capped at 1 h.
//!
//! **Scale (plan 01 §3.10, the factory IoT edge fleet):** this queue is for
//! internal work — tens of kinds, a backlog of hundreds. It must never hold
//! one job per edge device (thousands of rows per rollout): fleet-wide work
//! is ONE job that walks its targets itself, keeping per-target progress in
//! its own table. The doctor alert at [`QUEUE_DEPTH_ALERT`] ready jobs of a
//! kind is there to catch exactly that misuse.

use std::time::Duration;

use forge_core::RequestId;
use forge_errors::ForgeError;

mod doctor;
mod queue;
mod runner;

pub use doctor::{health, ExpiredLease, JobsHealth, KindDepth, NewestDead};
pub use doctor::{DEAD_WINDOW, LEASE_GRACE, QUEUE_DEPTH_ALERT, QUEUE_WAIT_ALERT};
pub(crate) use queue::enqueue_on;
pub use queue::{claim, complete, enqueue, fail, reap, REAP_BATCH};
pub use runner::{JobFuture, JobHandler, JobRunner, Ran, Reported};

/// How long a lease outlives the job's own timeout (ADR-0016 §7).
pub const LEASE_SLACK: Duration = Duration::from_secs(30);

/// A job kind: lowercase dotted words (`audit.seal`, `retention.purge`),
/// at most 64 bytes. Chosen in code, never from input.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct JobKind(String);

impl JobKind {
    pub const MAX_LEN: usize = 64;

    pub fn parse(raw: &str) -> Option<Self> {
        let word_ok = |w: &str| {
            w.chars().next().is_some_and(|c| c.is_ascii_lowercase())
                && w.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        };
        let ok = !raw.is_empty() && raw.len() <= Self::MAX_LEN && raw.split('.').all(word_ok);
        ok.then(|| Self(raw.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// `forge.jobs.id`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct JobId(i64);

impl JobId {
    pub fn new(id: i64) -> Self {
        Self(id)
    }

    pub fn get(self) -> i64 {
        self.0
    }
}

/// The worker that holds a lease (`forge.jobs.worker_id`): one per
/// `forge worker` process, `w<pid>-<random>`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WorkerId(String);

impl WorkerId {
    pub fn generate() -> Self {
        let random = uuid::Uuid::new_v4().simple().to_string();
        let short = random.get(..12).unwrap_or(&random);
        Self(format!("w{}-{short}", std::process::id()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A job's idempotency key within its kind (1..=200 bytes). Build it from
/// server-side ids, never from a raw client value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DedupKey(String);

impl DedupKey {
    pub const MAX_LEN: usize = 200;

    pub fn parse(raw: &str) -> Option<Self> {
        (!raw.is_empty() && raw.len() <= Self::MAX_LEN).then(|| Self(raw.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Runs before a job is dead (1..=25).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MaxAttempts(u8);

impl MaxAttempts {
    pub const DEFAULT: Self = Self(5);

    pub fn new(n: u8) -> Option<Self> {
        (1..=25).contains(&n).then_some(Self(n))
    }

    pub fn get(self) -> u8 {
        self.0
    }
}

/// How long one run of a job may take (1 ms ..= 1 h, the table's CHECK).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JobTimeout(Duration);

impl JobTimeout {
    pub const MAX: Duration = Duration::from_secs(3600);

    pub fn new(d: Duration) -> Option<Self> {
        (d >= Duration::from_millis(1) && d <= Self::MAX).then_some(Self(d))
    }

    pub fn get(self) -> Duration {
        self.0
    }

    /// Whole milliseconds (≤ 3 600 000, fits the column).
    fn millis(self) -> i32 {
        i32::try_from(self.0.as_millis()).unwrap_or(i32::MAX)
    }
}

/// A job to enqueue.
#[derive(Debug, Clone)]
pub struct NewJob {
    pub kind: JobKind,
    /// References only (ids, seqs) — never secrets; ≤ 16 KiB stored.
    pub payload: serde_json::Value,
    pub max_attempts: MaxAttempts,
    pub timeout: JobTimeout,
    pub dedup_key: Option<DedupKey>,
    /// The request (or event's request) the job belongs to: its logs carry it.
    pub origin_request_id: RequestId,
    pub origin_event_seq: Option<i64>,
}

/// The result of an enqueue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Enqueued {
    Queued(JobId),
    /// The kind already has a job with this `dedup_key`: nothing was added.
    Deduped,
}

/// What fences a claimed job's result: the claim's worker and attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lease {
    pub job_id: JobId,
    pub worker_id: WorkerId,
    /// `attempts` after this claim (1 for the first run).
    pub attempt: i32,
}

/// A job as [`claim`] handed it out.
#[derive(Debug, Clone)]
pub struct ClaimedJob {
    pub lease: Lease,
    pub kind: JobKind,
    pub payload: serde_json::Value,
    pub max_attempts: i32,
    pub timeout: JobTimeout,
    /// As stored (validated again before it is used as a log request id).
    pub origin_request_id: String,
    pub origin_event_seq: Option<i64>,
}

impl ClaimedJob {
    /// The request id its logs carry: the origin's, or a fresh one when the
    /// stored value is not a valid request id.
    pub fn request_id(&self) -> RequestId {
        RequestId::parse(&self.origin_request_id).unwrap_or_else(RequestId::generate)
    }
}

/// Whether a failed run may be retried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Retry {
    /// Queued again after the backoff, unless the attempts are used up.
    Backoff,
    /// Dead at once (the job can never succeed, e.g. its target is gone).
    Never,
}

/// A failed run as a handler reports it.
#[derive(Debug)]
pub struct JobFailure {
    pub error: ForgeError,
    pub retry: Retry,
}

impl JobFailure {
    pub fn retry(error: ForgeError) -> Self {
        Self {
            error,
            retry: Retry::Backoff,
        }
    }

    pub fn permanent(error: ForgeError) -> Self {
        Self {
            error,
            retry: Retry::Never,
        }
    }
}

/// The result of [`complete`].
#[derive(Debug)]
pub enum Completed {
    Applied,
    /// The lease was lost (`FG-DB-0007 reason=fenced`): the result is discarded.
    Fenced(ForgeError),
}

/// The result of [`fail`].
#[derive(Debug)]
pub enum Failed {
    /// Queued again; runnable after this delay.
    Retrying { retry_in: Duration },
    /// Attempts used up, or [`Retry::Never`].
    Dead,
    /// The lease was lost (`FG-DB-0007 reason=fenced`): nothing changed.
    Fenced(ForgeError),
}

/// What one [`reap`] did to one expired lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReapedJob {
    pub job_id: JobId,
    pub kind: String,
    pub worker_id: Option<String>,
    pub attempts: i32,
    pub max_attempts: i32,
    pub origin_request_id: String,
    /// `true`: dead (attempts used up); `false`: queued again.
    pub dead: bool,
}

/// The backoff before attempt `attempt + 1`: 10 s × 2^(attempt − 1), ≤ 1 h.
/// (The same formula is in the SQL of `fail` and `reap`: `BACKOFF_SECS`.)
pub fn backoff(attempt: i32) -> Duration {
    let exp = u32::try_from(attempt.saturating_sub(1).clamp(0, 20)).unwrap_or(0);
    Duration::from_secs(10u64.saturating_mul(1u64 << exp)).min(Duration::from_secs(3600))
}

#[cfg(test)]
mod boundary_tests;
#[cfg(test)]
mod doctor_tests;
#[cfg(test)]
mod runner_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn kinds_are_lowercase_dotted_words() {
        assert!(JobKind::parse("audit.seal").is_some());
        assert!(JobKind::parse("retention_purge").is_some());
        for bad in [
            "",
            "Audit",
            "audit.",
            ".seal",
            "1x",
            "a-b",
            "a b",
            &"a".repeat(65),
        ] {
            assert!(JobKind::parse(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn bounds_of_the_newtypes() {
        assert!(MaxAttempts::new(0).is_none());
        assert!(MaxAttempts::new(26).is_none());
        assert!(JobTimeout::new(Duration::ZERO).is_none());
        assert!(JobTimeout::new(Duration::from_secs(3601)).is_none());
        assert_eq!(
            JobTimeout::new(Duration::from_secs(3600)).map(JobTimeout::millis),
            Some(3_600_000)
        );
        assert!(DedupKey::parse("").is_none());
        assert!(DedupKey::parse(&"k".repeat(201)).is_none());
        assert!(WorkerId::generate().as_str().len() <= 32);
        assert_ne!(WorkerId::generate(), WorkerId::generate());
    }

    #[test]
    fn backoff_doubles_from_ten_seconds_up_to_an_hour() {
        assert_eq!(backoff(1), Duration::from_secs(10));
        assert_eq!(backoff(2), Duration::from_secs(20));
        assert_eq!(backoff(4), Duration::from_secs(80));
        assert_eq!(backoff(9), Duration::from_secs(2560));
        assert_eq!(backoff(10), Duration::from_secs(3600));
        assert_eq!(backoff(1000), Duration::from_secs(3600));
        assert_eq!(backoff(0), Duration::from_secs(10));
    }
}
