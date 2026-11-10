#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)] // tests may fail loudly

use super::{duration_ok, name_ok, parse_doctype, Pattern, Rule, ValueType};

const URS: &str = include_str!("../../../docs/plan/13-examples/.forge/doctypes/equipment.urs.yml");
const QP: &str = include_str!("../../../docs/plan/13-examples/.forge/doctypes/qms.procedure.yml");
const RA: &str =
    include_str!("../../../docs/plan/13-examples/.forge/doctypes/safety.risk-assessment.yml");

#[test]
fn the_example_definitions_parse() {
    let urs = parse_doctype("equipment.urs", URS.as_bytes()).expect("urs");
    assert_eq!(urs.name, "equipment.urs");
    assert!(urs
        .id_pattern
        .as_ref()
        .is_some_and(|p| p.matches("URS-L3-001")));
    assert!(!urs
        .id_pattern
        .as_ref()
        .is_some_and(|p| p.matches("URS-L3-001x")));
    let decision = urs.block("decision").expect("decision");
    let state = decision
        .attrs
        .iter()
        .find(|a| a.key == "state")
        .expect("state");
    assert!(state.required);
    assert_eq!(
        state.ty,
        ValueType::Enum(vec!["open".into(), "decided".into(), "na".into()])
    );
    let covers = urs
        .block("check")
        .and_then(|b| b.attrs.first())
        .expect("covers");
    assert_eq!(
        covers.ty,
        ValueType::BlockRef {
            kind: Some("req".into())
        }
    );
    assert!(urs.has_rule(Rule::DecisionsResolved));
    assert_eq!(urs.sections.len(), 4);

    let qp = parse_doctype("qms.procedure", QP.as_bytes()).expect("qp");
    assert_eq!(qp.default_review_cycle.as_deref(), Some("24M"));
    let supersedes = qp.field("supersedes").expect("field");
    assert_eq!(
        supersedes.ty,
        ValueType::DocRef {
            doctype: Some("qms.procedure".into())
        }
    );
    let ra = parse_doctype("safety.risk-assessment", RA.as_bytes()).expect("ra");
    let sev = ra
        .block("hazard")
        .and_then(|b| b.attrs.first())
        .expect("severity");
    assert_eq!(
        sev.ty,
        ValueType::Int {
            min: Some(1),
            max: Some(4)
        }
    );
}

#[test]
fn a_definition_breaking_a_rule_is_refused_with_its_reason() {
    let base = "name: t.x\n";
    for (yaml, reason) in [
        ("name: t.y\n", "name_not_filename"),
        ("name: T.X\n", "name_invalid"),
        ("version: 1\n", "name_missing"),
        ("name: t.x\napprovers: [qa]\n", "unknown_key"),
        ("name: t.x\nid_pattern: '('\n", "pattern_invalid"),
        ("name: t.x\nrules: [no_such_rule]\n", "rule_unknown"),
        (
            "name: t.x\nfields: { a: { type: colour } }\n",
            "type_unknown",
        ),
        (
            "name: t.x\nfields: { a: { type: text, min: 1 } }\n",
            "fields_invalid",
        ),
        (
            "name: t.x\nfields: { a: { type: int, min: 5, max: 1 } }\n",
            "fields_invalid",
        ),
        (
            "name: t.x\nfields: { a: { type: blockref } }\n",
            "type_unknown",
        ),
        (
            "name: t.x\nblocks: { s: { attrs: { status: { type: text } } } }\n",
            "attr_reserved",
        ),
        (
            "name: t.x\nblocks: { s: { attrs: { a: { type: list } } } }\n",
            "type_unknown",
        ),
        (
            "name: t.x\nsections: [{ title: A, blocks: [nope] }]\n",
            "section_block_unknown",
        ),
        (
            "name: t.x\nsections: [{ title: A }, { title: A }]\n",
            "section_duplicate",
        ),
        ("name: t.x\nconcurrency_hint: whenever\n", "hint_invalid"),
        (
            "name: t.x\nfields: { owner: { type: text } }\n",
            "field_reserved",
        ),
        (
            "name: t.x\napproval_hint: { approve: qa }\n",
            "hint_invalid",
        ),
        (
            "name: t.x\ndefaults: { classification: secret }\n",
            "defaults_invalid",
        ),
        ("name: t.x\n---\nname: t.x\n", "multiple_documents"),
        ("name: t.x\nlabel: &a { ja: x }\n", "yaml_anchor"),
    ] {
        let got = parse_doctype("t.x", yaml.as_bytes())
            .map(|_| ())
            .map_err(|p| p.reason);
        assert_eq!(got, Err(reason), "{yaml}");
    }
    assert!(parse_doctype("t.x", base.as_bytes()).is_ok());
    assert_eq!(
        parse_doctype("t.x", &[0xff])
            .map(|_| ())
            .map_err(|p| p.reason),
        Err("not_utf8")
    );
}

#[test]
fn patterns_match_the_whole_id_and_are_bounded() {
    let p = Pattern::new("[A-Z]+-\\d{2}").expect("pattern");
    assert!(p.matches("AB-12"));
    assert!(!p.matches("xAB-12") && !p.matches("AB-123"));
    // An alternation cannot escape the anchors.
    let p = Pattern::new("A|B").expect("pattern");
    assert!(p.matches("A") && !p.matches("AB"));
    assert!(Pattern::new(&"a".repeat(super::MAX_PATTERN_LEN + 1)).is_none());
    assert!(Pattern::new("").is_none());
    assert!(Pattern::new("\\w{1000}{1000}").is_none());
    // A source that closes the group around it cannot escape the anchors.
    assert!(Pattern::new("a)|(b").is_none());
    assert!(Pattern::new("A)|.*|(B").is_none());
}

#[test]
fn names_and_durations() {
    assert!(name_ok("safety.risk-assessment") && name_ok("glossary"));
    assert!(!name_ok("Safety") && !name_ok("a..b") && !name_ok(".a") && !name_ok("1a"));
    assert!(duration_ok("24M") && duration_ok("5Y") && duration_ok("7D") && duration_ok("2W"));
    assert!(
        !duration_ok("M") && !duration_ok("12345Y") && !duration_ok("1y") && !duration_ok("1年")
    );
}
