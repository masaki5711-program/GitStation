#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)] // tests may fail loudly

//! The AI asset lint (plan 17 P17-3, ADR-0035) on synthetic assets only.

use super::*;

const SKILL_SPC: &str = "---
name: pe-quality-spc
description: 不良率の上昇が偶然か原因のある変化かを判断する
axis: cross
anomaly_class: [defect_rate_up, spc_run_up]
indicators: [A1]
required_tools: [slice_by_4axis, compute_4m_correlation]
required_profile: core
reviewer: 生産技術（品質）
version: \"1.0\"
valid_from: 2026-08-16
---

# 本文
";

const SKILL_CHATTER: &str = "---
name: pe-machining-chatter
description: びびりの判定
axis: process
process_kind: 切削
anomaly_class: [defect_rate_up]
required_tools: [raw_waveform_query]
required_profile: full
reviewer: 生産技術（加工）
version: \"1.0\"
valid_from: 2026-08-16
---
";

const AGENT_4M: &str = "name = \"4m-correlator\"
allowed_tools = [\"compute_4m_correlation\", \"slice_by_4axis\", \"raw_waveform_query\"]
required_profile = \"core\"
";

const TOOLS: &str = "[[tool]]
name = \"slice_by_4axis\"
required_profile = \"core\"
[[tool]]
name = \"compute_4m_correlation\"
required_profile = \"core\"
[[tool]]
name = \"raw_waveform_query\"
required_profile = \"full\"
[[tool]]
name = \"edge_health\"
required_profile = \"forwarder\"
";

const PLAYBOOK: &str = "[[playbook]]
indicator_code = \"A1\"
plant_code = \"P900\"
scope_type = \"global\"
scope_key = \"*\"
severity_filter = \"*\"
valid_from = \"2026-08-16\"
priority = 10
agent_name = \"4m-correlator\"
skill_refs = [\"pe-quality-spc\"]
tool_sequence = [\"slice_by_4axis\", \"compute_4m_correlation\"]
max_tool_calls = 8
required_profile = \"core\"
";

const MODELS: &str =
    "small = \"qwen3:4b@sha256:1111111111111111111111111111111111111111111111111111111111111111\"
medium = \"qwen3:14b@sha256:2222222222222222222222222222222222222222222222222222222222222222\"
";

const CASE: &str = r#"{
  "id": "spc-0001",
  "anomaly_class": "defect_rate_up",
  "kind": "reproduction",
  "provenance": "replica",
  "source_hash": "3333333333333333333333333333333333333333333333333333333333333333",
  "classification": "internal",
  "plant_code": "P901",
  "offset_seconds": -3600,
  "tool_responses": [{"tool": "slice_by_4axis", "rows": [{"axis": "machine", "ratio": 2.4}]}],
  "expected": {"factor": "machine", "withhold": false}
}
"#;

fn set<'a>(extra: &[(&'a str, &'a str)]) -> Vec<(&'a str, &'a str)> {
    let mut files = vec![
        ("skills/pe-quality-spc.md", SKILL_SPC),
        ("agents/4m-correlator.toml", AGENT_4M),
        ("tools/tools.toml", TOOLS),
        ("playbooks/global.toml", PLAYBOOK),
        ("models/models.toml", MODELS),
        ("evals/cases/defect_rate_up/spc-0001.json", CASE),
        ("thresholds/proposals/a1-run.md", "# 提案\n"),
        ("README.md", "anything\n"),
    ];
    for (path, text) in extra {
        files.retain(|(p, _)| p != path);
        files.push((path, text));
    }
    files
}

fn run(files: &[(&str, &str)]) -> Vec<Problem> {
    let assets: Vec<AssetFile<'_>> = files
        .iter()
        .map(|(path, text)| AssetFile {
            path,
            bytes: text.as_bytes(),
        })
        .collect();
    lint(&assets)
}

fn rules(problems: &[Problem]) -> Vec<&'static str> {
    problems.iter().map(|p| p.rule).collect()
}

#[test]
fn a_consistent_asset_set_has_no_problem() {
    let problems = run(&set(&[]));
    assert!(problems.is_empty(), "{problems:#?}");
}

#[test]
fn model_latest_rejected() {
    for (models, rule) in [
        ("small = \"qwen3:latest@sha256:1111111111111111111111111111111111111111111111111111111111111111\"\n", "model_latest"),
        ("small = \"qwen3\"\n", "model_latest"),
        ("small = \"qwen3:14b\"\n", "model_digest_missing"),
        ("small = \"qwen3:14b@sha256:abc\"\n", "model_digest_missing"),
    ] {
        let problems = run(&set(&[("models/models.toml", models)]));
        assert_eq!(rules(&problems), vec![rule], "{models}: {problems:#?}");
        assert_eq!(problems[0].subject.as_deref(), Some("small"));
    }
    assert_eq!(
        model_ref_problem(
            "qwen3:14b@sha256:2222222222222222222222222222222222222222222222222222222222222222"
        ),
        None
    );
}

#[test]
fn playbook_unknown_skill_rejected() {
    let book = PLAYBOOK.replace(
        "[\"pe-quality-spc\"]",
        "[\"pe-quality-spc\", \"pe-no-such\"]",
    );
    let problems = run(&set(&[("playbooks/global.toml", &book)]));
    assert_eq!(rules(&problems), vec!["skill_unknown"], "{problems:#?}");
    assert_eq!(problems[0].path, "playbooks/global.toml");
    assert!(problems[0]
        .subject
        .as_deref()
        .unwrap()
        .contains("pe-no-such"));
}

#[test]
fn a_playbook_tool_outside_the_agents_allowlist_is_rejected() {
    let agent = AGENT_4M.replace(", \"slice_by_4axis\"", "");
    let problems = run(&set(&[("agents/4m-correlator.toml", &agent)]));
    // The sequence names it, and the skill needs it.
    assert_eq!(
        rules(&problems),
        vec!["tool_not_allowed", "tool_not_allowed"],
        "{problems:#?}"
    );
}

#[test]
fn a_playbook_whose_edge_profile_is_below_a_skill_or_tool_is_rejected() {
    let book = PLAYBOOK
        .replace("scope_type = \"global\"", "scope_type = \"process_kind\"")
        .replace("scope_key = \"*\"", "scope_key = \"切削\"")
        .replace("[\"pe-quality-spc\"]", "[\"pe-machining-chatter\"]")
        .replace("\"compute_4m_correlation\"]", "\"raw_waveform_query\"]");
    let files = set(&[
        ("playbooks/global.toml", &book),
        ("skills/pe-machining-chatter.md", SKILL_CHATTER),
    ]);
    let problems = run(&files);
    let subjects: Vec<&str> = problems
        .iter()
        .filter_map(|p| p.subject.as_deref())
        .collect();
    assert_eq!(
        rules(&problems),
        vec!["profile_too_low", "profile_too_low"],
        "{problems:#?}"
    );
    assert!(subjects
        .iter()
        .any(|s| s.contains("skill pe-machining-chatter")));
    assert!(subjects
        .iter()
        .any(|s| s.contains("tool raw_waveform_query")));
    // On a `full` edge it is consistent.
    let full = book.replace("required_profile = \"core\"", "required_profile = \"full\"");
    let problems = run(&set(&[
        ("playbooks/global.toml", &full),
        ("skills/pe-machining-chatter.md", SKILL_CHATTER),
    ]));
    assert!(problems.is_empty(), "{problems:#?}");
}

#[test]
fn a_process_skill_in_a_global_playbook_is_rejected() {
    let book = PLAYBOOK
        .replace("[\"pe-quality-spc\"]", "[\"pe-machining-chatter\"]")
        .replace("required_profile = \"core\"", "required_profile = \"full\"");
    let problems = run(&set(&[
        ("playbooks/global.toml", &book),
        ("skills/pe-machining-chatter.md", SKILL_CHATTER),
    ]));
    assert!(
        rules(&problems).contains(&"skill_process_mismatch"),
        "{problems:#?}"
    );
}

#[test]
fn skill_front_matter_is_checked_key_by_key() {
    let bad = SKILL_SPC
        .replace("name: pe-quality-spc", "name: pe-other")
        .replace("required_profile: core", "required_profile: huge")
        .replace("valid_from: 2026-08-16", "valid_from: soon\nowner: x")
        .replace("reviewer: 生産技術（品質）\n", "");
    let problems = run(&set(&[("skills/pe-quality-spc.md", &bad)]));
    let got = rules(&problems);
    for want in [
        "skill_name_mismatch",
        "profile_invalid",
        "date_invalid",
        "key_unknown",
        "field_missing",
    ] {
        assert!(got.contains(&want), "{want}: {problems:#?}");
    }
    let none = run(&set(&[("skills/pe-quality-spc.md", "# no front matter\n")]));
    assert!(rules(&none).contains(&"front_matter_missing"), "{none:#?}");
    let process = SKILL_CHATTER.replace("process_kind: 切削\n", "");
    let problems = run(&[("skills/pe-machining-chatter.md", process.as_str())]);
    assert_eq!(
        rules(&problems),
        vec!["process_kind_missing"],
        "{problems:#?}"
    );
}

#[test]
fn a_skill_tool_missing_from_the_tool_registry_is_rejected() {
    let skill = SKILL_SPC.replace(
        "compute_4m_correlation]",
        "compute_4m_correlation, no_such_tool]",
    );
    let problems = run(&set(&[("skills/pe-quality-spc.md", &skill)]));
    assert!(rules(&problems).contains(&"tool_unknown"), "{problems:#?}");
}

#[test]
fn confidential_case_rejected() {
    let case = CASE.replace("\"internal\"", "\"confidential\"");
    let problems = run(&set(&[("evals/cases/defect_rate_up/spc-0001.json", &case)]));
    assert_eq!(rules(&problems), vec!["case_raw_data"], "{problems:#?}");
    assert_eq!(problems[0].subject.as_deref(), Some("confidential"));
    assert_eq!(problems[0].line, 7);
}

#[test]
fn a_case_holding_raw_data_names_each_kind_and_line_never_the_value() {
    let case = CASE
        .replace(
            "\"offset_seconds\": -3600",
            "\"at\": \"2026-10-01T08:15:00+09:00\"",
        )
        .replace("\"P901\"", "\"P104\"")
        .replace(
            "\"expected\"",
            "\"host\": \"10.20.30.40\", \"feedback\": \"x\", \"staff_id\": \"x\", \"expected\"",
        );
    let raw = case_raw_data(&case);
    let kinds: Vec<&str> = raw.iter().map(|r| r.kind).collect();
    for want in [
        "absolute_timestamp",
        "plant_code",
        "ip_address",
        "free_text",
        "person",
    ] {
        assert!(kinds.contains(&want), "{want}: {raw:?}");
    }
    let at = raw.iter().find(|r| r.kind == "absolute_timestamp").unwrap();
    assert_eq!(at.line, 9);
    let problems = run(&set(&[("evals/cases/defect_rate_up/spc-0001.json", &case)]));
    assert!(
        problems.iter().all(|p| p.rule == "case_raw_data"),
        "{problems:#?}"
    );
    assert!(problems
        .iter()
        .all(|p| !format!("{p:?}").contains("10.20.30.40")));
}

#[test]
fn synthetic_plant_codes_relative_times_and_version_numbers_are_not_raw_data() {
    let text = r#"{"id": "x", "provenance": "synthetic", "plant_code": "P900", "note_ratio": 1.25,
"version": "1.2.3", "dt": "-3600", "freq_ratio": 3.14159}"#;
    assert_eq!(case_raw_data(text), Vec::new());
}

#[test]
fn a_case_without_a_synthetic_provenance_or_no_json_cannot_be_shown_safe() {
    let real = CASE.replace("\"replica\"", "\"production\"");
    assert!(case_raw_data(&real).iter().any(|r| r.kind == "provenance"));
    let missing = CASE.replace("\"provenance\": \"replica\",", "");
    assert!(case_raw_data(&missing)
        .iter()
        .any(|r| r.kind == "provenance"));
    assert!(case_raw_data("not json")
        .iter()
        .any(|r| r.kind == "not_json"));
    assert!(case_raw_data("[1, 2]").iter().any(|r| r.kind == "not_json"));
    // A second key a reader would keep cannot hide the first.
    let twice = CASE.replace(
        "\"provenance\": \"replica\",",
        "\"provenance\": \"production\",\n  \"provenance\": \"replica\",",
    );
    assert!(case_raw_data(&twice)
        .iter()
        .any(|r| r.kind == "duplicate_key"));
}

#[test]
fn a_case_shape_is_checked_against_its_path() {
    let case = CASE
        .replace("\"spc-0001\"", "\"spc-9999\"")
        .replace("\"reproduction\"", "\"other\"")
        .replace("\"source_hash\": \"3333333333333333333333333333333333333333333333333333333333333333\",", "");
    let problems = run(&set(&[("evals/cases/micro_stop/spc-0001.json", &case)]));
    let got = rules(&problems);
    for want in [
        "case_id_mismatch",
        "case_class_mismatch",
        "case_kind_invalid",
        "source_hash_invalid",
    ] {
        assert!(got.contains(&want), "{want}: {problems:#?}");
    }
    let flat = run(&[("evals/cases/spc-0001.json", CASE)]);
    assert!(rules(&flat).contains(&"case_path_invalid"), "{flat:#?}");
}

#[test]
fn a_security_case_is_a_safety_case_of_any_class() {
    let case = CASE
        .replace("\"reproduction\"", "\"safety\"")
        .replace("\"spc-0001\"", "\"inj-1\"");
    let problems = run(&[("evals/cases/security/inj-1.json", case.as_str())]);
    assert!(problems.is_empty(), "{problems:#?}");
    let regular = case.replace("\"safety\"", "\"regular\"");
    let problems = run(&[("evals/cases/security/inj-1.json", regular.as_str())]);
    assert_eq!(
        rules(&problems),
        vec!["case_kind_mismatch"],
        "{problems:#?}"
    );
}

#[test]
fn only_threshold_proposals_live_under_thresholds_and_none_ships_in_a_release() {
    let problems = run(&set(&[("thresholds/a1.toml", "x = 1\n")]));
    assert_eq!(
        rules(&problems),
        vec!["threshold_not_proposal"],
        "{problems:#?}"
    );
    let release = release_problems(&[
        "skills/pe-quality-spc.md",
        "thresholds/proposals/a1-run.md",
        "manifest/manifest.json",
        "models/models.toml",
    ]);
    assert_eq!(
        rules(&release),
        vec!["release_includes_manifest", "release_includes_thresholds"]
    );
}

#[test]
fn bench_images_must_be_pinned_by_digest() {
    let bench = "runs = 3\n[images]\nservice_image = \"registry.local/pfs/iotpf-ai-service:1.4\"\nollama_image = \"registry.local/ollama:0.6@sha256:4444444444444444444444444444444444444444444444444444444444444444\"\n";
    let problems = run(&set(&[("bench/bench.toml", bench)]));
    assert_eq!(rules(&problems), vec!["image_not_pinned"], "{problems:#?}");
    assert_eq!(problems[0].subject.as_deref(), Some("images.service_image"));
}

#[test]
fn unknown_agent_and_unreadable_files_are_named() {
    let book = PLAYBOOK.replace("\"4m-correlator\"", "\"timing-analyst\"");
    let problems = run(&set(&[("playbooks/global.toml", &book)]));
    assert_eq!(rules(&problems), vec!["agent_unknown"], "{problems:#?}");
    // Without agent definitions (stage 1: they are still PFS code) no agent check runs.
    let mut files = set(&[("playbooks/global.toml", &book)]);
    files.retain(|(p, _)| !p.starts_with("agents/"));
    assert!(run(&files).is_empty());
    let broken = run(&set(&[("models/models.toml", "small = \n")]));
    assert_eq!(rules(&broken), vec!["toml_syntax"]);
    let big = "x".repeat(MAX_FILE_BYTES + 1);
    let large = run(&[("evals/cases/defect_rate_up/big.json", big.as_str())]);
    assert_eq!(rules(&large), vec!["too_large"]);
}

#[test]
fn asset_paths_are_recognised_and_ordinary_paths_are_not() {
    for path in [
        "skills/pe-x.md",
        "evals/cases/a/b.json",
        "models/models.toml",
        "CODEOWNERS",
        "thresholds/proposals/a.md",
    ] {
        assert!(is_asset_path(path), "{path}");
    }
    for path in [
        "src/skills.rs",
        "docs/evals/x.md",
        "README.md",
        ".forge-ci.yml",
    ] {
        assert!(!is_asset_path(path), "{path}");
    }
    assert!(is_case_path("evals/cases/a/b.json"));
    assert!(!is_case_path("evals/baselines/m.json"));
}
