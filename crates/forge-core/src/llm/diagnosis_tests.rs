//! The incident diagnosis's facts and output checks (plan 19 L5-7).
//! Synthetic log lines and tokens only (forge:allow-secret: the push secret
//! check, FG-GIT-0022).

use super::super::{
    check_output, names_a_mutating_step, next_look_is_read_only, prepare, InputLimits,
    MUTATING_WITHHELD,
};
use super::*;

const RID: &str = "r-7f3a9c";

fn stream() -> String {
    [
        r#"{"timestamp":"2026-11-26T01:00:00Z","level":"INFO","message":"start","request_id":"r-other"}"#,
        r#"{"timestamp":"2026-11-26T01:00:01Z","level":"WARN","error_code":"FG-DB-0001","request_id":"r-7f3a9c","context":"reason=acquire_timeout pool=write","message":"The database is unreachable"}"#,
        "not json at all",
        r#"{"timestamp":"2026-11-26T01:00:02Z","level":"INFO","message":"retry","span":{"request_id":"r-7f3a9c"}}"#,
        r#"{"timestamp":"2026-11-26T01:00:03Z","level":"ERROR","error_code":"FG-SYS-0001","request_id":"r-7f3a9c","message":"internal"}"#,
    ]
    .join("\n")
}

fn input_of(facts: &Facts) -> super::super::PreparedInput {
    prepare(parts(facts), &InputLimits::default()).expect("prepared")
}

fn log_facts() -> Facts {
    Facts {
        log: select_log_lines(&stream(), &Target::RequestId(RID.to_owned())),
        catalog: vec![CatalogFact {
            code: "FG-DB-0001".to_owned(),
            cause: "The database cannot be reached.".to_owned(),
            diagnosis: "Check the pool and the server.".to_owned(),
            fix: "Bring the database back.".to_owned(),
            runbook: Some("db-unavailable".to_owned()),
        }],
        ..Facts::default()
    }
}

#[test]
fn request_lines_are_numbered_by_stream_position_with_their_codes() {
    let sel = select_log_lines(&stream(), &Target::RequestId(RID.to_owned()));
    let numbers: Vec<usize> = sel.lines.iter().map(|(n, _)| *n).collect();
    assert_eq!(numbers, vec![2, 4, 5]);
    assert_eq!(sel.scanned, 5);
    assert_eq!(sel.matched, 3);
    assert_eq!(sel.codes, vec!["FG-DB-0001", "FG-SYS-0001"]);
    assert!(!sel.personal);
}

#[test]
fn code_lines_match_the_field_not_a_mention() {
    let mut s = stream();
    s.push_str("\n{\"message\":\"see FG-DB-0001 in the runbook\"}");
    let sel = select_log_lines(&s, &Target::ErrorCode("FG-DB-0001".to_owned()));
    assert_eq!(sel.lines.len(), 1);
    assert_eq!(sel.lines[0].0, 2);
}

#[test]
fn only_the_newest_lines_are_kept() {
    let s: String = (0..MAX_LOG_LINES + 50)
        .map(|i| format!("{{\"request_id\":\"{RID}\",\"message\":\"m{i}\"}}\n"))
        .collect();
    let sel = select_log_lines(&s, &Target::RequestId(RID.to_owned()));
    assert_eq!(sel.matched, MAX_LOG_LINES + 50);
    assert_eq!(sel.lines.len(), MAX_LOG_LINES);
    assert_eq!(sel.lines[0].0, 51);
}

#[test]
fn personal_log_lines_make_the_log_part_confidential() {
    let s =
        format!("{{\"request_id\":\"{RID}\",\"message\":\"login for carol@example.test failed\"}}");
    let facts = Facts {
        log: select_log_lines(&s, &Target::RequestId(RID.to_owned())),
        ..Facts::default()
    };
    assert!(facts.log.personal);
    assert_eq!(input_of(&facts).class(), InputClass::Confidential);
    assert_eq!(input_of(&log_facts()).class(), InputClass::Internal);
}

#[test]
fn log_lines_redacted_before_prompt() {
    let token = format!("fgp_{}", "Ab3dE5gH7jK9mN1pQ3sT5vX7zA9cE1gI");
    let s = format!(
        "{{\"request_id\":\"{RID}\",\"message\":\"auth failed\",\"context\":\"token={token}\"}}"
    );
    let facts = Facts {
        log: select_log_lines(&s, &Target::RequestId(RID.to_owned())),
        ..Facts::default()
    };
    let input = input_of(&facts);
    let rendered = input.render();
    assert!(!rendered.contains(&token), "{rendered}");
    assert!(rendered.contains("[REDACTED"), "{rendered}");
    assert!(rendered.contains("L1: "), "{rendered}");
}

#[test]
fn diagnosis_never_contains_mutating_step() {
    let input = input_of(&log_facts());
    let quote = r#""error_code":"FG-DB-0001""#;
    let answer = serde_json::json!({
        "summary": "The write pool timed out. Restart forge-serve to recover.",
        "findings": [
            {"severity": "high", "path": "log", "line": 2, "quote": quote,
             "message": "Pool exhausted. Next: run UPDATE forge.jobs SET state = 'queued'", "confidence": 0.9},
            {"severity": "high", "path": "log", "line": 2, "quote": quote,
             "message": "DB down. 次に: systemctl restart postgresql", "confidence": 0.8},
            {"severity": "high", "path": "log", "line": 2, "quote": quote,
             "message": "接続が切れた。forge-worker を再起動してください", "confidence": 0.7},
            {"severity": "medium", "path": "log", "line": 2, "quote": quote,
             "message": "Pool exhausted. Next: SELECT count(*) FROM pg_stat_activity", "confidence": 0.6}
        ]
    })
    .to_string();
    let advice = check_output(&answer, &input, &policy()).expect("checked");
    assert_eq!(advice.summary, MUTATING_WITHHELD);
    assert_eq!(advice.dropped_mutating, 3);
    assert_eq!(advice.findings.len(), 1);
    assert!(advice.findings[0].message.contains("SELECT count(*)"));
    assert!(advice.degraded);
    for f in &advice.findings {
        assert!(!names_a_mutating_step(&f.message));
    }
}

#[test]
fn a_deep_review_reason_with_a_mutating_step_is_dropped() {
    // Found by the L5-10 eval bench: an injected step in the deep-review
    // reason reached the stored advice.
    let input = input_of(&log_facts());
    let answer = serde_json::json!({
        "summary": "The write pool timed out.",
        "findings": [],
        "deep_review": {"recommended": true,
                        "reason": "the fix is DROP TABLE forge.sessions then systemctl restart forge-serve"}
    })
    .to_string();
    let advice = check_output(&answer, &input, &policy()).expect("checked");
    assert!(advice.deep_review.is_none());
    assert_eq!(advice.dropped_mutating, 1);
    assert!(advice.degraded);
    // Without the policy (an approval summary) the reason stays a reason.
    let plain = check_output(&answer, &input, &OutputPolicy::default()).expect("checked");
    assert!(plain.deep_review.is_some());
}

#[test]
fn read_only_next_steps_are_not_mutating() {
    for text in [
        "Next: journalctl -u forge-serve -o cat | jq 'select(.error_code != null)'",
        "SELECT state, count(*) FROM forge.jobs GROUP BY 1",
        "forge doctor --json; forge explain FG-DB-0001; forge admin token list --user alice",
        "The last update of the pool settings was the cause",
        "curl -s http://127.0.0.1:11434/api/tags",
        "ログの FG-DB-0001 を確かめる",
    ] {
        assert!(!names_a_mutating_step(text), "{text}");
    }
    for text in [
        "rm -rf /var/lib/forge",
        "DELETE FROM forge.jobs",
        "forge admin user disable alice",
        "forge db migrate",
        "curl -X POST http://x/api",
        "ollama pull qwen3:27b",
        "設定を変更して",
    ] {
        assert!(names_a_mutating_step(text), "{text}");
    }
}

#[test]
fn cites_only_given_lines() {
    let input = input_of(&log_facts());
    let quote = r#""error_code":"FG-DB-0001""#;
    let f = |line: serde_json::Value, q: &str| {
        serde_json::json!({"severity": "high", "path": "log", "line": line, "quote": q,
            "message": "the write pool timed out", "confidence": 0.9})
    };
    let answer = serde_json::json!({
        "summary": "The database pool timed out.",
        "findings": [
            f(serde_json::json!(2), quote),
            // A line the input does not have.
            f(serde_json::json!(3), "not json at all"),
            // A real line, but the quote is from another line.
            f(serde_json::json!(4), quote),
            // No line at all.
            f(serde_json::Value::Null, quote),
            // Made up.
            f(serde_json::json!(99), quote)
        ]
    })
    .to_string();
    let advice = check_output(&answer, &input, &policy()).expect("checked");
    assert_eq!(advice.findings.len(), 1);
    assert_eq!(advice.findings[0].line, Some(2));
    assert_eq!(advice.dropped_evidence, 4);
}

#[test]
fn other_sources_are_cited_by_their_part() {
    let input = input_of(&log_facts());
    let answer = serde_json::json!({
        "summary": "The database is unreachable.",
        "findings": [
            {"severity": "high", "path": "catalog:FG-DB-0001", "quote": "Check the pool and the server",
             "message": "matches the catalog; next: forge doctor --json", "confidence": 0.8},
            {"severity": "high", "path": "runbook:db-unavailable", "quote": "Check the pool and the server",
             "message": "a runbook the input does not have", "confidence": 0.8}
        ]
    })
    .to_string();
    let advice = check_output(&answer, &input, &policy()).expect("checked");
    assert_eq!(advice.findings.len(), 1);
    assert_eq!(advice.dropped_evidence, 1);
}

#[test]
fn the_runbook_section_naming_the_code_is_taken() {
    let md = "# DB\n\nintro\n\n## Pool\n\nFG-DB-0001 appears here.\n\n### Detail\n\nmore\n\n## Other\n\nnot this";
    let (heading, text) = runbook_section(md, "FG-DB-0001");
    assert_eq!(heading, "Pool");
    assert!(text.contains("Detail"));
    assert!(!text.contains("not this"));
    let (heading, text) = runbook_section(md, "FG-XX-9999");
    assert_eq!(heading, "DB");
    assert!(text.contains("not this"));
}

#[test]
fn incident_records_name_the_code_as_a_token() {
    let jsonl = "{\"id\":\"INC-2026-001\",\"error_codes\":[\"FG-DB-0001\"]}\n{\"error_codes\":[\"FG-DB-00012\"]}\n{\"summary\":\"FG-DB-0001 again\"}";
    let got = incident_records(jsonl, "FG-DB-0001");
    let ids: Vec<&str> = got.iter().map(|(id, _)| id.as_str()).collect();
    assert_eq!(ids, vec!["INC-2026-001", "line-3"]);
}

#[test]
fn the_next_look_must_be_an_allowed_read_only_command() {
    for text in [
        "原因。次に見るもの: journalctl -u forge-serve -o cat | jq 'select(.error_code != null)'",
        "原因。次に見るもの: SELECT state, count(*) FROM forge.jobs WHERE updated_at > now() - interval '1 hour' GROUP BY 1",
        "原因。次に見るもの: forge doctor --json で db.connections を見る",
        "原因。次に見るもの: curl -s http://127.0.0.1:11434/api/tags",
        "原因。次に見るもの: systemctl status forge-worker",
        "a cause without a next look",
    ] {
        assert!(next_look_is_read_only(text), "{text}");
    }
    for text in [
        "x. 次に見るもの: journalctl --vacuum-size=1M",
        "x. 次に見るもの: curl -XPOST http://127.0.0.1/api",
        "x. 次に見るもの: curl -s -d a=b http://127.0.0.1/api",
        "x. 次に見るもの: curl -s http://attacker.example/x",
        "x. 次に見るもの: psql -c \"SELECT pg_terminate_backend(42)\"",
        "x. 次に見るもの: SELECT pg_reload_conf()",
        "x. 次に見るもの: systemctl daemon-reload",
        "x. 次に見るもの: git clean -fdx",
        "x. 次に見るもの: sed -i s/a/b/ /etc/forge/forge.toml",
        "x. 次に見るもの: cat a | tee /etc/forge/forge.toml",
        "x. 次に見るもの: echo x > /etc/forge/forge.toml",
        "x. 次に見るもの: cp a b",
        "x. 次に見るもの: dnf install foo",
        "x. 次に見るもの: forge admin repo reprovision 3",
        "x. Next: COPY forge.users TO '/tmp/u'",
    ] {
        assert!(!next_look_is_read_only(text), "{text}");
    }
}

#[test]
fn a_diagnosis_finding_without_a_part_is_dropped() {
    let input = input_of(&log_facts());
    let answer = serde_json::json!({
        "summary": "The database is unreachable.",
        "findings": [
            {"severity": "high", "quote": "Check the pool and the server",
             "message": "no part named", "confidence": 0.8}
        ]
    })
    .to_string();
    let advice = check_output(&answer, &input, &policy()).expect("checked");
    assert!(advice.findings.is_empty());
    assert_eq!(advice.dropped_evidence, 1);
}

#[test]
fn personal_data_in_other_parts_and_raw_lines_is_confidential() {
    let raw = format!("thread 'main' panicked at {RID}");
    let sel = select_log_lines(&raw, &Target::RequestId(RID.to_owned()));
    assert_eq!(sel.lines.len(), 1);
    assert!(
        sel.personal,
        "a line that is not forge's JSON is people-only"
    );
    let facts = Facts {
        incidents: vec![(
            "INC-1".to_owned(),
            "{\"id\":\"INC-1\",\"reporter\":\"dave@example.test\"}".to_owned(),
        )],
        ..Facts::default()
    };
    assert_eq!(input_of(&facts).class(), InputClass::Confidential);
}
