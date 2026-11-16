//! The deterministic autofix of merge requests (M4 C14, plan 14 C14,
//! ADR-0017 §1 / §1.2 / §2.1 and its Amendment C14): the pure parts — the
//! triggers and states of an attempt, the scope a candidate tree may
//! change, the conflicted-hunk rule, the bot's identity and its fixed
//! commit message. No LLM (M5); no I/O here.
//!
//! **What may change** ([`check_scope`]): every path the candidate changes
//! against the tree the fixer started from (`diff-tree -r --no-renames
//! --raw`) is decided alone, and the strictest verdict of all of them wins:
//!
//! 1. **Refused** (the candidate is discarded, `FG-GIT-0037`): a path
//!    that is no plain repository path ([`path_problem`]: `.git` in any
//!    spelling, `git~1`, a trailing dot or space, a backslash, a control
//!    character, `..`), two paths that differ only by case, a deleted
//!    file, a mode other than `100644` (no executable, symlink or
//!    submodule), the fixed denials of ADR-0017 §2.1 ([`fixed_denial`]:
//!    `.forge-ci.yml`, `.forge/**`, `CODEOWNERS`, build and toolchain
//!    files, agent configuration, secret-looking names), a file the target
//!    branch's `.forge-ci.yml` names in a script ([`ScopePolicy::script_paths`]),
//!    a path outside the fixer's `paths`, for a conflict a path that did
//!    not conflict, more than [`MAX_CHANGED_FILES`] files.
//! 2. **Proposal only** (nothing is pushed; the merge request's line names
//!    the paths): a migration ([`crate::risk::is_migration_path`]) or a
//!    protected path ([`crate::auto_merge::is_protected_path`]: forge's
//!    own configuration, CODEOWNERS, authentication and permission code),
//!    and — decided on the content by the caller — a binary file.
//! 3. Otherwise **allowed**.
//!
//! **Conflicts** ([`within_conflict_hunks`], ADR-0017 §1.2): the server
//! computes `merge-tree --write-tree` itself; a candidate may differ from
//! that automatic merge only inside the conflicted hunks of the conflicted
//! paths. Every line outside the `<<<<<<<` … `>>>>>>>` blocks must be kept,
//! in order, and no marker may be left.

use crate::ids::{MergeRequestId, ProjectId};
use crate::ObjectId;

/// The bot's display name (ADR-0017 §2.5: no login, no token).
pub const BOT_NAME: &str = "forge-autofix";
/// The bot's reserved email (`.invalid`: never a mailbox).
pub const BOT_EMAIL: &str = "autofix@forge.invalid";
/// The artifact a fixer job's runner uploads: a git bundle of one
/// transport commit on the input commit (only its tree is used).
pub const BUNDLE_ARTIFACT_PATH: &str = "autofix.bundle";
/// The ref inside the bundle.
pub const BUNDLE_REF: &str = "refs/forge/autofix";
/// The most files one candidate may change.
pub const MAX_CHANGED_FILES: usize = 100;
/// The largest changed blob read (secret scan, binary and hunk checks);
/// a larger one is refused (`too_large`).
pub const MAX_BLOB_BYTES: usize = 1024 * 1024;
/// The largest bundle the server imports.
pub const MAX_BUNDLE_BYTES: u64 = 64 * 1024 * 1024;
/// Attempts that ran a fixer, per merge request, in 24 hours (ADR-0017 §3).
pub const MAX_ATTEMPTS_PER_DAY: i64 = 6;
/// The trailer that names the attempt in the bot's commit message.
pub const ATTEMPT_TRAILER: &str = "Forge-Autofix-Attempt";

/// What started an attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AutofixTrigger {
    /// The merge request's `ci/pipeline` failed on its merge result.
    PipelineFailed,
    /// The merge request started to conflict with its target
    /// (`merge_request.conflict_detected`, plan 04 C-5).
    Conflict,
}

impl AutofixTrigger {
    pub const ALL: [AutofixTrigger; 2] = [Self::PipelineFailed, Self::Conflict];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::PipelineFailed => "pipeline_failed",
            Self::Conflict => "conflict",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.as_str() == raw)
    }
}

/// Where an attempt is (`forge.autofix_attempts.state`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AttemptState {
    /// Recorded by the trigger; no fixer chosen yet.
    Queued,
    /// The fixer job runs on a runner.
    Running,
    /// The bot's commit is on the source branch.
    Applied,
    /// The fixer changed nothing (the next fixer may try).
    NoChange,
    /// The candidate broke a rule (scope, preconditions, the bundle).
    Refused,
    /// The candidate touches a migration, a binary or a protected path:
    /// shown as a proposal, never pushed.
    Proposed,
    /// The fixer job failed or its output could not be read.
    Failed,
    /// Nothing ran (disabled, no fixer, a limit, the bot may not push).
    Skipped,
    /// The merge request moved on (a new head, closed) before it landed.
    Superseded,
}

impl AttemptState {
    pub const ALL: [AttemptState; 9] = [
        Self::Queued,
        Self::Running,
        Self::Applied,
        Self::NoChange,
        Self::Refused,
        Self::Proposed,
        Self::Failed,
        Self::Skipped,
        Self::Superseded,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Applied => "applied",
            Self::NoChange => "no_change",
            Self::Refused => "refused",
            Self::Proposed => "proposed",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
            Self::Superseded => "superseded",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.as_str() == raw)
    }

    /// Nothing happens to the attempt any more.
    pub fn is_terminal(self) -> bool {
        !matches!(self, Self::Queued | Self::Running)
    }
}

/// How a changed path changed (the `diff-tree --raw` status).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
    /// A type change (`T`) or anything else git reports.
    Other,
}

/// One path the candidate changes against the tree the fixer started from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateChange {
    pub path: String,
    pub kind: ChangeKind,
    /// Octal modes, `0` for the absent side.
    pub old_mode: u32,
    pub new_mode: u32,
}

/// The rules one candidate is checked against (all read from the
/// database and the target branch, never from the merge request's tree).
#[derive(Debug, Clone, Copy)]
pub struct ScopePolicy<'a> {
    /// The fixer's `paths` patterns (`**`, `*`; [`glob_matches`]).
    pub fixer_paths: &'a [String],
    /// For a conflict: the paths `merge-tree` listed. `None` for a
    /// pipeline failure.
    pub conflicted: Option<&'a [String]>,
    /// Every word of every script of the target branch's `.forge-ci.yml`
    /// (jobs and fixers): a changed path that is one of them (or
    /// `./<path>`) is a file the CI runs — refused.
    pub script_paths: &'a [String],
}

/// Why a candidate was refused or held as a proposal (a stable reason).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeFinding {
    pub path: String,
    pub reason: &'static str,
}

/// The verdict on a candidate's changed paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScopeVerdict {
    /// Every path may change.
    Allowed,
    /// Nothing may be pushed; these paths make it a proposal.
    Proposal(Vec<ScopeFinding>),
    /// The candidate breaks a rule: the first finding.
    Refused(ScopeFinding),
}

fn finding(path: &str, reason: &'static str) -> ScopeFinding {
    ScopeFinding {
        path: path.to_owned(),
        reason,
    }
}

/// Why `path` is no plain repository path (`None`: it is one).
pub fn path_problem(path: &str) -> Option<&'static str> {
    if path.is_empty() || path.len() > 4096 {
        return Some("path_invalid");
    }
    if path.starts_with('/') || path.contains('\\') || path.chars().any(char::is_control) {
        return Some("path_invalid");
    }
    for component in path.split('/') {
        let lower = component.to_ascii_lowercase();
        if component.is_empty() || component == "." || component == ".." {
            return Some("path_invalid");
        }
        // `.git` in every spelling Windows or macOS would fold onto it.
        let trimmed = lower.trim_end_matches(['.', ' ']);
        if trimmed == ".git" || lower.starts_with("git~") {
            return Some("git_dir");
        }
        if component.ends_with('.') || component.ends_with(' ') {
            return Some("path_invalid");
        }
    }
    None
}

/// The fixed denials of ADR-0017 §2.1 (no project setting loosens them):
/// the reason, or `None`.
pub fn fixed_denial(path: &str) -> Option<&'static str> {
    let lower = path.to_ascii_lowercase();
    let file = lower.rsplit('/').next().unwrap_or("");
    const FILES: &[&str] = &[
        ".forge-ci.yml",
        "codeowners",
        ".gitattributes",
        ".gitmodules",
        ".gitignore",
        "build.rs",
        "cargo.toml",
        "rustfmt.toml",
        ".rustfmt.toml",
        "clippy.toml",
        ".clippy.toml",
        "deny.toml",
        "package.json",
        ".npmrc",
        "makefile",
        "gnumakefile",
        ".envrc",
        ".pre-commit-config.yaml",
        "claude.md",
        "agents.md",
        "skill.md",
        ".cursorrules",
    ];
    if FILES.contains(&file) {
        return Some("forbidden_path");
    }
    if file.starts_with("rust-toolchain") || file.starts_with("dockerfile") {
        return Some("forbidden_path");
    }
    const DIRS: &[&str] = &[
        ".forge", ".cargo", ".vscode", ".idea", ".husky", ".claude", ".github", ".gitlab",
    ];
    let mut dirs = lower.split('/');
    dirs.next_back(); // the file itself
    if dirs.any(|d| DIRS.contains(&d)) || lower.starts_with("scripts/hooks/") {
        return Some("forbidden_path");
    }
    // Secret-looking names (ADR-0017 §2.1).
    if file.starts_with(".env")
        || file.starts_with("id_")
        || file.ends_with(".pem")
        || file.ends_with(".key")
        || file.contains("secret")
    {
        return Some("secret_path");
    }
    None
}

/// Whether `path` matches `pattern`: `/`-separated segments, `**` is any
/// number of segments (none included), `*` any characters inside one
/// segment, everything else literal (case-sensitive, like git).
pub fn glob_matches(pattern: &str, path: &str) -> bool {
    let pat: Vec<&str> = pattern.split('/').collect();
    let segs: Vec<&str> = path.split('/').collect();
    match_segments(&pat, &segs)
}

fn match_segments(pat: &[&str], segs: &[&str]) -> bool {
    match pat.split_first() {
        None => segs.is_empty(),
        Some((&"**", rest)) => (0..=segs.len()).any(|i| match_segments(rest, &segs[i..])),
        Some((p, rest)) => match segs.split_first() {
            Some((s, tail)) => {
                match_segment(p.as_bytes(), s.as_bytes()) && match_segments(rest, tail)
            }
            None => false,
        },
    }
}

fn match_segment(p: &[u8], s: &[u8]) -> bool {
    match p.split_first() {
        None => s.is_empty(),
        Some((b'*', rest)) => (0..=s.len()).any(|i| match_segment(rest, &s[i..])),
        Some((c, rest)) => s
            .split_first()
            .is_some_and(|(d, tail)| c == d && match_segment(rest, tail)),
    }
}

/// The verdict on `changes` under `policy` (module docs).
pub fn check_scope(changes: &[CandidateChange], policy: &ScopePolicy<'_>) -> ScopeVerdict {
    if changes.len() > MAX_CHANGED_FILES {
        let first = changes.first().map_or("", |c| c.path.as_str());
        return ScopeVerdict::Refused(finding(first, "too_many_files"));
    }
    let mut seen = std::collections::HashSet::new();
    let mut proposals = Vec::new();
    for c in changes {
        if let Some(reason) = refusal(c, policy) {
            return ScopeVerdict::Refused(finding(&c.path, reason));
        }
        if !seen.insert(c.path.to_lowercase()) {
            return ScopeVerdict::Refused(finding(&c.path, "case_collision"));
        }
        if crate::risk::is_migration_path(&c.path) {
            proposals.push(finding(&c.path, "migration"));
        } else if crate::auto_merge::is_protected_path(&c.path) {
            proposals.push(finding(&c.path, "protected_path"));
        }
    }
    if proposals.is_empty() {
        ScopeVerdict::Allowed
    } else {
        ScopeVerdict::Proposal(proposals)
    }
}

fn refusal(c: &CandidateChange, policy: &ScopePolicy<'_>) -> Option<&'static str> {
    if let Some(reason) = path_problem(&c.path) {
        return Some(reason);
    }
    match c.kind {
        ChangeKind::Deleted => return Some("deleted"),
        ChangeKind::Other => return Some("mode"),
        ChangeKind::Added | ChangeKind::Modified => {}
    }
    if c.new_mode != 0o100_644 || (c.kind == ChangeKind::Modified && c.old_mode != 0o100_644) {
        return Some("mode");
    }
    if let Some(reason) = fixed_denial(&c.path) {
        return Some(reason);
    }
    let dotted = format!("./{}", c.path);
    if policy
        .script_paths
        .iter()
        .any(|w| *w == c.path || *w == dotted)
    {
        return Some("ci_file");
    }
    if !policy.fixer_paths.iter().any(|p| glob_matches(p, &c.path)) {
        return Some("outside_fixer_paths");
    }
    if let Some(conflicted) = policy.conflicted {
        if !conflicted.contains(&c.path) {
            return Some("outside_conflict");
        }
    }
    None
}

/// The words of script lines that could name a file (split on
/// whitespace, quotes and shell punctuation; `./` kept).
pub fn script_words(lines: &[String]) -> Vec<String> {
    let mut out: Vec<String> = lines
        .iter()
        .flat_map(|l| {
            l.split(|c: char| c.is_whitespace() || "'\";|&<>()`$=".contains(c))
                .filter(|w| !w.is_empty() && !w.starts_with('-'))
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

/// A binary file by git's rule: a NUL in the first 8000 bytes.
pub fn is_binary(content: &[u8]) -> bool {
    content.iter().take(8000).any(|b| *b == 0)
}

fn is_marker(line: &[u8], marker: &[u8]) -> bool {
    line.starts_with(marker) && matches!(line.get(marker.len()), None | Some(b' ' | b'\n' | b'\r'))
}

/// Whether `candidate` keeps every line of `marked` (the file as
/// `merge-tree` wrote it, with its conflict blocks) outside the conflict
/// blocks, in order, and leaves no marker (module docs). A file without
/// any block must stay byte for byte; an unterminated block refuses.
pub fn within_conflict_hunks(marked: &[u8], candidate: &[u8]) -> bool {
    let lines = |b: &[u8]| -> Vec<Vec<u8>> {
        b.split_inclusive(|c| *c == b'\n')
            .map(<[u8]>::to_vec)
            .collect()
    };
    let starts = |l: &[u8]| is_marker(l, b"<<<<<<<");
    let ends = |l: &[u8]| is_marker(l, b">>>>>>>");
    // A resolution never keeps a block's start or end marker (a lone
    // `=======` is ordinary text, e.g. a Markdown heading).
    if lines(candidate).iter().any(|l| starts(l) || ends(l)) {
        return false;
    }
    // The literal pieces between the blocks.
    let mut pieces: Vec<Vec<u8>> = vec![Vec::new()];
    let mut inside = false;
    for line in lines(marked) {
        if inside {
            // Both sides of the block (and a diff3 base) are replaced.
            if ends(&line) {
                inside = false;
            }
            continue;
        }
        if starts(&line) {
            inside = true;
            pieces.push(Vec::new());
        } else if let Some(last) = pieces.last_mut() {
            last.extend_from_slice(&line);
        }
    }
    if inside {
        return false;
    }
    let (first, rest) = match pieces.split_first() {
        Some(split) => split,
        None => return candidate.is_empty(),
    };
    if rest.is_empty() {
        return candidate == first.as_slice();
    }
    let Some(mut tail) = candidate.strip_prefix(first.as_slice()) else {
        return false;
    };
    let (last, middles) = match rest.split_last() {
        Some(split) => split,
        None => return false,
    };
    for piece in middles {
        if piece.is_empty() {
            continue;
        }
        match find(tail, piece) {
            Some(at) => tail = &tail[at + piece.len()..],
            None => return false,
        }
    }
    tail.ends_with(last)
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// The bot's fixed commit message: `autofix(<trigger>): <summary>` and the
/// trailers (never `Signed-off-by`, ADR-0017 §2.6). The database
/// (`forge.autofix_attempts.commit_sha`) is the truth; the trailers can be
/// copied by anyone.
pub fn commit_message(
    trigger: AutofixTrigger,
    fixer: &str,
    attempt: i64,
    request_id: &str,
) -> String {
    let summary = match trigger {
        AutofixTrigger::PipelineFailed => format!("apply the {fixer} fixer"),
        AutofixTrigger::Conflict => format!("resolve the conflict with the {fixer} fixer"),
    };
    format!(
        "autofix({}): {summary}\n\n{ATTEMPT_TRAILER}: {attempt}\nForge-Autofix-Trigger: {}\nForge-Autofix-Fixer: {fixer}\nForge-Request-Id: {request_id}\n",
        trigger.as_str(),
        trigger.as_str(),
    )
}

/// A person's change of a project's autofix setting (audited, no event).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutofixChange {
    /// `autofix.settings_changed` (resource the project).
    SettingsChanged { project: ProjectId, enabled: bool },
}

impl AutofixChange {
    pub fn action(&self) -> &'static str {
        match self {
            Self::SettingsChanged { .. } => "autofix.settings_changed",
        }
    }

    pub fn project(&self) -> ProjectId {
        match self {
            Self::SettingsChanged { project, .. } => *project,
        }
    }

    pub fn to_value(&self) -> serde_json::Value {
        match self {
            Self::SettingsChanged { project, enabled } => serde_json::json!({
                "project_id": project.get(),
                "enabled": enabled,
            }),
        }
    }
}

/// What one finished attempt did, for the merge request's event
/// (`merge_request.autofix_finished`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutofixFinished {
    pub merge_request: MergeRequestId,
    pub iid: i64,
    pub attempt: i64,
    pub trigger: AutofixTrigger,
    pub fixer: Option<String>,
    pub state: AttemptState,
    pub reason: Option<String>,
    pub commit: Option<ObjectId>,
}

#[cfg(test)]
#[path = "autofix_tests.rs"]
mod tests;
