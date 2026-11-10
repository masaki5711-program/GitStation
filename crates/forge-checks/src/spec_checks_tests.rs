#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! The runs of `forge/spec-lint` and `forge/spec-trace` (M3 X4): pure, on
//! hand-built indexes.

use forge_core::checks::{CheckAnnotationLevel, CheckConclusion, MAX_CHECK_ANNOTATIONS};
use forge_core::repo_config::RepoConfigProblem;
use forge_core::spec_index::{trace_merge_request, ExternalScope, SpecIndex};
use forge_core::{ObjectId, WorkItemRef};

use super::{assess_lint, assess_trace, Compared};
use crate::spec_index::Indexed;
use crate::spec_refs::RepoConfigRead;

fn spec(blocks: &str) -> String {
    format!("---\nspec: s\ntitle: t\nstatus: approved\n---\n{blocks}")
}

fn indexed(docs: &[(&str, String)], tags: &[(&str, usize, &str)]) -> Indexed {
    let mut index = SpecIndex::default();
    for (path, text) in docs {
        index.add_document(path, text.as_bytes());
    }
    for (path, line, text) in tags {
        index.add_tag_line(path, *line, text);
    }
    Indexed {
        config: RepoConfigRead::Absent,
        dirs: vec!["spec/".to_owned()],
        index,
        external: ExternalScope::default(),
    }
}

fn base() -> Compared {
    Compared::MergeBase(ObjectId::parse(&"b".repeat(40)).expect("sha"))
}

#[test]
fn a_commit_without_specification_passes_spec_lint() {
    let run = assess_lint(&indexed(&[], &[])).expect("run");
    assert_eq!(run.name.as_str(), "forge/spec-lint");
    assert_eq!(run.conclusion, Some(CheckConclusion::Success));
    assert!(
        run.summary.contains("No specification document"),
        "{}",
        run.summary
    );
    assert!(run.annotations.is_empty());
}

#[test]
fn a_spec_lint_problem_fails_with_spec_lint_problem_and_an_annotation() {
    let head = indexed(
        &[
            ("spec/a.md", spec("::: req A-1\nx\n:::\n")),
            ("spec/b.md", spec("::: req A-1\ny\n:::\n")),
        ],
        &[("tests/t.rs", 4, "// @req A-9")],
    );
    let run = assess_lint(&head).expect("run");
    assert_eq!(run.conclusion, Some(CheckConclusion::Failure));
    assert!(run.summary.contains("FG-REPO-0028"), "{}", run.summary);
    assert!(
        run.summary.contains("spec/b.md:6 duplicate_id"),
        "{}",
        run.summary
    );
    assert!(
        run.summary.contains("tests/t.rs:4 broken_reference"),
        "{}",
        run.summary
    );
    let places: Vec<(&str, u32, CheckAnnotationLevel)> = run
        .annotations
        .iter()
        .map(|a| (a.path.as_str(), a.start_line, a.level))
        .collect();
    assert_eq!(
        places,
        vec![
            // Both documents are named `s`.
            ("spec/b.md", 2, CheckAnnotationLevel::Failure),
            ("spec/b.md", 6, CheckAnnotationLevel::Failure),
            ("tests/t.rs", 4, CheckAnnotationLevel::Failure),
        ]
    );
}

#[test]
fn an_incomplete_index_is_never_a_silent_pass() {
    let mut head = indexed(&[("spec/a.md", spec("::: req A-1\nx\n:::\n"))], &[]);
    head.index.left_out("spec_files");
    let run = assess_lint(&head).expect("run");
    assert_eq!(run.conclusion, Some(CheckConclusion::Neutral));
    assert!(run.summary.contains("FG-REPO-0031"), "{}", run.summary);
    assert!(run.summary.contains("spec_files"), "{}", run.summary);
}

#[test]
fn a_refused_repo_yml_is_named_and_the_default_directory_read() {
    let mut head = indexed(&[], &[]);
    head.config = RepoConfigRead::Invalid(RepoConfigProblem {
        line: 3,
        reason: "unknown_key",
    });
    let run = assess_lint(&head).expect("run");
    assert!(run.summary.contains("FG-REPO-0024"), "{}", run.summary);
    assert!(
        run.summary.contains("the default spec/ was read"),
        "{}",
        run.summary
    );
}

#[test]
fn lint_annotations_stop_at_the_cap() {
    let mut blocks = String::new();
    for _ in 0..(MAX_CHECK_ANNOTATIONS + 5) {
        blocks.push_str("::: req a-1\nx\n:::\n");
    }
    let run = assess_lint(&indexed(&[("spec/a.md", spec(&blocks))], &[])).expect("run");
    assert_eq!(run.annotations.len(), MAX_CHECK_ANNOTATIONS);
}

#[test]
fn an_untested_touched_requirement_is_a_neutral_trace_never_a_failure() {
    let before = indexed(&[("spec/a.md", spec("::: req A-1\nold\n:::\n"))], &[]);
    let head = indexed(
        &[(
            "spec/a.md",
            spec("::: req A-1\nnew\n:::\n::: req A-2\nadded\n:::\n"),
        )],
        &[
            ("tests/t.rs", 2, "// @req A-2"),
            ("tests/t.rs", 5, "// @req NOPE-1"),
        ],
    );
    let linked = [WorkItemRef::parse("PBI-7").expect("ref")];
    let trace = trace_merge_request(&before.index, &head.index, &linked, &head.external);
    let run = assess_trace(&trace, &base(), false).expect("run");
    assert_eq!(run.name.as_str(), "forge/spec-trace");
    assert_eq!(run.conclusion, Some(CheckConclusion::Neutral));
    for want in [
        "untested A-1 (spec_changed) spec/a.md:6 [FG-REPO-0029]",
        "tested A-2 (spec_changed, tag_added; 1 test(s))",
        "unknown NOPE-1 (tag_added) tests/t.rs:5",
        "FG-REPO-0030",
        "untested PBI-7 (work_item_linked) [FG-REPO-0029]",
        "Compared with the merge base",
    ] {
        assert!(run.summary.contains(want), "{want}\n{}", run.summary);
    }
    let places: Vec<(&str, u32, CheckAnnotationLevel)> = run
        .annotations
        .iter()
        .map(|a| (a.path.as_str(), a.start_line, a.level))
        .collect();
    assert_eq!(
        places,
        vec![
            ("spec/a.md", 6, CheckAnnotationLevel::Warning),
            ("tests/t.rs", 5, CheckAnnotationLevel::Notice),
        ]
    );
}

#[test]
fn a_fully_tested_merge_request_is_a_successful_trace() {
    let head = indexed(
        &[("spec/a.md", spec("::: req A-1\nx\n:::\n"))],
        &[("tests/t.rs", 2, "// @req A-1, PBI-7/AC-1")],
    );
    let linked = [WorkItemRef::parse("PBI-7").expect("ref")];
    let trace = trace_merge_request(&SpecIndex::default(), &head.index, &linked, &head.external);
    let run = assess_trace(&trace, &Compared::Nothing, false).expect("run");
    assert_eq!(
        run.conclusion,
        Some(CheckConclusion::Success),
        "{}",
        run.summary
    );
    assert!(run.summary.contains("No merge base"), "{}", run.summary);
    assert!(run.annotations.is_empty());
    // More links than one read keeps: never a clear pass.
    let cut = assess_trace(&trace, &Compared::Nothing, true).expect("run");
    assert_eq!(cut.conclusion, Some(CheckConclusion::Neutral));
}

#[test]
fn nothing_touched_is_a_successful_trace() {
    let head = indexed(&[], &[]);
    let trace = trace_merge_request(&head.index, &head.index, &[], &head.external);
    let run = assess_trace(&trace, &base(), false).expect("run");
    assert_eq!(run.conclusion, Some(CheckConclusion::Success));
    assert!(
        run.summary.contains("touches no requirement"),
        "{}",
        run.summary
    );
}

#[test]
fn an_annotation_on_a_path_the_table_refuses_stays_in_the_summary_only() {
    let head = indexed(
        &[("spec/a.md", spec("::: req A-1\nx\n:::\n"))],
        &[
            ("C:evil.rs", 2, "// @req A-9"),
            ("t\\x.rs", 3, "// @req A-8"),
        ],
    );
    let run = assess_lint(&head).expect("run");
    assert_eq!(run.conclusion, Some(CheckConclusion::Failure));
    assert!(
        run.summary.contains("C:evil.rs:2 broken_reference"),
        "{}",
        run.summary
    );
    assert!(run.annotations.is_empty(), "{:?}", run.annotations);
}

/// Review X4 (LOW): an incomplete index may have left out the definition a
/// tag names, so broken references alone are no failure then.
#[test]
fn broken_references_alone_in_an_incomplete_index_are_neutral() {
    let mut head = indexed(
        &[("spec/a.md", spec("::: req A-1\nx\n:::\n"))],
        &[("t.rs", 1, "// @req A-9")],
    );
    head.index.left_out("spec_files");
    let run = assess_lint(&head).expect("run");
    assert_eq!(
        run.conclusion,
        Some(CheckConclusion::Neutral),
        "{}",
        run.summary
    );
    assert!(run.summary.contains("broken_reference"), "{}", run.summary);
    // A document problem still fails it.
    let mut broken = indexed(&[("spec/a.md", spec("::: req a-1\nx\n:::\n"))], &[]);
    broken.index.left_out("spec_files");
    let run = assess_lint(&broken).expect("run");
    assert_eq!(run.conclusion, Some(CheckConclusion::Failure));
}

/// Review X4 (MEDIUM c): the last attempt always answers inside the job's
/// timeout — the evaluation, the three runs' writes and the three `error`
/// writes (plan 13 stage 2 added `forge/doc-lint`).
#[test]
fn the_last_attempt_answers_inside_the_job_timeout() {
    let worst = super::EVALUATION_BUDGET + super::WRITE_TIMEOUT * 6;
    // Review MR3: at least 10 s of margin.
    assert!(
        worst + std::time::Duration::from_secs(10)
            <= forge_db::checks::spec_index::SPEC_INDEX_TIMEOUT,
        "{worst:?}"
    );
}
