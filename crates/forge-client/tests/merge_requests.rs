//! End-to-end tests of `gstn mr …` (M3 R1a) against a scripted loopback
//! server. All data is synthetic.

#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly

mod common;
use common::{Mock, Out, Resp, Sandbox, TOKEN};
use serde_json::{json, Value};

fn gstn(sb: &Sandbox, srv: &Mock, args: &[&str]) -> Out {
    sb.run_env(&srv.url, args, "", &[("GSTN_TOKEN", TOKEN)])
}

const MR: &str = r#"{"project_id":7,"iid":3,"title":"Add the login form","description":"synthetic","state":"open","draft":true,"source_branch":"feature/login","target_branch":"main","author_id":12,"head_sha":null,"created_at":"2026-11-02T00:00:00Z","updated_at":"2026-11-02T00:00:00Z","closed_at":null,"closed_by":null}"#;

fn ok(body: &str) -> Resp {
    Resp::json(200, body)
}

#[test]
fn mr_create_posts_the_branches_in_the_body_and_renders_the_result() {
    let srv = Mock::start(|_| Resp::json(201, MR));
    let sb = Sandbox::new();
    let o = gstn(
        &sb,
        &srv,
        &[
            "mr",
            "create",
            "--project",
            "7",
            "--source",
            "feature/login",
            "--target",
            "main",
            "--title",
            "Add the login form",
            "--draft",
            "--json",
        ],
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    let r = srv.seen().pop().expect("a request");
    assert_eq!(
        (r.method.as_str(), r.target.as_str()),
        ("POST", "/api/v1/projects/7/merge-requests")
    );
    assert_eq!(
        serde_json::from_str::<Value>(&r.body).expect("json"),
        json!({"title": "Add the login form", "source_branch": "feature/login",
               "target_branch": "main", "draft": true})
    );
    assert_eq!(o.json()["iid"], 3);
    let h = gstn(
        &sb,
        &srv,
        &[
            "mr",
            "create",
            "--project",
            "7",
            "--source",
            "feature/login",
            "--target",
            "main",
            "--title",
            "x",
            "--description",
            "",
        ],
    );
    assert_eq!(h.code, 0, "{}", h.stderr);
    assert!(h.stdout.contains("!3") && h.stdout.contains("feature/login -> main"));
    let r = srv.seen().pop().expect("a request");
    assert_eq!(
        serde_json::from_str::<Value>(&r.body).expect("json")["description"],
        ""
    );
}

#[test]
fn mr_list_filters_by_state_pages_with_before_and_all_follows_next_before() {
    let srv = Mock::start(|r| {
        if r.target.contains("before=3") {
            ok(
                r#"{"merge_requests":[{"project_id":7,"iid":1,"title":"old","state":"closed","draft":false,"source_branch":"a","target_branch":"main","author_id":12}],"next_before":null}"#,
            )
        } else {
            ok(&format!(r#"{{"merge_requests":[{MR}],"next_before":3}}"#))
        }
    });
    let sb = Sandbox::new();
    let o = gstn(
        &sb,
        &srv,
        &[
            "mr",
            "list",
            "--project",
            "7",
            "--state",
            "all",
            "--limit",
            "1",
            "--json",
        ],
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(
        srv.seen().pop().expect("a request").target,
        "/api/v1/projects/7/merge-requests?state=all&limit=1"
    );
    assert_eq!(o.json()["next_before"], 3);
    let h = gstn(&sb, &srv, &["mr", "list", "--project", "7"]);
    assert_eq!(h.code, 0, "{}", h.stderr);
    assert!(h.stdout.contains("more: --before 3"), "{}", h.stdout);
    let all = gstn(
        &sb,
        &srv,
        &["mr", "list", "--project", "7", "--all", "--json"],
    );
    assert_eq!(all.code, 0, "{}", all.stderr);
    let iids: Vec<i64> = all.json()["merge_requests"]
        .as_array()
        .expect("rows")
        .iter()
        .map(|m| m["iid"].as_i64().expect("iid"))
        .collect();
    assert_eq!(iids, [3, 1]);
    // An unknown state never reaches the server.
    let before = srv.seen().len();
    let bad = gstn(
        &sb,
        &srv,
        &["mr", "list", "--project", "7", "--state", "draft"],
    );
    assert_eq!(bad.code, 2);
    assert_eq!(srv.seen().len(), before);
}

#[test]
fn mr_view_edit_close_and_reopen_call_their_routes() {
    let srv = Mock::start(|_| ok(MR));
    let sb = Sandbox::new();
    for (args, method, target) in [
        (
            vec!["mr", "view", "--project", "7", "--iid", "3"],
            "GET",
            "/api/v1/projects/7/merge-requests/3",
        ),
        (
            vec!["mr", "close", "--project", "7", "--iid", "3"],
            "POST",
            "/api/v1/projects/7/merge-requests/3/close",
        ),
        (
            vec!["mr", "reopen", "--project", "7", "--iid", "3"],
            "POST",
            "/api/v1/projects/7/merge-requests/3/reopen",
        ),
    ] {
        let o = gstn(&sb, &srv, &args);
        assert_eq!(o.code, 0, "{args:?}: {}", o.stderr);
        let r = srv.seen().pop().expect("a request");
        assert_eq!((r.method.as_str(), r.target.as_str()), (method, target));
    }
    let o = gstn(
        &sb,
        &srv,
        &[
            "mr",
            "edit",
            "--project",
            "7",
            "--iid",
            "3",
            "--ready",
            "--target",
            "release",
        ],
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    let r = srv.seen().pop().expect("a request");
    assert_eq!(
        (r.method.as_str(), r.target.as_str()),
        ("PATCH", "/api/v1/projects/7/merge-requests/3")
    );
    assert_eq!(
        serde_json::from_str::<Value>(&r.body).expect("json"),
        json!({"target_branch": "release", "draft": false})
    );
    // Nothing to change, or both flags: refused before any request.
    let before = srv.seen().len();
    let none = gstn(
        &sb,
        &srv,
        &["mr", "edit", "--project", "7", "--iid", "3", "--json"],
    );
    assert_eq!(none.code, 2);
    assert!(none.err_json()["detail"]
        .as_str()
        .expect("detail")
        .contains("nothing_to_change"));
    let both = gstn(
        &sb,
        &srv,
        &[
            "mr",
            "edit",
            "--project",
            "7",
            "--iid",
            "3",
            "--draft",
            "--ready",
        ],
    );
    assert_eq!(both.code, 2);
    let bad_iid = gstn(&sb, &srv, &["mr", "view", "--project", "7", "--iid", "0"]);
    assert_eq!(bad_iid.code, 2);
    assert_eq!(srv.seen().len(), before);
}

#[test]
fn mr_errors_keep_the_server_code_and_exit_class() {
    let srv = Mock::start(|_| {
        Resp::json(
            409,
            r#"{"error_code":"FG-REVIEW-0001","user_message":"同じブランチどうしの開いているマージリクエストがすでにあります。","request_id":"req-9"}"#,
        )
    });
    let sb = Sandbox::new();
    let o = gstn(
        &sb,
        &srv,
        &[
            "mr",
            "create",
            "--project",
            "7",
            "--source",
            "a",
            "--target",
            "main",
            "--title",
            "x",
            "--json",
        ],
    );
    assert_eq!(o.code, 5, "a conflict: {}", o.stderr);
    assert_eq!(o.err_json()["error_code"], "FG-REVIEW-0001");
    assert_eq!(o.err_json()["request_id"], "req-9");
    assert!(!o.stderr.contains(TOKEN));
}

const VERSIONS: &str = r#"{"iid":3,"versions":[{"version":1,"head_sha":"1111111111111111111111111111111111111111","base_sha":"0000000000000000000000000000000000000001","merge_base_sha":"0000000000000000000000000000000000000001","pushed_by":null,"created_at":"2026-11-02T00:00:00Z","refs_purged":true},{"version":2,"head_sha":"2222222222222222222222222222222222222222","base_sha":null,"merge_base_sha":null,"pushed_by":12,"created_at":"2026-11-02T01:00:00Z","refs_purged":false}]}"#;

const COMPARED: &str = r#"{"from_version":1,"to_version":2,"from_sha":"1111111111111111111111111111111111111111","to_sha":"2222222222222222222222222222222222222222","commits":[{"oid":"2222222222222222222222222222222222222222","parents":["1111111111111111111111111111111111111111"],"author_name":"forge-test","author_email":"forge-test@example.invalid","authored_at":"2026-11-02T01:00:00+09:00","committer_name":"forge-test","committer_email":"forge-test@example.invalid","committed_at":"2026-11-02T01:00:00+09:00","subject":"synthetic"}],"commits_truncated":false,"diff":{"files":[{"status":"modified","old_path":"feature.txt","new_path":"feature.txt","old_mode":"100644","new_mode":"100644","old_oid":"a","new_oid":"b","similarity":null,"binary":false,"patch":"@@ -1 +1 @@\n-one\n+two\n","omitted":null}],"truncated":false}}"#;

const MERGEABILITY: &str = r#"{"iid":3,"status":"cannot_be_merged","reason":"conflict","head_sha":"2222222222222222222222222222222222222222","checked_head_sha":"2222222222222222222222222222222222222222","checked_target_sha":"3333333333333333333333333333333333333333","checked_at":"2026-11-02T02:00:00Z","conflicts":["feature.txt","\u001b[31mevil"],"conflicts_truncated":true}"#;

/// M3 R2: `mr versions`, `mr diff` (a version or two versions) and
/// `mr mergeability` call their routes; server text is sanitized.
#[test]
fn mr_versions_diff_and_mergeability_call_their_routes() {
    let srv = Mock::start(|r| {
        if r.target.ends_with("/versions") {
            ok(VERSIONS)
        } else if r.target.contains("/mergeability") {
            ok(MERGEABILITY)
        } else {
            ok(COMPARED)
        }
    });
    let sb = Sandbox::new();
    let o = gstn(
        &sb,
        &srv,
        &["mr", "versions", "--project", "7", "--iid", "3"],
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    let r = srv.seen().pop().expect("a request");
    assert_eq!(r.target, "/api/v1/projects/7/merge-requests/3/versions");
    assert!(
        o.stdout.contains("v1  1111") && o.stdout.contains("(purged)"),
        "{}",
        o.stdout
    );
    assert!(o.stdout.contains("v2  2222"), "{}", o.stdout);

    let o = gstn(
        &sb,
        &srv,
        &[
            "mr",
            "diff",
            "--project",
            "7",
            "--iid",
            "3",
            "--version",
            "2",
        ],
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    let r = srv.seen().pop().expect("a request");
    assert_eq!(
        r.target,
        "/api/v1/projects/7/merge-requests/3/versions/2/diff"
    );
    let o = gstn(
        &sb,
        &srv,
        &[
            "mr",
            "diff",
            "--project",
            "7",
            "--iid",
            "3",
            "--from",
            "1",
            "--to",
            "2",
            "--patch",
        ],
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    let r = srv.seen().pop().expect("a request");
    assert_eq!(
        r.target,
        "/api/v1/projects/7/merge-requests/3/compare?from=1&to=2"
    );
    assert!(o.stdout.contains("from v1 1111"), "{}", o.stdout);
    assert!(o.stdout.contains("modified  feature.txt"), "{}", o.stdout);
    assert!(o.stdout.contains("+two"), "{}", o.stdout);

    let o = gstn(
        &sb,
        &srv,
        &["mr", "mergeability", "--project", "7", "--iid", "3"],
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    let r = srv.seen().pop().expect("a request");
    assert_eq!(r.target, "/api/v1/projects/7/merge-requests/3/mergeability");
    assert!(
        o.stdout.contains("cannot_be_merged (conflict)"),
        "{}",
        o.stdout
    );
    assert!(o.stdout.contains("conflict: feature.txt"), "{}", o.stdout);
    assert!(
        !o.stdout.contains('\u{1b}'),
        "terminal escapes are sanitized"
    );
    assert!(o.stdout.contains("more conflicted files"), "{}", o.stdout);

    // Neither a version nor a range, or half a range: refused before any
    // request.
    let before = srv.seen().len();
    for args in [
        vec!["mr", "diff", "--project", "7", "--iid", "3"],
        vec!["mr", "diff", "--project", "7", "--iid", "3", "--from", "1"],
        vec![
            "mr",
            "diff",
            "--project",
            "7",
            "--iid",
            "3",
            "--version",
            "1",
            "--from",
            "1",
            "--to",
            "2",
        ],
        vec![
            "mr",
            "diff",
            "--project",
            "7",
            "--iid",
            "3",
            "--version",
            "0",
        ],
    ] {
        let o = gstn(&sb, &srv, &args);
        assert_eq!(o.code, 2, "{args:?}: {}", o.stderr);
    }
    assert_eq!(srv.seen().len(), before);
}

const DISCUSSION: &str = r#"{"id":12,"kind":"inline","position":{"version":2,"path":"src/lib.rs","side":"new","line":10},"outdated":true,"started_by":12,"resolved":true,"resolved_by":14,"resolved_at":"2026-11-02T03:00:00Z","created_at":"2026-11-02T02:00:00Z","notes":[{"id":40,"author_id":12,"body":"Rename this\n\u001b[31mred\nthird","created_at":"2026-11-02T02:00:00Z"}]}"#;

/// M3 R3: `mr comment` starts a general or an inline discussion or
/// replies; `mr resolve` resolves or unresolves; `mr discussions` lists.
/// Each calls its route with its body; note bodies are printed line by
/// line, sanitized.
#[test]
fn mr_comment_resolve_and_discussions_call_their_routes() {
    let srv = Mock::start(|r| {
        if r.method == "GET" {
            ok(&format!(
                r#"{{"iid":3,"discussions":[{DISCUSSION}],"truncated":false}}"#
            ))
        } else if r.target.ends_with("resolve") {
            ok(DISCUSSION)
        } else {
            Resp::json(201, DISCUSSION)
        }
    });
    let sb = Sandbox::new();
    let base = "/api/v1/projects/7/merge-requests/3/discussions";
    let o = gstn(
        &sb,
        &srv,
        &[
            "mr",
            "comment",
            "--project",
            "7",
            "--iid",
            "3",
            "--body",
            "Looks good",
        ],
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    let r = srv.seen().pop().expect("a request");
    assert_eq!((r.method.as_str(), r.target.as_str()), ("POST", base));
    assert_eq!(
        serde_json::from_str::<Value>(&r.body).expect("json"),
        json!({"body": "Looks good"})
    );
    let o = gstn(
        &sb,
        &srv,
        &[
            "mr",
            "comment",
            "--project",
            "7",
            "--iid",
            "3",
            "--version",
            "2",
            "--path",
            "src/lib.rs",
            "--line",
            "10",
            "--body",
            "Rename this",
        ],
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    let r = srv.seen().pop().expect("a request");
    assert_eq!(
        serde_json::from_str::<Value>(&r.body).expect("json"),
        json!({"body": "Rename this", "position":
               {"version": 2, "path": "src/lib.rs", "line": 10, "side": "new"}})
    );
    assert!(
        o.stdout.contains("#12 inline src/lib.rs:10 (new, v2)"),
        "{}",
        o.stdout
    );
    assert!(o.stdout.contains("[outdated]"), "{}", o.stdout);
    assert!(
        !o.stdout.contains('\u{1b}'),
        "terminal escapes are sanitized"
    );
    assert!(
        o.stdout.contains("    third"),
        "every line is printed: {}",
        o.stdout
    );
    let o = gstn(
        &sb,
        &srv,
        &[
            "mr",
            "comment",
            "--project",
            "7",
            "--iid",
            "3",
            "--discussion",
            "12",
            "--body",
            "Done",
        ],
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    let r = srv.seen().pop().expect("a request");
    assert_eq!(r.target, format!("{base}/12/notes"));
    assert_eq!(
        serde_json::from_str::<Value>(&r.body).expect("json"),
        json!({"body": "Done"})
    );
    for (extra, verb) in [(None, "resolve"), (Some("--unresolve"), "unresolve")] {
        let mut args = vec![
            "mr",
            "resolve",
            "--project",
            "7",
            "--iid",
            "3",
            "--discussion",
            "12",
        ];
        args.extend(extra);
        let o = gstn(&sb, &srv, &args);
        assert_eq!(o.code, 0, "{}", o.stderr);
        let r = srv.seen().pop().expect("a request");
        assert_eq!(
            (r.method.as_str(), r.target.as_str()),
            ("POST", format!("{base}/12/{verb}").as_str())
        );
    }
    let o = gstn(
        &sb,
        &srv,
        &[
            "mr",
            "discussions",
            "--project",
            "7",
            "--iid",
            "3",
            "--json",
        ],
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    let r = srv.seen().pop().expect("a request");
    assert_eq!((r.method.as_str(), r.target.as_str()), ("GET", base));
    assert_eq!(o.json()["discussions"][0]["id"], 12);
    // A reply never carries a position; an inline comment needs its line.
    let before = srv.seen().len();
    for args in [
        vec![
            "mr",
            "comment",
            "--project",
            "7",
            "--iid",
            "3",
            "--body",
            "x",
            "--discussion",
            "1",
            "--line",
            "2",
        ],
        vec![
            "mr",
            "comment",
            "--project",
            "7",
            "--iid",
            "3",
            "--body",
            "x",
            "--version",
            "1",
            "--path",
            "a",
        ],
    ] {
        let o = gstn(&sb, &srv, &args);
        assert_ne!(o.code, 0, "{args:?}");
    }
    assert_eq!(srv.seen().len(), before, "refused before any request");
}

/// Review round 1: a position outside the diff (FG-REVIEW-0004, 422) is a
/// usage error (exit 2) and a full thread (FG-REVIEW-0005, 409) a conflict
/// (exit 5); both keep the server's code and request id.
#[test]
fn mr_comment_errors_keep_their_code_and_exit_class() {
    for (status, code, exit) in [(422, "FG-REVIEW-0004", 2), (409, "FG-REVIEW-0005", 5)] {
        let body =
            format!(r#"{{"error_code":"{code}","user_message":"synthetic","request_id":"req-c"}}"#);
        let srv = Mock::start(move |_| Resp::json(status, &body));
        let sb = Sandbox::new();
        let o = gstn(
            &sb,
            &srv,
            &[
                "mr",
                "comment",
                "--project",
                "7",
                "--iid",
                "3",
                "--version",
                "1",
                "--path",
                "lib.rs",
                "--line",
                "99",
                "--body",
                "x",
                "--json",
            ],
        );
        assert_eq!(o.code, exit, "{code}");
        let e = o.err_json();
        assert_eq!(e["error_code"], code);
        assert_eq!(e["request_id"], "req-c");
    }
}

const SHA: &str = "2222222222222222222222222222222222222222";

fn attempt(state: &str, extra: &str) -> String {
    format!(
        r#"{{"merge_id":41,"iid":3,"state":"{state}","method":"squash","head_sha":"{SHA}","target_branch":"main","delete_source_branch":true,"expected_old":null,"new_sha":null,"reason":null,"error_code":null,"source_outcome":null,"request_id":"req-merge-41","created_at":"2026-11-02T00:00:00Z","updated_at":"2026-11-02T00:00:00Z","finished_at":null{extra}}}"#
    )
}

/// M3 R4: `mr merge` posts the reviewed head, the method and the source
/// deletion, then (`--wait`) polls `GET …/merge` until the attempt is final.
#[test]
fn mr_merge_posts_the_sha_and_waits_until_applied() {
    let srv = Mock::start(|r| {
        if r.method == "POST" {
            Resp::json(202, &attempt("queued", ""))
        } else {
            let applied = attempt("applied", "")
                .replace(
                    r#""new_sha":null"#,
                    r#""new_sha":"4444444444444444444444444444444444444444""#,
                )
                .replace(r#""source_outcome":null"#, r#""source_outcome":"deleted""#);
            Resp::json(200, &format!(r#"{{"iid":3,"merge":{applied}}}"#))
        }
    });
    let sb = Sandbox::new();
    let o = gstn(
        &sb,
        &srv,
        &[
            "mr",
            "merge",
            "--project",
            "7",
            "--iid",
            "3",
            "--sha",
            SHA,
            "--method",
            "squash",
            "--delete-source-branch",
            "--wait",
        ],
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stdout.contains("applied"), "{}", o.stdout);
    assert!(o
        .stdout
        .contains("merged as: 4444444444444444444444444444444444444444"));
    assert!(o.stdout.contains("source branch: deleted"));
    let seen = srv.seen();
    assert_eq!(seen[0].method, "POST");
    assert_eq!(seen[0].target, "/api/v1/projects/7/merge-requests/3/merge");
    let body: Value = serde_json::from_str(&seen[0].body).expect("json");
    assert_eq!(
        body,
        json!({"sha": SHA, "method": "squash", "delete_source_branch": true})
    );
    assert_eq!(seen[1].method, "GET");
    assert_eq!(seen[1].target, "/api/v1/projects/7/merge-requests/3/merge");
    // Without --wait: the queued attempt, one request.
    let srv = Mock::start(|_| Resp::json(202, &attempt("queued", "")));
    let o = gstn(
        &sb,
        &srv,
        &[
            "mr",
            "merge",
            "--project",
            "7",
            "--iid",
            "3",
            "--sha",
            SHA,
            "--json",
        ],
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    let out: Value = serde_json::from_str(o.stdout.trim()).expect("json");
    assert_eq!(out["state"], "queued");
    assert_eq!(srv.seen().len(), 1);
}

/// A merge the job refused ends `superseded`: `--wait` exits with the
/// attempt's own code and the merge's request id (409 → exit 5).
#[test]
fn mr_merge_wait_exits_with_the_refusal_code() {
    let srv = Mock::start(|r| {
        if r.method == "POST" {
            Resp::json(202, &attempt("queued", ""))
        } else {
            let refused = attempt("superseded", "")
                .replace(r#""reason":null"#, r#""reason":"target_moved""#)
                .replace(r#""error_code":null"#, r#""error_code":"FG-REVIEW-0008""#);
            Resp::json(200, &format!(r#"{{"iid":3,"merge":{refused}}}"#))
        }
    });
    let sb = Sandbox::new();
    let o = gstn(
        &sb,
        &srv,
        &[
            "mr",
            "merge",
            "--project",
            "7",
            "--iid",
            "3",
            "--sha",
            SHA,
            "--wait",
            "--json",
        ],
    );
    assert_eq!(o.code, 5, "{}", o.stderr);
    assert_eq!(o.err_json()["error_code"], "FG-REVIEW-0008");
    assert_eq!(o.err_json()["request_id"], "req-merge-41");
    // A request refused at once keeps the server's code (here 0011, 409).
    let srv = Mock::start(|_| {
        Resp::json(
            409,
            r#"{"error_code":"FG-REVIEW-0011","user_message":"このマージリクエストはマージを実行中です。","request_id":"req-9"}"#,
        )
    });
    let o = gstn(
        &sb,
        &srv,
        &[
            "mr",
            "merge",
            "--project",
            "7",
            "--iid",
            "3",
            "--sha",
            SHA,
            "--json",
        ],
    );
    assert_eq!(o.code, 5, "{}", o.stderr);
    assert_eq!(o.err_json()["error_code"], "FG-REVIEW-0011");
}

/// FG-CLI-0008: the attempt is not final when `--wait-timeout` passes —
/// retryable (exit 6); nothing is cancelled. `mr merge-status` shows it.
#[test]
fn merge_wait_that_times_out_is_retryable() {
    let srv = Mock::start(|_| {
        Resp::json(
            200,
            &format!(r#"{{"iid":3,"merge":{}}}"#, attempt("executing", "")),
        )
    });
    let sb = Sandbox::new();
    let o = gstn(
        &sb,
        &srv,
        &[
            "mr",
            "merge-status",
            "--project",
            "7",
            "--iid",
            "3",
            "--wait",
            "--wait-timeout",
            "0",
            "--json",
        ],
    );
    assert_eq!(o.code, 6, "{}", o.stderr);
    assert_eq!(o.err_json()["error_code"], "FG-CLI-0008");
    assert_eq!(o.err_json()["retryable"], true);
    let o = gstn(
        &sb,
        &srv,
        &["mr", "merge-status", "--project", "7", "--iid", "3"],
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stdout.contains("executing"), "{}", o.stdout);
    // A wrong argument never reaches the server.
    let before = srv.seen().len();
    let o = gstn(
        &sb,
        &srv,
        &[
            "mr",
            "merge",
            "--project",
            "7",
            "--iid",
            "3",
            "--sha",
            SHA,
            "--method",
            "rebase",
        ],
    );
    assert_eq!(o.code, 2, "{}", o.stderr);
    assert_eq!(srv.seen().len(), before);
}

/// Review round 1: `--wait` polls through what is not yet an answer — a
/// 503 while `forge serve` restarts, `merge: null`, another attempt's
/// state — and succeeds only on its own attempt's final state.
#[test]
fn merge_wait_polls_through_transient_failures_and_other_attempts() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let gets = std::sync::Arc::new(AtomicUsize::new(0));
    let seen = std::sync::Arc::clone(&gets);
    let srv = Mock::start(move |r| {
        if r.method == "POST" {
            return Resp::json(202, &attempt("queued", ""));
        }
        match seen.fetch_add(1, Ordering::SeqCst) {
            0 => Resp::json(
                503,
                r#"{"error_code":"FG-WEB-0008","user_message":"busy","request_id":"req-1"}"#,
            ),
            1 => Resp::json(200, r#"{"iid":3,"merge":null}"#),
            2 => {
                let other = attempt("applied", "").replace(r#""merge_id":41"#, r#""merge_id":40"#);
                Resp::json(200, &format!(r#"{{"iid":3,"merge":{other}}}"#))
            }
            _ => Resp::json(
                200,
                &format!(r#"{{"iid":3,"merge":{}}}"#, attempt("applied", "")),
            ),
        }
    });
    let sb = Sandbox::new();
    let o = gstn(
        &sb,
        &srv,
        &[
            "mr",
            "merge",
            "--project",
            "7",
            "--iid",
            "3",
            "--sha",
            SHA,
            "--wait",
            "--json",
        ],
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    let out: Value = serde_json::from_str(o.stdout.trim()).expect("json");
    assert_eq!(
        (out["merge_id"].clone(), out["state"].clone()),
        (json!(41), json!("applied"))
    );
    assert_eq!(gets.load(Ordering::SeqCst), 4);
    // `merge: null` until the deadline is a timeout, never a success.
    let srv = Mock::start(|_| Resp::json(200, r#"{"iid":3,"merge":null}"#));
    let o = gstn(
        &sb,
        &srv,
        &[
            "mr",
            "merge-status",
            "--project",
            "7",
            "--iid",
            "3",
            "--wait",
            "--wait-timeout",
            "0",
            "--json",
        ],
    );
    assert_eq!(o.code, 6, "{}", o.stderr);
    assert_eq!(o.err_json()["error_code"], "FG-CLI-0008");
}

const APPROVED: &str = r#"{"iid":3,"outcome":"approved","counted":false,"uncounted_reason":"committer","approval":{"approval_id":5,"approver_id":12,"head_sha":"2222222222222222222222222222222222222222","target_branch":"main","created_at":"2026-11-03T00:00:00Z"}}"#;

const APPROVAL_STATE: &str = r#"{"iid":3,"head_sha":"2222222222222222222222222222222222222222","target_branch":"main","approvals":[{"approval_id":5,"approver_id":12,"username":"rita","counted":false,"uncounted_reason":"committer","committer_push_audit_id":44}],"rules":[{"name":"backend","applies":true,"required":1,"counted":0}],"codeowners":[{"line":2,"pattern":"*.rs","owners":["@mara"],"paths":1,"satisfied":false}],"config_change_approved":null,"blocked_by":[{"reason":"approvals_missing","detail":"rule=backend required=1 counted=0"},{"reason":"codeowner_approval_missing","detail":"pattern=*.rs owners=@mara"}]}"#;

/// M3 R5: `mr approve` posts the sha it was given; `mr unapprove` and
/// `mr approvals` call their routes; the human output says why an approval
/// does not count and what blocks.
#[test]
fn mr_approve_unapprove_and_approvals_call_their_routes() {
    let srv = Mock::start(|r| {
        if r.target.ends_with("/approvals") {
            ok(APPROVAL_STATE)
        } else {
            ok(APPROVED)
        }
    });
    let sb = Sandbox::new();
    let sha = "2222222222222222222222222222222222222222";
    let o = gstn(
        &sb,
        &srv,
        &[
            "mr",
            "approve",
            "--project",
            "7",
            "--iid",
            "3",
            "--sha",
            sha,
        ],
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(
        o.stdout.contains("does not count: committer"),
        "{}",
        o.stdout
    );
    let r = srv.seen().pop().expect("a request");
    assert_eq!(
        (r.method.as_str(), r.target.as_str()),
        ("POST", "/api/v1/projects/7/merge-requests/3/approve")
    );
    assert_eq!(
        serde_json::from_str::<Value>(&r.body).expect("json"),
        json!({"sha": sha})
    );
    let o = gstn(
        &sb,
        &srv,
        &["mr", "unapprove", "--project", "7", "--iid", "3"],
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    let r = srv.seen().pop().expect("a request");
    assert_eq!(
        (r.method.as_str(), r.target.as_str()),
        ("POST", "/api/v1/projects/7/merge-requests/3/unapprove")
    );
    let o = gstn(
        &sb,
        &srv,
        &["mr", "approvals", "--project", "7", "--iid", "3"],
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(
        o.stdout.contains("blocked: approvals_missing"),
        "{}",
        o.stdout
    );
    assert!(o.stdout.contains("CODEOWNERS line 2"), "{}", o.stdout);
    // A sha that is not a full commit id never leaves the client.
    let before = srv.seen().len();
    let bad = gstn(
        &sb,
        &srv,
        &[
            "mr",
            "approve",
            "--project",
            "7",
            "--iid",
            "3",
            "--sha",
            "main",
        ],
    );
    assert_eq!(bad.code, 2);
    assert_eq!(srv.seen().len(), before);
}

/// M3 R5: `approval-rule set` PUTs the rule by name, `list` and `remove`
/// call their routes; a name that could leave the path is refused.
#[test]
fn approval_rule_commands_call_their_routes() {
    let rule = r#"{"approval_rule_id":1,"name":"backend","target_pattern":"refs/heads/main","required_approvals":2,"user_ids":[12],"group_ids":[]}"#;
    let srv = Mock::start(move |r| {
        if r.method == "DELETE" {
            Resp::json(204, "")
        } else if r.method == "GET" {
            ok(&format!(r#"{{"rules":[{rule}]}}"#))
        } else {
            ok(rule)
        }
    });
    let sb = Sandbox::new();
    let o = gstn(
        &sb,
        &srv,
        &[
            "approval-rule",
            "set",
            "--project",
            "7",
            "--name",
            "backend",
            "--required",
            "2",
            "--target-pattern",
            "refs/heads/main",
            "--user",
            "rita",
        ],
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    let r = srv.seen().pop().expect("a request");
    assert_eq!(
        (r.method.as_str(), r.target.as_str()),
        ("PUT", "/api/v1/projects/7/approval-rules/backend")
    );
    assert_eq!(
        serde_json::from_str::<Value>(&r.body).expect("json"),
        json!({"target_pattern": "refs/heads/main", "required_approvals": 2, "users": ["rita"], "groups": []})
    );
    let o = gstn(&sb, &srv, &["approval-rule", "list", "--project", "7"]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    let o = gstn(
        &sb,
        &srv,
        &[
            "approval-rule",
            "remove",
            "--project",
            "7",
            "--name",
            "backend",
            "--yes",
        ],
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    let r = srv.seen().pop().expect("a request");
    assert_eq!(
        (r.method.as_str(), r.target.as_str()),
        ("DELETE", "/api/v1/projects/7/approval-rules/backend")
    );
    let before = srv.seen().len();
    for name in ["../x", "Backend", ""] {
        let bad = gstn(
            &sb,
            &srv,
            &[
                "approval-rule",
                "remove",
                "--project",
                "7",
                "--name",
                name,
                "--yes",
            ],
        );
        assert_eq!(bad.code, 2, "{name}");
    }
    assert_eq!(srv.seen().len(), before);
}

// ── M3 R6: the merge status, merge settings, the CODEOWNERS switch, merge rules ──

const STATUS: &str = r#"{"schema_version":1,"iid":3,"head_sha":"2222222222222222222222222222222222222222","target_branch":"main","method":"merge_commit","mergeable":false,"reasons":[{"reason":"approvals_missing","detail":"rule=@protected_branch required=1 counted=0"},{"reason":"required_check_missing","detail":"name=ci/gates"}],"approvals":{"rules":[{"name":"@protected_branch","applies":true,"required":1,"counted":0}],"approvals":[],"codeowners":[]},"checks":[{"name":"ci/gates","status":null,"conclusion":null}],"discussions":{"unresolved":0},"codeowners":{"enforced":true,"suspended_until":null}}"#;

const SETTINGS: &str = r#"{"project_id":7,"allowed_methods":["merge_commit","squash"],"default_method":"merge_commit","delete_source_default":false,"reset_approvals_on_push":true,"committer_approvals_excluded":true,"codeowners":{"enforced":true,"suspended_at":null,"suspended_until":null},"stored":false,"updated_at":null}"#;

/// `gstn mr status` reads the stable contract; `--json` passes it through
/// unchanged and the human output lists every reason.
#[test]
fn mr_status_reads_the_contract_and_lists_every_reason() {
    let srv = Mock::start(|_| ok(STATUS));
    let sb = Sandbox::new();
    let o = gstn(
        &sb,
        &srv,
        &["mr", "status", "--project", "7", "--iid", "3", "--json"],
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    let r = srv.seen().pop().expect("a request");
    assert_eq!(
        (r.method.as_str(), r.target.as_str()),
        ("GET", "/api/v1/projects/7/merge-requests/3/status")
    );
    assert_eq!(o.json()["schema_version"], 1);
    assert_eq!(o.json()["reasons"][1]["reason"], "required_check_missing");
    let h = gstn(&sb, &srv, &["mr", "status", "--project", "7", "--iid", "3"]);
    assert_eq!(h.code, 0, "{}", h.stderr);
    assert!(h.stdout.contains("!3: blocked"), "{}", h.stdout);
    assert!(
        h.stdout.contains("required_check_missing (name=ci/gates)"),
        "{}",
        h.stdout
    );
}

/// `merge-settings set` reads the settings and replaces only what was
/// asked; `show` reads them.
#[test]
fn merge_settings_set_keeps_what_was_not_asked() {
    let srv = Mock::start(|_| ok(SETTINGS));
    let sb = Sandbox::new();
    let o = gstn(
        &sb,
        &srv,
        &[
            "merge-settings",
            "set",
            "--project",
            "7",
            "--default",
            "squash",
            "--reset-approvals-on-push",
            "false",
        ],
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    let seen = srv.seen();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].method, "GET");
    assert_eq!(
        (seen[1].method.as_str(), seen[1].target.as_str()),
        ("PUT", "/api/v1/projects/7/merge-settings")
    );
    assert_eq!(
        serde_json::from_str::<Value>(&seen[1].body).expect("json"),
        json!({"allowed_methods": ["merge_commit", "squash"], "default_method": "squash",
               "delete_source_default": false, "reset_approvals_on_push": false,
               "committer_approvals_excluded": true})
    );
    let show = gstn(&sb, &srv, &["merge-settings", "show", "--project", "7"]);
    assert_eq!(show.code, 0, "{}", show.stderr);
    assert!(
        show.stdout.contains("CODEOWNERS enforced"),
        "{}",
        show.stdout
    );
    let bad = gstn(
        &sb,
        &srv,
        &[
            "merge-settings",
            "set",
            "--project",
            "7",
            "--allow",
            "rebase",
        ],
    );
    assert_eq!(
        bad.code, 2,
        "a method gstn does not know is refused locally"
    );
}

/// `codeowners-enforcement suspend` needs `--for` within 1..=1440 (clap)
/// and sends the expiry; `enforce` sends `enforced: true`.
#[test]
fn codeowners_enforcement_suspends_for_a_bounded_time() {
    let srv = Mock::start(|_| ok(SETTINGS));
    let sb = Sandbox::new();
    let o = gstn(
        &sb,
        &srv,
        &[
            "codeowners-enforcement",
            "suspend",
            "--project",
            "7",
            "--for",
            "60",
        ],
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    let r = srv.seen().pop().expect("a request");
    assert_eq!(
        (r.method.as_str(), r.target.as_str()),
        (
            "PUT",
            "/api/v1/projects/7/merge-settings/codeowners-enforcement"
        )
    );
    assert_eq!(
        serde_json::from_str::<Value>(&r.body).expect("json"),
        json!({"enforced": false, "for_minutes": 60})
    );
    let o = gstn(
        &sb,
        &srv,
        &["codeowners-enforcement", "enforce", "--project", "7"],
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    let r = srv.seen().pop().expect("a request");
    assert_eq!(
        serde_json::from_str::<Value>(&r.body).expect("json"),
        json!({"enforced": true})
    );
    let before = srv.seen().len();
    for minutes in ["0", "1441", "x"] {
        let bad = gstn(
            &sb,
            &srv,
            &[
                "codeowners-enforcement",
                "suspend",
                "--project",
                "7",
                "--for",
                minutes,
            ],
        );
        assert_eq!(bad.code, 2, "{minutes}");
    }
    assert_eq!(srv.seen().len(), before);
}

/// `merge-rule set` PUTs the whole merge rule; `remove` needs `--yes`.
#[test]
fn merge_rule_commands_call_their_routes() {
    let rule = r#"{"id":2,"pattern":"refs/heads/main","push_min_role":"maintainer","created_at":"2026-11-03T00:00:00Z","updated_at":"2026-11-03T00:00:00Z","merge_rule":{"required_approvals":1,"required_checks":["ci/gates"],"require_resolved_discussions":true,"direct_push_forbidden":true},"warnings":["required_checks_unreportable"]}"#;
    let srv = Mock::start(move |r| {
        if r.method == "DELETE" {
            Resp::json(204, "")
        } else if r.method == "GET" {
            ok(&format!(r#"{{"rules":[{rule}]}}"#))
        } else {
            ok(rule)
        }
    });
    let sb = Sandbox::new();
    // R6 review: options left out keep the stored values (read first).
    let o = gstn(
        &sb,
        &srv,
        &[
            "merge-rule",
            "set",
            "--project",
            "7",
            "--rule-id",
            "2",
            "--approvals",
            "2",
            "--direct-push-forbidden",
            "false",
        ],
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(
        o.stdout.contains("warning: no check can be reported"),
        "{}",
        o.stdout
    );
    let seen = srv.seen();
    assert_eq!(
        (seen[0].method.as_str(), seen[0].target.as_str()),
        ("GET", "/api/v1/projects/7/protected-refs")
    );
    let r = seen.last().expect("a request");
    assert_eq!(
        (r.method.as_str(), r.target.as_str()),
        ("PUT", "/api/v1/projects/7/protected-refs/2/merge-rule")
    );
    assert_eq!(
        serde_json::from_str::<Value>(&r.body).expect("json"),
        json!({"required_approvals": 2, "required_checks": ["ci/gates"],
               "require_resolved_discussions": true, "direct_push_forbidden": false})
    );
    let o = gstn(
        &sb,
        &srv,
        &[
            "merge-rule",
            "set",
            "--project",
            "7",
            "--rule-id",
            "2",
            "--clear-checks",
        ],
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    let r = srv.seen().pop().expect("a request");
    assert_eq!(
        serde_json::from_str::<Value>(&r.body).expect("json")["required_checks"],
        json!([])
    );
    let refused = gstn(
        &sb,
        &srv,
        &["merge-rule", "remove", "--project", "7", "--rule-id", "2"],
    );
    assert_ne!(refused.code, 0, "--yes is required");
    let o = gstn(
        &sb,
        &srv,
        &[
            "merge-rule",
            "remove",
            "--project",
            "7",
            "--rule-id",
            "2",
            "--yes",
        ],
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    let r = srv.seen().pop().expect("a request");
    assert_eq!(
        (r.method.as_str(), r.target.as_str()),
        ("DELETE", "/api/v1/projects/7/protected-refs/2/merge-rule")
    );
}
