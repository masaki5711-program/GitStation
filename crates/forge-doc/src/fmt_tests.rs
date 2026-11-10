#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)] // tests may fail loudly

use super::{format_document, FmtRefusal};
use crate::doctype::{parse_doctype, Doctype};

const QP: &str = include_str!("../../../docs/plan/13-examples/qms/procedures/QP-07-001.md");
const RA: &str = include_str!("../../../docs/plan/13-examples/safety/RA-2026-014.md");
const URS: &str = include_str!("../../../docs/plan/13-examples/equipment/URS-L3-001.md");

fn ty(name: &str) -> Doctype {
    let text = match name {
        "qms.procedure" => {
            include_str!("../../../docs/plan/13-examples/.forge/doctypes/qms.procedure.yml")
        }
        "safety.risk-assessment" => include_str!(
            "../../../docs/plan/13-examples/.forge/doctypes/safety.risk-assessment.yml"
        ),
        _ => include_str!("../../../docs/plan/13-examples/.forge/doctypes/equipment.urs.yml"),
    };
    parse_doctype(name, text.as_bytes()).expect("doctype")
}

fn fmt(text: &str, t: Option<&Doctype>) -> String {
    format_document(text.as_bytes(), t).expect("formats")
}

#[test]
fn the_examples_are_already_formatted() {
    for (text, name) in [
        (QP, "qms.procedure"),
        (RA, "safety.risk-assessment"),
        (URS, "equipment.urs"),
    ] {
        let t = ty(name);
        assert_eq!(fmt(text, Some(&t)), text, "{name}");
        assert_eq!(fmt(text, None), text, "{name} without its type");
    }
}

const FRONT: &str = "---\ndoctype: t.x\nid: A-1\ntitle: T\nowner: g\n---\n";

#[test]
fn a_line_is_split_after_each_sentence_outside_brackets_code_and_links() {
    let src = format!(
        "{FRONT}一つ目。二つ目！三つ目？\
         「中の。」は分けない。（括弧の。）も。`code。x` と [文。](https://example.internal/a。b) と [[A-1]]。終わり。\n"
    );
    let want = format!(
        "{FRONT}一つ目。\n二つ目！\n三つ目？\n\
         「中の。」は分けない。\n（括弧の。）も。\n`code。x` と [文。](https://example.internal/a。b) と [[A-1]]。\n終わり。\n"
    );
    assert_eq!(fmt(&src, None), want);
}

#[test]
fn lists_and_quotes_keep_their_continuation() {
    let src = format!("{FRONT}- 一。二。\n  1. 三。四。\n> 五。六。\n");
    let want = format!("{FRONT}- 一。\n  二。\n  1. 三。\n     四。\n> 五。\n> 六。\n");
    assert_eq!(fmt(&src, None), want);
}

#[test]
fn tables_headings_fences_comments_and_code_lines_are_not_split() {
    let src = format!(
        "{FRONT}## 見出し。続き。\n| a。b。 | c |\n```\n一。二。\n```\n<!-- 一。二。 -->\n<!--\n一。二。\n-->\n    一。二。\n"
    );
    assert_eq!(fmt(&src, None), src);
}

#[test]
fn front_matter_keys_follow_the_base_then_the_type_order() {
    let t = ty("safety.risk-assessment");
    let src = "---\n# 合成\nequipment: URS-L3-001\nother: x\ntitle: T\nregulations:\n  - JIS\nid: RA-2026-001\ndoctype: safety.risk-assessment\nowner: g\n---\n本文。\n";
    let want = "---\ndoctype: safety.risk-assessment\nid: RA-2026-001\ntitle: T\nowner: g\nregulations:\n  - JIS\n# 合成\nequipment: URS-L3-001\nother: x\n---\n本文。\n";
    assert_eq!(fmt(src, Some(&t)), want);
}

#[test]
fn block_attributes_follow_the_type_order_and_keep_their_quoting() {
    let t = ty("equipment.urs");
    let src = format!(
        "{FRONT}::: decision  D-01   status=deprecated owner=\"設備 の担当\" required=true state=decided\n本文。\n:::\n"
    );
    let want = format!(
        "{FRONT}::: decision D-01 state=decided required=true owner=\"設備 の担当\" status=deprecated\n本文。\n:::\n"
    );
    assert_eq!(fmt(&src, Some(&t)), want);
    // A header that is not well formed stays as written.
    let bad = format!("{FRONT}::: decision D-01 state=\"open\n本文。\n:::\n");
    assert_eq!(fmt(&bad, Some(&t)), bad);
}

#[test]
fn formatting_is_idempotent_and_keeps_crlf_and_the_bom() {
    let src = format!("\u{feff}{}一。二。\r\n", FRONT.replace('\n', "\r\n"));
    let once = fmt(&src, None);
    assert_eq!(
        once,
        format!("\u{feff}{}一。\r\n二。\r\n", FRONT.replace('\n', "\r\n"))
    );
    assert_eq!(fmt(&once, None), once);
    for text in [QP, RA, URS] {
        let shuffled = text.replace("。", "。 ");
        let a = fmt(&shuffled, None);
        assert_eq!(fmt(&a, None), a);
    }
}

#[test]
fn what_is_no_document_is_refused_not_guessed() {
    let refuse = |b: &[u8]| format_document(b, None).err();
    assert_eq!(refuse(b"# README\n"), Some(FmtRefusal::NotADocument));
    // The older format is changed only by an explicit migration.
    assert_eq!(
        refuse(b"---\nspec: a/b\ntitle: T\nstatus: draft\n---\n"),
        Some(FmtRefusal::NotADocument)
    );
    assert_eq!(
        refuse(b"---\ndoctype: [\n---\n"),
        Some(FmtRefusal::NotADocument)
    );
    assert_eq!(refuse(&[0xff]), Some(FmtRefusal::NotADocument));
    let big = vec![b'a'; crate::document::MAX_DOCUMENT_BYTES + 1];
    assert_eq!(refuse(&big), Some(FmtRefusal::TooLarge));
}

/// Format, then check the output is a document again and a fixed point.
fn fixed(text: &str) -> String {
    let once = fmt(text, None);
    assert_eq!(fmt(&once, None), once, "not idempotent for {text:?}");
    once
}

#[test]
fn a_sequence_at_column_zero_stays_with_its_key() {
    let src =
        "---\nrelated:\n- A-2\n- A-3\nlang: ja\nowner: g\ndoctype: t.x\nid: A-1\ntitle: T\n---\n";
    let want =
        "---\ndoctype: t.x\nid: A-1\ntitle: T\nowner: g\nrelated:\n- A-2\n- A-3\nlang: ja\n---\n";
    assert_eq!(fixed(src), want);
    // A flow collection closing at column 0.
    let src = "---\nrelated: [\n  A-2,\n]\ndoctype: t.x\nid: A-1\ntitle: T\nowner: g\n---\n";
    let out = fixed(src);
    assert!(
        crate::document::parse_document(out.as_bytes())
            .meta
            .is_some(),
        "{out}"
    );
}

#[test]
fn a_reorder_that_would_change_the_yaml_keeps_it_as_written() {
    // A block scalar whose body sits at column 0 is no valid YAML anyway;
    // a key-looking line inside a literal block is kept with its key.
    let src = "---\ntitle: |\n  一行目\n  doctype: x\ndoctype: t.x\nid: A-1\nowner: g\n---\n";
    let want = "---\ndoctype: t.x\nid: A-1\ntitle: |\n  一行目\n  doctype: x\nowner: g\n---\n";
    assert_eq!(fixed(src), want);
}

#[test]
fn quotes_keep_their_depth_and_lines_keep_a_hard_break() {
    assert_eq!(
        fixed(&format!("{FRONT}> > あ。い。\n")),
        format!("{FRONT}> > あ。\n> > い。\n")
    );
    assert_eq!(
        fixed(&format!("{FRONT}あ。い。  \n")),
        format!("{FRONT}あ。\nい。  \n")
    );
}

#[test]
fn no_cut_turns_the_rest_into_another_block() {
    for rest in [
        "- い",
        "# い",
        "---",
        "1. い",
        "> い",
        "| い",
        "::: note",
        "<b>い</b>",
        "*",
        "=== ",
    ] {
        let src = format!("{FRONT}あ。{rest}\n");
        assert_eq!(fixed(&src), src, "{rest}");
    }
}

#[test]
fn tables_without_leading_pipes_and_html_lines_are_kept() {
    let src = format!("{FRONT}a | b\n--|--\nあ。い。 | x\n\n<pre>あ。い。</pre>\n");
    assert_eq!(fixed(&src), src);
    // After the blank line prose is prose again.
    let src = format!("{FRONT}|a|b|\n|-|-|\n|1|2|\n\nあ。い。\n");
    assert_eq!(
        fixed(&src),
        format!("{FRONT}|a|b|\n|-|-|\n|1|2|\n\nあ。\nい。\n")
    );
}

#[test]
fn a_code_span_ends_only_on_its_own_run() {
    let src = format!("{FRONT}`a``。b` と続く。終わり。\n");
    assert_eq!(fixed(&src), format!("{FRONT}`a``。b` と続く。\n終わり。\n"));
}
