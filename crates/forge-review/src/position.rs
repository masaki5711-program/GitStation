//! Where an inline discussion may sit, and whether a later version changed
//! it (M3 R3) — pure functions over a diff of the M2 browse machinery
//! (`forge_git::browse::Diff`, the `diff-tree -p` hunks; no new git
//! plumbing).
//!
//! - [`check_position`]: the line must be inside a hunk (changed or context
//!   lines, as the diff shows them) of a file the version's diff
//!   (`merge_base..head`) lists with that path on that side. Otherwise the
//!   FG-REVIEW-0004 reason.
//! - [`line_changed`]: given the diff from the commit a discussion was made
//!   on (its side's commit of its version) to the same side's commit of the
//!   current version, whether its line was removed or rewritten — the file
//!   deleted, renamed or retyped, or a `-` line at that number. A file whose
//!   change cannot be read line by line (binary, a patch left out by a cap,
//!   a truncated diff that may hide it) counts as changed: "outdated" is a
//!   prompt to look again, never a claim that nothing moved.

use forge_core::DiscussionSide;
use forge_git::browse::{Diff, DiffFile, DiffStatus};

/// Why a position is not in a diff (FG-REVIEW-0004's `reason`).
pub const PATH_NOT_IN_DIFF: &str = "path_not_in_diff";
pub const DIFF_TRUNCATED: &str = "diff_truncated";
pub const NO_PATCH: &str = "no_patch";
pub const LINE_OUT_OF_RANGE: &str = "line_out_of_range";

/// One hunk header `@@ -old_start[,old_len] +new_start[,new_len] @@`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Hunk {
    old_start: i64,
    old_len: i64,
    new_start: i64,
    new_len: i64,
}

/// `a` or `a,b` (git leaves out `,1`).
fn range(raw: &str) -> Option<(i64, i64)> {
    match raw.split_once(',') {
        Some((start, len)) => Some((start.parse().ok()?, len.parse().ok()?)),
        None => Some((raw.parse().ok()?, 1)),
    }
}

/// The hunk a header line opens (`None`: not a header).
fn header(line: &str) -> Option<Hunk> {
    let rest = line.strip_prefix("@@ -")?;
    let (ranges, _) = rest.split_once(" @@")?;
    let (old, new) = ranges.split_once(" +")?;
    let (old_start, old_len) = range(old)?;
    let (new_start, new_len) = range(new)?;
    Some(Hunk {
        old_start,
        old_len,
        new_start,
        new_len,
    })
}

/// The file of `diff` with `path` on `side` (the path before the change
/// on `old`, after it on `new`).
/// A copy's `old_path` is a source that still exists unchanged (its own
/// change, if any, is another entry), so the old side never resolves to a
/// copy (review round 1).
fn file_on<'a>(diff: &'a Diff, path: &str, side: DiscussionSide) -> Option<&'a DiffFile> {
    diff.files.iter().find(|f| {
        let named = match side {
            DiscussionSide::Old if f.status == DiffStatus::Copied => None,
            DiscussionSide::Old => f.old_path.as_deref(),
            DiscussionSide::New => f.new_path.as_deref(),
        };
        named == Some(path)
    })
}

/// Whether `line` of `side` of `path` is in `diff` (a version's diff):
/// `Ok` or the FG-REVIEW-0004 reason.
pub fn check_position(
    diff: &Diff,
    path: &str,
    side: DiscussionSide,
    line: i64,
) -> Result<(), &'static str> {
    let Some(file) = file_on(diff, path, side) else {
        return Err(if diff.truncated {
            DIFF_TRUNCATED
        } else {
            PATH_NOT_IN_DIFF
        });
    };
    let Some(patch) = file.patch.as_deref().filter(|_| !file.binary) else {
        return Err(NO_PATCH);
    };
    let inside = patch.lines().filter_map(header).any(|h| {
        let (start, len) = match side {
            DiscussionSide::Old => (h.old_start, h.old_len),
            DiscussionSide::New => (h.new_start, h.new_len),
        };
        line >= start && line < start.saturating_add(len)
    });
    if inside {
        Ok(())
    } else {
        Err(LINE_OUT_OF_RANGE)
    }
}

/// Whether `diff` (from the commit a discussion sits on to the same side
/// of the current version) removed or rewrote `line` of `path` (module
/// docs).
pub fn line_changed(diff: &Diff, path: &str, line: i64) -> bool {
    let Some(file) = file_on(diff, path, DiscussionSide::Old) else {
        // Not listed: untouched, unless a cap may have hidden it.
        return diff.truncated;
    };
    match file.status {
        DiffStatus::Deleted | DiffStatus::Renamed | DiffStatus::TypeChanged => return true,
        DiffStatus::Added | DiffStatus::Modified | DiffStatus::Copied => {}
    }
    let Some(patch) = file.patch.as_deref().filter(|_| !file.binary) else {
        return true;
    };
    let mut old_no: Option<i64> = None;
    for text in patch.lines() {
        if let Some(h) = header(text) {
            old_no = Some(h.old_start);
            continue;
        }
        let Some(n) = old_no else { continue };
        match text.as_bytes().first() {
            Some(b'-') if n == line => return true,
            Some(b'-' | b' ') => old_no = Some(n + 1),
            // `+` lines add on the new side only; `\` is "no newline".
            Some(_) | None => {}
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(
        status: DiffStatus,
        old: Option<&str>,
        new: Option<&str>,
        patch: Option<&str>,
    ) -> DiffFile {
        DiffFile {
            status,
            old_path: old.map(str::to_owned),
            new_path: new.map(str::to_owned),
            old_mode: "100644".to_owned(),
            new_mode: "100644".to_owned(),
            old_oid: "a".repeat(40),
            new_oid: "b".repeat(40),
            similarity: None,
            binary: false,
            patch: patch.map(str::to_owned),
            omitted: None,
        }
    }

    fn diff(files: Vec<DiffFile>) -> Diff {
        Diff {
            files,
            truncated: false,
        }
    }

    /// Lines 10..=14 old, 10..=15 new: line 12 rewritten, one line added.
    const PATCH: &str = "@@ -10,5 +10,6 @@ fn main() {\n a\n b\n-c\n+C\n+D\n d\n e\n";

    #[test]
    fn a_position_must_be_inside_a_hunk_of_a_listed_file_on_its_side() {
        let d = diff(vec![file(
            DiffStatus::Modified,
            Some("src/lib.rs"),
            Some("src/lib.rs"),
            Some(PATCH),
        )]);
        let new = DiscussionSide::New;
        let old = DiscussionSide::Old;
        assert_eq!(check_position(&d, "src/lib.rs", new, 10), Ok(()));
        assert_eq!(check_position(&d, "src/lib.rs", new, 15), Ok(()));
        assert_eq!(
            check_position(&d, "src/lib.rs", new, 16),
            Err(LINE_OUT_OF_RANGE)
        );
        assert_eq!(
            check_position(&d, "src/lib.rs", new, 9),
            Err(LINE_OUT_OF_RANGE)
        );
        assert_eq!(check_position(&d, "src/lib.rs", old, 14), Ok(()));
        assert_eq!(
            check_position(&d, "src/lib.rs", old, 15),
            Err(LINE_OUT_OF_RANGE)
        );
        assert_eq!(
            check_position(&d, "README.md", new, 1),
            Err(PATH_NOT_IN_DIFF)
        );
        let cut = Diff {
            truncated: true,
            ..d.clone()
        };
        assert_eq!(
            check_position(&cut, "README.md", new, 1),
            Err(DIFF_TRUNCATED)
        );
    }

    #[test]
    fn added_deleted_renamed_and_binary_files_have_their_sides() {
        let added = file(
            DiffStatus::Added,
            None,
            Some("new.txt"),
            Some("@@ -0,0 +1,2 @@\n+x\n+y\n"),
        );
        let deleted = file(
            DiffStatus::Deleted,
            Some("gone.txt"),
            None,
            Some("@@ -1 +0,0 @@\n-x\n"),
        );
        let renamed = file(DiffStatus::Renamed, Some("a.txt"), Some("b.txt"), Some(""));
        let mut binary = file(
            DiffStatus::Modified,
            Some("logo.png"),
            Some("logo.png"),
            None,
        );
        binary.binary = true;
        let d = diff(vec![added, deleted, renamed, binary]);
        assert_eq!(
            check_position(&d, "new.txt", DiscussionSide::New, 2),
            Ok(())
        );
        assert_eq!(
            check_position(&d, "new.txt", DiscussionSide::Old, 1),
            Err(PATH_NOT_IN_DIFF)
        );
        assert_eq!(
            check_position(&d, "gone.txt", DiscussionSide::Old, 1),
            Ok(())
        );
        assert_eq!(
            check_position(&d, "gone.txt", DiscussionSide::New, 1),
            Err(PATH_NOT_IN_DIFF)
        );
        // A pure rename has no hunk: no line to comment on.
        assert_eq!(
            check_position(&d, "b.txt", DiscussionSide::New, 1),
            Err(LINE_OUT_OF_RANGE)
        );
        assert_eq!(
            check_position(&d, "logo.png", DiscussionSide::New, 1),
            Err(NO_PATCH)
        );
    }

    #[test]
    fn a_line_is_changed_only_when_removed_or_its_file_moved() {
        let d = diff(vec![file(
            DiffStatus::Modified,
            Some("src/lib.rs"),
            Some("src/lib.rs"),
            Some(PATCH),
        )]);
        assert!(line_changed(&d, "src/lib.rs", 12), "the - line");
        assert!(!line_changed(&d, "src/lib.rs", 11), "context");
        assert!(
            !line_changed(&d, "src/lib.rs", 13),
            "context after the change"
        );
        assert!(!line_changed(&d, "src/lib.rs", 40), "outside every hunk");
        assert!(!line_changed(&d, "other.rs", 1), "an untouched file");
        let cut = Diff {
            truncated: true,
            ..d.clone()
        };
        assert!(line_changed(&cut, "other.rs", 1), "a cap may hide it");
        let moved = diff(vec![file(
            DiffStatus::Renamed,
            Some("a.rs"),
            Some("b.rs"),
            Some(""),
        )]);
        assert!(line_changed(&moved, "a.rs", 1));
        let gone = diff(vec![file(DiffStatus::Deleted, Some("a.rs"), None, None)]);
        assert!(line_changed(&gone, "a.rs", 1));
        let mut omitted = file(DiffStatus::Modified, Some("big.rs"), Some("big.rs"), None);
        omitted.omitted = Some(forge_git::browse::PatchOmitted::TooLarge);
        assert!(line_changed(&diff(vec![omitted]), "big.rs", 1));
    }

    /// A copy is never the old side of its source: the source's own entry
    /// (or none) decides.
    #[test]
    fn the_old_side_never_resolves_to_a_copy() {
        let copy = file(
            DiffStatus::Copied,
            Some("a.rs"),
            Some("b.rs"),
            Some("@@ -1,2 +1,2 @@\n x\n-y\n+z\n"),
        );
        let d = diff(vec![copy]);
        assert_eq!(
            check_position(&d, "a.rs", DiscussionSide::Old, 2),
            Err(PATH_NOT_IN_DIFF)
        );
        assert_eq!(check_position(&d, "b.rs", DiscussionSide::New, 2), Ok(()));
        assert!(!line_changed(&d, "a.rs", 2), "the source is untouched");
    }

    #[test]
    fn hunk_headers_without_lengths_mean_one_line() {
        assert_eq!(
            header("@@ -3 +3,0 @@"),
            Some(Hunk {
                old_start: 3,
                old_len: 1,
                new_start: 3,
                new_len: 0
            })
        );
        assert_eq!(header(" @@ -3 +3 @@"), None);
        assert_eq!(header("@@ -x +3 @@"), None);
    }
}
