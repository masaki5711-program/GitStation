//! End-to-end tests of `gstn deploy summary` (M5 L5-8) against a scripted
//! loopback server: the route it calls, every fact listed in the text
//! whatever the summary says, and the summary labelled as advice. All data
//! is synthetic.

#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly

mod common;
use common::{Mock, Out, Resp, Sandbox, TOKEN};

fn gstn(sb: &Sandbox, srv: &Mock, args: &[&str]) -> Out {
    sb.run_env(&srv.url, args, "", &[("GSTN_TOKEN", TOKEN)])
}

const SUMMARY: &str = r#"{"deployment":{"id":12,"environment":"prod","sha":"cccccccccccccccccccccccccccccccccccccccc","status":"pending_approval","requested_by":3,"approved_by":null,"approved_at":null,"approval_expires_at":null,"previous_sha":null,"error_code":null,"failed_target":null,"created_at":"2026-11-26T00:00:00Z","started_at":null,"finished_at":null,"commit_subject":"Add orders","commit_author_name":"Carol","commit_author_email":"carol@example.test","approvals":0},"facts":{"base_sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","range":"read","range_error_code":null,"commits":2,"commits_truncated":false,"merge_requests":[{"iid":5,"title":"Add the order table","risk":"neutral"}],"migrations":["db/migrations/0002_orders.sql"],"protected_paths":[],"ci_config":[".forge-ci.yml"],"permissions":[],"lists_truncated":false,"risk":null,"failed_checks":[{"name":"ci/pipeline","conclusion":"failure"}],"freeze_windows":[],"frozen_now":false},"summary":{"state":"shown","label":"AI の助言（提案）","text":"カートの表示を直します。","note":null,"stale":true,"stale_reason":"newer_commit","degraded":false,"input_class":"internal","advice_id":9,"model":"qwen3:27b","prompt_version":1,"created_at":"2026-11-26T00:00:00Z"}}"#;

#[test]
fn deploy_summary_lists_every_fact_and_labels_the_summary() {
    let srv = Mock::start(|_| Resp::json(200, SUMMARY));
    let sb = Sandbox::new();
    let o = gstn(&sb, &srv, &["deploy", "summary", "--project", "7", "12"]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    let r = srv.seen().pop().expect("a request");
    assert_eq!(
        (r.method.as_str(), r.target.as_str()),
        ("GET", "/api/v1/projects/7/deployments/12/approval-summary")
    );
    // The summary leaves the migration and the CI change out; the facts
    // still list them.
    for needle in [
        "db/migrations/0002_orders.sql",
        ".forge-ci.yml",
        "!5 Add the order table",
        "ci/pipeline",
        "AI の助言（提案） (stale: newer_commit): カートの表示を直します。",
    ] {
        assert!(o.stdout.contains(needle), "{needle} missing:\n{}", o.stdout);
    }
    let j = gstn(
        &sb,
        &srv,
        &["deploy", "summary", "--project", "7", "12", "--json"],
    )
    .json();
    assert_eq!(j["facts"]["migrations"][0], "db/migrations/0002_orders.sql");
}

#[test]
fn deploy_summary_sends_nothing_for_a_bad_id() {
    let srv = Mock::start(|_| Resp::json(200, SUMMARY));
    let sb = Sandbox::new();
    let o = gstn(&sb, &srv, &["deploy", "summary", "--project", "7", "x"]);
    assert_ne!(o.code, 0);
    assert!(srv.seen().is_empty(), "nothing is sent");
}
