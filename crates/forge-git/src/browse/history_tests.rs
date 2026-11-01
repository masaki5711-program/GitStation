#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // tests may fail loudly
//! History browsing (M2 MR8) against real repositories (synthetic
//! content), plus fake binaries for the time limits.

use std::path::Path;
use std::time::Duration;

use forge_errors::ErrorCode;

use super::diff::{attach_patches, parse_raw, split_chunks};
use super::tests::{blob, fixture, mktree, plumb, reason, update_ref};
use super::*;

/// A commit with any number of parents and a message.
fn commit_with(repo: &Path, tree: &str, parents: &[&str], msg: &str) -> String {
    let mut args = vec!["commit-tree", tree, "-m", msg];
    for p in parents {
        args.extend(["-p", p]);
    }
    plumb(repo, &args, None)
}

/// `files`: (mode, name, content) at the root of a tree.
fn tree_of(repo: &Path, files: &[(&str, &str, &[u8])]) -> String {
    let blobs: Vec<String> = files
        .iter()
        .map(|(_, _, content)| blob(repo, content))
        .collect();
    let entries: Vec<(&str, &str, &str, &str)> = files
        .iter()
        .zip(&blobs)
        .map(|((mode, name, _), oid)| (*mode, "blob", oid.as_str(), *name))
        .collect();
    mktree(repo, &entries)
}

fn log_ids(repo: &Path, tip: &str, extra: &[&str]) -> Vec<String> {
    let mut args = vec!["log", "--format=%H", tip];
    args.extend(extra);
    plumb(repo, &args, None)
        .lines()
        .map(str::to_owned)
        .collect()
}

fn root() -> RepoPath {
    RepoPath::root()
}

/// Five commits on a linear history: `a.txt` changes in every commit,
/// `b.txt` in the 2nd and 4th only.
fn linear(repo: &Path) -> Vec<String> {
    let mut ids: Vec<String> = Vec::new();
    for i in 1..=5 {
        let a = format!("a {i}\n");
        let b = format!("b {}\n", i / 2);
        let tree = tree_of(
            repo,
            &[
                ("100644", "a.txt", a.as_bytes()),
                ("100644", "b.txt", b.as_bytes()),
            ],
        );
        let parents: Vec<&str> = ids.last().map(String::as_str).into_iter().collect();
        let id = commit_with(repo, &tree, &parents, &format!("commit {i}\n\nbody {i}"));
        ids.push(id);
    }
    update_ref(repo, "refs/heads/main", ids.last().unwrap());
    ids
}

#[tokio::test]
async fn commits_page_by_a_keyset_cursor_in_log_order() {
    let f = fixture().await;
    let ids = linear(&f.path);
    let tip = ids.last().unwrap();
    let b = f.browser(BrowseLimits::default());
    let expected = log_ids(&f.path, tip, &[]);
    assert_eq!(expected.len(), 5);

    let mut seen: Vec<String> = Vec::new();
    let mut after: Option<LogCursor> = None;
    let mut pages = 0;
    loop {
        let page = b
            .commits(f.id, tip, &root(), 2, after.as_ref())
            .await
            .expect("page");
        pages += 1;
        assert_eq!(&page.tip, tip);
        seen.extend(page.commits.iter().map(|c| c.oid.clone()));
        match page.next {
            Some(next) => {
                assert_eq!(&next.last, &seen[seen.len() - 1], "keyed on the last id");
                assert_eq!(next.skip, seen.len());
                // The cursor survives its text form.
                let text = next.encode();
                after = Some(LogCursor::parse(&text, 1000).expect("parse"));
                assert_eq!(after.as_ref(), Some(&next));
            }
            None => break,
        }
    }
    assert_eq!(pages, 3);
    assert_eq!(seen, expected, "no commit lost or repeated");

    // The metadata is filled in.
    let page = b.commits(f.id, tip, &root(), 1, None).await.expect("one");
    let c = &page.commits[0];
    assert_eq!(c.oid, *tip);
    assert_eq!(c.subject, "commit 5");
    assert_eq!(c.parents, vec![ids[3].clone()]);
    assert_eq!(c.author_name, "forge-test");
    assert_eq!(c.author_email, "forge-test@example.invalid");
    assert!(c.authored_at.contains('T'), "{}", c.authored_at);
    assert!(page.next.is_some());
    // A page that ends exactly at the history has no cursor.
    let all = b.commits(f.id, tip, &root(), 5, None).await.expect("all");
    assert_eq!((all.commits.len(), all.next), (5, None));
}

#[tokio::test]
async fn a_path_limits_the_history_and_its_cursor() {
    let f = fixture().await;
    let ids = linear(&f.path);
    let tip = ids.last().unwrap();
    let b = f.browser(BrowseLimits::default());
    let path = RepoPath::parse("b.txt").unwrap();
    let expected = log_ids(&f.path, tip, &["--", "b.txt"]);
    assert_eq!(expected.len(), 3, "commits 1, 2 and 4 changed b.txt");
    let first = b.commits(f.id, tip, &path, 2, None).await.expect("p1");
    let next = first.next.clone().expect("more");
    let second = b
        .commits(f.id, tip, &path, 2, Some(&next))
        .await
        .expect("p2");
    let all: Vec<String> = first
        .commits
        .iter()
        .chain(&second.commits)
        .map(|c| c.oid.clone())
        .collect();
    assert_eq!(all, expected);
    assert!(second.next.is_none());

    // The same cursor with another path is refused: the commit it names
    // is not where it says.
    let e = b
        .commits(f.id, tip, &root(), 2, Some(&next))
        .await
        .expect_err("another path");
    assert_eq!(e.code(), ErrorCode::RequestInvalid);
    assert_eq!(reason(&e), "cursor_invalid");
    let forged = LogCursor {
        last: ids[0].clone(),
        ..next.clone()
    };
    let e = b
        .commits(f.id, tip, &path, 2, Some(&forged))
        .await
        .expect_err("forged last");
    assert_eq!(reason(&e), "cursor_invalid");
    // A cursor whose tip is not a commit of the repository.
    let tree = plumb(&f.path, &["rev-parse", &format!("{tip}^{{tree}}")], None);
    let e = b
        .commits(
            f.id,
            &tree,
            &path,
            2,
            Some(&LogCursor {
                tip: tree.clone(),
                last: next.last.clone(),
                skip: 1,
            }),
        )
        .await
        .expect_err("tree tip");
    assert_eq!(e.code(), ErrorCode::RepositoryPathNotResolved);
}

#[test]
fn cursors_parse_strictly() {
    let id = "a".repeat(40);
    let ok = format!("v1.{id}.{id}.3");
    assert_eq!(LogCursor::parse(&ok, 10).expect("ok").skip, 3);
    for bad in [
        String::new(),
        "v1".to_owned(),
        format!("v2.{id}.{id}.3"),
        format!("v1.{id}.{id}.0"),
        format!("v1.{id}.{id}.11"),
        format!("v1.{id}.{id}.-1"),
        format!("v1.{id}.{id}.+1"),
        format!("v1.{id}.{id}.3.4"),
        format!("v1.{}.{id}.3", "A".repeat(40)),
        format!("v1.{}.{id}.3", "a".repeat(39)),
        format!("v1.{id}.--output=x.3"),
    ] {
        assert_eq!(LogCursor::parse(&bad, 10), None, "{bad}");
    }
}

#[tokio::test]
async fn option_like_paths_and_ids_are_names_not_options() {
    let f = fixture().await;
    let tree = tree_of(
        &f.path,
        &[
            ("100644", "-n", b"dash\n"),
            ("100644", "--output=x", b"opt\n"),
        ],
    );
    let c = commit_with(&f.path, &tree, &[], "dashes");
    let b = f.browser(BrowseLimits::default());
    for name in ["-n", "--output=x", "--all"] {
        let path = RepoPath::parse(name).unwrap();
        let page = b.commits(f.id, &c, &path, 5, None).await.expect(name);
        let want = usize::from(name != "--all");
        assert_eq!(page.commits.len(), want, "{name}");
    }
    assert!(!f.path.join("x").exists());
    assert!(!f.path.join("--output=x").exists());
    // Ids that are not full hex never reach git.
    for bad in ["--output=x", "-n", "HEAD", &c[..39]] {
        let e = b.commits(f.id, bad, &root(), 5, None).await.expect_err(bad);
        assert_eq!(e.code(), ErrorCode::RepositoryPathNotResolved, "{bad}");
        let e = b.commit(f.id, bad).await.expect_err(bad);
        assert_eq!(e.code(), ErrorCode::RepositoryPathNotResolved, "{bad}");
        let e = b.compare(f.id, bad, &c).await.expect_err(bad);
        assert_eq!(e.code(), ErrorCode::RepositoryPathNotResolved, "{bad}");
    }
    let blame = b
        .blame(f.id, &c, &RepoPath::parse("--output=x").unwrap())
        .await
        .expect("blame");
    assert_eq!(blame.hunks[0].lines, vec!["opt"]);
    assert!(!f.path.join("x").exists());
}

#[tokio::test]
async fn merge_commits_list_their_parents_and_diff_against_the_first() {
    let f = fixture().await;
    let base_tree = tree_of(&f.path, &[("100644", "a.txt", b"a\n")]);
    let base = commit_with(&f.path, &base_tree, &[], "base");
    let main_tree = tree_of(
        &f.path,
        &[("100644", "a.txt", b"a\n"), ("100644", "m.txt", b"main\n")],
    );
    let main = commit_with(&f.path, &main_tree, &[&base], "main side");
    let side_tree = tree_of(
        &f.path,
        &[("100644", "a.txt", b"a\n"), ("100644", "s.txt", b"side\n")],
    );
    let side = commit_with(&f.path, &side_tree, &[&base], "side side");
    let merged = tree_of(
        &f.path,
        &[
            ("100644", "a.txt", b"a\n"),
            ("100644", "m.txt", b"main\n"),
            ("100644", "s.txt", b"side\n"),
        ],
    );
    let merge = commit_with(&f.path, &merged, &[&main, &side], "merge");
    let b = f.browser(BrowseLimits::default());

    // One by one through cursors: every commit of both sides, once.
    let expected = log_ids(&f.path, &merge, &[]);
    assert_eq!(expected.len(), 4);
    let mut seen = Vec::new();
    let mut after: Option<LogCursor> = None;
    loop {
        let page = b
            .commits(f.id, &merge, &root(), 1, after.as_ref())
            .await
            .expect("page");
        seen.extend(page.commits.iter().map(|c| c.oid.clone()));
        if seen.len() == 1 {
            assert_eq!(page.commits[0].parents, vec![main.clone(), side.clone()]);
        }
        match page.next {
            Some(n) => after = Some(n),
            None => break,
        }
    }
    assert_eq!(seen, expected);

    // The diff is against the first parent: the side branch's file.
    let detail = b.commit(f.id, &merge).await.expect("merge");
    assert_eq!(detail.info.parents.len(), 2);
    let files: Vec<(&str, DiffStatus)> = detail
        .diff
        .files
        .iter()
        .map(|d| (d.new_path.as_deref().unwrap(), d.status))
        .collect();
    assert_eq!(files, vec![("s.txt", DiffStatus::Added)]);
}

#[tokio::test]
async fn a_commit_has_its_message_and_a_diff_with_patches() {
    let f = fixture().await;
    let t1 = tree_of(&f.path, &[("100644", "a.txt", b"one\n")]);
    let c1 = commit_with(&f.path, &t1, &[], "first");
    let t2 = tree_of(
        &f.path,
        &[("100644", "a.txt", b"one\ntwo\n"), ("100644", "n.txt", b"")],
    );
    let c2 = commit_with(&f.path, &t2, &[&c1], "second line\ncontinues\n\nthe body\n");
    let b = f.browser(BrowseLimits::default());

    // A root commit is compared with nothing.
    let root_commit = b.commit(f.id, &c1).await.expect("root");
    assert!(root_commit.info.parents.is_empty());
    assert_eq!(root_commit.diff.files.len(), 1);
    let added = &root_commit.diff.files[0];
    assert_eq!(added.status, DiffStatus::Added);
    assert_eq!(
        (added.old_path.as_deref(), added.new_path.as_deref()),
        (None, Some("a.txt"))
    );
    assert_eq!(added.patch.as_deref(), Some("@@ -0,0 +1 @@\n+one\n"));

    let d = b.commit(f.id, &c2).await.expect("c2");
    assert_eq!(d.info.subject, "second line continues");
    assert_eq!(d.message.trim_end(), "second line\ncontinues\n\nthe body");
    assert!(!d.diff.truncated);
    let a = &d.diff.files[0];
    assert_eq!(a.status, DiffStatus::Modified);
    assert_eq!(a.patch.as_deref(), Some("@@ -1 +1,2 @@\n one\n+two\n"));
    // An empty new file: no hunks, an empty patch (not an omission).
    let n = &d.diff.files[1];
    assert_eq!(n.status, DiffStatus::Added);
    assert_eq!(
        (n.patch.as_deref(), n.omitted, n.binary),
        (Some(""), None, false)
    );
    assert_eq!(n.old_oid, "0".repeat(40));
}

#[tokio::test]
async fn renames_binaries_and_mode_changes_are_told_apart() {
    let f = fixture().await;
    let text: String = (1..=20).map(|i| format!("line {i}\n")).collect();
    let changed = text.replace("line 10\n", "line ten\n");
    let t1 = tree_of(
        &f.path,
        &[
            ("100644", "old.txt", text.as_bytes()),
            ("100644", "pic.bin", b"\0\x01old"),
            ("100644", "run.sh", b"echo\n"),
        ],
    );
    let c1 = commit_with(&f.path, &t1, &[], "first");
    let t2 = tree_of(
        &f.path,
        &[
            ("100644", "new.txt", changed.as_bytes()),
            ("100644", "pic.bin", b"\0\x02new"),
            ("100755", "run.sh", b"echo\n"),
        ],
    );
    let c2 = commit_with(&f.path, &t2, &[&c1], "second");
    let b = f.browser(BrowseLimits::default());
    let d = b.commit(f.id, &c2).await.expect("c2").diff;
    let by = |name: &str| {
        d.files
            .iter()
            .find(|x| x.new_path.as_deref() == Some(name))
            .unwrap_or_else(|| panic!("{name}: {:?}", d.files))
    };
    let renamed = by("new.txt");
    assert_eq!(renamed.status, DiffStatus::Renamed);
    assert_eq!(renamed.old_path.as_deref(), Some("old.txt"));
    assert!(renamed.similarity.is_some_and(|s| (50..100).contains(&s)));
    assert!(renamed.patch.as_deref().unwrap().contains("-line 10"));
    let bin = by("pic.bin");
    assert_eq!(bin.status, DiffStatus::Modified);
    assert!(bin.binary);
    assert_eq!((bin.patch.as_deref(), bin.omitted), (None, None));
    let mode = by("run.sh");
    assert_eq!(
        (mode.old_mode.as_str(), mode.new_mode.as_str()),
        ("100644", "100755")
    );
    assert_eq!(mode.patch.as_deref(), Some(""));
    assert!(!d.truncated);
}

#[tokio::test]
async fn a_type_change_is_one_file_with_both_halves() {
    let f = fixture().await;
    let t1 = tree_of(
        &f.path,
        &[("100644", "x", b"plain\n"), ("100644", "y", b"y\n")],
    );
    let c1 = commit_with(&f.path, &t1, &[], "first");
    let t2 = tree_of(
        &f.path,
        &[("120000", "x", b"y"), ("100644", "y", b"y changed\n")],
    );
    let c2 = commit_with(&f.path, &t2, &[&c1], "second");
    let b = f.browser(BrowseLimits::default());
    let d = b.commit(f.id, &c2).await.expect("c2").diff;
    assert_eq!(d.files.len(), 2);
    assert_eq!(d.files[0].status, DiffStatus::TypeChanged);
    let patch = d.files[0].patch.as_deref().expect("patch");
    assert!(patch.contains("-plain") && patch.contains("+y"), "{patch}");
    assert_eq!(
        d.files[1].status,
        DiffStatus::Modified,
        "the chunks stayed in step"
    );
    assert!(d.files[1].patch.as_deref().unwrap().contains("+y changed"));
}

#[tokio::test]
async fn diff_caps_truncate_and_say_so() {
    let f = fixture().await;
    let names: Vec<String> = (0..5).map(|i| format!("f{i}.txt")).collect();
    let build = |word: &str| -> String {
        let contents: Vec<String> = (0..5).map(|i| format!("{word} {i}\n").repeat(30)).collect();
        let files: Vec<(&str, &str, &[u8])> = names
            .iter()
            .zip(&contents)
            .map(|(n, c)| ("100644", n.as_str(), c.as_bytes()))
            .collect();
        tree_of(&f.path, &files)
    };
    let c1 = commit_with(&f.path, &build("old"), &[], "old");
    let c2 = commit_with(&f.path, &build("new"), &[&c1], "new");

    // Whole.
    let whole = f.browser(BrowseLimits::default());
    let d = whole.commit(f.id, &c2).await.expect("whole").diff;
    assert_eq!(d.files.len(), 5);
    assert!(!d.truncated);
    assert!(d.files.iter().all(|x| x.patch.is_some()));

    // Too many files.
    let few = f.browser(BrowseLimits {
        max_diff_files: 2,
        ..BrowseLimits::default()
    });
    let d = few.commit(f.id, &c2).await.expect("few").diff;
    assert_eq!(d.files.len(), 2);
    assert!(d.truncated);
    assert!(
        d.files.iter().all(|x| x.patch.is_some()),
        "the shown ones keep theirs"
    );

    // One file's patch too large: left out, named.
    let small = f.browser(BrowseLimits {
        max_file_patch_bytes: 50,
        ..BrowseLimits::default()
    });
    let d = small.commit(f.id, &c2).await.expect("small").diff;
    assert!(d.truncated);
    assert!(d
        .files
        .iter()
        .all(|x| x.patch.is_none() && x.omitted == Some(PatchOmitted::TooLarge)));

    // The patch bytes run out: the list stays complete, the last file has
    // no patch.
    let tight = f.browser(BrowseLimits {
        max_diff_bytes: 400,
        ..BrowseLimits::default()
    });
    let d = tight.commit(f.id, &c2).await.expect("tight").diff;
    assert_eq!(d.files.len(), 5, "the list itself is complete");
    assert!(d.truncated);
    assert_eq!(d.files[4].omitted, Some(PatchOmitted::TotalCap));
    assert!(d.files[4].patch.is_none());
    for x in &d.files {
        assert!(x.patch.is_some() ^ x.omitted.is_some(), "{x:?}");
    }
}

#[tokio::test]
async fn compare_lists_the_commits_and_the_tree_diff() {
    let f = fixture().await;
    let ids = linear(&f.path);
    let b = f.browser(BrowseLimits::default());
    let c = b.compare(f.id, &ids[0], &ids[3]).await.expect("compare");
    let got: Vec<&str> = c.commits.iter().map(|c| c.oid.as_str()).collect();
    assert_eq!(got, vec![ids[3].as_str(), ids[2].as_str(), ids[1].as_str()]);
    assert!(!c.commits_truncated);
    assert!(c
        .diff
        .files
        .iter()
        .any(|d| d.new_path.as_deref() == Some("a.txt")));

    // The other way round: no commits to list, the same files reversed.
    let back = b.compare(f.id, &ids[3], &ids[0]).await.expect("back");
    assert!(back.commits.is_empty());
    assert_eq!(back.diff.files.len(), c.diff.files.len());
    let same = b.compare(f.id, &ids[2], &ids[2]).await.expect("same");
    assert!(same.commits.is_empty() && same.diff.files.is_empty());

    let capped = f.browser(BrowseLimits {
        max_compare_commits: 2,
        ..BrowseLimits::default()
    });
    let c = capped
        .compare(f.id, &ids[0], &ids[4])
        .await
        .expect("capped");
    assert_eq!(c.commits.len(), 2);
    assert!(c.commits_truncated);
}

#[tokio::test]
async fn blame_groups_lines_by_the_commit_that_last_changed_them() {
    let f = fixture().await;
    let t1 = tree_of(&f.path, &[("100644", "f.txt", b"a\nb\nc\n")]);
    let c1 = commit_with(&f.path, &t1, &[], "first");
    let t2 = tree_of(&f.path, &[("100644", "f.txt", b"a\nB\nc\nd\n")]);
    let c2 = commit_with(&f.path, &t2, &[&c1], "second");
    let b = f.browser(BrowseLimits::default());
    let path = RepoPath::parse("f.txt").unwrap();
    let blame = b.blame(f.id, &c2, &path).await.expect("blame");
    assert!(!blame.truncated && !blame.binary);
    assert_eq!(blame.size, 8);
    let shape: Vec<(usize, usize, Vec<&str>, &str)> = blame
        .hunks
        .iter()
        .map(|h| {
            (
                h.start,
                h.orig_start,
                h.lines.iter().map(String::as_str).collect(),
                blame.commits[h.commit].summary.as_str(),
            )
        })
        .collect();
    assert_eq!(
        shape,
        vec![
            (1, 1, vec!["a"], "first"),
            (2, 2, vec!["B"], "second"),
            (3, 3, vec!["c"], "first"),
            (4, 4, vec!["d"], "second"),
        ]
    );
    assert_eq!(blame.commits.len(), 2);
    let first = blame.commits.iter().find(|c| c.oid == c1).expect("c1");
    assert!(first.boundary);
    assert_eq!(first.author_name, "forge-test");
    assert_eq!(first.author_email, "forge-test@example.invalid");
    assert_eq!(first.path, "f.txt");
    assert!(first.author_time > 0);
    assert!(first.author_tz.starts_with(['+', '-']));

    // Not a file / no such file.
    let e = b.blame(f.id, &c2, &root()).await.expect_err("root");
    assert_eq!(reason(&e), "not_a_blob");
    let e = b
        .blame(f.id, &c2, &RepoPath::parse("nope").unwrap())
        .await
        .expect_err("missing");
    assert_eq!(reason(&e), "path_not_found");
}

#[tokio::test]
async fn blame_caps_lines_and_refuses_to_print_binary_files() {
    let f = fixture().await;
    let many: String = (1..=10).map(|i| format!("l{i}\n")).collect();
    let t = tree_of(
        &f.path,
        &[
            ("100644", "many.txt", many.as_bytes()),
            ("100644", "bin.dat", b"a\0b\n"),
            ("100644", "latin.txt", b"caf\xe9\n"),
            ("100644", "empty.txt", b""),
            ("100644", "nonl.txt", b"no newline"),
        ],
    );
    let c = commit_with(&f.path, &t, &[], "files");
    let b = f.browser(BrowseLimits {
        max_blame_lines: 4,
        ..BrowseLimits::default()
    });
    let blame = b
        .blame(f.id, &c, &RepoPath::parse("many.txt").unwrap())
        .await
        .expect("many");
    assert!(blame.truncated);
    let lines: usize = blame.hunks.iter().map(|h| h.lines.len()).sum();
    assert_eq!(lines, 4);
    let exact = f.browser(BrowseLimits {
        max_blame_lines: 10,
        ..BrowseLimits::default()
    });
    let blame = exact
        .blame(f.id, &c, &RepoPath::parse("many.txt").unwrap())
        .await
        .expect("exact");
    assert!(!blame.truncated, "a file of exactly the cap is whole");
    for name in ["bin.dat", "latin.txt"] {
        let blame = b
            .blame(f.id, &c, &RepoPath::parse(name).unwrap())
            .await
            .expect(name);
        assert!(blame.binary, "{name}");
        assert!(blame.hunks.is_empty() && blame.commits.is_empty(), "{name}");
    }
    let empty = b
        .blame(f.id, &c, &RepoPath::parse("empty.txt").unwrap())
        .await
        .expect("empty");
    assert!(empty.hunks.is_empty() && !empty.binary);
    let nonl = b
        .blame(f.id, &c, &RepoPath::parse("nonl.txt").unwrap())
        .await
        .expect("nonl");
    assert_eq!(nonl.hunks[0].lines, vec!["no newline"]);
}

#[cfg(unix)]
#[tokio::test]
async fn blame_and_compare_past_their_time_limits_are_git_timeout() {
    let f = fixture().await;
    let t = tree_of(&f.path, &[("100644", "f.txt", b"a\n")]);
    let c = commit_with(&f.path, &t, &[], "first");
    let tmp = tempfile::tempdir().expect("tmp");
    // Everything else runs for real; only blame hangs.
    let bin = super::tests::fake_git(
        tmp.path(),
        "if [ \"$1\" = blame ]; then exec sleep 5; fi\nexec git \"$@\"",
    );
    let b = f.browser_with_bin(
        &bin,
        BrowseLimits {
            blame_timeout: Duration::from_millis(500),
            ..BrowseLimits::default()
        },
    );
    let started = std::time::Instant::now();
    let e = b
        .blame(f.id, &c, &RepoPath::parse("f.txt").unwrap())
        .await
        .expect_err("timeout");
    assert_eq!(e.code(), ErrorCode::GitTimeout);
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "killed at the limit"
    );
    let bin = super::tests::fake_git(
        tmp.path(),
        "if [ \"$1\" = log ]; then exec sleep 5; fi\nexec git \"$@\"",
    );
    let b = f.browser_with_bin(
        &bin,
        BrowseLimits {
            compare_timeout: Duration::from_millis(500),
            ..BrowseLimits::default()
        },
    );
    let e = b.compare(f.id, &c, &c).await.expect_err("timeout");
    assert_eq!(e.code(), ErrorCode::GitTimeout);
}

#[cfg(unix)]
#[tokio::test]
async fn answers_that_do_not_fit_are_process_failed() {
    let f = fixture().await;
    let ids = linear(&f.path);
    let tmp = tempfile::tempdir().expect("tmp");
    // A log record whose id is not an id.
    let bin = super::tests::wrapping_git(
        tmp.path(),
        "[ \"$1\" = log ]",
        "printf 'nothex\\037\\037a\\037b\\0372026-01-01T00:00:00+00:00\\037c\\037d\\0372026-01-01T00:00:00+00:00\\037s\\000'",
    );
    let b = f.browser_with_bin(&bin, BrowseLimits::default());
    let e = b
        .commits(f.id, ids.last().unwrap(), &root(), 5, None)
        .await
        .expect_err("bad id");
    assert_eq!(e.code(), ErrorCode::GitProcessFailed);
    assert_eq!(reason(&e), "log_record_oid");
    // A patch with more files than the list names.
    let bin = super::tests::wrapping_git(
        tmp.path(),
        "[ \"$1\" = diff-tree ] && [ \"$7\" = -p ]",
        "printf 'diff --git a/x b/x\\n@@ -1 +1 @@\\n-a\\n+b\\ndiff --git a/y b/y\\n@@ -1 +1 @@\\n-a\\n+b\\n'",
    );
    let b = f.browser_with_bin(&bin, BrowseLimits::default());
    let e = b.commit(f.id, &ids[4]).await.expect_err("mismatch");
    assert_eq!(e.code(), ErrorCode::GitProcessFailed);
    assert_eq!(reason(&e), "diff_chunk_mismatch");
}

#[test]
fn raw_diff_output_is_parsed_exactly_or_refused() {
    let oid = "1".repeat(40);
    let zero = "0".repeat(40);
    let rec = |meta: String, paths: &[&str]| {
        let mut v = format!(":{meta}\0").into_bytes();
        for p in paths {
            v.extend_from_slice(p.as_bytes());
            v.push(0);
        }
        v
    };
    let mut out = rec(format!("000000 100644 {zero} {oid} A"), &["new file"]);
    out.extend(rec(
        format!("100644 100644 {oid} {oid} R087"),
        &["old\tname", "ünï.txt"],
    ));
    let parsed = parse_raw(&out, false, 10).expect("parse");
    assert_eq!(parsed.files.len(), 2);
    assert_eq!(parsed.files[0].new_path.as_deref(), Some("new file"));
    assert_eq!(parsed.files[1].old_path.as_deref(), Some("old\tname"));
    assert_eq!(parsed.files[1].similarity, Some(87));
    // The file cap.
    let capped = parse_raw(&out, false, 1).expect("cap");
    assert_eq!((capped.files.len(), capped.truncated), (1, true));
    // A cut drops the incomplete entry and says truncated.
    let cut = &out[..out.len() - 4];
    let parsed = parse_raw(cut, true, 10).expect("cut");
    assert_eq!((parsed.files.len(), parsed.truncated), (1, true));
    // Complete output that is wrong is an error, not a guess.
    for bad in [
        b"x\0".to_vec(),
        b":100644 100644 a b Q\0p\0".to_vec(),
        b":100644 100644 a b M\0".to_vec(),
        b":100644 a b M\0p\0".to_vec(),
        b":100644 100644 a b M\0p".to_vec(),
    ] {
        let e = parse_raw(&bad, false, 10).expect_err("bad");
        assert_eq!(e.code(), ErrorCode::GitProcessFailed);
    }
}

#[test]
fn patches_split_at_headers_and_match_files_in_order() {
    let patch = b"diff --git a/a b/a\nindex 1..2 100644\n--- a/a\n+++ b/a\n@@ -1 +1 @@\n-x\n+diff --git not a header\ndiff --git a/b b/b\nBinary files a/b and b/b differ\ndiff --git a/c b/c\nold mode 100644\nnew mode 100755\n";
    let chunks = split_chunks(patch);
    assert_eq!(chunks.len(), 3);
    assert!(!chunks[0].binary && chunks[0].hunks.starts_with(b"@@ -1 +1 @@"));
    assert!(chunks[0].hunks.ends_with(b"+diff --git not a header\n"));
    assert!(chunks[1].binary);
    assert!(!chunks[2].binary && chunks[2].hunks.is_empty());

    let one = |name: &str| {
        format!(
            ":100644 100644 {} {} M\0{name}\0",
            "1".repeat(40),
            "2".repeat(40)
        )
    };
    let bytes = format!("{}{}{}", one("a"), one("b"), one("c")).into_bytes();
    let mut files = parse_raw(&bytes, false, 10).expect("raw").files;
    assert_eq!(files.len(), 3);
    assert!(!attach_patches(&mut files, patch, false, 1000).expect("attach"));
    assert!(files[0].patch.is_some() && files[1].binary && files[2].patch.as_deref() == Some(""));
    // Two files for three chunks (and the reverse) is a mismatch.
    let mut two = parse_raw(&bytes, false, 2).expect("raw").files;
    let e = attach_patches(&mut two, patch, false, 1000).expect_err("extra chunk");
    assert_eq!(reason(&e), "diff_chunk_mismatch");
    let mut files = parse_raw(&bytes, false, 10).expect("raw").files;
    let e = attach_patches(&mut files, &patch[..120], false, 1000).expect_err("missing");
    assert_eq!(reason(&e), "diff_chunk_mismatch");
}
