//! The clap command tree. Descriptions live in `registry.rs` (one source for
//! `help --json`); a test keeps the two in step.

use clap::{Args, Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "gstn",
    version,
    about = "GitStation client (REST API v1). Machine-readable: `gstn help --json`.",
    disable_help_subcommand = true
)]
pub struct Cli {
    /// JSON output (stdout), JSON errors (stderr).
    #[arg(long, global = true)]
    pub json: bool,
    /// Server base URL (overrides GSTN_HOST and config.toml).
    #[arg(long, global = true, value_name = "URL")]
    pub host: Option<String>,
    /// Top-level fields to keep in --json output.
    #[arg(long, global = true, value_delimiter = ',', value_name = "A,B")]
    pub fields: Vec<String>,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Log in, check, log out; git credential helper.
    #[command(disable_help_subcommand = true)]
    Auth {
        #[command(subcommand)]
        command: AuthCommand,
    },
    /// Projects: list, view, create, edit, visibility, move, delete, restore.
    #[command(disable_help_subcommand = true)]
    Project {
        #[command(subcommand)]
        command: ProjectCommand,
    },
    /// Groups: list, view, create, edit, move.
    #[command(disable_help_subcommand = true)]
    Group {
        #[command(subcommand)]
        command: GroupCommand,
    },
    /// Direct members of a project or a group.
    #[command(disable_help_subcommand = true)]
    Member {
        #[command(subcommand)]
        command: MemberCommand,
    },
    /// Protected ref rules of a project.
    #[command(name = "protected-ref", disable_help_subcommand = true)]
    ProtectedRef {
        #[command(subcommand)]
        command: ProtectedRefCommand,
    },
    /// Approval rules of a project (M3 R5): list, set, remove.
    #[command(name = "approval-rule", disable_help_subcommand = true)]
    ApprovalRule {
        #[command(subcommand)]
        command: ApprovalRuleCommand,
    },
    /// A project's merge settings (M3 R6): show, set.
    #[command(name = "merge-settings", disable_help_subcommand = true)]
    MergeSettings {
        #[command(subcommand)]
        command: MergeSettingsCommand,
    },
    /// Suspend CODEOWNERS enforcement for a while, or enforce it again (M3 R6).
    #[command(name = "codeowners-enforcement", disable_help_subcommand = true)]
    CodeownersEnforcement {
        #[command(subcommand)]
        command: CodeownersCommand,
    },
    /// A protected branch rule's merge conditions (M3 R6): set, remove.
    #[command(name = "merge-rule", disable_help_subcommand = true)]
    MergeRule {
        #[command(subcommand)]
        command: MergeRuleCommand,
    },
    /// Check runs of a commit (M3 C1): report (a service account), list.
    #[command(disable_help_subcommand = true)]
    Checks {
        #[command(subcommand)]
        command: ChecksCommand,
    },
    /// A project's integration binding (M3 I1a): list, set, remove.
    #[command(name = "integration-binding", disable_help_subcommand = true)]
    IntegrationBinding {
        #[command(subcommand)]
        command: IntegrationBindingCommand,
    },
    /// Merge requests of a project: create, list, view, edit, close, reopen.
    #[command(disable_help_subcommand = true)]
    Mr {
        #[command(subcommand)]
        command: MrCommand,
    },
    /// Namespace paths.
    #[command(disable_help_subcommand = true)]
    Namespace {
        #[command(subcommand)]
        command: NamespaceCommand,
    },
    /// Read a project's repository: refs, resolve, tree, file.
    #[command(disable_help_subcommand = true)]
    Browse {
        #[command(subcommand)]
        command: BrowseCommand,
    },
    /// Users: search by name; administrators: list, disable, sessions, rename.
    #[command(disable_help_subcommand = true)]
    User {
        #[command(subcommand)]
        command: UserCommand,
    },
    /// Your personal access tokens: list, revoke (create one in the browser).
    #[command(disable_help_subcommand = true)]
    Token {
        #[command(subcommand)]
        command: TokenCommand,
    },
    /// Administration: service accounts (M3 S0), integrations (M3 I1a).
    #[command(disable_help_subcommand = true)]
    Admin {
        #[command(subcommand)]
        command: AdminCommand,
    },
    /// Call any REST API v1 route.
    Api(ApiArgs),
    /// Describe the commands (`--json` for machines).
    Help {
        /// Limit to one command.
        command: Vec<String>,
    },
    /// Machine-readable description of one command.
    Schema {
        #[arg(required = true)]
        command: Vec<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum AuthCommand {
    /// Verify a token and store it.
    Login {
        /// Read the token from stdin.
        #[arg(long)]
        with_token: bool,
    },
    /// Who the stored token belongs to.
    Status,
    /// Remove the stored token.
    Logout,
    /// git credential helper for the configured host; --install registers it.
    #[command(name = "git-credential")]
    GitCredential {
        #[arg(value_enum, required_unless_present = "install")]
        operation: Option<GitOperation>,
        /// Register as `credential.<host>.helper` in the global git config.
        #[arg(long, conflicts_with = "operation")]
        install: bool,
    },
}

/// The largest text the client sends in one field (the server has its own,
/// stricter rules; this only stops absurd input before a request).
const MAX_TEXT_BYTES: usize = 4096;

/// A server id: a positive integer that fits the API's int64.
pub fn parse_id(raw: &str) -> Result<i64, String> {
    match raw.parse::<i64>() {
        Ok(n) if n >= 1 => Ok(n),
        _ => Err("an id is a positive whole number".to_string()),
    }
}

/// A full commit id (40 or 64 hex digits), lower-cased. It is formatted
/// into the request path, so nothing else may pass.
pub fn parse_oid(raw: &str) -> Result<String, String> {
    if matches!(raw.len(), 40 | 64) && raw.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(raw.to_ascii_lowercase())
    } else {
        Err("a commit id is 40 (or 64) hexadecimal digits".to_string())
    }
}

fn parse_limit(raw: &str) -> Result<u32, String> {
    match raw.parse::<u32>() {
        Ok(n) if (1..=100).contains(&n) => Ok(n),
        _ => Err("--limit is a whole number from 1 to 100".to_string()),
    }
}

fn check_text(raw: &str) -> Result<(), String> {
    if raw.len() > MAX_TEXT_BYTES {
        return Err(format!("at most {MAX_TEXT_BYTES} bytes"));
    }
    if raw
        .chars()
        .any(|c| c.is_control() && c != '\n' && c != '\t')
    {
        return Err("no control characters".to_string());
    }
    Ok(())
}

/// A required, non-blank single-line text (a path, a name, a pattern, a cursor).
pub fn parse_text(raw: &str) -> Result<String, String> {
    if raw.trim().is_empty() {
        return Err("must not be empty".to_string());
    }
    if raw.chars().any(char::is_control) {
        return Err("no control characters".to_string());
    }
    check_text(raw)?;
    Ok(raw.to_string())
}

/// A free text that may be empty (a description).
pub fn parse_description(raw: &str) -> Result<String, String> {
    check_text(raw)?;
    Ok(raw.to_string())
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Guest,
    Reporter,
    Developer,
    Maintainer,
    Owner,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Guest => "guest",
            Self::Reporter => "reporter",
            Self::Developer => "developer",
            Self::Maintainer => "maintainer",
            Self::Owner => "owner",
        }
    }
}

/// The roles that may be a protected ref's minimum pusher.
#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum PushRole {
    Developer,
    Maintainer,
    Owner,
}

impl PushRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Developer => "developer",
            Self::Maintainer => "maintainer",
            Self::Owner => "owner",
        }
    }
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Visibility {
    Private,
    Internal,
}

impl Visibility {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Private => "private",
            Self::Internal => "internal",
        }
    }
}

/// Paging of a list command.
#[derive(Args, Debug)]
pub struct ListArgs {
    /// Rows per page, 1..=100 (server default 20; 100 with --all).
    #[arg(long, value_parser = parse_limit, value_name = "N")]
    pub limit: Option<u32>,
    /// Continue after this `next_cursor` (from a previous page).
    #[arg(long, value_parser = parse_text, value_name = "CURSOR", conflicts_with = "all")]
    pub cursor: Option<String>,
    /// Follow next_cursor and merge every page (bounded).
    #[arg(long)]
    pub all: bool,
}

#[derive(Args, Debug)]
pub struct IdArg {
    /// The numeric id.
    #[arg(value_parser = parse_id)]
    pub id: i64,
}

#[derive(Subcommand, Debug)]
pub enum ProjectCommand {
    /// List the projects you can see.
    List(ListArgs),
    /// Show one project.
    View(IdArg),
    /// Create a project in a namespace.
    Create {
        /// The parent namespace id (a user's, a group's, or from `namespace resolve`).
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        namespace_id: i64,
        /// The path segment of the new project.
        #[arg(long, value_parser = parse_text)]
        path: String,
        #[arg(long, value_parser = parse_text)]
        display_name: String,
        #[arg(long, value_parser = parse_description)]
        description: Option<String>,
        /// private | internal (server default: private).
        #[arg(long, value_enum)]
        visibility: Option<Visibility>,
    },
    /// Change a project's display name or description.
    Edit {
        #[arg(value_parser = parse_id)]
        id: i64,
        #[arg(long, value_parser = parse_text)]
        display_name: Option<String>,
        #[arg(long, value_parser = parse_description)]
        description: Option<String>,
    },
    /// Set a project's visibility (private | internal). Needs --yes.
    Visibility {
        #[arg(value_parser = parse_id)]
        id: i64,
        #[arg(long, value_enum)]
        visibility: Visibility,
        /// Confirm the change (gstn never prompts).
        #[arg(long)]
        yes: bool,
    },
    /// Move a project to another namespace and/or rename it (--path alone).
    /// The old path stops working for others. Needs --yes.
    Move {
        #[arg(value_parser = parse_id)]
        id: i64,
        /// The new parent namespace id (a group's or a user's).
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        namespace_id: Option<i64>,
        /// The new last path segment.
        #[arg(long, value_parser = parse_text)]
        path: Option<String>,
        /// Take back an old path of this very project (A -> B -> A).
        #[arg(long)]
        reclaim: bool,
        /// Confirm the move (gstn never prompts).
        #[arg(long)]
        yes: bool,
    },
    /// Delete a project (restorable for the grace period).
    Delete {
        #[arg(value_parser = parse_id)]
        id: i64,
        /// Confirm the deletion (gstn never prompts).
        #[arg(long)]
        yes: bool,
    },
    /// Restore a deleted project within its grace period.
    Restore(IdArg),
}

#[derive(Subcommand, Debug)]
pub enum GroupCommand {
    /// List the groups you can see.
    List(ListArgs),
    /// Show one group.
    View(IdArg),
    /// Create a top-level group, or a subgroup with --parent-id.
    Create {
        #[arg(long, value_parser = parse_text)]
        path: String,
        #[arg(long, value_parser = parse_text)]
        display_name: String,
        #[arg(long, value_parser = parse_description)]
        description: Option<String>,
        /// Create it inside this group.
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        parent_id: Option<i64>,
    },
    /// Change a group's display name or description.
    Edit {
        #[arg(value_parser = parse_id)]
        id: i64,
        #[arg(long, value_parser = parse_text)]
        display_name: Option<String>,
        #[arg(long, value_parser = parse_description)]
        description: Option<String>,
    },
    /// Move a group (and its subtree) and/or rename it. Needs --yes.
    Move {
        #[arg(value_parser = parse_id)]
        id: i64,
        /// The new parent group's namespace id.
        #[arg(long, alias = "namespace", value_parser = parse_id, value_name = "ID", conflicts_with = "top_level")]
        namespace_id: Option<i64>,
        /// Move to the top level (administrators only).
        #[arg(long)]
        top_level: bool,
        /// The new last path segment.
        #[arg(long, value_parser = parse_text)]
        path: Option<String>,
        /// Take back an old path of this group's subtree.
        #[arg(long)]
        reclaim: bool,
        /// Confirm the move (gstn never prompts).
        #[arg(long)]
        yes: bool,
    },
}

/// Which object's members: exactly one of the two.
#[derive(Args, Debug)]
#[group(required = true, multiple = false)]
pub struct MemberTarget {
    /// The project id.
    #[arg(long, value_parser = parse_id, value_name = "ID")]
    pub project: Option<i64>,
    /// The group id.
    #[arg(long, value_parser = parse_id, value_name = "ID")]
    pub group: Option<i64>,
}

#[derive(Subcommand, Debug)]
pub enum MemberCommand {
    /// List direct members.
    List {
        #[command(flatten)]
        target: MemberTarget,
        /// Members per page, 1..=100 (server default 20; 100 with --all).
        #[arg(long, value_parser = parse_limit, value_name = "N")]
        limit: Option<u32>,
        /// Continue after this user id (a previous page's next_after).
        #[arg(long, value_parser = parse_id, value_name = "USER_ID", conflicts_with = "all")]
        after: Option<i64>,
        /// Follow next_after and merge every page (bounded).
        #[arg(long)]
        all: bool,
    },
    /// Make a user a direct member.
    Add {
        #[command(flatten)]
        target: MemberTarget,
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        user_id: i64,
        #[arg(long, value_enum)]
        role: Role,
    },
    /// Change a member's direct role.
    #[command(name = "set-role")]
    SetRole {
        #[command(flatten)]
        target: MemberTarget,
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        user_id: i64,
        #[arg(long, value_enum)]
        role: Role,
    },
    /// Remove a direct member.
    Remove {
        #[command(flatten)]
        target: MemberTarget,
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        user_id: i64,
        /// Confirm the removal (gstn never prompts).
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum ApprovalRuleCommand {
    /// List a project's approval rules.
    List {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
    },
    /// Create or replace a rule (Maintainer): how many approvals, for which
    /// targets, from whom (no --user and no --group: any eligible person).
    /// Refused (FG-REVIEW-0018) when the rule changed after gstn read it:
    /// run it again.
    Set {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
        /// The rule's name (a-z 0-9 _ . -).
        #[arg(long, value_parser = crate::mr_approvals::parse_rule_name)]
        name: String,
        /// How many counted approvals it needs (1..=20).
        #[arg(long, value_parser = clap::value_parser!(i32).range(1..=20))]
        required: i32,
        /// A protected-ref pattern (refs/heads/main, refs/heads/release/*);
        /// absent: every target.
        #[arg(long, value_parser = parse_text)]
        target_pattern: Option<String>,
        /// A username who may approve (repeatable).
        #[arg(long, value_parser = parse_text)]
        user: Vec<String>,
        /// A group full path whose direct members may approve (repeatable).
        #[arg(long, value_parser = parse_text)]
        group: Vec<String>,
    },
    /// Remove a rule.
    Remove {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
        #[arg(long, value_parser = crate::mr_approvals::parse_rule_name)]
        name: String,
        /// Confirm the removal (gstn never prompts).
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum MergeSettingsCommand {
    /// Show a project's merge settings and CODEOWNERS enforcement.
    Show {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
    },
    /// Change a project's merge settings (Maintainer); options left out keep
    /// their value. Refused (FG-REVIEW-0017) when they changed after gstn
    /// read them: run it again.
    Set {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
        /// An allowed method (repeatable): merge_commit, squash, fast_forward.
        #[arg(long, value_parser = crate::mr_conditions::parse_method)]
        allow: Vec<String>,
        /// The default method (one of the allowed).
        #[arg(long, value_parser = crate::mr_conditions::parse_method)]
        default: Option<String>,
        /// Delete the source branch after a merge unless asked otherwise.
        #[arg(long, value_name = "BOOL")]
        delete_source_branch: Option<bool>,
        /// New commits end the approvals (false: carry them when allowed).
        #[arg(long, value_name = "BOOL")]
        reset_approvals_on_push: Option<bool>,
        /// Committers' approvals do not count.
        #[arg(long, value_name = "BOOL")]
        exclude_committer_approvals: Option<bool>,
    },
}

#[derive(Subcommand, Debug)]
pub enum CodeownersCommand {
    /// Suspend CODEOWNERS enforcement for 1..=1440 minutes (it comes back by
    /// itself); an approved runbook step (runbook codeowners).
    Suspend {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
        /// How long, in minutes (1..=1440).
        #[arg(long = "for", value_parser = clap::value_parser!(u32).range(1..=1440), value_name = "MINUTES")]
        for_minutes: u32,
    },
    /// Enforce CODEOWNERS again now.
    Enforce {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
    },
}

#[derive(Subcommand, Debug)]
pub enum MergeRuleCommand {
    /// Set a protected branch rule's merge conditions (Maintainer). Options
    /// left out keep their current value (read first). Refused
    /// (FG-REVIEW-0019) when they changed after gstn read them: run it
    /// again.
    Set {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
        /// The protection rule's id (from protected-ref list).
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        rule_id: i64,
        /// Minimum counted approvals (0..=20).
        #[arg(long, value_parser = clap::value_parser!(u32).range(0..=20))]
        approvals: Option<u32>,
        /// A required check (repeatable; replaces the list): ci/…, kanflow/…
        /// or forge/….
        #[arg(long, value_parser = parse_text, conflicts_with = "clear_checks")]
        check: Vec<String>,
        /// Require no check.
        #[arg(long)]
        clear_checks: bool,
        /// true / false: every discussion must be resolved.
        #[arg(long, value_name = "BOOL")]
        resolved_discussions: Option<bool>,
        /// true / false: forbid direct pushes (the branch changes through
        /// merges only).
        #[arg(long, value_name = "BOOL")]
        direct_push_forbidden: Option<bool>,
    },
    /// Remove a protected branch rule's merge conditions.
    Remove {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        rule_id: i64,
        /// Confirm the removal (gstn never prompts).
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum ProtectedRefCommand {
    /// List a project's protected ref rules.
    List {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
    },
    /// Protect a ref pattern.
    Add {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
        /// `refs/heads/…` or `refs/tags/…`, at most one `*` in the last part.
        #[arg(long, value_parser = parse_text)]
        pattern: String,
        #[arg(long, value_enum)]
        push_min_role: PushRole,
    },
    /// Change a rule's minimum push role (the pattern is fixed).
    Edit {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        rule_id: i64,
        #[arg(long, value_enum)]
        push_min_role: PushRole,
    },
    /// Remove a rule.
    Remove {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        rule_id: i64,
        /// Confirm the removal (gstn never prompts).
        #[arg(long)]
        yes: bool,
    },
}

/// The states `mr list --state` filters on.
#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum MrState {
    Open,
    Closed,
    Merged,
    All,
}

impl MrState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Closed => "closed",
            Self::Merged => "merged",
            Self::All => "all",
        }
    }
}

#[derive(Subcommand, Debug)]
pub enum MrCommand {
    /// Open a merge request from one existing branch into another.
    Create {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
        /// The source branch (`feature/x` or `refs/heads/feature/x`).
        #[arg(long, value_parser = parse_text, value_name = "BRANCH")]
        source: String,
        /// The target branch (another branch than the source).
        #[arg(long, value_parser = parse_text, value_name = "BRANCH")]
        target: String,
        #[arg(long, value_parser = parse_text)]
        title: String,
        #[arg(long, value_parser = parse_description)]
        description: Option<String>,
        /// Open it as a draft.
        #[arg(long)]
        draft: bool,
    },
    /// List a project's merge requests, newest first (open by default).
    List {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
        #[arg(long, value_enum)]
        state: Option<MrState>,
        /// Rows per page, 1..=100 (server default 20; 100 with --all).
        #[arg(long, value_parser = parse_limit, value_name = "N")]
        limit: Option<u32>,
        /// Continue below this `next_before` iid (from a previous page).
        #[arg(long, value_parser = parse_id, value_name = "IID", conflicts_with = "all")]
        before: Option<i64>,
        /// Follow next_before and merge every page (bounded).
        #[arg(long)]
        all: bool,
    },
    /// Show one merge request.
    View {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
        /// The merge request's number in the project.
        #[arg(long, value_parser = parse_id, value_name = "IID")]
        iid: i64,
    },
    /// Change a merge request's title, description, target or draft flag.
    Edit {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
        #[arg(long, value_parser = parse_id, value_name = "IID")]
        iid: i64,
        #[arg(long, value_parser = parse_text)]
        title: Option<String>,
        #[arg(long, value_parser = parse_description)]
        description: Option<String>,
        /// A new target branch.
        #[arg(long, value_parser = parse_text, value_name = "BRANCH")]
        target: Option<String>,
        /// Mark it a draft.
        #[arg(long, conflicts_with = "ready")]
        draft: bool,
        /// Mark it ready (not a draft).
        #[arg(long)]
        ready: bool,
    },
    /// Close a merge request.
    Close {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
        #[arg(long, value_parser = parse_id, value_name = "IID")]
        iid: i64,
    },
    /// Reopen a closed merge request (its branches must still exist).
    Reopen {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
        #[arg(long, value_parser = parse_id, value_name = "IID")]
        iid: i64,
    },
    /// List a merge request's versions (one per pushed head), oldest first.
    Versions {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
        #[arg(long, value_parser = parse_id, value_name = "IID")]
        iid: i64,
    },
    /// One version's diff (merge base to head), or two versions compared
    /// (head to head) with --from / --to.
    Diff {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
        #[arg(long, value_parser = parse_id, value_name = "IID")]
        iid: i64,
        /// The version whose diff to show.
        #[arg(long, value_parser = parse_id, value_name = "N", conflicts_with_all = ["from", "to"])]
        version: Option<i64>,
        /// The earlier version of a comparison.
        #[arg(long, value_parser = parse_id, value_name = "N", requires = "to")]
        from: Option<i64>,
        /// The later version of a comparison.
        #[arg(long, value_parser = parse_id, value_name = "N", requires = "from")]
        to: Option<i64>,
        /// Print the patches too.
        #[arg(long)]
        patch: bool,
    },
    /// Whether a merge request can be merged, as last checked (never
    /// starts a check).
    Mergeability {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
        #[arg(long, value_parser = parse_id, value_name = "IID")]
        iid: i64,
    },
    /// List a merge request's discussions with their notes (and whether a
    /// later version changed an inline thread's line).
    Discussions {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
        #[arg(long, value_parser = parse_id, value_name = "IID")]
        iid: i64,
    },
    /// Start a discussion (general, or on a line of a version's diff with
    /// --version/--path/--line), or reply to one with --discussion.
    Comment {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
        #[arg(long, value_parser = parse_id, value_name = "IID")]
        iid: i64,
        /// The note: plain text / Markdown (stored as written).
        #[arg(long, value_parser = parse_description)]
        body: String,
        /// Reply to this discussion instead of starting one.
        #[arg(long, value_parser = parse_id, value_name = "DISCUSSION", conflicts_with_all = ["version", "path", "line", "side"])]
        discussion: Option<i64>,
        /// The version whose diff the line is in (an inline discussion).
        #[arg(long, value_parser = parse_id, value_name = "N", requires_all = ["path", "line"])]
        version: Option<i64>,
        /// The file's path on that side of the diff.
        #[arg(long, value_parser = parse_text, value_name = "PATH", requires = "version")]
        path: Option<String>,
        /// The line (inside a hunk of that file on that side).
        #[arg(long, value_parser = parse_id, value_name = "LINE", requires = "version")]
        line: Option<i64>,
        /// The side of the diff (default new: the version's head).
        #[arg(long, value_enum, requires = "version")]
        side: Option<DiffSide>,
    },
    /// Merge a merge request (M3 R4): queue the merge of the head you
    /// reviewed (--sha); --wait waits until it is applied or refused.
    Merge {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
        #[arg(long, value_parser = parse_id, value_name = "IID")]
        iid: i64,
        /// The head commit you reviewed (`mr view` shows it); another head
        /// is refused (FG-REVIEW-0008).
        #[arg(long, value_parser = parse_text, value_name = "SHA")]
        sha: String,
        /// The method (default: the project's, merge_commit).
        #[arg(long, value_enum)]
        method: Option<MergeMethodArg>,
        /// Delete the source branch after the merge (a protected branch is kept).
        #[arg(long)]
        delete_source_branch: bool,
        /// The commit message (default: from the title and description).
        #[arg(long, value_parser = parse_description)]
        message: Option<String>,
        /// Wait until the merge is applied, superseded or failed.
        #[arg(long)]
        wait: bool,
        /// How long --wait waits, in seconds (default 300, at most 3600).
        #[arg(long, value_parser = parse_wait_secs, value_name = "SECS", requires = "wait")]
        wait_timeout: Option<u64>,
    },
    /// Approve the head you reviewed (--sha; M3 R5). The answer says
    /// whether the approval counts now.
    Approve {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
        #[arg(long, value_parser = parse_id, value_name = "IID")]
        iid: i64,
        /// The head commit you reviewed (`mr view` shows it); another head
        /// is refused (FG-REVIEW-0008).
        #[arg(long, value_parser = parse_oid, value_name = "SHA")]
        sha: String,
    },
    /// Withdraw your approval.
    Unapprove {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
        #[arg(long, value_parser = parse_id, value_name = "IID")]
        iid: i64,
    },
    /// How each approval counts now, the rules, CODEOWNERS and what blocks
    /// the merge on approvals.
    Approvals {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
        #[arg(long, value_parser = parse_id, value_name = "IID")]
        iid: i64,
    },
    /// The work item links of a merge request, open and ended (M3 I1b).
    #[command(name = "work-items")]
    WorkItems {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
        #[arg(long, value_parser = parse_id, value_name = "IID")]
        iid: i64,
    },
    /// Link a work item (PBI-<n> of the project's kanflow binding) by hand.
    #[command(name = "link-work-item")]
    LinkWorkItem {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
        #[arg(long, value_parser = parse_id, value_name = "IID")]
        iid: i64,
        /// The work item, `PBI-<n>`.
        #[arg(long = "ref", value_parser = crate::mr_work_items::parse_work_item_ref, value_name = "REF")]
        reference: String,
    },
    /// Unlink a work item; the link stays in the history and detection
    /// does not link it again.
    #[command(name = "unlink-work-item")]
    UnlinkWorkItem {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
        #[arg(long, value_parser = parse_id, value_name = "IID")]
        iid: i64,
        /// The work item, `PBI-<n>` or its work item id.
        #[arg(long = "ref", value_parser = crate::mr_work_items::parse_unlink_target, value_name = "REF")]
        reference: String,
    },
    /// Whether a merge request may merge now, and every reason it may not
    /// (a stable JSON contract with --json).
    Status {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
        #[arg(long, value_parser = parse_id, value_name = "IID")]
        iid: i64,
    },
    /// The latest merge attempt of a merge request (--wait: until it is final).
    #[command(name = "merge-status")]
    MergeStatus {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
        #[arg(long, value_parser = parse_id, value_name = "IID")]
        iid: i64,
        /// Wait until the latest attempt is applied, superseded or failed.
        #[arg(long)]
        wait: bool,
        /// How long --wait waits, in seconds (default 300, at most 3600).
        #[arg(long, value_parser = parse_wait_secs, value_name = "SECS", requires = "wait")]
        wait_timeout: Option<u64>,
    },
    /// Resolve a discussion (or open it again with --unresolve).
    Resolve {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
        #[arg(long, value_parser = parse_id, value_name = "IID")]
        iid: i64,
        #[arg(long, value_parser = parse_id, value_name = "DISCUSSION")]
        discussion: i64,
        /// Open a resolved discussion again.
        #[arg(long)]
        unresolve: bool,
    },
}

/// How a merge request is merged (ADR-0024 §4.1).
#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
#[value(rename_all = "snake_case")]
pub enum MergeMethodArg {
    MergeCommit,
    Squash,
    FastForward,
}

impl MergeMethodArg {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MergeCommit => "merge_commit",
            Self::Squash => "squash",
            Self::FastForward => "fast_forward",
        }
    }
}

/// `--wait-timeout`: 0..=3600 seconds.
pub fn parse_wait_secs(raw: &str) -> Result<u64, String> {
    match raw.parse::<u64>() {
        Ok(n) if n <= 3600 => Ok(n),
        _ => Err("seconds, 0 to 3600".to_string()),
    }
}

/// The side of a diff an inline discussion sits on.
#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffSide {
    /// The version's merge base.
    Old,
    /// The version's head.
    New,
}

impl DiffSide {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Old => "old",
            Self::New => "new",
        }
    }
}

#[derive(Subcommand, Debug)]
pub enum NamespaceCommand {
    /// Which project, group or user a path names.
    Resolve {
        /// The full path, e.g. `team/app`.
        #[arg(value_parser = parse_text)]
        path: String,
    },
    /// Release a moved/deleted path's tombstone so it can be used again. Needs --yes.
    #[command(name = "release-path")]
    ReleasePath {
        /// The tombstoned full path, exactly (`group/sub/project`).
        #[arg(value_parser = parse_text)]
        path: String,
        /// Confirm the release (gstn never prompts).
        #[arg(long)]
        yes: bool,
    },
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefType {
    Branch,
    Tag,
}

impl RefType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Branch => "branch",
            Self::Tag => "tag",
        }
    }
}

#[derive(Subcommand, Debug)]
pub enum BrowseCommand {
    /// List the branches and tags.
    Refs {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
        /// Only branches or only tags.
        #[arg(long = "type", value_enum)]
        kind: Option<RefType>,
    },
    /// Split `<ref>/<path>` into a revision and a path.
    Resolve {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
        /// e.g. `feature/x/src/lib.rs`.
        #[arg(value_parser = parse_text)]
        spec: String,
    },
    /// One tree level.
    Tree {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
        /// A commit id, a branch or a tag.
        #[arg(long = "ref", value_parser = parse_text)]
        rev: String,
        /// The directory (default: the root).
        #[arg(long, value_parser = parse_text)]
        path: Option<String>,
    },
    /// One file's head (text; binary files say so).
    File {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
        #[arg(long = "ref", value_parser = parse_text)]
        rev: String,
        #[arg(long, value_parser = parse_text)]
        path: String,
    },
    /// A page of a revision's history, newest first.
    Commits {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
        /// A commit id, a branch or a tag.
        #[arg(long = "ref", value_parser = parse_text)]
        rev: String,
        /// Only commits that changed this path.
        #[arg(long, value_parser = parse_text)]
        path: Option<String>,
        /// Commits per page, 1..=100 (server default 30).
        #[arg(long, value_parser = parse_limit)]
        limit: Option<u32>,
        /// Continue after this next_cursor (same --path).
        #[arg(long, value_parser = parse_text)]
        cursor: Option<String>,
    },
    /// One commit: message and changed files (--patch: and the patches).
    Commit {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
        /// The full commit id (40 or 64 hexadecimal digits).
        #[arg(value_parser = parse_oid)]
        oid: String,
        /// Print the patches too.
        #[arg(long)]
        patch: bool,
    },
    /// Compare two revisions: the commits `to` has and `from` does not, and the changed files.
    Compare {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
        /// The base: a commit id, a branch or a tag.
        #[arg(long, value_parser = parse_text)]
        from: String,
        /// The head: a commit id, a branch or a tag.
        #[arg(long, value_parser = parse_text)]
        to: String,
        /// Print the patches too.
        #[arg(long)]
        patch: bool,
    },
    /// Who last changed each line of a file.
    Blame {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
        #[arg(long = "ref", value_parser = parse_text)]
        rev: String,
        #[arg(long, value_parser = parse_text)]
        path: String,
    },
}

#[derive(Subcommand, Debug)]
pub enum TokenCommand {
    /// Your tokens (never a secret).
    List,
    /// Revoke one of your tokens. Needs --yes.
    Revoke {
        #[arg(value_parser = parse_id)]
        id: i64,
        /// Confirm (gstn never prompts).
        #[arg(long)]
        yes: bool,
    },
}

/// The state filter of `user list`.
#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum UserState {
    Active,
    Disabled,
}

impl UserState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Disabled => "disabled",
        }
    }
}

#[derive(Subcommand, Debug)]
pub enum UserCommand {
    /// Find active users by username prefix or display name (names only).
    Search {
        #[arg(value_parser = parse_text)]
        term: String,
    },
    /// Every user with its state (administrators).
    List {
        #[command(flatten)]
        list: ListArgs,
        /// Only active or only disabled users.
        #[arg(long, value_enum)]
        state: Option<UserState>,
    },
    /// Disable a user (administrators): its tokens and sessions end. Needs --yes.
    Disable {
        #[arg(value_parser = parse_id)]
        id: i64,
        /// Confirm (gstn never prompts).
        #[arg(long)]
        yes: bool,
    },
    /// A user's active web sessions (administrators).
    Sessions {
        #[arg(value_parser = parse_id)]
        id: i64,
    },
    /// End every web session of a user (administrators). Needs --yes.
    #[command(name = "end-sessions")]
    EndSessions {
        #[arg(value_parser = parse_id)]
        id: i64,
        /// Confirm (gstn never prompts).
        #[arg(long)]
        yes: bool,
    },
    /// Rename a user and their personal namespace (administrators). Needs --yes.
    Rename {
        #[arg(value_parser = parse_id)]
        id: i64,
        #[arg(long, value_parser = parse_text)]
        username: String,
        /// Take back an old username of this very user.
        #[arg(long)]
        reclaim: bool,
        /// Confirm the rename (gstn never prompts).
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum ChecksCommand {
    /// Report a check run on a commit (a service account holding the
    /// namespace; the same name again updates it).
    Report {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
        /// The full commit id (40 or 64 hex digits).
        #[arg(long, value_parser = parse_oid)]
        sha: String,
        /// `<namespace>/<name>`, e.g. ci/gates.
        #[arg(long, value_parser = parse_text)]
        name: String,
        /// queued, in_progress or completed.
        #[arg(long, value_parser = crate::checks::parse_status)]
        status: String,
        /// With completed: success, neutral, failure, error, action_required.
        #[arg(long, value_parser = crate::checks::parse_conclusion)]
        conclusion: Option<String>,
        /// The summary (plain text / Markdown).
        #[arg(long, value_parser = parse_description, conflicts_with = "summary_file")]
        summary: Option<String>,
        /// The summary from a file (UTF-8, at most 64 KiB).
        #[arg(long = "summary-file", value_name = "FILE")]
        summary_file: Option<PathBuf>,
        /// An http(s) link for people (never fetched by the server).
        #[arg(long = "details-url", value_parser = parse_text)]
        details_url: Option<String>,
        /// The annotations: a JSON array of {path, start_line, end_line?,
        /// level, message, title?} (at most 50).
        #[arg(long = "annotations-file", value_name = "FILE")]
        annotations_file: Option<PathBuf>,
    },
    /// Every check run of a commit, by name.
    List {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
        /// The full commit id (40 or 64 hex digits).
        #[arg(long, value_parser = parse_oid)]
        sha: String,
    },
}

/// A check namespace a service account may be granted.
pub fn parse_grant_namespace(raw: &str) -> Result<String, String> {
    match raw {
        "ci" | "kanflow" => Ok(raw.to_owned()),
        _ => Err("a check namespace to grant is ci or kanflow".to_owned()),
    }
}

#[derive(Subcommand, Debug)]
pub enum AdminCommand {
    /// Service accounts: list, view, update, tokens, revoke-token, the check
    /// namespaces they report (create and issue in the browser).
    #[command(name = "service-account", disable_help_subcommand = true)]
    ServiceAccount {
        #[command(subcommand)]
        command: ServiceAccountCommand,
    },
    /// Integrations (M3 I1a): list, view, bindings (create and change in
    /// the browser).
    #[command(name = "integration", disable_help_subcommand = true)]
    Integration {
        #[command(subcommand)]
        command: IntegrationCommand,
    },
}

#[derive(Subcommand, Debug)]
pub enum IntegrationCommand {
    /// Every integration with its service account and binding count.
    List,
    /// One integration.
    View {
        #[arg(value_parser = parse_id)]
        id: i64,
    },
    /// The projects bound to an integration.
    Bindings {
        #[arg(value_parser = parse_id)]
        id: i64,
    },
}

#[derive(Subcommand, Debug)]
pub enum IntegrationBindingCommand {
    /// Show a project's integration binding (none or one).
    List {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
    },
    /// Bind a project to an external (kanflow) project of an integration
    /// (Maintainer); its service account becomes a Reporter.
    Set {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
        /// The integration id (admin integration list).
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        integration: i64,
        /// The external project's id (A-Z a-z 0-9 . _ : -, at most 100).
        #[arg(long = "external-project", value_parser = parse_text)]
        external_project: String,
    },
    /// Unbind a project; the membership the binding granted is removed.
    Remove {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        integration: i64,
        /// Confirm (gstn never prompts).
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum ServiceAccountCommand {
    /// Every service account with its owner and token summary.
    List,
    /// One service account.
    View {
        #[arg(value_parser = parse_id)]
        id: i64,
    },
    /// Change a service account's display name, purpose or owner.
    Update {
        #[arg(value_parser = parse_id)]
        id: i64,
        #[arg(long = "display-name", value_parser = parse_text)]
        display_name: Option<String>,
        #[arg(long, value_parser = parse_text)]
        purpose: Option<String>,
        /// The new owner's user id (an active person).
        #[arg(long, value_parser = parse_id)]
        owner: Option<i64>,
    },
    /// A service account's tokens with their issuer (never a secret).
    Tokens {
        #[arg(value_parser = parse_id)]
        id: i64,
    },
    /// The check namespace grants of a service account, by project (M3 C1).
    #[command(name = "check-grants")]
    CheckGrants {
        #[arg(value_parser = parse_id)]
        id: i64,
    },
    /// Let a service account report a check namespace (ci, kanflow) on one
    /// project.
    #[command(name = "grant-checks")]
    GrantChecks {
        #[arg(value_parser = parse_id)]
        id: i64,
        /// The project the grant is for.
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
        #[arg(value_parser = parse_grant_namespace)]
        namespace: String,
    },
    /// Stop a service account reporting a check namespace on one project.
    #[command(name = "revoke-checks")]
    RevokeChecks {
        #[arg(value_parser = parse_id)]
        id: i64,
        /// The project the grant is for.
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
        #[arg(value_parser = parse_grant_namespace)]
        namespace: String,
    },
    /// Revoke one of a service account's tokens (a leak). Needs --yes.
    #[command(name = "revoke-token")]
    RevokeToken {
        #[arg(value_parser = parse_id)]
        id: i64,
        #[arg(value_parser = parse_id)]
        token_id: i64,
        /// Confirm (gstn never prompts).
        #[arg(long)]
        yes: bool,
    },
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum GitOperation {
    Get,
    Store,
    Erase,
}

#[derive(Args, Debug)]
pub struct ApiArgs {
    /// GET | POST | PUT | PATCH | DELETE
    pub method: String,
    /// A path on the configured host, e.g. /api/v1/projects
    pub path: String,
    /// key=value (string) or key:=<json>; repeatable.
    #[arg(long = "field", short = 'f', value_name = "K=V")]
    pub field: Vec<String>,
    /// JSON body from a file, or `-` for stdin.
    #[arg(long, value_name = "FILE")]
    pub input: Option<PathBuf>,
    /// Follow next_cursor (GET lists).
    #[arg(long)]
    pub paginate: bool,
    /// Print the request without sending it.
    #[arg(long)]
    pub dry_run: bool,
}

/// Every leaf command path of the tree, e.g. `auth login` (for the registry test).
pub fn command_paths() -> Vec<String> {
    use clap::CommandFactory;
    fn walk(c: &clap::Command, prefix: &str, out: &mut Vec<String>) {
        let subs: Vec<_> = c.get_subcommands().collect();
        if subs.is_empty() {
            out.push(prefix.to_string());
        }
        for s in subs {
            let p = if prefix.is_empty() {
                s.get_name().to_string()
            } else {
                format!("{prefix} {}", s.get_name())
            };
            walk(s, &p, out);
        }
    }
    let mut out = Vec::new();
    walk(&Cli::command(), "", &mut out);
    out.sort();
    out
}
