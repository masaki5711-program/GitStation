//! What a runner's long poll waits for between two claims (plan 18 S2,
//! ADR-0037 (a)): `NOTIFY forge_jobs, 'ci.job'` — sent by every statement
//! that makes a CI job claimable (a planned pipeline, a retry, a played
//! manual job, a job whose needs succeeded, a lease given back by the
//! reaper) and by the end of a job or a cancel (a GPU place may be free) —
//! or, at the latest, the safety-net period `jobs.safety_poll_ms`.
//!
//! The wake is a hint: the caller claims again and the claim reads
//! `forge.ci_jobs`. Without a hub on the [`Db`], or while its LISTEN
//! connection is down, the wait is the plain poll of before plan 18.

use std::time::Duration;

use forge_core::RequestId;
use forge_errors::{ErrorCode, ForgeError};

use crate::wake::{Channel, WakeHub, WakeRx, Woken, CI_JOB};
use crate::Db;

/// The subscription of one long poll on the process's hub.
#[derive(Debug)]
struct Listening {
    hub: WakeHub,
    rx: WakeRx,
    safety: Duration,
}

/// The idle wait of one runner's long poll. Made before the first claim,
/// so that a job made pending after that claim ends the first wait.
#[derive(Debug)]
pub struct JobWait {
    listening: Option<Listening>,
    /// How often the poll looks without a live listener.
    poll: Duration,
    /// The last wait ran the whole safety-net period with the hub live.
    after_safety_net: bool,
}

impl JobWait {
    /// The wait of a long poll on `db`; `poll` is its period without a hub
    /// and while the hub's connection is down.
    pub fn new(db: &Db, poll: Duration) -> Self {
        let listening = match (db.wake(), db.jobs_safety_poll()) {
            (Some(hub), Some(safety)) if hub.listens(Channel::Jobs) => Some(Listening {
                rx: hub.subscribe(Channel::Jobs),
                hub: hub.clone(), // clone: a handle on the shared hub
                safety,
            }),
            _ => None,
        };
        Self {
            listening,
            poll,
            after_safety_net: false,
        }
    }

    /// Wait at most `max` until another claim is worth a try.
    pub async fn wait(&mut self, max: Duration) {
        let Some(l) = &mut self.listening else {
            tokio::time::sleep(max.min(self.poll)).await;
            return;
        };
        let live = l.hub.is_live(Channel::Jobs);
        let period = if live {
            l.safety
        } else {
            l.safety.min(self.poll)
        };
        let wait = max.min(period);
        let woken = l.rx.wait_matching(wait, |kind| kind == CI_JOB).await;
        self.after_safety_net =
            woken == Woken::SafetyNet && live && wait >= l.safety && l.hub.is_live(Channel::Jobs);
    }

    /// A claim took `job`: one found by a safety-net look that no wake
    /// announced is reported (`FG-DB-0024`, at most once a minute).
    pub fn claimed(&mut self, runner: i64, job: i64, request_id: &RequestId) {
        let after_safety_net = std::mem::take(&mut self.after_safety_net);
        let Some(l) = &self.listening else {
            return;
        };
        if after_safety_net && l.hub.is_live(Channel::Jobs) {
            let error = ForgeError::new(ErrorCode::WakeupMissed)
                .with_context("safety_poll_ms", l.safety.as_millis())
                .with_context("kind", CI_JOB)
                .with_context("job_id", job)
                .with_context("runner_id", runner);
            l.hub.missed(Channel::Jobs, error, request_id);
        }
    }
}
