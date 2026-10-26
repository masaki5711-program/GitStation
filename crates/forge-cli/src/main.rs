//! `forge` — operator CLI and (from M1) server entry point.

use std::process::ExitCode;

use clap::{Parser, Subcommand};
use forge_core::telemetry::{self, LogFormat};
use forge_core::RequestId;
use forge_errors::{ForgeError, Lang, Severity};

mod admin;
mod explain;

#[derive(Parser)]
#[command(name = "forge", version, about = "forge — internal Git platform")]
struct Cli {
    /// Language for user-facing messages.
    #[arg(long, global = true, value_parser = ["ja", "en", "id"], default_value = "ja")]
    lang: String,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show what an FG error code means: cause, diagnosis, fix, runbook.
    Explain {
        /// The code, e.g. FG-SYS-0001
        code: String,
    },
    /// Error registry commands.
    Errors {
        #[command(subcommand)]
        command: ErrorsCommand,
    },
    /// Operator commands (read-only).
    Admin {
        #[command(subcommand)]
        command: AdminCommand,
    },
}

#[derive(Subcommand)]
enum AdminCommand {
    /// Print where a repository is stored (derived from its id) and whether it exists.
    RepoPath {
        /// The repository id.
        repo_id: i64,
        /// Storage root (default: $FORGE_STORAGE_ROOT).
        #[arg(long, env = "FORGE_STORAGE_ROOT")]
        storage_root: std::path::PathBuf,
    },
}

#[derive(Subcommand)]
enum ErrorsCommand {
    /// List every registered error code.
    List,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let lang = Lang::parse(&cli.lang).unwrap_or(Lang::Ja);
    let request_id = RequestId::generate();
    telemetry::init(LogFormat::Pretty, "warn");

    let result = match cli.command {
        Command::Explain { code } => explain::explain(&code).map(|text| print!("{text}")),
        Command::Errors {
            command: ErrorsCommand::List,
        } => {
            print!("{}", explain::list());
            Ok(())
        }
        Command::Admin {
            command:
                AdminCommand::RepoPath {
                    repo_id,
                    storage_root,
                },
        } => admin::repo_path(&storage_root, repo_id).map(|text| print!("{text}")),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => report(&err, &request_id, lang),
    }
}

/// The CLI boundary: log once, show the user message + request id.
fn report(err: &ForgeError, request_id: &RequestId, lang: Lang) -> ExitCode {
    telemetry::log_failure(err, request_id);
    eprintln!(
        "{} [{}] ({}: {request_id})",
        err.user_message(lang),
        err.code(),
        request_id_label(lang)
    );
    // Exit 2 for an expected client-side condition (wrong input), 1 otherwise.
    match err.code().severity() {
        Severity::Info => ExitCode::from(2),
        Severity::Warn | Severity::Alert => ExitCode::FAILURE,
    }
}

/// The label shown next to the request id (問い合わせ番号) in each language.
fn request_id_label(lang: Lang) -> &'static str {
    match lang {
        Lang::Ja => "問い合わせ番号",
        Lang::En => "Request ID",
        Lang::Id => "ID permintaan",
    }
}
