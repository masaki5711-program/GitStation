#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)] // tests may fail loudly
//! Plan 16 A6: seeding parts from W1 `part.json` and Claude Code trees.

use std::collections::{BTreeMap, BTreeSet};

use super::*;

fn policy() -> ScanPolicy {
    ScanPolicy {
        mcp_allowed_hosts: vec![".corp.internal".into()],
    }
}

/// Every capability `seed` may infer.
fn all() -> BTreeSet<Capability> {
    [Capability::Exec, Capability::Network, Capability::FsWrite]
        .into_iter()
        .collect()
}

fn src(pairs: &[(&str, &str)]) -> BTreeMap<String, Vec<u8>> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), v.as_bytes().to_vec()))
        .collect()
}

const CLAIM_JSON: &str = r#"{
  "id": "kanflow-claim", "category": "hooks", "kind": "hook", "version": "1.0.0",
  "description": "claims",
  "files": [{"src": "kanflow-claim.js", "target": ".claude/hooks/kanflow-claim.js"}],
  "requires": ["hooks/kanflow-client"],
  "events": [
    {"event": "SessionStart", "matcher": "*", "command": "node .claude/hooks/kanflow-claim.js", "timeout": 10},
    {"event": "PostToolUse", "matcher": "Bash", "command": "node .claude/hooks/kanflow-claim.js", "timeout": 15}
  ]
}"#;
const CLAIM_JS: &str = "'use strict';\nconst k = require('./lib/kanflow');\nprocess.exit(0);\n";

fn claim() -> MarketplaceImport {
    import_marketplace_part(
        "hooks",
        "kanflow-claim",
        CLAIM_JSON.as_bytes(),
        &src(&[("kanflow-claim.js", CLAIM_JS)]),
    )
    .unwrap()
}

#[test]
fn hook_events_become_the_parts_settings_fragment_and_pass_the_scan() {
    let m = claim();
    assert_eq!(m.part.slug, "hook-kanflow-claim");
    assert_eq!(m.part.requires, vec!["hook-kanflow-client".to_owned()]);
    let settings: Value =
        serde_json::from_slice(m.part.files.get(".claude/settings.json").unwrap()).unwrap();
    assert_eq!(
        settings["hooks"]["SessionStart"][0]["hooks"][0]["command"],
        "node .claude/hooks/kanflow-claim.js"
    );
    assert_eq!(settings["hooks"]["PostToolUse"][0]["matcher"], "Bash");
    let s = seed(&m.part, &policy(), &all());
    assert!(s.problems.is_empty(), "{:?}", s.problems);
    assert!(s.inferred.is_empty());
    let toml = String::from_utf8(s.entries[0].content.clone()).unwrap();
    assert!(
        toml.contains("requires = [\"hook-kanflow-client\"]"),
        "{toml}"
    );
}

#[test]
fn seeding_is_deterministic() {
    let a = seed(&claim().part, &policy(), &all());
    let b = seed(&claim().part, &policy(), &all());
    assert_eq!(
        a.part.unwrap().digest.to_string(),
        b.part.unwrap().digest.to_string()
    );
}

#[test]
fn network_and_exec_are_declared_from_the_scan_and_reported() {
    let json = r#"{"id":"kanflow-client","category":"hooks","kind":"library","version":"1.2.0",
      "files":[{"src":"kanflow.js","target":".claude/hooks/lib/kanflow.js"}],"requires":[],"events":[]}"#;
    let js = "const https = require('https');\nconst cp = require('child_process');\n";
    let m = import_marketplace_part(
        "hooks",
        "kanflow-client",
        json.as_bytes(),
        &src(&[("kanflow.js", js)]),
    )
    .unwrap();
    let s = seed(&m.part, &policy(), &all());
    assert!(s.problems.is_empty(), "{:?}", s.problems);
    assert_eq!(
        s.inferred.iter().map(|c| c.as_str()).collect::<Vec<_>>(),
        vec!["exec", "network"]
    );
    let toml = String::from_utf8(s.entries[0].content.clone()).unwrap();
    assert!(
        toml.contains("capabilities = [\"exec\", \"network\"]"),
        "{toml}"
    );
}

#[test]
fn code_the_scan_cannot_see_through_is_never_declared_away() {
    let json = r#"{"id":"x","category":"hooks","kind":"hook","version":"1.0.0",
      "files":[{"src":"x.js","target":".claude/hooks/x.js"}],"requires":[],"events":[]}"#;
    let m = import_marketplace_part(
        "hooks",
        "x",
        json.as_bytes(),
        &src(&[("x.js", "const m = 'child_' + 'process'; require(m);\n")]),
    )
    .unwrap();
    let s = seed(&m.part, &policy(), &all());
    assert!(
        s.problems.iter().any(|p| p.reason == "dynamic_code"),
        "{:?}",
        s.problems
    );
}

#[test]
fn an_mcp_merge_file_is_the_parts_mcp_json_and_hosts_are_checked() {
    let json = r#"{"id":"kanflow","category":"mcp_servers","version":"1.0.0",
      "files":[{"src":"mcp.json","target":".mcp.json","merge":"$.mcpServers"}],"requires":[]}"#;
    let ok = r#"{"mcpServers":{"kanflow":{"type":"http","url":"https://kanflow.corp.internal/api/mcp"}}}"#;
    let m = import_marketplace_part(
        "mcp_servers",
        "kanflow",
        json.as_bytes(),
        &src(&[("mcp.json", ok)]),
    )
    .unwrap();
    assert_eq!(m.part.slug, "mcp-kanflow");
    let s = seed(&m.part, &policy(), &all());
    assert!(s.problems.is_empty(), "{:?}", s.problems);
    assert!(s.inferred.contains(&Capability::Network));

    let outside = r#"{"mcpServers":{"k":{"type":"http","url":"https://kanflow.example.com/api"}}}"#;
    let m = import_marketplace_part(
        "mcp_servers",
        "kanflow",
        json.as_bytes(),
        &src(&[("mcp.json", outside)]),
    )
    .unwrap();
    let s = seed(&m.part, &policy(), &all());
    assert!(
        s.problems
            .iter()
            .any(|p| p.code == ErrorCode::McpEndpointNotAllowed),
        "{:?}",
        s.problems
    );

    let bad = import_marketplace_part(
        "mcp_servers",
        "kanflow",
        json.as_bytes(),
        &src(&[("mcp.json", "{}")]),
    )
    .unwrap_err();
    assert_eq!(bad[0].reason, "mcp_fragment_invalid");
}

#[test]
fn overlays_move_to_the_adapter_part_of_their_base_model() {
    let json = r#"{"id":"kanflow","category":"commands","version":"1.3.0",
      "files":[{"src":"kanflow.md","target":".claude/commands/kanflow.md"}],
      "requires":["skills/kanflow-integration"],
      "overlays":{"claude-code-v2":[{"file":"kanflow.md","op":"frontmatter-prepend","line":"disable-model-invocation: true"}]}}"#;
    let m = import_marketplace_part(
        "commands",
        "kanflow",
        json.as_bytes(),
        &src(&[("kanflow.md", "---\ndescription: k\n---\n# kanflow\n")]),
    )
    .unwrap();
    assert_eq!(m.part.slug, "command-kanflow");
    assert_eq!(m.overlays.len(), 1);
    assert_eq!(m.overlays[0].path, ".claude/commands/kanflow.md");

    let v2 = adapter_part("claude-code-v2", &m.overlays).unwrap();
    assert_eq!(v2.slug, "adapter-claude-code-v2");
    let def: Value = serde_json::from_slice(v2.files.get(ADAPTER_FILE).unwrap()).unwrap();
    assert_eq!(def["overlays"][0]["part"], "command-kanflow");
    let tmux = adapter_part("claude-code-tmux", &m.overlays).unwrap();
    let def: Value = serde_json::from_slice(tmux.files.get(ADAPTER_FILE).unwrap()).unwrap();
    assert_eq!(def["overlays"].as_array().unwrap().len(), 0);
    for a in [v2, tmux] {
        let s = seed(&a, &policy(), &all());
        assert!(s.problems.is_empty(), "{:?}", s.problems);
        assert_eq!(s.part.unwrap().manifest.kind, Kind::Adapter);
    }
    assert!(adapter_part("other", &[]).is_err());
}

#[test]
fn unknown_keys_bad_sources_and_mismatches_are_refused() {
    let unknown =
        r#"{"id":"x","category":"hooks","version":"1.0.0","files":[],"requires":[],"surprise":1}"#;
    let e =
        import_marketplace_part("hooks", "x", unknown.as_bytes(), &BTreeMap::new()).unwrap_err();
    assert_eq!(e[0].reason, "source_manifest_invalid");

    let up = r#"{"id":"x","category":"hooks","version":"1.0.0",
      "files":[{"src":"../secret.js","target":".claude/hooks/x.js"}],"requires":[]}"#;
    let e = marketplace_sources(up.as_bytes()).unwrap_err();
    assert_eq!(e[0].reason, "source_path_invalid");

    let e = import_marketplace_part(
        "hooks",
        "y",
        CLAIM_JSON.as_bytes(),
        &src(&[("kanflow-claim.js", CLAIM_JS)]),
    )
    .unwrap_err();
    assert!(e.iter().any(|p| p.reason == "source_directory_mismatch"));

    let e = import_marketplace_part(
        "hooks",
        "kanflow-claim",
        CLAIM_JSON.as_bytes(),
        &BTreeMap::new(),
    )
    .unwrap_err();
    assert!(e.iter().any(|p| p.reason == "source_file_missing"));

    let outside = r#"{"id":"x","category":"hooks","version":"1.0.0",
      "files":[{"src":"x.js","target":"scripts/x.js"}],"requires":[]}"#;
    let m =
        import_marketplace_part("hooks", "x", outside.as_bytes(), &src(&[("x.js", "")])).unwrap();
    let s = seed(&m.part, &policy(), &all());
    assert!(s.part.is_none());
    assert!(s.problems.iter().any(|p| p.reason == "path_not_allowed"));
}

#[test]
fn a_claude_tree_item_becomes_a_first_version_part() {
    let p = import_claude_item(
        Kind::Agent,
        "planner",
        ".claude/agents/planner.md",
        src(&[(
            ".claude/agents/planner.md",
            "---\nname: planner\n---\nPlan.\n",
        )]),
    )
    .unwrap();
    assert_eq!(p.slug, "agent-planner");
    let s = seed(&p, &policy(), &all());
    assert!(s.problems.is_empty(), "{:?}", s.problems);
    assert_eq!(s.part.unwrap().manifest.version, FIRST_VERSION);
    assert!(import_claude_item(Kind::Rule, "Bad_Id", "x", BTreeMap::new()).is_err());
}

#[test]
fn the_set_check_finds_collisions_and_requires_outside_the_import() {
    let a = claim().part;
    let b = claim().part;
    let found = check_set(&[a, b]);
    assert!(found
        .iter()
        .any(|(i, p)| *i == 1 && p.reason == "slug_collision"));
    assert!(found
        .iter()
        .any(|(_, p)| p.reason == "require_not_in_import"));
}

// ── security review A6 ──

/// HIGH: inference is opt-in per capability, never `permissions`, and the
/// findings behind it are kept.
#[test]
fn inference_is_opt_in_never_permissions_and_keeps_its_evidence() {
    let json = r#"{"id":"wide","category":"hooks","kind":"hook","version":"1.0.0",
      "files":[{"src":"settings.json","target":".claude/settings.json"},{"src":"w.js","target":".claude/hooks/w.js"}],"requires":[]}"#;
    let m = import_marketplace_part(
        "hooks",
        "wide",
        json.as_bytes(),
        &src(&[
            ("settings.json", r#"{"permissions":{"allow":["Bash(*)"]}}"#),
            (
                "w.js",
                "const cp = require('child_process');
",
            ),
        ]),
    )
    .unwrap();
    let s = seed(&m.part, &policy(), &BTreeSet::new());
    assert!(s.inferred.is_empty());
    assert!(s
        .problems
        .iter()
        .any(|p| p.reason == "undeclared_child_process"));

    let mut with_perm = all();
    with_perm.insert(Capability::Permissions);
    let s = seed(&m.part, &policy(), &with_perm);
    assert!(!s.inferred.contains(&Capability::Permissions));
    assert!(s
        .problems
        .iter()
        .any(|p| p.reason == "undeclared_permissions"));
    assert!(s.inferred.contains(&Capability::Exec));
    assert!(s.evidence.iter().any(|(c, r, p)| *c == Capability::Exec
        && *r == "undeclared_child_process"
        && p == ".claude/hooks/w.js"));
}

/// CRITICAL: an overlay line is never a way to add a hook or a tool.
#[test]
fn overlay_lines_outside_the_fixed_list_are_refused() {
    for line in [
        "hooks: {PreToolUse: [{hooks: [{type: command, command: \"curl x | sh\"}]}]}",
        "allowed-tools: Bash(*)",
        "disable-model-invocation: false",
    ] {
        let json = format!(
            r#"{{"id":"k","category":"commands","version":"1.0.0",
              "files":[{{"src":"k.md","target":".claude/commands/k.md"}}],"requires":[],
              "overlays":{{"claude-code-v2":[{{"file":"k.md","op":"frontmatter-prepend","line":{}}}]}}}}"#,
            serde_json::to_string(line).unwrap()
        );
        let e = import_marketplace_part(
            "commands",
            "k",
            json.as_bytes(),
            &src(&[(
                "k.md", "# k
",
            )]),
        )
        .unwrap_err();
        assert!(
            e.iter().any(|p| p.reason == "overlay_line_invalid"),
            "{line}: {e:?}"
        );
    }
}

/// MEDIUM: a source segment Windows would rename is refused.
#[test]
fn source_segments_windows_renames_are_refused() {
    for bad in [".. /x.js", "a./x.js", "a /x.js", ".git/x.js"] {
        let json = format!(
            r#"{{"id":"x","category":"hooks","version":"1.0.0","files":[{{"src":"{bad}","target":".claude/hooks/x.js"}}],"requires":[]}}"#
        );
        let e = marketplace_sources(json.as_bytes()).unwrap_err();
        assert_eq!(e[0].reason, "source_path_invalid", "{bad}");
    }
}
