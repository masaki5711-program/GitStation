//! Plan 18 S4 (review): the carry-forward rule against real git.
//!
//! `forge_core::merge_paths::may_interact` says when a landing cannot
//! change a merge request's trial merge. Its own property tests model the
//! merge; this one asks git. A base tree and two change sets over it are
//! generated from a small vocabulary that collides on purpose — names
//! with shared prefixes (`a`, `a/b`, `ab`), case variants, deletions, a
//! file replaced by a directory, a directory deleted, a rename — and both
//! sides are listed as the product lists them (`changed_path_entries`).
//! Whenever the rule says "cannot interact":
//!
//! - `git merge-tree` of the two sides is clean, and
//! - the merged tree is the tree obtained by applying both change sets
//!   one after the other, in either order.
//!
//! The case count is small (every case spawns git a few times, which is
//! slow on Windows) and the generator's seed is fixed; a failure is kept
//! in `proptest-regressions/` next to the crate's sources.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};

use forge_core::merge_paths::{may_interact, PathSet};
use proptest::prelude::*;
use proptest::sample::Index;
use proptest::test_runner::{Config, RngAlgorithm, TestRng, TestRunner};

use super::*;

/// A tree: file path → content.
type Tree = BTreeMap<String, String>;

/// `review.paths_max`'s default.
const PATHS_MAX: usize = 5000;
const CASES: u32 = 64;

fn name() -> impl Strategy<Value = &'static str> {
    prop_oneof![
        4 => Just("a"),
        3 => Just("b"),
        2 => Just("ab"),
        2 => Just("A"),
        1 => Just("c"),
    ]
}

fn path() -> impl Strategy<Value = String> {
    prop_oneof![
        3 => name().prop_map(str::to_owned),
        4 => (name(), name()).prop_map(|(x, y)| format!("{x}/{y}")),
        2 => (name(), name(), name()).prop_map(|(x, y, z)| format!("{x}/{y}/{z}")),
    ]
}

fn content() -> impl Strategy<Value = &'static str> {
    prop_oneof![Just("one\n"), Just("two\n"), Just("three\n")]
}

/// One change of a change set. An [`Index`] picks among the files (or
/// directories) the tree has when the change is applied.
#[derive(Debug, Clone)]
enum Change {
    /// Write a file. A file in the way of one of its directories is
    /// replaced by that directory; a directory of its name by the file.
    Put(String, &'static str),
    /// Other content for a file.
    Edit(Index, &'static str),
    /// Delete a file.
    Delete(Index),
    /// Delete a directory with everything below it.
    DeleteDir(Index),
    /// Move a file, content unchanged (a rename).
    Rename(Index, String),
    /// Replace a file by a directory of its name holding one file.
    Sprout(Index, &'static str, &'static str),
}

fn change() -> impl Strategy<Value = Change> {
    prop_oneof![
        3 => (path(), content()).prop_map(|(p, c)| Change::Put(p, c)),
        3 => (any::<Index>(), content()).prop_map(|(i, c)| Change::Edit(i, c)),
        2 => any::<Index>().prop_map(Change::Delete),
        2 => any::<Index>().prop_map(Change::DeleteDir),
        2 => (any::<Index>(), path()).prop_map(|(i, p)| Change::Rename(i, p)),
        2 => (any::<Index>(), name(), content()).prop_map(|(i, n, c)| Change::Sprout(i, n, c)),
    ]
}

/// Write `path`, as a checkout would have to: no file stays where the
/// path needs a directory, no directory where it needs the file.
fn put(tree: &mut Tree, path: &str, content: &str) {
    for (slash, _) in path.match_indices('/') {
        tree.remove(&path[..slash]);
    }
    let below = format!("{path}/");
    tree.retain(|p, _| !p.starts_with(&below));
    tree.insert(path.to_owned(), content.to_owned());
}

fn directories(tree: &Tree) -> Vec<String> {
    let mut dirs = BTreeSet::new();
    for path in tree.keys() {
        for (slash, _) in path.match_indices('/') {
            dirs.insert(path[..slash].to_owned());
        }
    }
    dirs.into_iter().collect()
}

fn apply(base: &Tree, changes: &[Change]) -> Tree {
    let mut tree = base.clone(); // clone: the base stays what both sides start from
    for change in changes {
        let files: Vec<String> = tree.keys().cloned().collect();
        let picked = |i: &Index| (!files.is_empty()).then(|| i.get(&files).clone());
        match change {
            Change::Put(path, content) => put(&mut tree, path, content),
            Change::Edit(i, content) => {
                if let Some(file) = picked(i) {
                    tree.insert(file, (*content).to_owned());
                }
            }
            Change::Delete(i) => {
                if let Some(file) = picked(i) {
                    tree.remove(&file);
                }
            }
            Change::DeleteDir(i) => {
                let dirs = directories(&tree);
                if !dirs.is_empty() {
                    let below = format!("{}/", i.get(&dirs));
                    tree.retain(|p, _| !p.starts_with(&below));
                }
            }
            Change::Rename(i, to) => {
                if let Some(content) = picked(i).and_then(|from| tree.remove(&from)) {
                    put(&mut tree, to, &content);
                }
            }
            Change::Sprout(i, name, content) => {
                if let Some(file) = picked(i) {
                    put(&mut tree, &format!("{file}/{name}"), content);
                }
            }
        }
    }
    tree
}

/// What a change set did to the base: the paths that are gone and the
/// files written (new, or with other content).
struct Diff {
    gone: Vec<String>,
    written: Vec<(String, String)>,
}

impl Diff {
    fn changes(&self) -> bool {
        !self.gone.is_empty() || !self.written.is_empty()
    }
}

fn diff(base: &Tree, tree: &Tree) -> Diff {
    Diff {
        gone: base
            .keys()
            .filter(|p| !tree.contains_key(*p))
            .cloned()
            .collect(),
        written: tree
            .iter()
            .filter(|(p, c)| base.get(*p) != Some(*c))
            .map(|(p, c)| (p.clone(), c.clone()))
            .collect(),
    }
}

/// `tree` with a change set applied on top of it, entry by entry — `None`
/// when the result is no tree (one name would be a file and a directory).
fn replay(tree: &Tree, changes: &Diff) -> Option<Tree> {
    let mut out = tree.clone(); // clone: the side replayed onto is compared again
    for path in &changes.gone {
        out.remove(path);
    }
    for (path, content) in &changes.written {
        out.insert(path.clone(), content.clone());
    }
    let is_tree = out.keys().all(|path| {
        path.match_indices('/')
            .all(|(slash, _)| !out.contains_key(&path[..slash]))
    });
    is_tree.then_some(out)
}

/// The commits of `base`, the two sides on top of it, and the tree id of
/// `expected` — one `fast-import` and one `rev-parse` (test setup only).
/// `core.ignorecase` is off for the import: a bare repository created on
/// Windows has it on, and `a` and `A` are two names here, as on the
/// server.
fn import(repo: &Path, [base, x, y, expected]: [&Tree; 4]) -> [ObjectId; 4] {
    let mut stream = String::new();
    for (mark, (name, tree)) in [("b", base), ("x", x), ("y", y), ("e", expected)]
        .into_iter()
        .enumerate()
    {
        stream.push_str(&format!(
            "commit refs/prop/{name}\nmark :{}\n\
             committer forge-test <forge-test@example.invalid> 1700000000 +0000\n\
             data 10\nsynthetic\n\n",
            mark + 1
        ));
        if name == "x" || name == "y" {
            stream.push_str("from :1\n");
        }
        stream.push_str("deleteall\n");
        for (path, content) in tree {
            stream.push_str(&format!(
                "M 100644 inline {path}\ndata {}\n{content}\n",
                content.len()
            ));
        }
        stream.push('\n');
    }
    plumb(
        repo,
        &[
            "-c",
            "core.ignorecase=false",
            "fast-import",
            "--force",
            "--quiet",
        ],
        Some(stream.as_str()),
    );
    let ids = plumb(
        repo,
        &[
            "rev-parse",
            "refs/prop/b",
            "refs/prop/x",
            "refs/prop/y",
            "refs/prop/e^{tree}",
        ],
        None,
    );
    let ids: Vec<ObjectId> = ids
        .lines()
        .map(|id| ObjectId::parse(id).expect("oid"))
        .collect();
    ids.try_into().expect("four ids")
}

/// The path set of `base..side`, listed and built as the mergeability
/// check and a landing list and build theirs.
async fn path_set(f: &Fixture, base: &ObjectId, side: &ObjectId) -> PathSet {
    let listed = f
        .git
        .changed_path_entries(&f.root, f.id, base, side, PATHS_MAX, T)
        .await
        .expect("diff-tree");
    listed
        .and_then(|entries| PathSet::build(entries, PATHS_MAX))
        .unwrap_or_else(|_| PathSet::incomplete())
}

fn tree_of(files: &[(&str, &str)]) -> Tree {
    files
        .iter()
        .map(|(p, c)| ((*p).to_owned(), (*c).to_owned()))
        .collect()
}

/// The harness itself, on named cases: the import keeps case variants
/// apart and nests directories; disjoint sides merge to the replayed
/// tree; a landing in a directory the other side deleted interacts (and
/// git would move or conflict on it), so nothing is claimed about it.
#[tokio::test]
async fn the_git_harness_builds_and_merges_what_the_model_says() {
    let f = fixture().await;
    let base = tree_of(&[("a", "one\n"), ("A", "one\n"), ("ab/c", "one\n")]);
    let x = apply(&base, &[Change::Put("a/b".to_owned(), "two\n")]);
    assert_eq!(
        x,
        tree_of(&[("a/b", "two\n"), ("A", "one\n"), ("ab/c", "one\n")]),
        "the file `a` became a directory"
    );
    let y = apply(&base, &[Change::Put("ab/d".to_owned(), "three\n")]);
    let expected = replay(&x, &diff(&base, &y)).expect("a tree");
    let [b, cx, cy, e] = import(&f.path, [&base, &x, &y, &expected]);
    assert_eq!(
        plumb(&f.path, &["ls-tree", "-r", "--name-only", b.as_str()], None),
        "A\na\nab/c",
        "case variants are two entries"
    );
    let (sx, sy) = (path_set(&f, &b, &cx).await, path_set(&f, &b, &cy).await);
    assert_eq!(sx.files(), ["a", "a/b"]);
    assert_eq!(sy.files(), ["ab/d"]);
    assert!(!may_interact(Some(&sx), Some(&sy)));
    let merged = f
        .trial(&cy, &cx, MergeTreeLimits::default())
        .await
        .expect("merge-tree");
    assert_eq!(merged, MergeTree::Clean { tree: e });

    // `ab/` deleted on one side, a file added below it on the other.
    let mut gone = base.clone(); // clone: the base without its only file below `ab/`
    assert!(gone.remove("ab/c").is_some());
    let [b, cg, cy, _] = import(&f.path, [&base, &gone, &y, &base]);
    let (sg, sy) = (path_set(&f, &b, &cg).await, path_set(&f, &b, &cy).await);
    assert_eq!(sg.deleted_dirs(), ["ab"]);
    assert!(may_interact(Some(&sg), Some(&sy)));
}

/// The property (module docs).
#[test]
fn sides_the_rule_keeps_apart_merge_cleanly_to_both_change_sets() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let f = rt.block_on(fixture());
    let config = Config {
        cases: CASES,
        max_shrink_iters: 200,
        source_file: Some(file!()),
        ..Config::default()
    };
    let mut runner =
        TestRunner::new_with_rng(config, TestRng::deterministic_rng(RngAlgorithm::ChaCha));
    let cases = (
        prop::collection::vec((path(), content()), 2..9),
        prop::collection::vec(change(), 1..4),
        prop::collection::vec(change(), 1..4),
    );
    let apart = Cell::new(0_u32);
    let outcome = runner.run(&cases, |(files, mine, theirs)| {
        let mut base = Tree::new();
        for (path, content) in &files {
            put(&mut base, path, content);
        }
        let (x, y) = (apply(&base, &mine), apply(&base, &theirs));
        let (dx, dy) = (diff(&base, &x), diff(&base, &y));
        let expected = replay(&x, &dy);
        // The base stands in for a result that is no tree: the import
        // needs four trees, and the assertion below fails first.
        let [b, cx, cy, e] = import(&f.path, [&base, &x, &y, expected.as_ref().unwrap_or(&base)]);
        let (sx, sy) =
            rt.block_on(async { (path_set(&f, &b, &cx).await, path_set(&f, &b, &cy).await) });
        prop_assert!(sx.is_complete() && sy.is_complete(), "{sx:?} {sy:?}");
        if may_interact(Some(&sx), Some(&sy)) {
            return Ok(());
        }
        if dx.changes() && dy.changes() {
            apart.set(apart.get() + 1);
        }
        prop_assert!(
            expected.is_some(),
            "both change sets do not make a tree: {x:?} then {y:?}"
        );
        prop_assert_eq!(
            replay(&y, &dx),
            expected,
            "the order of the change sets matters"
        );
        // The landing is the target, the merge request the head.
        let merged = rt
            .block_on(f.trial(&cy, &cx, MergeTreeLimits::default()))
            .expect("merge-tree");
        prop_assert_eq!(
            merged,
            MergeTree::Clean { tree: e },
            "base {:?}, merge request {:?} ({:?}), landing {:?} ({:?})",
            base,
            x,
            sx,
            y,
            sy
        );
        Ok(())
    });
    if let Err(failure) = outcome {
        panic!("{failure}");
    }
    assert!(
        apart.get() >= 8,
        "the generator must reach changed sides the rule keeps apart: {} of {CASES}",
        apart.get()
    );
}
