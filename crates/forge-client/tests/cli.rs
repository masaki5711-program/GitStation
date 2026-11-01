//! End-to-end tests of `gstn` against a scripted loopback server. Test names
//! that match `test = …` fields of FG-CLI-* in errors.toml are load-bearing.

mod common;
use common::{user_server, Mock, Resp, Sandbox, TOKEN, USER_JSON};
use serde_json::json;

fn err_body(code: &str, rid: &str) -> String {
    format!(r#"{{"error_code":"{code}","user_message":"msg","request_id":"{rid}"}}"#)
}

fn login(sb: &Sandbox, host: &str) {
    let o = sb.run(
        host,
        &["auth", "login", "--with-token"],
        &format!("{TOKEN}\n"),
    );
    assert_eq!(o.code, 0, "login failed: {}", o.stderr);
}

// ── tokens are never printed ───────────────────────────────────────────

#[test]
fn token_is_never_printed_by_any_command() {
    let srv = user_server();
    let sb = Sandbox::new();
    let mut all = String::new();
    for (args, stdin) in [
        (vec!["auth", "login", "--with-token"], format!("{TOKEN}\n")),
        (
            vec!["auth", "login", "--with-token", "--json"],
            format!("{TOKEN}\n"),
        ),
        (vec!["auth", "status"], String::new()),
        (vec!["auth", "status", "--json"], String::new()),
        (vec!["api", "GET", "/api/v1/user"], String::new()),
        (vec!["api", "GET", "/api/v1/nothing"], String::new()),
        (
            vec!["api", "GET", "/api/v1/user", "--dry-run"],
            String::new(),
        ),
        (vec!["help", "--json"], String::new()),
        (vec!["auth", "logout"], String::new()),
        (vec!["auth", "status", "--json"], String::new()),
    ] {
        let o = sb.run(&srv.url, &args, &stdin);
        all.push_str(&o.all());
    }
    assert!(!all.contains(TOKEN), "token leaked:\n{all}");
    assert!(!all.contains("SYNTHETIC_0123"), "token fragment leaked");
}

#[test]
fn token_is_never_printed_when_the_server_rejects_it() {
    let srv = user_server();
    let sb = Sandbox::new();
    let bad = "gst_WRONG_TOKEN_VALUE_123";
    let o = sb.run(&srv.url, &["auth", "login", "--with-token"], bad);
    assert_eq!(o.code, 3);
    assert!(!o.all().contains(bad));
    // And it was not stored.
    assert!(!sb.path().join("credentials.toml").exists());
}

#[test]
fn only_the_git_answer_contains_the_token_and_stderr_stays_empty() {
    let srv = user_server();
    let sb = Sandbox::new();
    login(&sb, &srv.url);
    let host = srv.url.trim_start_matches("http://");
    let o = sb.run(
        &srv.url,
        &["auth", "git-credential", "get"],
        &format!("protocol=http\nhost={host}\n\n"),
    );
    assert_eq!(o.code, 0);
    assert!(o.stdout.contains(&format!("password={TOKEN}\n")));
    assert!(o.stderr.is_empty(), "stderr: {}", o.stderr);
}

// ── login / status / logout ────────────────────────────────────────────

#[test]
fn login_verifies_then_stores_and_status_reports_the_user() {
    let srv = user_server();
    let sb = Sandbox::new();
    let o = sb.run(
        &srv.url,
        &["auth", "login", "--with-token", "--json"],
        &format!("  {TOKEN}  \n"),
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    let v = o.json();
    assert_eq!(v["username"], "alice");
    assert_eq!(v["stored_in"], "file");
    assert_eq!(v["host"], srv.url.as_str());

    let s = sb.run(&srv.url, &["auth", "status", "--json"], "");
    assert_eq!(s.code, 0, "{}", s.stderr);
    let v = s.json();
    assert_eq!(v["logged_in"], true);
    assert_eq!(v["user"]["username"], "alice");
    assert_eq!(v["user"]["is_agent"], false);
    assert_eq!(v["token_scopes"], json!(["read_api", "read_repository"]));
    assert_eq!(v["token_source"], "file");

    let human = sb.run(&srv.url, &["auth", "status"], "");
    assert!(human.stdout.contains("alice") && human.stdout.contains("read_api"));

    let out = sb.run(&srv.url, &["auth", "logout", "--json"], "");
    assert_eq!(out.json()["removed"], true);
    let again = sb.run(&srv.url, &["auth", "logout", "--json"], "");
    assert_eq!(again.code, 0);
    assert_eq!(again.json()["removed"], false);
}

#[test]
fn not_logged_in_exits_3() {
    let srv = user_server();
    let sb = Sandbox::new();
    let o = sb.run(&srv.url, &["auth", "status", "--json"], "");
    assert_eq!(o.code, 3);
    assert_eq!(o.err_json()["error_code"], "FG-CLI-0002");
    assert!(srv.seen().is_empty(), "no request without a token");
}

#[test]
fn gstn_token_env_is_used_and_reported_as_its_source() {
    let srv = user_server();
    let sb = Sandbox::new();
    let o = sb.run_env(
        &srv.url,
        &["auth", "status", "--json"],
        "",
        &[("GSTN_TOKEN", TOKEN)],
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(o.json()["token_source"], "env");
    assert!(!o.all().contains(TOKEN));
}

#[test]
fn login_requires_with_token_and_never_prompts() {
    let sb = Sandbox::new();
    let o = sb.run("https://git.example.internal", &["auth", "login"], "");
    assert_eq!(o.code, 2);
    assert!(o.stderr.contains("FG-CLI-0001"));
}

#[test]
fn credential_file_is_created_with_mode_0600() {
    let srv = user_server();
    let sb = Sandbox::new();
    login(&sb, &srv.url);
    let f = sb.path().join("credentials.toml");
    assert!(f.exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&f).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(sb.path()).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
}

#[cfg(unix)]
#[test]
fn a_credential_file_readable_by_others_is_refused() {
    use std::os::unix::fs::PermissionsExt;
    let srv = user_server();
    let sb = Sandbox::new();
    login(&sb, &srv.url);
    let f = sb.path().join("credentials.toml");
    std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
    let o = sb.run(&srv.url, &["auth", "status", "--json"], "");
    assert_eq!(o.code, 3, "must not trust a world-readable file");
    assert!(o.stderr.contains("insecure_permissions"));
    assert!(!o.all().contains(TOKEN));
}

#[cfg(unix)]
#[test]
fn credential_file_failure_is_credential_store_failed() {
    let srv = user_server();
    let sb = Sandbox::new();
    // The credentials path is a directory: it can be neither read nor replaced.
    std::fs::create_dir_all(sb.path().join("credentials.toml")).unwrap();
    let o = sb.run(
        &srv.url,
        &["auth", "login", "--with-token", "--json"],
        TOKEN,
    );
    assert_eq!(o.code, 1);
    assert_eq!(o.err_json()["error_code"], "FG-CLI-0006");
    assert!(!o.all().contains(TOKEN));
}

// ── git credential helper ──────────────────────────────────────────────

#[test]
fn git_credential_answers_only_for_the_configured_host() {
    let srv = user_server();
    let sb = Sandbox::new();
    login(&sb, &srv.url);
    let host = srv.url.trim_start_matches("http://").to_string();
    let get = |input: &str| sb.run(&srv.url, &["auth", "git-credential", "get"], input);

    let ok = get(&format!(
        "protocol=http\nhost={host}\npath=team/app.git\n\n"
    ));
    assert_eq!(ok.code, 0);
    assert!(ok.stdout.contains(&format!("host={host}\n")));
    assert!(ok.stdout.contains("username=gstn\n"));

    for other in [
        "protocol=https\nhost=github.com\n\n".to_string(),
        "protocol=http\nhost=evil.example\n\n".to_string(),
        format!("protocol=https\nhost={host}\n\n"),
        "protocol=http\nhost=127.0.0.1:1\n\n".to_string(),
        format!("protocol=http\nhost=x.{host}\n\n"),
        "protocol=http\n\n".to_string(),
        String::new(),
    ] {
        let o = get(&other);
        assert_eq!(o.code, 0, "other hosts are not an error");
        assert!(o.stdout.is_empty(), "answered for {other:?}: {}", o.stdout);
    }
    // The url= form is understood too.
    let u = get(&format!("url=http://{host}/team/app.git\n\n"));
    assert!(u.stdout.contains("password="));
}

#[test]
fn git_credential_store_is_a_noop_and_erase_removes_only_the_rejected_token() {
    let srv = user_server();
    let sb = Sandbox::new();
    login(&sb, &srv.url);
    let host = srv.url.trim_start_matches("http://").to_string();
    let run = |op: &str, input: String| sb.run(&srv.url, &["auth", "git-credential", op], &input);

    // store: nothing changes
    let s = run(
        "store",
        format!("protocol=http\nhost={host}\nusername=u\npassword=typed-by-mistake\n\n"),
    );
    assert_eq!((s.code, s.stdout.is_empty()), (0, true));
    // erase of a different password: kept
    run(
        "erase",
        format!("protocol=http\nhost={host}\npassword=something-else\n\n"),
    );
    assert!(run("get", format!("protocol=http\nhost={host}\n\n"))
        .stdout
        .contains("password="));
    // erase of another host: kept
    run(
        "erase",
        format!("protocol=https\nhost=github.com\npassword={TOKEN}\n\n"),
    );
    assert!(run("get", format!("protocol=http\nhost={host}\n\n"))
        .stdout
        .contains("password="));
    // erase of the rejected token: removed
    run(
        "erase",
        format!("protocol=http\nhost={host}\npassword={TOKEN}\n\n"),
    );
    assert!(run("get", format!("protocol=http\nhost={host}\n\n"))
        .stdout
        .is_empty());
}

#[test]
fn git_credential_requires_an_operation_or_install() {
    let sb = Sandbox::new();
    let o = sb.run(
        "https://git.example.internal",
        &["auth", "git-credential"],
        "",
    );
    assert_eq!(o.code, 2);
}

#[test]
fn git_credential_install_sets_a_host_scoped_helper() {
    let sb = Sandbox::new();
    let gitconfig = sb.dir.path().join("gitconfig");
    let gc = gitconfig.to_str().unwrap();
    let args = ["auth", "git-credential", "--install", "--json"];
    // Twice: the second run replaces, never duplicates.
    for _ in 0..2 {
        let o = sb.run_env(
            "https://git.example.internal",
            &args,
            "",
            &[("GIT_CONFIG_GLOBAL", gc)],
        );
        assert_eq!(o.code, 0, "{}", o.stderr);
        let v = o.json();
        assert_eq!(
            v["git_config_key"],
            "credential.https://git.example.internal.helper"
        );
        assert!(v["helper"].as_str().unwrap().starts_with(
            "!'/usr/local/bin/gstn' --host 'https://git.example.internal' auth git-credential"
        ));
    }
    let got = std::process::Command::new("git")
        .args([
            "config",
            "--file",
            gc,
            "--get-all",
            "credential.https://git.example.internal.helper",
        ])
        .output()
        .unwrap();
    let lines: Vec<String> = String::from_utf8_lossy(&got.stdout)
        .lines()
        .map(str::to_string)
        .collect();
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert_eq!(lines[0], "");
    assert!(lines[1].starts_with(
        "!'/usr/local/bin/gstn' --host 'https://git.example.internal' auth git-credential"
    ));
}

// ── api ────────────────────────────────────────────────────────────────

#[test]
fn api_sends_the_token_and_prints_the_body() {
    let srv = user_server();
    let sb = Sandbox::new();
    login(&sb, &srv.url);
    let o = sb.run(
        &srv.url,
        &["api", "GET", "/api/v1/user", "--fields", "username,id"],
        "",
    );
    assert_eq!(o.code, 0);
    assert_eq!(o.json(), json!({"username": "alice", "id": 7}));
    let last = srv.seen().pop().unwrap();
    assert_eq!(last.headers["authorization"], format!("Bearer {TOKEN}"));
    assert!(last.headers["user-agent"].starts_with("gstn/"));
}

#[test]
fn api_fields_become_query_for_get_and_json_body_for_post() {
    let srv = Mock::start(|_| Resp::json(200, r#"{"ok":true}"#));
    let sb = Sandbox::new();
    sb.run(
        &srv.url,
        &[
            "api",
            "GET",
            "/api/v1/projects",
            "-f",
            "limit=5",
            "-f",
            "q=a b&c",
        ],
        "",
    );
    sb.run(
        &srv.url,
        &[
            "api",
            "POST",
            "/api/v1/projects",
            "-f",
            "name=app",
            "-f",
            "parent_id:=12",
            "-f",
            "flag:=true",
        ],
        "",
    );
    let seen = srv.seen();
    assert_eq!(seen[0].target, "/api/v1/projects?limit=5&q=a+b%26c");
    let body: serde_json::Value = serde_json::from_str(&seen[1].body).unwrap();
    assert_eq!(body, json!({"name": "app", "parent_id": 12, "flag": true}));
    assert_eq!(seen[1].headers["content-type"], "application/json");
}

#[test]
fn api_input_file_and_stdin_are_the_body() {
    let srv = Mock::start(|_| Resp::json(200, "{}"));
    let sb = Sandbox::new();
    let f = sb.dir.path().join("b.json");
    std::fs::write(&f, r#"{"visibility":"internal"}"#).unwrap();
    sb.run(
        &srv.url,
        &[
            "api",
            "PUT",
            "/api/v1/projects/7/visibility",
            "--input",
            f.to_str().unwrap(),
        ],
        "",
    );
    sb.run(
        &srv.url,
        &["api", "PUT", "/x", "--input", "-"],
        r#"{"a":1}"#,
    );
    let seen = srv.seen();
    assert_eq!(seen[0].method, "PUT");
    assert_eq!(seen[0].body, r#"{"visibility":"internal"}"#);
    assert_eq!(seen[1].body, r#"{"a":1}"#);
}

#[test]
fn api_usage_errors() {
    let sb = Sandbox::new();
    let h = "https://git.example.internal";
    for args in [
        vec!["api", "FETCH", "/x"],
        vec!["api", "GET", "no-slash"],
        vec!["api", "GET", "//evil.example/x"],
        vec!["api", "GET", "https://evil.example/x"],
        vec!["api", "GET", "/a b"],
        vec!["api", "POST", "/x", "--paginate"],
        vec!["api", "POST", "/x", "-f", "novalue"],
        vec!["api", "POST", "/x", "-f", "k:=notjson{"],
        vec!["api", "POST", "/x", "-f", "a=b", "--input", "-"],
        vec!["api", "GET", "/x", "-f", "a:=[1]"],
        vec!["api", "GET", "/x", "--input", "f.json"],
    ] {
        let o = sb.run(h, &args, "{}");
        assert_eq!(
            (o.code, o.stderr.contains("FG-CLI-0001")),
            (2, true),
            "{args:?}: {}",
            o.stderr
        );
    }
}

#[test]
fn cli_usage_errors_exit_2() {
    let sb = Sandbox::new();
    for args in [
        vec![],
        vec!["nosuch"],
        vec!["auth"],
        vec!["auth", "login", "--bogus"],
        vec!["schema"],
    ] {
        let o = sb.run("https://git.example.internal", &args, "");
        assert_eq!(o.code, 2, "{args:?}");
        assert!(o.stderr.contains("FG-CLI-0001"), "{}", o.stderr);
    }
    let j = sb.run("https://git.example.internal", &["nosuch", "--json"], "");
    assert_eq!(j.err_json()["error_code"], "FG-CLI-0001");
    let unknown = sb.run("https://git.example.internal", &["schema", "nope"], "");
    assert_eq!(unknown.code, 2);
}

#[test]
fn api_never_leaves_the_configured_host_and_never_follows_redirects() {
    let other = Mock::start(|_| Resp::json(200, "{}"));
    let loc = format!("{}/steal", other.url);
    let srv = Mock::start(move |_| Resp {
        status: 302,
        headers: vec![("Location".into(), loc.clone())],
        body: String::new(),
    });
    let sb = Sandbox::new();
    let o = sb.run_env(
        &srv.url,
        &["api", "GET", "/api/v1/user"],
        "",
        &[("GSTN_TOKEN", TOKEN)],
    );
    assert_eq!(o.code, 1);
    assert!(o.stderr.contains("FG-CLI-0004") && o.stderr.contains("unexpected_redirect"));
    assert!(
        other.seen().is_empty(),
        "the redirect target must never be contacted"
    );
    assert!(!o.all().contains(TOKEN));
}

#[test]
fn api_dry_run_sends_nothing_and_shows_no_token() {
    let srv = user_server();
    let sb = Sandbox::new();
    let o = sb.run_env(
        &srv.url,
        &[
            "api",
            "POST",
            "/api/v1/projects",
            "-f",
            "name=x",
            "--dry-run",
        ],
        "",
        &[("GSTN_TOKEN", TOKEN)],
    );
    assert_eq!(o.code, 0);
    let v = o.json();
    assert_eq!(v["method"], "POST");
    assert_eq!(v["body"], json!({"name": "x"}));
    assert!(srv.seen().is_empty());
    assert!(!o.all().contains(TOKEN));
}

#[test]
fn api_paginate_follows_cursors_and_merges_items() {
    let srv = Mock::start(|r| {
        let q = r
            .target
            .split_once("cursor=")
            .map(|(_, c)| c.split('&').next().unwrap_or("").to_string());
        match q.as_deref() {
            None => Resp::json(
                200,
                r#"{"items":[{"id":1},{"id":2}],"next_cursor":"c1","has_more":true}"#,
            ),
            Some("c1") => Resp::json(
                200,
                r#"{"items":[{"id":3}],"next_cursor":"c2","has_more":true}"#,
            ),
            Some("c2") => Resp::json(
                200,
                r#"{"items":[{"id":4}],"next_cursor":null,"has_more":false}"#,
            ),
            Some(_) => Resp::json(400, &err_body("FG-WEB-0006", "r")),
        }
    });
    let sb = Sandbox::new();
    let o = sb.run(
        &srv.url,
        &[
            "api",
            "GET",
            "/api/v1/projects",
            "--paginate",
            "-f",
            "limit=2",
        ],
        "",
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(
        o.json(),
        json!({"items":[{"id":1},{"id":2},{"id":3},{"id":4}],"next_cursor":null,"has_more":false})
    );
    let t: Vec<String> = srv.seen().into_iter().map(|r| r.target).collect();
    assert_eq!(
        t,
        [
            "/api/v1/projects?limit=2",
            "/api/v1/projects?limit=2&cursor=c1",
            "/api/v1/projects?limit=2&cursor=c2"
        ]
    );
    // --fields applies to each item.
    let f = sb.run(
        &srv.url,
        &[
            "api",
            "GET",
            "/api/v1/projects",
            "--paginate",
            "--fields",
            "id",
        ],
        "",
    );
    assert_eq!(f.json()["items"][3], json!({"id": 4}));
}

#[test]
fn api_paginate_stops_on_a_repeated_cursor() {
    let srv = Mock::start(|_| {
        Resp::json(
            200,
            r#"{"items":[{"id":1}],"next_cursor":"same","has_more":true}"#,
        )
    });
    let sb = Sandbox::new();
    let o = sb.run(
        &srv.url,
        &["api", "GET", "/api/v1/projects", "--paginate"],
        "",
    );
    assert_eq!(o.code, 1);
    assert!(o.stderr.contains("cursor_repeated"));
    assert_eq!(srv.seen().len(), 2);
}

// ── exit codes and error shapes ────────────────────────────────────────

#[test]
fn server_errors_map_to_stable_exit_codes() {
    let cases = [
        (401, "FG-AUTH-0001", 3),
        (403, "FG-AUTH-0003", 3),
        (404, "FG-REPO-0005", 4),
        (409, "FG-REPO-0010", 5),
        (400, "FG-WEB-0006", 2),
        (503, "FG-WEB-0008", 6),
        (503, "FG-DB-0001", 6),
        (500, "FG-SYS-0001", 1),
        // a code this client does not know (newer server): the status decides
        (404, "FG-ZZZ-9999", 4),
        (429, "FG-ZZZ-9998", 6),
        (500, "FG-ZZZ-9997", 1),
    ];
    for (status, code, exit) in cases {
        let body = err_body(code, "req-77");
        let srv = Mock::start(move |_| Resp::json(status, &body));
        let sb = Sandbox::new();
        let o = sb.run(&srv.url, &["api", "GET", "/api/v1/x", "--json"], "");
        assert_eq!(o.code, exit, "{status} {code}");
        let e = o.err_json();
        assert_eq!(e["error_code"], code);
        assert_eq!(e["request_id"], "req-77");
        assert_eq!(e["retryable"], exit == 6);
        assert_eq!(o.stdout, "");
    }
}

#[test]
fn json_error_shape_is_stable() {
    let srv = Mock::start(|_| Resp::json(404, &err_body("FG-REPO-0005", "r1")));
    let sb = Sandbox::new();
    let o = sb.run(&srv.url, &["api", "GET", "/x", "--json"], "");
    let keys: Vec<String> = o.err_json().as_object().unwrap().keys().cloned().collect();
    assert_eq!(
        keys,
        [
            "detail",
            "error_code",
            "request_id",
            "retryable",
            "user_message"
        ]
    );
    assert_eq!(o.stderr.trim().lines().count(), 1, "one JSON line");
    // human form: message, code and request id, no JSON
    let h = sb.run(&srv.url, &["api", "GET", "/x"], "");
    assert!(
        h.stderr
            .starts_with("error: msg [FG-REPO-0005 request_id=r1]"),
        "{}",
        h.stderr
    );
}

#[test]
fn output_shapes_are_stable() {
    let srv = user_server();
    let sb = Sandbox::new();
    let keys =
        |v: &serde_json::Value| -> Vec<String> { v.as_object().unwrap().keys().cloned().collect() };
    let l = sb
        .run(
            &srv.url,
            &["auth", "login", "--with-token", "--json"],
            TOKEN,
        )
        .json();
    assert_eq!(
        keys(&l),
        ["host", "is_agent", "stored_in", "token_scopes", "username"]
    );
    let s = sb.run(&srv.url, &["auth", "status", "--json"], "").json();
    assert_eq!(
        keys(&s),
        ["host", "logged_in", "token_scopes", "token_source", "user"]
    );
    assert_eq!(
        keys(&s["user"]),
        ["display_name", "id", "is_admin", "is_agent", "username"]
    );
    let o = sb.run(&srv.url, &["auth", "logout", "--json"], "").json();
    assert_eq!(keys(&o), ["host", "removed"]);
}

#[test]
fn unreachable_server_exits_6_and_is_retryable() {
    let sb = Sandbox::new();
    // Nothing listens on port 1.
    let o = sb.run(
        "http://127.0.0.1:1",
        &["api", "GET", "/healthz", "--json"],
        "",
    );
    assert_eq!(o.code, 6);
    let e = o.err_json();
    assert_eq!(
        (e["error_code"].as_str(), e["retryable"].as_bool()),
        (Some("FG-CLI-0003"), Some(true))
    );
}

#[test]
fn non_api_response_is_response_invalid() {
    let srv = Mock::start(|_| Resp {
        status: 200,
        headers: vec![],
        body: "<html>login</html>".into(),
    });
    let sb = Sandbox::new();
    let o = sb.run(&srv.url, &["api", "GET", "/api/v1/user", "--json"], "");
    assert_eq!(o.code, 1);
    assert_eq!(o.err_json()["error_code"], "FG-CLI-0004");
    let srv2 = Mock::start(|_| Resp::json(500, r#"{"nope":1}"#));
    let o2 = sb.run(&srv2.url, &["api", "GET", "/x", "--json"], "");
    assert_eq!(o2.err_json()["error_code"], "FG-CLI-0004");
}

#[test]
fn config_without_host_is_config_invalid() {
    let sb = Sandbox::new();
    let o = sb.run("", &["auth", "status", "--json"], "");
    assert_eq!(o.code, 2);
    assert_eq!(o.err_json()["error_code"], "FG-CLI-0005");
    // a plain-http, non-loopback host would send tokens in the clear
    let h = sb.run(
        "http://git.example.internal",
        &["auth", "status", "--json"],
        "",
    );
    assert_eq!(h.code, 2);
    assert!(h.stderr.contains("insecure_scheme"));
}

#[test]
fn config_file_supplies_the_host_and_rejects_unknown_keys() {
    let srv = user_server();
    let sb = Sandbox::new();
    std::fs::create_dir_all(sb.path()).unwrap();
    std::fs::write(
        sb.path().join("config.toml"),
        format!(
            "host = \"{}\"\ntimeout_secs = 5\n[update]\nchannel = \"stable\"\n",
            srv.url
        ),
    )
    .unwrap();
    login(&sb, "");
    let ok = sb.run("", &["auth", "status", "--json"], "");
    assert_eq!(ok.code, 0, "{}", ok.stderr);
    std::fs::write(sb.path().join("config.toml"), "hots = \"x\"\n").unwrap();
    let bad = sb.run("", &["auth", "status", "--json"], "");
    assert_eq!(bad.code, 2);
    assert!(bad.stderr.contains("reason=parse") && bad.stderr.contains("line=1"));
}

#[test]
fn a_missing_extra_ca_file_is_config_invalid() {
    let srv = user_server();
    let sb = Sandbox::new();
    let o = sb.run_env(
        &srv.url,
        &["api", "GET", "/healthz"],
        "",
        &[("GSTN_CA_FILE", "/nonexistent/ca.pem")],
    );
    assert_eq!(o.code, 2);
    assert!(o.stderr.contains("ca_file"));
}

#[test]
fn closed_stdout_is_output_failed() {
    use forge_client::app::{run, Io};
    struct Closed;
    impl std::io::Write for Closed {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut input = std::io::Cursor::new(Vec::new());
    let mut err = Vec::new();
    let mut out = Closed;
    let env = |_: &str| None;
    let args: Vec<String> = ["gstn", "help", "--json"]
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    let code = {
        let mut io = Io {
            stdin: &mut input,
            stdout: &mut out,
            stderr: &mut err,
            stdin_tty: false,
            stdout_tty: false,
        };
        run(&args, &env, "gstn", &mut io)
    };
    assert_eq!(code, 1);
    assert!(String::from_utf8_lossy(&err).contains("FG-CLI-0007"));
}

// ── self-description ───────────────────────────────────────────────────

#[test]
fn help_json_matches_the_committed_snapshot() {
    let sb = Sandbox::new();
    let o = sb.run("", &["help", "--json"], "");
    assert_eq!(o.code, 0);
    let mut v = o.json();
    v["gstn_version"] = json!("<version>");
    let text = serde_json::to_string_pretty(&v).unwrap() + "\n";
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/snapshots/help.json");
    if std::env::var("GSTN_UPDATE_SNAPSHOT").is_ok() {
        std::fs::write(path, &text).unwrap();
    }
    let want = std::fs::read_to_string(path)
        .expect("snapshot (run with GSTN_UPDATE_SNAPSHOT=1 to create)");
    assert_eq!(text, want, "help --json changed: review the diff, then GSTN_UPDATE_SNAPSHOT=1 cargo nextest run -p forge-client");
}

#[test]
fn help_json_lists_every_command_with_args_examples_and_scopes() {
    let sb = Sandbox::new();
    let v = sb.run("", &["help", "--json"], "").json();
    let names: Vec<&str> = v["commands"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "auth login",
            "auth status",
            "auth logout",
            "auth git-credential",
            "project list",
            "project view",
            "project create",
            "project edit",
            "project visibility",
            "project move",
            "project delete",
            "project restore",
            "group list",
            "group view",
            "group create",
            "group edit",
            "group move",
            "member list",
            "member add",
            "member set-role",
            "member remove",
            "protected-ref list",
            "protected-ref add",
            "protected-ref edit",
            "protected-ref remove",
            "namespace resolve",
            "namespace release-path",
            "browse refs",
            "browse resolve",
            "browse tree",
            "browse file",
            "user rename",
            "api",
            "help",
            "schema"
        ]
    );
    for c in v["commands"].as_array().unwrap() {
        assert!(!c["examples"].as_array().unwrap().is_empty(), "{c}");
        assert!(c["scopes"].is_array() && c["routes"].is_array() && c["output"].is_object());
    }
    assert_eq!(v["exit_codes"].as_array().unwrap().len(), 7);
    let one = sb.run("", &["help", "--json", "auth", "status"], "").json();
    assert_eq!(one["commands"].as_array().unwrap().len(), 1);
}

#[test]
fn schema_describes_one_command() {
    let sb = Sandbox::new();
    let s = sb.run("", &["schema", "auth", "status"], "");
    assert_eq!(s.code, 0);
    let v = s.json();
    assert_eq!(v["command"], serde_json::Value::Null);
    assert_eq!(v["name"], "auth status");
    assert_eq!(v["scopes"], json!(["read_api"]));
    assert!(v["output"]["properties"]["token_source"].is_object());
    assert_eq!(v["exit_codes"].as_array().unwrap().len(), 7);
    let q = sb.run("", &["schema", "auth status"], "");
    assert_eq!(q.code, 0, "a quoted path works too");
}

#[test]
fn global_args_in_help_exist_in_the_binary() {
    let sb = Sandbox::new();
    let o = sb.run("", &["--version"], "");
    assert_eq!(o.code, 0);
    assert!(o.stdout.starts_with("gstn "));
    let _ = USER_JSON;
}

#[test]
fn hostile_hosts_never_reach_the_git_helper_line() {
    let sb = Sandbox::new();
    let gc = sb.dir.path().join("gitconfig");
    for host in [
        "https://a.example;touch${IFS}x",
        "https://a$(id).example",
        "https://a`id`.example",
        "https://a&b.example",
        "https://a(b).example",
        "https://a'b.example",
        "https://a|b.example",
    ] {
        let o = sb.run_env(
            host,
            &["auth", "git-credential", "--install"],
            "",
            &[("GIT_CONFIG_GLOBAL", gc.to_str().unwrap())],
        );
        assert_eq!(o.code, 2, "{host}: {}", o.stderr);
        assert!(o.stderr.contains("FG-CLI-0005"));
    }
    assert!(!gc.exists(), "nothing was written");
}

#[test]
fn api_input_over_the_cap_is_refused() {
    let sb = Sandbox::new();
    let f = sb.dir.path().join("big.json");
    std::fs::write(&f, format!("\"{}\"", "a".repeat(16 * 1024 * 1024))).unwrap();
    let o = sb.run(
        "https://git.example.internal",
        &["api", "POST", "/x", "--input", f.to_str().unwrap()],
        "",
    );
    assert_eq!(o.code, 2);
    assert!(o.stderr.contains("input_too_large"));
}

#[test]
fn control_characters_in_server_messages_are_not_printed() {
    let body = r#"{"error_code":"FG-REPO-0005","user_message":"a\u001b[31mred\nfake: line\u202e","request_id":"r\u0007"}"#;
    let srv = Mock::start(move |_| Resp::json(404, body));
    let sb = Sandbox::new();
    let o = sb.run(&srv.url, &["api", "GET", "/x"], "");
    assert!(
        !o.stderr.chars().any(|c| c.is_control() && c != '\n'),
        "{:?}",
        o.stderr
    );
    assert_eq!(o.stderr.trim_end().lines().count(), 1, "{:?}", o.stderr);
    assert!(!o.stderr.contains('\u{202e}'));
}

#[test]
fn paginate_stops_at_item_and_byte_limits() {
    use forge_client::config::{Config, Overrides};
    use forge_client::http::{ApiClient, PageLimits};
    let n = std::sync::atomic::AtomicUsize::new(0);
    let srv = Mock::start(move |_| {
        let i = n.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Resp::json(
            200,
            &format!(
                r#"{{"items":[{{"id":1}},{{"id":2}},{{"id":3}}],"next_cursor":"c{i}","has_more":true}}"#
            ),
        )
    });
    let sb = Sandbox::new();
    let url = srv.url.clone();
    let env = move |k: &str| (k == "GSTN_HOST").then(|| url.clone());
    let cfg = Config::load(
        &env,
        &Overrides {
            host: None,
            config_dir: Some(sb.path()),
        },
    )
    .unwrap();
    let c = ApiClient::new(&cfg, None).unwrap();
    let base = PageLimits {
        max_pages: 100,
        max_items: 7,
        max_bytes: 1 << 20,
    };
    let e = c.get_all_pages_limited("/l", &[], base).unwrap_err();
    assert!(e.human().contains("paginate_items_limit"), "{}", e.human());
    let e = c
        .get_all_pages_limited(
            "/l",
            &[],
            PageLimits {
                max_items: 1000,
                max_bytes: 100,
                ..base
            },
        )
        .unwrap_err();
    assert!(e.human().contains("paginate_bytes_limit"), "{}", e.human());
    let e = c
        .get_all_pages_limited(
            "/l",
            &[],
            PageLimits {
                max_items: 1000,
                max_pages: 2,
                ..base
            },
        )
        .unwrap_err();
    assert!(e.human().contains("too_many_pages"));
}

#[cfg(unix)]
#[test]
fn a_group_writable_config_directory_is_refused_for_the_file_store() {
    use std::os::unix::fs::PermissionsExt;
    let srv = user_server();
    let sb = Sandbox::new();
    std::fs::create_dir_all(sb.path()).unwrap();
    std::fs::set_permissions(sb.path(), std::fs::Permissions::from_mode(0o775)).unwrap();
    let o = sb.run(&srv.url, &["auth", "login", "--with-token"], TOKEN);
    assert_eq!(o.code, 1);
    assert!(o.stderr.contains("FG-CLI-0006") && o.stderr.contains("insecure_directory"));
}

#[cfg(unix)]
#[test]
fn stale_temp_files_are_removed_on_write() {
    let srv = user_server();
    let sb = Sandbox::new();
    std::fs::create_dir_all(sb.path()).unwrap();
    let stale = sb.path().join(".credentials.deadbeef.tmp");
    std::fs::write(&stale, "x").unwrap();
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
    std::fs::File::options()
        .write(true)
        .open(&stale)
        .unwrap()
        .set_modified(old)
        .unwrap();
    let fresh = sb.path().join(".credentials.cafe.tmp");
    std::fs::write(&fresh, "x").unwrap();
    login(&sb, &srv.url);
    assert!(!stale.exists());
    assert!(
        fresh.exists(),
        "a recent temp file may belong to a running gstn"
    );
}
