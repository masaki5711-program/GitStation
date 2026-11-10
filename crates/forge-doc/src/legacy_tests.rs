#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)] // tests may fail loudly

//! The older and the newer parser agree on the older format (ADR-0026 §7):
//! generated documents (synthetic) built from the lines the older format
//! uses — front matter, `::: req` with attributes, plain `::: note`
//! containers, code fences, prose, closes — give the same requirements
//! whenever the older parser reports no problem.

use proptest::prelude::*;

use super::{identity, is_legacy, requirements, SPEC_SYSTEM};
use crate::document::parse_document;
use crate::spec_doc::parse_spec_document;

const FRONT: &str = "---\nspec: zeva-engine/config\ntitle: 設定管理\nstatus: approved\nowners: [po@example.internal]\n---\n";

#[test]
fn the_older_format_is_the_spec_system_type() {
    let d = parse_document(FRONT.as_bytes());
    assert!(is_legacy(&d));
    assert_eq!(identity(&d), Some(("zeva-engine/config", SPEC_SYSTEM)));
    let new = parse_document(b"---\ndoctype: t.x\nspec: a\n---\n");
    assert!(!is_legacy(&new) && identity(&new).is_none());
}

#[test]
fn a_deprecated_document_deprecates_its_requirements_in_both() {
    let text = format!(
        "{}\n::: req ZEVA-CFG-001 priority=must\n一。\n:::\n",
        FRONT.replace("status: approved", "status: deprecated")
    );
    let old = parse_spec_document(text.as_bytes());
    let new = requirements(&parse_document(text.as_bytes()));
    assert_eq!(new, old.requirements);
    assert!(new.iter().all(|r| r.deprecated));
}

/// Prose and code lines (never a block marker at a line start).
fn prose() -> impl Strategy<Value = Vec<String>> {
    prop::collection::vec(
        prop_oneof![
            Just("要件の本文。".to_owned()),
            Just("- 箇条 [[ZEVA-CFG-001]]".to_owned()),
            Just(String::new()),
            Just("  字下げの行".to_owned()),
            Just("`::: req` は例".to_owned()),
            Just("```\n::: req IN-FENCE-1\n```".to_owned()),
            Just("~~~~\n:::\n~~~~".to_owned()),
        ],
        0..4,
    )
}

/// A well-formed segment: prose, a requirement (with notes inside), or a
/// note around requirements.
fn requirement() -> impl Strategy<Value = String> {
    let id = prop_oneof![
        Just("ZEVA-CFG-001"),
        Just("ZEVA-CFG-002"),
        Just("AB-12-X3"),
        Just("Q-9"),
    ];
    let attrs = prop_oneof![
        Just(""),
        Just(" priority=must"),
        Just(" priority=could status=deprecated"),
        Just(" status=active"),
        Just(" status=deprecated priority=wont"),
    ];
    let req = (id, attrs, prose(), prose(), any::<bool>()).prop_map(|(id, a, p1, p2, note)| {
        let mut out = vec![format!("::: req {id}{a}")];
        out.extend(p1);
        out.push("本文がある。".to_owned());
        if note {
            out.push("::: note".to_owned());
            out.extend(p2);
            out.push(":::".to_owned());
        }
        out.push(":::".to_owned());
        out.join("\n")
    });
    req
}

fn segment() -> impl Strategy<Value = String> {
    prop_oneof![
        2 => prose().prop_map(|p| p.join("\n")),
        3 => requirement(),
        1 => prop::collection::vec(requirement(), 1..3)
            .prop_map(|rs| format!("::: note\n{}\n:::", rs.join("\n"))),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn both_parsers_give_the_same_requirements(parts in prop::collection::vec(segment(), 0..8)) {
        let text = format!("{FRONT}{}\n", parts.join("\n"));
        let old = parse_spec_document(text.as_bytes());
        // Only documents the older parser reads cleanly are compared: where
        // it reports a problem, the newer one may report it differently.
        prop_assume!(old.problems.is_empty());
        let new = parse_document(text.as_bytes());
        // The newer parser may only be stricter in the documented ways: an
        // unclosed plain container (`::: note`) is reported.
        for p in &new.problems {
            let plain = new.blocks.iter().any(|b| b.line == p.line && b.id.is_none());
            prop_assert!(p.rule == "block_unclosed" && plain, "{:?}\n{text}", new.problems);
        }
        prop_assert_eq!(requirements(&new), old.requirements, "{}", text);
    }
}
