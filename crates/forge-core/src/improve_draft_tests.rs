use crate::improve_draft::{is_improve_branch, is_protected_path};
use crate::RefName;

fn name(raw: &str) -> RefName {
    RefName::parse(raw).expect("valid ref")
}

#[test]
fn only_improve_branches_are_improve_branches() {
    assert!(is_improve_branch(&name("refs/heads/improve/req-42")));
    assert!(is_improve_branch(&name("refs/heads/improve/a/b")));
    for other in [
        "refs/heads/main",
        "refs/heads/improvements/x",
        "refs/heads/feature/improve/x",
        "refs/tags/improve/x",
    ] {
        assert!(!is_improve_branch(&name(other)), "{other}");
    }
}

#[test]
fn the_ai_asset_and_forge_paths_are_protected() {
    for path in [
        "agents/allowlist.toml",
        "Agents/allowlist.toml",
        "models/registry.toml",
        "bench/run.py",
        "evals/baselines/m1.json",
        "thresholds/proposals/a.md",
        "CODEOWNERS",
        ".forge-ci.yml",
        ".forge/repo.yml",
        "migrations/0001_init.sql",
        "src/auth/mod.rs",
        "package.json",
        ".claude/settings.json",
        ".mcp.json",
        "sub/.mcp.json",
        ".gitstation/hooks.toml",
        // As a developer's file system reads them.
        "Agents./x.toml",
        "AGENTS/x.toml",
        ".cl\u{200b}aude/settings.json",
        "agents\u{200c}/x.toml",
        "models /m.bin",
    ] {
        assert!(is_protected_path(path), "{path}");
    }
}

#[test]
fn skills_cases_and_playbooks_are_not_protected() {
    for path in [
        "skills/pe-vibration.md",
        "playbooks/line-a.toml",
        "evals/cases/step/c1.json",
        "evals/cases/security/s1.json",
        "README.md",
        "docs/agents.txt",
    ] {
        assert!(!is_protected_path(path), "{path}");
    }
}
