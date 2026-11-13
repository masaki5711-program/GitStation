#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)] // tests may fail loudly

use super::*;

/// 2026-11-13 00:00 UTC, a Friday.
const FRI: i64 = 1_794_528_000;
const HOUR: i64 = 3600;
const DAY: i64 = 86_400;
const JST: i32 = 540;

fn next(rule: &str, after: i64, offset: i32) -> Option<i64> {
    Rule::parse(rule).expect(rule).next_after(after, offset)
}

#[test]
fn a_daily_rule_runs_at_its_local_time() {
    // 02:00 JST on Saturday is 17:00 UTC on Friday.
    assert_eq!(next("0 2 * * *", FRI, JST), Some(FRI + 17 * HOUR));
    assert_eq!(next("0 2 * * *", FRI, 0), Some(FRI + 2 * HOUR));
    // WIB (UTC+7).
    assert_eq!(
        next("30 6 * * *", FRI, 420),
        Some(FRI + 23 * HOUR + 30 * 60)
    );
}

#[test]
fn the_next_run_is_strictly_after() {
    assert_eq!(next("0 0 * * *", FRI, 0), Some(FRI + DAY));
    assert_eq!(next("0 0 * * *", FRI - 1, 0), Some(FRI));
    assert_eq!(next("*/15 * * * *", FRI, 0), Some(FRI + 15 * 60));
    assert_eq!(next("*/15 * * * *", FRI + 1, 0), Some(FRI + 15 * 60));
}

#[test]
fn weekdays_count_from_sunday_and_seven_is_sunday() {
    // Monday 2026-11-16.
    assert_eq!(next("0 0 * * 1", FRI, 0), Some(FRI + 3 * DAY));
    assert_eq!(next("0 0 * * 0", FRI, 0), Some(FRI + 2 * DAY));
    assert_eq!(next("0 0 * * 7", FRI, 0), Some(FRI + 2 * DAY));
    assert_eq!(next("0 9 * * 1-5", FRI, 0), Some(FRI + 9 * HOUR));
}

#[test]
fn a_restricted_day_and_weekday_match_either() {
    // The 1st of the month or a Monday: Monday 16th comes first.
    assert_eq!(next("0 0 1 * 1", FRI, 0), Some(FRI + 3 * DAY));
    // Only the 1st: 2026-12-01.
    assert_eq!(next("0 0 1 * *", FRI, 0), Some(1_796_083_200));
}

#[test]
fn a_leap_day_is_found_and_an_impossible_day_is_none() {
    assert_eq!(next("0 0 29 2 *", FRI, 0), Some(1_835_395_200));
    assert_eq!(next("0 0 31 2 *", FRI, 0), None);
}

#[test]
fn lists_ranges_and_steps_combine() {
    assert_eq!(next("5/20 * * * *", FRI, 0), Some(FRI + 5 * 60));
    assert_eq!(
        next("10-50/20 3,4 * * *", FRI, 0),
        Some(FRI + 3 * HOUR + 10 * 60)
    );
    assert_eq!(next("0 0 * 12 *", FRI, 0), Some(1_796_083_200));
}

#[test]
fn malformed_rules_name_their_reason() {
    for (rule, reason) in [
        ("0 0 * *", "cron_field_count"),
        ("0 0 * * * *", "cron_field_count"),
        ("", "cron_field_count"),
        ("60 * * * *", "cron_value_out_of_range"),
        ("0 24 * * *", "cron_value_out_of_range"),
        ("0 0 0 * *", "cron_value_out_of_range"),
        ("0 0 * 13 *", "cron_value_out_of_range"),
        ("0 0 * * 8", "cron_value_out_of_range"),
        ("0 0 * * 5-1", "cron_value_out_of_range"),
        ("a * * * *", "cron_field_invalid"),
        ("*/0 * * * *", "cron_field_invalid"),
        ("0 0 * * MON", "cron_field_invalid"),
        ("0 0 L * *", "cron_field_invalid"),
        ("0\t0 * * *", "cron_field_invalid"),
        ("-1 * * * *", "cron_field_invalid"),
    ] {
        assert_eq!(Rule::parse(rule).expect_err(rule), reason, "{rule:?}");
    }
    let long = format!("0 0 * * {}", "1,".repeat(60));
    assert_eq!(Rule::parse(&long).expect_err("long"), "cron_too_long");
}

#[test]
fn a_rule_more_often_than_every_five_minutes_is_too_frequent() {
    for rule in [
        "* * * * *",
        "*/2 * * * *",
        "0,3 * * * *",
        "0,58 * * * *",
        "*/4 0 * * *",
    ] {
        assert_eq!(Rule::parse(rule).expect_err(rule), "too_frequent", "{rule}");
    }
    for rule in ["*/5 * * * *", "0 * * * *", "0,30 * * * *", "57 * * * *"] {
        assert!(Rule::parse(rule).is_ok(), "{rule}");
    }
}

#[test]
fn offsets_are_quarter_hours_within_the_world() {
    for ok in [0, 540, 420, -300, 345, -720, 840] {
        assert!(check_offset(ok).is_ok(), "{ok}");
    }
    for bad in [541, -721, 841, 10] {
        assert_eq!(check_offset(bad), Err("utc_offset_invalid"), "{bad}");
    }
}

#[test]
fn the_civil_calendar_matches_known_days() {
    assert_eq!(civil_from_days(0), (1970, 1, 1));
    assert_eq!(civil_from_days(FRI / DAY), (2026, 11, 13));
    assert_eq!(civil_from_days(1_835_395_200 / DAY), (2028, 2, 29));
}
