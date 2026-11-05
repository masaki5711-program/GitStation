#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! `assess`: the run for what the head's `.forge/repo.yml` came to (M3 X2).

use forge_core::checks::{CheckAnnotationLevel, CheckConclusion, CheckStatus};
use forge_core::repo_config::RepoConfigProblem;

use super::{assess, Assessed, EntryResult, EntryState};

fn entry(
    line: usize,
    written: &str,
    pin: Option<(&'static str, &str)>,
    state: EntryState,
) -> EntryResult {
    EntryResult {
        line,
        written: written.to_owned(),
        pin: pin.map(|(k, n)| (k, n.to_owned())),
        state,
    }
}

#[test]
fn no_repo_yml_is_success_with_nothing_pinned() {
    let run = assess(&Assessed::Absent).expect("run");
    assert_eq!(run.name.as_str(), "forge/spec-drift");
    assert_eq!(run.status, CheckStatus::Completed);
    assert_eq!(run.conclusion, Some(CheckConclusion::Success));
    assert!(run.annotations.is_empty());
    assert!(
        run.summary.contains("no specification is pinned"),
        "{}",
        run.summary
    );
}

#[test]
fn a_refused_repo_yml_is_neutral_at_its_line() {
    let run = assess(&Assessed::Invalid(RepoConfigProblem {
        line: 4,
        reason: "unknown_key",
    }))
    .expect("run");
    assert_eq!(run.conclusion, Some(CheckConclusion::Neutral));
    assert!(run.summary.contains("FG-REPO-0024"), "{}", run.summary);
    assert_eq!(run.annotations.len(), 1);
    let a = &run.annotations[0];
    assert_eq!((a.path.as_str(), a.start_line), (".forge/repo.yml", 4));
    // The whole file (line 0) is annotated on its first line.
    let whole = assess(&Assessed::Invalid(RepoConfigProblem {
        line: 0,
        reason: "too_large",
    }))
    .expect("run");
    assert_eq!(whole.annotations[0].start_line, 1);
}

#[test]
fn current_tracking_local_and_unversioned_entries_are_success_with_a_notice_for_odd_pins() {
    let run = assess(&Assessed::Entries(vec![
        entry(
            2,
            "platform/specs",
            Some(("version", "v1.2")),
            EntryState::Current,
        ),
        entry(
            5,
            "platform/api",
            Some(("track", "main")),
            EntryState::Tracking,
        ),
        entry(
            8,
            "platform/legacy",
            Some(("version", "stable")),
            EntryState::Unversioned,
        ),
        entry(10, "spec/", None, EntryState::Local),
    ]))
    .expect("run");
    assert_eq!(run.conclusion, Some(CheckConclusion::Success));
    // Review X2 (LOW): a pin that is no version is said, at notice level.
    let notes: Vec<(u32, CheckAnnotationLevel)> = run
        .annotations
        .iter()
        .map(|a| (a.start_line, a.level))
        .collect();
    assert_eq!(notes, [(8, CheckAnnotationLevel::Notice)]);
    assert!(run.summary.contains("(3 pinned)"), "{}", run.summary);
    assert!(
        run.summary
            .contains("line 8: platform/legacy version stable"),
        "{}",
        run.summary
    );
}

#[test]
fn each_notable_entry_makes_the_run_neutral_and_is_annotated() {
    let run = assess(&Assessed::Entries(vec![
        entry(
            2,
            "platform/specs",
            Some(("version", "v1.2")),
            EntryState::Behind,
        ),
        entry(
            5,
            "platform/gone",
            Some(("version", "v1")),
            EntryState::Unavailable,
        ),
        entry(
            7,
            "platform/api",
            Some(("version", "v9")),
            EntryState::VersionNotFound,
        ),
        entry(
            9,
            "platform/huge",
            Some(("version", "v1")),
            EntryState::Undecided,
        ),
        entry(
            11,
            "platform/ok",
            Some(("version", "v3")),
            EntryState::Current,
        ),
    ]))
    .expect("run");
    assert_eq!(run.conclusion, Some(CheckConclusion::Neutral));
    assert!(run.summary.starts_with("4 of 5 pinned"), "{}", run.summary);
    for needle in [
        "line 2: platform/specs version v1.2 — behind: a later version is published [FG-REPO-0027]",
        "[FG-REPO-0026]",
        "[FG-REPO-0025]",
    ] {
        assert!(run.summary.contains(needle), "{needle} in {}", run.summary);
    }
    let lines: Vec<(u32, CheckAnnotationLevel)> = run
        .annotations
        .iter()
        .map(|a| (a.start_line, a.level))
        .collect();
    assert_eq!(
        lines,
        [
            (2, CheckAnnotationLevel::Warning),
            (5, CheckAnnotationLevel::Notice),
            (7, CheckAnnotationLevel::Warning),
            (9, CheckAnnotationLevel::Notice),
        ]
    );
}

/// The largest file (32 entries of the longest text) stays inside the
/// run's limits: the summary and each message are cut, never refused.
#[test]
fn the_largest_file_fits_the_run_limits() {
    let long = "p/".repeat(forge_core::repo_config::MAX_SPEC_PATH_LEN / 2);
    let entries = (0..forge_core::repo_config::MAX_SPEC_REFS)
        .map(|i| entry(i + 1, &long, Some(("version", "v1")), EntryState::Behind))
        .collect();
    let run = assess(&Assessed::Entries(entries)).expect("run");
    assert!(run.annotations.len() <= forge_core::checks::MAX_CHECK_ANNOTATIONS);
    assert!(run.summary.len() <= forge_core::checks::MAX_CHECK_SUMMARY_BYTES);
    assert!(run.summary.ends_with('…'), "a cut summary says so");
    assert!(run.annotations.iter().all(|a| a.message.len() <= 4096));
}
