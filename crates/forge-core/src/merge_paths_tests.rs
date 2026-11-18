#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! The rule of [`crate::merge_paths`]: every doubtful case is an
//! interaction (plan 18 S4, ADR-0037 (c)).

use proptest::prelude::*;

use crate::merge_paths::{
    may_interact, ChangedPath, Incomplete, Landed, LandedPaths, PathSet, LANDED_BUDGET, LANDED_KEY,
    RULE_VERSION,
};
use crate::{ObjectId, RefName};

fn changed(path: &str) -> ChangedPath {
    ChangedPath {
        path: path.to_owned(),
        deleted: false,
        special: false,
    }
}

fn deleted(path: &str) -> ChangedPath {
    ChangedPath {
        deleted: true,
        ..changed(path)
    }
}

fn set(paths: &[&str]) -> PathSet {
    PathSet::build(paths.iter().map(|p| changed(p)), 5000).expect("complete")
}

fn set_deleting(paths: &[&str]) -> PathSet {
    PathSet::build(paths.iter().map(|p| deleted(p)), 5000).expect("complete")
}

fn interact(mr: &PathSet, landed: &PathSet) -> bool {
    may_interact(Some(mr), Some(landed))
}

#[test]
fn the_same_file_is_an_interaction() {
    assert!(interact(&set(&["docs/a.md"]), &set(&["docs/a.md"])));
    assert!(interact(
        &set(&["docs/a.md", "x.txt"]),
        &set(&["y.txt", "docs/a.md"])
    ));
}

#[test]
fn disjoint_files_do_not_interact() {
    // Neighbours in one directory, and names that only share a prefix.
    assert!(!interact(&set(&["docs/a.md"]), &set(&["docs/b.md"])));
    assert!(!interact(&set(&["a"]), &set(&["ab"])));
    assert!(!interact(&set(&["a/b"]), &set(&["ab/b"])));
    assert!(!interact(&set(&["docs/a/x.md"]), &set(&["docs/ab/x.md"])));
    assert!(!interact(&set(&[]), &set(&["docs/a.md"])));
    assert!(!interact(&set(&["docs/a.md"]), &set(&[])));
}

#[test]
fn a_file_dir_clash_is_an_interaction() {
    // `a` is a file on one side and a directory on the other.
    assert!(interact(&set(&["a"]), &set(&["a/b"])));
    assert!(interact(&set(&["a/b"]), &set(&["a"])));
    assert!(interact(&set(&["docs/spec"]), &set(&["docs/spec/v2/x.md"])));
    assert!(interact(&set(&["docs/spec/v2/x.md"]), &set(&["docs/spec"])));
    // A deleted file whose name becomes a directory, too.
    assert!(interact(&set_deleting(&["a"]), &set(&["a/b"])));
    assert!(interact(&set(&["a/b"]), &set_deleting(&["a"])));
}

#[test]
fn a_landed_directory_deletion_rechecks_mrs_adding_under_it() {
    // The landing deleted (or renamed away) `specs/old/`; the merge request
    // adds a file below it.
    let landed = set_deleting(&["specs/old/a.md", "specs/old/b.md"]);
    assert!(interact(&set(&["specs/old/c.md"]), &landed));
    assert!(interact(&set(&["specs/old/deep/c.md"]), &landed));
    // The parent of what was deleted counts as well (wider, never narrower).
    assert!(interact(&set(&["specs/new.md"]), &landed));
    // The reverse: the merge request deletes the directory, the landing
    // added below it.
    let mr = set_deleting(&["specs/old/a.md"]);
    assert!(interact(&mr, &set(&["specs/old/c.md"])));
    // Nothing in common with the deleted directories: no interaction.
    assert!(!interact(&set(&["guides/c.md"]), &landed));
    assert!(!interact(&mr, &set(&["guides/c.md"])));
}

#[test]
fn a_case_only_difference_is_an_interaction() {
    assert!(interact(&set(&["Docs/A.md"]), &set(&["docs/a.md"])));
    assert!(interact(&set(&["DOCS"]), &set(&["docs/a.md"])));
    assert_eq!(set(&["Docs/A.md"]), set(&["docs/a.md"]));
}

#[test]
fn a_gitattributes_change_rechecks_everything() {
    for global in [
        ".gitattributes",
        "docs/.gitattributes",
        "a/b/.GitAttributes",
        ".gitmodules",
        ".forge/repo.yml",
        ".forge/agents/x/part.md",
        ".Forge/repo.yml",
        ".forge-ci.yml",
        "CODEOWNERS",
    ] {
        let built = PathSet::build([changed("docs/a.md"), changed(global)], 5000);
        assert_eq!(built, Err(Incomplete::Global), "{global}");
    }
    // Only the root's CI definition and CODEOWNERS are global.
    for plain in [
        "docs/.forge-ci.yml",
        "docs/CODEOWNERS",
        "forge/x",
        "x.forge",
    ] {
        assert!(PathSet::build([changed(plain)], 5000).is_ok(), "{plain}");
    }
    // What the callers store for it interacts with everything, either side.
    let everything = PathSet::incomplete();
    assert!(interact(&set(&["unrelated/x.md"]), &everything));
    assert!(interact(&everything, &set(&["unrelated/x.md"])));
    assert!(interact(&set(&[]), &everything));
}

#[test]
fn an_incomplete_path_set_is_rechecked() {
    let any = set(&["docs/a.md"]);
    let other = set(&["src/b.rs"]);
    assert!(!interact(&any, &other));
    // Unknown (never stored, or an old row) on either side.
    assert!(may_interact(None, Some(&other)));
    assert!(may_interact(Some(&any), None));
    assert!(may_interact(None, None));
    // Incomplete on either side.
    assert!(interact(&PathSet::incomplete(), &other));
    assert!(interact(&any, &PathSet::incomplete()));
    assert!(interact(
        &PathSet::from_stored(vec!["docs/a.md".to_owned()], vec![], vec![], false),
        &other
    ));
    // The cap: one more than `max` is no complete set.
    assert_eq!(
        PathSet::build([changed("a"), changed("b"), changed("c")], 2),
        Err(Incomplete::TooMany)
    );
    assert!(PathSet::build([changed("a"), changed("b")], 2).is_ok());
    assert_eq!(
        PathSet::build([changed("a/b/c/d")], 2),
        Err(Incomplete::TooMany),
        "directories count against the cap"
    );
    // A symbolic link or a submodule.
    let link = ChangedPath {
        special: true,
        ..changed("docs/link")
    };
    assert_eq!(PathSet::build([link], 5000), Err(Incomplete::Special));
    // Paths no key stands for.
    for unusual in ["", "/a", "a/", "a//b", "a/./b", "../a", "a\0b"] {
        assert_eq!(
            PathSet::build([changed(unusual)], 5000),
            Err(Incomplete::Unusual),
            "{unusual:?}"
        );
    }
    // A null column is an unknown set.
    assert!(PathSet::from_columns(Some(vec![]), Some(vec![]), None, Some(true)).is_none());
    assert!(PathSet::from_columns(Some(vec![]), Some(vec![]), Some(vec![]), None).is_none());
    assert!(PathSet::from_columns(Some(vec![]), Some(vec![]), Some(vec![]), Some(true)).is_some());
}

#[test]
fn a_set_lists_every_ancestor_directory() {
    let built = PathSet::build([changed("a/b/c.md"), deleted("x/y/z.md")], 5000).expect("set");
    assert_eq!(built.files(), ["a/b/c.md", "x/y/z.md"]);
    assert_eq!(built.dirs(), ["a", "a/b", "x", "x/y"]);
    assert_eq!(built.deleted_dirs(), ["x", "x/y"]);
    assert!(built.is_complete());
}

fn oid(c: char) -> ObjectId {
    ObjectId::parse(&c.to_string().repeat(40)).expect("oid")
}

fn landed(paths: PathSet) -> LandedPaths {
    LandedPaths {
        target: RefName::parse("refs/heads/main").expect("ref"),
        old: oid('a'),
        new: oid('b'),
        paths,
    }
}

#[test]
fn the_landed_block_reads_back_what_was_written() {
    let block =
        landed(PathSet::build([changed("docs/A.md"), deleted("old/x.md")], 5000).expect("set"));
    let payload = serde_json::json!({ "project_id": 1, LANDED_KEY: block.to_value() });
    assert_eq!(LandedPaths::read(&payload), Landed::Usable(block));
}

#[test]
fn a_payload_without_the_block_has_no_landed_paths() {
    // A direct push, or an event written before the path sets.
    assert_eq!(
        LandedPaths::read(&serde_json::json!({ "project_id": 1, "refs": [] })),
        Landed::Absent
    );
    assert_eq!(
        LandedPaths::read(&serde_json::json!({ LANDED_KEY: null })),
        Landed::Absent
    );
}

#[test]
fn a_block_that_cannot_be_trusted_is_unusable() {
    let good = landed(set(&["docs/a.md"])).to_value();
    let with = |key: &str, value: serde_json::Value| {
        let mut block = good.clone(); // clone: one changed copy per case
        block[key] = value;
        LandedPaths::read(&serde_json::json!({ LANDED_KEY: block }))
    };
    assert_eq!(
        with("rule", serde_json::json!(RULE_VERSION + 1)),
        Landed::Unusable("rule")
    );
    assert_eq!(
        with("rule", serde_json::json!("1")),
        Landed::Unusable("rule")
    );
    assert_eq!(
        with("paths_complete", serde_json::json!(false)),
        Landed::Unusable("incomplete")
    );
    assert_eq!(
        with("paths_complete", serde_json::json!("true")),
        Landed::Unusable("malformed")
    );
    assert_eq!(
        with("old", serde_json::json!("zzz")),
        Landed::Unusable("malformed")
    );
    assert_eq!(
        with("changed_files", serde_json::json!([1, 2])),
        Landed::Unusable("malformed")
    );
    assert_eq!(
        with("deleted_parent_dirs", serde_json::json!(null)),
        Landed::Unusable("malformed")
    );
    // An incomplete set is written without its lists and read as unusable.
    let incomplete = landed(PathSet::incomplete()).to_value();
    assert_eq!(incomplete["changed_files"], serde_json::json!([]));
    assert_eq!(
        LandedPaths::read(&serde_json::json!({ LANDED_KEY: incomplete })),
        Landed::Unusable("incomplete")
    );
}

#[test]
fn a_landing_larger_than_the_budget_is_sent_incomplete() {
    let many: Vec<ChangedPath> = (0..2000)
        .map(|i| changed(&format!("documents/section-{i:04}/page-{i:04}.md")))
        .collect();
    let block = landed(PathSet::build(many, 5000).expect("set")).to_value();
    assert!(block.to_string().len() <= LANDED_BUDGET);
    assert_eq!(block["paths_complete"], serde_json::json!(false));
    assert_eq!(block["changed_files"], serde_json::json!([]));
    assert_eq!(block["changed_dirs"], serde_json::json!([]));
}

/// Path components that collide on purpose: few names, mixed case.
fn component() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("a".to_owned()),
        Just("ab".to_owned()),
        Just("A".to_owned()),
        Just("b".to_owned()),
        Just("docs".to_owned()),
        Just("ドキュメント".to_owned()),
        "[a-c]{1,2}",
    ]
}

fn path() -> impl Strategy<Value = ChangedPath> {
    (proptest::collection::vec(component(), 1..4), any::<bool>()).prop_map(|(parts, deleted)| {
        ChangedPath {
            path: parts.join("/"),
            deleted,
            special: false,
        }
    })
}

fn paths() -> impl Strategy<Value = Vec<ChangedPath>> {
    proptest::collection::vec(path(), 0..6)
}

proptest! {
    /// Whatever the two changes are: when they share a file key, or one's
    /// file is the other's directory, they interact.
    #[test]
    fn a_shared_key_always_interacts(mr in paths(), landed in paths()) {
        let (a, b) = (
            PathSet::build(mr, 5000).expect("set"),
            PathSet::build(landed, 5000).expect("set"),
        );
        let shares = |x: &[String], y: &[String]| x.iter().any(|k| y.contains(k));
        if shares(a.files(), b.files()) || shares(a.files(), b.dirs()) || shares(a.dirs(), b.files()) {
            prop_assert!(may_interact(Some(&a), Some(&b)));
        }
        // No key in common at all (files and directories): no interaction.
        let all = |s: &PathSet| -> Vec<String> {
            s.files().iter().chain(s.dirs()).cloned().collect()
        };
        if !shares(&all(&a), &all(&b)) {
            prop_assert!(!may_interact(Some(&a), Some(&b)));
        }
    }

    /// Keys are folded and well formed: no empty component, no upper case
    /// that folds further.
    #[test]
    fn keys_are_folded(mr in paths()) {
        let set = PathSet::build(mr, 5000).expect("set");
        for key in set.files().iter().chain(set.dirs()).chain(set.deleted_dirs()) {
            prop_assert!(!key.split('/').any(str::is_empty));
            let folded: String = key
                .chars()
                .flat_map(char::to_uppercase)
                .flat_map(char::to_lowercase)
                .collect();
            prop_assert_eq!(&folded, key);
        }
        for dir in set.deleted_dirs() {
            prop_assert!(set.dirs().contains(dir));
        }
    }
}
