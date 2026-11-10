#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)] // tests may fail loudly

//! The document view on the synthetic examples of plan 13
//! (`docs/plan/13-examples/`, synthetic data only).

use super::{join_cjk_lines, view_with_body, Piece, Resolver, Target, TargetDoc, VIEW_VERSION};
use crate::document::{parse_document, Classification, TypeDefault};

const QP: &str = include_str!("../../../docs/plan/13-examples/qms/procedures/QP-07-001.md");
const RA: &str = include_str!("../../../docs/plan/13-examples/safety/RA-2026-014.md");

/// The other examples as the server would know them.
struct Examples;

impl Resolver for Examples {
    fn document(&self, id: &str) -> Option<TargetDoc> {
        match id {
            "URS-L3-001" => Some(TargetDoc {
                title: "プレス機 No.3 更新 設備要求仕様書".into(),
                classification: Classification::Confidential,
                blocks: vec![("REQ-L3-012".into(), "req".into())],
            }),
            "QP-07-001" => Some(TargetDoc {
                title: "文書管理規程".into(),
                classification: Classification::Internal,
                blocks: vec![("T-01".into(), "term".into())],
            }),
            "RA-2026-014" => Some(TargetDoc {
                title: "プレス機 No.3 リスクアセスメント".into(),
                classification: Classification::Internal,
                blocks: vec![
                    ("H-01".into(), "hazard".into()),
                    ("H-02".into(), "hazard".into()),
                    ("H-03".into(), "hazard".into()),
                ],
            }),
            _ => None,
        }
    }
}

fn ra() -> super::DocumentView {
    view_with_body(
        RA,
        &parse_document(RA.as_bytes()),
        TypeDefault::Known(None),
        &Examples,
    )
}

#[test]
fn the_view_carries_the_front_matter_and_the_classification() {
    let v = ra();
    assert_eq!(v.version, VIEW_VERSION);
    assert_eq!(v.doctype.as_deref(), Some("safety.risk-assessment"));
    assert_eq!(v.id.as_deref(), Some("RA-2026-014"));
    assert_eq!(v.owner.as_deref(), Some("safety-team"));
    assert_eq!(v.classification, Classification::Internal);
    let keys: Vec<&str> = v.fields.iter().map(|f| f.key.as_str()).collect();
    assert_eq!(
        keys,
        vec!["review_cycle", "related", "regulations", "equipment"]
    );
}

#[test]
fn sections_hold_text_and_blocks_in_order() {
    let v = ra();
    let headings: Vec<Option<&str>> = v.sections.iter().map(|s| s.heading.as_deref()).collect();
    assert_eq!(
        headings,
        vec![
            None,
            Some("対象と範囲"),
            Some("危険源の特定とリスクの見積り"),
            Some("リスク低減措置"),
            Some("残留リスク"),
        ]
    );
    let hazards = &v.sections[2].pieces;
    let ids: Vec<&str> = hazards
        .iter()
        .filter_map(|p| match p {
            Piece::Block(b) => b.id.as_deref(),
            Piece::Text { .. } => None,
        })
        .collect();
    assert_eq!(ids, vec!["H-01", "H-02", "H-03"]);
    let Piece::Block(h1) = &hazards[0] else {
        panic!("a block first");
    };
    assert_eq!(h1.kind, "hazard");
    assert!(h1.attrs.contains(&("severity".to_owned(), "4".to_owned())));
    assert!(h1.body.starts_with("スライドの下降中に"));
    // The table under 対象と範囲 is text, kept as written.
    let Piece::Text { markdown, .. } = &v.sections[1].pieces[0] else {
        panic!("text first");
    };
    assert!(markdown.contains("| 値 | 重大さ | 起こりやすさ |"));
}

#[test]
fn a_nested_container_is_part_of_its_block() {
    let v = view_with_body(
        QP,
        &parse_document(QP.as_bytes()),
        TypeDefault::Known(None),
        &Examples,
    );
    let steps: Vec<&super::BlockView> = v
        .sections
        .iter()
        .flat_map(|s| s.pieces.iter())
        .filter_map(|p| match p {
            Piece::Block(b) if b.kind == "step" => Some(b),
            Piece::Block(_) | Piece::Text { .. } => None,
        })
        .collect();
    assert_eq!(steps.len(), 5);
    assert!(steps[1].body.contains("軽微な訂正"), "{}", steps[1].body);
    assert!(steps[4].deprecated);
    // No `note` block on its own.
    assert!(v
        .sections
        .iter()
        .flat_map(|s| s.pieces.iter())
        .all(|p| !matches!(p, Piece::Block(b) if b.kind == "note")));
}

#[test]
fn a_more_confidential_target_shows_its_id_only() {
    let v = ra();
    let to_urs: Vec<&Target> = v
        .refs
        .iter()
        .filter(|r| r.doc == "URS-L3-001")
        .map(|r| &r.target)
        .collect();
    assert!(!to_urs.is_empty());
    assert!(to_urs.iter().all(|t| **t == Target::Hidden), "{to_urs:?}");
    // The same reference from a confidential document is shown in full.
    let secret = RA.replace("classification: internal", "classification: confidential");
    let v = view_with_body(
        &secret,
        &parse_document(secret.as_bytes()),
        TypeDefault::Known(None),
        &Examples,
    );
    let r = v
        .refs
        .iter()
        .find(|r| r.doc == "URS-L3-001" && r.block.is_some())
        .expect("ref");
    assert_eq!(
        r.target,
        Target::Found {
            title: "プレス機 No.3 更新 設備要求仕様書".into(),
            block_kind: Some("req".into()),
        }
    );
}

#[test]
fn references_resolve_or_say_why_not() {
    let v = ra();
    let target = |doc: &str, block: Option<&str>| {
        v.refs
            .iter()
            .find(|r| r.doc == doc && r.block.as_deref() == block)
            .map(|r| r.target.clone())
            .expect("ref")
    };
    assert_eq!(
        target("RA-2026-014", Some("H-02")),
        Target::Found {
            title: "プレス機 No.3 リスクアセスメント".into(),
            block_kind: Some("hazard".into()),
        }
    );
    assert_eq!(
        target("QP-07-001", None),
        Target::Found {
            title: "文書管理規程".into(),
            block_kind: None,
        }
    );
    let broken = RA.replace(
        "[[RA-2026-014#H-02]]",
        "[[RA-2026-014#H-09]] [[NOPE-1]] [[specs/x:A-1]]",
    );
    let v = view_with_body(
        &broken,
        &parse_document(broken.as_bytes()),
        TypeDefault::Known(None),
        &Examples,
    );
    let of = |doc: &str| {
        v.refs
            .iter()
            .find(|r| r.doc == doc)
            .map(|r| r.target.clone())
    };
    assert_eq!(
        v.refs
            .iter()
            .find(|r| r.block.as_deref() == Some("H-09"))
            .map(|r| r.target.clone()),
        Some(Target::Broken)
    );
    assert_eq!(of("NOPE-1"), Some(Target::Broken));
    assert_eq!(of("A-1"), Some(Target::External));
}

#[test]
fn japanese_lines_join_without_a_space_and_others_with_one() {
    assert_eq!(join_cjk_lines("一つ目。\n二つ目。"), "一つ目。二つ目。");
    assert_eq!(join_cjk_lines("one.\ntwo."), "one. two.");
    assert_eq!(join_cjk_lines("規程は\nQMS に従う。"), "規程はQMS に従う。");
    assert_eq!(join_cjk_lines("see\n規程"), "see規程");
    // Paragraphs and hard breaks stay.
    assert_eq!(join_cjk_lines("一。\n\n二。"), "一。\n\n二。");
    assert_eq!(join_cjk_lines("一。  \n二。"), "一。  \n二。");
    assert_eq!(join_cjk_lines("一。\\\n二。"), "一。\\\n二。");
    // Three lines in a row.
    assert_eq!(join_cjk_lines("あ。\nい。\nう。"), "あ。い。う。");
    // List continuation indentation goes with the break.
    assert_eq!(join_cjk_lines("- あ。\n  い。"), "- あ。い。");
}
