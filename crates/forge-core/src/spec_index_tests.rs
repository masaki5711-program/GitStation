#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! The requirement index, spec-lint, the requirement diff and spec-trace
//! (M3 X4, plan 03 §4–§5, plan 06 D-7).

use super::{
    diff, lint, trace_merge_request, trace_table, ChangeKind, ExternalScope, SpecIndex,
    TouchReason, TraceState,
};
use crate::repo_config::{parse_repo_config, ScopePattern};
use crate::WorkItemRef;

fn spec(name: &str, blocks: &str) -> String {
    format!("---\nspec: {name}\ntitle: t\nstatus: approved\n---\n{blocks}")
}

fn index(docs: &[(&str, String)], tags: &[(&str, usize, &str)]) -> SpecIndex {
    let mut i = SpecIndex::default();
    for (path, text) in docs {
        i.add_document(path, text.as_bytes());
    }
    for (path, line, text) in tags {
        i.add_tag_line(path, *line, text);
    }
    i
}

fn none() -> ExternalScope {
    ExternalScope::default()
}

fn rules(found: &[super::LintFinding]) -> Vec<(String, usize, &'static str)> {
    found
        .iter()
        .map(|f| (f.path.clone(), f.line, f.rule))
        .collect()
}

#[test]
fn documents_that_are_no_spec_are_not_indexed() {
    let i = index(
        &[
            ("spec/README.md", "# Specs\n".to_owned()),
            ("spec/a.md", spec("a", "::: req A-1\nx\n:::\n")),
        ],
        &[],
    );
    assert_eq!(i.documents.len(), 1);
    assert_eq!(i.requirements().len(), 1);
}

#[test]
fn lint_reports_document_problems_and_duplicate_ids_across_documents() {
    let i = index(
        &[
            ("spec/a.md", spec("a", "::: req A-1\nx\n:::\n::: req A-2\n")),
            ("spec/b.md", spec("b", "::: req A-1\ny\n:::\n")),
        ],
        &[],
    );
    let found = lint(&i, &none());
    assert_eq!(
        rules(&found),
        vec![
            ("spec/a.md".to_owned(), 9, "block_unclosed"),
            ("spec/b.md".to_owned(), 6, "duplicate_id"),
        ]
    );
    assert_eq!(
        found[1].detail.as_deref(),
        Some("A-1 (first at spec/a.md:6)")
    );
}

#[test]
fn lint_reports_duplicate_spec_names() {
    let i = index(
        &[
            ("spec/a.md", spec("same", "::: req A-1\nx\n:::\n")),
            ("spec/b.md", spec("same", "::: req A-2\ny\n:::\n")),
        ],
        &[],
    );
    assert_eq!(
        rules(&lint(&i, &none())),
        vec![("spec/b.md".to_owned(), 2, "duplicate_spec")]
    );
}

#[test]
fn a_tag_for_an_undefined_id_of_an_own_family_is_a_broken_reference() {
    let docs = [("spec/a.md", spec("a", "::: req ZEVA-CFG-1\nx\n:::\n"))];
    let tags = [
        ("tests/t.rs", 3, "// @req ZEVA-CFG-1"),
        ("tests/t.rs", 9, "// @req ZEVA-CFG-99"),
        // Another family: not this repository's to judge.
        ("tests/t.rs", 12, "// @req OTHER-X-1"),
        ("tests/t.rs", 14, "// @req PBI-4/AC-1"),
    ];
    let i = index(&docs, &tags);
    let found = lint(&i, &none());
    assert_eq!(
        rules(&found),
        vec![("tests/t.rs".to_owned(), 9, "broken_reference")]
    );
    assert_eq!(found[0].detail.as_deref(), Some("ZEVA-CFG-99"));

    // Covered by a referenced specification's scope: not broken here.
    let config = parse_repo_config(
        b"specs:\n  - repo: specs/zeva\n    version: v1\n    scope: [ZEVA-CFG-9*]\n",
    )
    .expect("config");
    let scoped = ExternalScope::of(&config);
    assert!(lint(&i, &scoped).is_empty());
    // A reference without a scope may cover any id.
    let all =
        parse_repo_config(b"specs:\n  - repo: specs/zeva\n    version: v1\n").expect("config");
    assert!(lint(&i, &ExternalScope::of(&all)).is_empty());
    assert!(ExternalScope::of(&all).covers("ANY-1"));
    assert!(!none().covers("ANY-1"));
}

#[test]
fn diff_lists_added_changed_removed_and_deprecated() {
    let from = index(
        &[(
            "spec/a.md",
            spec(
                "a",
                "::: req A-1\nsame\n:::\n::: req A-2\nold\n:::\n::: req A-3\ngone\n:::\n::: req A-4\nkept\n:::\n::: req A-6 priority=should\np\n:::\n",
            ),
        )],
        &[],
    );
    let to = index(
        &[
            (
                "spec/a.md",
                spec(
                    "a",
                    "::: req A-2\nnew\n:::\n::: req A-4 status=deprecated\nkept\n:::\n::: req A-5\nadded\n:::\n::: req A-6 priority=must\np\n:::\n",
                ),
            ),
            // Moved to another file without a change: not a change.
            ("spec/b.md", spec("b", "\n\n::: req A-1\nsame\n:::\n")),
        ],
        &[],
    );
    let changes: Vec<(String, ChangeKind)> = diff(&from, &to)
        .into_iter()
        .map(|c| (c.id.as_str().to_owned(), c.kind))
        .collect();
    assert_eq!(
        changes,
        vec![
            ("A-2".to_owned(), ChangeKind::Changed),
            ("A-3".to_owned(), ChangeKind::Removed),
            ("A-4".to_owned(), ChangeKind::Deprecated),
            ("A-5".to_owned(), ChangeKind::Added),
            ("A-6".to_owned(), ChangeKind::Changed),
        ]
    );
}

#[test]
fn spec_trace_lists_a_changed_requirement_without_a_test_as_untested() {
    let base = index(
        &[(
            "spec/a.md",
            spec("a", "::: req A-1\nold\n:::\n::: req A-2\nkeep\n:::\n"),
        )],
        &[],
    );
    let head = index(
        &[(
            "spec/a.md",
            spec(
                "a",
                "::: req A-1\nnew\n:::\n::: req A-2\nkeep\n:::\n::: req A-3\nadded\n:::\n::: req A-4 status=deprecated\nx\n:::\n",
            ),
        )],
        &[("tests/a.rs", 5, "// @req A-3"), ("tests/b.rs", 1, "// @req A-3")],
    );
    let t = trace_merge_request(&base, &head, &[], &none());
    let rows: Vec<(String, TraceState)> = t
        .rows
        .iter()
        .map(|r| (r.subject.to_string(), r.state.clone()))
        .collect();
    assert_eq!(
        rows,
        vec![
            ("A-1".to_owned(), TraceState::Untested),
            ("A-3".to_owned(), TraceState::Tested(2)),
        ],
        "A-2 is unchanged and A-4 deprecated: neither is touched"
    );
    assert_eq!(t.rows[0].reasons, vec![TouchReason::SpecChanged]);
    assert_eq!(t.rows[0].at, Some(("spec/a.md".to_owned(), 6)));
    assert!(!t.advisory_clear());
}

#[test]
fn spec_trace_counts_added_tags_and_flags_unknown_and_malformed_ones() {
    let base = index(
        &[("spec/a.md", spec("a", "::: req A-1\nx\n:::\n"))],
        &[("tests/a.rs", 2, "// @req A-1")],
    );
    let head = index(
        &[("spec/a.md", spec("a", "::: req A-1\nx\n:::\n"))],
        &[
            // Moved down a line: not an added tag.
            ("tests/a.rs", 3, "// @req A-1"),
            ("tests/b.rs", 1, "// @req A-1"),
            ("tests/b.rs", 2, "// @req NOPE-1"),
            ("tests/b.rs", 3, "// @req EXT-9"),
            ("tests/b.rs", 4, "// @req PBI-8/AC-1"),
            ("tests/b.rs", 5, "// @req lower"),
        ],
    );
    let config =
        parse_repo_config(b"specs:\n  - repo: specs/ext\n    version: v1\n    scope: [EXT-*]\n")
            .expect("config");
    let t = trace_merge_request(&base, &head, &[], &ExternalScope::of(&config));
    let rows: Vec<(String, TraceState, Vec<TouchReason>)> = t
        .rows
        .iter()
        .map(|r| (r.subject.to_string(), r.state.clone(), r.reasons.clone()))
        .collect();
    assert_eq!(
        rows,
        vec![
            (
                "A-1".to_owned(),
                TraceState::Tested(2),
                vec![TouchReason::TagAdded]
            ),
            (
                "EXT-9".to_owned(),
                TraceState::Tested(1),
                vec![TouchReason::TagAdded]
            ),
            (
                "NOPE-1".to_owned(),
                TraceState::Unknown,
                vec![TouchReason::TagAdded]
            ),
            (
                "PBI-8/AC-1".to_owned(),
                TraceState::Tested(1),
                vec![TouchReason::TagAdded]
            ),
        ]
    );
    assert_eq!(t.rows[2].at, Some(("tests/b.rs".to_owned(), 2)));
    assert_eq!(t.malformed.len(), 1);
    assert_eq!(t.malformed[0].raw, "lower");
}

#[test]
fn spec_trace_checks_each_linked_work_item_for_criterion_tests() {
    let base = SpecIndex::default();
    let head = index(
        &[],
        &[("tests/a.rs", 1, "// @req PBI-43/AC-1, PBI-43/AC-2")],
    );
    let linked = [
        WorkItemRef::parse("PBI-43").expect("ref"),
        WorkItemRef::parse("PBI-44").expect("ref"),
    ];
    let t = trace_merge_request(&base, &head, &linked, &none());
    let rows: Vec<(String, TraceState)> = t
        .rows
        .iter()
        .filter(|r| r.reasons.contains(&TouchReason::WorkItemLinked))
        .map(|r| (r.subject.to_string(), r.state.clone()))
        .collect();
    assert_eq!(
        rows,
        vec![
            ("PBI-43".to_owned(), TraceState::Tested(2)),
            ("PBI-44".to_owned(), TraceState::Untested),
        ]
    );
}

#[test]
fn spec_trace_with_nothing_touched_is_clear() {
    let i = index(&[("spec/a.md", spec("a", "::: req A-1\nx\n:::\n"))], &[]);
    let t = trace_merge_request(&i, &i, &[], &none());
    assert!(t.rows.is_empty());
    assert!(t.advisory_clear());
}

#[test]
fn the_trace_table_lists_each_requirement_with_its_tests_within_scope() {
    let spec_index = index(
        &[(
            "spec/a.md",
            spec(
                "a",
                "::: req A-1\nx\n:::\n::: req A-2\ny\n:::\n::: req B-1\nz\n:::\n",
            ),
        )],
        &[],
    );
    let tags = index(
        &[],
        &[
            ("t/1.rs", 1, "@req A-1"),
            ("t/2.rs", 1, "@req A-1"),
            ("t/3.rs", 1, "@req A-1"),
        ],
    );
    let table = trace_table(&spec_index, None, &tags, 2);
    let rows: Vec<(String, usize, usize)> = table
        .iter()
        .map(|r| (r.id.as_str().to_owned(), r.test_count, r.tests.len()))
        .collect();
    assert_eq!(
        rows,
        vec![
            ("A-1".to_owned(), 3, 2),
            ("A-2".to_owned(), 0, 0),
            ("B-1".to_owned(), 0, 0),
        ]
    );
    let scope = [ScopePattern::parse("A-*").expect("pattern")];
    let scoped = trace_table(&spec_index, Some(&scope), &tags, 2);
    assert_eq!(scoped.len(), 2);
}

/// M3 X5 (plan 06 D-5): the paths of the documents an AI agent is not
/// given.
#[test]
fn confidential_paths_lists_the_withheld_documents() {
    let i = index(
        &[
            ("spec/open.md", spec("a/open", "::: req APP-A-1\nx\n:::\n")),
            (
                "spec/secret.md",
                "---\nspec: a/secret\ntitle: t\nstatus: approved\nconfidential: true\n---\n::: req APP-A-2\ny\n:::\n"
                    .to_owned(),
            ),
        ],
        &[],
    );
    let paths: Vec<&str> = i.confidential_paths().into_iter().collect();
    assert_eq!(paths, ["spec/secret.md"]);
}

/// ADR-0026 §7: a document migrated to the plan 13 format (`gstn doc fmt
/// --migrate`) is indexed as before — the same requirements, the same
/// spec-lint answer (a name used twice is still `duplicate_spec` across the
/// two formats), and a document of another type is not indexed.
#[test]
fn a_migrated_document_is_indexed_and_linted_as_before() {
    let older = spec("a", "::: req A-1 priority=must\nx\n:::\n");
    let migrated = forge_doc::fmt::migrate_legacy(older.as_bytes(), None)
        .expect("migrates")
        .text;
    let before = index(
        &[("spec/a.md", older.clone())],
        &[("src/t.rs", 1, "// @req A-1")],
    );
    let after = index(
        &[("spec/a.md", migrated.clone())],
        &[("src/t.rs", 1, "// @req A-1")],
    );
    assert!(lint(&before, &none()).is_empty());
    assert!(
        lint(&after, &none()).is_empty(),
        "{:?}",
        lint(&after, &none())
    );
    let ids = |i: &SpecIndex| -> Vec<String> {
        i.requirements()
            .iter()
            .map(|l| format!("{} {:?}", l.requirement.id, l.requirement.priority))
            .collect()
    };
    assert_eq!(ids(&after), ids(&before));
    let both = index(
        &[
            ("spec/a.md", older),
            ("spec/b.md", migrated.replace("A-1", "A-2")),
        ],
        &[],
    );
    let found = lint(&both, &none());
    assert!(
        found.iter().any(|f| f.rule == "duplicate_spec"),
        "{found:?}"
    );
    let other = index(
        &[(
            "spec/QM-001.md",
            "---\ndoctype: qms.manual\nid: QM-001\ntitle: t\nowner: qa\n---\n".to_owned(),
        )],
        &[],
    );
    assert!(other.documents.is_empty());
}
