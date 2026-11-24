//! The halt / advance decision for one wave (plan 21 §5.6, ADR-0041 §5).
//!
//! The rules, first match wins:
//!
//! 1. any `rollback_failed` → **halt** `rollback_failed` (a device's state
//!    is unknown);
//! 2. a canary wave with any `failed` → **halt** `canary_failure`;
//! 3. no members at all (every device offline, or a canary wave with no
//!    canary device) → **wait** `no_members` — never an advance without
//!    evidence; it becomes `stalled` like any wait;
//! 4. a percentage wave with `failed + unresponsive ≥ min_failures` and
//!    `failed ÷ (succeeded + failed) > failure_rate` → **halt**
//!    `failure_rate` (exactly the rate does not halt). Its own count
//!    (`min_failures`) guards small samples, so it does not wait for
//!    `min_sample`;
//! 5. fewer devices attempted than `min(min_sample, members)` → **wait**
//!    `below_min_sample` (neither halts nor advances; a group smaller than
//!    the sample can still finish);
//! 6. `unresponsive ÷ attempted > unresponsive_rate` → **halt**
//!    `unresponsive_rate`;
//! 7. at least `done_ratio` of the members in a final state and the soak
//!    (canary 24 h, percentage 4 h) over since the wave started →
//!    **advance**;
//! 8. otherwise **wait** (`in_progress` or `soaking`), `stalled` once the
//!    wave started `stall_after` (7 days) ago.
//!
//! Counts are the caller's `GROUP BY state` of the wave's devices: `failed`
//! counts `failed` and `rolled_back` (the device recovered, the release
//! still failed); offline and architecture-mismatched devices are not
//! members. Arithmetic is integer (basis points), saturating; an
//! inconsistent count (more final than attempted) is read as that many
//! attempted, never as a smaller failure rate.

use forge_errors::ErrorCode;
use jiff::Timestamp;

use crate::{Problem, WaveKind};

/// One wave's counts (module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WaveStats {
    /// Devices in the wave (online, matching arch / os).
    pub members: u32,
    /// Devices the wave was offered to that have started (any state past
    /// `offered`, including the final ones and `unresponsive`).
    pub attempted: u32,
    pub succeeded: u32,
    /// `failed` + `rolled_back`.
    pub failed: u32,
    pub rollback_failed: u32,
    pub unresponsive: u32,
    pub started_at: Timestamp,
}

/// The thresholds (plan 21 §5.6 defaults; per rollout).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HaltPolicy {
    min_sample: u32,
    failure_rate_bp: u32,
    min_failures: u32,
    unresponsive_rate_bp: u32,
    done_ratio_bp: u32,
    soak_canary_secs: i64,
    soak_percent_secs: i64,
    stall_after_secs: i64,
}

const DAY: i64 = 86_400;

impl Default for HaltPolicy {
    fn default() -> Self {
        HaltPolicy {
            min_sample: 3,
            failure_rate_bp: 200,
            min_failures: 3,
            unresponsive_rate_bp: 1_000,
            done_ratio_bp: 9_000,
            soak_canary_secs: DAY,
            soak_percent_secs: 4 * 3_600,
            stall_after_secs: 7 * DAY,
        }
    }
}

fn invalid(reason: &'static str) -> Problem {
    Problem::about(ErrorCode::EdgeRolloutPolicyInvalid, reason, reason)
}

/// A halt policy as stored or sent, before its ranges are checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HaltPolicySpec {
    pub min_sample: u32,
    pub failure_rate_bp: u32,
    pub min_failures: u32,
    pub unresponsive_rate_bp: u32,
    pub done_ratio_bp: u32,
    pub soak_canary_secs: i64,
    pub soak_percent_secs: i64,
    pub stall_after_secs: i64,
}

impl HaltPolicy {
    /// A policy from its stored values; each in its range or
    /// `FG-EDGE-0005` with the field as `reason`.
    pub fn new(spec: HaltPolicySpec) -> Result<HaltPolicy, Problem> {
        let bp = 1..=10_000;
        if !(1..=1_000).contains(&spec.min_sample) {
            return Err(invalid("min_sample"));
        }
        if !bp.contains(&spec.failure_rate_bp) {
            return Err(invalid("failure_rate"));
        }
        if !(1..=1_000).contains(&spec.min_failures) {
            return Err(invalid("min_failures"));
        }
        if !bp.contains(&spec.unresponsive_rate_bp) {
            return Err(invalid("unresponsive_rate"));
        }
        if !(100..=10_000).contains(&spec.done_ratio_bp) {
            return Err(invalid("done_ratio"));
        }
        let soak = 0..=30 * DAY;
        if !soak.contains(&spec.soak_canary_secs) || !soak.contains(&spec.soak_percent_secs) {
            return Err(invalid("soak"));
        }
        if !(3_600..=90 * DAY).contains(&spec.stall_after_secs) {
            return Err(invalid("stall_after"));
        }
        Ok(HaltPolicy {
            min_sample: spec.min_sample,
            failure_rate_bp: spec.failure_rate_bp,
            min_failures: spec.min_failures,
            unresponsive_rate_bp: spec.unresponsive_rate_bp,
            done_ratio_bp: spec.done_ratio_bp,
            soak_canary_secs: spec.soak_canary_secs,
            soak_percent_secs: spec.soak_percent_secs,
            stall_after_secs: spec.stall_after_secs,
        })
    }

    /// The values, to store (round-trips through [`HaltPolicy::new`]).
    pub fn spec(&self) -> HaltPolicySpec {
        HaltPolicySpec {
            min_sample: self.min_sample,
            failure_rate_bp: self.failure_rate_bp,
            min_failures: self.min_failures,
            unresponsive_rate_bp: self.unresponsive_rate_bp,
            done_ratio_bp: self.done_ratio_bp,
            soak_canary_secs: self.soak_canary_secs,
            soak_percent_secs: self.soak_percent_secs,
            stall_after_secs: self.stall_after_secs,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HaltReason {
    RollbackFailed,
    CanaryFailure,
    FailureRate,
    UnresponsiveRate,
}

impl HaltReason {
    pub fn as_str(self) -> &'static str {
        match self {
            HaltReason::RollbackFailed => "rollback_failed",
            HaltReason::CanaryFailure => "canary_failure",
            HaltReason::FailureRate => "failure_rate",
            HaltReason::UnresponsiveRate => "unresponsive_rate",
        }
    }
}

/// Why the rollout stopped, with the numbers the caller stores in
/// `halt_details` (E5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Halt {
    pub reason: HaltReason,
    pub attempted: u32,
    pub failed: u32,
    pub unresponsive: u32,
    pub rollback_failed: u32,
    /// `failed ÷ (succeeded + failed)` in basis points (0 with no result).
    pub failure_rate_bp: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitReason {
    NoMembers,
    BelowMinSample,
    InProgress,
    Soaking,
}

impl WaitReason {
    pub fn as_str(self) -> &'static str {
        match self {
            WaitReason::NoMembers => "no_members",
            WaitReason::BelowMinSample => "below_min_sample",
            WaitReason::InProgress => "in_progress",
            WaitReason::Soaking => "soaking",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Halt(Halt),
    Advance,
    /// `stalled`: the wave started `stall_after` ago and still cannot
    /// advance (the caller logs `RolloutStalled` once, E5).
    Wait {
        reason: WaitReason,
        stalled: bool,
    },
}

/// `a ÷ b > rate_bp ÷ 10000`, in integers.
fn over(a: u32, b: u32, rate_bp: u32) -> bool {
    u64::from(a) * 10_000 > u64::from(rate_bp) * u64::from(b)
}

/// The decision for one wave at `now` (module docs).
pub fn decide(stats: &WaveStats, kind: WaveKind, policy: &HaltPolicy, now: Timestamp) -> Decision {
    let finals = stats
        .succeeded
        .saturating_add(stats.failed)
        .saturating_add(stats.rollback_failed)
        .saturating_add(stats.unresponsive);
    let attempted = stats.attempted.max(finals);
    let members = stats.members.max(attempted);
    let results = stats.succeeded.saturating_add(stats.failed);
    let rate_bp = if results == 0 {
        0
    } else {
        u32::try_from(u64::from(stats.failed) * 10_000 / u64::from(results)).unwrap_or(u32::MAX)
    };
    let halt = |reason| {
        Decision::Halt(Halt {
            reason,
            attempted,
            failed: stats.failed,
            unresponsive: stats.unresponsive,
            rollback_failed: stats.rollback_failed,
            failure_rate_bp: rate_bp,
        })
    };
    // A clock behind the wave's start counts as no time passed.
    let elapsed = now.duration_since(stats.started_at).as_secs().max(0);
    let stalled = elapsed >= policy.stall_after_secs;

    if stats.rollback_failed > 0 {
        return halt(HaltReason::RollbackFailed);
    }
    if kind == WaveKind::Canary && stats.failed > 0 {
        return halt(HaltReason::CanaryFailure);
    }
    if members == 0 {
        return Decision::Wait {
            reason: WaitReason::NoMembers,
            stalled,
        };
    }
    if kind == WaveKind::Percent
        && stats.failed.saturating_add(stats.unresponsive) >= policy.min_failures
        && over(stats.failed, results, policy.failure_rate_bp)
    {
        return halt(HaltReason::FailureRate);
    }
    if attempted < policy.min_sample.min(members) {
        return Decision::Wait {
            reason: WaitReason::BelowMinSample,
            stalled,
        };
    }
    if over(stats.unresponsive, attempted, policy.unresponsive_rate_bp) {
        return halt(HaltReason::UnresponsiveRate);
    }
    let soak = match kind {
        WaveKind::Canary => policy.soak_canary_secs,
        WaveKind::Percent => policy.soak_percent_secs,
    };
    let done = u64::from(finals) * 10_000 >= u64::from(policy.done_ratio_bp) * u64::from(members);
    match (done, elapsed >= soak) {
        (true, true) => Decision::Advance,
        (true, false) => Decision::Wait {
            reason: WaitReason::Soaking,
            stalled,
        },
        (false, _) => Decision::Wait {
            reason: WaitReason::InProgress,
            stalled,
        },
    }
}
