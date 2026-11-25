use super::*;
use crate::document::parse_document;
use crate::lint::{lint, Source};

fn lint_seed(title: &str, owner: &str) -> Vec<crate::lint::FileReport> {
    let files = seed(title, owner);
    let doctypes = [Source {
        path: files[0].0,
        bytes: files[0].1.as_bytes(),
    }];
    let documents = [Source {
        path: files[1].0,
        bytes: files[1].1.as_bytes(),
    }];
    lint(&doctypes, &documents)
}

/// Plan 20 W2: the first commit of a new wiki passes the document lint
/// as it is (the page type parses, the first page matches it).
#[test]
fn seed_passes_doc_lint() {
    let reports = lint_seed("web Wiki", "team/web");
    assert!(reports.is_empty(), "{reports:?}");
    let doc = parse_document(seed("web Wiki", "team")[1].1.as_bytes());
    assert!(doc.problems.is_empty(), "{:?}", doc.problems);
}

/// A display name with YAML's special characters stays a title.
#[test]
fn seed_title_cannot_change_the_front_matter() {
    for title in [
        "a: b",
        "\"quoted\" \\ back",
        "# not a comment",
        "line\nbreak: doctype: x",
        "設備 Wiki — [[NOT-A-REF]]",
        "{ flow: map }",
    ] {
        let reports = lint_seed(title, "team");
        assert!(reports.is_empty(), "{title:?}: {reports:?}");
    }
}

#[test]
fn long_title_is_cut() {
    let page = first_page(&"あ".repeat(500), "team");
    let line = page
        .lines()
        .find(|l| l.starts_with("title: "))
        .expect("title");
    assert_eq!(line.chars().filter(|c| *c == 'あ').count(), MAX_TITLE_CHARS);
}

#[test]
fn the_doctype_file_is_named_after_the_type() {
    assert!(DOCTYPE_PATH.ends_with(&format!("{DOCTYPE_NAME}.yml")));
}
