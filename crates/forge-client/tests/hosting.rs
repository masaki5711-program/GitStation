//! End-to-end tests of the hosting commands (`project`, `group`, `member`,
//! `protected-ref`, `namespace`) against a scripted loopback server. All data
//! is synthetic.

#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly

mod common;
use common::{Mock, Out, Resp, Sandbox, TOKEN};
use serde_json::{json, Value};

fn gstn(sb: &Sandbox, srv: &Mock, args: &[&str]) -> Out {
    sb.run_env(&srv.url, args, "", &[("GSTN_TOKEN", TOKEN)])
}

fn body_of(srv: &Mock) -> Value {
    let last = srv.seen().pop().expect("a request");
    serde_json::from_str(&last.body).expect("json body")
}

fn ok(body: &str) -> Resp {
    Resp::json(200, body)
}

const PROJECT: &str = r#"{"id":7,"namespace_id":3,"path":"team/app","name":"app","display_name":"App","description":"","visibility":"private","kind":"system","created_at":"2026-01-01T00:00:00Z","role":"owner"}"#;
const GROUP: &str = r#"{"id":3,"namespace_id":9,"parent_id":null,"path":"team","name":"team","display_name":"Team","description":"","created_at":"2026-01-01T00:00:00Z","role":"owner"}"#;
const MEMBER: &str = r#"{"user_id":12,"username":"bob","display_name":"Bob","role":"developer","active":true,"created_at":"2026-01-01T00:00:00Z"}"#;
const RULE: &str = r#"{"id":2,"pattern":"refs/heads/main","push_min_role":"maintainer","created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z"}"#;

fn error_server(status: u16, code: &str) -> Mock {
    let code = code.to_string();
    Mock::start(move |_| {
        Resp::json(
            status,
            &format!(
                r#"{{"error_code":"{code}","user_message":"操作できません。","request_id":"req-77","sql":"SELECT secret FROM users"}}"#
            ),
        )
    })
}

// ── project ────────────────────────────────────────────────────────────

#[test]
fn project_list_sends_limit_and_cursor_and_hints_the_next_page() {
    let srv = Mock::start(|_| {
        ok(&format!(
            r#"{{"items":[{PROJECT}],"next_cursor":"c-next","has_more":true}}"#
        ))
    });
    let sb = Sandbox::new();
    let o = gstn(
        &sb,
        &srv,
        &[
            "project", "list", "--limit", "5", "--cursor", "c-1", "--json",
        ],
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    let r = srv.seen().pop().unwrap();
    assert_eq!(r.method, "GET");
    assert_eq!(r.target, "/api/v1/projects?limit=5&cursor=c-1");
    assert_eq!(r.headers["authorization"], format!("Bearer {TOKEN}"));
    assert_eq!(o.json()["items"][0]["path"], "team/app");
    assert_eq!(o.json()["next_cursor"], "c-next");

    let h = gstn(&sb, &srv, &["project", "list"]);
    assert_eq!(h.code, 0);
    assert!(h.stdout.contains("team/app") && h.stdout.contains("more: --cursor c-next"));
    assert_eq!(srv.seen().pop().unwrap().target, "/api/v1/projects");
}

#[test]
fn project_list_all_follows_the_cursor_and_merges() {
    let srv = Mock::start(|r| {
        if r.target.contains("cursor=c-2") {
            ok(r#"{"items":[{"id":3}],"next_cursor":null,"has_more":false}"#)
        } else if r.target.contains("cursor=c-1") {
            ok(r#"{"items":[{"id":2}],"next_cursor":"c-2","has_more":true}"#)
        } else {
            ok(r#"{"items":[{"id":1}],"next_cursor":"c-1","has_more":true}"#)
        }
    });
    let sb = Sandbox::new();
    let o = gstn(&sb, &srv, &["project", "list", "--all", "--json"]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(
        o.json(),
        json!({"items":[{"id":1},{"id":2},{"id":3}],"next_cursor":null,"has_more":false})
    );
    let targets: Vec<String> = srv.seen().into_iter().map(|r| r.target).collect();
    assert_eq!(
        targets,
        [
            "/api/v1/projects?limit=100",
            "/api/v1/projects?limit=100&cursor=c-1",
            "/api/v1/projects?limit=100&cursor=c-2"
        ]
    );
}

#[test]
fn project_list_all_stops_on_a_repeated_cursor() {
    let srv = Mock::start(|_| ok(r#"{"items":[{"id":1}],"next_cursor":"same","has_more":true}"#));
    let sb = Sandbox::new();
    let o = gstn(&sb, &srv, &["project", "list", "--all", "--json"]);
    assert_eq!(o.code, 1);
    assert_eq!(o.err_json()["error_code"], "FG-CLI-0004");
    assert_eq!(srv.seen().len(), 2, "stopped at the repeat");
}

#[test]
fn list_options_are_validated_before_any_request() {
    let srv = Mock::start(|_| ok("{}"));
    let sb = Sandbox::new();
    for args in [
        vec!["project", "list", "--limit", "0"],
        vec!["project", "list", "--limit", "101"],
        vec!["project", "list", "--limit", "abc"],
        vec!["project", "list", "--cursor", ""],
        vec!["project", "list", "--all", "--cursor", "x"],
        vec!["group", "list", "--limit", "-1"],
    ] {
        let o = gstn(&sb, &srv, &args);
        assert_eq!(o.code, 2, "{args:?}: {}", o.stderr);
    }
    assert!(srv.seen().is_empty());
}

#[test]
fn project_view_requests_the_id_and_renders_a_detail() {
    let srv = Mock::start(|_| ok(PROJECT));
    let sb = Sandbox::new();
    let o = gstn(&sb, &srv, &["project", "view", "7"]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(srv.seen().pop().unwrap().target, "/api/v1/projects/7");
    assert!(o.stdout.contains("path: team/app") && o.stdout.contains("kind: system"));
    let j = gstn(
        &sb,
        &srv,
        &["project", "view", "7", "--json", "--fields", "id,path"],
    );
    assert_eq!(j.json(), json!({"id": 7, "path": "team/app"}));
}

#[test]
fn ids_are_validated_and_never_reach_the_path_unchecked() {
    let srv = Mock::start(|_| ok("{}"));
    let sb = Sandbox::new();
    for bad in [
        "0",
        "-1",
        "abc",
        "../x",
        "7/restore",
        "7?x=1",
        "99999999999999999999",
        "",
    ] {
        for args in [
            vec!["project", "view", bad],
            vec!["group", "view", bad],
            vec!["project", "delete", bad, "--yes"],
            vec!["member", "list", "--project", bad],
            vec!["protected-ref", "list", "--project", bad],
        ] {
            let o = gstn(&sb, &srv, &args);
            assert_eq!(o.code, 2, "{args:?}");
        }
    }
    assert!(srv.seen().is_empty(), "no request for a bad id");
}

#[test]
fn project_create_posts_only_what_was_given() {
    let srv = Mock::start(|_| Resp::json(201, PROJECT));
    let sb = Sandbox::new();
    let o = gstn(
        &sb,
        &srv,
        &[
            "project",
            "create",
            "--namespace-id",
            "3",
            "--path",
            "app",
            "--display-name",
            "App",
            "--json",
        ],
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    let r = srv.seen().pop().unwrap();
    assert_eq!(
        (r.method.as_str(), r.target.as_str()),
        ("POST", "/api/v1/projects")
    );
    assert_eq!(
        serde_json::from_str::<Value>(&r.body).unwrap(),
        json!({"namespace_id": 3, "path": "app", "display_name": "App"})
    );
    assert_eq!(o.json()["id"], 7);

    let full = gstn(
        &sb,
        &srv,
        &[
            "project",
            "create",
            "--namespace-id",
            "3",
            "--path",
            "spec",
            "--display-name",
            "Spec",
            "--description",
            "d",
            "--kind",
            "spec",
            "--visibility",
            "internal",
        ],
    );
    assert_eq!(full.code, 0, "{}", full.stderr);
    assert_eq!(
        body_of(&srv),
        json!({"namespace_id": 3, "path": "spec", "display_name": "Spec", "description": "d",
               "kind": "spec", "visibility": "internal"})
    );
    assert!(full.stdout.starts_with("Created project 7"));
}

#[test]
fn project_create_rejects_bad_input_locally() {
    let srv = Mock::start(|_| ok("{}"));
    let sb = Sandbox::new();
    for args in [
        vec![
            "project",
            "create",
            "--namespace-id",
            "3",
            "--path",
            "",
            "--display-name",
            "A",
        ],
        vec![
            "project",
            "create",
            "--namespace-id",
            "3",
            "--path",
            "a\nb",
            "--display-name",
            "A",
        ],
        vec![
            "project",
            "create",
            "--namespace-id",
            "3",
            "--path",
            "a",
            "--display-name",
            "A",
            "--kind",
            "other",
        ],
        vec![
            "project",
            "create",
            "--namespace-id",
            "3",
            "--path",
            "a",
            "--display-name",
            "A",
            "--visibility",
            "public",
        ],
        vec!["project", "create", "--path", "a", "--display-name", "A"],
    ] {
        assert_eq!(gstn(&sb, &srv, &args).code, 2, "{args:?}");
    }
    assert!(srv.seen().is_empty());
}

#[test]
fn project_edit_patches_the_given_fields_and_needs_one() {
    let srv = Mock::start(|_| ok(PROJECT));
    let sb = Sandbox::new();
    let o = gstn(
        &sb,
        &srv,
        &[
            "project",
            "edit",
            "7",
            "--display-name",
            "New",
            "--description",
            "",
        ],
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    let r = srv.seen().pop().unwrap();
    assert_eq!(
        (r.method.as_str(), r.target.as_str()),
        ("PATCH", "/api/v1/projects/7")
    );
    assert_eq!(
        serde_json::from_str::<Value>(&r.body).unwrap(),
        json!({"display_name": "New", "description": ""})
    );
    let none = gstn(&sb, &srv, &["project", "edit", "7", "--json"]);
    assert_eq!(none.code, 2);
    assert!(none.err_json()["detail"]
        .as_str()
        .unwrap()
        .contains("nothing_to_change"));
    assert_eq!(srv.seen().len(), 1, "no request for an empty edit");
}

#[test]
fn project_delete_needs_yes_before_anything_happens() {
    let srv = Mock::start(|_| {
        ok(r#"{"id":7,"deleted_at":"2026-01-01T00:00:00Z","purge_after":"2026-01-31T00:00:00Z"}"#)
    });
    let sb = Sandbox::new();
    // no token either: the guard comes first
    let o = sb.run(&srv.url, &["project", "delete", "7", "--json"], "");
    assert_eq!(o.code, 2);
    let e = o.err_json();
    assert_eq!(e["error_code"], "FG-CLI-0001");
    assert!(e["detail"]
        .as_str()
        .unwrap()
        .contains("confirmation_required"));
    assert!(srv.seen().is_empty());

    let y = gstn(&sb, &srv, &["project", "delete", "7", "--yes", "--json"]);
    assert_eq!(y.code, 0, "{}", y.stderr);
    let r = srv.seen().pop().unwrap();
    assert_eq!(
        (r.method.as_str(), r.target.as_str()),
        ("DELETE", "/api/v1/projects/7")
    );
    assert_eq!(y.json()["purge_after"], "2026-01-31T00:00:00Z");
    let h = gstn(&sb, &srv, &["project", "delete", "7", "--yes"]);
    assert!(h.stdout.contains("restorable until 2026-01-31"));
}

#[test]
fn project_restore_posts_to_restore() {
    let srv = Mock::start(|_| ok(PROJECT));
    let sb = Sandbox::new();
    let o = gstn(&sb, &srv, &["project", "restore", "7"]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    let r = srv.seen().pop().unwrap();
    assert_eq!(
        (r.method.as_str(), r.target.as_str()),
        ("POST", "/api/v1/projects/7/restore")
    );
    assert!(r.body.is_empty());
}

// ── group ──────────────────────────────────────────────────────────────

#[test]
fn group_list_view_create_edit() {
    let srv = Mock::start(|r| {
        if r.target.starts_with("/api/v1/groups?")
            || r.target == "/api/v1/groups" && r.method == "GET"
        {
            ok(&format!(
                r#"{{"items":[{GROUP}],"next_cursor":null,"has_more":false}}"#
            ))
        } else if r.method == "POST" {
            Resp::json(201, GROUP)
        } else {
            ok(GROUP)
        }
    });
    let sb = Sandbox::new();
    let l = gstn(&sb, &srv, &["group", "list", "--limit", "10", "--json"]);
    assert_eq!(l.code, 0, "{}", l.stderr);
    assert_eq!(srv.seen().pop().unwrap().target, "/api/v1/groups?limit=10");
    assert_eq!(l.json()["items"][0]["path"], "team");

    let v = gstn(&sb, &srv, &["group", "view", "3"]);
    assert_eq!(srv.seen().pop().unwrap().target, "/api/v1/groups/3");
    assert!(v.stdout.contains("path: team") && v.stdout.contains("parent_id: -"));

    let c = gstn(
        &sb,
        &srv,
        &[
            "group",
            "create",
            "--path",
            "team",
            "--display-name",
            "Team",
            "--description",
            "x",
        ],
    );
    assert_eq!(c.code, 0, "{}", c.stderr);
    let r = srv.seen().pop().unwrap();
    assert_eq!(
        (r.method.as_str(), r.target.as_str()),
        ("POST", "/api/v1/groups")
    );
    assert_eq!(
        serde_json::from_str::<Value>(&r.body).unwrap(),
        json!({"path": "team", "display_name": "Team", "description": "x"})
    );

    let s = gstn(
        &sb,
        &srv,
        &[
            "group",
            "create",
            "--parent-id",
            "3",
            "--path",
            "web",
            "--display-name",
            "Web",
        ],
    );
    assert_eq!(s.code, 0, "{}", s.stderr);
    let r = srv.seen().pop().unwrap();
    assert_eq!(r.target, "/api/v1/groups/3/subgroups");
    assert_eq!(
        serde_json::from_str::<Value>(&r.body).unwrap(),
        json!({"path": "web", "display_name": "Web"})
    );

    let e = gstn(&sb, &srv, &["group", "edit", "3", "--display-name", "T2"]);
    assert_eq!(e.code, 0, "{}", e.stderr);
    let r = srv.seen().pop().unwrap();
    assert_eq!(
        (r.method.as_str(), r.target.as_str()),
        ("PATCH", "/api/v1/groups/3")
    );
    assert_eq!(
        serde_json::from_str::<Value>(&r.body).unwrap(),
        json!({"display_name": "T2"})
    );
    assert_eq!(gstn(&sb, &srv, &["group", "edit", "3"]).code, 2);
}

// ── member ─────────────────────────────────────────────────────────────

#[test]
fn member_list_uses_after_and_all_follows_next_after() {
    let srv = Mock::start(|r| {
        if r.target.contains("after=20") {
            ok(&format!(r#"{{"members":[{MEMBER}],"next_after":null}}"#))
        } else if r.target.contains("after=10") {
            ok(r#"{"members":[{"user_id":20}],"next_after":20}"#)
        } else {
            ok(r#"{"members":[{"user_id":10}],"next_after":10}"#)
        }
    });
    let sb = Sandbox::new();
    let one = gstn(
        &sb,
        &srv,
        &[
            "member",
            "list",
            "--project",
            "7",
            "--limit",
            "1",
            "--after",
            "5",
            "--json",
        ],
    );
    assert_eq!(one.code, 0, "{}", one.stderr);
    let r = srv.seen().pop().unwrap();
    assert_eq!(r.target, "/api/v1/projects/7/members?limit=1&after=5");
    assert_eq!(one.json()["next_after"], 10);

    let h = gstn(&sb, &srv, &["member", "list", "--group", "3"]);
    assert!(h.stdout.contains("more: --after 10"), "{}", h.stdout);

    let all = gstn(
        &sb,
        &srv,
        &["member", "list", "--group", "3", "--all", "--json"],
    );
    assert_eq!(all.code, 0, "{}", all.stderr);
    let v = all.json();
    assert_eq!(v["members"].as_array().unwrap().len(), 3);
    assert_eq!(v["members"][2]["username"], "bob");
    assert_eq!(v["next_after"], Value::Null);
    let targets: Vec<String> = srv
        .seen()
        .into_iter()
        .rev()
        .take(3)
        .map(|r| r.target)
        .collect();
    assert_eq!(
        targets,
        [
            "/api/v1/groups/3/members?limit=100&after=20",
            "/api/v1/groups/3/members?limit=100&after=10",
            "/api/v1/groups/3/members?limit=100"
        ]
    );
    let f = gstn(
        &sb,
        &srv,
        &[
            "member", "list", "--group", "3", "--all", "--json", "--fields", "user_id",
        ],
    );
    assert_eq!(f.json()["members"][2], json!({"user_id": 12}));
}

#[test]
fn member_all_stops_on_a_repeated_next_after() {
    let srv = Mock::start(|_| ok(r#"{"members":[{"user_id":10}],"next_after":10}"#));
    let sb = Sandbox::new();
    let o = gstn(
        &sb,
        &srv,
        &["member", "list", "--project", "7", "--all", "--json"],
    );
    assert_eq!(o.code, 1);
    assert_eq!(o.err_json()["error_code"], "FG-CLI-0004");
}

#[test]
fn member_needs_exactly_one_target() {
    let srv = Mock::start(|_| ok("{}"));
    let sb = Sandbox::new();
    for args in [
        vec!["member", "list"],
        vec!["member", "list", "--project", "7", "--group", "3"],
        vec!["member", "add", "--user-id", "1", "--role", "guest"],
        vec!["member", "remove", "--user-id", "1", "--yes"],
        vec![
            "member",
            "add",
            "--project",
            "7",
            "--user-id",
            "1",
            "--role",
            "boss",
        ],
        vec![
            "member",
            "add",
            "--project",
            "7",
            "--user-id",
            "0",
            "--role",
            "guest",
        ],
    ] {
        assert_eq!(gstn(&sb, &srv, &args).code, 2, "{args:?}");
    }
    assert!(srv.seen().is_empty());
}

#[test]
fn member_add_set_role_remove_for_project_and_group() {
    let srv = Mock::start(|r| match r.method.as_str() {
        "DELETE" => Resp {
            status: 204,
            headers: vec![],
            body: String::new(),
        },
        "POST" => Resp::json(201, MEMBER),
        _ => ok(MEMBER),
    });
    let sb = Sandbox::new();

    let a = gstn(
        &sb,
        &srv,
        &[
            "member",
            "add",
            "--project",
            "7",
            "--user-id",
            "12",
            "--role",
            "developer",
            "--json",
        ],
    );
    assert_eq!(a.code, 0, "{}", a.stderr);
    let r = srv.seen().pop().unwrap();
    assert_eq!(
        (r.method.as_str(), r.target.as_str()),
        ("POST", "/api/v1/projects/7/members")
    );
    assert_eq!(
        serde_json::from_str::<Value>(&r.body).unwrap(),
        json!({"user_id": 12, "role": "developer"})
    );
    assert_eq!(a.json()["username"], "bob");

    let g = gstn(
        &sb,
        &srv,
        &[
            "member",
            "add",
            "--group",
            "3",
            "--user-id",
            "12",
            "--role",
            "owner",
        ],
    );
    assert_eq!(g.code, 0, "{}", g.stderr);
    assert_eq!(srv.seen().pop().unwrap().target, "/api/v1/groups/3/members");
    assert!(g.stdout.contains("Added user 12 to group 3 as owner"));

    let s = gstn(
        &sb,
        &srv,
        &[
            "member",
            "set-role",
            "--group",
            "3",
            "--user-id",
            "12",
            "--role",
            "maintainer",
        ],
    );
    assert_eq!(s.code, 0, "{}", s.stderr);
    let r = srv.seen().pop().unwrap();
    assert_eq!(
        (r.method.as_str(), r.target.as_str()),
        ("PATCH", "/api/v1/groups/3/members/12")
    );
    assert_eq!(
        serde_json::from_str::<Value>(&r.body).unwrap(),
        json!({"role": "maintainer"})
    );

    let p = gstn(
        &sb,
        &srv,
        &[
            "member",
            "set-role",
            "--project",
            "7",
            "--user-id",
            "12",
            "--role",
            "guest",
        ],
    );
    assert_eq!(p.code, 0, "{}", p.stderr);
    assert_eq!(
        srv.seen().pop().unwrap().target,
        "/api/v1/projects/7/members/12"
    );

    let n = srv.seen().len();
    let no = gstn(
        &sb,
        &srv,
        &["member", "remove", "--project", "7", "--user-id", "12"],
    );
    assert_eq!(no.code, 2);
    assert_eq!(srv.seen().len(), n, "no request without --yes");

    let d = gstn(
        &sb,
        &srv,
        &[
            "member",
            "remove",
            "--project",
            "7",
            "--user-id",
            "12",
            "--yes",
            "--json",
        ],
    );
    assert_eq!(d.code, 0, "{}", d.stderr);
    let r = srv.seen().pop().unwrap();
    assert_eq!(
        (r.method.as_str(), r.target.as_str()),
        ("DELETE", "/api/v1/projects/7/members/12")
    );
    assert_eq!(
        d.json(),
        json!({"removed": true, "target": "project", "target_id": 7, "user_id": 12})
    );
    let dg = gstn(
        &sb,
        &srv,
        &[
            "member",
            "remove",
            "--group",
            "3",
            "--user-id",
            "12",
            "--yes",
        ],
    );
    assert_eq!(
        srv.seen().pop().unwrap().target,
        "/api/v1/groups/3/members/12"
    );
    assert!(dg.stdout.contains("Removed user 12 from group 3"));
}

// ── protected-ref ──────────────────────────────────────────────────────

#[test]
fn protected_ref_list_add_remove() {
    let srv = Mock::start(|r| match r.method.as_str() {
        "DELETE" => Resp {
            status: 204,
            headers: vec![],
            body: String::new(),
        },
        "POST" => Resp::json(201, RULE),
        _ => ok(&format!(r#"{{"rules":[{RULE}]}}"#)),
    });
    let sb = Sandbox::new();

    let l = gstn(&sb, &srv, &["protected-ref", "list", "--project", "7"]);
    assert_eq!(l.code, 0, "{}", l.stderr);
    assert_eq!(
        srv.seen().pop().unwrap().target,
        "/api/v1/projects/7/protected-refs"
    );
    assert!(l.stdout.contains("refs/heads/main") && l.stdout.contains("maintainer"));
    let lj = gstn(
        &sb,
        &srv,
        &[
            "protected-ref",
            "list",
            "--project",
            "7",
            "--json",
            "--fields",
            "id",
        ],
    );
    assert_eq!(lj.json(), json!({"rules": [{"id": 2}]}));

    let a = gstn(
        &sb,
        &srv,
        &[
            "protected-ref",
            "add",
            "--project",
            "7",
            "--pattern",
            "refs/heads/main",
            "--push-min-role",
            "maintainer",
            "--json",
        ],
    );
    assert_eq!(a.code, 0, "{}", a.stderr);
    let r = srv.seen().pop().unwrap();
    assert_eq!(
        (r.method.as_str(), r.target.as_str()),
        ("POST", "/api/v1/projects/7/protected-refs")
    );
    assert_eq!(
        serde_json::from_str::<Value>(&r.body).unwrap(),
        json!({"pattern": "refs/heads/main", "push_min_role": "maintainer"})
    );
    assert_eq!(a.json()["id"], 2);
    // guest cannot be a minimum pusher: refused before any request
    let n = srv.seen().len();
    let bad = gstn(
        &sb,
        &srv,
        &[
            "protected-ref",
            "add",
            "--project",
            "7",
            "--pattern",
            "refs/heads/x",
            "--push-min-role",
            "guest",
        ],
    );
    assert_eq!(bad.code, 2);

    let no = gstn(
        &sb,
        &srv,
        &[
            "protected-ref",
            "remove",
            "--project",
            "7",
            "--rule-id",
            "2",
        ],
    );
    assert_eq!(no.code, 2);
    assert_eq!(
        srv.seen().len(),
        n,
        "no request without --yes or with a bad role"
    );

    let d = gstn(
        &sb,
        &srv,
        &[
            "protected-ref",
            "remove",
            "--project",
            "7",
            "--rule-id",
            "2",
            "--yes",
            "--json",
        ],
    );
    assert_eq!(d.code, 0, "{}", d.stderr);
    let r = srv.seen().pop().unwrap();
    assert_eq!(
        (r.method.as_str(), r.target.as_str()),
        ("DELETE", "/api/v1/projects/7/protected-refs/2")
    );
    assert_eq!(
        d.json(),
        json!({"removed": true, "project_id": 7, "rule_id": 2})
    );
}

// ── namespace ──────────────────────────────────────────────────────────

#[test]
fn namespace_resolve_encodes_the_path_as_a_query_value() {
    let srv = Mock::start(|_| {
        ok(
            r#"{"namespace_id":3,"kind":"project","canonical_path":"team/app","moved":true,"project_id":7}"#,
        )
    });
    let sb = Sandbox::new();
    let o = gstn(&sb, &srv, &["namespace", "resolve", "team/app", "--json"]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    let r = srv.seen().pop().unwrap();
    assert_eq!(r.method, "GET");
    assert_eq!(r.target, "/api/v1/namespaces/resolve?path=team%2Fapp");
    assert_eq!(o.json()["project_id"], 7);

    let h = gstn(&sb, &srv, &["namespace", "resolve", "team/app"]);
    assert!(
        h.stdout.starts_with("project 7 team/app (moved)"),
        "{}",
        h.stdout
    );

    // hostile characters stay inside the one value
    let x = gstn(&sb, &srv, &["namespace", "resolve", "a&b=c#d e"]);
    assert_eq!(x.code, 0, "{}", x.stderr);
    let t = srv.seen().pop().unwrap().target;
    assert_eq!(t.matches('&').count(), 0, "{t}");
    assert!(!t.contains('#') && !t.contains(' '), "{t}");
    assert!(t.starts_with("/api/v1/namespaces/resolve?path="));

    for bad in ["", "   ", "a\u{0}b", "a\nb"] {
        assert_eq!(
            gstn(&sb, &srv, &["namespace", "resolve", bad]).code,
            2,
            "{bad:?}"
        );
    }
}

#[test]
fn namespace_resolve_rejects_an_unknown_kind_from_the_server() {
    let srv = Mock::start(|_| {
        ok(r#"{"namespace_id":3,"kind":"alien","canonical_path":"x","moved":false}"#)
    });
    let sb = Sandbox::new();
    let o = gstn(&sb, &srv, &["namespace", "resolve", "x", "--json"]);
    assert_eq!(o.code, 1);
    assert_eq!(o.err_json()["error_code"], "FG-CLI-0004");
}

// ── errors ─────────────────────────────────────────────────────────────

#[test]
fn server_errors_show_the_user_message_and_request_id_only() {
    for (status, code, exit) in [
        (403, "FG-AUTH-0003", 3),
        (404, "FG-REPO-0005", 4),
        (409, "FG-REPO-0010", 5),
        (503, "FG-DB-0001", 6),
    ] {
        let srv = error_server(status, code);
        let sb = Sandbox::new();
        for args in [
            vec!["project", "view", "7"],
            vec!["project", "delete", "7", "--yes"],
            vec![
                "member",
                "remove",
                "--group",
                "3",
                "--user-id",
                "1",
                "--yes",
            ],
            vec!["protected-ref", "list", "--project", "7"],
            vec!["namespace", "resolve", "a/b"],
            vec!["project", "list", "--all"],
        ] {
            let o = gstn(&sb, &srv, &args);
            assert_eq!(o.code, exit, "{args:?}: {}", o.stderr);
            assert!(o.stdout.is_empty(), "nothing on stdout on failure");
            assert!(
                o.stderr.contains("操作できません。") && o.stderr.contains("request_id=req-77")
            );
            assert!(o.stderr.contains(code));
            assert!(!o.all().contains("SELECT") && !o.all().contains(TOKEN));
        }
        let j = gstn(&sb, &srv, &["project", "view", "7", "--json"]);
        let e = j.err_json();
        assert_eq!(e["error_code"], code);
        assert_eq!(e["request_id"], "req-77");
        assert_eq!(e["user_message"], "操作できません。");
        assert!(!j.stderr.contains("SELECT"));
    }
}

#[test]
fn hosting_commands_need_a_login_and_make_no_request_without_one() {
    let srv = Mock::start(|_| ok("{}"));
    let sb = Sandbox::new();
    for args in [
        vec!["project", "list"],
        vec!["group", "view", "3"],
        vec!["member", "list", "--project", "7"],
        vec!["protected-ref", "list", "--project", "7"],
        vec!["namespace", "resolve", "a"],
    ] {
        let o = sb.run(&srv.url, &args, "");
        assert_eq!(o.code, 3, "{args:?}");
    }
    assert!(srv.seen().is_empty());
}

#[test]
fn an_unreachable_server_is_retryable() {
    let sb = Sandbox::new();
    let o = sb.run_env(
        "http://127.0.0.1:1",
        &["project", "view", "7", "--json"],
        "",
        &[("GSTN_TOKEN", TOKEN)],
    );
    assert_eq!(o.code, 6);
    assert_eq!(o.err_json()["retryable"], true);
}

#[test]
fn every_hosting_command_is_described_for_machines() {
    let sb = Sandbox::new();
    let v = sb.run("", &["help", "--json"], "").json();
    let by_name = |n: &str| {
        v["commands"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == n)
            .cloned()
            .unwrap_or_else(|| panic!("{n} missing"))
    };
    for n in ["project delete", "member remove", "protected-ref remove"] {
        let c = by_name(n);
        assert_eq!(c["scopes"], json!(["api"]));
        assert!(
            c["args"]
                .as_array()
                .unwrap()
                .iter()
                .any(|a| a["name"] == "--yes" && a["required"] == true),
            "{n} documents --yes"
        );
    }
    assert_eq!(by_name("project list")["scopes"], json!(["read_api"]));
}

// ── visibility, moves, renames, releases (feat/client-gstn-hosting-more) ──

const MOVE: &str = r#"{"id":7,"kind":"project","namespace_id":21,"old_path":"team/app","path":"other/app","tombstones":1,"projects":1,"reclaimed_tombstones":0}"#;

fn req_of(srv: &Mock) -> (String, String, Value) {
    let r = srv.seen().pop().expect("a request");
    let b = serde_json::from_str(&r.body).expect("json body");
    (r.method, r.target, b)
}

/// Each destructive command: no `--yes` means a usage error (exit 2,
/// `confirmation_required`) before any token read or request.
#[test]
fn moves_renames_visibility_and_release_need_yes_before_anything_happens() {
    let srv = Mock::start(|_| ok(MOVE));
    let sb = Sandbox::new();
    for args in [
        vec!["project", "visibility", "7", "--visibility", "internal"],
        vec!["project", "move", "7", "--path", "x"],
        vec!["group", "move", "3", "--top-level"],
        vec!["user", "rename", "12", "--username", "robert"],
        vec!["namespace", "release-path", "team/old"],
    ] {
        let mut a = args.clone();
        a.push("--json");
        // no token either: the guard comes first
        let o = sb.run(&srv.url, &a, "");
        assert_eq!(o.code, 2, "{args:?}: {}", o.stderr);
        let e = o.err_json();
        assert_eq!(e["error_code"], "FG-CLI-0001");
        assert!(e["detail"]
            .as_str()
            .unwrap()
            .contains("confirmation_required"));
    }
    assert!(srv.seen().is_empty());
}

#[test]
fn project_visibility_puts_the_visibility() {
    let srv = Mock::start(|_| ok(&PROJECT.replace("private", "internal")));
    let sb = Sandbox::new();
    let o = gstn(
        &sb,
        &srv,
        &[
            "project",
            "visibility",
            "7",
            "--visibility",
            "internal",
            "--yes",
            "--json",
        ],
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    let (m, t, b) = req_of(&srv);
    assert_eq!(
        (m.as_str(), t.as_str()),
        ("PUT", "/api/v1/projects/7/visibility")
    );
    assert_eq!(b, json!({"visibility":"internal"}));
    assert_eq!(o.json()["visibility"], "internal");
    let h = gstn(
        &sb,
        &srv,
        &[
            "project",
            "visibility",
            "7",
            "--visibility",
            "private",
            "--yes",
        ],
    );
    assert!(h.stdout.contains("Project 7 (team/app) is now"));
    // an unknown visibility is rejected locally
    let n = srv.seen().len();
    let bad = gstn(
        &sb,
        &srv,
        &[
            "project",
            "visibility",
            "7",
            "--visibility",
            "public",
            "--yes",
        ],
    );
    assert_eq!(bad.code, 2);
    assert_eq!(srv.seen().len(), n);
}

#[test]
fn project_move_and_rename_send_only_what_was_given() {
    let srv = Mock::start(|_| ok(MOVE));
    let sb = Sandbox::new();
    let o = gstn(
        &sb,
        &srv,
        &[
            "project",
            "move",
            "7",
            "--namespace-id",
            "21",
            "--path",
            "app",
            "--reclaim",
            "--yes",
            "--json",
        ],
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    let (m, t, b) = req_of(&srv);
    assert_eq!(
        (m.as_str(), t.as_str()),
        ("POST", "/api/v1/projects/7/transfer")
    );
    assert_eq!(b, json!({"namespace_id":21,"path":"app","reclaim":true}));
    assert_eq!(o.json()["path"], "other/app");

    // a rename: --path alone
    let r = gstn(
        &sb,
        &srv,
        &["project", "move", "7", "--path", "app2", "--yes"],
    );
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(body_of(&srv), json!({"path":"app2"}));
    assert!(r.stdout.contains("Moved project 7: team/app -> other/app"));
    assert!(r.stdout.contains("tombstones: 1"));

    // nothing to change: no request
    let n = srv.seen().len();
    let e = gstn(&sb, &srv, &["project", "move", "7", "--yes", "--json"]);
    assert_eq!(e.code, 2);
    assert!(e.err_json()["detail"]
        .as_str()
        .unwrap()
        .contains("nothing_to_change"));
    assert_eq!(srv.seen().len(), n);
    for bad in [
        vec!["project", "move", "0", "--path", "x", "--yes"],
        vec!["project", "move", "7", "--namespace-id", "-1", "--yes"],
        vec!["project", "move", "7", "--path", "", "--yes"],
    ] {
        assert_eq!(gstn(&sb, &srv, &bad).code, 2, "{bad:?}");
    }
    assert_eq!(srv.seen().len(), n);
}

#[test]
fn group_move_takes_a_namespace_or_top_level() {
    let srv = Mock::start(|_| ok(&MOVE.replace("project", "group")));
    let sb = Sandbox::new();
    let o = gstn(
        &sb,
        &srv,
        &[
            "group",
            "move",
            "3",
            "--namespace-id",
            "9",
            "--yes",
            "--json",
        ],
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    let (m, t, b) = req_of(&srv);
    assert_eq!(
        (m.as_str(), t.as_str()),
        ("POST", "/api/v1/groups/3/transfer")
    );
    assert_eq!(b, json!({"namespace_id":9}));

    // --namespace is the same flag
    let a = gstn(
        &sb,
        &srv,
        &["group", "move", "3", "--namespace", "9", "--yes"],
    );
    assert_eq!(a.code, 0, "{}", a.stderr);
    assert_eq!(body_of(&srv), json!({"namespace_id":9}));
    assert!(a.stdout.contains("Moved group 7"));

    let t = gstn(
        &sb,
        &srv,
        &[
            "group",
            "move",
            "3",
            "--top-level",
            "--path",
            "platform",
            "--yes",
        ],
    );
    assert_eq!(t.code, 0, "{}", t.stderr);
    assert_eq!(body_of(&srv), json!({"top_level":true,"path":"platform"}));

    let n = srv.seen().len();
    // both, or neither: usage errors, no request
    let both = gstn(
        &sb,
        &srv,
        &[
            "group",
            "move",
            "3",
            "--namespace-id",
            "9",
            "--top-level",
            "--yes",
        ],
    );
    assert_eq!(both.code, 2);
    let none = gstn(&sb, &srv, &["group", "move", "3", "--yes", "--json"]);
    assert_eq!(none.code, 2);
    assert!(none.err_json()["detail"]
        .as_str()
        .unwrap()
        .contains("nothing_to_change"));
    assert_eq!(srv.seen().len(), n);
}

#[test]
fn user_rename_posts_the_username() {
    let srv = Mock::start(|_| ok(&MOVE.replace("project", "user")));
    let sb = Sandbox::new();
    let o = gstn(
        &sb,
        &srv,
        &[
            "user",
            "rename",
            "12",
            "--username",
            "robert",
            "--reclaim",
            "--yes",
            "--json",
        ],
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    let (m, t, b) = req_of(&srv);
    assert_eq!(
        (m.as_str(), t.as_str()),
        ("POST", "/api/v1/users/12/rename")
    );
    assert_eq!(b, json!({"username":"robert","reclaim":true}));
    let h = gstn(
        &sb,
        &srv,
        &["user", "rename", "12", "--username", "robert", "--yes"],
    );
    assert_eq!(body_of(&srv), json!({"username":"robert"}));
    assert!(h.stdout.contains("Moved user 7"));
    // --username is required
    assert_eq!(gstn(&sb, &srv, &["user", "rename", "12", "--yes"]).code, 2);
}

#[test]
fn namespace_release_path_posts_the_path_in_the_body() {
    let srv = Mock::start(|_| ok(r#"{"path":"team/old app","kind":"moved"}"#));
    let sb = Sandbox::new();
    let o = gstn(
        &sb,
        &srv,
        &[
            "namespace",
            "release-path",
            "team/old app",
            "--yes",
            "--json",
        ],
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    let (m, t, b) = req_of(&srv);
    assert_eq!(
        (m.as_str(), t.as_str()),
        ("POST", "/api/v1/namespaces/tombstones/release")
    );
    assert_eq!(b, json!({"path":"team/old app"}));
    let h = gstn(
        &sb,
        &srv,
        &["namespace", "release-path", "team/old", "--yes"],
    );
    assert!(h.stdout.contains("Released") && h.stdout.contains("moved tombstone"));
}

#[test]
fn protected_ref_edit_patches_the_push_role() {
    let srv = Mock::start(|_| ok(&RULE.replace("maintainer", "owner")));
    let sb = Sandbox::new();
    let edit = |role: &'static str| {
        vec![
            "protected-ref",
            "edit",
            "--project",
            "7",
            "--rule-id",
            "2",
            "--push-min-role",
            role,
        ]
    };
    let mut a = edit("owner");
    a.push("--json");
    let o = gstn(&sb, &srv, &a);
    assert_eq!(o.code, 0, "{}", o.stderr);
    let (m, t, b) = req_of(&srv);
    assert_eq!(
        (m.as_str(), t.as_str()),
        ("PATCH", "/api/v1/projects/7/protected-refs/2")
    );
    assert_eq!(b, json!({"push_min_role":"owner"}));
    assert_eq!(o.json()["push_min_role"], "owner");
    let h = gstn(&sb, &srv, &edit("owner"));
    assert!(h
        .stdout
        .contains("Rule 2 (refs/heads/main) now needs owner"));
    let n = srv.seen().len();
    assert_eq!(gstn(&sb, &srv, &edit("guest")).code, 2);
    assert_eq!(srv.seen().len(), n);
}

#[test]
fn move_commands_render_server_errors_without_internals() {
    for (status, code, exit) in [(403, "FG-AUTH-0003", 3), (409, "FG-REPO-0010", 5)] {
        let srv = error_server(status, code);
        let sb = Sandbox::new();
        for args in [
            vec![
                "project",
                "visibility",
                "7",
                "--visibility",
                "internal",
                "--yes",
            ],
            vec!["project", "move", "7", "--path", "x", "--yes"],
            vec!["group", "move", "3", "--top-level", "--yes"],
            vec!["user", "rename", "12", "--username", "r", "--yes"],
            vec!["namespace", "release-path", "a/b", "--yes"],
            vec![
                "protected-ref",
                "edit",
                "--project",
                "7",
                "--rule-id",
                "2",
                "--push-min-role",
                "owner",
            ],
        ] {
            let o = gstn(&sb, &srv, &args);
            assert_eq!(o.code, exit, "{args:?}: {}", o.stderr);
            assert!(o.stdout.is_empty());
            assert!(
                o.stderr.contains("操作できません。")
                    && o.stderr.contains("request_id=req-77")
                    && o.stderr.contains(code)
            );
            assert!(!o.all().contains("SELECT") && !o.all().contains(TOKEN));
        }
    }
}

#[test]
fn move_commands_document_yes_and_need_a_login() {
    let sb = Sandbox::new();
    let v = sb.run("", &["help", "--json"], "").json();
    for n in [
        "project visibility",
        "project move",
        "group move",
        "user rename",
        "namespace release-path",
    ] {
        let c = v["commands"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == n)
            .unwrap_or_else(|| panic!("{n} missing"));
        assert_eq!(c["scopes"], json!(["api"]));
        assert!(c["args"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["name"] == "--yes" && a["required"] == true));
    }
    // with --yes but no login: exit 3, no request
    let srv = Mock::start(|_| ok("{}"));
    let o = sb.run(
        &srv.url,
        &["user", "rename", "12", "--username", "r", "--yes"],
        "",
    );
    assert_eq!(o.code, 3);
    assert!(srv.seen().is_empty());
}
