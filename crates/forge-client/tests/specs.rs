//! End-to-end tests of `gstn spec` (M3 X4: requirements, versions, diff,
//! trace) against a scripted loopback server. All data is synthetic.

#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly

mod common;
use common::{Mock, Out, Resp, Sandbox, TOKEN};

fn gstn(sb: &Sandbox, srv: &Mock, args: &[&str]) -> Out {
    sb.run_env(&srv.url, args, "", &[("GSTN_TOKEN", TOKEN)])
}

fn ok(body: &str) -> Resp {
    Resp::json(200, body)
}

const C: &str = "0123456789abcdef0123456789abcdef01234567";

fn rev() -> String {
    format!(r#"{{"type":"branch","name":"main","full_name":"refs/heads/main","commit":"{C}"}}"#)
}

#[test]
fn spec_requirements_lists_each_requirement_and_problem() {
    let srv = Mock::start(|_| {
        ok(&format!(
            r#"{{"ref":{},"config":"valid","dirs":["spec/"],"documents":[{{"path":"spec/core.md","spec":"app/core","title":"Core","status":"approved","requirements":1,"problems":1}}],"requirements":[{{"id":"APP-CORE-1","path":"spec/core.md","line":6,"priority":"must","deprecated":false,"spec":"app/core","body":"Reload within 10 s."}}],"truncated":false,"problems":[{{"error_code":"FG-REPO-0028","path":"spec/core.md","line":9,"rule":"block_unclosed","detail":null}}],"incomplete":null}}"#,
            rev()
        ))
    });
    let sb = Sandbox::new();
    let o = gstn(
        &sb,
        &srv,
        &[
            "spec",
            "requirements",
            "--project",
            "7",
            "--ref",
            "v1.0",
            "--search",
            "reload",
            "--limit",
            "5",
        ],
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(
        srv.seen().pop().expect("request").target,
        "/api/v1/projects/7/specs/requirements?ref=v1.0&q=reload&limit=5"
    );
    for want in [
        "APP-CORE-1",
        "spec/core.md:6",
        "must",
        "FG-REPO-0028",
        "block_unclosed",
    ] {
        assert!(o.stdout.contains(want), "{want}\n{}", o.stdout);
    }
    let j = gstn(
        &sb,
        &srv,
        &["spec", "requirements", "--project", "7", "--json"],
    );
    assert_eq!(j.code, 0, "{}", j.stderr);
    assert_eq!(j.json()["requirements"][0]["id"], "APP-CORE-1");
}

#[test]
fn spec_versions_diff_and_trace_call_their_routes() {
    let srv = Mock::start(|r| {
        if r.target.contains("/versions") {
            return ok(&format!(
                r#"{{"versions":[{{"name":"v1.1","full_name":"refs/tags/v1.1","commit":"{C}"}}],"truncated":false}}"#
            ));
        }
        if r.target.contains("/diff") {
            return ok(&format!(
                r#"{{"from":{r},"to":{r},"changes":[{{"id":"APP-CORE-2","kind":"added","path":"spec/core.md","line":10}}],"incomplete":null}}"#,
                r = rev()
            ));
        }
        ok(&format!(
            r#"{{"ref":{},"requirements":[{{"id":"ZEVA-CFG-13","source":"specs/engine","path":"spec/config.md","line":9,"deprecated":false,"status":"untested","test_count":0,"tests":[]}}],"references":[{{"line":5,"repo":"specs/secret","status":"unavailable","error_code":"FG-REPO-0026","requirements":0}}],"incomplete":{{"error_code":"FG-REPO-0031","reason":"tag_lines"}}}}"#,
            rev()
        ))
    });
    let sb = Sandbox::new();
    let v = gstn(&sb, &srv, &["spec", "versions", "--project", "7"]);
    assert_eq!(v.code, 0, "{}", v.stderr);
    assert_eq!(
        srv.seen().pop().expect("request").target,
        "/api/v1/projects/7/specs/versions"
    );
    assert!(v.stdout.contains("v1.1"), "{}", v.stdout);
    let d = gstn(
        &sb,
        &srv,
        &[
            "spec",
            "diff",
            "--project",
            "7",
            "--from",
            "v1.0",
            "--to",
            "v1.1",
        ],
    );
    assert_eq!(d.code, 0, "{}", d.stderr);
    assert_eq!(
        srv.seen().pop().expect("request").target,
        "/api/v1/projects/7/specs/diff?from=v1.0&to=v1.1"
    );
    assert!(d.stdout.contains("added  APP-CORE-2"), "{}", d.stdout);
    let t = gstn(
        &sb,
        &srv,
        &["spec", "trace", "--project", "7", "--ref", "main"],
    );
    assert_eq!(t.code, 0, "{}", t.stderr);
    assert_eq!(
        srv.seen().pop().expect("request").target,
        "/api/v1/projects/7/specs/trace?ref=main"
    );
    for want in [
        "untested  ZEVA-CFG-13",
        "specs/engine",
        "FG-REPO-0026",
        "FG-REPO-0031",
        "tag_lines",
    ] {
        assert!(t.stdout.contains(want), "{want}\n{}", t.stdout);
    }
    // `diff` needs both revisions.
    assert_eq!(
        gstn(
            &sb,
            &srv,
            &["spec", "diff", "--project", "7", "--from", "v1.0"]
        )
        .code,
        2
    );
}
