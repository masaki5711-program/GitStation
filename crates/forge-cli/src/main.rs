//! `forge` — operator CLI and (from M1) server entry point.

use std::io::Write as _;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use forge_core::telemetry::{self, LogFormat};
use forge_core::RequestId;
use forge_errors::{ErrorCode, ForgeError, Lang, Severity};

mod admin;
mod explain;
mod serve;

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
    /// Run the HTTP server.
    Serve {
        /// Listen address.
        #[arg(long, env = "FORGE_BIND", default_value = "127.0.0.1:8080")]
        bind: String,
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
    // The server logs JSON at info level; operator commands stay quiet.
    match cli.command {
        Command::Serve { .. } => telemetry::init(LogFormat::Json, "info"),
        Command::Explain { .. } | Command::Errors { .. } | Command::Admin { .. } => {
            telemetry::init(LogFormat::Pretty, "warn")
        }
    };

    let result = match cli.command {
        Command::Explain { code } => explain::explain(&code),
        Command::Errors {
            command: ErrorsCommand::List,
        } => Ok(explain::list()),
        Command::Admin {
            command:
                AdminCommand::RepoPath {
                    repo_id,
                    storage_root,
                },
        } => admin::repo_path(&storage_root, repo_id),
        Command::Serve { bind } => serve::serve(&bind),
    };
    match result.and_then(|text| emit(&text)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => report(&err, &request_id, lang),
    }
}

/// Write the command output. A closed pipe (`forge errors list | head`) is
/// not a failure; any other write error is reported, never a panic.
fn emit(text: &str) -> Result<(), ForgeError> {
    let mut out = std::io::stdout().lock();
    match out.write_all(text.as_bytes()).and_then(|()| out.flush()) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        Err(e) => Err(ForgeError::new(ErrorCode::Internal)
            .with_context("operation", "write_stdout")
            .with_source(e)),
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
