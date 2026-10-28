//! [`Dispatcher`]: every registered subscriber, run by `forge worker`.
//! A [`Dispatcher::round`] looks up which enabled subscribers are behind
//! `high_seq` (one read-pool query — the comparison of ADR-0016 §7) and
//! runs up to [`BATCH_PER_ROUND`] steps for each. A subscriber whose step
//! failed waits (in memory) before its next attempt — 200 ms doubling up
//! to [`MAX_RETRY_DELAY`] — so a stuck one is retried, and alerts, about
//! once a minute without holding the others back. [`Dispatcher::wait`]
//! waits for the next NOTIFY or the [`POLL`] interval.

use std::sync::Arc;
use std::time::Duration;

use forge_errors::ForgeError;
use tokio::time::Instant;

use super::run::{run_once, Incident, Progress};
use super::wake::{Wake, Woken};
use super::{register, Registry, Start, Subscriber};
use crate::{read_query_error, Db};

/// The longest wait for work without a notification (ADR-0016 §7).
pub const POLL: Duration = Duration::from_secs(5);
/// Steps per subscriber per round (then the round reports `more`).
pub const BATCH_PER_ROUND: u32 = 100;
const FIRST_RETRY_DELAY: Duration = Duration::from_millis(200);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(60);

struct Entry {
    subscriber: Arc<dyn Subscriber>,
    failures: u32,
    not_before: Option<Instant>,
}

impl Entry {
    fn succeeded(&mut self) {
        self.failures = 0;
        self.not_before = None;
    }

    fn failed(&mut self, now: Instant) {
        self.failures = self.failures.saturating_add(1);
        let factor = 1u32 << (self.failures - 1).min(16);
        self.not_before = Some(
            now + FIRST_RETRY_DELAY
                .saturating_mul(factor)
                .min(MAX_RETRY_DELAY),
        );
    }
}

/// What one round did.
#[derive(Debug, Default)]
pub struct Round {
    /// Events handled or quarantined.
    pub handled: u64,
    /// A subscriber used its whole batch: run again without waiting.
    pub more: bool,
    /// Failures to log (once, by the caller).
    pub incidents: Vec<Incident>,
}

pub struct Dispatcher {
    entries: Vec<Entry>,
    wake: Wake,
}

impl std::fmt::Debug for Dispatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list()
            .entries(self.entries.iter().map(|e| e.subscriber.name()))
            .finish()
    }
}

impl Dispatcher {
    /// Register every subscriber (new ones start at the head, ADR-0016 §1).
    pub async fn start(db: &Db, registry: &Registry) -> Result<Self, ForgeError> {
        let mut entries = Vec::new();
        for subscriber in registry.iter() {
            register(db, subscriber.as_ref(), Start::Head).await?;
            entries.push(Entry {
                subscriber: Arc::clone(subscriber),
                failures: 0,
                not_before: None,
            });
        }
        Ok(Self {
            entries,
            wake: Wake::new(),
        })
    }

    pub async fn round(&mut self, db: &Db) -> Result<Round, ForgeError> {
        let behind = behind(db).await?;
        let mut round = Round::default();
        for entry in &mut self.entries {
            let name = entry.subscriber.name();
            if !behind.iter().any(|b| b == name) {
                continue;
            }
            if entry.not_before.is_some_and(|t| Instant::now() < t) {
                continue;
            }
            let mut steps = 0;
            loop {
                if steps == BATCH_PER_ROUND {
                    round.more = true;
                    break;
                }
                steps += 1;
                match run_once(db, entry.subscriber.as_ref()).await {
                    Ok(Progress::Handled { .. }) => {
                        round.handled += 1;
                        entry.succeeded();
                    }
                    Ok(Progress::Quarantined(incident)) => {
                        round.handled += 1;
                        round.incidents.push(incident);
                        entry.succeeded();
                    }
                    Ok(Progress::CaughtUp | Progress::Busy | Progress::Disabled) => break,
                    Ok(Progress::Failed(incident) | Progress::Stuck(incident)) => {
                        round.incidents.push(incident);
                        entry.failed(Instant::now());
                        break;
                    }
                    Err(error) => {
                        round.incidents.push(Incident {
                            seq: None,
                            request_id: None,
                            error,
                        });
                        entry.failed(Instant::now());
                        break;
                    }
                }
            }
        }
        Ok(round)
    }

    /// Wait for the next notification, at most [`POLL`].
    pub async fn wait(&mut self, db: &Db) -> Result<Woken, ForgeError> {
        self.wake.wait(db, POLL).await
    }
}

/// The enabled subscribers whose cursor is below `high_seq`.
async fn behind(db: &Db) -> Result<Vec<String>, ForgeError> {
    db.within("subscribers.behind", async {
        let mut conn = db.acquire().await?;
        sqlx::query_scalar(
            "SELECT s.name FROM forge.event_subscribers AS s \
             JOIN forge.event_feed_state AS f ON f.singleton \
             WHERE s.enabled AND s.last_seq < f.high_seq",
        )
        .fetch_all(&mut *conn)
        .await
        .map_err(|e| read_query_error("subscribers.behind", e))
    })
    .await
}

#[cfg(test)]
mod unit {
    use super::*;

    struct Nothing;
    impl Subscriber for Nothing {
        fn name(&self) -> &'static str {
            "nothing"
        }
        fn ordering(&self) -> super::super::Ordering {
            super::super::Ordering::Independent
        }
        fn handle<'a>(
            &'a self,
            _event: &'a super::super::SourceEvent,
            _tx: &'a mut super::super::DerivedTx<'_>,
        ) -> super::super::HandleFuture<'a> {
            Box::pin(async { Ok(()) })
        }
    }

    #[test]
    fn a_failing_subscriber_backs_off_up_to_a_minute() {
        let now = Instant::now();
        let mut e = Entry {
            subscriber: Arc::new(Nothing),
            failures: 0,
            not_before: None,
        };
        e.failed(now);
        assert_eq!(e.not_before, Some(now + Duration::from_millis(200)));
        e.failed(now);
        assert_eq!(e.not_before, Some(now + Duration::from_millis(400)));
        for _ in 0..40 {
            e.failed(now);
        }
        assert_eq!(e.not_before, Some(now + MAX_RETRY_DELAY));
        e.succeeded();
        assert_eq!((e.failures, e.not_before), (0, None));
    }
}
