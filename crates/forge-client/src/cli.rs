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
