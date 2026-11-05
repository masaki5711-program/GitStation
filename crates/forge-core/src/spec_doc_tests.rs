#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! Structured specification documents (M3 X4, plan 03 §4.2).

use super::{parse_spec_document, Priority, RequirementId, SpecDocument, SpecStatus};

fn doc(text: &str) -> SpecDocument {
    parse_spec_document(text.as_bytes())
}

fn rules(d: &SpecDocument) -> Vec<(usize, &'static str)> {
    d.problems.iter().map(|p| (p.line, p.rule)).collect()
}

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

#[test]
fn a_requirement_block_is_parsed_with_its_line_attributes_and_body() {
    let d = doc(PLAN_EXAMPLE);
    assert!(d.problems.is_empty(), "{:?}", d.problems);
    assert!(d.is_spec());
    let meta = d.meta.as_ref().expect("front matter");
    assert_eq!(meta.spec, "zeva-engine/config");
    assert_eq!(meta.title, "設定管理");
    assert_eq!(meta.status, SpecStatus::Approved);
    assert_eq!(meta.owners, vec!["po@example.internal".to_owned()]);
    assert_eq!(d.requirements.len(), 2);
    let first = &d.requirements[0];
    assert_eq!(first.id.as_str(), "ZEVA-CFG-012");
    assert_eq!(first.line, 10);
    assert_eq!(first.end_line, 13);
    assert_eq!(first.priority, None);
    assert!(!first.deprecated);
    assert_eq!(
        first.body,
        "設定ファイルの変更は、再起動せずに 10 秒以内に反映されること。\n受入: 変更から反映までの時間を計測するテストがある。"
    );
    let second = &d.requirements[1];
    assert_eq!(second.id.as_str(), "ZEVA-CFG-013");
    assert_eq!(second.line, 15);
    assert_eq!(second.priority, Some(Priority::Must));
}

#[test]
fn front_matter_needs_spec_title_and_a_known_status() {
    let missing = doc("---\nspec: a\ntitle: t\n---\n");
    assert_eq!(rules(&missing), vec![(1, "status_missing")]);
    let bad = doc("---\nspec: a\ntitle: t\nstatus: done\n---\n");
    assert_eq!(rules(&bad), vec![(4, "status_invalid")]);
    let unknown = doc("---\nspec: a\ntitle: t\nstatus: draft\nkind: spec\n---\n");
    assert_eq!(rules(&unknown), vec![(5, "front_matter_unknown_key")]);
    let no_spec = doc("---\ntitle: t\nstatus: draft\n---\n");
    assert_eq!(rules(&no_spec), vec![(1, "spec_missing")]);
    let no_title = doc("---\nspec: a\nstatus: draft\n---\n");
    assert_eq!(rules(&no_title), vec![(1, "title_missing")]);
    let unclosed = doc("---\nspec: a\n");
    assert_eq!(rules(&unclosed), vec![(1, "front_matter_unclosed")]);
    let yaml = doc("---\nspec: [a\n---\n");
    assert_eq!(
        rules(&yaml).first().map(|p| p.1),
        Some("front_matter_syntax")
    );
    let owners = doc("---\nspec: a\ntitle: t\nstatus: draft\nowners: x\n---\n");
    assert_eq!(rules(&owners), vec![(5, "owners_invalid")]);
}

#[test]
fn a_block_inside_a_code_fence_is_not_a_requirement() {
    let d = doc(
        "---\nspec: a\ntitle: t\nstatus: draft\n---\n```markdown\n::: req A-1\nx\n:::\n```\n~~~~\n::: req A-2\n~~~~\n",
    );
    assert!(d.problems.is_empty(), "{:?}", d.problems);
    assert!(d.requirements.is_empty());
}

#[test]
fn block_rules_are_problems_with_their_line() {
    let head = "---\nspec: a\ntitle: t\nstatus: draft\n---\n";
    let cases: &[(&str, (usize, &str))] = &[
        ("::: req A-1\nbody\n", (6, "block_unclosed")),
        (
            "::: req A-1\n::: req A-2\nx\n:::\n:::\n",
            (7, "block_nested"),
        ),
        ("::: req a-1\nx\n:::\n", (6, "requirement_id_invalid")),
        ("::: req\nx\n:::\n", (6, "requirement_id_missing")),
        ("::: req A-1 color=red\nx\n:::\n", (6, "attribute_unknown")),
        (
            "::: req A-1 priority=must priority=should\nx\n:::\n",
            (6, "attribute_duplicate"),
        ),
        (
            "::: req A-1 priority=high\nx\n:::\n",
            (6, "priority_invalid"),
        ),
        (
            "::: req A-1 status=gone\nx\n:::\n",
            (6, "requirement_status_invalid"),
        ),
        ("::: req A-1 must\nx\n:::\n", (6, "attribute_invalid")),
        ("::: req A-1\n\n:::\n", (6, "body_empty")),
        (":::\n", (6, "close_unmatched")),
    ];
    for (body, want) in cases {
        let d = doc(&format!("{head}{body}"));
        assert_eq!(rules(&d), vec![*want], "{body}");
    }
}

#[test]
fn a_file_without_front_matter_or_blocks_is_no_spec_document() {
    let d = doc("# README\n\nSee the specs.\n");
    assert!(!d.is_spec());
    assert!(d.problems.is_empty());
}

#[test]
fn blocks_without_front_matter_are_a_problem() {
    let d = doc("# x\n::: req A-1\nbody\n:::\n");
    assert!(d.is_spec());
    assert_eq!(rules(&d), vec![(1, "front_matter_missing")]);
    assert_eq!(d.requirements.len(), 1, "the blocks are still read");
}

#[test]
fn requirement_ids_are_upper_case_segments() {
    for ok in ["ZEVA-CFG-012", "A-1", "AB1-X2-3", "ZEVA-CFG-NET-1"] {
        assert!(RequirementId::parse(ok).is_some(), "{ok}");
    }
    for bad in [
        "",
        "A",
        "a-1",
        "A--1",
        "-A-1",
        "A-1-",
        "1A-2",
        "A_1",
        "A-1*",
        "PBI-1/AC-2",
        "PBI-12",
        "PBI-X-1",
    ] {
        assert!(RequirementId::parse(bad).is_none(), "{bad}");
    }
    assert!(RequirementId::parse(&format!("A-{}", "1".repeat(62))).is_some());
    assert!(RequirementId::parse(&format!("A-{}", "1".repeat(63))).is_none());
    let id = RequirementId::parse("ZEVA-CFG-012").expect("id");
    assert_eq!(id.family(), "ZEVA-CFG");
    assert_eq!(RequirementId::parse("A-1").expect("id").family(), "A");
}

#[test]
fn a_deprecated_document_deprecates_its_requirements() {
    let d = doc("---\nspec: a\ntitle: t\nstatus: deprecated\n---\n::: req A-1\nx\n:::\n");
    assert!(d.requirements[0].deprecated);
    let attr = doc("---\nspec: a\ntitle: t\nstatus: approved\n---\n::: req A-1 status=deprecated\nx\n:::\n::: req A-2 status=active\ny\n:::\n");
    assert!(attr.problems.is_empty(), "{:?}", attr.problems);
    assert!(attr.requirements[0].deprecated);
    assert!(!attr.requirements[1].deprecated);
}

#[test]
fn crlf_line_endings_are_read_like_lf() {
    let d = doc(&PLAN_EXAMPLE.replace('\n', "\r\n"));
    assert!(d.problems.is_empty(), "{:?}", d.problems);
    assert_eq!(d.requirements.len(), 2);
    assert!(!d.requirements[0].body.contains('\r'));
}

#[test]
fn a_file_that_is_not_utf8_is_a_problem() {
    let d = parse_spec_document(&[0xff, 0xfe, b'\n']);
    assert!(d.is_spec());
    assert_eq!(rules(&d), vec![(0, "not_utf8")]);
}

#[test]
fn other_containers_nest_and_close_without_ending_a_requirement() {
    let d = doc(
        "---\nspec: a\ntitle: t\nstatus: draft\n---\n::: note\n::: req A-1\nx\n:::\ntext\n:::\n",
    );
    assert!(d.problems.is_empty(), "{:?}", d.problems);
    assert_eq!(d.requirements.len(), 1);
    assert_eq!(d.requirements[0].end_line, 9);
}

#[test]
fn a_body_over_the_limit_is_a_problem() {
    let long = "x".repeat(super::MAX_REQUIREMENT_BODY_BYTES + 1);
    let d = doc(&format!(
        "---\nspec: a\ntitle: t\nstatus: draft\n---\n::: req A-1\n{long}\n:::\n"
    ));
    assert_eq!(rules(&d), vec![(6, "body_too_long")]);
}
