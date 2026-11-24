//! Waves (plan 21 §5.3, ADR-0041 §5): which devices of a group a rollout
//! reaches at each step.
//!
//! - A canary wave (only first) holds the group's devices labelled
//!   `edge.canary=true`.
//! - A percentage wave holds the devices whose bucket is below the
//!   percentage: `bucket = u32_be(SHA-256("forge-edge-wave-v1\n" ‖
//!   field(rollout_id) ‖ field(device_id))[0..4]) mod 10000`, in basis
//!   points (`field` = u32 big-endian length ‖ bytes, so the two ids cannot
//!   run into each other).
//! - Membership is **monotonic**: a device's first wave is the earliest
//!   that holds it, and it is a member of every later wave (the plan's
//!   percentages strictly increase and the last is 100 %). The bucket
//!   depends only on the rollout and the device, so re-computing never
//!   moves a device between waves.

use forge_errors::ErrorCode;

use crate::{sha256, Problem};

const BUCKET_DOMAIN: &[u8] = b"forge-edge-wave-v1\n";
const MAX_WAVES: usize = 10;

/// A share of devices in basis points: 1..=10000 (0.01 %..=100 %).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BasisPoints(u16);

impl BasisPoints {
    pub const ALL: BasisPoints = BasisPoints(10_000);

    pub fn new(bp: u16) -> Option<BasisPoints> {
        (1..=10_000).contains(&bp).then_some(BasisPoints(bp))
    }

    pub fn get(self) -> u16 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wave {
    Canary,
    Percent(BasisPoints),
}

/// What kind of wave a decision is about ([`crate::decide`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaveKind {
    Canary,
    Percent,
}

impl Wave {
    pub fn kind(self) -> WaveKind {
        match self {
            Wave::Canary => WaveKind::Canary,
            Wave::Percent(_) => WaveKind::Percent,
        }
    }
}

/// A validated wave plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WavePlan(Vec<Wave>);

fn invalid(reason: &'static str, subject: String) -> Problem {
    Problem::new(ErrorCode::EdgeRolloutPolicyInvalid, reason, Some(subject))
}

impl WavePlan {
    /// The default `[canary, 5 %, 25 %, 100 %]` (plan 21 §5.3).
    pub fn standard() -> WavePlan {
        WavePlan(vec![
            Wave::Canary,
            Wave::Percent(BasisPoints(500)),
            Wave::Percent(BasisPoints(2_500)),
            Wave::Percent(BasisPoints::ALL),
        ])
    }

    /// A plan: 1..=10 waves, a canary wave only first, percentages strictly
    /// increasing, the last 100 % (so every device has a wave).
    pub fn new(waves: Vec<Wave>) -> Result<WavePlan, Problem> {
        if waves.is_empty() {
            return Err(invalid("no_waves", "waves".to_owned()));
        }
        if waves.len() > MAX_WAVES {
            return Err(invalid("too_many_waves", "waves".to_owned()));
        }
        let mut last: Option<BasisPoints> = None;
        for (i, w) in waves.iter().enumerate() {
            match w {
                Wave::Canary if i > 0 => {
                    return Err(invalid("canary_not_first", format!("wave {i}")));
                }
                Wave::Canary => {}
                Wave::Percent(bp) => {
                    if last.is_some_and(|l| *bp <= l) {
                        return Err(invalid("percent_not_increasing", format!("wave {i}")));
                    }
                    last = Some(*bp);
                }
            }
        }
        if waves.last() != Some(&Wave::Percent(BasisPoints::ALL)) {
            return Err(invalid("last_wave_not_100", "waves".to_owned()));
        }
        Ok(WavePlan(waves))
    }

    /// A plan from percentages in basis points (`None` = the canary wave),
    /// as stored or sent: a value outside 1..=10000 is `percent_range`.
    pub fn from_basis_points(raw: &[Option<u32>]) -> Result<WavePlan, Problem> {
        let mut waves = Vec::with_capacity(raw.len());
        for (i, r) in raw.iter().enumerate() {
            waves.push(match r {
                None => Wave::Canary,
                Some(bp) => Wave::Percent(
                    u16::try_from(*bp)
                        .ok()
                        .and_then(BasisPoints::new)
                        .ok_or_else(|| invalid("percent_range", format!("wave {i}")))?,
                ),
            });
        }
        WavePlan::new(waves)
    }

    pub fn waves(&self) -> &[Wave] {
        &self.0
    }
}

fn field(out: &mut Vec<u8>, bytes: &[u8]) {
    let len = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(bytes);
}

/// The device's bucket in this rollout, 0..10000 (module docs).
pub fn wave_bucket(rollout_id: &str, device_id: &str) -> u16 {
    let mut msg = Vec::with_capacity(BUCKET_DOMAIN.len() + 8 + rollout_id.len() + device_id.len());
    msg.extend_from_slice(BUCKET_DOMAIN);
    field(&mut msg, rollout_id.as_bytes());
    field(&mut msg, device_id.as_bytes());
    let h = sha256(&msg);
    let n = u32::from_be_bytes([h[0], h[1], h[2], h[3]]);
    // < 10000, so it always fits.
    u16::try_from(n % 10_000).unwrap_or(0)
}

/// The index of the first wave of `plan` that holds the device.
pub fn first_wave(plan: &WavePlan, canary: bool, bucket: u16) -> usize {
    let last = plan.0.len().saturating_sub(1);
    plan.0
        .iter()
        .position(|w| match w {
            Wave::Canary => canary,
            Wave::Percent(bp) => bucket < bp.get(),
        })
        // A validated plan ends at 100 %, which holds every bucket.
        .unwrap_or(last)
}

/// Whether the device is a member of wave `k` (monotonic in `k`).
pub fn in_wave(plan: &WavePlan, k: usize, canary: bool, bucket: u16) -> bool {
    first_wave(plan, canary, bucket) <= k
}
