#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // tests may fail loudly

use forge_git::browse::ListedBlob;

use super::*;

fn tree(files: &[(&str, &str)], incomplete: Option<&'static str>) -> DocTree {
    let mut t = DocTree::default();
    for (p, b) in files {
        t.add(p, b.as_bytes().to_vec());
    }
    t.sort();
    t.incomplete = incomplete;
    t
}

const NOTE_TYPE: (&str, &str) = (".forge/doctypes/note.yml", "name: note\n");
const HR_TYPE: (&str, &str) = (
    ".forge/doctypes/hr.yml",
    "name: hr\ndefaults:\n  classification: confidential\n",
);

fn doc(id: &str, ty: &str, extra: &str, body: &str) -> String {
    format!("---\nid: {id}\ndoctype: {ty}\ntitle: Title {id}\n{extra}---\n{body}")
}

fn entry<'a>(found: &'a [FoundDocument], path: &str) -> &'a DocumentEntry {
    &found
        .iter()
        .find(|f| f.entry.path == path)
        .unwrap_or_else(|| panic!("{path} indexed"))
        .entry
}

#[test]
fn a_document_gives_title_ids_and_body_without_front_matter() {
    let body = "# 設備の承認\n\n::: req REQ-0012\nポンプは止まる。\n:::\n";
    let t = tree(
        &[
            NOTE_TYPE,
            ("docs/SPEC-1.md", &doc("SPEC-1", "note", "", body)),
            ("README.md", "# readme, no type\n"),
        ],
        None,
    );
    let found = document_entries(&t);
    assert_eq!(found.len(), 1, "only documents of a type: {found:?}");
    let e = entry(&found, "docs/SPEC-1.md");
    assert_eq!(e.title, "Title SPEC-1");
    assert!(e.ids.starts_with("SPEC-1"), "{}", e.ids);
    assert!(e.ids.contains("設備の承認"), "headings: {}", e.ids);
    assert!(e.ids.contains("REQ-0012"), "block ids: {}", e.ids);
    assert!(!e.body.contains("doctype"), "no front matter: {}", e.body);
    assert!(e.body.contains("ポンプは止まる。"));
    assert_eq!(e.classification, "internal");
}

#[test]
fn confidential_at_tip_marked() {
    let t = tree(
        &[
            NOTE_TYPE,
            HR_TYPE,
            ("open.md", &doc("open", "note", "", "x\n")),
            (
                "own.md",
                &doc("own", "note", "classification: confidential\n", "x\n"),
            ),
            ("by-type.md", &doc("by-type", "hr", "", "x\n")),
            ("no-type.md", &doc("no-type", "missing", "", "x\n")),
            (
                "pub.md",
                &doc("pub", "note", "classification: public\n", "x\n"),
            ),
            (
                "own.en.md",
                &doc(
                    "own",
                    "note",
                    "lang: en\ntranslation_of: own\nclassification: public\n",
                    "x\n",
                ),
            ),
        ],
        None,
    );
    let found = document_entries(&t);
    for p in ["own.md", "by-type.md", "no-type.md", "own.en.md"] {
        assert_eq!(entry(&found, p).classification, "confidential", "{p}");
    }
    assert_eq!(entry(&found, "open.md").classification, "internal");
    assert_eq!(entry(&found, "pub.md").classification, "public");
}

#[test]
fn an_unclosed_or_missing_front_matter_body() {
    assert_eq!(body_of("---\nid: a\n"), "");
    assert_eq!(body_of("plain\ntext"), "plain\ntext");
    assert_eq!(body_of("---\r\nid: a\r\n...\r\nbody"), "body");
    assert_eq!(body_of(""), "");
}

fn listing(files: &[(&str, &str)], truncated: bool) -> BlobListing {
    BlobListing {
        blobs: files
            .iter()
            .map(|(p, oid)| ListedBlob {
                path: (*p).to_owned(),
                oid: (*oid).to_owned(),
                size: 1,
            })
            .collect(),
        truncated,
    }
}

#[test]
fn changed_paths_compare_blob_ids_of_documents_and_types() {
    let old = listing(
        &[
            ("a.md", "1"),
            ("b.md", "2"),
            ("gone.md", "3"),
            ("src/x.rs", "4"),
            (".forge/doctypes/note.yml", "5"),
        ],
        false,
    );
    let new = listing(
        &[
            ("a.md", "1"),
            ("b.md", "9"),
            ("new.md", "7"),
            ("src/x.rs", "8"),
            (".forge/doctypes/note.yml", "5"),
        ],
        false,
    );
    let c = changed_paths(&old, &new);
    let want: BTreeSet<String> = ["b.md", "gone.md", "new.md"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    assert_eq!(c.documents, want);
    assert!(!c.doctypes);
    let retyped = listing(&[(".forge/doctypes/note.yml", "6")], false);
    assert!(changed_paths(&old, &retyped).doctypes);
}

#[test]
fn a_changed_original_takes_its_translations_and_gone_paths_are_removed() {
    let t = tree(
        &[
            NOTE_TYPE,
            ("A.md", &doc("A", "note", "", "x\n")),
            (
                "A.en.md",
                &doc("A", "note", "lang: en\ntranslation_of: A\n", "x\n"),
            ),
            ("B.md", &doc("B", "note", "", "x\n")),
            ("plain.md", "no type\n"),
        ],
        None,
    );
    let found = document_entries(&t);
    let changed: BTreeSet<String> = ["A.md", "gone.md", "plain.md"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    let (entries, scope) = plan_update(&found, Some(&changed), &BlobListing::default());
    let paths: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();
    assert_eq!(paths, vec!["A.en.md", "A.md"]);
    assert_eq!(
        scope,
        DocumentScope::Changed {
            removed: vec!["gone.md".to_owned(), "plain.md".to_owned()]
        }
    );
    let (all, full) = plan_update(&found, None, &listing(&[("A.md", "1")], false));
    assert_eq!(all.len(), 3);
    assert_eq!(
        full,
        DocumentScope::Full {
            listed: Some(vec!["A.md".to_owned()])
        }
    );
    let (_, cut) = plan_update(&found, None, &listing(&[], true));
    assert_eq!(cut, DocumentScope::Full { listed: None });
}

#[test]
fn an_unread_document_under_a_bound_is_never_open() {
    // A read that hit a bound: what was read keeps its own classification.
    // A translation whose original was cut is withheld (review K4).
    let t = tree(
        &[
            NOTE_TYPE,
            ("A.md", &doc("A", "note", "", "x\n")),
            (
                "B.en.md",
                &doc("B", "note", "lang: en\ntranslation_of: B\n", "x\n"),
            ),
        ],
        Some("doc_files"),
    );
    let found = document_entries(&t);
    assert_eq!(entry(&found, "A.md").classification, "internal");
    assert_eq!(entry(&found, "B.en.md").classification, "confidential");
}
