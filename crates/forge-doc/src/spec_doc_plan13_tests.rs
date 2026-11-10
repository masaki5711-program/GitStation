#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)] // tests may fail loudly
//! Plan 13 `doctype: spec.system` documents read by `spec_doc` (ADR-0026
//! §7): a migrated older document answers as it did before migration.
//! The older fixtures are those of `spec_doc_tests.rs` (synthetic).

use super::{parse_spec_document, Requirement, SpecDocument, SpecStatus};
use crate::fmt::migrate_legacy;

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
        let migrated = migrate_legacy(older.as_bytes(), None)
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

#[test]
fn a_document_of_another_type_is_no_specification() {
    for text in [
        "---\ndoctype: qms.manual\nid: QM-001\ntitle: t\nowner: qa\n---\n::: req A-1\nx\n:::\n",
        "---\ndoctype: [spec.system]\nid: a\ntitle: t\n---\n::: req A-1\nx\n:::\n",
    ] {
        let d = doc(text);
        assert!(!d.is_spec(), "{text}");
        assert!(d.requirements.is_empty() && d.problems.is_empty());
        assert!(!d.confidential());
    }
}
