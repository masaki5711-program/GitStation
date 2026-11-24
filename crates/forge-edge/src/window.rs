//! Install windows (plan 21 §5.4, ADR-0041 §6): when a device may install.
//!
//! A group's [`Schedule`] is weekly windows in one IANA time zone (weekday,
//! local start `HH:MM`, length in minutes, 1..=1440) minus blackouts
//! (absolute instants, like the deploy freezes). [`install_window`] answers
//! the window that contains `now`, or the next one within 8 days.
//!
//! Wall-clock rules: a window is from its local start to its local end
//! (start + length on the civil clock — a 23:00 + 240 min window ends at
//! 03:00 on the next day's clock, whatever DST did in between). Each end is
//! resolved to an instant with the "compatible" rule: a time in a DST gap
//! moves forward by the gap, a time in a fold takes the earlier instant. So
//! a window never lasts longer than its wall-clock length except across a
//! fold (the clock really shows those hours twice), and a window whose
//! start moved past its end (a start inside the gap, shorter than the
//! shift) is empty that day — the safe side: the line's stop is planned on
//! the wall clock. Overlapping or touching windows merge.
//!
//! Nothing here reads a clock or a file: the tz database is compiled in, so
//! the server, gstn and the device agree on every instant. An instant out of
//! jiff's range yields no window — never "install now".

use forge_errors::ErrorCode;
use jiff::civil::{Date, Time, Weekday};
use jiff::tz::TimeZone;
use jiff::{SignedDuration, Timestamp};

use crate::Problem;

const MAX_WINDOWS: usize = 64;
const MAX_BLACKOUTS: usize = 256;
/// Days looked at from the day before `now`'s local date.
const DAYS: usize = 10;

fn invalid(reason: &'static str, subject: &str) -> Problem {
    Problem::about(ErrorCode::EdgeRolloutPolicyInvalid, reason, subject)
}

/// One weekly window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WeeklyWindow {
    weekday: Weekday,
    start: Time,
    minutes: u16,
}

impl WeeklyWindow {
    /// `start` is `HH:MM` (24-hour), `minutes` 1..=1440.
    pub fn new(weekday: Weekday, start: &str, minutes: u16) -> Result<WeeklyWindow, Problem> {
        let start_time = parse_hh_mm(start).ok_or_else(|| invalid("window_start", start))?;
        if !(1..=1440).contains(&minutes) {
            return Err(invalid("window_duration", start));
        }
        Ok(WeeklyWindow {
            weekday,
            start: start_time,
            minutes,
        })
    }

    pub fn weekday(&self) -> Weekday {
        self.weekday
    }

    pub fn start(&self) -> Time {
        self.start
    }

    pub fn minutes(&self) -> u16 {
        self.minutes
    }
}

fn parse_hh_mm(s: &str) -> Option<Time> {
    let &[h1, h2, b':', m1, m2] = s.as_bytes() else {
        return None;
    };
    let num = |hi: u8, lo: u8| {
        (hi.is_ascii_digit() && lo.is_ascii_digit())
            .then(|| i8::try_from((hi - b'0') * 10 + (lo - b'0')).ok())
            .flatten()
    };
    Time::new(num(h1, h2)?, num(m1, m2)?, 0, 0).ok()
}

/// No installs from `from` (inclusive) to `until` (exclusive).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Blackout {
    from: Timestamp,
    until: Timestamp,
}

impl Blackout {
    pub fn new(from: Timestamp, until: Timestamp) -> Result<Blackout, Problem> {
        if until <= from {
            return Err(invalid("blackout_order", &from.to_string()));
        }
        Ok(Blackout { from, until })
    }
}

/// A group's maintenance schedule.
#[derive(Debug, Clone)]
pub struct Schedule {
    tz: TimeZone,
    tz_name: String,
    windows: Vec<WeeklyWindow>,
    blackouts: Vec<Blackout>,
}

impl Schedule {
    /// `tz_name` is an IANA zone (`Asia/Jakarta`, `Europe/Berlin`, `UTC`).
    pub fn new(
        tz_name: &str,
        windows: Vec<WeeklyWindow>,
        blackouts: Vec<Blackout>,
    ) -> Result<Schedule, Problem> {
        if tz_name.is_empty() || tz_name.len() > 64 {
            return Err(invalid("time_zone", "time_zone"));
        }
        let tz = TimeZone::get(tz_name).map_err(|_| invalid("time_zone", tz_name))?;
        if windows.len() > MAX_WINDOWS {
            return Err(invalid("too_many_windows", "windows"));
        }
        if blackouts.len() > MAX_BLACKOUTS {
            return Err(invalid("too_many_blackouts", "blackouts"));
        }
        Ok(Schedule {
            tz,
            tz_name: tz_name.to_owned(),
            windows,
            blackouts,
        })
    }

    pub fn time_zone(&self) -> &str {
        &self.tz_name
    }
}

/// An install window as the desired-state answer carries it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InstallWindow {
    pub not_before: Timestamp,
    pub not_after: Timestamp,
}

/// The instant of a local date and time (module docs: "compatible").
fn instant(tz: &TimeZone, date: Date, time: Time, plus_minutes: u16) -> Option<Timestamp> {
    let start = date.to_datetime(time);
    let local = start
        .checked_add(SignedDuration::from_secs(i64::from(plus_minutes) * 60))
        .ok()?;
    tz.to_ambiguous_timestamp(local).compatible().ok()
}

/// `[from, until)` minus every blackout.
fn subtract(
    from: Timestamp,
    until: Timestamp,
    blackouts: &[Blackout],
) -> Vec<(Timestamp, Timestamp)> {
    let mut pieces = vec![(from, until)];
    for b in blackouts {
        let mut next = Vec::with_capacity(pieces.len() + 1);
        for (s, e) in pieces {
            if b.until <= s || b.from >= e {
                next.push((s, e));
                continue;
            }
            if s < b.from {
                next.push((s, b.from));
            }
            if b.until < e {
                next.push((b.until, e));
            }
        }
        pieces = next;
    }
    pieces
}

/// The window containing `now`, else the next to start within 8 days;
/// `None` when there is none (no weekly windows, all blacked out).
pub fn install_window(schedule: &Schedule, now: Timestamp) -> Option<InstallWindow> {
    // clone: TimeZone is reference-counted; to_zoned takes it by value.
    let today = now.to_zoned(schedule.tz.clone()).date();
    let mut day = today.yesterday().unwrap_or(today);
    let mut spans = Vec::new();
    for _ in 0..DAYS {
        for w in schedule
            .windows
            .iter()
            .filter(|w| w.weekday == day.weekday())
        {
            let (Some(s), Some(e)) = (
                instant(&schedule.tz, day, w.start, 0),
                instant(&schedule.tz, day, w.start, w.minutes),
            ) else {
                continue;
            };
            if e > s {
                spans.extend(subtract(s, e, &schedule.blackouts));
            }
        }
        match day.tomorrow() {
            Ok(d) => day = d,
            Err(_) => break,
        }
    }
    spans.sort_unstable();
    let mut merged: Vec<(Timestamp, Timestamp)> = Vec::with_capacity(spans.len());
    for (s, e) in spans {
        match merged.last_mut() {
            Some(last) if s <= last.1 => last.1 = last.1.max(e),
            Some(_) | None => merged.push((s, e)),
        }
    }
    merged
        .into_iter()
        .find(|(_, e)| *e > now)
        .map(|(not_before, not_after)| InstallWindow {
            not_before,
            not_after,
        })
}

/// Whether an install of at most `max_install_seconds` may start at `now`:
/// inside the window, with at least that long left (plan 21 §5.4-3; the
/// `can-update` hook is asked after this, on the device).
pub fn may_start_install(window: &InstallWindow, now: Timestamp, max_install_seconds: u32) -> bool {
    now >= window.not_before
        && now < window.not_after
        && window.not_after.duration_since(now).as_secs() >= i64::from(max_install_seconds)
}
