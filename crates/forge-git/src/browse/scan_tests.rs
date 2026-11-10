#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // tests may fail loudly
//! The specification scans (M3 X4) against a real bare repository
//! (synthetic content).

use std::time::Duration;

use super::tests::{blob, commit, fixture, mktree};
use super::*;

/// `spec/a.md`, `spec/deep/b.md`, `spec/link.md` (a symlink),
/// `spec/sub` (a submodule), `docs/c.md`, `tests/t.rs` with two `@req`
/// lines and a binary file naming `@req`.
fn tree(p: &std::path::Path) -> String {
    let a = blob(p, b"---\nspec: a\n---\n");
    let b = blob(p, b"# b\n");
    let c = blob(p, b"# c\n");
    let t = blob(p, b"fn x() {}\n// @req A-1\n\n// @req PBI-4/AC-1\n");
    let bin = blob(p, b"\0\x01@req A-1\n");
    let deep = mktree(p, &[("100644", "blob", &b, "b.md")]);
    let base = mktree(p, &[("100644", "blob", &c, "x")]);
    let sub_commit = commit(p, &base, None);
    let spec = mktree(
        p,
        &[
            ("100644", "blob", &a, "a.md"),
            ("040000", "tree", &deep, "deep"),
            ("120000", "blob", &a, "link.md"),
            ("160000", "commit", &sub_commit, "sub"),
        ],
    );
    let docs = mktree(p, &[("100644", "blob", &c, "c.md")]);
    let tests = mktree(
        p,
        &[
            ("100644", "blob", &t, "t.rs"),
            ("100644", "blob", &bin, "x.bin"),
        ],
    );
    mktree(
        p,
        &[
            ("040000", "tree", &docs, "docs"),
            ("040000", "tree", &spec, "spec"),
            ("040000", "tree", &tests, "tests"),
        ],
    )
}

fn path(s: &str) -> RepoPath {
    RepoPath::parse(s).expect("path")
}

#[tokio::test]
async fn blobs_under_lists_regular_files_below_each_directory_only() {
    let f = fixture().await;
    let c = commit(&f.path, &tree(&f.path), None);
    let b = f.browser(BrowseLimits::default());
    let listed = b
        .blobs_under(f.id, &c, &[path("spec/"), path("docs")], 100)
        .await
        .expect("list");
    let names: Vec<&str> = listed.blobs.iter().map(|e| e.path.as_str()).collect();
    assert_eq!(names, vec!["docs/c.md", "spec/a.md", "spec/deep/b.md"]);
    assert!(!listed.truncated);
    assert_eq!(listed.blobs[1].size, 16);
    // A directory that is not there lists nothing; no directory, nothing.
    let none = b
        .blobs_under(f.id, &c, &[path("nope")], 100)
        .await
        .expect("list");
    assert!(none.blobs.is_empty());
    assert!(b
        .blobs_under(f.id, &c, &[], 100)
        .await
        .expect("list")
        .blobs
        .is_empty());
    // Over the cap: cut and marked.
    let cut = b
        .blobs_under(f.id, &c, &[path("spec"), path("docs")], 2)
        .await
        .expect("list");
    assert_eq!(cut.blobs.len(), 2);
    assert!(cut.truncated);
}

/// Plan 13 stage 2: the document lint's one listing of a head is the
/// whole tree — regular files only, in git's order, cut at the cap.
#[tokio::test]
async fn blobs_in_tree_lists_every_regular_file_of_the_commit() {
    let f = fixture().await;
    let c = commit(&f.path, &tree(&f.path), None);
    let b = f.browser(BrowseLimits::default());
    let listed = b.blobs_in_tree(f.id, &c, 100).await.expect("list");
    let names: Vec<&str> = listed.blobs.iter().map(|e| e.path.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "docs/c.md",
            "spec/a.md",
            "spec/deep/b.md",
            "tests/t.rs",
            "tests/x.bin"
        ],
        "no symlink, no submodule"
    );
    assert!(!listed.truncated);
    let cut = b.blobs_in_tree(f.id, &c, 3).await.expect("list");
    assert_eq!(cut.blobs.len(), 3);
    assert!(cut.truncated);
}

#[tokio::test]
async fn read_blob_prefix_reads_at_most_the_cap() {
    let f = fixture().await;
    let c = commit(&f.path, &tree(&f.path), None);
    let b = f.browser(BrowseLimits::default());
    let listed = b
        .blobs_under(f.id, &c, &[path("spec")], 100)
        .await
        .expect("list");
    let first = &listed.blobs[0];
    assert_eq!(
        b.read_blob_prefix(f.id, first, 5).await.expect("read"),
        b"---\ns".to_vec()
    );
    assert_eq!(
        b.read_blob_prefix(f.id, first, 1000).await.expect("read"),
        b"---\nspec: a\n---\n".to_vec()
    );
}

#[tokio::test]
async fn read_blobs_returns_each_content_in_order() {
    let f = fixture().await;
    let c = commit(&f.path, &tree(&f.path), None);
    let b = f.browser(BrowseLimits::default());
    let listed = b
        .blobs_under(f.id, &c, &[path("spec")], 100)
        .await
        .expect("list");
    let read = b.read_blobs(f.id, &listed.blobs).await.expect("read");
    assert_eq!(
        read,
        vec![b"---\nspec: a\n---\n".to_vec(), b"# b\n".to_vec()]
    );
    assert!(b.read_blobs(f.id, &[]).await.expect("read").is_empty());
    // Only object ids reach git.
    let bad = ListedBlob {
        path: "x".to_owned(),
        oid: "--batch-all-objects".to_owned(),
        size: 1,
    };
    let err = b.read_blobs(f.id, &[bad]).await.expect_err("refused");
    assert_eq!(err.code(), forge_errors::ErrorCode::InvalidRefName);
}

#[tokio::test]
async fn grep_fixed_finds_every_text_line_naming_the_needle() {
    let f = fixture().await;
    let c = commit(&f.path, &tree(&f.path), None);
    let b = f.browser(BrowseLimits::default());
    let hits = b
        .grep_fixed(f.id, &c, "@req", 1024 * 1024, Duration::from_secs(10))
        .await
        .expect("grep");
    let found: Vec<(&str, usize, &str)> = hits
        .hits
        .iter()
        .map(|h| (h.path.as_str(), h.line, h.text.as_str()))
        .collect();
    assert_eq!(
        found,
        vec![
            ("tests/t.rs", 2, "// @req A-1"),
            ("tests/t.rs", 4, "// @req PBI-4/AC-1"),
        ],
        "the binary file is skipped"
    );
    assert!(!hits.truncated);
    // Nothing found is an empty answer, not a failure (git exits 1).
    let none = b
        .grep_fixed(f.id, &c, "@nothing-here", 1024, Duration::from_secs(10))
        .await
        .expect("grep");
    assert!(none.hits.is_empty());
    // Over the output cap: what came whole, marked truncated.
    let cut = b
        .grep_fixed(f.id, &c, "@req", 100, Duration::from_secs(10))
        .await
        .expect("grep");
    assert!(cut.truncated);
    assert_eq!(cut.hits.len(), 1);
}

#[tokio::test]
async fn merge_base_names_the_common_ancestor() {
    let f = fixture().await;
    let t = tree(&f.path);
    let root = commit(&f.path, &t, None);
    // Different trees: the same tree, parent and time would be one commit.
    let l = blob(
        &f.path, b"left
",
    );
    let r = blob(
        &f.path, b"right
",
    );
    let left = commit(
        &f.path,
        &mktree(&f.path, &[("100644", "blob", &l, "f")]),
        Some(&root),
    );
    let right = commit(
        &f.path,
        &mktree(&f.path, &[("100644", "blob", &r, "f")]),
        Some(&root),
    );
    let other = commit(&f.path, &mktree(&f.path, &[]), None);
    let b = f.browser(BrowseLimits::default());
    let id = |s: &str| forge_core::ObjectId::parse(s).expect("id");
    assert_eq!(
        b.merge_base(f.id, &id(&left), &id(&right))
            .await
            .expect("base"),
        Some(id(&root))
    );
    assert_eq!(
        b.merge_base(f.id, &id(&left), &id(&other))
            .await
            .expect("base"),
        None
    );
}
