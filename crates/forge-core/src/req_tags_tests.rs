#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! `@req` tags (M3 X4, plan 03 §4.3, plan 06 D-7).

use super::{scan_line, CriterionId, TagScan, TagTarget};

fn targets(line: &str) -> Vec<String> {
    scan_line(line)
        .into_iter()
        .map(|t| match t {
            TagScan::Target(t) => t.to_string(),
            TagScan::Malformed(raw) => format!("malformed:{raw}"),
        })
        .collect()
}

#[test]
fn a_tag_names_a_requirement_or_an_acceptance_criterion() {
    assert_eq!(targets("// @req ZEVA-CFG-012"), vec!["ZEVA-CFG-012"]);
    assert_eq!(targets("# @req PBI-123/AC-2"), vec!["PBI-123/AC-2"]);
    assert_eq!(targets("@req\tA-1"), vec!["A-1"]);
    assert_eq!(
        targets("fn t() {} // @req A-1  and later @req B-2"),
        vec!["A-1", "B-2"]
    );
}

#[test]
fn several_ids_follow_one_tag_separated_by_commas() {
    assert_eq!(
        targets("/* @req A-1, A-2,PBI-7/AC-1 */"),
        vec!["A-1", "A-2", "PBI-7/AC-1"]
    );
    // A second word without a comma is prose, not an id.
    assert_eq!(targets("@req A-1 covers the reload"), vec!["A-1"]);
}

#[test]
fn words_that_contain_at_req_are_no_tags() {
    for line in [
        "@required A-1",
        "mail me@req A-1",
        "the `@req` index",
        "x.@req A-1",
        "@reqA-1",
        "@REQ A-1",
    ] {
        assert!(targets(line).is_empty(), "{line}");
    }
}

#[test]
fn a_malformed_id_is_reported_as_malformed() {
    assert_eq!(targets("// @req zeva-cfg-1"), vec!["malformed:zeva-cfg-1"]);
    assert_eq!(targets("// @req"), vec!["malformed:"]);
    assert_eq!(targets("// @req   "), vec!["malformed:"]);
    assert_eq!(
        targets("// @req PBI-012/AC-1"),
        vec!["malformed:PBI-012/AC-1"]
    );
    assert_eq!(targets("// @req A-1, x"), vec!["A-1", "malformed:x"]);
}

#[test]
fn trailing_punctuation_is_not_part_of_the_id() {
    assert_eq!(targets("see (@req A-1)."), vec!["A-1"]);
    assert_eq!(targets("`@req A-1`"), vec!["A-1"]);
    assert_eq!(targets("\"@req A-1\";"), vec!["A-1"]);
}

#[test]
fn a_long_malformed_token_is_cut_when_reported() {
    let long = "x".repeat(500);
    let found = scan_line(&format!("@req {long}"));
    let [TagScan::Malformed(raw)] = found.as_slice() else {
        panic!("{found:?}");
    };
    assert!(
        raw.len() <= super::MAX_REPORTED_TOKEN_BYTES,
        "{}",
        raw.len()
    );
}

#[test]
fn acceptance_criterion_ids_are_canonical() {
    for ok in ["PBI-1/AC-1", "PBI-1234567/AC-9999"] {
        let c = CriterionId::parse(ok).expect(ok);
        assert_eq!(c.to_string(), ok);
    }
    for bad in [
        "PBI-1/AC-0",
        "PBI-1/AC-01",
        "PBI-01/AC-1",
        "PBI-1/ac-1",
        "pbi-1/AC-1",
        "PBI-1/AC-10000",
        "PBI-1",
        "PBI-1/AC-",
    ] {
        assert!(CriterionId::parse(bad).is_none(), "{bad}");
    }
    let c = CriterionId::parse("PBI-43/AC-2").expect("id");
    assert_eq!(c.work_item().as_str(), "PBI-43");
    assert!(matches!(
        TagTarget::parse("PBI-43/AC-2"),
        Some(TagTarget::Criterion(_))
    ));
    assert!(matches!(
        TagTarget::parse("ZEVA-CFG-1"),
        Some(TagTarget::Requirement(_))
    ));
    // A work item alone is no tag target (a test covers one of its
    // criteria), and `PBI-` is kanflow's: never a requirement family.
    assert!(TagTarget::parse("PBI-43").is_none());
    assert_eq!(targets("@req PBI-43"), vec!["malformed:PBI-43"]);
}
