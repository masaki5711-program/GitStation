//! End-to-end tests of `gstn browse` (refs, resolve, tree, file, commits, commit, compare, blame; M2 MR7 / MR8)
//! against a scripted loopback server. All data is synthetic.

#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly

mod common;
use common::{Mock, Out, Resp, Sandbox, TOKEN};
use serde_json::json;

fn gstn(sb: &Sandbox, srv: &Mock, args: &[&str]) -> Out {
    sb.run_env(&srv.url, args, "", &[("GSTN_TOKEN", TOKEN)])
}

fn ok(body: &str) -> Resp {
    Resp::json(200, body)
}

const C: &str = "0123456789abcdef0123456789abcdef01234567";

#[test]
fn browse_refs_lists_and_filters_by_type() {
    let srv = Mock::start(|_| {
        ok(&format!(
            r#"{{"default_branch":"main","refs":[{{"name":"main","full_name":"refs/heads/main","type":"branch","commit":"{C}","target":"{C}"}}],"truncated":false}}"#
        ))
    });
    let sb = Sandbox::new();
    let o = gstn(&sb, &srv, &["browse", "refs", "--project", "7", "--json"]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    let r = srv.seen().pop().expect("request");
    assert_eq!(r.method, "GET");
    assert_eq!(r.target, "/api/v1/projects/7/repository/refs");
    assert_eq!(r.headers["authorization"], format!("Bearer {TOKEN}"));
    assert_eq!(o.json()["default_branch"], "main");

    let h = gstn(
        &sb,
        &srv,
        &["browse", "refs", "--project", "7", "--type", "tag"],
    );
    assert_eq!(h.code, 0, "{}", h.stderr);
    assert_eq!(
        srv.seen().pop().expect("request").target,
        "/api/v1/projects/7/repository/refs?type=tag"
    );
    assert!(h.stdout.contains("default branch: main") && h.stdout.contains("branch  main"));
    assert_eq!(
        gstn(
            &sb,
            &srv,
            &["browse", "refs", "--project", "7", "--type", "note"]
        )
        .code,
        2
    );
}

#[test]
fn browse_resolve_tree_and_file_send_values_as_query_values() {
    let srv = Mock::start(|r| {
        if r.target.contains("/resolve") {
            ok(&format!(
                r#"{{"ref":{{"type":"branch","name":"feature/x","full_name":"refs/heads/feature/x","commit":"{C}"}},"path":"src/lib.rs"}}"#
            ))
        } else if r.target.contains("/tree") {
            ok(&format!(
                r#"{{"ref":{{"type":"tag","name":"v1","full_name":"refs/tags/v1","commit":"{C}"}},"path":"src","oid":"{C}","entries":[{{"name":"lib.rs","path":"src/lib.rs","type":"blob","mode":"100644","oid":"{C}","size":14}}],"truncated":true}}"#
            ))
        } else {
            ok(&format!(
                r#"{{"ref":{{"type":"commit","name":"{C}","full_name":null,"commit":"{C}"}},"path":"a.md","name":"a.md","type":"blob","mode":"100644","oid":"{C}","size":9,"binary":false,"content":"line 1\n\u001b[31mred\n","truncated":false}}"#
            ))
        }
    });
    let sb = Sandbox::new();
    let o = gstn(
        &sb,
        &srv,
        &[
            "browse",
            "resolve",
            "--project",
            "7",
            "feature/x/src/lib.rs",
        ],
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(
        srv.seen().pop().expect("request").target,
        "/api/v1/projects/7/repository/resolve?spec=feature%2Fx%2Fsrc%2Flib.rs"
    );
    assert!(o.stdout.starts_with("branch feature/x "), "{}", o.stdout);
    assert!(o.stdout.contains("path: src/lib.rs"));

    let t = gstn(
        &sb,
        &srv,
        &[
            "browse",
            "tree",
            "--project",
            "7",
            "--ref",
            "v1",
            "--path",
            "src",
        ],
    );
    assert_eq!(t.code, 0, "{}", t.stderr);
    assert_eq!(
        srv.seen().pop().expect("request").target,
        "/api/v1/projects/7/repository/tree?ref=v1&path=src"
    );
    assert!(t.stdout.contains("blob  14  src/lib.rs") && t.stdout.contains("truncated"));

    // An option-like ref and hostile characters stay inside one value.
    let x = gstn(
        &sb,
        &srv,
        &["browse", "tree", "--project", "7", "--ref=--output=x&y#z"],
    );
    assert_eq!(x.code, 0, "{}", x.stderr);
    let target = srv.seen().pop().expect("request").target;
    assert_eq!(target.matches('&').count(), 0, "{target}");
    assert!(target.starts_with("/api/v1/projects/7/repository/tree?ref=--output%3Dx"));

    let f = gstn(
        &sb,
        &srv,
        &[
            "browse",
            "file",
            "--project",
            "7",
            "--ref",
            C,
            "--path",
            "a.md",
        ],
    );
    assert_eq!(f.code, 0, "{}", f.stderr);
    assert_eq!(
        srv.seen().pop().expect("request").target,
        format!("/api/v1/projects/7/repository/blob?ref={C}&path=a.md")
    );
    // The file's lines are kept; its escape sequence is not.
    assert!(f.stdout.contains("line 1\n?[31mred"), "{:?}", f.stdout);
    let j = gstn(
        &sb,
        &srv,
        &[
            "browse",
            "file",
            "--project",
            "7",
            "--ref",
            "main",
            "--path",
            "a.md",
            "--json",
        ],
    );
    assert_eq!(j.json()["content"], "line 1\n\u{1b}[31mred\n");
}

#[test]
fn a_binary_file_says_so() {
    let srv = Mock::start(|_| {
        ok(&format!(
            r#"{{"ref":{{"type":"branch","name":"main","full_name":"refs/heads/main","commit":"{C}"}},"path":"x.bin","name":"x.bin","type":"blob","mode":"100644","oid":"{C}","size":4096,"binary":true,"content":null,"truncated":true}}"#
        ))
    });
    let sb = Sandbox::new();
    let o = gstn(
        &sb,
        &srv,
        &[
            "browse",
            "file",
            "--project",
            "7",
            "--ref",
            "main",
            "--path",
            "x.bin",
        ],
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stdout.contains("binary file, 4096 bytes"), "{}", o.stdout);
}

#[test]
fn browse_validates_before_any_request_and_shows_server_errors() {
    let srv = Mock::start(|_| {
        Resp::json(
            404,
            r#"{"error_code":"FG-REPO-0023","user_message":"見つかりません。","request_id":"req-9","path":"/srv/secret"}"#,
        )
    });
    let sb = Sandbox::new();
    for args in [
        vec!["browse", "refs", "--project", "0"],
        vec!["browse", "refs"],
        vec!["browse", "tree", "--project", "7"],
        vec!["browse", "file", "--project", "7", "--ref", "main"],
        vec!["browse", "resolve", "--project", "7", ""],
        vec![
            "browse",
            "file",
            "--project",
            "7",
            "--ref",
            "a\nb",
            "--path",
            "x",
        ],
    ] {
        assert_eq!(gstn(&sb, &srv, &args).code, 2, "{args:?}");
    }
    assert!(srv.seen().is_empty(), "no request for invalid input");
    let o = gstn(
        &sb,
        &srv,
        &[
            "browse",
            "file",
            "--project",
            "7",
            "--ref",
            "main",
            "--path",
            "nope",
        ],
    );
    assert_eq!(o.code, 4);
    assert!(o.stderr.contains("FG-REPO-0023") && o.stderr.contains("request_id=req-9"));
    assert!(!o.all().contains("/srv/secret"));
}

#[test]
fn browse_commands_are_described_for_machines() {
    let sb = Sandbox::new();
    let v = sb.run("", &["help", "--json"], "").json();
    for name in [
        "browse refs",
        "browse resolve",
        "browse tree",
        "browse file",
        "browse commits",
        "browse commit",
        "browse compare",
        "browse blame",
    ] {
        let c = v["commands"]
            .as_array()
            .expect("commands")
            .iter()
            .find(|c| c["name"] == name)
            .cloned()
            .unwrap_or_else(|| panic!("{name} missing"));
        assert_eq!(c["scopes"], json!(["read_api"]), "{name}");
        assert_eq!(c["routes"].as_array().expect("routes").len(), 1, "{name}");
    }
}

const C2: &str = "fedcba9876543210fedcba9876543210fedcba98";

fn commit_json(oid: &str, subject: &str) -> String {
    format!(
        r#"{{"oid":"{oid}","parents":["{C}"],"author_name":"Ana","author_email":"ana@example.invalid","authored_at":"2026-11-01T10:00:00+09:00","committer_name":"Ana","committer_email":"ana@example.invalid","committed_at":"2026-11-01T10:00:00+09:00","subject":"{subject}"}}"#
    )
}

#[test]
fn browse_commits_pages_with_a_cursor() {
    let srv = Mock::start(|_| {
        ok(&format!(
            r#"{{"ref":{{"type":"branch","name":"main","full_name":"refs/heads/main","commit":"{C2}"}},"head":"{C2}","path":"","commits":[{}],"next_cursor":"v1.next","has_more":true}}"#,
            commit_json(C2, "fix the thing")
        ))
    });
    let sb = Sandbox::new();
    let o = gstn(
        &sb,
        &srv,
        &[
            "browse",
            "commits",
            "--project",
            "7",
            "--ref",
            "main",
            "--path",
            "src/a b.rs",
            "--limit",
            "5",
            "--cursor",
            "v1.abc",
        ],
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(
        srv.seen().pop().expect("request").target,
        "/api/v1/projects/7/repository/commits?ref=main&path=src%2Fa+b.rs&limit=5&cursor=v1.abc"
    );
    assert!(
        o.stdout.contains("fix the thing") && o.stdout.contains("more: --cursor v1.next"),
        "{}",
        o.stdout
    );
    let j = gstn(
        &sb,
        &srv,
        &[
            "browse",
            "commits",
            "--project",
            "7",
            "--ref",
            "main",
            "--json",
        ],
    );
    assert_eq!(j.json()["next_cursor"], "v1.next");
    // The limit is checked before any request.
    let before = srv.seen().len();
    for limit in ["0", "101", "x"] {
        let o = gstn(
            &sb,
            &srv,
            &[
                "browse",
                "commits",
                "--project",
                "7",
                "--ref",
                "main",
                "--limit",
                limit,
            ],
        );
        assert_eq!(o.code, 2, "{limit}");
    }
    assert_eq!(srv.seen().len(), before);
}

#[test]
fn browse_commit_puts_only_a_full_commit_id_in_the_path() {
    let diff = r#"{"files":[{"status":"modified","old_path":"a.rs","new_path":"a.rs","old_mode":"100644","new_mode":"100644","old_oid":"1","new_oid":"2","similarity":null,"binary":false,"patch":"@@ -1 +1 @@\n-x\n+\u001b[31my\n","omitted":null},{"status":"renamed","old_path":"o.txt","new_path":"n.txt","old_mode":"100644","new_mode":"100644","old_oid":"1","new_oid":"1","similarity":100,"binary":false,"patch":"","omitted":null},{"status":"modified","old_path":"b.bin","new_path":"b.bin","old_mode":"100644","new_mode":"100644","old_oid":"1","new_oid":"2","similarity":null,"binary":true,"patch":null,"omitted":null},{"status":"added","old_path":null,"new_path":"big.txt","old_mode":"000000","new_mode":"100644","old_oid":"0","new_oid":"2","similarity":null,"binary":false,"patch":null,"omitted":"too_large"}],"truncated":true}"#;
    let srv = Mock::start(move |_| {
        ok(&format!(
            r#"{{"commit":{},"message":"fix the thing\n\nbody\u001b[2J","diff":{diff}}}"#,
            commit_json(C2, "fix the thing")
        ))
    });
    let sb = Sandbox::new();
    // An upper-case id is lower-cased; the path holds nothing else.
    let o = gstn(
        &sb,
        &srv,
        &["browse", "commit", "--project", "7", &C2.to_uppercase()],
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(
        srv.seen().pop().expect("request").target,
        format!("/api/v1/projects/7/repository/commits/{C2}")
    );
    assert!(o.stdout.contains("commit fedcba98"), "{}", o.stdout);
    assert!(o.stdout.contains("renamed  o.txt -> n.txt"), "{}", o.stdout);
    assert!(o.stdout.contains("b.bin  (binary)"), "{}", o.stdout);
    assert!(
        o.stdout.contains("(patch left out: too_large)") && o.stdout.contains("(truncated"),
        "{}",
        o.stdout
    );
    assert!(
        !o.stdout.contains("@@ -1 +1 @@"),
        "patches only with --patch"
    );
    assert!(!o.stdout.contains('\u{1b}'));

    let p = gstn(
        &sb,
        &srv,
        &["browse", "commit", "--project", "7", C2, "--patch"],
    );
    assert!(
        p.stdout.contains("@@ -1 +1 @@\n-x\n+?[31my"),
        "{:?}",
        p.stdout
    );
    assert!(!p.stdout.contains('\u{1b}'));
    let j = gstn(
        &sb,
        &srv,
        &["browse", "commit", "--project", "7", C2, "--json"],
    );
    assert_eq!(
        j.json()["diff"]["files"][0]["patch"],
        "@@ -1 +1 @@\n-x\n+\u{1b}[31my\n"
    );

    // Anything but a full id is refused before any request: it would be
    // part of the path.
    let before = srv.seen().len();
    for bad in [
        "main",
        "HEAD",
        &C2[..39],
        "../x",
        "a/b",
        "--patch",
        &format!("{C2}/x"),
    ] {
        let o = gstn(&sb, &srv, &["browse", "commit", "--project", "7", bad]);
        assert_eq!(o.code, 2, "{bad}");
    }
    assert_eq!(srv.seen().len(), before, "no request for invalid input");
}

#[test]
fn browse_compare_and_blame_send_values_as_query_values() {
    let srv = Mock::start(|r| {
        if r.target.contains("/compare") {
            ok(&format!(
                r#"{{"from":{{"type":"tag","name":"v1","full_name":"refs/tags/v1","commit":"{C}"}},"to":{{"type":"branch","name":"main","full_name":"refs/heads/main","commit":"{C2}"}},"commits":[{}],"commits_truncated":true,"diff":{{"files":[{{"status":"deleted","old_path":"gone.txt","new_path":null,"old_mode":"100644","new_mode":"000000","old_oid":"1","new_oid":"0","similarity":null,"binary":false,"patch":"@@ -1 +0,0 @@\n-x\n","omitted":null}}],"truncated":false}}}}"#,
                commit_json(C2, "later")
            ))
        } else {
            ok(&format!(
                r#"{{"ref":{{"type":"branch","name":"main","full_name":"refs/heads/main","commit":"{C2}"}},"path":"a.txt","oid":"{C}","size":6,"binary":false,"hunks":[{{"commit":"{C}","start":1,"orig_start":1,"lines":["one","tw\u001b[0mo"]}},{{"commit":"{C2}","start":3,"orig_start":3,"lines":["three"]}}],"commits":[{{"oid":"{C}","author_name":"Ana","author_email":"a@example.invalid","author_time":1,"author_tz":"+0900","summary":"first","boundary":true,"path":"a.txt"}},{{"oid":"{C2}","author_name":"Bo","author_email":"b@example.invalid","author_time":2,"author_tz":"+0900","summary":"second","boundary":false,"path":"a.txt"}}],"truncated":true}}"#
            ))
        }
    });
    let sb = Sandbox::new();
    let o = gstn(
        &sb,
        &srv,
        &[
            "browse",
            "compare",
            "--project",
            "7",
            "--from",
            "v1",
            "--to=--output=x&y",
        ],
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    let target = srv.seen().pop().expect("request").target;
    assert!(
        target.starts_with("/api/v1/projects/7/repository/compare?from=v1&to=--output%3Dx%26y"),
        "{target}"
    );
    assert!(o.stdout.contains("commits (1):") && o.stdout.contains("later"));
    assert!(o.stdout.contains("deleted  gone.txt"), "{}", o.stdout);
    assert!(o.stdout.contains("at most 200 commits"), "{}", o.stdout);

    let b = gstn(
        &sb,
        &srv,
        &[
            "browse",
            "blame",
            "--project",
            "7",
            "--ref",
            "main",
            "--path",
            "a.txt",
        ],
    );
    assert_eq!(b.code, 0, "{}", b.stderr);
    assert_eq!(
        srv.seen().pop().expect("request").target,
        "/api/v1/projects/7/repository/blame?ref=main&path=a.txt"
    );
    let lines: Vec<&str> = b.stdout.lines().collect();
    assert_eq!(lines[0], "    1  01234567  Ana  one", "{}", b.stdout);
    assert_eq!(lines[1], "    2  01234567  Ana  tw?[0mo");
    assert_eq!(lines[2], "    3  fedcba98  Bo  three");
    assert!(b.stdout.contains("at most 10000 lines"));

    let before = srv.seen().len();
    for args in [
        vec!["browse", "compare", "--project", "7", "--from", "v1"],
        vec!["browse", "compare", "--project", "7", "--to", "main"],
        vec!["browse", "blame", "--project", "7", "--ref", "main"],
        vec!["browse", "blame", "--project", "7", "--path", "a"],
    ] {
        assert_eq!(gstn(&sb, &srv, &args).code, 2, "{args:?}");
    }
    assert_eq!(srv.seen().len(), before, "no request for invalid input");
}

#[test]
fn a_binary_blame_says_so() {
    let srv = Mock::start(|_| {
        ok(&format!(
            r#"{{"ref":{{"type":"branch","name":"main","full_name":"refs/heads/main","commit":"{C}"}},"path":"x.bin","oid":"{C}","size":4096,"binary":true,"hunks":[],"commits":[],"truncated":false}}"#
        ))
    });
    let sb = Sandbox::new();
    let o = gstn(
        &sb,
        &srv,
        &[
            "browse",
            "blame",
            "--project",
            "7",
            "--ref",
            "main",
            "--path",
            "x.bin",
        ],
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stdout.contains("binary file, 4096 bytes"), "{}", o.stdout);
}
