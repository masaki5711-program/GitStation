#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)

use forge_core::checks::{CheckConclusion, CheckStatus};
use forge_errors::{ErrorCode, ForgeError};

use super::*;

fn reason(err: &ForgeError) -> String {
    assert_eq!(err.code(), ErrorCode::CheckPayloadInvalid, "{err}");
    err.context()
        .iter()
        .find(|(k, _)| *k == "reason")
        .map(|(_, v)| v.clone())
        .unwrap_or_default()
}

fn note(path: &str) -> AnnotationInput {
    AnnotationInput {
        path: path.to_owned(),
        start_line: 3,
        end_line: Some(4),
        level: "warning".to_owned(),
        message: "synthetic".to_owned(),
        title: None,
    }
}

fn input(status: &str, conclusion: Option<&str>) -> ReportInput {
    ReportInput {
        status: status.to_owned(),
        conclusion: conclusion.map(str::to_owned),
        summary: Some("ok".to_owned()),
        details_url: Some("https://ci.example.test/runs/1".to_owned()),
        annotations: vec![note("src/lib.rs")],
    }
}

fn name() -> forge_core::CheckName {
    parse_name("ci/gates").expect("name")
}

fn refused(input: &ReportInput) -> String {
    reason(&validate(name(), input).expect_err("refused"))
}

#[test]
fn a_well_formed_report_is_accepted() {
    let run = validate(name(), &input("completed", Some("success"))).expect("valid");
    assert_eq!(run.status, CheckStatus::Completed);
    assert_eq!(run.conclusion, Some(CheckConclusion::Success));
    assert_eq!(run.annotations.len(), 1);
    assert_eq!(run.annotations[0].end_line, 4);
    let queued = validate(name(), &input("queued", None)).expect("valid");
    assert_eq!(queued.conclusion, None);
    // An annotation without an end line covers its start line.
    let mut one = input("in_progress", None);
    one.annotations[0].end_line = None;
    assert_eq!(
        validate(name(), &one).expect("valid").annotations[0].end_line,
        3
    );
}

#[test]
fn an_annotation_over_the_limit_is_check_payload_invalid() {
    let mut many = input("completed", Some("failure"));
    many.annotations = (0..50).map(|i| note(&format!("src/{i}.rs"))).collect();
    validate(name(), &many).expect("50 annotations are the limit");
    many.annotations.push(note("src/one-more.rs"));
    let err = validate(name(), &many).expect_err("51");
    assert_eq!(reason(&err), "too_many_annotations");
    assert!(err
        .context()
        .iter()
        .any(|(k, v)| *k == "limit" && v == "50"));
}

#[test]
fn names_follow_check_name_parse() {
    for bad in ["gates", "ci/", "ci/A", "other/x", "ci/a//b", "ci/a/", ""] {
        let err = parse_name(bad).expect_err(bad);
        assert_eq!(reason(&err), "name_invalid", "{bad}");
    }
    for ok in ["ci/gates", "kanflow/work-item", "forge/risk", "ext/lint.v2"] {
        parse_name(ok).expect(ok);
    }
}

#[test]
fn status_and_conclusion_must_agree() {
    assert_eq!(refused(&input("done", None)), "status_invalid");
    assert_eq!(
        refused(&input("completed", Some("passed"))),
        "conclusion_invalid"
    );
    assert_eq!(
        refused(&input("completed", None)),
        "completed_without_conclusion"
    );
    assert_eq!(
        refused(&input("queued", Some("success"))),
        "conclusion_without_completed"
    );
    assert_eq!(
        refused(&input("in_progress", Some("neutral"))),
        "conclusion_without_completed"
    );
}

#[test]
fn the_summary_is_bounded() {
    let mut long = input("queued", None);
    long.summary = Some("x".repeat(64 * 1024));
    validate(name(), &long).expect("64 KiB is the limit");
    long.summary = Some("x".repeat(64 * 1024 + 1));
    assert_eq!(refused(&long), "summary_too_long");
    long.summary = Some("a\0b".to_owned());
    assert_eq!(refused(&long), "summary_invalid");
}

#[test]
fn the_details_url_is_http_or_https_and_never_carries_credentials() {
    for bad in [
        "ftp://ci.example.test/1",
        "javascript:alert(1)",
        "file:///etc/passwd",
        "https://user:secret@ci.example.test/",
        "https://ci.example.test/a b",
        "not a url",
        "https://",
    ] {
        let mut r = input("queued", None);
        r.details_url = Some(bad.to_owned());
        assert_eq!(refused(&r), "details_url_invalid", "{bad}");
    }
    let mut long = input("queued", None);
    long.details_url = Some(format!("https://ci.example.test/{}", "a".repeat(2048)));
    assert_eq!(refused(&long), "details_url_invalid");
    // The stored form is normalized (the table's CHECK expects a
    // lowercase scheme).
    let mut upper = input("queued", None);
    upper.details_url = Some("HTTPS://CI.Example.test/run".to_owned());
    let run = validate(name(), &upper).expect("valid");
    assert_eq!(
        run.details_url.as_deref(),
        Some("https://ci.example.test/run")
    );
}

#[test]
fn annotation_paths_are_relative_without_dot_dot() {
    for bad in [
        "",
        "/etc/passwd",
        "../a.rs",
        "src/../../a.rs",
        "src/..",
        "src\\a.rs",
        "src/a\n.rs",
    ] {
        let mut r = input("queued", None);
        r.annotations = vec![note(bad)];
        assert_eq!(refused(&r), "annotation_path_invalid", "{bad:?}");
    }
    let mut long = input("queued", None);
    long.annotations = vec![note(&"a".repeat(4097))];
    assert_eq!(refused(&long), "annotation_path_invalid");
    for ok in ["a.rs", "src/..x/..y.rs", "a..b/c.rs", "docs/日本語.md"] {
        let mut r = input("queued", None);
        r.annotations = vec![note(ok)];
        validate(name(), &r).expect(ok);
    }
}

#[test]
fn annotation_lines_levels_messages_and_titles_are_checked() {
    let with = |f: &dyn Fn(&mut AnnotationInput)| {
        let mut r = input("queued", None);
        f(&mut r.annotations[0]);
        refused(&r)
    };
    assert_eq!(with(&|a| a.start_line = 0), "annotation_lines_invalid");
    assert_eq!(with(&|a| a.end_line = Some(2)), "annotation_lines_invalid");
    assert_eq!(
        with(&|a| a.start_line = i64::from(i32::MAX) + 1),
        "annotation_lines_invalid"
    );
    assert_eq!(
        with(&|a| a.level = "error".to_owned()),
        "annotation_level_invalid"
    );
    assert_eq!(
        with(&|a| a.message = String::new()),
        "annotation_message_invalid"
    );
    assert_eq!(
        with(&|a| a.message = "m".repeat(4097)),
        "annotation_message_invalid"
    );
    assert_eq!(
        with(&|a| a.title = Some("t".repeat(256))),
        "annotation_title_invalid"
    );
    // The failing annotation is named by its position.
    let mut r = input("queued", None);
    r.annotations = vec![note("a.rs"), note("/b.rs")];
    let err = validate(name(), &r).expect_err("second");
    assert!(err.context().iter().any(|(k, v)| *k == "index" && v == "1"));
}
