#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)] // tests may fail loudly

//! The lint against the synthetic examples of plan 13
//! (`docs/plan/13-examples/`, all synthetic data): their README lists the
//! errors left in on purpose, and nothing else may be reported.

use super::{lint, FileReport, Source};
use crate::document::Problem;

const QP: &str = include_str!("../../../docs/plan/13-examples/qms/procedures/QP-07-001.md");
const RA: &str = include_str!("../../../docs/plan/13-examples/safety/RA-2026-014.md");
const URS: &str = include_str!("../../../docs/plan/13-examples/equipment/URS-L3-001.md");
const T_QP: &str = include_str!("../../../docs/plan/13-examples/.forge/doctypes/qms.procedure.yml");
const T_RA: &str =
    include_str!("../../../docs/plan/13-examples/.forge/doctypes/safety.risk-assessment.yml");
const T_URS: &str =
    include_str!("../../../docs/plan/13-examples/.forge/doctypes/equipment.urs.yml");

const P_QP: &str = "qms/procedures/QP-07-001.md";
const P_RA: &str = "safety/RA-2026-014.md";
const P_URS: &str = "equipment/URS-L3-001.md";

fn types() -> Vec<Source<'static>> {
    vec![
        Source {
            path: ".forge/doctypes/qms.procedure.yml",
            bytes: T_QP.as_bytes(),
        },
        Source {
            path: ".forge/doctypes/safety.risk-assessment.yml",
            bytes: T_RA.as_bytes(),
        },
        Source {
            path: ".forge/doctypes/equipment.urs.yml",
            bytes: T_URS.as_bytes(),
        },
    ]
}

/// Lint `docs` (path, text) with the three example types.
fn run(docs: &[(&str, &str)]) -> Vec<FileReport> {
    let sources: Vec<Source<'_>> = docs
        .iter()
        .map(|(path, text)| Source {
            path,
            bytes: text.as_bytes(),
        })
        .collect();
    lint(&types(), &sources)
}

/// The 1-based line of the `nth` (0-based) line containing `needle`.
fn line_of(text: &str, needle: &str, nth: usize) -> usize {
    text.lines()
        .enumerate()
        .filter(|(_, l)| l.contains(needle))
        .nth(nth)
        .map(|(i, _)| i + 1)
        .unwrap_or_else(|| panic!("{needle:?} #{nth} not in the text"))
}

fn p(line: usize, rule: &'static str, subject: &str) -> Problem {
    Problem::on(line, rule, subject)
}

fn examples() -> Vec<(&'static str, &'static str)> {
    vec![(P_QP, QP), (P_RA, RA), (P_URS, URS)]
}

#[test]
fn the_examples_report_exactly_the_errors_their_readme_expects() {
    let reports = run(&examples());
    let mut qp = vec![
        // `related: [QM-001]` and `[[QM-001]]` twice: not in this repository.
        p(line_of(QP, "related: [QM-001]", 0), "ref_broken", "QM-001"),
        p(line_of(QP, "[[QM-001]]", 0), "ref_broken", "QM-001"),
        p(line_of(QP, "[[QM-001]]", 1), "ref_broken", "QM-001"),
        // `[[QP-07-002]]` twice.
        p(line_of(QP, "[[QP-07-002]]", 0), "ref_broken", "QP-07-002"),
        p(line_of(QP, "[[QP-07-002]]", 1), "ref_broken", "QP-07-002"),
        // The form of R-01.
        p(line_of(QP, "form=QF-07-001", 0), "ref_broken", "QF-07-001"),
    ];
    qp.sort();
    let urs = vec![Problem::new(
        line_of(URS, "::: decision D-02", 0),
        "decisions_resolved",
    )];
    assert_eq!(
        reports,
        vec![
            FileReport {
                path: P_URS.to_owned(),
                problems: urs,
                problems_dropped: 0,
            },
            FileReport {
                path: P_QP.to_owned(),
                problems: qp,
                problems_dropped: 0,
            },
        ]
    );
}

#[test]
fn the_passing_version_has_no_problems() {
    // The referenced documents exist and D-02 is decided (README: "合格の版"),
    // with two small types for the manual and the form.
    let manual_t = "name: qms.manual\nid_pattern: '^QM-\\d{3}$'\n";
    let form_t = "name: qms.form\nid_pattern: '^QF-\\d{2}-\\d{3}$'\n";
    let doc = |doctype: &str, id: &str, title: &str| {
        format!(
            "---\ndoctype: {doctype}\nid: {id}\ntitle: {title}\nowner: qa-team\n---\n\n本文。\n"
        )
    };
    let qm = doc("qms.manual", "QM-001", "品質マニュアル");
    let qf = doc("qms.form", "QF-07-001", "配布先の一覧");
    let qp2 = QP
        .replace("id: QP-07-001", "id: QP-07-002")
        .replace("[[QP-07-002]]", "[[QP-07-001]]")
        .replace("[[QP-07-001#T-01]]", "[[QP-07-002#T-01]]");
    let urs = URS.replace("D-02 state=open", "D-02 state=decided");
    let mut all = types();
    all.push(Source {
        path: ".forge/doctypes/qms.manual.yml",
        bytes: manual_t.as_bytes(),
    });
    all.push(Source {
        path: ".forge/doctypes/qms.form.yml",
        bytes: form_t.as_bytes(),
    });
    let docs = [
        (P_QP, QP),
        (P_RA, RA),
        (P_URS, urs.as_str()),
        ("qms/QM-001.md", qm.as_str()),
        ("qms/procedures/QP-07-002.md", qp2.as_str()),
        ("qms/forms/QF-07-001.md", qf.as_str()),
    ];
    let sources: Vec<Source<'_>> = docs
        .iter()
        .map(|(path, text)| Source {
            path,
            bytes: text.as_bytes(),
        })
        .collect();
    assert_eq!(lint(&all, &sources), Vec::<FileReport>::new());
}

/// One document changed by `edit`, linted with the other two examples.
fn edited(path: &str, edit: impl Fn(&str) -> String) -> Vec<Problem> {
    let docs: Vec<(&str, String)> = examples()
        .into_iter()
        .map(|(p, t)| (p, if p == path { edit(t) } else { t.to_owned() }))
        .collect();
    let refs: Vec<(&str, &str)> = docs.iter().map(|(p, t)| (*p, t.as_str())).collect();
    run(&refs)
        .into_iter()
        .find(|r| r.path == path)
        .map(|r| r.problems)
        .unwrap_or_default()
}

fn rules(problems: &[Problem]) -> Vec<&'static str> {
    problems.iter().map(|p| p.rule).collect()
}

#[test]
fn each_risk_assessment_rule_fails_on_its_counter_example() {
    // A hazard without a control.
    let got = edited(P_RA, |t| t.replace("for=H-02", "for=H-01"));
    assert_eq!(rules(&got), vec!["every_hazard_has_control"]);
    // Residual risk above the initial one.
    let got = edited(P_RA, |t| {
        t.replace(
            "res_severity=2 res_likelihood=1",
            "res_severity=3 res_likelihood=1",
        )
    });
    assert_eq!(rules(&got), vec!["residual_not_above_initial"]);
    // A controlled hazard without its residual risk.
    let got = edited(P_RA, |t| t.replace(" res_severity=2 res_likelihood=1", ""));
    assert_eq!(rules(&got), vec!["residual_set_when_controlled"]);
    // A control that names a block that is no hazard.
    let got = edited(P_RA, |t| t.replace("for=H-02", "for=C-01a"));
    assert_eq!(
        rules(&got),
        vec!["every_hazard_has_control", "ref_kind_mismatch"]
    );
    // An int out of its range, and a value outside the enum.
    let got = edited(P_RA, |t| {
        t.replace("severity=2 likelihood=3", "severity=5 likelihood=3")
    });
    assert_eq!(rules(&got), vec!["attr_invalid"]);
    let got = edited(P_RA, |t| t.replace("type=ppe", "type=gloves"));
    assert_eq!(rules(&got), vec!["attr_invalid"]);
}

#[test]
fn each_urs_rule_fails_on_its_counter_example() {
    let fixed = |t: &str| t.replace("D-02 state=open", "D-02 state=decided");
    // A must requirement without a check.
    let got = edited(P_URS, |t| {
        fixed(t).replace("covers=REQ-L3-014", "covers=REQ-L3-011")
    });
    assert_eq!(rules(&got), vec!["every_must_req_has_check"]);
    // `na` without a reason.
    let got = edited(P_URS, |t| {
        fixed(t).replace(
            "海外の規格への適合: 対象外（国内の工場だけで使うため）。\n",
            "",
        )
    });
    assert_eq!(
        rules(&got),
        vec!["block_body_empty", "decision_na_needs_reason"]
    );
    // A deprecated must requirement needs no check.
    let got = edited(P_URS, |t| {
        fixed(t).replace("REQ-L3-013 priority=could", "REQ-L3-013 priority=must")
    });
    assert_eq!(rules(&got), Vec::<&str>::new());
    // A requirement id that is no requirement id, and a missing attribute.
    let got = edited(P_URS, |t| {
        fixed(t).replace("REQ-L3-010 priority=must", "req-1 priority=must")
    });
    assert!(rules(&got).contains(&"block_id_invalid"), "{got:?}");
    let got = edited(P_URS, |t| {
        fixed(t).replace(
            "REQ-L3-011 priority=should verify=demonstration",
            "REQ-L3-011 priority=should",
        )
    });
    assert_eq!(rules(&got), vec!["attr_missing"]);
}

#[test]
fn the_front_matter_is_checked_against_the_type() {
    let fixed = |t: &str| t.replace("D-02 state=open", "D-02 state=decided");
    // A key neither the base nor the type knows (a typo).
    let got = edited(P_URS, |t| {
        fixed(t).replace("line: 第 3 ライン", "lines: 第 3 ライン")
    });
    assert_eq!(rules(&got), vec!["field_unknown"]);
    // A required type field missing.
    let got = edited(P_RA, |t| {
        t.replace("regulations: [労働安全衛生規則 第131条, JIS B 9700]\n", "")
    });
    assert_eq!(rules(&got), vec!["field_missing"]);
    // The id's shape and the file's name.
    let got = edited(P_RA, |t| t.replace("id: RA-2026-014", "id: RA-26-014"));
    assert!(rules(&got).contains(&"id_invalid"), "{got:?}");
    assert!(rules(&got).contains(&"id_not_filename"), "{got:?}");
    // The base keys are required.
    let got = edited(P_RA, |t| t.replace("owner: safety-team\n", ""));
    assert_eq!(rules(&got), vec!["field_missing"]);
    // A ref field of the wrong type.
    let got = edited(P_RA, |t| {
        t.replace("equipment: URS-L3-001", "equipment: RA-2026-014")
    });
    assert_eq!(rules(&got), vec!["ref_doctype_mismatch"]);
    // A duration.
    let got = edited(P_RA, |t| {
        t.replace("review_cycle: 12M", "review_cycle: 1 year")
    });
    assert_eq!(rules(&got), vec!["field_invalid"]);
}

#[test]
fn classification_and_the_older_mark_cannot_both_be_written() {
    let got = edited(P_RA, |t| {
        t.replace(
            "classification: internal",
            "classification: internal\nconfidential: true",
        )
    });
    assert_eq!(rules(&got), vec!["classification_conflict"]);
    let got = edited(P_RA, |t| {
        t.replace("classification: internal", "classification: secret")
    });
    assert_eq!(rules(&got), vec!["classification_invalid"]);
}

#[test]
fn sections_must_all_be_there_in_order() {
    let got = edited(P_RA, |t| t.replace("## 残留リスク", "## 残留のリスク"));
    assert_eq!(rules(&got), vec!["section_missing"]);
    // Swap two headings (and keep the blocks under their own kinds' homes).
    let got = edited(P_RA, |t| {
        t.replace("## 対象と範囲", "## X")
            .replace("## 残留リスク", "## 対象と範囲")
            .replace("## X", "## 残留リスク")
    });
    assert!(rules(&got).contains(&"section_order"), "{got:?}");
}

#[test]
fn blocks_are_checked_against_their_kind() {
    // A kind the type does not define (with an id) …
    let got = edited(P_RA, |t| t.replace("::: hazard H-02", "::: risk H-02"));
    assert!(rules(&got).contains(&"block_kind_unknown"), "{got:?}");
    // … a defined kind without an id, a duplicate id, a kind out of its section.
    let got = edited(P_RA, |t| t.replace("::: hazard H-02 ", "::: hazard "));
    assert!(rules(&got).contains(&"block_id_missing"), "{got:?}");
    let got = edited(P_RA, |t| t.replace("::: hazard H-02", "::: hazard H-01"));
    assert!(rules(&got).contains(&"block_id_duplicate"), "{got:?}");
    let got = edited(P_RA, |t| {
        t.replace("## リスク低減措置\n", "")
            .replace("## 残留リスク", "## リスク低減措置\n\n## 残留リスク")
    });
    assert!(rules(&got).contains(&"block_outside_section"), "{got:?}");
    // An unknown attribute, and replaced_by without deprecation.
    let got = edited(P_RA, |t| t.replace("type=ppe", "type=ppe colour=red"));
    assert_eq!(rules(&got), vec!["attr_unknown"]);
    let got = edited(P_RA, |t| t.replace("type=ppe", "type=ppe replaced_by=C-03"));
    assert_eq!(rules(&got), vec!["replaced_by_not_deprecated"]);
    let got = edited(P_RA, |t| {
        t.replace("type=ppe", "type=ppe status=deprecated replaced_by=C-09")
    });
    assert!(rules(&got).contains(&"ref_broken"), "{got:?}");
}

#[test]
fn references_resolve_to_documents_and_their_blocks() {
    let got = edited(P_RA, |t| {
        t.replace("[[URS-L3-001#REQ-L3-012]]", "[[URS-L3-001#REQ-L3-099]]")
    });
    assert_eq!(rules(&got), vec!["ref_broken"]);
    // Another repository's reference is the caller's to resolve.
    let got = edited(P_RA, |t| {
        t.replace(
            "[[URS-L3-001#REQ-L3-012]]",
            "[[specs/zeva-engine:ZEVA-CFG-012]]",
        )
    });
    assert_eq!(rules(&got), Vec::<&str>::new());
    // Inside a code span or an HTML comment it is no reference.
    let got = edited(P_RA, |t| {
        t.replace(
            "[[URS-L3-001#REQ-L3-012]]",
            "`[[NOPE-1]]` <!-- [[NOPE-2]] -->",
        )
    });
    assert_eq!(rules(&got), Vec::<&str>::new());
}

#[test]
fn a_translation_is_named_after_its_original() {
    let en = RA.replace(
        "classification: internal",
        "classification: internal\nlang: en\ntranslation_of: RA-2026-014",
    );
    let reports = run(&[
        (P_QP, QP),
        (P_RA, RA),
        (P_URS, URS),
        ("safety/RA-2026-014.en.md", &en),
    ]);
    assert!(
        reports.iter().all(|r| r.path != "safety/RA-2026-014.en.md"),
        "{reports:?}"
    );
    // A wrong suffix, and translation_of on an original.
    let reports = run(&[
        (P_QP, QP),
        (P_RA, RA),
        (P_URS, URS),
        ("safety/RA-2026-014.fr.md", &en),
    ]);
    let r = reports
        .iter()
        .find(|r| r.path == "safety/RA-2026-014.fr.md")
        .expect("report");
    assert_eq!(rules(&r.problems), vec!["id_not_filename"]);
    let got = edited(P_RA, |_| en.clone());
    assert_eq!(rules(&got), vec!["translation_invalid"]);
}

#[test]
fn a_duplicate_document_id_is_reported_on_the_later_path() {
    let reports = run(&[
        (P_RA, RA),
        ("zzz/RA-2026-014.md", RA),
        (P_URS, URS),
        (P_QP, QP),
    ]);
    let r = reports
        .iter()
        .find(|r| r.path == "zzz/RA-2026-014.md")
        .expect("report");
    assert!(rules(&r.problems).contains(&"id_duplicate"), "{r:?}");
}

#[test]
fn unknown_or_broken_types_are_reported_once() {
    let got = edited(P_RA, |t| {
        t.replace("doctype: safety.risk-assessment", "doctype: safety.unknown")
    });
    assert_eq!(rules(&got), vec!["doctype_unknown"]);
    // A definition that cannot be read: its file says why, its documents
    // say the type is invalid (and nothing else about the type).
    let broken = T_RA.replace("every_hazard_has_control", "every_hazard_is_fine");
    let docs = [Source {
        path: P_RA,
        bytes: RA.as_bytes(),
    }];
    let reports = lint(
        &[Source {
            path: ".forge/doctypes/safety.risk-assessment.yml",
            bytes: broken.as_bytes(),
        }],
        &docs,
    );
    let rules_of = |path: &str| {
        reports
            .iter()
            .find(|r| r.path == path)
            .map(|r| rules(&r.problems))
            .unwrap_or_default()
    };
    assert_eq!(
        rules_of(".forge/doctypes/safety.risk-assessment.yml"),
        vec!["doctype_rule_unknown"]
    );
    // The broken references (URS is not in this run) are still reported.
    assert!(rules_of(P_RA).contains(&"doctype_invalid"));
}

#[test]
fn markdown_that_names_no_type_is_not_a_document() {
    let readme = "# README\n\n[[NOPE-1]] <div>html</div>\n";
    let reports = run(&[(P_QP, QP), (P_RA, RA), (P_URS, URS), ("README.md", readme)]);
    assert!(reports.iter().all(|r| r.path != "README.md"));
}

#[test]
fn problems_name_what_they_are_about() {
    let got = edited(P_RA, |t| {
        t.replace("owner: safety-team\n", "")
            .replace("## 残留リスク", "## 残留")
    });
    let subjects: Vec<(&str, Option<&str>)> =
        got.iter().map(|p| (p.rule, p.subject.as_deref())).collect();
    assert_eq!(
        subjects,
        vec![
            ("field_missing", Some("owner")),
            ("section_missing", Some("残留リスク"))
        ]
    );
}

#[test]
fn a_file_that_cannot_be_read_is_reported_never_skipped() {
    // Not UTF-8, broken YAML with a quoted key, a doctype that is no text.
    let mut not_utf8 = RA.as_bytes().to_vec();
    not_utf8.push(0xff);
    let broken = RA.replace(
        "doctype: safety.risk-assessment",
        "\"doctype\" : safety.risk-assessment\nx: [",
    );
    let listed = RA.replace("doctype: safety.risk-assessment", "doctype: [a, b]");
    for (bytes, rule) in [
        (not_utf8.as_slice(), "not_utf8"),
        (broken.as_bytes(), "front_matter_syntax"),
        (listed.as_bytes(), "field_invalid"),
    ] {
        let reports = lint(&types(), &[Source { path: P_RA, bytes }]);
        let r = reports.iter().find(|r| r.path == P_RA).expect("reported");
        assert!(rules(&r.problems).contains(&rule), "{rule}: {r:?}");
    }
    let big = format!("{}{}", RA, "a".repeat(crate::document::MAX_DOCUMENT_BYTES));
    let reports = lint(
        &types(),
        &[Source {
            path: P_RA,
            bytes: big.as_bytes(),
        }],
    );
    assert_eq!(
        reports.first().map(|r| rules(&r.problems)),
        Some(vec!["too_large"])
    );
}

#[test]
fn a_document_cannot_lower_its_types_default_classification() {
    let ty = T_RA.replace("classification: internal", "classification: confidential");
    let docs = [Source {
        path: P_RA,
        bytes: RA.as_bytes(),
    }];
    let reports = lint(
        &[Source {
            path: ".forge/doctypes/safety.risk-assessment.yml",
            bytes: ty.as_bytes(),
        }],
        &docs,
    );
    let r = reports.first().expect("report");
    assert!(
        rules(&r.problems).contains(&"classification_below_default"),
        "{r:?}"
    );
}

#[test]
fn a_translation_needs_its_original() {
    let en = RA.replace(
        "classification: internal",
        "classification: internal\nlang: en\ntranslation_of: RA-2026-014",
    );
    let reports = run(&[(P_URS, URS), ("safety/RA-2026-014.en.md", &en)]);
    let r = reports
        .iter()
        .find(|r| r.path == "safety/RA-2026-014.en.md")
        .expect("report");
    assert!(rules(&r.problems).contains(&"translation_invalid"), "{r:?}");
}
