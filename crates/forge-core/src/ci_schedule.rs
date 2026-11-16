//! The time rule of a scheduled CI pipeline (plan 14 C12): five-field cron
//! (`minute hour day-of-month month day-of-week`) at a fixed UTC offset.
//! Pure: no clock (the caller passes the database's `now()` as Unix
//! seconds), no time zone database (the sites are in Japan and Indonesia,
//! which keep no daylight saving time: a fixed offset is exact there).
//!
//! Each field is `*`, a number, a range `a-b`, a step `*/n` or `a-b/n`, or
//! a comma list of those. Day-of-week is `0`–`7` (both 0 and 7 are
//! Sunday). No names (`MON`, `JAN`) and no `L` / `W` / `#` / `?`: one
//! spelling per rule keeps the rule readable in the audit log.
//!
//! Like cron, when both day-of-month and day-of-week are restricted (not
//! `*`), a day matches if **either** matches.
//!
//! A rule may fire at most once every [`MIN_INTERVAL_MINUTES`] minutes
//! (`too_frequent`): a schedule is a job for the runners, not a poll.
//! Every refusal is a stable `reason` for `FG-WEB-0006 param=cron`.

/// The longest rule text.
pub const MAX_RULE_BYTES: usize = 100;
/// The shortest gap between two runs.
pub const MIN_INTERVAL_MINUTES: u32 = 5;
/// The UTC offset bounds in minutes (UTC−12:00 … UTC+14:00).
pub const MIN_OFFSET_MINUTES: i32 = -720;
pub const MAX_OFFSET_MINUTES: i32 = 840;
/// How far [`Rule::next_after`] looks ahead before giving up (a rule such
/// as `0 0 31 2 *` never fires): a little over four years, so 29 February
/// is found.
const HORIZON_DAYS: i64 = 4 * 366 + 1;

/// A parsed five-field rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    minutes: u64,
    hours: u32,
    days: u32,
    months: u16,
    weekdays: u8,
    days_any: bool,
    weekdays_any: bool,
}

/// The bounds of one field.
struct Field {
    lo: u32,
    hi: u32,
}

const MINUTE: Field = Field { lo: 0, hi: 59 };
const HOUR: Field = Field { lo: 0, hi: 23 };
const DAY: Field = Field { lo: 1, hi: 31 };
const MONTH: Field = Field { lo: 1, hi: 12 };
const WEEKDAY: Field = Field { lo: 0, hi: 7 };

fn number(s: &str) -> Result<u32, &'static str> {
    if s.is_empty() || s.len() > 2 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err("cron_field_invalid");
    }
    s.parse().map_err(|_| "cron_field_invalid")
}

/// The values of one field as a bit set (bit `v` set: `v` matches), and
/// whether it was `*`.
fn field(text: &str, f: &Field) -> Result<(u64, bool), &'static str> {
    if text == "*" {
        let mut set = 0u64;
        for v in f.lo..=f.hi {
            set |= 1 << v;
        }
        return Ok((set, true));
    }
    let mut set = 0u64;
    for part in text.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((r, s)) => {
                let step = number(s)?;
                if step == 0 {
                    return Err("cron_field_invalid");
                }
                (r, step)
            }
            None => (part, 1),
        };
        let (lo, hi) = if range == "*" {
            (f.lo, f.hi)
        } else if let Some((a, b)) = range.split_once('-') {
            (number(a)?, number(b)?)
        } else {
            let v = number(range)?;
            // `5/15` means 5, 20, 35, 50 like `5-59/15`.
            if part.contains('/') {
                (v, f.hi)
            } else {
                (v, v)
            }
        };
        if lo < f.lo || hi > f.hi || lo > hi {
            return Err("cron_value_out_of_range");
        }
        let mut v = lo;
        while v <= hi {
            set |= 1 << v;
            v += step;
        }
    }
    Ok((set, false))
}

impl Rule {
    /// Parse `text` (fields separated by single or repeated spaces).
    pub fn parse(text: &str) -> Result<Self, &'static str> {
        if text.len() > MAX_RULE_BYTES {
            return Err("cron_too_long");
        }
        if !text.bytes().all(|b| b == b' ' || b.is_ascii_graphic()) {
            return Err("cron_field_invalid");
        }
        let parts: Vec<&str> = text.split(' ').filter(|p| !p.is_empty()).collect();
        let [mi, ho, dm, mo, dw] = parts.as_slice() else {
            return Err("cron_field_count");
        };
        let (minutes, _) = field(mi, &MINUTE)?;
        let (hours, _) = field(ho, &HOUR)?;
        let (days, days_any) = field(dm, &DAY)?;
        let (months, _) = field(mo, &MONTH)?;
        let (mut weekdays, weekdays_any) = field(dw, &WEEKDAY)?;
        // 7 is Sunday too.
        if weekdays & (1 << 7) != 0 {
            weekdays |= 1;
        }
        let rule = Self {
            minutes,
            hours: u32::try_from(hours).map_err(|_| "cron_value_out_of_range")?,
            days: u32::try_from(days).map_err(|_| "cron_value_out_of_range")?,
            months: u16::try_from(months).map_err(|_| "cron_value_out_of_range")?,
            weekdays: u8::try_from(weekdays & 0x7f).map_err(|_| "cron_value_out_of_range")?,
            days_any,
            weekdays_any,
        };
        if rule.shortest_gap_minutes() < MIN_INTERVAL_MINUTES {
            return Err("too_frequent");
        }
        Ok(rule)
    }

    /// The shortest gap between two matching minutes of one hour, the step
    /// over the hour's end included (an every-minute hour matters; a rule
    /// that fires in one minute of the hour has a gap of 60).
    fn shortest_gap_minutes(&self) -> u32 {
        let set: Vec<u32> = (0..60).filter(|m| self.minutes & (1 << m) != 0).collect();
        let (Some(first), Some(last)) = (set.first(), set.last()) else {
            return 60;
        };
        let mut gap = 60 - last + first;
        for pair in set.windows(2) {
            if let [a, b] = pair {
                gap = gap.min(b - a);
            }
        }
        gap
    }

    fn day_matches(&self, day: u32, weekday: u32) -> bool {
        let dom = self.days & (1 << day) != 0;
        let dow = self.weekdays & (1 << weekday) != 0;
        match (self.days_any, self.weekdays_any) {
            (true, true) => true,
            (true, false) => dow,
            (false, true) => dom,
            (false, false) => dom || dow,
        }
    }

    /// The first run strictly after `after` (Unix seconds, UTC) at the
    /// fixed `offset_minutes`, as Unix seconds on a whole minute; `None`
    /// when the rule matches no day in the next four years (`0 0 30 2 *`).
    pub fn next_after(&self, after: i64, offset_minutes: i32) -> Option<i64> {
        let offset = i64::from(offset_minutes) * 60;
        // The local minute after `after`.
        let local = after.checked_add(offset)?;
        let start_minute = local.div_euclid(60) + 1;
        let mut day = start_minute.div_euclid(1440);
        let mut minute_of_day = start_minute.rem_euclid(1440);
        let last_day = day + HORIZON_DAYS;
        while day <= last_day {
            let (_, month, dom) = civil_from_days(day);
            // 1970-01-01 was a Thursday (4).
            let weekday = u32::try_from((day + 4).rem_euclid(7)).ok()?;
            if self.months & (1 << month) != 0 && self.day_matches(dom, weekday) {
                let mut m = minute_of_day;
                while m < 1440 {
                    let hour = u32::try_from(m / 60).ok()?;
                    let min = u32::try_from(m % 60).ok()?;
                    if self.hours & (1 << hour) == 0 {
                        m = (m / 60 + 1) * 60;
                        continue;
                    }
                    if self.minutes & (1 << min) != 0 {
                        let local_secs = (day * 1440 + m) * 60;
                        return local_secs.checked_sub(offset);
                    }
                    m += 1;
                }
            }
            day += 1;
            minute_of_day = 0;
        }
        None
    }
}

/// Validate a UTC offset in minutes (`utc_offset_invalid`): within
/// UTC−12:00 … UTC+14:00 and on a quarter hour.
pub fn check_offset(offset_minutes: i32) -> Result<(), &'static str> {
    if !(MIN_OFFSET_MINUTES..=MAX_OFFSET_MINUTES).contains(&offset_minutes)
        || offset_minutes % 15 != 0
    {
        return Err("utc_offset_invalid");
    }
    Ok(())
}

/// (year, month 1..=12, day 1..=31) of a day number since 1970-01-01
/// (Howard Hinnant's `civil_from_days`).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    // d in 1..=31 and m in 1..=12 by construction.
    (
        year,
        u32::try_from(m).unwrap_or(1),
        u32::try_from(d).unwrap_or(1),
    )
}

#[cfg(test)]
#[path = "ci_schedule_tests.rs"]
mod tests;
