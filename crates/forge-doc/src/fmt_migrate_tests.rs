#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)] // tests may fail loudly
//! `migrate_legacy` (ADR-0026 §7, `gstn doc fmt --migrate`) on the older
//! format's fixtures (the documents of `spec_doc_tests.rs`; synthetic).

use super::{format_document, migrate_legacy, migrate_legacy_with, FmtRefusal, MigrationNote};
use crate::document::{parse_document, Classification, TypeDefault};
use crate::legacy::{requirement, requirements};
use crate::lint::{lint, Source};

/// `spec_doc_tests.rs`' PLAN_EXAMPLE (plan 03 §4.2).
const PLAN_EXAMPLE: &str = "---
spec: zeva-engine/config
title: 設定管理
status: approved        # draft / review / approved / deprecated
owners: [po@example.internal]
---

## 設定の再読み込み

::: req ZEVA-CFG-012
設定ファイルの変更は、再起動せずに 10 秒以内に反映されること。
受入: 変更から反映までの時間を計測するテストがある。
:::

::: req ZEVA-CFG-013 priority=must
不正な設定は反映せず、エラーコードを記録して直前の設定で動き続けること。
:::
";

fn migrate(text: &str) -> super::Migrated {
    migrate_legacy(text.as_bytes(), None).expect("migrates")
}

#[test]
fn the_plan_example_is_migrated_without_inventing_an_owner() {
    let m = migrate(PLAN_EXAMPLE);
    let want = "---
doctype: spec.system
id: zeva-engine/config
title: 設定管理
# TODO(gstn doc fmt --migrate): owner: <the owning group>
# (the older file named people: po@example.internal)
---

## 設定の再読み込み

::: req ZEVA-CFG-012
設定ファイルの変更は、再起動せずに 10 秒以内に反映されること。
受入: 変更から反映までの時間を計測するテストがある。
:::

::: req ZEVA-CFG-013 priority=must
不正な設定は反映せず、エラーコードを記録して直前の設定で動き続けること。
:::
";
    assert_eq!(m.text, want);
    assert_eq!(
        m.notes,
        [MigrationNote::OwnerMissing, MigrationNote::StatusDropped]
    );
    // The lint asks a person for the owner (and knows no spec.system type
    // in this repository).
    let reports = lint(
        &[],
        &[Source {
            path: "spec/zeva-engine-config.md",
            bytes: m.text.as_bytes(),
        }],
    );
    let problems: Vec<_> = reports.iter().flat_map(|r| &r.problems).collect();
    assert!(
        problems
            .iter()
            .any(|p| p.rule == "field_missing" && p.subject.as_deref() == Some("owner")),
        "{problems:?}"
    );
}

#[test]
fn the_requirements_are_the_same_after_the_migration() {
    let before = requirements(&parse_document(PLAN_EXAMPLE.as_bytes()));
    let after = parse_document(migrate(PLAN_EXAMPLE).text.as_bytes());
    let after: Vec<_> = after
        .blocks
        .iter()
        .filter_map(|b| requirement(b, false))
        .collect();
    let key =
        |r: &crate::spec_doc::Requirement| (r.id.clone(), r.priority, r.deprecated, r.body.clone());
    assert_eq!(
        before.iter().map(key).collect::<Vec<_>>(),
        after.iter().map(key).collect::<Vec<_>>()
    );
}

#[test]
fn a_deprecated_document_is_retired_without_a_date() {
    let text = "---\nspec: a\ntitle: t\nstatus: deprecated\n---\n::: req A-1\nx\n:::\n";
    let m = migrate(text);
    assert_eq!(
        m.text,
        "---\ndoctype: spec.system\nid: a\ntitle: t\nretired: {}\n# TODO(gstn doc fmt --migrate): owner: <the owning group>\n# TODO(gstn doc fmt --migrate): retired: date: YYYY-MM-DD (the day it was deprecated)\n---\n::: req A-1\nx\n:::\n"
    );
    assert_eq!(
        m.notes,
        [
            MigrationNote::OwnerMissing,
            MigrationNote::RetiredDateUnknown
        ]
    );
    // `retired: {}` is valid: the lint reports only the owner.
    let reports = lint(
        &[],
        &[Source {
            path: "a.md",
            bytes: m.text.as_bytes(),
        }],
    );
    let rules: Vec<_> = reports
        .iter()
        .flat_map(|r| &r.problems)
        .map(|p| p.rule)
        .filter(|r| *r != "doctype_unknown" && *r != "id_not_filename")
        .collect();
    assert_eq!(rules, ["field_missing"]);
}

#[test]
fn confidential_becomes_classification_and_stays_withheld() {
    let marked = migrate("---\nspec: s\ntitle: t\nstatus: approved\nconfidential: true\n---\n::: req APP-X-1\nsecret\n:::\n");
    assert!(
        marked.text.contains("\nclassification: confidential\n"),
        "{}",
        marked.text
    );
    let d = parse_document(marked.text.as_bytes());
    assert_eq!(
        d.classification(TypeDefault::Known(None)),
        Classification::Confidential
    );
    assert!(d.meta_field("confidential").is_none());
    let open = migrate("---\nspec: s\ntitle: t\nstatus: approved\nconfidential: false\n---\n");
    assert!(
        open.text.contains("\nclassification: internal\n"),
        "{}",
        open.text
    );
    let unmarked = migrate("---\nspec: s\ntitle: t\nstatus: approved\n---\n");
    assert!(
        !unmarked.text.contains("classification"),
        "{}",
        unmarked.text
    );
}

#[test]
fn a_migration_is_idempotent_through_the_format() {
    let once = migrate(PLAN_EXAMPLE).text;
    assert_eq!(
        migrate_legacy(once.as_bytes(), None),
        Err(FmtRefusal::NotOlderFormat),
        "a migrated file is in the newer format"
    );
    assert_eq!(
        format_document(once.as_bytes(), None).expect("formats"),
        once
    );
}

#[test]
fn anything_but_a_readable_older_file_is_refused() {
    let refuse = |text: &[u8]| migrate_legacy(text, None).err();
    assert_eq!(refuse(b"# README\n"), Some(FmtRefusal::NotOlderFormat));
    assert_eq!(
        refuse(b"---\ndoctype: t.x\nid: A-1\ntitle: T\nowner: g\n---\n"),
        Some(FmtRefusal::NotOlderFormat)
    );
    assert_eq!(
        refuse(b"---\ndoctype: t.x\nspec: a\ntitle: T\n---\n"),
        Some(FmtRefusal::NotOlderFormat)
    );
    // An older front matter that breaks a rule is fixed first.
    for broken in [
        "---\nspec: a\ntitle: t\nstatus: nope\n---\n",
        "---\nspec: a\ntitle: t\n---\n",
        "---\nspec: a\ntitle: t\nstatus: draft\nkind: x\n---\n",
        "---\nspec: a\ntitle: t\nstatus: draft\nconfidential: maybe\n---\n",
    ] {
        assert_eq!(
            refuse(broken.as_bytes()),
            Some(FmtRefusal::OlderFormatInvalid),
            "{broken}"
        );
    }
    assert_eq!(refuse(&[0xff, 0xfe]), Some(FmtRefusal::NotUtf8));
    let big = format!(
        "---\nspec: a\ntitle: t\nstatus: draft\n---\n{}",
        "x".repeat(super::MAX_DOCUMENT_BYTES)
    );
    assert_eq!(refuse(big.as_bytes()), Some(FmtRefusal::TooLarge));
}

#[test]
fn comments_line_endings_and_the_byte_order_mark_are_kept() {
    let text = "\u{feff}---\r\n# 設定の仕様\r\nspec: a   # the name\r\ntitle: t\r\n\r\n# who\r\nowners:\r\n  - x@example.internal\r\n  - y@example.internal\r\nstatus: approved\r\n---\r\n本文。\r\n";
    let m = migrate(text);
    assert_eq!(
        m.text,
        "\u{feff}---\r\ndoctype: spec.system\r\n# 設定の仕様\r\nid: a   # the name\r\ntitle: t\r\n\r\n# who\r\n# TODO(gstn doc fmt --migrate): owner: <the owning group>\r\n# (the older file named people: x@example.internal, y@example.internal)\r\n---\r\n本文。\r\n"
    );
}

/// A migrated file is read by `spec_doc` as the older one was (plan 13
/// `doctype: spec.system`): the same name, title and requirements, no
/// problem (the owner TODO is doc-lint's), not withheld.
#[test]
fn a_migrated_file_is_read_by_the_spec_reader_as_before() {
    let m = migrate(PLAN_EXAMPLE);
    let older = crate::spec_doc::parse_spec_document(m.text.as_bytes());
    assert!(older.problems.is_empty(), "{:?}", older.problems);
    let meta = older.meta.as_ref().expect("meta");
    assert_eq!(meta.spec, "zeva-engine/config");
    assert_eq!(older.requirements.len(), 2);
    assert!(!older.confidential());
}

/// Review (0d59521, MEDIUM): `status: draft` / `review` would read
/// `approved` after a migration: refused unless the loss is accepted, and
/// then the older value stays in a TODO comment.
#[test]
fn a_draft_or_review_status_is_not_lost_silently() {
    for status in ["draft", "review"] {
        let text = format!("---\nspec: a\ntitle: t\nstatus: {status}\n---\n::: req A-1\nx\n:::\n");
        assert_eq!(
            migrate_legacy(text.as_bytes(), None),
            Err(FmtRefusal::StatusWouldBeLost),
            "{status}"
        );
        let m = migrate_legacy_with(text.as_bytes(), None, true).expect("accepted");
        assert!(
            m.text.contains(&format!(
                "\n# TODO(gstn doc fmt --migrate): the older file said status: {status} ("
            )),
            "{}",
            m.text
        );
        assert_eq!(
            m.notes,
            [MigrationNote::OwnerMissing, MigrationNote::StatusDropped]
        );
    }
    assert_eq!(
        FmtRefusal::StatusWouldBeLost.as_str(),
        "status_would_be_lost"
    );
}
