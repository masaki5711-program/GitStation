use forge_doc::document::Problem;

use super::*;

fn report(path: &str, problems: &[(&'static str, Option<&str>)], dropped: usize) -> FileReport {
    FileReport {
        path: path.to_owned(),
        problems: problems
            .iter()
            .enumerate()
            .map(|(i, (rule, subject))| Problem {
                line: i + 1,
                rule,
                subject: subject.map(str::to_owned),
            })
            .collect(),
        problems_dropped: dropped,
    }
}

fn key(path: &str, rule: &str, reference: Option<&str>) -> TrainLintProblem {
    TrainLintProblem {
        path: path.to_owned(),
        rule: rule.to_owned(),
        reference: reference.map(str::to_owned),
    }
}

#[test]
fn a_problem_is_the_same_one_by_path_rule_and_referenced_id_never_by_line() {
    // The base cites QM-404 on line 1; the tip cites it on line 2 (a line
    // was added above): the same problem, nothing new.
    let base = problems_of(
        &[report(
            "qms/QM-001.md",
            &[("ref_broken", Some("QM-404"))],
            0,
        )],
        None,
    );
    let moved = FileReport {
        path: "qms/QM-001.md".to_owned(),
        problems: vec![Problem {
            line: 2,
            rule: "ref_broken",
            subject: Some("QM-404".to_owned()),
        }],
        problems_dropped: 0,
    };
    assert_eq!(new_against(&problems_of(&[moved], None), &base), vec![]);

    // Another id, another file or another rule is new.
    let tip = problems_of(
        &[
            report(
                "qms/QM-001.md",
                &[
                    ("ref_broken", Some("QM-404")),
                    ("ref_broken", Some("QM-002")),
                ],
                0,
            ),
            report("qms/QM-003.md", &[("ref_broken", Some("QM-404"))], 0),
            report("qms/QM-004.md", &[("no_title", None)], 0),
        ],
        None,
    );
    assert_eq!(
        new_against(&tip, &base),
        vec![
            key("qms/QM-001.md", "ref_broken", Some("QM-002")),
            key("qms/QM-003.md", "ref_broken", Some("QM-404")),
            key("qms/QM-004.md", "no_title", None),
        ]
    );
}

#[test]
fn a_second_occurrence_of_the_same_problem_is_new() {
    let once = problems_of(
        &[report("a.md", &[("ref_broken", Some("QM-404"))], 0)],
        None,
    );
    let twice = problems_of(
        &[report(
            "a.md",
            &[
                ("ref_broken", Some("QM-404")),
                ("ref_broken", Some("QM-404")),
            ],
            0,
        )],
        None,
    );
    assert_eq!(
        new_against(&twice, &once),
        vec![key("a.md", "ref_broken", Some("QM-404"))]
    );
    // Fewer than the base is not new (a train may repair).
    assert_eq!(new_against(&once, &twice), vec![]);
}

#[test]
fn problems_past_a_files_bound_are_compared_by_count() {
    let base = problems_of(&[report("a.md", &[], 3)], None);
    assert_eq!(
        new_against(&problems_of(&[report("a.md", &[], 3)], None), &base),
        vec![]
    );
    assert_eq!(
        new_against(&problems_of(&[report("a.md", &[], 4)], None), &base),
        vec![key("a.md", DROPPED_RULE, None)]
    );
}

#[test]
fn references_lowered_by_the_migration_do_not_count() {
    let reports = [report(
        "a.md",
        &[("ref_broken", Some("QM-404")), ("no_title", None)],
        0,
    )];
    let counted = problems_of(&reports, Some("2099-01-01"));
    assert_eq!(
        counted.into_keys().collect::<Vec<_>>(),
        vec![key("a.md", "no_title", None)]
    );
    assert_eq!(problems_of(&reports, None).len(), 2);
}

#[test]
fn only_documents_and_type_definitions_make_the_lint_apply() {
    for path in [
        "README.md",
        "docs/deep/a.md",
        ".forge/doctypes/qms.manual.yml",
        ".forge/doctypes/sub/x.txt",
    ] {
        assert!(is_lint_input(path), "{path}");
    }
    for path in [
        "src/main.rs",
        ".forge/repo.yml",
        ".forge/doctypes",
        "a.mdx",
        "md",
    ] {
        assert!(!is_lint_input(path), "{path}");
    }
}

#[test]
fn at_most_twenty_problems_are_listed() {
    let many: Vec<TrainLintProblem> = (0..30)
        .map(|i| key(&format!("d/{i:02}.md"), "ref_broken", Some("X")))
        .collect();
    assert_eq!(listed(many).len(), MAX_LISTED_PROBLEMS);
}

#[test]
fn the_train_reads_more_than_a_head_and_the_same_tree_listing() {
    let head = DocLimits::default();
    assert!(TRAIN_DOC_LIMITS.max_files > head.max_files);
    assert!(TRAIN_DOC_LIMITS.max_total_bytes > head.max_total_bytes);
    assert_eq!(TRAIN_DOC_LIMITS.max_tree_entries, head.max_tree_entries);
    assert_eq!(TRAIN_DOC_LIMITS.max_doctypes, head.max_doctypes);
}
