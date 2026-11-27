#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)] // tests may fail loudly
//! `gstn part import` (plan 16 A6, ADR-0032 Amendment A6) as a user runs it:
//! the real binary on synthetic W1 and `.claude/` trees, no host and no
//! token. The plan's test: every seeded part passes `gstn agent verify`.

use std::path::Path;
use std::process::Command;

struct Out {
    code: i32,
    stdout: String,
    stderr: String,
}

impl Out {
    fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.stdout)
            .unwrap_or_else(|e| panic!("stdout is not JSON ({e}): {}", self.stdout))
    }
}

fn gstn(cwd: &Path, args: &[&str]) -> Out {
    let config = tempfile::tempdir().unwrap();
    let o = Command::new(env!("CARGO_BIN_EXE_gstn"))
        .args(args)
        .current_dir(cwd)
        .env_remove("GSTN_TOKEN")
        .env("GSTN_HOST", "https://unreachable.invalid")
        .env("GSTN_CONFIG_DIR", config.path())
        .output()
        .expect("gstn");
    Out {
        code: o.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
    }
}

fn write(root: &Path, rel: &str, text: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

/// A W1 `marketplace/` like the agent models' (synthetic content).
fn marketplace(root: &Path) {
    write(root, "README.md", "# parts\n");
    write(
        root,
        "hooks/kanflow-client/part.json",
        r#"{"id":"kanflow-client","category":"hooks","kind":"library","version":"1.2.0","description":"client",
           "files":[{"src":"kanflow.js","target":".claude/hooks/lib/kanflow.js"}],"requires":[],"events":[]}"#,
    );
    write(
        root,
        "hooks/kanflow-client/kanflow.js",
        "'use strict';\nconst https = require('https');\nmodule.exports = {};\n",
    );
    write(
        root,
        "hooks/kanflow-claim/part.json",
        r#"{"id":"kanflow-claim","category":"hooks","kind":"hook","version":"1.0.0","description":"claims",
           "files":[{"src":"kanflow-claim.js","target":".claude/hooks/kanflow-claim.js"}],
           "requires":["hooks/kanflow-client"],
           "events":[{"event":"SessionStart","matcher":"*","command":"node .claude/hooks/kanflow-claim.js","timeout":10}]}"#,
    );
    write(
        root,
        "hooks/kanflow-claim/kanflow-claim.js",
        "'use strict';\nconst k = require('./lib/kanflow');\nprocess.exit(0);\n",
    );
    write(
        root,
        "mcp_servers/kanflow/part.json",
        r#"{"id":"kanflow","category":"mcp_servers","version":"1.0.0","description":"mcp",
           "files":[{"src":"mcp.json","target":".mcp.json","merge":"$.mcpServers"}],"requires":[]}"#,
    );
    write(
        root,
        "mcp_servers/kanflow/mcp.json",
        r#"{"mcpServers":{"kanflow":{"type":"http","url":"https://kanflow.corp.internal/api/mcp"}}}"#,
    );
    write(
        root,
        "commands/kanflow/part.json",
        r#"{"id":"kanflow","category":"commands","version":"1.3.0","description":"cmd",
           "files":[{"src":"kanflow.md","target":".claude/commands/kanflow.md"}],
           "requires":["skills/kanflow-integration"],
           "overlays":{"claude-code-v2":[{"file":"kanflow.md","op":"frontmatter-prepend","line":"disable-model-invocation: true"}]}}"#,
    );
    write(
        root,
        "commands/kanflow/kanflow.md",
        "---\ndescription: kanflow\n---\n# /kanflow\n",
    );
    write(
        root,
        "skills/kanflow-integration/part.json",
        r#"{"id":"kanflow-integration","category":"skills","version":"1.3.0","description":"skill",
           "files":[{"src":"SKILL.md","target":".claude/skills/kanflow-integration/SKILL.md"},
                    {"src":"kanflow-snapshot.json","target":".claude/skills/kanflow-integration/kanflow-snapshot.json"}],
           "requires":["mcp_servers/kanflow"]}"#,
    );
    write(
        root,
        "skills/kanflow-integration/SKILL.md",
        "---\nname: kanflow-integration\n---\nUse Kanflow.\n",
    );
    write(
        root,
        "skills/kanflow-integration/kanflow-snapshot.json",
        "{\"tools\":[]}\n",
    );
}

fn slugs(v: &serde_json::Value) -> Vec<String> {
    v["parts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["slug"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn every_seeded_part_passes_agent_verify() {
    let src = tempfile::tempdir().unwrap();
    marketplace(src.path());
    let work = tempfile::tempdir().unwrap();
    let out = work.path().join("parts");
    let from = src.path().to_str().unwrap();
    let out_s = out.to_str().unwrap();

    // Dry run first: the report, nothing written.
    let dry = gstn(
        work.path(),
        &[
            "part",
            "import",
            "--from",
            from,
            "--format",
            "marketplace",
            "--mcp-allowed-hosts",
            ".corp.internal",
            "--declare-from-scan",
            "exec,network",
            "--json",
        ],
    );
    assert_eq!(dry.code, 0, "{}{}", dry.stdout, dry.stderr);
    let v = dry.json();
    assert_eq!(v["dry_run"], true);
    assert!(!out.exists());
    let mut got = slugs(&v);
    got.sort();
    assert_eq!(
        got,
        [
            "adapter-claude-code-tmux",
            "adapter-claude-code-v2",
            "command-kanflow",
            "hook-kanflow-claim",
            "hook-kanflow-client",
            "mcp-kanflow",
            "skill-kanflow-integration",
        ]
    );
    let client = v["parts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["slug"] == "hook-kanflow-client")
        .unwrap();
    assert_eq!(
        client["inferred_capabilities"],
        serde_json::json!(["network"])
    );
    assert_eq!(
        client["inferred_from"][0]["path"],
        ".claude/hooks/lib/kanflow.js"
    );

    let wrote = gstn(
        work.path(),
        &[
            "part",
            "import",
            "--from",
            from,
            "--format",
            "marketplace",
            "--mcp-allowed-hosts",
            ".corp.internal",
            "--declare-from-scan",
            "exec,network",
            "--out",
            out_s,
            "--json",
        ],
    );
    assert_eq!(wrote.code, 0, "{}{}", wrote.stdout, wrote.stderr);
    let digests: std::collections::BTreeMap<String, String> = wrote.json()["parts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| {
            (
                p["slug"].as_str().unwrap().to_owned(),
                p["digest"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    for (slug, digest) in &digests {
        let dir = out.join(slug);
        let check = gstn(
            work.path(),
            &[
                "agent",
                "verify",
                "--part",
                dir.to_str().unwrap(),
                "--mcp-allowed-hosts",
                ".corp.internal",
                "--json",
            ],
        );
        assert_eq!(check.code, 0, "{slug}: {}{}", check.stdout, check.stderr);
        assert_eq!(
            &check.json()["digest"],
            digest.as_str(),
            "{slug}: same digest"
        );
    }
    let adapter =
        std::fs::read_to_string(out.join("adapter-claude-code-v2/.claude/gitstation/adapter.json"))
            .unwrap();
    assert!(
        adapter.contains("disable-model-invocation: true"),
        "{adapter}"
    );
    let report: serde_json::Value =
        serde_json::from_slice(&std::fs::read(out.join("import-report.json")).unwrap()).unwrap();
    assert_eq!(report["written"], out_s);

    // Never over an existing tree.
    let again = gstn(
        work.path(),
        &[
            "part",
            "import",
            "--from",
            from,
            "--format",
            "marketplace",
            "--mcp-allowed-hosts",
            ".corp.internal",
            "--declare-from-scan",
            "exec,network",
            "--out",
            out_s,
        ],
    );
    assert_eq!(again.code, 1);
    assert!(again.stderr.contains("FG-CLI-0007"), "{}", again.stderr);
    assert!(again.stderr.contains("out_not_empty"), "{}", again.stderr);
}

#[test]
fn a_failing_part_blocks_writing_and_is_reported() {
    let src = tempfile::tempdir().unwrap();
    marketplace(src.path());
    let work = tempfile::tempdir().unwrap();
    let out = work.path().join("parts");
    // No allow-list: the MCP host is refused.
    let r = gstn(
        work.path(),
        &[
            "part",
            "import",
            "--from",
            src.path().to_str().unwrap(),
            "--format",
            "marketplace",
            "--out",
            out.to_str().unwrap(),
            "--json",
        ],
    );
    assert_eq!(r.code, 1, "{}{}", r.stdout, r.stderr);
    assert!(r.stdout.contains("mcp_host_not_allowed"), "{}", r.stdout);
    assert_eq!(r.json()["written"], serde_json::Value::Null);
    assert!(!out.exists(), "all or nothing");

    // --only keeps the named slugs; a capability is declared only when
    // named (security review A6); an unknown slug is an input error.
    let only = |extra: &[&str]| {
        let mut args = vec![
            "part",
            "import",
            "--from",
            src.path().to_str().unwrap(),
            "--format",
            "marketplace",
            "--only",
            "hook-kanflow-client",
            "--json",
        ];
        args.extend_from_slice(extra);
        gstn(work.path(), &args)
    };
    let r = only(&[]);
    assert_eq!(r.code, 1, "{}{}", r.stdout, r.stderr);
    assert!(r.stdout.contains("undeclared_network"), "{}", r.stdout);
    let r = only(&["--declare-from-scan", "permissions"]);
    assert_ne!(r.code, 0, "permissions is never declared from the scan");
    let r = only(&["--declare-from-scan", "network"]);
    assert_eq!(r.code, 0, "{}{}", r.stdout, r.stderr);
    assert_eq!(slugs(&r.json()), ["hook-kanflow-client"]);
    let r = gstn(
        work.path(),
        &[
            "part",
            "import",
            "--from",
            src.path().to_str().unwrap(),
            "--format",
            "marketplace",
            "--only",
            "nope",
        ],
    );
    assert_eq!(r.code, 1);
    assert!(r.stderr.contains("only_unknown_slug"), "{}", r.stderr);
}

#[test]
fn a_dynamic_require_in_a_source_hook_is_a_problem_not_a_capability() {
    let src = tempfile::tempdir().unwrap();
    write(
        src.path(),
        "hooks/sneaky/part.json",
        r#"{"id":"sneaky","category":"hooks","kind":"hook","version":"1.0.0",
           "files":[{"src":"s.js","target":".claude/hooks/s.js"}],"requires":[],"events":[]}"#,
    );
    write(
        src.path(),
        "hooks/sneaky/s.js",
        "const m = 'child_' + 'process';\nrequire(m);\n",
    );
    write(
        src.path(),
        "hooks/up/part.json",
        r#"{"id":"up","category":"hooks","version":"1.0.0","files":[{"src":"../sneaky/s.js","target":".claude/hooks/u.js"}],"requires":[]}"#,
    );
    let r = gstn(
        src.path(),
        &[
            "part",
            "import",
            "--from",
            src.path().to_str().unwrap(),
            "--format",
            "marketplace",
            "--json",
        ],
    );
    assert_eq!(r.code, 1);
    assert!(r.stdout.contains("dynamic_code"), "{}", r.stdout);
    assert!(r.stdout.contains("source_path_invalid"), "{}", r.stdout);
}

#[test]
fn a_claude_tree_becomes_parts_and_the_harness_is_skipped() {
    let repo = tempfile::tempdir().unwrap();
    let r = repo.path();
    write(
        r,
        ".claude/skills/tdd-workflow/SKILL.md",
        "---\nname: tdd-workflow\n---\nRed, green.\n",
    );
    write(r, ".claude/skills/tdd-workflow/reference.md", "More.\n");
    write(
        r,
        ".claude/agents/planner.md",
        "---\nname: planner\n---\nPlan.\n",
    );
    write(
        r,
        ".claude/commands/plan.md",
        "---\ndescription: plan\n---\nPlan it.\n",
    );
    write(r, ".claude/rules/common/baseline.md", "# Baseline\n");
    write(r, ".claude/settings.json", "{\"hooks\":{}}");
    write(r, ".claude/mistakes/records.jsonl", "{}\n");
    let work = tempfile::tempdir().unwrap();
    let out = work.path().join("parts");
    let o = gstn(
        work.path(),
        &[
            "part",
            "import",
            "--from",
            r.to_str().unwrap(),
            "--format",
            "claude",
            "--out",
            out.to_str().unwrap(),
            "--json",
        ],
    );
    assert_eq!(o.code, 0, "{}{}", o.stdout, o.stderr);
    let v = o.json();
    let mut got = slugs(&v);
    got.sort();
    assert_eq!(
        got,
        [
            "agent-planner",
            "command-plan",
            "rule-common-baseline",
            "skill-tdd-workflow"
        ]
    );
    assert_eq!(
        v["skipped"],
        serde_json::json!([".claude/mistakes", ".claude/settings.json"])
    );
    for slug in &got {
        let check = gstn(
            work.path(),
            &[
                "agent",
                "verify",
                "--part",
                out.join(slug).to_str().unwrap(),
                "--json",
            ],
        );
        assert_eq!(check.code, 0, "{slug}: {}{}", check.stdout, check.stderr);
    }
    assert!(out
        .join("skill-tdd-workflow/.claude/skills/tdd-workflow/reference.md")
        .is_file());
}
