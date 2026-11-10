//! End-to-end tests of `gstn doc show` / `gstn doc list` (plan 13 stage 3)
//! against a scripted loopback server: the route and query each command
//! sends, the text of a document (sections, blocks, references: a hidden
//! one by its id only), the list's paging note and what an agent's list
//! left out, an error shown as its code and request id, and nothing sent
//! for a document id that is not one. All data is synthetic.

#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly

mod common;
use common::{Mock, Out, Resp, Sandbox, TOKEN};

fn gstn(sb: &Sandbox, srv: &Mock, args: &[&str]) -> Out {
    sb.run_env(&srv.url, args, "", &[("GSTN_TOKEN", TOKEN)])
}

const DOC: &str = r#"{"version":1,"ref":{"type":"branch","name":"main","full_name":"refs/heads/main","commit":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"},"path":"safety/RA-1.md","lang":null,"translations":["en"],"id":"RA-1","doctype":"safety.risk-assessment","title":"Synthetic assessment","owner":"safety-team","classification":"internal","fields":[],"sections":[{"heading":"Hazards","line":7,"pieces":[{"type":"text","line":9,"markdown":"one.\ntwo.","display":"one. two.","block":null},{"type":"block","line":11,"markdown":null,"display":null,"block":{"kind":"hazard","id":"H-01","attrs":[{"key":"severity","value":"3"}],"deprecated":true,"body":"A hand\nin the gap.","display":"A hand in the gap."}}]}],"refs":[{"repo":null,"doc":"QP-1","block":null,"line":9,"target":"found","title":"Synthetic procedure","block_kind":null},{"repo":null,"doc":"URS-1","block":"REQ-2","line":12,"target":"hidden","title":null,"block_kind":null},{"repo":null,"doc":"QM-9","block":null,"line":13,"target":"broken","title":null,"block_kind":null}],"problems":[],"problem_count":0,"incomplete":null}"#;

const LIST: &str = r#"{"ref":{"type":"branch","name":"main","full_name":"refs/heads/main","commit":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"},"documents":[{"id":"QP-1","title":"Synthetic procedure","doctype":"qms.procedure","path":"qms/QP-1.md","classification":"internal","translations":[]}],"next_after":"QP-1","incomplete":null,"withheld":{"error_code":"FG-AUTH-0021","documents":1,"requirements":0,"work_items":0,"files":0}}"#;

#[test]
fn doc_show_reads_one_document_and_prints_it() {
    let srv = Mock::start(|_| Resp::json(200, DOC));
    let sb = Sandbox::new();
    let o = gstn(
        &sb,
        &srv,
        &[
            "doc",
            "show",
            "--project",
            "7",
            "RA-1",
            "--ref",
            "v1.0",
            "--lang",
            "en",
        ],
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    let r = srv.seen().pop().expect("a request");
    assert_eq!(r.method, "GET");
    assert_eq!(
        r.target,
        "/api/v1/projects/7/documents/RA-1?ref=v1.0&lang=en"
    );
    for want in [
        "RA-1  Synthetic assessment",
        "classification internal",
        "## Hazards",
        "one. two.",
        "[hazard H-01] severity=3 (deprecated)",
        "A hand in the gap.",
        "QP-1 Synthetic procedure",
        "URS-1#REQ-2 (more confidential: id only)",
        "QM-9 (broken)",
    ] {
        assert!(o.stdout.contains(want), "{want}: {}", o.stdout);
    }
    let sb = Sandbox::new();
    let json = gstn(
        &sb,
        &srv,
        &["doc", "show", "--project", "7", "RA-1", "--json"],
    )
    .json();
    assert_eq!(json["version"], 1);
    assert_eq!(
        srv.seen().pop().expect("a request").target,
        "/api/v1/projects/7/documents/RA-1"
    );
}

#[test]
fn doc_list_pages_and_says_what_was_withheld() {
    let srv = Mock::start(|_| Resp::json(200, LIST));
    let sb = Sandbox::new();
    let o = gstn(
        &sb,
        &srv,
        &[
            "doc",
            "list",
            "--project",
            "7",
            "--doctype",
            "qms.procedure",
            "--limit",
            "1",
            "--after",
            "A-0",
        ],
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    let r = srv.seen().pop().expect("a request");
    assert_eq!(
        r.target,
        "/api/v1/projects/7/documents?doctype=qms.procedure&limit=1&after=A-0"
    );
    assert!(
        o.stdout
            .contains("QP-1  internal  qms.procedure  qms/QP-1.md  Synthetic procedure"),
        "{}",
        o.stdout
    );
    assert!(o.stdout.contains("(more: --after QP-1)"), "{}", o.stdout);
    assert!(
        o.stdout.contains("1 confidential documents"),
        "{}",
        o.stdout
    );
}

#[test]
fn a_refused_document_shows_the_code_and_request_id() {
    let srv = Mock::start(|_| {
        Resp::json(
            403,
            r#"{"error_code":"FG-AUTH-0021","user_message":"This item is marked confidential and is not given to an AI agent's token. Open it with a person's account.","request_id":"req-synthetic-2"}"#,
        )
    });
    let sb = Sandbox::new();
    let o = gstn(&sb, &srv, &["doc", "show", "--project", "7", "URS-1"]);
    assert_ne!(o.code, 0);
    assert!(o.stderr.contains("FG-AUTH-0021"), "{}", o.stderr);
    assert!(o.stderr.contains("req-synthetic-2"), "{}", o.stderr);
}

#[test]
fn doc_show_refuses_a_bad_id_before_any_request() {
    let srv = Mock::start(|_| Resp::json(200, "{}"));
    let sb = Sandbox::new();
    for args in [
        vec!["doc", "show", "--project", "7", "../x"],
        vec!["doc", "show", "--project", "7", "a/b"],
        vec!["doc", "show", "--project", "7", "RA-1#H-1"],
        vec!["doc", "show", "--project", "7", "-x"],
        vec!["doc", "show", "--project", "0", "RA-1"],
        vec!["doc", "show", "RA-1"],
        vec!["doc", "list", "--project", "7", "--limit", "0"],
    ] {
        let o = gstn(&sb, &srv, &args);
        assert_ne!(o.code, 0, "{args:?}");
    }
    assert!(srv.seen().is_empty(), "nothing was sent");
}
