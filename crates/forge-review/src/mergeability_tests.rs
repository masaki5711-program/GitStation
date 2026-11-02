#![allow(clippy::expect_used, clippy::unwrap_used)] // tests may fail loudly

use super::*;

#[test]
fn stored_paths_are_capped_by_count_and_bytes_never_cut_inside_a_path() {
    let many: Vec<String> = (0..150).map(|n| format!("f{n}")).collect();
    let (kept, cut) = stored_files(&many);
    assert_eq!(kept.len(), MAX_STORED_FILES);
    assert!(cut);
    let big: Vec<String> = (0..3)
        .map(|n| format!("{n}{}", "x".repeat(8_000)))
        .collect();
    let (kept, cut) = stored_files(&big);
    assert_eq!(kept.len(), 2, "the third would pass 16 KiB");
    assert!(cut);
    let few = vec!["a".to_owned(), "b".to_owned()];
    assert_eq!(stored_files(&few), (few.clone(), false));
}

#[test]
fn git_failures_map_to_their_stored_reason() {
    for (code, reason) in [
        (ErrorCode::GitTimeout, CheckReason::GitTimeout),
        (ErrorCode::GitBusy, CheckReason::GitBusy),
        (ErrorCode::GitProcessFailed, CheckReason::GitFailed),
        (ErrorCode::GitUnavailable, CheckReason::GitFailed),
        (ErrorCode::StorageIoFailed, CheckReason::GitFailed),
    ] {
        assert_eq!(git_reason(&ForgeError::new(code)), reason, "{code:?}");
    }
}
