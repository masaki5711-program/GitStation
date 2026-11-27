//! The improvement-draft token (plan 17 P17-5, ADR-0035 Amendment P17-5):
//! what a `pfs-ai-bot` push may touch. Pure: `forge-auth` decides the
//! branch, the pre-receive hook (`forge-git`) and the single-file commit
//! (`forge-web`) the paths.
//!
//! - Its branches are `refs/heads/improve/<name>` ([`is_improve_branch`]).
//! - It never changes a protected path ([`is_protected_path`]): forge's own
//!   (`auto_merge::is_protected_path`: migrations, `.forge/`,
//!   `.forge-ci.yml`, CODEOWNERS, authorization code), the autofix bot's
//!   fixed denials (`autofix::fixed_denial`: build and tool configuration,
//!   agent instructions, secret-looking names), agent configuration
//!   (`risk::is_agent_config_path`: `.claude/`, `.gitstation/`,
//!   `.mcp.json`, `CLAUDE.md`) and the AI asset repository's
//!   (plan 17 §2: `agents/`, `models/`, `bench/`, `evals/baselines/`,
//!   `thresholds/`). ASCII case is ignored (a case-insensitive checkout
//!   folds `Agents/` onto `agents/`).

use crate::RefName;

/// The branch prefix of an improvement draft (after `refs/heads/`).
pub const BRANCH_PREFIX: &str = "improve/";

/// The AI asset repository's protected directories (plan 17 §2).
const AI_ASSET_DIRS: &[&str] = &[
    "agents/",
    "models/",
    "bench/",
    "evals/baselines/",
    "thresholds/",
];

/// Whether `name` is `refs/heads/improve/<something>`.
pub fn is_improve_branch(name: &RefName) -> bool {
    name.branch()
        .and_then(|b| b.strip_prefix(BRANCH_PREFIX))
        .is_some_and(|rest| !rest.is_empty())
}

/// The branch prefix of the local LLM's draft of an improvement (plan 17
/// P17-10, after `refs/heads/`): never `improve/*`, so the bot's token
/// cannot create or race it, and never the improvement's own branch (the
/// draft is proposed beside it, a person applies it).
pub const AI_DRAFT_PREFIX: &str = "ai-draft/";

/// Whether `name` is `refs/heads/ai-draft/<something>`.
pub fn is_ai_draft_branch(name: &RefName) -> bool {
    name.branch()
        .and_then(|b| b.strip_prefix(AI_DRAFT_PREFIX))
        .is_some_and(|rest| !rest.is_empty())
}

/// The draft branch of the improvement branch `source`
/// (`refs/heads/improve/<x>` → `refs/heads/ai-draft/<x>`); `None` for any
/// other branch.
pub fn ai_draft_branch_for(source: &RefName) -> Option<RefName> {
    let rest = source.branch()?.strip_prefix(BRANCH_PREFIX)?;
    if rest.is_empty() {
        return None;
    }
    RefName::parse(&format!("refs/heads/{AI_DRAFT_PREFIX}{rest}")).ok()
}

/// Whether an improvement draft may not change `path` (see the module
/// docs). `path` is repository-relative, `/`-separated. Each component is
/// also read as a developer's file system reads it
/// (`risk::on_disk_upper`: ignorable code points removed, an NTFS stream
/// suffix dropped, trailing dots and spaces trimmed, case folded), so
/// `Agents./x`, `.cl<U+200B>aude/` or `AGENTS/x` are the protected ones.
pub fn is_protected_path(path: &str) -> bool {
    let on_disk = path
        .split('/')
        .map(|c| crate::risk::on_disk_upper(c).to_lowercase())
        .collect::<Vec<_>>()
        .join("/");
    [path, on_disk.as_str()].iter().any(|p| rule(p))
}

fn rule(path: &str) -> bool {
    if crate::auto_merge::is_protected_path(path)
        || crate::autofix::fixed_denial(path).is_some()
        || crate::risk::is_agent_config_path(path)
    {
        return true;
    }
    let lower = path.to_ascii_lowercase();
    AI_ASSET_DIRS.iter().any(|dir| lower.starts_with(dir))
}
