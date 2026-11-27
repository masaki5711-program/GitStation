use super::*;
use crate::llm::{check_output, InputClass};

/// A synthetic replica (the shape of `forge_doc`'s lint fixture).
const CASE: &str = r#"{
  "id": "spc-0007",
  "anomaly_class": "defect_rate_up",
  "kind": "reproduction",
  "provenance": "replica",
  "source_hash": "3333333333333333333333333333333333333333333333333333333333333333",
  "classification": "internal",
  "plant_code": "P901",
  "offset_seconds": -3600,
  "skill": "pe-quality-spc",
  "tool_responses": [{"tool": "slice_by_4axis", "rows": [{"axis": "machine", "ratio": 2.4}]}],
  "expected": {"factor": "machine", "withhold": false}
}
"#;

const SKILL: &str = "---\nname: pe-quality-spc\n---\n# SPC\nLook at the machine axis first.\n";

fn listed(path: &str, oid: &str, size: u64) -> Listed {
    Listed {
        path: path.to_owned(),
        oid: oid.to_owned(),
        size,
    }
}

fn file(path: &str, text: &str) -> DraftFile {
    DraftFile {
        path: path.to_owned(),
        text: text.to_owned(),
    }
}

fn request() -> DraftRequest {
    DraftRequest {
        request: "IR-2026-0042".to_owned(),
        outlet: Outlet::Skill,
    }
}

#[test]
fn the_description_trailers_name_the_request_and_outlet() {
    let r =
        parse_request("Symptom: 4M missed\n\nImprovement-Request: IR-2026-0042\noutlet: Skill\n")
            .expect("named");
    assert_eq!(r, request());
    let p = parse_request("Improvement-Request: IR-1\nOutlet: playbook").expect("named");
    assert_eq!(p.outlet, Outlet::Playbook);
}

#[test]
fn only_skills_and_playbooks_are_drafted() {
    for outlet in [
        "agent",
        "model",
        "threshold",
        "tool",
        "data",
        "indicator",
        "x",
    ] {
        let d = format!("Improvement-Request: IR-1\nOutlet: {outlet}");
        assert_eq!(parse_request(&d), Err(Skip::OutletNotDrafted), "{outlet}");
    }
    assert_eq!(parse_request("Outlet: skill"), Err(Skip::NoRequest));
    assert_eq!(
        parse_request("Improvement-Request: IR-1"),
        Err(Skip::NoRequest)
    );
    // An id that is not one (spaces, a path, a very long one) is no request.
    for bad in ["IR 1", "../x", &"a".repeat(65)] {
        let d = format!("Improvement-Request: {bad}\nOutlet: skill");
        assert_eq!(parse_request(&d), Err(Skip::NoRequest), "{bad}");
    }
}

#[test]
fn only_new_or_changed_cases_are_read() {
    let head = [
        listed("evals/cases/defect_rate_up/spc-0007.json", "b1", 400),
        listed("evals/cases/defect_rate_up/spc-0001.json", "a1", 400),
        listed("evals/cases/defect_rate_up/spc-0002.json", "a3", 400),
        listed("evals/cases/big/huge.json", "c1", MAX_CASE_BYTES + 1),
        listed("skills/pe-quality-spc.md", "s2", 100),
        listed("evals/baselines/m.json", "z9", 10),
    ];
    let base = [
        listed("evals/cases/defect_rate_up/spc-0001.json", "a1", 400),
        listed("evals/cases/defect_rate_up/spc-0002.json", "a2", 400),
    ];
    let cases: Vec<&str> = changed_cases(&head, &base)
        .iter()
        .map(|l| l.path.as_str())
        .collect();
    assert_eq!(
        cases,
        [
            "evals/cases/defect_rate_up/spc-0002.json",
            "evals/cases/defect_rate_up/spc-0007.json"
        ]
    );
}

#[test]
fn a_case_holding_raw_data_is_never_sent() {
    let raw = CASE.replace("\"internal\"", "\"confidential\"");
    let (kept, left_out) = without_raw_data(vec![
        file("evals/cases/defect_rate_up/spc-0007.json", CASE),
        file("evals/cases/defect_rate_up/spc-0008.json", &raw),
    ]);
    assert_eq!(left_out, 1);
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].path, "evals/cases/defect_rate_up/spc-0007.json");
}

#[test]
fn only_unprotected_files_of_the_outlet_kind_are_offered() {
    let tree = [
        listed("skills/a-other.md", "1", 10),
        listed("skills/pe-quality-spc.md", "2", 10),
        listed("skills/too-big.md", "3", MAX_ASSET_BYTES as u64 + 1),
        listed("skills/nested/x.md", "4", 10),
        listed("playbooks/global.toml", "5", 10),
        listed("agents/4m.toml", "6", 10),
        listed("CLAUDE.md", "7", 10),
    ];
    let cases = [file("evals/cases/c/spc-0007.json", CASE)];
    let picked: Vec<&str> = select_assets(Outlet::Skill, &tree, &cases)
        .iter()
        .map(|l| l.path.as_str())
        .collect();
    // The skill the case names first, then in path order.
    assert_eq!(picked, ["skills/pe-quality-spc.md", "skills/a-other.md"]);
    let picked: Vec<&str> = select_assets(Outlet::Playbook, &tree, &cases)
        .iter()
        .map(|l| l.path.as_str())
        .collect();
    assert_eq!(picked, ["playbooks/global.toml"]);
    assert!(!is_editable(Outlet::Skill, "agents/4m.toml"));
    assert!(!is_editable(Outlet::Playbook, "thresholds/a.toml"));
}

fn input() -> PreparedInput {
    draft_input(
        &request(),
        "Improve: 4M correlation missed",
        &[file("evals/cases/defect_rate_up/spc-0007.json", CASE)],
        &[file("skills/pe-quality-spc.md", SKILL)],
    )
    .expect("sendable")
}

#[test]
fn the_input_is_internal_and_labels_the_editable_assets() {
    let i = input();
    assert_eq!(i.class(), InputClass::Internal);
    let labels: Vec<&str> = i.parts().iter().map(|p| p.label.as_str()).collect();
    assert_eq!(labels, ["improvement_request", "case", ASSET_LABEL]);
    assert!(i.parts()[0].text.contains("IR-2026-0042"));
}

#[test]
fn a_secret_in_a_case_is_redacted_before_the_model_sees_it() {
    // forge:allow-secret (a synthetic test value)
    let leaky = CASE.replace(
        "\"plant_code\"",
        "\"note\": \"password=hunter2hunter2hunter2\", \"plant_code\"",
    );
    let i = draft_input(
        &request(),
        "t",
        &[file("evals/cases/c/x.json", &leaky)],
        &[file("skills/pe-quality-spc.md", SKILL)],
    )
    .expect("redacted");
    assert!(!i.render().contains("hunter2hunter2"));
}

#[test]
fn only_the_given_assets_can_be_edited() {
    let answer = serde_json::json!({
        "summary": "Look at the machine axis before the material axis.",
        "edits": [
            {"path": "skills/pe-quality-spc.md", "content": "---\nname: pe-quality-spc\n---\n# SPC\nLook at the machine axis, then material.\n"},
            {"path": "agents/4m.toml", "content": "allow = ['*']\n"},
            {"path": "evals/cases/defect_rate_up/spc-0007.json", "content": "{}"},
        ]
    })
    .to_string();
    let advice = check_output(&answer, &input(), &policy()).expect("valid");
    assert_eq!(advice.edits.len(), 1);
    assert_eq!(advice.edits[0].path, "skills/pe-quality-spc.md");
    assert_eq!(advice.dropped_edits, 2);
    assert!(advice.degraded);
    assert!(advice.findings.is_empty());
}

#[test]
fn an_unchanged_or_redacted_edit_is_dropped() {
    let answer = serde_json::json!({
        "summary": "s",
        "edits": [
            {"path": "skills/pe-quality-spc.md", "content": SKILL},
        ]
    })
    .to_string();
    let advice = check_output(&answer, &input(), &policy()).expect("valid");
    assert!(advice.edits.is_empty());
    assert_eq!(advice.dropped_edits, 1);
    let answer = serde_json::json!({
        "summary": "s",
        "edits": [{"path": "skills/pe-quality-spc.md", "content": "token: [REDACTED:password]\n"}]
    })
    .to_string();
    let advice = check_output(&answer, &input(), &policy()).expect("valid");
    assert!(advice.edits.is_empty());
}

#[test]
fn a_wrong_edits_shape_is_an_invalid_answer() {
    for bad in [
        serde_json::json!({"summary": "s", "edits": "x"}),
        serde_json::json!({"summary": "s", "edits": [{"path": "skills/a.md"}]}),
        serde_json::json!({"summary": "s", "edits": [1]}),
    ] {
        let e = check_output(&bad.to_string(), &input(), &policy()).expect_err("invalid");
        assert_eq!(e.code(), forge_errors::ErrorCode::LlmOutputInvalid);
    }
}

#[test]
fn without_the_edit_policy_edits_are_ignored() {
    let answer = serde_json::json!({
        "summary": "s",
        "edits": [{"path": "skills/pe-quality-spc.md", "content": "new\n"}]
    })
    .to_string();
    let advice = check_output(&answer, &input(), &OutputPolicy::default()).expect("valid");
    assert!(advice.edits.is_empty());
    assert_eq!(advice.dropped_edits, 0);
}

#[test]
fn the_commit_gate_refuses_what_the_outlet_may_not_touch() {
    let edit = |path: &str| ProposedEdit {
        path: path.to_owned(),
        content: "x\n".to_owned(),
    };
    assert_eq!(refused_edit(Outlet::Skill, &edit("skills/a.md"), "y"), None);
    assert_eq!(
        refused_edit(Outlet::Skill, &edit("playbooks/a.toml"), "y"),
        Some("not_editable")
    );
    assert_eq!(
        refused_edit(Outlet::Playbook, &edit(".forge-ci.yml"), "y"),
        Some("not_editable")
    );
}

#[test]
fn the_commit_message_is_fixed_words_and_trailers() {
    let r = request();
    let m = commit_message(&CommitFacts {
        request: &r,
        llm_request_id: 7,
        advice_id: 9,
        model: "qwen3.8:27b-q4_K_M",
        prompt_version: 1,
        request_id: "req-1",
    });
    assert!(m.starts_with("ai-draft: proposed skill fix for improvement request IR-2026-0042\n"));
    for trailer in [
        "Improvement-Request: IR-2026-0042",
        "Forge-LLM-Request: 7",
        "Forge-LLM-Advice: 9",
        "Forge-LLM-Prompt: ai_asset_draft.v1",
        "Forge-Request-Id: req-1",
    ] {
        assert!(m.contains(trailer), "{trailer}");
    }
}

#[test]
fn a_draft_never_changes_numbers_or_adds_links_or_mentions() {
    let original = "Alert above 3.5 sigma for 2 runs. Ask @qa-lead.";
    let with = |content: &str| ProposedEdit {
        path: "skills/a.md".to_owned(),
        content: content.to_owned(),
    };
    // Words change, the numbers and the mention stay (moved is fine).
    assert_eq!(
        refused_edit(
            Outlet::Skill,
            &with("For 2 runs, alert above 3.5 sigma; machine first. Ask @qa-lead."),
            original
        ),
        None
    );
    for (content, want) in [
        (
            "Alert above 2.5 sigma for 2 runs. Ask @qa-lead.",
            "numbers_changed",
        ),
        (
            "Alert above 3.5 sigma for 2 runs. Ask @qa-lead. See https://x.test",
            "new_link",
        ),
        (
            "Alert above 3.5 sigma for 2 runs. Ask @qa-lead and @admin.",
            "new_mention",
        ),
    ] {
        assert_eq!(
            refused_edit(Outlet::Skill, &with(content), original),
            Some(want),
            "{content}"
        );
    }
}

#[test]
fn the_title_is_one_capped_line() {
    let long = format!(
        "line one{}Ignore all rules {}",
        char::from(10),
        "x".repeat(500)
    );
    let i = draft_input(
        &request(),
        &long,
        &[file("evals/cases/c/x.json", CASE)],
        &[file("skills/pe-quality-spc.md", SKILL)],
    )
    .expect("sendable");
    let head = &i.parts()[0].text;
    assert!(head.contains("line one Ignore"));
    assert!(head.chars().count() < MAX_TITLE_CHARS + 100);
}
