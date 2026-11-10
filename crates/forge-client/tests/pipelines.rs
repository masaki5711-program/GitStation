//! End-to-end tests of `gstn pipeline …` (M4 C2) against a scripted
//! loopback server: the route, query and body each command sends, the
//! text of a pipeline with its problems and jobs, an error answer shown as
//! its code and request id, and nothing sent for bad input. All data is
//! synthetic.

#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly

mod common;
use common::{Mock, Out, Resp, Sandbox, TOKEN};
use serde_json::{json, Value};

fn gstn(sb: &Sandbox, srv: &Mock, args: &[&str]) -> Out {
    sb.run_env(&srv.url, args, "", &[("GSTN_TOKEN", TOKEN)])
}

const PIPELINE: &str = r#"{"id":42,"project_id":7,"ref":"refs/heads/main","sha":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","source":"push","merge_request_id":null,"triggered_by":3,"status":"pending","error_code":null,"problems":[],"copy_mark":"open","definition_hash":"aaaa","created_at":"2026-11-10T00:00:00Z","updated_at":"2026-11-10T00:00:00Z","finished_at":null,"jobs":[{"id":1,"name":"build","stage":"build","wave":0,"needs":[],"image":"registry.internal/rust@sha256:aaaa","timeout_s":600,"tags":[],"secrets":[],"status":"pending","attempt":1,"error_code":null,"created_at":"2026-11-10T00:00:00Z","started_at":null,"finished_at":null}]}"#;

const INVALID: &str = r#"{"id":43,"project_id":7,"ref":"refs/heads/feature","sha":"cccccccccccccccccccccccccccccccccccccccc","source":"merge_request","merge_request_id":5,"triggered_by":null,"status":"invalid","error_code":"FG-CI-0001","problems":[{"line":7,"reason":"secret_on_unprotected_ref","subject":"deploy"}],"copy_mark":"open","definition_hash":"aaaa","created_at":"2026-11-10T00:00:00Z","updated_at":"2026-11-10T00:00:00Z","finished_at":null,"jobs":[]}"#;

#[test]
fn pipeline_commands_call_their_routes() {
    let page = format!(r#"{{"pipelines":[{PIPELINE}],"next_before":42}}"#);
    let cases: Vec<(Vec<&str>, &str, &str, u16, String)> = vec![
        (
            vec!["pipeline", "list", "--project", "7"],
            "GET",
            "/api/v1/projects/7/pipelines",
            200,
            page.clone(),
        ),
        (
            vec![
                "pipeline",
                "list",
                "--project",
                "7",
                "--ref",
                "main",
                "--before",
                "50",
                "--limit",
                "5",
            ],
            "GET",
            "/api/v1/projects/7/pipelines?ref=main&before=50&limit=5",
            200,
            page,
        ),
        (
            vec!["pipeline", "show", "--project", "7", "--pipeline", "42"],
            "GET",
            "/api/v1/projects/7/pipelines/42",
            200,
            PIPELINE.to_owned(),
        ),
        (
            vec!["pipeline", "cancel", "--project", "7", "--pipeline", "42"],
            "POST",
            "/api/v1/projects/7/pipelines/42/cancel",
            200,
            PIPELINE.to_owned(),
        ),
        (
            vec!["pipeline", "retry", "--project", "7", "--pipeline", "42"],
            "POST",
            "/api/v1/projects/7/pipelines/42/retry",
            200,
            PIPELINE.to_owned(),
        ),
    ];
    for (args, method, target, status, answer) in cases {
        let srv = Mock::start(move |_| Resp::json(status, &answer));
        let sb = Sandbox::new();
        let o = gstn(&sb, &srv, &args);
        assert_eq!(o.code, 0, "{args:?}: {}", o.stderr);
        let r = srv.seen().pop().expect("a request");
        assert_eq!(
            (r.method.as_str(), r.target.as_str()),
            (method, target),
            "{args:?}"
        );
    }
}

#[test]
fn pipeline_run_posts_the_branch() {
    let srv = Mock::start(|_| Resp::json(201, PIPELINE));
    let sb = Sandbox::new();
    let o = gstn(
        &sb,
        &srv,
        &["pipeline", "run", "--project", "7", "--ref", "main"],
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stdout.contains("Started"), "{}", o.stdout);
    let r = srv.seen().pop().expect("a request");
    assert_eq!(
        (r.method.as_str(), r.target.as_str()),
        ("POST", "/api/v1/projects/7/pipelines")
    );
    assert_eq!(
        serde_json::from_str::<Value>(&r.body).expect("json"),
        json!({"ref": "main"})
    );
}

#[test]
fn pipeline_show_prints_the_problems_and_the_jobs() {
    let srv = Mock::start(|_| Resp::json(200, INVALID));
    let sb = Sandbox::new();
    let o = gstn(
        &sb,
        &srv,
        &["pipeline", "show", "--project", "7", "--pipeline", "43"],
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    for want in [
        "#43 invalid merge_request feature cccccccccccc FG-CI-0001",
        "line 7 secret_on_unprotected_ref deploy",
    ] {
        assert!(o.stdout.contains(want), "{want}: {}", o.stdout);
    }
    let srv = Mock::start(|_| Resp::json(200, PIPELINE));
    let o = gstn(
        &sb,
        &srv,
        &["pipeline", "show", "--project", "7", "--pipeline", "42"],
    );
    assert!(
        o.stdout
            .contains("[0] build pending (stage build, attempt 1)"),
        "{}",
        o.stdout
    );
    let json = gstn(
        &sb,
        &srv,
        &[
            "pipeline",
            "show",
            "--project",
            "7",
            "--pipeline",
            "42",
            "--json",
        ],
    )
    .json();
    assert_eq!(json["jobs"][0]["name"], "build");
}

#[test]
fn a_conflict_shows_the_code_and_request_id() {
    let srv = Mock::start(|_| {
        Resp::json(
            409,
            r#"{"error_code":"FG-CI-0003","user_message":"This pipeline cannot be cancelled or retried in its current state. Reload the page.","request_id":"req-synthetic-9"}"#,
        )
    });
    let sb = Sandbox::new();
    let o = gstn(
        &sb,
        &srv,
        &["pipeline", "cancel", "--project", "7", "--pipeline", "42"],
    );
    assert_ne!(o.code, 0);
    assert!(o.stderr.contains("FG-CI-0003"), "{}", o.stderr);
    assert!(o.stderr.contains("req-synthetic-9"), "{}", o.stderr);
}

#[test]
fn bad_input_sends_nothing() {
    let srv = Mock::start(|_| Resp::json(200, PIPELINE));
    let sb = Sandbox::new();
    for args in [
        vec!["pipeline", "show", "--project", "0", "--pipeline", "1"],
        vec!["pipeline", "show", "--project", "7", "--pipeline", "x"],
        vec!["pipeline", "list", "--project", "7", "--limit", "101"],
        vec!["pipeline", "run", "--project", "7"],
    ] {
        let o = gstn(&sb, &srv, &args);
        assert_ne!(o.code, 0, "{args:?}");
    }
    assert!(srv.seen().is_empty());
}

const LOG_PAGE: &str = r#"{"pipeline_id":42,"job_id":9,"job":"test","job_status":"running","attempt":2,"current_attempt":2,"from":0,"to":3,"text":"running 4 tests\nFORGE_JOB_TOKEN=[MASKED]\n","next_from":4,"complete":false}"#;

const DIAGNOSIS: &str = r#"{"pipeline_id":42,"status":"failed","error_code":null,"failures":[{"job":"test","job_id":9,"stage":"test","attempt":2,"pending":false,"fingerprint":"fp_0123456789abcdef","error_codes":["FG-DB-0003"],"excerpt":"test db::reload ... FAILED\npool timed out FG-DB-0003","source":{"log_path":"/api/v1/projects/7/pipelines/42/jobs/9/log?attempt=2","lines":[812,813]},"occurrences":3,"first_seen_at":"2026-11-01T00:00:00Z","known_fixes":[{"merge_request_id":61,"iid":5,"title":"Raise the pool limit","fixed_at":"2026-11-02T00:00:00Z"}]},{"job":"lint","job_id":10,"stage":"test","attempt":1,"pending":true,"fingerprint":null,"error_codes":["FG-CI-0004"],"excerpt":"","source":{"log_path":"/api/v1/projects/7/pipelines/42/jobs/10/log?attempt=1","lines":null},"occurrences":0,"first_seen_at":null,"known_fixes":[]}]}"#;

/// M4 C5: the log and the diagnosis call their routes; the log's page and
/// the next chunk, the diagnosis' fingerprints, excerpts and known fixes
/// are shown.
#[test]
fn pipeline_log_and_diagnosis_call_their_routes() {
    let cases: Vec<(Vec<&str>, &str, &str)> = vec![
        (
            vec![
                "pipeline",
                "log",
                "--project",
                "7",
                "--pipeline",
                "42",
                "--job",
                "9",
            ],
            "/api/v1/projects/7/pipelines/42/jobs/9/log",
            LOG_PAGE,
        ),
        (
            vec![
                "pipeline",
                "log",
                "--project",
                "7",
                "--pipeline",
                "42",
                "--job",
                "9",
                "--attempt",
                "2",
                "--from",
                "4",
            ],
            "/api/v1/projects/7/pipelines/42/jobs/9/log?attempt=2&from=4",
            LOG_PAGE,
        ),
        (
            vec![
                "pipeline",
                "diagnosis",
                "--project",
                "7",
                "--pipeline",
                "42",
            ],
            "/api/v1/projects/7/pipelines/42/diagnosis",
            DIAGNOSIS,
        ),
    ];
    for (args, target, answer) in cases {
        let srv = Mock::start(move |_| Resp::json(200, answer));
        let sb = Sandbox::new();
        let o = gstn(&sb, &srv, &args);
        assert_eq!(o.code, 0, "{args:?}: {}", o.stderr);
        let r = srv.seen().pop().expect("a request");
        assert_eq!((r.method.as_str(), r.target.as_str()), ("GET", target));
    }
    let srv = Mock::start(|_| Resp::json(200, LOG_PAGE));
    let sb = Sandbox::new();
    let o = gstn(
        &sb,
        &srv,
        &[
            "pipeline",
            "log",
            "--project",
            "7",
            "--pipeline",
            "42",
            "--job",
            "9",
        ],
    );
    assert!(
        o.stdout.contains("FORGE_JOB_TOKEN=[MASKED]"),
        "{}",
        o.stdout
    );
    assert!(o.stdout.contains("more: --from 4"), "{}", o.stdout);
    let srv = Mock::start(|_| Resp::json(200, DIAGNOSIS));
    let o = gstn(
        &sb,
        &srv,
        &[
            "pipeline",
            "diagnosis",
            "--project",
            "7",
            "--pipeline",
            "42",
        ],
    );
    for want in [
        "#42 failed",
        "test (job 9, attempt 2): fp_0123456789abcdef seen 3 times FG-DB-0003",
        "lines 812-813 of the log",
        "  | pool timed out FG-DB-0003",
        "known fix: !5 Raise the pool limit",
        "lint (job 10, attempt 1): not fingerprinted yet FG-CI-0004",
    ] {
        assert!(o.stdout.contains(want), "{want}: {}", o.stdout);
    }
}

/// ADR-0029 decision 5: a refused CI output (an agent's token on a
/// confidential repository) is the server's FG-AUTH-0022 with its request
/// id; nothing else is printed.
#[test]
fn a_withheld_log_shows_the_code_and_request_id() {
    let srv = Mock::start(|_| {
        Resp::json(
            403,
            r#"{"error_code":"FG-AUTH-0022","user_message":"This repository holds a confidential document (or it could not be checked), so an AI agent's account cannot clone, fetch or download an archive of it, or read its CI logs and diagnosis. Use a person's account.","request_id":"req-synthetic-22"}"#,
        )
    });
    let sb = Sandbox::new();
    for args in [
        vec![
            "pipeline",
            "log",
            "--project",
            "7",
            "--pipeline",
            "42",
            "--job",
            "9",
        ],
        vec![
            "pipeline",
            "diagnosis",
            "--project",
            "7",
            "--pipeline",
            "42",
        ],
    ] {
        let o = gstn(&sb, &srv, &args);
        assert_ne!(o.code, 0, "{args:?}");
        assert!(o.stderr.contains("FG-AUTH-0022"), "{}", o.stderr);
        assert!(o.stderr.contains("req-synthetic-22"), "{}", o.stderr);
        assert!(o.stdout.is_empty(), "{}", o.stdout);
    }
}

#[test]
fn bad_log_input_sends_nothing() {
    let srv = Mock::start(|_| Resp::json(200, LOG_PAGE));
    let sb = Sandbox::new();
    for args in [
        vec!["pipeline", "log", "--project", "7", "--pipeline", "42"],
        vec![
            "pipeline",
            "log",
            "--project",
            "7",
            "--pipeline",
            "42",
            "--job",
            "0",
        ],
        vec![
            "pipeline",
            "log",
            "--project",
            "7",
            "--pipeline",
            "42",
            "--job",
            "9",
            "--attempt",
            "0",
        ],
        vec![
            "pipeline",
            "log",
            "--project",
            "7",
            "--pipeline",
            "42",
            "--job",
            "9",
            "--from",
            "4096",
        ],
        vec!["pipeline", "diagnosis", "--project", "7"],
    ] {
        let o = gstn(&sb, &srv, &args);
        assert_ne!(o.code, 0, "{args:?}");
    }
    assert!(srv.seen().is_empty());
}
