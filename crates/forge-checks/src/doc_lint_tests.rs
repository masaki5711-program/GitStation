#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! The `forge/doc-lint` run (plan 13 stage 2): pure, on hand-built trees of
//! synthetic documents.

use forge_core::checks::{CheckAnnotationLevel, CheckConclusion};
use forge_git::browse::{BlobListing, ListedBlob};

use super::{assess_doc_lint, is_doctype_file, is_document_file, select, DocLimits, DocTree};

const MANUAL_T: &str = "name: qms.manual\nid_pattern: '^QM-\\d{3}$'\n";

fn doc(id: &str, body: &str) -> String {
    format!("---\ndoctype: qms.manual\nid: {id}\ntitle: 品質マニュアル\nowner: qa-team\n---\n\n{body}\n")
}

fn tree(doctypes: &[(&str, &str)], markdown: &[(&str, String)]) -> DocTree {
    let mut t = DocTree::default();
    for (p, b) in doctypes {
        t.add(p, b.as_bytes().to_vec());
    }
    for (p, b) in markdown {
        t.add(p, b.as_bytes().to_vec());
    }
    t.sort();
    t
}

#[test]
fn documents_in_any_folder_that_keep_their_type_pass() {
    let t = tree(
        &[(".forge/doctypes/qms.manual.yml", MANUAL_T)],
        &[
            ("qms/QM-001.md", doc("QM-001", "本文。[[QM-002]]")),
            ("deep/a/b/QM-002.md", doc("QM-002", "本文。")),
            ("README.md", "# not a document\n".to_owned()),
        ],
    );
    let run = assess_doc_lint(&t).expect("run");
    assert_eq!(run.name.as_str(), "forge/doc-lint");
    assert_eq!(
        run.conclusion,
        Some(CheckConclusion::Success),
        "{}",
        run.summary
    );
    assert!(
        run.summary.contains("3 Markdown file(s)"),
        "{}",
        run.summary
    );
    assert!(run.annotations.is_empty());
}

#[test]
fn a_doc_lint_problem_fails_with_doc_lint_problem() {
    let t = tree(
        &[(".forge/doctypes/qms.manual.yml", MANUAL_T)],
        &[("qms/QM-001.md", doc("QM-001", "本文。[[QM-404]]"))],
    );
    let run = assess_doc_lint(&t).expect("run");
    assert_eq!(run.conclusion, Some(CheckConclusion::Failure));
    assert!(run.summary.contains("[FG-DOC-0001]"), "{}", run.summary);
    // Q13: a broken reference is an error.
    assert!(
        run.summary
            .contains("qms/QM-001.md:8 ref_broken — QM-404 [FG-DOC-0001]"),
        "{}",
        run.summary
    );
    assert_eq!(run.annotations.len(), 1);
    let a = &run.annotations[0];
    assert_eq!((a.path.as_str(), a.start_line), ("qms/QM-001.md", 8));
    assert_eq!(a.level, CheckAnnotationLevel::Failure);
}

#[test]
fn an_invalid_definition_fails_under_doctype_definition_invalid() {
    let t = tree(
        &[(".forge/doctypes/qms.manual.yml", "name: qms.other\n")],
        &[("qms/QM-001.md", doc("QM-001", "本文。"))],
    );
    let run = assess_doc_lint(&t).expect("run");
    assert_eq!(run.conclusion, Some(CheckConclusion::Failure));
    assert!(run.summary.contains("[FG-DOC-0003]:"), "{}", run.summary);
    let paths: Vec<&str> = run.annotations.iter().map(|a| a.path.as_str()).collect();
    assert_eq!(
        paths,
        vec![".forge/doctypes/qms.manual.yml", "qms/QM-001.md"]
    );
    assert!(
        run.annotations
            .iter()
            .all(|a| a.message.ends_with("[FG-DOC-0003]")),
        "{:?}",
        run.annotations
    );
}

/// A bound hit is never a silent pass: `neutral` with FG-DOC-0002 when
/// nothing was found, and broken references alone are no failure then.
#[test]
fn a_bounded_doc_index_is_never_a_silent_pass() {
    let mut t = tree(
        &[(".forge/doctypes/qms.manual.yml", MANUAL_T)],
        &[("qms/QM-001.md", doc("QM-001", "本文。"))],
    );
    t.left_out("doc_files");
    t.left_out("doc_bytes");
    assert_eq!(t.incomplete, Some("doc_files"), "the first bound is named");
    let run = assess_doc_lint(&t).expect("run");
    assert_eq!(run.conclusion, Some(CheckConclusion::Neutral));
    assert!(
        run.summary.contains("(doc_files)") && run.summary.contains("[FG-DOC-0002]"),
        "{}",
        run.summary
    );
    let mut broken = tree(
        &[(".forge/doctypes/qms.manual.yml", MANUAL_T)],
        &[("qms/QM-001.md", doc("QM-001", "本文。[[QM-404]]"))],
    );
    broken.left_out("doc_tree");
    let run = assess_doc_lint(&broken).expect("run");
    assert_eq!(
        run.conclusion,
        Some(CheckConclusion::Neutral),
        "{}",
        run.summary
    );
    // Any other problem still fails.
    let mut bad = tree(
        &[(".forge/doctypes/qms.manual.yml", MANUAL_T)],
        &[("qms/QM-001.md", doc("QM-1", "本文。"))],
    );
    bad.left_out("doc_tree");
    let run = assess_doc_lint(&bad).expect("run");
    assert_eq!(
        run.conclusion,
        Some(CheckConclusion::Failure),
        "{}",
        run.summary
    );
}

#[test]
fn a_document_over_the_parser_limit_is_too_large_like_gstn_reads_it() {
    let mut body = "---\ndoctype: qms.manual\nid: QM-001\ntitle: t\nowner: o\n---\n".to_owned();
    body.push_str(&"x".repeat(forge_doc::document::MAX_DOCUMENT_BYTES));
    let t = tree(
        &[(".forge/doctypes/qms.manual.yml", MANUAL_T)],
        &[("QM-001.md", body)],
    );
    assert_eq!(
        t.markdown[0].1.len(),
        forge_doc::document::MAX_DOCUMENT_BYTES + 1,
        "kept as gstn reads it"
    );
    let run = assess_doc_lint(&t).expect("run");
    assert_eq!(run.conclusion, Some(CheckConclusion::Failure));
    assert!(run.summary.contains("too_large"), "{}", run.summary);
}

fn blob(path: &str, size: u64) -> ListedBlob {
    ListedBlob {
        path: path.to_owned(),
        oid: "a".repeat(40),
        size,
    }
}

#[test]
fn select_reads_markdown_and_definitions_within_the_bounds() {
    assert!(is_document_file("a/b/QM-001.md"));
    assert!(!is_document_file("a/README.markdown"));
    assert!(is_doctype_file(".forge/doctypes/qms.manual.yml"));
    assert!(!is_doctype_file(".forge/doctypes/sub/x.yml"));
    assert!(!is_doctype_file(".forge/doctypes.yml"));
    let listing = BlobListing {
        blobs: vec![
            blob(".forge/doctypes/a.yml", 10),
            blob(".forge/repo.yml", 10),
            blob("a.md", 10),
            blob("b.md", 10),
            blob("big.md", 1_000),
            blob("src/main.rs", 10),
        ],
        truncated: false,
    };
    let (chosen, t) = select(&listing, &DocLimits::default());
    let paths: Vec<&str> = chosen.whole.iter().map(|b| b.path.as_str()).collect();
    assert_eq!(
        paths,
        vec![".forge/doctypes/a.yml", "a.md", "b.md", "big.md"]
    );
    assert!(chosen.prefix.is_empty());
    assert_eq!(t.incomplete, None);
    let few = DocLimits {
        max_files: 1,
        ..DocLimits::default()
    };
    let (chosen, t) = select(&listing, &few);
    assert_eq!(chosen.whole.len(), 2);
    assert_eq!(t.incomplete, Some("doc_files"));
    // A file over the parser's limit is read in part, counted at the cap.
    let huge = forge_doc::document::MAX_DOCUMENT_BYTES as u64 * 8;
    let large = BlobListing {
        blobs: vec![blob("a.md", 10), blob("huge.md", huge), blob("z.md", 10)],
        truncated: false,
    };
    let (chosen, t) = select(&large, &DocLimits::default());
    assert_eq!(chosen.prefix.len(), 1);
    assert_eq!(chosen.whole.len(), 2);
    assert_eq!(t.incomplete, None, "a large file is no bound by itself");
    let none = DocLimits {
        max_large_files: 0,
        ..DocLimits::default()
    };
    let (chosen, t) = select(&large, &none);
    assert!(chosen.prefix.is_empty());
    assert_eq!(chosen.whole.len(), 2);
    assert_eq!(t.incomplete, Some("doc_large_files"));
    let total = DocLimits {
        max_total_bytes: 25,
        ..DocLimits::default()
    };
    let (chosen, t) = select(&listing, &total);
    assert_eq!(chosen.whole.len(), 2);
    assert_eq!(t.incomplete, Some("doc_bytes"));
    let types = DocLimits {
        max_doctypes: 0,
        ..DocLimits::default()
    };
    let (_, t) = select(&listing, &types);
    assert_eq!(t.incomplete, Some("doctype_files"));
    let cut = BlobListing {
        truncated: true,
        ..listing
    };
    let (_, t) = select(&cut, &DocLimits::default());
    assert_eq!(t.incomplete, Some("doc_tree"));
}

/// Review MR3: a real bound hit (not a hand-set one) through `select`
/// makes the run `neutral` with FG-DOC-0002.
#[test]
fn a_bound_hit_while_selecting_makes_the_run_neutral() {
    let listing = BlobListing {
        blobs: vec![blob("QM-001.md", 10), blob("QM-002.md", 10)],
        truncated: false,
    };
    let few = DocLimits {
        max_files: 1,
        ..DocLimits::default()
    };
    let (chosen, mut t) = select(&listing, &few);
    t.add(
        ".forge/doctypes/qms.manual.yml",
        MANUAL_T.as_bytes().to_vec(),
    );
    for b in &chosen.whole {
        t.add(&b.path, doc("QM-001", "本文。").into_bytes());
    }
    let run = assess_doc_lint(&t).expect("run");
    assert_eq!(
        run.conclusion,
        Some(CheckConclusion::Neutral),
        "{}",
        run.summary
    );
    assert!(
        run.summary.contains("(doc_files)") && run.summary.contains("[FG-DOC-0002]"),
        "{}",
        run.summary
    );
}
