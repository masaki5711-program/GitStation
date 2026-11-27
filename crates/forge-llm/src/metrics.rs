//! The gateway's metrics (plan 19 §8.4; names reserved by ADR-0017):
//!
//! - `forge_llm_queue_wait_seconds{priority}` (histogram)
//! - `forge_llm_request_duration_seconds{purpose}` (histogram)
//! - `forge_llm_requests_total{purpose,outcome}` (`ok` / `error_code`)
//! - `forge_llm_output_invalid_total{purpose}`
//! - `forge_llm_tokens_total{direction}` (`input` / `output`)
//! - `forge_llm_yield_total{reason}` (`kanflow_busy` / `kanflow_unreachable` /
//!   `gpu_eval`)
//! - `forge_llm_evidence_dropped_total{purpose}`
//! - `forge_llm_advice_feedback_total{purpose,verdict}`: people's verdicts
//!   on the advice, counted from `forge.llm_advice_feedback` (rows are never
//!   deleted) at each snapshot — the votes are taken by `forge serve`, not
//!   the worker, so the table is the one counter both processes share.
//!
//! In-process counters; [`Metrics::render`] writes the Prometheus text
//! format, and the worker logs a snapshot (`forge.llm.metrics`) every
//! five minutes until forge serves a metrics endpoint.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Duration;

/// Histogram buckets, in seconds.
const BUCKETS: [f64; 10] = [0.5, 1.0, 2.0, 5.0, 10.0, 30.0, 60.0, 120.0, 600.0, 1800.0];

#[derive(Debug, Default, Clone)]
struct Histogram {
    counts: [u64; BUCKETS.len()],
    count: u64,
    sum: f64,
}

#[derive(Debug, Default)]
struct Inner {
    counters: BTreeMap<(&'static str, String), u64>,
    histograms: BTreeMap<(&'static str, String), Histogram>,
}

/// The counters of one gateway.
#[derive(Debug, Default)]
pub struct Metrics {
    inner: Mutex<Inner>,
}

impl Metrics {
    fn with(&self, f: impl FnOnce(&mut Inner)) {
        // A poisoned lock only means a panic elsewhere while counting: the
        // numbers are still usable.
        let mut guard = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        f(&mut guard);
    }

    pub fn add(&self, name: &'static str, labels: String, n: u64) {
        self.with(|m| *m.counters.entry((name, labels)).or_default() += n);
    }

    pub fn observe(&self, name: &'static str, labels: String, value: Duration) {
        let secs = value.as_secs_f64();
        self.with(|m| {
            let h = m.histograms.entry((name, labels)).or_default();
            for (slot, bound) in h.counts.iter_mut().zip(BUCKETS) {
                if secs <= bound {
                    *slot += 1;
                }
            }
            h.count += 1;
            h.sum += secs;
        });
    }

    /// Replace the feedback totals with `totals` (`(purpose, verdict, n)`,
    /// [`forge_db::llm::ops::feedback_totals`]): a label that is gone
    /// from the table is gone here too.
    pub fn set_feedback_totals(&self, totals: &[(String, String, u64)]) {
        self.with(|m| {
            m.counters.retain(|(name, _), _| *name != ADVICE_FEEDBACK);
            for (purpose, verdict, n) in totals {
                m.counters.insert(
                    (
                        ADVICE_FEEDBACK,
                        label(&[("purpose", purpose), ("verdict", verdict)]),
                    ),
                    *n,
                );
            }
        });
    }

    /// The counter's value (tests, doctor).
    pub fn counter(&self, name: &'static str, labels: &str) -> u64 {
        let mut value = 0;
        self.with(|m| {
            value = m
                .counters
                .get(&(name, labels.to_owned()))
                .copied()
                .unwrap_or(0)
        });
        value
    }

    /// The histogram's sample count.
    pub fn samples(&self, name: &'static str, labels: &str) -> u64 {
        let mut value = 0;
        self.with(|m| {
            value = m
                .histograms
                .get(&(name, labels.to_owned()))
                .map_or(0, |h| h.count);
        });
        value
    }

    /// The Prometheus text format.
    pub fn render(&self) -> String {
        let mut out = String::new();
        self.with(|m| {
            for ((name, labels), v) in &m.counters {
                out.push_str(&format!("{name}{{{labels}}} {v}\n"));
            }
            for ((name, labels), h) in &m.histograms {
                let sep = if labels.is_empty() { "" } else { "," };
                for (bound, c) in BUCKETS.iter().zip(h.counts) {
                    out.push_str(&format!(
                        "{name}_bucket{{{labels}{sep}le=\"{bound}\"}} {c}\n"
                    ));
                }
                out.push_str(&format!(
                    "{name}_bucket{{{labels}{sep}le=\"+Inf\"}} {}\n",
                    h.count
                ));
                out.push_str(&format!("{name}_sum{{{labels}}} {}\n", h.sum));
                out.push_str(&format!("{name}_count{{{labels}}} {}\n", h.count));
            }
        });
        out
    }
}

/// `key="value"` label text (values are forge's own enums and codes).
pub fn label(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{k}=\"{v}\""))
        .collect::<Vec<_>>()
        .join(",")
}

pub const QUEUE_WAIT: &str = "forge_llm_queue_wait_seconds";
pub const REQUEST_DURATION: &str = "forge_llm_request_duration_seconds";
pub const REQUESTS: &str = "forge_llm_requests_total";
pub const OUTPUT_INVALID: &str = "forge_llm_output_invalid_total";
pub const TOKENS: &str = "forge_llm_tokens_total";
pub const YIELDS: &str = "forge_llm_yield_total";
pub const EVIDENCE_DROPPED: &str = "forge_llm_evidence_dropped_total";
pub const ADVICE_FEEDBACK: &str = "forge_llm_advice_feedback_total";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_and_histograms_render() {
        let m = Metrics::default();
        m.add(
            REQUESTS,
            label(&[("purpose", "mr_review"), ("outcome", "ok")]),
            2,
        );
        m.observe(
            QUEUE_WAIT,
            label(&[("priority", "review")]),
            Duration::from_secs(3),
        );
        assert_eq!(
            m.counter(REQUESTS, "purpose=\"mr_review\",outcome=\"ok\""),
            2
        );
        assert_eq!(m.samples(QUEUE_WAIT, "priority=\"review\""), 1);
        let text = m.render();
        assert!(text.contains("forge_llm_requests_total{purpose=\"mr_review\",outcome=\"ok\"} 2"));
        assert!(
            text.contains("forge_llm_queue_wait_seconds_bucket{priority=\"review\",le=\"5\"} 1")
        );
        assert!(
            text.contains("forge_llm_queue_wait_seconds_bucket{priority=\"review\",le=\"2\"} 0")
        );
    }

    #[test]
    fn feedback_totals_replace_the_last_snapshot() {
        let m = Metrics::default();
        m.set_feedback_totals(&[
            ("mr_review".to_owned(), "useful".to_owned(), 3),
            ("mr_review".to_owned(), "wrong".to_owned(), 1),
        ]);
        assert_eq!(
            m.counter(ADVICE_FEEDBACK, "purpose=\"mr_review\",verdict=\"useful\""),
            3
        );
        m.set_feedback_totals(&[("mr_review".to_owned(), "useful".to_owned(), 4)]);
        let text = m.render();
        assert!(text.contains(
            "forge_llm_advice_feedback_total{purpose=\"mr_review\",verdict=\"useful\"} 4"
        ));
        assert!(!text.contains("verdict=\"wrong\""), "{text}");
    }
}
