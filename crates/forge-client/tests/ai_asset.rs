#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)] // tests may fail loudly
//! `gstn ai-asset lint` (plan 17 P17-3, ADR-0035) as a user runs it: the
//! real binary in a git working tree, with no host and no token (all data
//! synthetic).

use std::path::Path;
use std::process::Command;

const SKILL: &str = "---
name: pe-quality-spc
description: 不良率の上昇を読む
axis: cross
anomaly_class: [defect_rate_up]
required_tools: [slice_by_4axis]
required_profile: core
reviewer: 生産技術
version: \"1.0\"
valid_from: 2026-08-16
---
";

const PLAYBOOK: &str = "[[playbook]]
indicator_code = \"A1\"
scope_type = \"global\"
scope_key = \"*\"
severity_filter = \"*\"
valid_from = \"2026-08-16\"
priority = 10
agent_name = \"4m-correlator\"
skill_refs = [\"pe-quality-spc\"]
tool_sequence = [\"slice_by_4axis\"]
required_profile = \"core\"
";

const MODELS: &str = "small = \"qwen3:14b@sha256:1111111111111111111111111111111111111111111111111111111111111111\"\n";

const CASE: &str = "{\"id\": \"c1\", \"anomaly_class\": \"defect_rate_up\", \"kind\": \"regular\", \"provenance\": \"synthetic\", \"expected\": {\"withhold\": true}}\n";

fn repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let st = Command::new("git")
        .args(["init", "-q"])
        .current_dir(dir.path())
        .status()
        .expect("git init");
    assert!(st.success());
    for (path, text) in [
        ("skills/pe-quality-spc.md", SKILL),
        ("playbooks/global.toml", PLAYBOOK),
        ("models/models.toml", MODELS),
        ("evals/cases/defect_rate_up/c1.json", CASE),
        ("src/main.rs", "fn main() {}\n"),
    ] {
        let file = dir.path().join(path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, text).unwrap();
    }
    dir
}

struct Out {
    code: i32,
    stdout: String,
    stderr: String,
}

fn gstn(cwd: &Path, args: &[&str]) -> Out {
    let o = Command::new(env!("CARGO_BIN_EXE_gstn"))
        .args(args)
        .current_dir(cwd)
        .env_remove("GSTN_TOKEN")
        .env("GSTN_HOST", "https://unreachable.invalid")
        .env("GSTN_CONFIG_DIR", cwd.join(".no-gstn-config"))
        .output()
        .expect("gstn");
    Out {
        code: o.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
    }
}

#[test]
fn ai_asset_lint_passes_a_consistent_tree_from_a_subdirectory() {
    let r = repo();
    let o = gstn(&r.path().join("skills"), &["ai-asset", "lint"]);
    assert_eq!(o.code, 0, "{} {}", o.stdout, o.stderr);
    assert!(
        o.stdout.contains("No problem in 4 asset file(s)."),
        "{}",
        o.stdout
    );
}

#[test]
fn an_asset_repo_with_a_latest_model_is_ai_asset_invalid() {
    let r = repo();
    std::fs::write(
        r.path().join("models/models.toml"),
        "small = \"qwen3:latest\"\n",
    )
    .unwrap();
    std::fs::write(
        r.path().join("evals/cases/defect_rate_up/c1.json"),
        CASE.replace(
            "\"synthetic\"",
            "\"synthetic\", \"at\": \"2026-10-01T08:00:00Z\"",
        ),
    )
    .unwrap();
    let o = gstn(r.path(), &["ai-asset", "lint", "--json"]);
    assert_eq!(o.code, 1, "{} {}", o.stdout, o.stderr);
    let v: serde_json::Value = serde_json::from_str(&o.stdout).expect("json");
    let rules: Vec<(&str, &str)> = v["problems"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| (p["path"].as_str().unwrap(), p["rule"].as_str().unwrap()))
        .collect();
    assert_eq!(
        rules,
        vec![
            ("evals/cases/defect_rate_up/c1.json", "case_raw_data"),
            ("models/models.toml", "model_latest"),
        ]
    );
    assert!(o.stderr.contains("FG-AIASSET-0001"), "{}", o.stderr);
    // The matched value is never echoed.
    assert!(!o.stdout.contains("2026-10-01"), "{}", o.stdout);
}

#[test]
fn ai_asset_lint_reads_a_given_directory() {
    let r = repo();
    std::fs::write(
        r.path().join("playbooks/global.toml"),
        PLAYBOOK.replace("[\"pe-quality-spc\"]", "[\"pe-missing\"]"),
    )
    .unwrap();
    let elsewhere = tempfile::tempdir().expect("tempdir");
    let dir = r.path().to_str().unwrap();
    let o = gstn(elsewhere.path(), &["ai-asset", "lint", "--dir", dir]);
    assert_eq!(o.code, 1, "{} {}", o.stdout, o.stderr);
    assert!(
        o.stdout
            .contains("playbooks/global.toml:0: skill_unknown — playbook[0].skill_refs: pe-missing [FG-AIASSET-0001]"),
        "{}",
        o.stdout
    );
}
