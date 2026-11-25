//! Which waiting request runs next (ADR-0017 §3, plan 19 §1.1). Pure: the
//! gateway calls [`Scheduler::pick`] with the clock and the conditions
//! and hands the turn to the ticket it returns.
//!
//! Rules, in order:
//! 1. Nothing starts while [`Scheduler::running`] is at the concurrency
//!    (default 1). A running request is never preempted.
//! 2. A request that waited [`PROMOTE_AFTER`] counts one tier higher.
//! 3. Eligibility: an interactive request always; while a Kanflow user
//!    waits for the shared Ollama ([`Kanflow::Busy`]) nothing else starts;
//!    while Kanflow cannot be asked ([`Kanflow::Unreachable`]) the others
//!    start at most one per [`UNREACHABLE_INTERVAL`] (Kanflow's own
//!    `degraded` rule); while a `gpu-eval` job runs, batch work does not
//!    start.
//! 4. The review reserve: when the GPU time of the last
//!    [`RESERVE_WINDOW`] went less than [`REVIEW_RESERVE_PERCENT`] % to
//!    reviews and a review waits, it goes first — interactive work cannot
//!    starve reviews.
//! 5. Otherwise the highest (effective) tier, oldest first.

use std::collections::VecDeque;
use std::time::Duration;

use forge_core::llm::{Priority, PROMOTE_AFTER, REVIEW_RESERVE_PERCENT};
use tokio::time::Instant;

/// The window the review reserve is measured over.
pub const RESERVE_WINDOW: Duration = Duration::from_secs(60 * 60);
/// Non-interactive starts while Kanflow is unreachable: one per minute.
pub const UNREACHABLE_INTERVAL: Duration = Duration::from_secs(60);

/// What Kanflow's `ai-busy` said last.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Kanflow {
    /// No user waits (or no Kanflow gate is configured).
    #[default]
    Idle,
    /// A Kanflow user waits for the shared Ollama.
    Busy,
    /// Kanflow did not answer: treated as idle, throttled.
    Unreachable,
}

/// The world outside the queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Conditions {
    pub kanflow: Kanflow,
    pub gpu_eval_running: bool,
}

/// Why a waiting request did not start (the yield metric's reason).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Held {
    KanflowBusy,
    KanflowUnreachable,
    GpuEval,
}

impl Held {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::KanflowBusy => "kanflow_busy",
            Self::KanflowUnreachable => "kanflow_unreachable",
            Self::GpuEval => "gpu_eval",
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Waiting {
    ticket: u64,
    priority: Priority,
    queued_at: Instant,
}

/// The queue's state.
#[derive(Debug)]
pub struct Scheduler {
    concurrency: usize,
    running: usize,
    waiting: Vec<Waiting>,
    /// (ended at, busy time, was a review) of the recent runs.
    history: VecDeque<(Instant, Duration, bool)>,
    last_throttled_start: Option<Instant>,
}

impl Scheduler {
    pub fn new(concurrency: usize) -> Self {
        Self {
            concurrency: concurrency.max(1),
            running: 0,
            waiting: Vec::new(),
            history: VecDeque::new(),
            last_throttled_start: None,
        }
    }

    pub fn running(&self) -> usize {
        self.running
    }

    pub fn waiting(&self) -> usize {
        self.waiting.len()
    }

    /// Whether any request waits.
    pub fn has_waiting(&self) -> bool {
        !self.waiting.is_empty()
    }

    pub fn enqueue(&mut self, ticket: u64, priority: Priority, now: Instant) {
        self.waiting.push(Waiting {
            ticket,
            priority,
            queued_at: now,
        });
    }

    /// Remove a waiting ticket (its deadline passed); `false` when it was
    /// not waiting (it got its turn meanwhile).
    pub fn cancel(&mut self, ticket: u64) -> bool {
        let before = self.waiting.len();
        self.waiting.retain(|w| w.ticket != ticket);
        self.waiting.len() != before
    }

    /// Every waiting ticket, removed (the gateway stopped).
    pub fn drain(&mut self) -> Vec<u64> {
        self.waiting.drain(..).map(|w| w.ticket).collect()
    }

    /// A started request ended after `busy`.
    pub fn finished(&mut self, busy: Duration, review: bool, now: Instant) {
        self.running = self.running.saturating_sub(1);
        self.history.push_back((now, busy, review));
        self.prune(now);
    }

    fn prune(&mut self, now: Instant) {
        while self
            .history
            .front()
            .is_some_and(|(at, _, _)| now.saturating_duration_since(*at) > RESERVE_WINDOW)
        {
            self.history.pop_front();
        }
    }

    /// The share of the window's GPU time that went to reviews, in percent
    /// (`None`: no GPU time in the window).
    pub fn review_share(&self) -> Option<u32> {
        let total: Duration = self.history.iter().map(|(_, d, _)| *d).sum();
        if total.is_zero() {
            return None;
        }
        let review: Duration = self
            .history
            .iter()
            .filter(|(_, _, r)| *r)
            .map(|(_, d, _)| *d)
            .sum();
        let pct = review.as_millis().saturating_mul(100) / total.as_millis().max(1);
        Some(u32::try_from(pct).unwrap_or(100))
    }

    fn effective(w: &Waiting, now: Instant) -> Priority {
        if now.saturating_duration_since(w.queued_at) >= PROMOTE_AFTER {
            w.priority.promoted()
        } else {
            w.priority
        }
    }

    /// Why `w` may not start now (`None`: it may).
    fn held(&self, effective: Priority, cond: Conditions, now: Instant) -> Option<Held> {
        if effective == Priority::Interactive {
            return None;
        }
        match cond.kanflow {
            Kanflow::Busy => return Some(Held::KanflowBusy),
            Kanflow::Unreachable => {
                if self
                    .last_throttled_start
                    .is_some_and(|t| now.saturating_duration_since(t) < UNREACHABLE_INTERVAL)
                {
                    return Some(Held::KanflowUnreachable);
                }
            }
            Kanflow::Idle => {}
        }
        if cond.gpu_eval_running && effective == Priority::Batch {
            return Some(Held::GpuEval);
        }
        None
    }

    /// The next ticket to start, removed from the queue and counted as
    /// running; and the waiting tickets that were held, with why (for the
    /// yield metric).
    pub fn pick(&mut self, now: Instant, cond: Conditions) -> (Option<u64>, Vec<(u64, Held)>) {
        let mut held = Vec::new();
        if self.running >= self.concurrency || self.waiting.is_empty() {
            return (None, held);
        }
        let mut eligible: Vec<(usize, Priority)> = Vec::new();
        for (i, w) in self.waiting.iter().enumerate() {
            let eff = Self::effective(w, now);
            match self.held(eff, cond, now) {
                None => eligible.push((i, eff)),
                Some(h) => held.push((w.ticket, h)),
            }
        }
        let oldest = |items: &mut dyn Iterator<Item = &(usize, Priority)>| {
            items
                .min_by_key(|(i, _)| self.waiting.get(*i).map(|w| (w.queued_at, w.ticket)))
                .map(|(i, _)| *i)
        };
        let reserve_due = self
            .review_share()
            .is_some_and(|pct| pct < REVIEW_RESERVE_PERCENT);
        let review_first = if reserve_due {
            oldest(&mut eligible.iter().filter(|(i, _)| {
                self.waiting
                    .get(*i)
                    .is_some_and(|w| w.priority == Priority::Review)
            }))
        } else {
            None
        };
        let chosen = review_first.or_else(|| {
            let best = eligible.iter().map(|(_, p)| p.rank()).min()?;
            oldest(&mut eligible.iter().filter(|(_, p)| p.rank() == best))
        });
        let Some(index) = chosen else {
            return (None, held);
        };
        let w = self.waiting.remove(index);
        let eff = Self::effective(&w, now);
        if eff != Priority::Interactive && cond.kanflow == Kanflow::Unreachable {
            self.last_throttled_start = Some(now);
        }
        self.running += 1;
        (Some(w.ticket), held)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(base: Instant, secs: u64) -> Instant {
        base + Duration::from_secs(secs)
    }

    #[test]
    fn interactive_preempts_queue_not_running() {
        let t0 = Instant::now();
        let mut s = Scheduler::new(1);
        s.enqueue(1, Priority::Batch, t0);
        s.enqueue(2, Priority::Review, at(t0, 1));
        let (first, _) = s.pick(at(t0, 2), Conditions::default());
        assert_eq!(first, Some(2), "review before batch");
        // An interactive request arrives while the review runs: it waits for
        // the running one (never preempted) but goes before the batch.
        s.enqueue(3, Priority::Interactive, at(t0, 3));
        assert_eq!(s.pick(at(t0, 4), Conditions::default()).0, None);
        assert_eq!(s.running(), 1);
        s.finished(Duration::from_secs(5), true, at(t0, 7));
        assert_eq!(s.pick(at(t0, 8), Conditions::default()).0, Some(3));
        s.finished(Duration::from_secs(1), false, at(t0, 9));
        assert_eq!(s.pick(at(t0, 10), Conditions::default()).0, Some(1));
    }

    #[test]
    fn review_tier_not_starved() {
        let t0 = Instant::now();
        let mut s = Scheduler::new(1);
        // The last hour went to interactive work only.
        s.history
            .push_back((at(t0, 0), Duration::from_secs(600), false));
        s.enqueue(1, Priority::Interactive, at(t0, 1));
        s.enqueue(2, Priority::Review, at(t0, 2));
        assert_eq!(s.review_share(), Some(0));
        assert_eq!(
            s.pick(at(t0, 3), Conditions::default()).0,
            Some(2),
            "the reserve gives the review its turn"
        );
        s.finished(Duration::from_secs(300), true, at(t0, 303));
        // 300 / 900 = 33 % ≥ 20 %: priority order again.
        s.enqueue(3, Priority::Review, at(t0, 304));
        assert_eq!(s.pick(at(t0, 305), Conditions::default()).0, Some(1));
    }

    #[test]
    fn waiting_ten_minutes_promotes_one_tier() {
        let t0 = Instant::now();
        let mut s = Scheduler::new(1);
        s.enqueue(1, Priority::Batch, t0);
        s.enqueue(2, Priority::Autofix, at(t0, 30));
        assert_eq!(s.pick(at(t0, 31), Conditions::default()).0, Some(2));
        s.finished(Duration::from_secs(1), false, at(t0, 32));
        s.enqueue(3, Priority::Autofix, at(t0, 599));
        // The batch request waited 10 min: it counts as autofix and is older.
        assert_eq!(s.pick(at(t0, 600), Conditions::default()).0, Some(1));
    }

    #[test]
    fn kanflow_busy_defers_batch() {
        let t0 = Instant::now();
        let mut s = Scheduler::new(1);
        s.enqueue(1, Priority::Batch, t0);
        s.enqueue(2, Priority::Review, t0);
        let busy = Conditions {
            kanflow: Kanflow::Busy,
            gpu_eval_running: false,
        };
        let (picked, held) = s.pick(at(t0, 1), busy);
        assert_eq!(picked, None);
        assert_eq!(held, vec![(1, Held::KanflowBusy), (2, Held::KanflowBusy)]);
        // Interactive work still runs: a person waits for it too.
        s.enqueue(3, Priority::Interactive, at(t0, 2));
        assert_eq!(s.pick(at(t0, 3), busy).0, Some(3));
        s.finished(Duration::from_secs(1), false, at(t0, 4));
        assert_eq!(s.pick(at(t0, 5), Conditions::default()).0, Some(2));
    }

    #[test]
    fn kanflow_unreachable_throttles_not_blocks() {
        let t0 = Instant::now();
        let mut s = Scheduler::new(1);
        let down = Conditions {
            kanflow: Kanflow::Unreachable,
            gpu_eval_running: false,
        };
        s.enqueue(1, Priority::Review, t0);
        s.enqueue(2, Priority::Review, t0);
        assert_eq!(s.pick(at(t0, 1), down).0, Some(1), "not blocked");
        s.finished(Duration::from_secs(5), true, at(t0, 6));
        let (picked, held) = s.pick(at(t0, 30), down);
        assert_eq!(picked, None, "one per minute");
        assert_eq!(held, vec![(2, Held::KanflowUnreachable)]);
        assert_eq!(s.pick(at(t0, 61), down).0, Some(2));
    }

    #[test]
    fn gpu_eval_holds_batch_only() {
        let t0 = Instant::now();
        let mut s = Scheduler::new(2);
        let bench = Conditions {
            kanflow: Kanflow::Idle,
            gpu_eval_running: true,
        };
        s.enqueue(1, Priority::Batch, t0);
        s.enqueue(2, Priority::Review, t0);
        assert_eq!(s.pick(at(t0, 1), bench).0, Some(2));
        let (picked, held) = s.pick(at(t0, 2), bench);
        assert_eq!(picked, None);
        assert_eq!(held, vec![(1, Held::GpuEval)]);
        assert!(s.cancel(1));
        assert!(!s.cancel(1));
    }
}
