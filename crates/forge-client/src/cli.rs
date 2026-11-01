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
    /// Users (administrators): rename.
    #[command(disable_help_subcommand = true)]
    User {
        #[command(subcommand)]
        command: UserCommand,
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
pub enum Kind {
    Spec,
    System,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Spec => "spec",
            Self::System => "system",
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
        /// spec | system (server default: system; fixed at creation).
        #[arg(long, value_enum)]
        kind: Option<Kind>,
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
pub enum UserCommand {
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
