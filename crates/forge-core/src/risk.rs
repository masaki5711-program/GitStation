//! The deterministic risk rules (M3 X7, plan 04 M-6 layer 1, ADR-0023
//! Amendment X7). Pure: the same diff gives the same findings, with no
//! clock, no I/O and no model — so a prompt in a commit message or a file
//! can never lower a result.
//!
//! The input is what git said about the change between a merge request's
//! merge base and its head ([`ChangedFile`]), the content of the few files
//! whose text matters ([`content_kind`] picks them, [`content_findings`]
//! reads them) and the size of the target's tree ([`TreeTotals`], for the
//! mass-deletion rule). The output is [`Finding`]s, each a class of
//! [`RiskLevel`]:
//!
//! - `stop` — program destruction (mass deletion), database destruction
//!   (an applied migration changed, a destructive statement), a leak of a
//!   secret (a credential file by name), a data lifecycle job changed, a
//!   rollback step in a deploy file. Nothing lowers it, in any merge mode.
//!   Plus (W15 A2, plan 16 §9 item 1, ADR-0017 Amendment 2026-11-12) code
//!   that runs on developers' PCs: `agent_config`, a change to `.claude/**`,
//!   `CLAUDE.md` at any level, `.mcp.json` or `.gitstation/**` (names as a
//!   developer's file system folds them), or a symlink resolving to one;
//!   and `submodule_change`, a gitlink added or changed. They have no
//!   exemption yet; the required `forge/agent-lock` check (plan 16 MR A7)
//!   adds one for a change whose lock check is green.
//! - `warn` — authentication / permission code, CI or deploy
//!   configuration, a dependency manifest, a new migration, a symlink or a
//!   submodule, an object dropped or a privilege changed in SQL. The merge
//!   proceeds; the warning stays on the commit.
//!
//! A finding names the rule, the path and (for text) the line — **never the
//! matched text** (a secret or a table name must not be copied into a run
//! every reader of the project can see). Each rule is a row of [`RULES`]
//! with a fixed English reason.

use std::cmp::Reverse;

/// The most findings one file's text yields.
pub const MAX_FINDINGS_PER_FILE: usize = 20;
/// More changed files than this are not assessed one by one (a `stop`).
pub const MAX_CHANGED_FILES: usize = 5000;
/// Mass deletion (a): this many removed lines …
pub const MASS_DELETE_LINES: u64 = 500;
/// … or (b) this many deleted files …
pub const MASS_DELETE_FILES: u64 = 20;
/// … and this share (percent) of the target's text lines / files.
pub const MASS_DELETE_PERCENT: u64 = 30;

/// How serious a finding is (and the worst of them is the run's level).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RiskLevel {
    Ok,
    Warn,
    Stop,
}

impl RiskLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warn => "warn",
            Self::Stop => "stop",
        }
    }

    /// The level named `raw` (its `as_str`); `None` for anything else.
    pub fn parse(raw: &str) -> Option<Self> {
        [Self::Ok, Self::Warn, Self::Stop]
            .into_iter()
            .find(|l| l.as_str() == raw)
    }
}

/// One rule: a stable id, its level and the reason shown with a finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rule {
    pub id: &'static str,
    pub level: RiskLevel,
    pub reason: &'static str,
}

const fn rule(id: &'static str, level: RiskLevel, reason: &'static str) -> Rule {
    Rule { id, level, reason }
}

/// Every rule, by id.
pub const RULES: &[Rule] = &[
    rule(
        "applied_migration_changed",
        RiskLevel::Stop,
        "a migration that may already be applied was changed, removed or renamed; migrations are append-only, add a new one",
    ),
    rule(
        "sql_drop",
        RiskLevel::Stop,
        "a table, column, schema or database is dropped",
    ),
    rule(
        "sql_truncate",
        RiskLevel::Stop,
        "a table is truncated",
    ),
    rule(
        "sql_unbounded_write",
        RiskLevel::Stop,
        "a DELETE or UPDATE has no WHERE condition",
    ),
    rule(
        "sql_column_change",
        RiskLevel::Stop,
        "a column type is changed or a table or column is renamed",
    ),
    rule(
        "sql_drop_constraint",
        RiskLevel::Stop,
        "a constraint (a unique key or a foreign key, for one) is dropped",
    ),
    rule(
        "sql_disable_guard",
        RiskLevel::Stop,
        "a trigger or row level security is disabled",
    ),
    rule(
        "deploy_rollback",
        RiskLevel::Stop,
        "a deploy file rolls the database back or drops it",
    ),
    rule(
        "secret_file",
        RiskLevel::Stop,
        "a file that holds credentials by its name is added or changed",
    ),
    rule(
        "secret_in_content",
        RiskLevel::Stop,
        "a file holds something that looks like a private key or an access token",
    ),
    rule(
        "data_lifecycle_job",
        RiskLevel::Stop,
        "a backup, restore or retention job is changed",
    ),
    rule(
        "agent_config",
        RiskLevel::Stop,
        "a Claude Code configuration (.claude, CLAUDE.md, .mcp.json or .gitstation) changed; it runs on developers' PCs, so a person reviews it",
    ),
    rule(
        "submodule_change",
        RiskLevel::Stop,
        "a submodule was added or changed; a recursive checkout brings in its files, Claude Code configuration among them, unreviewed",
    ),
    rule(
        "mass_deletion_lines",
        RiskLevel::Stop,
        "500 or more lines are removed, 30 % or more of the target's text",
    ),
    rule(
        "mass_deletion_files",
        RiskLevel::Stop,
        "20 or more files are deleted, 30 % or more of the target's files",
    ),
    rule(
        "critical_content_unread",
        RiskLevel::Stop,
        "a new migration or a CI / deploy file could not be read (too large, or past the read limit), so it cannot be shown safe",
    ),
    rule(
        "too_many_changes",
        RiskLevel::Stop,
        "more than 5000 files changed; the change is too large to assess file by file",
    ),
    rule(
        "new_migration",
        RiskLevel::Warn,
        "a new database migration",
    ),
    rule(
        "sql_drop_object",
        RiskLevel::Warn,
        "an index, view, trigger or other object is dropped",
    ),
    rule(
        "sql_privilege",
        RiskLevel::Warn,
        "privileges are granted or revoked",
    ),
    rule(
        "auth_change",
        RiskLevel::Warn,
        "authentication or permission code or configuration changed",
    ),
    rule(
        "ci_deploy_config",
        RiskLevel::Warn,
        "CI, deploy or infrastructure configuration changed",
    ),
    rule(
        "dependency_change",
        RiskLevel::Warn,
        "a dependency manifest or lock file changed",
    ),
    rule(
        "special_entry",
        RiskLevel::Warn,
        "a symbolic link or a submodule was added or changed",
    ),
    rule(
        "no_open_merge_request",
        RiskLevel::Warn,
        "no open merge request has this commit as its head, so there is no target to compare with",
    ),
    rule(
        "target_missing",
        RiskLevel::Warn,
        "the target branch of the merge request no longer exists",
    ),
    rule(
        "no_common_history",
        RiskLevel::Warn,
        "the head shares no history with the target branch",
    ),
    rule(
        "non_utf8_path",
        RiskLevel::Warn,
        "a changed path is not UTF-8 and cannot be assessed",
    ),
    rule(
        "content_not_scanned",
        RiskLevel::Warn,
        "some changed files were not read for secrets or destructive statements (too many, or too large)",
    ),
    rule(
        "targets_not_assessed",
        RiskLevel::Warn,
        "the commit is the head of merge requests into more branches than are assessed",
    ),
];

/// One thing the rules found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub rule: &'static str,
    pub level: RiskLevel,
    pub reason: &'static str,
    pub path: Option<String>,
    pub line: Option<u32>,
}

impl Finding {
    pub fn new(rule: &'static str, level: RiskLevel, reason: &'static str) -> Self {
        Self {
            rule,
            level,
            reason,
            path: None,
            line: None,
        }
    }

    /// The finding of rule `id` (`None` for an id not in [`RULES`]).
    pub fn for_rule(id: &str) -> Option<Self> {
        RULES
            .iter()
            .find(|r| r.id == id)
            .map(|r| Self::new(r.id, r.level, r.reason))
    }

    /// The finding of the rule `id`, which is one of [`RULES`]. An id that
    /// is not fails closed (a `stop` that says so) instead of panicking.
    pub(crate) fn of(id: &'static str) -> Self {
        Self::for_rule(id)
            .unwrap_or_else(|| Self::new(id, RiskLevel::Stop, "an unknown rule matched"))
    }

    /// The same finding at `path`.
    #[must_use]
    pub fn at(mut self, path: &str) -> Self {
        self.path = Some(path.to_owned());
        self
    }

    /// The same finding on `line` (1-based).
    #[must_use]
    pub fn on_line(mut self, line: u32) -> Self {
        self.line = Some(line);
        self
    }
}

/// The worst level of `findings` ([`RiskLevel::Ok`] for none).
pub fn level(findings: &[Finding]) -> RiskLevel {
    findings
        .iter()
        .map(|f| f.level)
        .max()
        .unwrap_or(RiskLevel::Ok)
}

/// Worst first, then by path, line and rule: a stable order for the
/// summary and for the annotations a run can hold.
pub fn sort_findings(findings: &mut [Finding]) {
    findings.sort_by(|a, b| {
        (Reverse(a.level), &a.path, a.line, a.rule).cmp(&(
            Reverse(b.level),
            &b.path,
            b.line,
            b.rule,
        ))
    });
}

/// How a path changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
    TypeChanged,
}

/// One changed entry between the merge base and the head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedFile {
    /// The path after the change (before it, for a deletion).
    pub path: String,
    /// The path before a rename.
    pub old_path: Option<String>,
    pub status: ChangeStatus,
    /// Octal modes before (`0`: added) and after (`0`: deleted).
    pub old_mode: u32,
    pub new_mode: u32,
    /// Lines added / removed (`None`: binary or a submodule).
    pub added: Option<u64>,
    pub deleted: Option<u64>,
}

/// The size of the target's tree: its files and the text lines in them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TreeTotals {
    pub files: u64,
    pub lines: u64,
    /// The count stopped at a limit: the real totals are larger (shares
    /// computed from it only err towards `stop`).
    pub truncated: bool,
}

// ── paths ──

fn components(path: &str) -> impl Iterator<Item = &str> {
    path.split('/').filter(|c| !c.is_empty())
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn extension(name: &str) -> Option<&str> {
    name.rsplit_once('.')
        .map(|(_, e)| e)
        .filter(|e| !e.is_empty())
}

/// Words of a name: runs of letters and digits, lower-cased.
fn words(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_ascii_lowercase)
}

fn is_doc(path: &str) -> bool {
    let first = components(path).next().unwrap_or("");
    if matches!(first, "docs" | "doc") {
        return true;
    }
    extension(basename(path)).is_some_and(|e| {
        matches!(
            e.to_ascii_lowercase().as_str(),
            "md" | "txt" | "rst" | "adoc" | "png" | "jpg" | "jpeg" | "gif" | "svg"
        )
    })
}

fn is_test(path: &str) -> bool {
    if components(path).any(|c| {
        matches!(
            c,
            "tests" | "test" | "__tests__" | "fixtures" | "e2e" | "testdata"
        )
    }) {
        return true;
    }
    let name = basename(path);
    let stem = name.rsplit_once('.').map_or(name, |(s, _)| s);
    ["_test", "_tests", ".test", ".spec", "-test"]
        .iter()
        .any(|suffix| stem.ends_with(suffix))
}

/// A migration file: SQL under a `migrations` or `migrate` directory (M4
/// C8: the merge queue runs a merge request that touches one alone,
/// ADR-0030 §4 — the same rule as `forge/risk`'s).
pub fn is_migration_path(path: &str) -> bool {
    is_migration(path)
}

fn is_migration(path: &str) -> bool {
    let name = basename(path);
    extension(name).is_some_and(|e| e.eq_ignore_ascii_case("sql"))
        && components(path)
            .take_while(|c| *c != name)
            .any(|c| matches!(c, "migrations" | "migrate"))
}

const CI_DIRS: &[&str] = &[
    ".forge",
    ".github",
    ".gitlab",
    ".circleci",
    "deploy",
    "deployment",
    "ops",
    "infra",
    "k8s",
    "helm",
    "terraform",
    "ansible",
    "systemd",
];

fn is_ci_or_deploy(path: &str) -> bool {
    if is_doc(path) {
        return false;
    }
    let name = basename(path);
    let lower = name.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        ".gitlab-ci.yml" | "jenkinsfile" | "compose.yml"
    ) || lower.starts_with("dockerfile")
        || lower.starts_with("docker-compose")
    {
        return true;
    }
    components(path)
        .take_while(|c| *c != name)
        .any(|c| CI_DIRS.contains(&c))
}

const DEPENDENCY_FILES: &[&str] = &[
    "cargo.toml",
    "cargo.lock",
    "deny.toml",
    "package.json",
    "package-lock.json",
    "pnpm-lock.yaml",
    "yarn.lock",
    "pyproject.toml",
    "poetry.lock",
    "pipfile",
    "pipfile.lock",
    "go.mod",
    "go.sum",
    "gemfile",
    "gemfile.lock",
    "composer.json",
    "composer.lock",
    "pom.xml",
];

fn is_dependency_file(path: &str) -> bool {
    let lower = basename(path).to_ascii_lowercase();
    DEPENDENCY_FILES.contains(&lower.as_str())
        || (lower.starts_with("requirements") && lower.ends_with(".txt"))
}

const AUTH_WORDS: &[&str] = &[
    "auth",
    "authn",
    "authz",
    "authentication",
    "authorization",
    "authorize",
    "authorise",
    "oauth",
    "oidc",
    "saml",
    "ldap",
    "sso",
    "login",
    "password",
    "passwords",
    "credential",
    "credentials",
    "permission",
    "permissions",
    "acl",
    "acls",
    "rbac",
    "codeowners",
];

fn is_auth_path(path: &str) -> bool {
    components(path).any(|c| {
        let stem = c.rsplit_once('.').map_or(c, |(s, _)| s);
        words(stem).any(|w| AUTH_WORDS.contains(&w.as_str()))
    })
}

fn is_secret_file(path: &str) -> bool {
    let lower = basename(path).to_ascii_lowercase();
    if lower == ".env" {
        return true;
    }
    if let Some(suffix) = lower.strip_prefix(".env.") {
        return !matches!(
            suffix,
            "example" | "sample" | "template" | "dist" | "defaults"
        );
    }
    if matches!(
        lower.as_str(),
        "id_rsa" | "id_dsa" | "id_ecdsa" | "id_ed25519" | ".htpasswd" | ".netrc" | ".pgpass"
    ) {
        return true;
    }
    extension(&lower).is_some_and(|e| matches!(e, "p12" | "pfx" | "jks" | "keystore"))
}

const JOB_EXTENSIONS: &[&str] = &[
    "sh", "bash", "zsh", "ps1", "bat", "cmd", "py", "rb", "pl", "sql", "service", "timer", "cron",
    "crontab", "yml", "yaml", "toml", "ini", "conf", "cfg", "json",
];
const SOURCE_EXTENSIONS: &[&str] = &[
    "rs", "go", "java", "ts", "tsx", "js", "jsx", "c", "cc", "cpp", "h", "cs", "kt",
];
const JOB_DIRS: &[&str] = &["ops", "scripts", "deploy", "cron", "systemd", "jobs"];

/// A backup, restore or retention job: a script or job / unit / config file
/// by name, or any non-source file in an operations directory. Application
/// source code (a worker module that happens to be named for retention) is
/// no job file: a change there is the code review's, not a stop.
fn is_lifecycle_job(path: &str) -> bool {
    let name = basename(path);
    let stem = name.rsplit_once('.').map_or(name, |(s, _)| s);
    if !words(stem).any(|w| matches!(w.as_str(), "backup" | "backups" | "restore" | "retention")) {
        return false;
    }
    let ext = extension(name).map(str::to_ascii_lowercase);
    let ext = ext.as_deref().unwrap_or("");
    JOB_EXTENSIONS.contains(&ext)
        || (!SOURCE_EXTENSIONS.contains(&ext)
            && components(path)
                .take_while(|c| *c != name)
                .any(|c| JOB_DIRS.contains(&c)))
}

/// A code point a developer's file system drops from a name (NTFS and
/// APFS / HFS+ ignore the default-ignorable ones), and U+FFFD, which stands
/// for a byte of a non-UTF-8 path that was decoded lossily: such a name
/// could be anything there, so it is read without the unknown byte.
fn ignorable(c: char) -> bool {
    matches!(
        u32::from(c),
        0x00AD
            | 0x034F
            | 0x061C
            | 0x115F..=0x1160
            | 0x17B4..=0x17B5
            | 0x180B..=0x180F
            | 0x200B..=0x200F
            | 0x202A..=0x202E
            | 0x2060..=0x206F
            | 0x3164
            | 0xFE00..=0xFE0F
            | 0xFEFF
            | 0xFFA0
            | 0xFFF0..=0xFFF8
            | 0xFFFD
            | 0x1BCA0..=0x1BCA3
            | 0x1D173..=0x1D17A
            | 0xE0000..=0xE0FFF
    )
}

/// Whether one path component names `upper` (an upper-case ASCII name) on
/// a developer's PC: ignorable code points removed, an NTFS stream suffix
/// (`:` and what follows, `::$DATA`) dropped, trailing dots and spaces
/// trimmed (Win32 drops them), then compared by Unicode upper case (NTFS
/// folds `ſ` to `S` and `ı` to `I`; `ß` stays two letters and never matches).
fn names(component: &str, upper: &str) -> bool {
    on_disk_upper(component) == upper
}

/// One path component as a developer's file system reads it, upper-cased
/// ([`names`]' rule: ignorable code points removed, an NTFS stream suffix
/// dropped, trailing dots and spaces trimmed, Unicode upper case). The
/// simple editor's path check (forge-git `file_commit`) compares it with
/// `.GIT` and git's 8.3 short name `GIT~<n>`.
pub fn on_disk_upper(component: &str) -> String {
    let kept: String = component.chars().filter(|c| !ignorable(*c)).collect();
    let stream_free = kept.split(':').next().unwrap_or("");
    let trimmed = stream_free.trim_end_matches(['.', ' ']);
    trimmed.chars().flat_map(char::to_uppercase).collect()
}

const AGENT_CONFIG_FILES: &[&str] = &["CLAUDE.MD", "CLAUDE.LOCAL.MD", ".MCP.JSON"];
const AGENT_CONFIG_DIRS: &[&str] = &[".CLAUDE", ".GITSTATION"];

/// Claude Code configuration, which runs (hooks, MCP servers) or steers an
/// agent on every developer's PC that checks the repository out: anything
/// under a `.claude` or `.gitstation` directory, a `CLAUDE.md` /
/// `CLAUDE.local.md` at any level, a `.mcp.json` at any level. Whole
/// components are compared, so `docs/.claude-notes.md` and `my.mcp.json`
/// are not; each as the developer's file system reads it ([`names`]), so
/// `.Claude/`, `.claude./` and `.cl<U+200B>aude/` are.
fn is_agent_config(path: &str) -> bool {
    let name = basename(path);
    AGENT_CONFIG_FILES.iter().any(|n| names(name, n))
        || components(path).any(|c| AGENT_CONFIG_DIRS.iter().any(|d| names(c, d)))
}

/// The findings of a symbolic link at `link` whose target (the blob's
/// bytes) is `target`: `agent_config` when the target, resolved against
/// the link's directory (`.` and `..` applied, `\` read as `/`), names
/// Claude Code configuration — a link outside `.claude` that a hook or a
/// setting reaches through. An absolute target is read as it is; `..` past
/// the repository's root is dropped and the rest still read.
pub fn symlink_findings(link: &str, target: &[u8]) -> Vec<Finding> {
    let target = String::from_utf8_lossy(target).replace('\\', "/");
    let absolute = target.starts_with('/') || target.get(1..2) == Some(":");
    let mut resolved: Vec<&str> = if absolute {
        Vec::new()
    } else {
        let mut dir: Vec<&str> = components(link).collect();
        dir.pop();
        dir
    };
    for part in components(&target) {
        match part {
            "." => {}
            ".." => {
                resolved.pop();
            }
            other => resolved.push(other),
        }
    }
    if is_agent_config(&resolved.join("/")) {
        vec![Finding::of("agent_config").at(link)]
    } else {
        Vec::new()
    }
}

fn add(findings: &mut Vec<Finding>, id: &'static str, path: &str) {
    findings.push(Finding::of(id).at(path));
}

/// The findings the path, status and modes of one changed entry give.
pub fn path_findings(file: &ChangedFile) -> Vec<Finding> {
    let mut found = Vec::new();
    let path = file.path.as_str();
    let old_is_migration = file.old_path.as_deref().is_some_and(is_migration);
    let present = file.status != ChangeStatus::Deleted;

    if is_migration(path) || old_is_migration {
        match file.status {
            ChangeStatus::Modified | ChangeStatus::Deleted | ChangeStatus::TypeChanged => {
                add(&mut found, "applied_migration_changed", path);
            }
            ChangeStatus::Renamed if old_is_migration => {
                add(&mut found, "applied_migration_changed", path);
            }
            ChangeStatus::Added | ChangeStatus::Renamed => {
                add(&mut found, "new_migration", path);
            }
        }
    }
    // Neither docs nor tests are exempt: a CLAUDE.md is both a doc and a
    // prompt, and a deletion or a rename out of `.claude` changes what runs.
    if is_agent_config(path) || file.old_path.as_deref().is_some_and(is_agent_config) {
        add(&mut found, "agent_config", path);
    }
    let quiet = is_doc(path) || is_test(path);
    if present && !quiet && is_secret_file(path) {
        add(&mut found, "secret_file", path);
    }
    if !quiet && is_lifecycle_job(path) {
        add(&mut found, "data_lifecycle_job", path);
    }
    if !quiet && is_auth_path(path) {
        add(&mut found, "auth_change", path);
    }
    if is_ci_or_deploy(path) && !is_test(path) {
        add(&mut found, "ci_deploy_config", path);
    }
    if is_dependency_file(path) {
        add(&mut found, "dependency_change", path);
    }
    if present && matches!(file.new_mode, 0o120_000 | 0o160_000) {
        add(&mut found, "special_entry", path);
    }
    // A recursive clone or `submodule update` checks the other repository
    // out, its `.claude/` included, which this check never sees.
    if present && file.new_mode == 0o160_000 {
        add(&mut found, "submodule_change", path);
    }
    found
}

// ── content ──

/// What the text of a file is read for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentKind {
    /// A new migration: destructive statements.
    Sql,
    /// A CI / deploy / infrastructure file: a database rollback.
    Deploy,
}

/// Whether (and for what) `file`'s text at the head is read. A modified
/// migration is a `stop` by its path already.
pub fn content_kind(file: &ChangedFile) -> Option<ContentKind> {
    if file.status == ChangeStatus::Deleted || file.status == ChangeStatus::TypeChanged {
        return None;
    }
    let path = file.path.as_str();
    if is_migration(path) && matches!(file.status, ChangeStatus::Added | ChangeStatus::Renamed) {
        return Some(ContentKind::Sql);
    }
    (is_ci_or_deploy(path) && !is_test(path)).then_some(ContentKind::Deploy)
}

/// The findings in `content`, the text of `path`.
pub fn content_findings(kind: ContentKind, path: &str, content: &str) -> Vec<Finding> {
    let mut found = match kind {
        ContentKind::Sql => sql_findings(path, content),
        ContentKind::Deploy => deploy_findings(path, content),
    };
    found.truncate(MAX_FINDINGS_PER_FILE);
    found
}

const ROLLBACK_MARKS: &[&str] = &[
    "migrate revert",
    "migration revert",
    "db:rollback",
    "dropdb",
    "drop database",
];

fn deploy_findings(path: &str, content: &str) -> Vec<Finding> {
    let mut found = Vec::new();
    for (index, line) in content.lines().enumerate() {
        if found.len() >= MAX_FINDINGS_PER_FILE {
            break;
        }
        let lower = line.to_ascii_lowercase();
        if ROLLBACK_MARKS.iter().any(|m| lower.contains(m)) {
            let number = u32::try_from(index + 1).unwrap_or(u32::MAX);
            found.push(Finding::of("deploy_rollback").at(path).on_line(number));
        }
    }
    found
}

/// One lexical token of SQL: an upper-cased word, a quoted identifier
/// (`"x`), a string (`'`) or one symbol; and its line.
struct Tok {
    text: String,
    line: u32,
}

const PLPGSQL_BLOCK: &str = concat!("BE", "GIN");

/// The statements of `content`: comments and string contents dropped,
/// split at `;` and at dollar-quote delimiters (so a function body is read
/// as statements of its own).
fn sql_statements(content: &str) -> Vec<Vec<Tok>> {
    let chars: Vec<char> = content.chars().collect();
    let mut statements: Vec<Vec<Tok>> = Vec::new();
    let mut current: Vec<Tok> = Vec::new();
    let mut line: u32 = 1;
    let mut i = 0;
    let at = |i: usize| chars.get(i).copied().unwrap_or('\0');
    while i < chars.len() {
        let c = at(i);
        if c == '\n' {
            line = line.saturating_add(1);
            i += 1;
        } else if c.is_whitespace() {
            i += 1;
        } else if c == '-' && at(i + 1) == '-' {
            while i < chars.len() && at(i) != '\n' {
                i += 1;
            }
        } else if c == '/' && at(i + 1) == '*' {
            i += 2;
            while i < chars.len() && !(at(i) == '*' && at(i + 1) == '/') {
                if at(i) == '\n' {
                    line = line.saturating_add(1);
                }
                i += 1;
            }
            i += 2;
        } else if c == '\'' || c == '"' {
            let start = line;
            i += 1;
            let mut name = String::new();
            while i < chars.len() {
                if at(i) == c {
                    if at(i + 1) == c {
                        i += 2;
                        continue;
                    }
                    break;
                }
                if at(i) == '\n' {
                    line = line.saturating_add(1);
                }
                if c == '"' {
                    name.push(at(i).to_ascii_uppercase());
                }
                i += 1;
            }
            i += 1;
            let text = if c == '\'' {
                "'".to_owned()
            } else {
                format!("\"{name}")
            };
            current.push(Tok { text, line: start });
        } else if c == '$' && dollar_tag_len(&chars, i).is_some() {
            i += dollar_tag_len(&chars, i).unwrap_or(1);
            statements.push(std::mem::take(&mut current));
        } else if c == ';' {
            i += 1;
            statements.push(std::mem::take(&mut current));
        } else if c.is_ascii_alphanumeric() || c == '_' || c == '$' {
            let mut word = String::new();
            while i < chars.len() && (at(i).is_ascii_alphanumeric() || at(i) == '_' || at(i) == '$')
            {
                word.push(at(i).to_ascii_uppercase());
                i += 1;
            }
            current.push(Tok { text: word, line });
        } else {
            current.push(Tok {
                text: c.to_string(),
                line,
            });
            i += 1;
        }
    }
    statements.push(current);
    statements.retain(|s| !s.is_empty());
    statements
}

/// The length of a dollar-quote delimiter (`$$`, `$tag$`) at `i`.
fn dollar_tag_len(chars: &[char], i: usize) -> Option<usize> {
    let mut j = i + 1;
    while chars
        .get(j)
        .is_some_and(|c| c.is_ascii_alphabetic() || *c == '_')
    {
        j += 1;
    }
    (chars.get(j) == Some(&'$')).then_some(j - i + 1)
}

fn word(tokens: &[Tok], i: usize) -> &str {
    tokens.get(i).map_or("", |t| t.text.as_str())
}

/// Whether the statement has a WHERE that limits anything: `WHERE TRUE`
/// and `WHERE 1 = 1` do not.
fn has_real_where(tokens: &[Tok]) -> bool {
    tokens.iter().enumerate().any(|(i, t)| {
        t.text == "WHERE"
            && !(word(tokens, i + 1) == "TRUE"
                || (word(tokens, i + 1) == "1"
                    && word(tokens, i + 2) == "="
                    && word(tokens, i + 3) == "1"))
    })
}

fn sql_findings(path: &str, content: &str) -> Vec<Finding> {
    let mut found: Vec<Finding> = Vec::new();
    for statement in sql_statements(content) {
        let tokens = skip_control_flow(&statement);
        let Some(first) = tokens.first() else {
            continue;
        };
        let line = first.line;
        let mut hit = |id: &'static str| {
            let f = Finding::of(id).at(path).on_line(line);
            if !found.contains(&f) {
                found.push(f);
            }
        };
        match first.text.as_str() {
            "DROP" => match word(tokens, 1) {
                "TABLE" | "SCHEMA" | "DATABASE" => hit("sql_drop"),
                _ => hit("sql_drop_object"),
            },
            "TRUNCATE" => hit("sql_truncate"),
            "GRANT" | "REVOKE" => hit("sql_privilege"),
            "ALTER" if word(tokens, 1) == "TABLE" => alter_table(tokens, &mut hit),
            _ => {}
        }
        for (i, t) in tokens.iter().enumerate() {
            let starts_statement = i == 0 || word(tokens, i - 1) == "(";
            let dml = starts_statement
                && match t.text.as_str() {
                    "DELETE" => word(tokens, i + 1) == "FROM",
                    "UPDATE" => true,
                    _ => false,
                };
            if dml && !has_real_where(dml_segment(tokens, i)) {
                hit("sql_unbounded_write");
            }
        }
        if found.len() >= MAX_FINDINGS_PER_FILE {
            break;
        }
    }
    found
}

/// The statement without its leading control flow: a block keyword (spelled
/// in two parts: the db-write-boundary gate reads a plain string literal of
/// it as a transaction statement), `ELSE` / `LOOP` / `THEN`, and an
/// `IF … THEN` / `WHILE … LOOP` condition (plpgsql bodies read as statements).
fn skip_control_flow(tokens: &[Tok]) -> &[Tok] {
    let mut rest = tokens;
    loop {
        match word(rest, 0) {
            w if w == PLPGSQL_BLOCK || matches!(w, "ELSE" | "LOOP" | "THEN") => {
                rest = rest.get(1..).unwrap_or(&[]);
            }
            "IF" | "ELSIF" | "ELSEIF" | "WHILE" => {
                let end = rest
                    .iter()
                    .position(|t| matches!(t.text.as_str(), "THEN" | "LOOP"))
                    .map_or(rest.len(), |p| p + 1);
                rest = rest.get(end..).unwrap_or(&[]);
            }
            _ => return rest,
        }
    }
}

/// The tokens of the data-modifying command that starts at `from`: up to the
/// parenthesis that closes the one it sits in (a CTE), else the statement.
fn dml_segment(tokens: &[Tok], from: usize) -> &[Tok] {
    let mut depth = 0usize;
    for (offset, t) in tokens.iter().enumerate().skip(from) {
        match t.text.as_str() {
            "(" => depth += 1,
            ")" if depth == 0 => return tokens.get(from..offset).unwrap_or(&[]),
            ")" => depth -= 1,
            _ => {}
        }
    }
    tokens.get(from..).unwrap_or(&[])
}

/// The sub-commands of one `ALTER TABLE` statement.
fn alter_table(tokens: &[Tok], hit: &mut impl FnMut(&'static str)) {
    for (i, t) in tokens.iter().enumerate() {
        match t.text.as_str() {
            "DROP" => match word(tokens, i + 1) {
                "NOT" | "DEFAULT" | "IDENTITY" | "EXPRESSION" => {}
                "CONSTRAINT" => hit("sql_drop_constraint"),
                _ => hit("sql_drop"),
            },
            "TYPE" if i >= 1 && word(tokens, i - 1) == "DATA" => hit("sql_column_change"),
            "TYPE" if i >= 2 && matches!(word(tokens, i - 2), "ALTER" | "COLUMN") => {
                hit("sql_column_change");
            }
            "RENAME" => hit("sql_column_change"),
            "DISABLE" if matches!(word(tokens, i + 1), "TRIGGER" | "ROW") => {
                hit("sql_disable_guard");
            }
            _ => {}
        }
    }
}

// ── mass deletion ──

/// The mass-deletion rule (module docs; plan 04 M-6): the lines removed
/// (every entry's, renames and edits included) are [`MASS_DELETE_LINES`] or
/// more and [`MASS_DELETE_PERCENT`] % or more of the target's text lines,
/// or the files deleted (a rename is none) are [`MASS_DELETE_FILES`] or
/// more and that share of the target's files. Binary files count as files,
/// never as lines. No exception is made for generated files or lock files.
pub fn mass_deletion(files: &[ChangedFile], totals: &TreeTotals) -> Option<Finding> {
    let lines: u64 = files
        .iter()
        .map(|f| f.deleted.unwrap_or(0))
        .fold(0, u64::saturating_add);
    let removed_files = u64::try_from(
        files
            .iter()
            .filter(|f| f.status == ChangeStatus::Deleted)
            .count(),
    )
    .unwrap_or(u64::MAX);
    let share = |part: u64, whole: u64| {
        whole > 0 && u128::from(part) * 100 >= u128::from(whole) * u128::from(MASS_DELETE_PERCENT)
    };
    if lines >= MASS_DELETE_LINES && share(lines, totals.lines) {
        return Some(Finding::of("mass_deletion_lines"));
    }
    if removed_files >= MASS_DELETE_FILES && share(removed_files, totals.files) {
        return Some(Finding::of("mass_deletion_files"));
    }
    None
}

#[cfg(test)]
#[path = "risk_tests.rs"]
mod tests;
