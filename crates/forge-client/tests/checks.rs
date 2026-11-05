//! End-to-end tests of `gstn checks …` and the check namespace grants
//! (M3 C1) against a scripted loopback server. All data is synthetic.

#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly

mod common;
use common::{Mock, Out, Resp, Sandbox, TOKEN};
use serde_json::{json, Value};

fn gstn(sb: &Sandbox, srv: &Mock, args: &[&str]) -> Out {
    sb.run_env(&srv.url, args, "", &[("GSTN_TOKEN", TOKEN)])
}

const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

const RUN: &str = r#"{"id":5,"name":"ci/gates","head_sha":"0123456789abcdef0123456789abcdef01234567","status":"completed","conclusion":"success","summary":"all green","details_url":"https://ci.example.test/1","annotations":[],"reported_by":{"id":42,"username":"ci-runner"},"revision":1,"created_at":"2026-11-03T00:00:00.000000Z","updated_at":"2026-11-03T00:00:00.000000Z","completed_at":"2026-11-03T00:00:00.000000Z"}"#;

#[test]
fn checks_report_posts_the_run_with_its_files_and_renders_it() {
    let srv = Mock::start(|_| Resp::json(201, RUN));
    let sb = Sandbox::new();
    let summary = sb.dir.path().join("summary.md");
    std::fs::write(&summary, "## all green\n").expect("summary");
    let notes = sb.dir.path().join("annotations.json");
    std::fs::write(
        &notes,
        r#"[{"path":"src/lib.rs","start_line":3,"level":"warning","message":"synthetic"}]"#,
    )
    .expect("annotations");
    let o = gstn(
        &sb,
        &srv,
        &[
            "checks",
            "report",
            "--project",
            "7",
            "--sha",
            &SHA.to_uppercase(),
            "--name",
            "ci/gates",
            "--status",
            "completed",
            "--conclusion",
            "success",
            "--summary-file",
            summary.to_str().expect("path"),
            "--details-url",
            "https://ci.example.test/1",
            "--annotations-file",
            notes.to_str().expect("path"),
        ],
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(
        o.stdout
            .contains("ci/gates completed success (by ci-runner)"),
        "{}",
        o.stdout
    );
    let r = srv.seen().pop().expect("a request");
    // The commit id goes into the path lower-cased.
    assert_eq!(
        (r.method.as_str(), r.target.as_str()),
        (
            "POST",
            format!("/api/v1/projects/7/commits/{SHA}/checks").as_str()
        )
    );
    assert_eq!(
        serde_json::from_str::<Value>(&r.body).expect("json"),
        json!({"name": "ci/gates", "status": "completed", "conclusion": "success",
               "summary": "## all green\n", "details_url": "https://ci.example.test/1",
               "annotations": [{"path": "src/lib.rs", "start_line": 3, "level": "warning",
                                "message": "synthetic"}]})
    );
}

#[test]
fn checks_report_refuses_bad_input_before_any_request() {
    let srv = Mock::start(|_| Resp::json(201, RUN));
    let sb = Sandbox::new();
    for args in [
        vec!["--sha", "abc", "--status", "queued"],
        vec!["--sha", SHA, "--status", "done"],
        vec![
            "--sha",
            SHA,
            "--status",
            "completed",
            "--conclusion",
            "passed",
        ],
    ] {
        let mut full = vec!["checks", "report", "--project", "7", "--name", "ci/gates"];
        full.extend(args.iter().copied());
        let o = gstn(&sb, &srv, &full);
        assert_eq!(o.code, 2, "{args:?}: {}", o.stderr);
    }
    let bad = sb.dir.path().join("bad.json");
    std::fs::write(&bad, r#"{"path":"a"}"#).expect("file");
    let o = gstn(
        &sb,
        &srv,
        &[
            "checks",
            "report",
            "--project",
            "7",
            "--sha",
            SHA,
            "--name",
            "ci/gates",
            "--status",
            "queued",
            "--annotations-file",
            bad.to_str().expect("path"),
        ],
    );
    assert_eq!(o.code, 2, "{}", o.stderr);
    assert!(srv.seen().is_empty(), "nothing was sent");
}

#[test]
fn checks_list_reads_the_commit_and_prints_each_run() {
    let list = format!(r#"{{"schema_version":1,"head_sha":"{SHA}","check_runs":[{RUN}]}}"#);
    let srv = Mock::start(move |_| Resp::json(200, &list));
    let sb = Sandbox::new();
    let o = gstn(
        &sb,
        &srv,
        &["checks", "list", "--project", "7", "--sha", SHA],
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(o.stdout.trim(), "ci/gates completed success (by ci-runner)");
    let r = srv.seen().pop().expect("a request");
    assert_eq!(
        (r.method.as_str(), r.target.as_str()),
        (
            "GET",
            format!("/api/v1/projects/7/commits/{SHA}/checks").as_str()
        )
    );
    let j = gstn(
        &sb,
        &srv,
        &["checks", "list", "--project", "7", "--sha", SHA, "--json"],
    );
    assert_eq!(j.json()["check_runs"][0]["name"], "ci/gates");
}

/// M3 X2: forge's own run has no reporter (`reported_by: null`): shown as
/// reported by forge, and the `--json` answer still matches its schema.
#[test]
fn checks_list_shows_forge_as_the_reporter_of_its_own_runs() {
    let forge_run = RUN
        .replace(r#""name":"ci/gates""#, r#""name":"forge/spec-drift""#)
        .replace(r#""conclusion":"success""#, r#""conclusion":"neutral""#)
        .replace(r#"{"id":42,"username":"ci-runner"}"#, "null");
    let list = format!(r#"{{"schema_version":1,"head_sha":"{SHA}","check_runs":[{forge_run}]}}"#);
    let srv = Mock::start(move |_| Resp::json(200, &list));
    let sb = Sandbox::new();
    let o = gstn(
        &sb,
        &srv,
        &["checks", "list", "--project", "7", "--sha", SHA],
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(
        o.stdout.trim(),
        "forge/spec-drift completed neutral (by forge)"
    );
    let j = gstn(
        &sb,
        &srv,
        &["checks", "list", "--project", "7", "--sha", SHA, "--json"],
    );
    assert_eq!(j.code, 0, "{}", j.stderr);
    assert!(j.json()["check_runs"][0]["reported_by"].is_null());
}

#[test]
fn the_grant_commands_call_their_routes() {
    let srv = Mock::start(|r| {
        if r.method == "GET" {
            Resp::json(200, r#"{"grants":[{"project_id":7,"namespace":"ci"}]}"#)
        } else {
            Resp::json(
                200,
                r#"{"grants":[{"project_id":7,"namespace":"ci"}],"changed":true}"#,
            )
        }
    });
    let sb = Sandbox::new();
    let o = gstn(
        &sb,
        &srv,
        &[
            "admin",
            "service-account",
            "grant-checks",
            "42",
            "--project",
            "7",
            "ci",
        ],
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(
        o.stdout.contains("ci/ on project 7 granted"),
        "{}",
        o.stdout
    );
    let o = gstn(
        &sb,
        &srv,
        &[
            "admin",
            "service-account",
            "revoke-checks",
            "42",
            "--project",
            "7",
            "kanflow",
        ],
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    let o = gstn(
        &sb,
        &srv,
        &["admin", "service-account", "check-grants", "42"],
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stdout.contains("reports: ci/ on 7"), "{}", o.stdout);
    let seen: Vec<(String, String)> = srv
        .seen()
        .into_iter()
        .map(|r| (r.method, r.target))
        .collect();
    assert_eq!(
        seen,
        [
            (
                "PUT".to_owned(),
                "/api/v1/admin/service-accounts/42/check-grants/7/ci".to_owned()
            ),
            (
                "DELETE".to_owned(),
                "/api/v1/admin/service-accounts/42/check-grants/7/kanflow".to_owned()
            ),
            (
                "GET".to_owned(),
                "/api/v1/admin/service-accounts/42/check-grants".to_owned()
            ),
        ]
    );
    // A grant names its project; forge/ and ext/ are never granted: both
    // refused before any request.
    for args in [
        vec!["admin", "service-account", "grant-checks", "42", "ci"],
        vec![
            "admin",
            "service-account",
            "grant-checks",
            "42",
            "--project",
            "7",
            "forge",
        ],
    ] {
        let o = gstn(&sb, &srv, &args);
        assert_eq!(o.code, 2, "{args:?}: {}", o.stderr);
    }
}
