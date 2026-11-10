#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)] // tests may fail loudly

use super::{parse_document, Attr, Classification, Document};

fn doc(text: &str) -> Document {
    parse_document(text.as_bytes())
}

fn rules(d: &Document) -> Vec<&'static str> {
    d.problems.iter().map(|p| p.rule).collect()
}

const FRONT: &str = "---\ndoctype: t.x\nid: A-1\ntitle: T\nowner: g\n---\n";

#[test]
fn a_block_header_reads_words_and_quoted_values() {
    let d = doc(&format!(
        "{FRONT}::: step S-1 role=作成者 note=\"品質 保証の\\\"担当\\\"\" path=\"a\\\\b\"\n本文。\n:::\n"
    ));
    assert_eq!(rules(&d), Vec::<&str>::new());
    let b = d.blocks.first().expect("block");
    assert_eq!((b.kind.as_str(), b.id.as_deref()), ("step", Some("S-1")));
    assert_eq!(
        b.attrs,
        vec![
            Attr {
                key: "role".into(),
                value: "作成者".into()
            },
            Attr {
                key: "note".into(),
                value: "品質 保証の\"担当\"".into()
            },
            Attr {
                key: "path".into(),
                value: "a\\b".into()
            },
        ]
    );
    assert_eq!((b.line, b.end_line, b.body.as_str()), (7, 9, "本文。"));
}

#[test]
fn a_bad_block_header_is_refused_with_its_rule() {
    for (header, rule) in [
        ("::: Step S-1", "block_kind_invalid"),
        ("::: step S-1 role=a role=b", "attr_duplicate"),
        ("::: step S-1 Role=a", "attr_invalid"),
        ("::: step S-1 extra", "attr_invalid"),
        ("::: step S-1 role=\"unclosed", "attr_invalid"),
        ("::: step S-1 role=\"a\"b", "attr_invalid"),
        ("::: step S-1 role=\"\"", "attr_invalid"),
        ("::: step S-1 role=\"a\\x\"", "attr_invalid"),
        ("::: step S-1 role=", "attr_invalid"),
    ] {
        let d = doc(&format!("{FRONT}{header}\n本文。\n:::\n"));
        assert!(rules(&d).contains(&rule), "{header}: {:?}", rules(&d));
        assert!(d.blocks.is_empty(), "{header}");
    }
}

#[test]
fn plain_containers_nest_but_blocks_with_ids_do_not() {
    let d = doc(&format!(
        "{FRONT}::: step S-2 role=a\n一。\n\n::: note\n二。\n:::\n:::\n::: step S-3 role=a\n::: step S-4 role=b\n:::\n:::\n"
    ));
    assert_eq!(rules(&d), vec!["block_nested"]);
    let s2 = d.block("S-2").expect("S-2");
    // The note's text is the step's text too.
    assert_eq!(s2.body, "一。\n\n二。");
    assert!(d.block("S-4").is_none());
    assert_eq!(d.blocks.iter().filter(|b| b.id.is_none()).count(), 1);
}

#[test]
fn nothing_inside_a_code_fence_is_a_block_or_a_reference() {
    let d = doc(&format!(
        "{FRONT}```markdown\n::: step S-9\n[[NOPE-1]] <div>\n:::\n```\n~~~\n## not a heading\n~~~\n"
    ));
    assert_eq!(rules(&d), Vec::<&str>::new());
    assert!(d.blocks.is_empty() && d.refs.is_empty() && d.headings.is_empty());
    let d = doc(&format!("{FRONT}```\n::: step S-9\n"));
    assert_eq!(rules(&d), vec!["unclosed_fence"]);
}

#[test]
fn unclosed_and_unmatched_blocks_are_problems() {
    let d = doc(&format!("{FRONT}:::\n::: step S-1 role=a\n本文。\n"));
    assert_eq!(rules(&d), vec!["close_unmatched", "block_unclosed"]);
    let b = d.block("S-1").expect("kept");
    assert_eq!((b.end_line, b.body.as_str()), (10, "本文。"));
}

#[test]
fn references_are_read_outside_code_spans_and_comments() {
    let d = doc(&format!(
        "{FRONT}見る [[QP-07-001]] と [[RA-2026-014#H-03]]、[[specs/zeva-engine:ZEVA-CFG-012]]。\n\
         `[[CODE-1]]` <!-- [[HIDDEN-1]]\n[[HIDDEN-2]] --> [[AFTER-1]]\n[[bad id]] [[A:]]\n"
    ));
    let got: Vec<(Option<&str>, &str, Option<&str>, usize)> = d
        .refs
        .iter()
        .map(|r| {
            (
                r.repo.as_deref(),
                r.doc.as_str(),
                r.block.as_deref(),
                r.line,
            )
        })
        .collect();
    assert_eq!(
        got,
        vec![
            (None, "QP-07-001", None, 7),
            (None, "RA-2026-014", Some("H-03"), 7),
            (Some("specs/zeva-engine"), "ZEVA-CFG-012", None, 7),
            (None, "AFTER-1", None, 9),
        ]
    );
    assert_eq!(rules(&d), vec!["ref_invalid", "ref_invalid"]);
}

#[test]
fn raw_html_is_refused_but_comments_and_autolinks_are_not() {
    let d = doc(&format!(
        "{FRONT}<!-- 合成 -->\n<https://example.internal/x> と <mailto:a@example.internal>\na < b、1<2\n`<b>` は文字\n<div>x</div>\n<!--\n<div>\n-->\n"
    ));
    assert_eq!(
        d.problems
            .iter()
            .map(|p| (p.line, p.rule))
            .collect::<Vec<_>>(),
        vec![(11, "html_not_allowed")]
    );
    let d = doc(&format!("{FRONT}<!-- never closed\n"));
    assert_eq!(rules(&d), vec!["unclosed_comment"]);
}

#[test]
fn headings_set_the_section_of_the_blocks_under_them() {
    let d = doc(&format!(
        "{FRONT}# 題\n\n## 手順 ##\n\n::: step S-1 role=a\n一。\n:::\n\n### 細目\n\n::: step S-2 role=a\n二。\n:::\n\n##見出しでない\n"
    ));
    let h: Vec<(u8, &str)> = d
        .headings
        .iter()
        .map(|h| (h.level, h.text.as_str()))
        .collect();
    assert_eq!(h, vec![(1, "題"), (2, "手順"), (3, "細目")]);
    // A level-3 heading does not change the section.
    assert_eq!(d.block("S-1").and_then(|b| b.section), Some(1));
    assert_eq!(d.block("S-2").and_then(|b| b.section), Some(1));
}

#[test]
fn the_front_matter_is_kept_as_written_with_its_lines() {
    let d = doc("---\ndoctype: t.x\nid: A-1\nrelated:\n  - B-1\n  - C-1\n---\n本文\n");
    assert_eq!(d.doctype(), Some("t.x"));
    let related = d.meta_field("related").expect("related");
    assert_eq!(related.line, 4);
    let super::Value::List(items) = &related.value else {
        panic!("a list");
    };
    let lines: Vec<usize> = items.iter().map(|(l, _)| *l).collect();
    assert_eq!(lines, vec![5, 6]);
    for (text, rule) in [
        (
            "---\ndoctype: a\ndoctype: b\n---\n",
            "front_matter_duplicate_key",
        ),
        ("---\na: &x 1\n---\n", "front_matter_yaml_feature"),
        ("---\n- a\n---\n", "front_matter_not_mapping"),
        ("---\ndoctype: a\n", "front_matter_unclosed"),
        ("---\n---\n", "front_matter_empty"),
        ("---\na: [\n---\n", "front_matter_syntax"),
    ] {
        assert_eq!(rules(&doc(text)), vec![rule], "{text}");
    }
}

#[test]
fn classification_fails_closed() {
    let with = |extra: &str| doc(&format!("---\ndoctype: t.x\n{extra}---\n"));
    assert_eq!(with("").classification(None), Classification::Internal);
    assert_eq!(
        with("").classification(Some(Classification::Public)),
        Classification::Public
    );
    assert_eq!(
        with("classification: public\n").classification(None),
        Classification::Public
    );
    // The older mark (ADR-0026 §6).
    assert_eq!(
        with("confidential: true\n").classification(None),
        Classification::Confidential
    );
    assert_eq!(
        with("confidential: false\n").classification(None),
        Classification::Internal
    );
    // Both, an unknown word, an odd old value, an unreadable front matter.
    assert_eq!(
        with("classification: public\nconfidential: false\n").classification(None),
        Classification::Confidential
    );
    assert_eq!(
        with("classification: secret\n").classification(None),
        Classification::Confidential
    );
    assert_eq!(
        with("confidential: yes\n").classification(None),
        Classification::Confidential
    );
    assert_eq!(
        doc("---\na: [\n---\n").classification(Some(Classification::Public)),
        Classification::Confidential
    );
    assert_eq!(
        doc("no front matter").classification(None),
        Classification::Internal
    );
    assert_eq!(
        parse_document(&[0xff, 0xfe]).classification(None),
        Classification::Confidential
    );
}

#[test]
fn bytes_that_are_no_document_are_problems_not_panics() {
    assert_eq!(rules(&parse_document(&[0xff])), vec!["not_utf8"]);
    let big = vec![b'a'; super::MAX_DOCUMENT_BYTES + 1];
    assert_eq!(rules(&parse_document(&big)), vec!["too_large"]);
    // An editor's byte order mark and CRLF line ends.
    let d = parse_document(b"\xEF\xBB\xBF---\r\ndoctype: t.x\r\n---\r\n");
    assert_eq!(d.doctype(), Some("t.x"));
}
