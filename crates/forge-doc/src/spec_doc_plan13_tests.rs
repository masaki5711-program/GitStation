#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)] // tests may fail loudly
//! Plan 13 `doctype: spec.system` documents read by `spec_doc` (ADR-0026
//! §7): a migrated older document answers as it did before migration.
//! The older fixtures are those of `spec_doc_tests.rs` (synthetic).

use super::{parse_spec_document, Requirement, SpecDocument, SpecStatus};
use crate::fmt::migrate_legacy_with;

fn doc(text: &str) -> SpecDocument {
    parse_spec_document(text.as_bytes())
}

fn rules(d: &SpecDocument) -> Vec<&'static str> {
    d.problems.iter().map(|p| p.rule).collect()
}

/// A requirement without its lines (a migration moves them).
fn content(r: &Requirement) -> (String, Option<&'static str>, bool, String) {
    (
        r.id.as_str().to_owned(),
        r.priority.map(|p| p.as_str()),
        r.deprecated,
        r.body.clone(),
    )
}

const HEAD: &str = "---\nspec: a\ntitle: t\nstatus: draft\n---\n";

/// The older fixtures of `spec_doc_tests.rs` with a readable front matter.
fn fixtures() -> Vec<String> {
    let mut out = vec![
        "---\nspec: zeva-engine/config\ntitle: 設定管理\nstatus: approved        # draft / review / approved / deprecated\nowners: [po@example.internal]\n---\n\n## 設定の再読み込み\n\n::: req ZEVA-CFG-012\n設定ファイルの変更は、再起動せずに 10 秒以内に反映されること。\n受入: 変更から反映までの時間を計測するテストがある。\n:::\n\n::: req ZEVA-CFG-013 priority=must\n不正な設定は反映せず、エラーコードを記録して直前の設定で動き続けること。\n:::\n".to_owned(),
        "---\nspec: a\ntitle: t\nstatus: draft\n---\n```markdown\n::: req A-1\nx\n:::\n```\n~~~~\n::: req A-2\n~~~~\n".to_owned(),
        "---\nspec: a\ntitle: t\nstatus: deprecated\n---\n::: req A-1\nx\n:::\n".to_owned(),
        "---\nspec: a\ntitle: t\nstatus: approved\n---\n::: req A-1 status=deprecated\nx\n:::\n::: req A-2 status=active\ny\n:::\n".to_owned(),
        "---\nspec: a\ntitle: t\nstatus: draft\n---\n::: note\n::: req A-1\nx\n:::\ntext\n:::\n".to_owned(),
        "---\nspec: a\ntitle: t\nstatus: draft\n---\n``` a`b\n::: req A-1\nx\n:::\n".to_owned(),
        "---\nspec: a\ntitle: t\nstatus: draft\n---\n```\n::: req A-1\nx\n:::\n".to_owned(),
        "---\nspec: s\ntitle: t\nstatus: approved\nconfidential: true\n---\n::: req APP-X-1\nsecret\n:::\n".to_owned(),
        "---\nspec: s\ntitle: t\nstatus: approved\nconfidential: false\n---\n".to_owned(),
        "---\nspec: s\ntitle: t\nstatus: approved\n---\n".to_owned(),
    ];
    for body in [
        "::: req A-1\nbody\n",
        "::: req A-1\n::: req A-2\nx\n:::\n:::\n",
        "::: req a-1\nx\n:::\n",
        "::: req\nx\n:::\n",
        "::: req A-1 color=red\nx\n:::\n",
        "::: req A-1 priority=must priority=should\nx\n:::\n",
        "::: req A-1 priority=high\nx\n:::\n",
        "::: req A-1 status=gone\nx\n:::\n",
        "::: req A-1 must\nx\n:::\n",
        "::: req A-1\n\n:::\n",
        ":::\n",
    ] {
        out.push(format!("{HEAD}{body}"));
    }
    out.push(format!(
        "{HEAD}::: req A-1\n{}\n:::\n",
        "x".repeat(super::MAX_REQUIREMENT_BODY_BYTES + 1)
    ));
    out
}

#[test]
fn a_migrated_document_gives_the_same_requirements_and_spec_lint_answer() {
    for older in fixtures() {
        let before = doc(&older);
        let migrated = migrate_legacy_with(older.as_bytes(), None, true)
            .unwrap_or_else(|e| panic!("{e:?}: {older}"))
            .text;
        let after = doc(&migrated);
        assert!(after.is_spec(), "{migrated}");
        assert_eq!(rules(&after), rules(&before), "{migrated}");
        assert_eq!(
            after.requirements.iter().map(content).collect::<Vec<_>>(),
            before.requirements.iter().map(content).collect::<Vec<_>>(),
            "{migrated}"
        );
        let (b, a) = (before.meta.as_ref().unwrap(), after.meta.as_ref().unwrap());
        assert_eq!((&a.spec, &a.title), (&b.spec, &b.title));
        assert_eq!(
            a.status == SpecStatus::Deprecated,
            b.status == SpecStatus::Deprecated
        );
        assert_eq!(after.confidential(), before.confidential(), "{migrated}");
    }
}

const NEW: &str =
    "---\ndoctype: spec.system\nid: zeva-engine/config\ntitle: 設定管理\nowner: po-team\n";

#[test]
fn retired_deprecates_and_a_document_not_retired_reads_approved() {
    let open = doc(&format!("{NEW}---\n::: req A-1\nx\n:::\n"));
    assert!(open.problems.is_empty(), "{:?}", open.problems);
    let meta = open.meta.as_ref().expect("meta");
    assert_eq!(meta.status, SpecStatus::Approved);
    assert_eq!(meta.spec_line, 3);
    assert!(!open.requirements[0].deprecated);
    let retired = doc(&format!(
        "{NEW}retired: {{ date: 2026-11-01, replaced_by: X-1 }}\n---\n::: req A-1\nx\n:::\n"
    ));
    assert!(retired.problems.is_empty(), "{:?}", retired.problems);
    assert_eq!(
        retired.meta.as_ref().unwrap().status,
        SpecStatus::Deprecated
    );
    assert!(retired.requirements[0].deprecated);
}

#[test]
fn classification_is_the_confidential_mark_and_fails_closed() {
    let class = |fm: &str| doc(&format!("{NEW}{fm}---\n"));
    for (fm, confidential) in [
        ("", false),
        ("classification: public\n", false),
        ("classification: internal\n", false),
        ("classification: confidential\n", true),
        ("confidential: true\n", true),
        ("confidential: false\n", false),
        // Both keys: fail closed, as Document::classification.
        ("classification: public\nconfidential: false\n", true),
    ] {
        let d = class(fm);
        assert!(d.problems.is_empty(), "{fm}: {:?}", d.problems);
        assert_eq!(d.confidential(), confidential, "{fm}");
    }
    for fm in [
        "classification: secret\n",
        "confidential: maybe\n",
        "Classification: public\n",
    ] {
        let d = class(fm);
        assert!(d.meta.is_none(), "{fm}");
        assert!(d.confidential(), "{fm}: an unreadable mark fails closed");
    }
    assert_eq!(
        rules(&class("classification: secret\n")),
        ["confidential_invalid"]
    );
}

#[test]
fn the_base_keys_are_accepted_and_others_refused() {
    let d = doc(&format!(
        "{NEW}review_cycle: 1y\nrelated: [X-1]\nlang: ja\ntranslation_of: zeva-engine/config\n---\n"
    ));
    assert!(d.problems.is_empty(), "{:?}", d.problems);
    for (fm, line) in [
        ("spec: a\n", 6),
        ("status: approved\n", 6),
        ("owners: [a]\n", 6),
    ] {
        let d = doc(&format!("{NEW}{fm}---\n"));
        assert_eq!(
            d.problems
                .iter()
                .map(|p| (p.line, p.rule))
                .collect::<Vec<_>>(),
            [(line, "front_matter_unknown_key")],
            "{fm}"
        );
    }
    let no_id = doc("---\ndoctype: spec.system\ntitle: t\n---\n");
    assert_eq!(rules(&no_id), ["spec_missing"]);
    let no_title = doc("---\ndoctype: spec.system\nid: a\n---\n");
    assert_eq!(rules(&no_title), ["title_missing"]);
}

/// Review (0d59521, HIGH): a document of another type is not indexed —
/// unless it looks like a specification: then it is one, with
/// `doctype_not_spec`, its requirements read and withheld from an AI.
#[test]
fn a_document_of_another_type_is_no_specification_unless_it_looks_like_one() {
    let plain = doc("---\ndoctype: qms.manual\nid: QM-001\ntitle: t\nowner: qa\n---\n本文。\n");
    assert!(!plain.is_spec());
    assert!(plain.requirements.is_empty() && plain.problems.is_empty());
    for (text, line) in [
        // A requirement block.
        ("---\ndoctype: qms.manual\nid: QM-001\ntitle: t\nowner: qa\n---\n::: req A-1\nx\n:::\n", 2),
        // A non-scalar doctype.
        ("---\ndoctype: [spec.system]\nid: a\ntitle: t\n---\n::: req A-1\nx\n:::\n", 2),
        // An older file with a stray (typo'd) doctype.
        ("---\nspec: a\ntitle: t\nstatus: approved\ndoctype: spec.sytem\nconfidential: false\n---\n::: req A-1\nx\n:::\n", 5),
    ] {
        let d = doc(text);
        assert!(d.is_spec(), "{text}");
        assert_eq!(
            d.problems.iter().map(|p| (p.line, p.rule)).collect::<Vec<_>>(),
            [(line, "doctype_not_spec")],
            "{text}"
        );
        assert_eq!(d.requirements.len(), 1, "{text}");
        assert!(d.meta.is_none());
        assert!(d.confidential(), "{text}: fails closed");
    }
    // Older keys alone (no block) keep it a specification too.
    let keys = doc("---\nspec: a\ntitle: t\nstatus: draft\ndoctype: x.y\n---\n");
    assert!(keys.is_spec());
    assert_eq!(rules(&keys), ["doctype_not_spec"]);
}

/// Review (0d59521, HIGH): an HTML comment never closed hides the
/// requirement blocks after it from the newer parser — never silently.
#[test]
fn an_unclosed_comment_before_a_requirement_is_a_problem() {
    let d = doc("---\nspec: a\ntitle: t\nstatus: draft\n---\n::: req A-1\nx\n:::\n<!-- 下書き\n::: req A-2\ny\n:::\n");
    assert_eq!(
        d.problems
            .iter()
            .map(|p| (p.line, p.rule))
            .collect::<Vec<_>>(),
        [(9, "unclosed_comment")]
    );
    assert_eq!(d.requirements.len(), 1);
    // Without a requirement after it, the older answer stands (no problem).
    let quiet =
        doc("---\nspec: a\ntitle: t\nstatus: draft\n---\n::: req A-1\nx\n:::\n<!-- 下書き\n");
    assert!(quiet.problems.is_empty(), "{:?}", quiet.problems);
    // Reopened on a line that closes one: the last opening is named.
    let again = doc(
        "---\nspec: a\ntitle: t\nstatus: draft\n---\n<!-- a --> b <!-- c\n::: req A-2\ny\n:::\n",
    );
    assert_eq!(rules(&again), ["unclosed_comment"]);
    assert_eq!(again.problems[0].line, 6);
    // A file with no front matter: still a specification document.
    let bare = doc("<!--\n::: req A-1\nx\n:::\n");
    assert!(bare.is_spec());
    assert_eq!(rules(&bare), ["unclosed_comment", "front_matter_missing"]);
}

/// Review (0d59521, LOW): past the newer parser's block cap (plain
/// containers count) the requirements left out are reported.
#[test]
fn requirements_past_the_block_cap_are_reported() {
    let text = format!(
        "---\nspec: a\ntitle: t\nstatus: draft\n---\n{}{}",
        "::: note\n:::\n".repeat(1500),
        (0..600)
            .map(|i| format!("::: req A-{i}\nx\n:::\n"))
            .collect::<String>()
    );
    let d = doc(&text);
    assert_eq!(d.requirements.len(), 500);
    let cut = d
        .problems
        .iter()
        .filter(|p| p.rule == "too_many_requirements")
        .count();
    assert_eq!(cut + d.problems_dropped, 100, "{:?}", d.problems.len());
}

/// Review (0d59521, MEDIUM): a stricter `spec.system` type default (or a
/// definition that cannot be read) withholds every document, in both
/// formats; an absent or lower default changes nothing.
#[test]
fn the_type_default_never_lowers_and_can_withhold() {
    use crate::document::{Classification, TypeDefault};
    let texts = [
        format!("{NEW}---\n::: req A-1\nx\n:::\n"),
        format!("{NEW}classification: public\n---\n"),
        "---\nspec: a\ntitle: t\nstatus: approved\nconfidential: false\n---\n".to_owned(),
        "::: req A-1\nx\n:::\n".to_owned(),
    ];
    for text in &texts {
        for (ty, withheld) in [
            (TypeDefault::Known(None), false),
            (TypeDefault::Known(Some(Classification::Internal)), false),
            (TypeDefault::Known(Some(Classification::Confidential)), true),
            (TypeDefault::Unresolved, true),
        ] {
            let mut d = doc(text);
            assert!(!d.confidential());
            d.apply_type_default(ty);
            assert_eq!(d.confidential(), withheld, "{ty:?}: {text}");
        }
    }
}
