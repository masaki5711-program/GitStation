#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! `forge/spec-drift`'s version order (M3 X2, ADR-0023 Amendment X2).

use super::{drift, Drift, SpecVersion};

fn tags(list: &[&'static str]) -> Vec<&'static str> {
    list.to_vec()
}

#[test]
fn a_version_is_a_prefix_and_up_to_four_numbers() {
    for ok in [
        "1",
        "v1",
        "v1.5",
        "V1.5.0",
        "spec-v2.0.10",
        "release_3.1.4.1",
    ] {
        assert!(SpecVersion::parse(ok).is_some(), "{ok}");
    }
    for no in [
        "",
        "v",
        "v1.",
        ".1",
        "v1..2",
        "v1.2.3.4.5",
        "v1.2-rc1",
        "v1.2+build",
        "latest",
        "2026-11-06",
        "v1.2a",
        "v01x",
        "v1.12345678901",
        "x/v1.2",
    ] {
        assert!(SpecVersion::parse(no).is_none(), "{no}");
    }
}

#[test]
fn numbers_compare_as_numbers_and_missing_parts_are_zero() {
    let v = |s| SpecVersion::parse(s).expect(s);
    assert!(v("v1.10") > v("v1.9"));
    assert!(v("v2") > v("v1.99.99"));
    assert!(v("v1.5.1") > v("v1.5"));
    assert_eq!(v("v1.5").cmp(&v("v1.5.0")), std::cmp::Ordering::Equal);
}

#[test]
fn the_pin_is_current_when_no_later_version_of_its_series_exists() {
    assert_eq!(
        drift("v1.5", tags(&["v1.4", "v1.5", "v1.5.0"])),
        Drift::Current
    );
    // Another series (another prefix) and prerelease tags do not count.
    assert_eq!(
        drift("v1.5", tags(&["v1.5", "spec-v9.0", "v2.0-rc1", "nightly"])),
        Drift::Current
    );
}

#[test]
fn a_later_version_of_the_series_is_behind_with_the_latest_named() {
    assert_eq!(
        drift("v1.5", tags(&["v1.4", "v1.5", "v1.10", "v1.6", "v2.0-rc1"])),
        Drift::Behind {
            latest: "v1.10".to_owned(),
            newer: 2
        }
    );
    // Prefixes compare exactly (`V` is another series than `v`).
    assert_eq!(
        drift("spec-v1.0", tags(&["spec-v1.0", "spec-v1.1", "v7"])),
        Drift::Behind {
            latest: "spec-v1.1".to_owned(),
            newer: 1
        }
    );
}

#[test]
fn equal_versions_spelled_twice_pick_one_deterministically() {
    let a = drift("v1.0", tags(&["v1.0", "v2.0.0", "v2.0"]));
    let b = drift("v1.0", tags(&["v2.0", "v1.0", "v2.0.0"]));
    assert_eq!(a, b);
    assert_eq!(
        a,
        Drift::Behind {
            latest: "v2.0.0".to_owned(),
            newer: 2
        }
    );
}

#[test]
fn a_pin_that_is_no_version_is_unversioned() {
    assert_eq!(
        drift("stable", tags(&["stable", "v1.0"])),
        Drift::Unversioned
    );
    assert_eq!(drift("v1.0-rc1", tags(&["v1.0"])), Drift::Unversioned);
}
