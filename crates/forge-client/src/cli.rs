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
    /// A project's webhooks (M3 I2): list, view, create, update, delete,
    /// rotate-secret, deliveries, redeliver (Maintainers).
    #[command(disable_help_subcommand = true)]
    Webhook {
        #[command(subcommand)]
        command: WebhookCommand,
    },
    /// A project's push mirror to GitLab (M3 O2): show, set, remove, sync
    /// (administrators; a token needs the admin scope).
    #[command(disable_help_subcommand = true)]
    Mirror {
        #[command(subcommand)]
        command: MirrorCommand,
    },
    /// A project's CI pipelines (M4 C2, C5): list, show, cancel, retry, run, log, diagnosis
    /// (reading: project readers; the rest: Developers).
    #[command(disable_help_subcommand = true)]
    Pipeline {
        #[command(subcommand)]
        command: PipelineCommand,
    },
    /// The merge queue (M4 C8): add a merge request (merge when ready),
    /// remove it, its place, a target's queue, pause and resume.
    #[command(disable_help_subcommand = true)]
    Queue {
        #[command(subcommand)]
        command: QueueCommand,
    },
    /// An integration's event feed (M3 I1c): events, cursor — with the
    /// integration's own service account token.
    #[command(disable_help_subcommand = true)]
    Integration {
        #[command(subcommand)]
        command: IntegrationFeedCommand,
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
    /// A merge request's predicted conflicts with other open merge requests
    /// on its target, drafts included (M3 X6; never starts a scan).
    Conflicts(ConflictsArgs),
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
    /// A project's specification (M3 X4): requirements, versions, the
    /// requirement diff, and the trace to `@req` tests.
    #[command(disable_help_subcommand = true)]
    Spec {
        #[command(subcommand)]
        command: SpecCommand,
    },
    /// Documents in the plan 13 format: fmt and lint on your working tree
    /// (never the server); show, list and migration through the server.
    #[command(disable_help_subcommand = true)]
    Doc {
        #[command(subcommand)]
        command: DocCommand,
    },
    /// CI pipeline definitions (.forge-ci.yml): lint and plan on your
    /// working tree (never the server).
    #[command(disable_help_subcommand = true)]
    Ci {
        #[command(subcommand)]
        command: CiCommand,
    },
    /// The context API (M3 X5): a work item's, a requirement's or a merge
    /// request's context, every item with its source.
    #[command(disable_help_subcommand = true)]
    Context {
        #[command(subcommand)]
        command: ContextCommand,
    },
    /// An MCP server on stdin / stdout (M3 X5) for an AI agent: the tools of
    /// docs/api/agent-tools.toml (read the context, specification,
    /// documents, merge requests, checks and pipelines; open, update,
    /// comment on, close and reopen merge requests; run, cancel and retry
    /// pipelines) — with your token, and never what is marked confidential
    /// (plan 06 D-5). Approving, merging, resolving and deploying are kept
    /// for people.
    ///
    /// Log in with a token issued with `is_agent` for the agent: a person's
    /// ordinary token is a person's on every other route.
    Mcp,
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
    /// Your SSH keys for git over SSH: list, delete (add one in the browser).
    #[command(name = "ssh-key", disable_help_subcommand = true)]
    SshKey {
        #[command(subcommand)]
        command: SshKeyCommand,
    },
    /// Your notifications: the in-app list (M3 U4) and your settings (M3 N1).
    #[command(disable_help_subcommand = true)]
    Notifications {
        #[command(subcommand)]
        command: NotificationsCommand,
    },
    /// The recent events of the projects you are involved in, newest
    /// first (plan 15 MR 8).
    Activity {
        /// Continue below this `next_before` seq (from a previous page).
        #[arg(long, value_parser = parse_id, value_name = "SEQ")]
        before: Option<i64>,
        /// Events read per page, 1..=100 (server default 30).
        #[arg(long, value_parser = parse_limit, value_name = "N")]
        limit: Option<u32>,
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

fn parse_spec_limit(raw: &str) -> Result<u32, String> {
    match raw.parse::<u32>() {
        Ok(n) if (1..=500).contains(&n) => Ok(n),
        _ => Err("--limit is a whole number from 1 to 500".to_string()),
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
        /// true / false: require an open link to a work item (PBI-<n>) of
        /// the project's integration binding (M3 I1d).
        #[arg(long, value_name = "BOOL")]
        work_item_link: Option<bool>,
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

/// The scopes of `mr mine --scope` (the server's `user_lists`).
#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum MineScope {
    /// Open, your own.
    Authored,
    /// Open, not draft, you may approve its current head and have not yet.
    Approvable,
    /// Your open ones with a conflict, a predicted conflict or a failed
    /// required check.
    Attention,
}

impl MineScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Authored => "authored",
            Self::Approvable => "approvable",
            Self::Attention => "attention",
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
    /// Your own merge requests across projects, newest first (plan 15
    /// MR 6): one page of a scope, or with --counts how many each holds.
    Mine {
        #[arg(long, value_enum, required_unless_present = "counts")]
        scope: Option<MineScope>,
        /// Rows per page, 1..=100 (server default 20).
        #[arg(long, value_parser = parse_limit, value_name = "N", conflicts_with = "counts")]
        limit: Option<u32>,
        /// Continue below this `next_before` id (from a previous page).
        #[arg(long, value_parser = parse_id, value_name = "ID", conflicts_with = "counts")]
        before: Option<i64>,
        /// How many open merge requests each scope holds (no page).
        #[arg(long, conflicts_with = "scope")]
        counts: bool,
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
    /// A revision's `.forge/repo.yml` and its spec references, each
    /// resolved for you (M3 X1).
    SpecRefs {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
        /// A commit id, a branch or a tag (default: the default branch).
        #[arg(long = "ref", value_parser = parse_text)]
        rev: Option<String>,
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
    /// Commit one text file read from standard input (the simple editor's
    /// API, E1): on --create-branch (with a merge request unless
    /// --no-merge-request) or directly on --branch.
    CommitFile {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
        /// The branch the file was read from.
        #[arg(long, value_parser = parse_text)]
        branch: String,
        /// The commit the file was read at (refused when the file changed since).
        #[arg(long = "start-sha", value_parser = parse_oid)]
        start_sha: String,
        #[arg(long, value_parser = parse_text)]
        path: String,
        #[arg(long, value_parser = parse_description)]
        message: String,
        /// Commit on this new branch instead of --branch.
        #[arg(long = "create-branch", value_parser = parse_text)]
        create_branch: Option<String>,
        /// With --create-branch: do not open a merge request.
        #[arg(long = "no-merge-request")]
        no_merge_request: bool,
        /// The merge request's title (default: the message's first line).
        #[arg(long, value_parser = parse_text)]
        title: Option<String>,
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

/// `gstn ssh-key` (M4 C13).
#[derive(Subcommand, Debug)]
pub enum SshKeyCommand {
    /// Your SSH keys (fingerprints) and the server's host key fingerprints.
    List,
    /// Delete one of your SSH keys. Needs --yes.
    Delete {
        #[arg(value_parser = parse_id)]
        id: i64,
        /// Confirm (gstn never prompts).
        #[arg(long)]
        yes: bool,
    },
}

/// `notifications set --email`.
#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum MailSwitch {
    On,
    Off,
}

#[derive(Subcommand, Debug)]
pub enum NotificationsCommand {
    /// Your settings: mail on or off, the language of the mail, the kinds muted.
    Get,
    /// Change your settings; a flag changes only what it names.
    Set {
        /// Turn all mail on or off.
        #[arg(long, value_enum)]
        email: Option<MailSwitch>,
        /// The language of the mail: ja, en, id, or default (the instance's).
        #[arg(long, value_parser = ["ja", "en", "id", "default"])]
        language: Option<String>,
        /// Stop a kind of notification (repeatable; `notifications get` lists them).
        #[arg(long, value_parser = parse_text, value_name = "KIND")]
        mute: Vec<String>,
        /// Receive a kind again (repeatable).
        #[arg(long, value_parser = parse_text, value_name = "KIND")]
        unmute: Vec<String>,
        /// Turn all in-app notifications on or off (M3 U4).
        #[arg(long, value_enum)]
        in_app: Option<MailSwitch>,
    },
    /// Your in-app notifications, newest first (M3 U4).
    List {
        /// Only unread ones.
        #[arg(long)]
        unread: bool,
        /// Only items older than this id (`next_before` of the previous page).
        #[arg(long, value_parser = parse_id)]
        before: Option<i64>,
        #[arg(long, value_parser = parse_limit)]
        limit: Option<u32>,
    },
    /// How many in-app notifications are unread (M3 U4).
    Count,
    /// Mark one in-app notification read, or every unread one with --all.
    Read {
        /// The notification id (`notifications list`).
        #[arg(value_parser = parse_id, required_unless_present = "all", conflicts_with = "all")]
        id: Option<i64>,
        /// Every unread one.
        #[arg(long)]
        all: bool,
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

#[derive(Subcommand, Debug)]
pub enum WebhookCommand {
    /// A project's webhooks (never a secret).
    List {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
    },
    /// The event types a webhook may name, by class, and the classes you may subscribe to.
    EventTypes {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
    },
    /// One webhook: its URL, event types, state and failure count.
    View {
        #[arg(value_parser = parse_id)]
        id: i64,
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
    },
    /// Create a webhook owned by you; its secret is shown once.
    Create {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
        /// An https URL whose host and port the operator's allowlist names.
        #[arg(long, value_parser = parse_text)]
        url: String,
        /// An event type, a family (`merge_request.*`) or `*` (repeatable;
        /// default `*`).
        #[arg(long = "event", value_parser = parse_text, value_name = "TYPE")]
        events: Vec<String>,
    },
    /// Change a webhook's URL, event types (replaced), or enable / disable it.
    Update {
        #[arg(value_parser = parse_id)]
        id: i64,
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
        #[arg(long, value_parser = parse_text)]
        url: Option<String>,
        /// The new event types (repeatable; replaces them all).
        #[arg(long = "event", value_parser = parse_text, value_name = "TYPE")]
        events: Vec<String>,
        /// Enable it (clears the failure count).
        #[arg(long, conflicts_with = "disable")]
        enable: bool,
        #[arg(long)]
        disable: bool,
    },
    /// Delete a webhook and its delivery history.
    Delete {
        #[arg(value_parser = parse_id)]
        id: i64,
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
        /// Confirm (gstn never prompts).
        #[arg(long)]
        yes: bool,
    },
    /// Replace the secret (shown once); the previous one signs for 24 hours.
    #[command(name = "rotate-secret")]
    RotateSecret {
        #[arg(value_parser = parse_id)]
        id: i64,
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
    },
    /// The delivery history, newest first.
    Deliveries {
        #[arg(value_parser = parse_id)]
        id: i64,
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
        /// Only deliveries older than this delivery id.
        #[arg(long, value_parser = parse_id)]
        before: Option<i64>,
        #[arg(long, value_parser = parse_limit)]
        limit: Option<u32>,
    },
    /// Deliver one delivery again (the same delivery id).
    Redeliver {
        #[arg(value_parser = parse_id)]
        id: i64,
        /// The delivery's id (from `webhook deliveries`).
        #[arg(value_parser = parse_id)]
        delivery: i64,
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
    },
}

#[derive(Subcommand, Debug)]
pub enum MirrorCommand {
    /// The project's mirror: destination, credential name, progress.
    Show {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
    },
    /// Set (create or change) the project's mirror.
    Set {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
        /// An https URL whose host and port mirror.allowed_hosts names,
        /// without user information (the token is in the credential file).
        #[arg(long, value_parser = parse_text)]
        url: String,
        /// The name of a credential file in mirror.credentials_dir.
        #[arg(long, value_parser = parse_text, value_name = "NAME")]
        credential: String,
        /// Keep the setting without pushing.
        #[arg(long)]
        disabled: bool,
    },
    /// Remove the project's mirror (GitLab keeps what it has).
    Remove {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
        /// Confirm (gstn never prompts).
        #[arg(long)]
        yes: bool,
    },
    /// Queue one push of every branch and tag now.
    Sync {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
    },
}

#[derive(Subcommand, Debug)]
pub enum QueueCommand {
    /// Add a merge request to its target's merge queue: it is merged once
    /// a pipeline on it, together with what is ahead of it, passes (as a
    /// merge: the target's push role; a token needs merge_execute).
    Add {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
        #[arg(long, value_parser = parse_id, value_name = "IID")]
        iid: i64,
        /// The head commit you reviewed (`mr view` shows it); another head
        /// is refused (FG-REVIEW-0008).
        #[arg(long, value_parser = parse_oid, value_name = "SHA")]
        sha: String,
        /// The method (default: the project's).
        #[arg(long, value_enum)]
        method: Option<MergeMethodArg>,
    },
    /// Take a merge request out of the queue.
    Remove {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
        #[arg(long, value_parser = parse_id, value_name = "IID")]
        iid: i64,
    },
    /// A merge request's latest entry and its place in the queue.
    Status {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
        #[arg(long, value_parser = parse_id, value_name = "IID")]
        iid: i64,
    },
    /// A target branch's queue: the batch being tested and the entries.
    List {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
        /// The target branch (`main` or `refs/heads/main`).
        #[arg(long, value_parser = parse_text, value_name = "BRANCH")]
        target: String,
    },
    /// Stop a target's queue (no new batch, no landing) — Developer.
    Pause {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
        #[arg(long, value_parser = parse_text, value_name = "BRANCH")]
        target: String,
    },
    /// Start a paused queue again — Maintainer.
    Resume {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
        #[arg(long, value_parser = parse_text, value_name = "BRANCH")]
        target: String,
    },
}

#[derive(Subcommand, Debug)]
pub enum PipelineCommand {
    /// The project's pipelines, newest first.
    List {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
        /// Only this branch (`main` or `refs/heads/main`).
        #[arg(long = "ref", value_parser = parse_text, value_name = "BRANCH")]
        git_ref: Option<String>,
        /// Only pipelines below this id (the previous page's `more`).
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        before: Option<i64>,
        /// At most this many (1..=100, default 20).
        #[arg(long, value_parser = parse_limit)]
        limit: Option<u32>,
    },
    /// One pipeline with its jobs (an invalid one with its problems).
    Show {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        pipeline: i64,
    },
    /// Cancel a pending or running pipeline.
    Cancel {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        pipeline: i64,
    },
    /// Retry a failed or cancelled pipeline (its failed and cancelled jobs).
    Retry {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        pipeline: i64,
    },
    /// Run a pipeline by hand on a branch's tip (never protected: a job
    /// with secrets makes it invalid).
    Run {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
        /// The branch (`main` or `refs/heads/main`).
        #[arg(long = "ref", value_parser = parse_text, value_name = "BRANCH")]
        git_ref: String,
    },
    /// One page of a job's log, secrets masked (M4 C5). An AI agent's
    /// token is refused (FG-AUTH-0022) when the pipeline's commit or the
    /// default branch holds a confidential document.
    Log {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        pipeline: i64,
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        job: i64,
        /// The job's attempt (default its current one).
        #[arg(long, value_parser = parse_attempt, value_name = "N")]
        attempt: Option<u32>,
        /// The first chunk (the previous page's `more: --from`; default 0).
        #[arg(long, value_parser = parse_log_from, value_name = "CHUNK")]
        from: Option<u32>,
    },
    /// A pipeline's deterministic diagnosis (M4 C5): each failed job's
    /// fingerprint, error codes, excerpt and known fixes. Refused to an AI
    /// agent's token as `log` is.
    Diagnosis {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        pipeline: i64,
    },
    /// A job's artifacts (M4 C6): path, size, sha256, expiry. Refused to an
    /// AI agent's token as `log` is.
    Artifacts {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        pipeline: i64,
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        job: i64,
    },
    /// Download one artifact to a new file and check its sha256.
    ArtifactDownload {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        pipeline: i64,
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        job: i64,
        /// The artifact id (from `pipeline artifacts`).
        #[arg(value_parser = parse_id)]
        id: i64,
        /// The file to write (created new, never over an existing one).
        #[arg(long, value_name = "FILE")]
        output: std::path::PathBuf,
    },
}

/// A job's attempt: 1..=100.
fn parse_attempt(raw: &str) -> Result<u32, String> {
    match raw.parse::<u32>() {
        Ok(n) if (1..=100).contains(&n) => Ok(n),
        _ => Err("--attempt is a whole number from 1 to 100".to_string()),
    }
}

/// A log chunk: 0..=4095.
fn parse_log_from(raw: &str) -> Result<u32, String> {
    match raw.parse::<u32>() {
        Ok(n) if n <= 4095 => Ok(n),
        _ => Err("--from is a whole number from 0 to 4095".to_string()),
    }
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
pub enum IntegrationFeedCommand {
    /// One page of the integration's event feed after a cursor.
    Events {
        #[arg(value_parser = parse_id)]
        id: i64,
        /// A cursor the feed gave (next_cursor, an event's cursor, or
        /// `integration cursor`); without it the oldest retained event.
        #[arg(long, value_parser = parse_feed_cursor)]
        cursor: Option<String>,
        /// Events per page, 1..=500 (default 100).
        #[arg(long, value_parser = parse_feed_limit, value_name = "N")]
        limit: Option<u32>,
    },
    /// The feed's head as a cursor (a resync takes it first).
    Cursor {
        #[arg(value_parser = parse_id)]
        id: i64,
    },
    /// One work item's development status (M3 I1d): its linked merge
    /// requests across the integration's bound projects.
    #[command(name = "dev-status")]
    DevStatus {
        #[arg(value_parser = parse_id)]
        id: i64,
        /// The work item, `PBI-<n>`.
        #[arg(long = "ref", value_parser = parse_work_item_ref, value_name = "PBI-N")]
        reference: String,
        /// The kanflow project id of the binding (default: every bound one).
        #[arg(long, value_parser = parse_external_project, value_name = "ID")]
        project_id: Option<String>,
        /// all (default), open, merged or closed.
        #[arg(long, value_parser = ["all", "open", "merged", "closed"])]
        state: Option<String>,
    },
    /// Every work item's development status, one page (the resync after
    /// a 410).
    #[command(name = "dev-status-list")]
    DevStatusList {
        #[arg(value_parser = parse_id)]
        id: i64,
        /// The previous page's next_cursor.
        #[arg(long, value_parser = parse_feed_cursor)]
        cursor: Option<String>,
        /// Work items per page, 1..=100 (default 50).
        #[arg(long, value_parser = clap::value_parser!(u32).range(1..=100), value_name = "N")]
        limit: Option<u32>,
        /// all (default), open, merged or closed.
        #[arg(long, value_parser = ["all", "open", "merged", "closed"])]
        state: Option<String>,
    },
}

/// A work item reference in its canonical form `PBI-<n>` (no leading 0).
fn parse_work_item_ref(raw: &str) -> Result<String, String> {
    let ok = raw.strip_prefix("PBI-").is_some_and(|d| {
        !d.is_empty()
            && d.len() <= 7
            && d.bytes().all(|b| b.is_ascii_digit())
            && (d == "0" || !d.starts_with('0'))
    });
    if ok {
        Ok(raw.to_string())
    } else {
        Err("--ref is PBI-<n> (digits, no leading zero)".to_string())
    }
}

/// A kanflow project id (`A-Z a-z 0-9 . _ : -`, 1..=100).
fn parse_external_project(raw: &str) -> Result<String, String> {
    let ok = (1..=100).contains(&raw.len())
        && raw
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-'));
    if ok {
        Ok(raw.to_string())
    } else {
        Err("--project-id is 1-100 of A-Z a-z 0-9 . _ : -".to_string())
    }
}

/// A feed cursor: base64url text, at most 1024 characters (opaque to gstn).
fn parse_feed_cursor(raw: &str) -> Result<String, String> {
    let ok = !raw.is_empty()
        && raw.len() <= 1024
        && raw
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if ok {
        Ok(raw.to_string())
    } else {
        Err("--cursor is the opaque text the feed gave (A-Z a-z 0-9 - _)".to_string())
    }
}

/// The feed's page size, 1..=500.
fn parse_feed_limit(raw: &str) -> Result<u32, String> {
    match raw.parse::<u32>() {
        Ok(n) if (1..=500).contains(&n) && !raw.starts_with('0') => Ok(n),
        _ => Err("--limit is a whole number from 1 to 500".to_string()),
    }
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

/// `gstn spec` (M3 X4, plan 03 §7.2): the spec API, read with your
/// permissions.
#[derive(Subcommand, Debug)]
pub enum SpecCommand {
    /// The own specification's requirements at a revision, and the
    /// documents' problems.
    Requirements {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
        /// A commit id, a branch or a tag (default: the default branch).
        #[arg(long = "ref", value_parser = parse_text)]
        rev: Option<String>,
        /// Only requirements whose id or text contains this (case-insensitive).
        #[arg(long, value_parser = parse_text)]
        search: Option<String>,
        /// At most this many requirements, 1..=500 (server default 100).
        #[arg(long, value_parser = parse_spec_limit)]
        limit: Option<u32>,
    },
    /// The specification's versions (version tags), newest first.
    Versions {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
    },
    /// The requirements added, changed, removed or deprecated between two revisions.
    Diff {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
        /// The earlier revision: a commit id, a branch or a tag.
        #[arg(long, value_parser = parse_text)]
        from: String,
        /// The later revision: a commit id, a branch or a tag.
        #[arg(long, value_parser = parse_text)]
        to: String,
    },
    /// Each requirement (own, and every pinned reference you may read)
    /// with the `@req` tests naming it.
    Trace {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
        /// A commit id, a branch or a tag (default: the default branch).
        #[arg(long = "ref", value_parser = parse_text)]
        rev: Option<String>,
    },
}

/// `gstn ci` (plan 14 C1, plan 04 P-3): the `.forge-ci.yml` lint and plan
/// on the working tree, with forge-ci (the forge/ci-lint check's code).
/// Local only: nothing is sent.
#[derive(Subcommand, Debug)]
pub enum CiCommand {
    /// Check .forge-ci.yml: schema, needs, timeouts, pinned images,
    /// secrets on protected branches only, bounds (the same lint as the
    /// forge/ci-lint check).
    Lint {
        /// The file (default: .forge-ci.yml at the repository root).
        path: Option<PathBuf>,
        /// The repository's protected branch patterns (e.g.
        /// main,release/*). Without it no branch counts as protected.
        #[arg(long, value_delimiter = ',', value_parser = parse_text, value_name = "A,B")]
        protected: Option<Vec<String>>,
    },
    /// Show which jobs a push or a merge request would run, and in what
    /// order (waves), without running anything.
    Plan {
        /// The file (default: .forge-ci.yml at the repository root).
        path: Option<PathBuf>,
        /// The branch pushed (for --event mr: the source branch).
        #[arg(long, alias = "ref", value_parser = parse_text)]
        branch: String,
        /// push (default) or mr.
        #[arg(long, value_enum, default_value_t = CiEvent::Push)]
        event: CiEvent,
        /// The repository's protected branch patterns (e.g.
        /// main,release/*). Without it no branch counts as protected.
        #[arg(long, value_delimiter = ',', value_parser = parse_text, value_name = "A,B")]
        protected: Option<Vec<String>>,
    },
    /// A project's CI variables — the secrets jobs name in `secrets:`
    /// (Maintainer; never a value is shown): list, set, delete.
    #[command(disable_help_subcommand = true)]
    Variable {
        #[command(subcommand)]
        command: CiVariableCommand,
    },
}

/// `gstn ci variable` (M4 C6): a project's CI secrets on the server.
#[derive(Subcommand, Debug)]
pub enum CiVariableCommand {
    /// The project's variables: name, flags, length, who set them (never
    /// a value).
    List {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
    },
    /// Set a variable; the value is read from standard input (one line;
    /// a trailing newline is dropped), never from an argument.
    Set {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
        /// The name, [A-Z][A-Z0-9_]* (not CI, PATH, HOME, HOSTNAME, FORGE_*
        /// or CI_*).
        #[arg(value_parser = parse_text)]
        name: String,
        /// Do not mask the value in job logs (default: masked).
        #[arg(long)]
        unmasked: bool,
    },
    /// Delete a variable.
    Delete {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
        #[arg(value_parser = parse_text)]
        name: String,
        /// Confirm (gstn never prompts).
        #[arg(long)]
        yes: bool,
    },
}

/// The event of `gstn ci plan`.
#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum CiEvent {
    Push,
    Mr,
}

/// `gstn doc` (plan 13 stage 2, ADR-0026): the document format's tools on
/// the working tree. Local only: the files and `.forge/doctypes/` of the
/// repository found upward from the current directory; nothing is sent.
#[derive(Subcommand, Debug)]
pub enum DocCommand {
    /// Format documents: one sentence per line, the front matter's keys
    /// and the blocks' attributes in their type's order.
    Fmt {
        /// Write nothing; exit 1 (FG-DOC-0004) when a file would change.
        #[arg(long)]
        check: bool,
        /// Also rewrite older-format specifications (`spec:` front matter)
        /// into the plan 13 format (ADR-0026 §7); never without it.
        #[arg(long)]
        migrate: bool,
        /// With --migrate: also migrate a `status: draft` / `review`
        /// specification (the plan 13 format keeps no state in the file;
        /// the older value stays in a TODO comment).
        #[arg(long, requires = "migrate")]
        accept_status_loss: bool,
        /// Files or directories (default: the whole repository).
        paths: Vec<PathBuf>,
    },
    /// Check documents against their types and each other (the same lint
    /// as the forge/doc-lint check).
    Lint {
        /// Report only these files or directories (every document is
        /// read: references are checked against the whole repository).
        paths: Vec<PathBuf>,
        /// Report broken references as warnings while migrating (plan 13
        /// Q13; the server's forge/doc-lint decides).
        #[arg(long)]
        migration_warnings: bool,
    },
    /// One document of a project, read through the server's document API
    /// (plan 13 stage 3): its front matter, sections, blocks and resolved
    /// references.
    Show {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
        /// The document id (e.g. RA-2026-014).
        #[arg(value_parser = crate::doc_view::parse_doc_id, value_name = "DOC_ID")]
        id: String,
        /// A commit id, a branch or a tag (default: the default branch).
        #[arg(long = "ref", value_parser = parse_text)]
        rev: Option<String>,
        /// A translation's language (e.g. en).
        #[arg(long, value_parser = parse_text)]
        lang: Option<String>,
        /// Only this block of the document (e.g. H-01), with the
        /// references inside it.
        #[arg(long, value_parser = crate::doc_view::parse_doc_id, value_name = "BLOCK_ID")]
        block: Option<String>,
    },
    /// The documents that refer to one document (or one of its blocks),
    /// through the server's document API: id, classification, path,
    /// title and the lines of the references.
    Refs {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
        /// The document id (e.g. RA-2026-014).
        #[arg(value_parser = crate::doc_view::parse_doc_id, value_name = "DOC_ID")]
        id: String,
        /// Only the references to this block (e.g. H-01).
        #[arg(long, value_parser = crate::doc_view::parse_doc_id, value_name = "BLOCK_ID")]
        block: Option<String>,
        /// A commit id, a branch or a tag (default: the default branch).
        #[arg(long = "ref", value_parser = parse_text)]
        rev: Option<String>,
        /// At most this many referrers, 1..=500 (server default 100).
        #[arg(long, value_parser = parse_spec_limit)]
        limit: Option<u32>,
        /// The previous page's next_after.
        #[arg(long, value_parser = crate::doc_view::parse_doc_id, value_name = "KEY")]
        after: Option<String>,
    },
    /// A project's documents (id, classification, type, path, title),
    /// through the server's document API.
    List {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
        /// A commit id, a branch or a tag (default: the default branch).
        #[arg(long = "ref", value_parser = parse_text)]
        rev: Option<String>,
        /// Only documents of this type.
        #[arg(long, value_parser = parse_text)]
        doctype: Option<String>,
        /// At most this many documents, 1..=500 (server default 100).
        #[arg(long, value_parser = parse_spec_limit)]
        limit: Option<u32>,
        /// The previous page's next_after.
        #[arg(long, value_parser = crate::doc_view::parse_doc_id, value_name = "DOC_ID")]
        after: Option<String>,
    },
    /// The repository setting that lowers broken references to warnings
    /// in forge/doc-lint until a date (plan 13 Q13): show, set, clear.
    #[command(disable_help_subcommand = true)]
    Migration {
        #[command(subcommand)]
        command: DocMigrationCommand,
    },
}

/// `gstn doc migration` (plan 13 Q13): the server's setting.
#[derive(Subcommand, Debug)]
pub enum DocMigrationCommand {
    /// The project's migration date, whether it is in force, and the
    /// server's date.
    Show {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
    },
    /// Lower broken references to warnings until a date (Maintainer).
    Set {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
        /// The last day (UTC), YYYY-MM-DD: after today and at most 180
        /// days ahead.
        #[arg(long, value_parser = parse_text, value_name = "YYYY-MM-DD")]
        until: String,
    },
    /// End the migration now: broken references are errors again.
    Clear {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
        /// Confirm (gstn never prompts).
        #[arg(long)]
        yes: bool,
    },
}

/// `gstn context` (M3 X5, plan 03 §6): the context API, read with your
/// permissions.
#[derive(Subcommand, Debug)]
pub enum ContextCommand {
    /// A kanflow work item's context: the project's linked merge requests
    /// and the `@req PBI-<n>/AC-<m>` tests.
    #[command(name = "work-item")]
    WorkItem {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
        /// The work item, `PBI-<n>`.
        #[arg(long = "ref", value_parser = parse_text, value_name = "PBI-N")]
        reference: String,
    },
    /// A requirement's context: its text, document, tests and versions.
    Requirement {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
        /// The requirement id (ZEVA-CFG-012).
        #[arg(long, value_parser = parse_text)]
        id: String,
        /// A commit id, a branch or a tag (default: the default branch).
        #[arg(long = "ref", value_parser = parse_text)]
        rev: Option<String>,
    },
    /// A merge request's review context: files, requirements, work items,
    /// check runs.
    #[command(name = "merge-request")]
    MergeRequest {
        #[arg(long, value_parser = parse_id, value_name = "ID")]
        project: i64,
        #[arg(long, value_parser = parse_id, value_name = "IID")]
        iid: i64,
    },
}

#[derive(Args, Debug)]
pub struct ConflictsArgs {
    #[arg(long, value_parser = parse_id, value_name = "ID")]
    pub project: i64,
    #[arg(long, value_parser = parse_id, value_name = "IID")]
    pub iid: i64,
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
