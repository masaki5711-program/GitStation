//! End-to-end tests of `gstn browse` (refs, resolve, tree, file; M2 MR7)
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
