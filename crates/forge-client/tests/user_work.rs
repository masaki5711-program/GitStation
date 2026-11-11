//! End-to-end tests of `gstn mr mine` (plan 15 MR 6) and `gstn activity`
//! (plan 15 MR 8): the caller's own work across projects, against a
//! scripted loopback server. All data is synthetic.

#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly

mod common;
use common::{Mock, Out, Resp, Sandbox, TOKEN};
use serde_json::json;

fn gstn(sb: &Sandbox, srv: &Mock, args: &[&str]) -> Out {
    sb.run_env(&srv.url, args, "", &[("GSTN_TOKEN", TOKEN)])
}

fn mr_page() -> String {
    json!({
        "scope": "approvable",
        "merge_requests": [{
            "project_id": 7, "project_path": "acme/web", "iid": 3,
            "title": "Add the login form", "state": "open", "draft": false,
            "source_branch": "feature/login", "target_branch": "main",
            "author_id": 2, "author": {"username": "dave", "display_name": "Dave"},
            "head_sha": null, "attention": [],
            "created_at": "2026-11-01T00:00:00Z", "updated_at": "2026-11-01T00:00:00Z"
        }],
        "next_before": 41
    })
    .to_string()
}

#[test]
fn mr_mine_asks_for_one_scope_page() {
    let srv = Mock::start(|_| Resp::json(200, &mr_page()));
    let sb = Sandbox::new();
    let o = gstn(
        &sb,
        &srv,
        &[
            "mr",
            "mine",
            "--scope",
            "approvable",
            "--limit",
            "5",
            "--before",
            "42",
            "--json",
        ],
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    let seen = srv.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(
        (seen[0].method.as_str(), seen[0].target.as_str()),
        (
            "GET",
            "/api/v1/user/merge-requests?scope=approvable&limit=5&before=42"
        )
    );
    assert_eq!(o.json()["merge_requests"][0]["iid"], 3);
    let h = gstn(&sb, &srv, &["mr", "mine", "--scope", "approvable"]);
    assert_eq!(h.code, 0, "{}", h.stderr);
    assert!(
        h.stdout.contains("acme/web") && h.stdout.contains("more: --before 41"),
        "{}",
        h.stdout
    );
}

#[test]
fn mr_mine_counts_reads_the_counts_route() {
    let count = |n: i64, capped: bool| json!({"count": n, "capped": capped, "truncated": false});
    let body = json!({
        "authored": count(2, false),
        "approvable": count(100, true),
        "attention": count(0, false)
    })
    .to_string();
    let srv = Mock::start(move |_| Resp::json(200, &body));
    let sb = Sandbox::new();
    let o = gstn(&sb, &srv, &["mr", "mine", "--counts"]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    let seen = srv.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(
        (seen[0].method.as_str(), seen[0].target.as_str()),
        ("GET", "/api/v1/user/merge-requests/counts")
    );
    assert!(
        o.stdout.contains("authored: 2") && o.stdout.contains("approvable: 100+"),
        "{}",
        o.stdout
    );
}

#[test]
fn mr_mine_needs_exactly_one_of_scope_and_counts_before_any_request() {
    let srv = Mock::start(|_| Resp::json(200, "{}"));
    let sb = Sandbox::new();
    for args in [
        &["mr", "mine"][..],
        &["mr", "mine", "--scope", "authored", "--counts"],
        &["mr", "mine", "--scope", "mine"],
        &["mr", "mine", "--counts", "--limit", "5"],
    ] {
        let o = gstn(&sb, &srv, args);
        assert_eq!(o.code, 2, "{args:?}: {}", o.stderr);
    }
    assert!(srv.seen().is_empty());
}

#[test]
fn activity_reads_one_page_of_the_own_feed() {
    let body = json!({
        "events": [
            {
                "seq": 9, "type": "merge_request.opened",
                "occurred_at": "2026-11-01T00:00:00.000Z",
                "project_id": 7, "project_path": "acme/web",
                "actor": {"username": "dave", "display_name": "Dave"},
                "merge_request": {"iid": 3, "title": "Add the login form"},
                "ref_count": null
            },
            {
                "seq": 8, "type": "repository.pushed",
                "occurred_at": "2026-10-31T00:00:00.000Z",
                "project_id": 7, "project_path": "acme/web",
                "actor": null, "merge_request": null, "ref_count": 2
            }
        ],
        "next_before": 8,
        "truncated": true
    })
    .to_string();
    let srv = Mock::start(move |_| Resp::json(200, &body));
    let sb = Sandbox::new();
    let o = gstn(&sb, &srv, &["activity", "--before", "10", "--limit", "2"]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    let seen = srv.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(
        (seen[0].method.as_str(), seen[0].target.as_str()),
        ("GET", "/api/v1/user/activity?before=10&limit=2")
    );
    assert!(
        o.stdout.contains("merge_request.opened  acme/web!3  dave")
            && o.stdout.contains("repository.pushed  acme/web  -")
            && o.stdout.contains("more: --before 8")
            && o.stdout.contains("truncated"),
        "{}",
        o.stdout
    );
    let j = gstn(&sb, &srv, &["activity", "--json"]);
    assert_eq!(j.code, 0, "{}", j.stderr);
    assert_eq!(j.json()["events"][0]["seq"], 9);
    assert_eq!(srv.seen()[1].target, "/api/v1/user/activity");
}
