#![allow(clippy::expect_used, clippy::unwrap_used)] // tests may fail loudly

use super::*;

/// The default bounds keep one scan's events under a change's cap (two per
/// tried pair) and its git time under the job's limit.
#[test]
fn the_default_bounds_fit_the_job() {
    let l = ScanLimits::default();
    assert!(l.pairs_max * 2 <= forge_db::change::MAX_EVENTS);
    assert!(l.pairs_max <= l.peers_max);
    assert!(l.budget + Duration::from_secs(30) <= forge_db::review::CONFLICT_SCAN_TIMEOUT);
    assert!(l.merge_tree.timeout <= l.budget);
}

fn set(paths: &[&str]) -> BTreeSet<String> {
    paths.iter().map(|p| (*p).to_owned()).collect()
}

/// Review HIGH: a path and a directory of it overlap (directory/file), so
/// the narrowing never drops such a pair; unrelated paths do not.
#[test]
fn a_path_and_its_directory_overlap() {
    assert!(overlaps(&set(&["d"]), &set(&["d/x"])));
    assert!(overlaps(&set(&["d/x/y"]), &set(&["d"])));
    assert!(overlaps(&set(&["a.txt"]), &set(&["a.txt"])));
    assert!(!overlaps(&set(&["d"]), &set(&["dx", "d.txt", "e/d"])));
}

/// Review HIGH: a conflict is downgraded only when every path is changed by
/// exactly one side AND by the target between the bases; a rename
/// destination (one side only, the target never touched it), an unknown
/// side or delta, an empty or cut list stay conflicts.
#[test]
fn only_a_target_only_conflict_is_downgraded() {
    let mine = set(&["a.txt", "b.txt"]);
    let theirs = set(&["b.txt"]);
    let delta = set(&["a.txt"]);
    let files = || vec!["a.txt".to_owned()];
    assert_eq!(
        verdict(files(), false, Some(&mine), Some(&theirs), Some(&delta)),
        Verdict::TargetOnly
    );
    // A rename destination only one side has, untouched by the target.
    let renamed = set(&["a.txt", "moved.txt"]);
    assert_eq!(
        verdict(
            vec!["moved.txt".to_owned()],
            false,
            Some(&renamed),
            Some(&set(&["a.txt"])),
            Some(&set(&[]))
        ),
        Verdict::Conflict {
            kept: vec!["moved.txt".to_owned()]
        }
    );
    for (truncated, m, d) in [
        (true, Some(&mine), Some(&delta)),
        (false, None, Some(&delta)),
        (false, Some(&mine), None),
    ] {
        assert!(
            matches!(
                verdict(files(), truncated, m, Some(&theirs), d),
                Verdict::Conflict { .. }
            ),
            "{truncated} {m:?} {d:?}"
        );
    }
    assert!(matches!(
        verdict(Vec::new(), false, Some(&mine), Some(&theirs), Some(&delta)),
        Verdict::Conflict { .. }
    ));
    // A mixed list keeps the paths that are not the target's.
    assert_eq!(
        verdict(
            vec!["a.txt".to_owned(), "b.txt".to_owned()],
            false,
            Some(&mine),
            Some(&theirs),
            Some(&delta)
        ),
        Verdict::Conflict {
            kept: vec!["b.txt".to_owned()]
        }
    );
}
