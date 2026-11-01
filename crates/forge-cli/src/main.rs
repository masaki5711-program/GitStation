//! `forge` — operator CLI and (from M1) server entry point.

use std::io::Write as _;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use forge_core::config;
use forge_core::telemetry::{self, LogFormat};
use forge_core::RequestId;
use forge_errors::{ErrorCode, ForgeError, Lang, Severity};

mod admin;
// ── D4 MR3b ──
mod admin_repo;
// ── end D4 MR3b ──
mod admin_token;
mod admin_user;
mod db;
mod doctor;
mod explain;
mod runtime;
mod serve;
mod worker;

#[derive(Parser)]
#[command(name = "forge", version, about = "forge — internal Git platform")]
struct Cli {
    /// Language for user-facing messages.
    #[arg(long, global = true, value_parser = ["ja", "en", "id"], default_value = "ja")]
    lang: String,
    /// Configuration file (TOML); environment variables `FORGE_<SECTION>_<KEY>`
    /// override it, and command-line flags override both.
    #[arg(long, global = true, env = "FORGE_CONFIG")]
    config: Option<std::path::PathBuf>,
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
        /// Listen address (default: `server.bind`, 127.0.0.1:8080).
        #[arg(long)]
        bind: Option<String>,
    },
    /// Run the background tasks (the outbox sequencer; later jobs and
    /// audit seals) until SIGINT / SIGTERM.
    Worker,
    /// Read-only health checks of everything `forge serve` needs
    /// (configuration, database, schema, storage, git).
    Doctor {
        /// Machine-readable output.
        #[arg(long)]
        json: bool,
    },
    /// Database schema: status (read-only) and migrate.
    Db {
        #[command(subcommand)]
        command: DbCommand,
    },
    /// Operator commands. `token issue` / `token revoke`, `user disable`,
    /// `bootstrap-admin`, `repo reprovision` and `repo record-push` change
    /// data (in production: approved runbook steps); the others only read.
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
        /// Storage root (default: `storage.root`).
        #[arg(long)]
        storage_root: Option<std::path::PathBuf>,
    },
    /// Personal access tokens (ADR-0019).
    Token {
        #[command(subcommand)]
        command: TokenCommand,
    },
    /// Users (ADR-0019 §10).
    User {
        #[command(subcommand)]
        command: UserCommand,
    },
    /// Make an active human the administrator when forge has none (first
    /// install, or recovery after a forced disable). Refused while an active
    /// human administrator exists.
    BootstrapAdmin {
        /// The username to promote (or, with --create, to create).
        #[arg(long)]
        user: String,
        /// Create the user (a local human) instead of promoting an existing one.
        #[arg(long, requires = "email")]
        create: bool,
        /// The new user's email (with --create).
        #[arg(long, requires = "create")]
        email: Option<String>,
        /// The new user's display name (with --create; default: the username).
        #[arg(long, requires = "create")]
        display_name: Option<String>,
    },
    // ── D4 MR3b ──
    /// Repositories (step D4).
    Repo {
        #[command(subcommand)]
        command: RepoCommand,
    },
    // ── end D4 MR3b ──
}

// ── D4 MR3b ──
#[derive(Subcommand)]
enum RepoCommand {
    /// Enqueue the provisioning job of a repository still `provisioning`
    /// again (after its job died: `forge doctor` repositories.provisioning,
    /// FG-REPO-0012). Refused for a ready repository.
    Reprovision {
        /// The repository id.
        repo_id: i64,
    },
    // ── M1 phase 9 MR2 ──
    /// Record pushes whose `repository.pushed` record could not be written
    /// (FG-GIT-0012, runbook push-record-failed), from their
    /// `forge.git.push_applied` log lines. Idempotent: a push already
    /// recorded is reported and left as it is.
    RecordPush {
        /// A file holding the push's log lines (other lines are skipped).
        #[arg(long)]
        from_log: std::path::PathBuf,
    },
    // ── end M1 phase 9 MR2 ──
}
// ── end D4 MR3b ──

#[derive(Subcommand)]
enum UserCommand {
    /// Disable a user and revoke their active tokens. Refused when it would
    /// leave no active human administrator, or a top-level group without a
    /// direct active Owner, unless --force.
    Disable {
        /// The username.
        username: String,
        /// Disable even if no administrator or group Owner is left
        /// (recorded in the audit log; recover with `bootstrap-admin`).
        #[arg(long)]
        force: bool,
    },
}

#[derive(Subcommand)]
enum TokenCommand {
    /// Issue a token for a user and print it once.
    Issue {
        /// The owner's username.
        #[arg(long)]
        user: String,
        /// Comma-separated scopes: read_repository, write_repository, read_api, api, admin.
        #[arg(long)]
        scopes: String,
        /// Lifetime as `<days>d`, 1d..365d (default 90d).
        #[arg(long)]
        expires_in: Option<String>,
        /// A label for the token (1..100 characters).
        #[arg(long, default_value = "cli")]
        name: String,
        /// The token is held by an AI agent (ADR-0017 §4.0).
        #[arg(long)]
        agent: bool,
    },
    /// Revoke a token by id.
    Revoke {
        /// The token id (from `token list`).
        token_id: i64,
    },
    /// List a user's tokens (never the secret).
    List {
        /// The owner's username.
        #[arg(long)]
        user: String,
    },
}

#[derive(Subcommand)]
enum DbCommand {
    /// Show applied / pending / unknown / modified / dirty migrations and
    /// whether `forge serve` would start. Changes nothing.
    Status {
        /// Exit non-zero with FG-DB-0004 when `forge serve` would refuse to
        /// start (for scripts and runbook checks).
        #[arg(long)]
        check: bool,
    },
    /// Apply pending migrations (production: approved runbook step,
    /// ops/runbooks/db-migration.md).
    Migrate,
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
    // The server, and the one mutating operator command (its
    // forge.db.migrate.applied event is the audit trail of a schema change),
    // log JSON at info level; the other operator commands stay quiet.
    match cli.command {
        Command::Serve { .. }
        | Command::Worker
        | Command::Db {
            command: DbCommand::Migrate,
        }
        | Command::Admin {
            command:
                AdminCommand::Token {
                    command: TokenCommand::Issue { .. } | TokenCommand::Revoke { .. },
                }
                | AdminCommand::User {
                    command: UserCommand::Disable { .. },
                }
                | AdminCommand::BootstrapAdmin { .. }
                | AdminCommand::Repo {
                    command: RepoCommand::Reprovision { .. } | RepoCommand::RecordPush { .. },
                },
        } => telemetry::init(LogFormat::Json, "info"),
        Command::Explain { .. }
        | Command::Errors { .. }
        | Command::Admin {
            command:
                AdminCommand::RepoPath { .. }
                | AdminCommand::Token {
                    command: TokenCommand::List { .. },
                },
        }
        | Command::Doctor { .. }
        | Command::Db {
            command: DbCommand::Status { .. },
        } => telemetry::init(LogFormat::Pretty, "warn"),
    };

    let config_file = cli.config;
    // doctor prints its report even when a check fails, then exits with the
    // most severe failure.
    if let Command::Doctor { json } = cli.command {
        return match doctor::run(config_file.as_deref(), json, &request_id) {
            // The checks were logged inside doctor; print the worst only.
            Ok(r) => match (emit(&r.text), r.failure) {
                (Err(write_err), failure) => {
                    if let Some(f) = failure {
                        print_failure(&f, &request_id, lang);
                    }
                    report(&write_err, &request_id, lang)
                }
                (Ok(()), Some(f)) => print_failure(&f, &request_id, lang),
                (Ok(()), None) => ExitCode::SUCCESS,
            },
            Err(e) => report(&e, &request_id, lang),
        };
    }
    let config = || {
        let env = config::Env::from_process()?;
        config::load(config_file.as_deref(), &env)
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
        } => match storage_root {
            // An explicit root needs nothing else from the configuration.
            Some(root) => admin::repo_path(&root, repo_id),
            None => config().and_then(|cfg| admin::repo_path(cfg.storage_root()?, repo_id)),
        },
        Command::Admin {
            command: AdminCommand::Token { command },
        } => config().and_then(|cfg| match command {
            TokenCommand::Issue {
                user,
                scopes,
                expires_in,
                name,
                agent,
            } => admin_token::issue(
                &cfg.database,
                &request_id,
                &admin_token::IssueArgs {
                    username: user,
                    scopes,
                    expires_in,
                    name,
                    agent,
                },
            ),
            TokenCommand::Revoke { token_id } => {
                admin_token::revoke(&cfg.database, &request_id, token_id)
            }
            TokenCommand::List { user } => admin_token::list(&cfg.database, &user),
        }),
        Command::Admin {
            command:
                AdminCommand::User {
                    command: UserCommand::Disable { username, force },
                },
        } => config()
            .and_then(|cfg| admin_user::disable(&cfg.database, &request_id, &username, force)),
        Command::Admin {
            command:
                AdminCommand::BootstrapAdmin {
                    user,
                    create,
                    email,
                    display_name,
                },
        } => config().and_then(|cfg| {
            let create = match (create, email) {
                (true, Some(email)) => Some(admin_user::NewAdmin {
                    email,
                    display_name,
                }),
                // clap requires --email with --create; without --create there is nothing to create.
                _ => None,
            };
            admin_user::bootstrap_admin(
                &cfg.database,
                &request_id,
                &admin_user::BootstrapArgs {
                    username: user,
                    create,
                },
            )
        }),
        Command::Admin {
            command:
                AdminCommand::Repo {
                    command: RepoCommand::Reprovision { repo_id },
                },
        } => config().and_then(|cfg| admin_repo::reprovision(&cfg.database, &request_id, repo_id)),
        Command::Admin {
            command:
                AdminCommand::Repo {
                    command: RepoCommand::RecordPush { from_log },
                },
        } => {
            config().and_then(|cfg| admin_repo::record_push(&cfg.database, &request_id, &from_log))
        }
        Command::Serve { bind } => config().and_then(|cfg| {
            let bind = bind.unwrap_or(cfg.server.bind);
            serve::serve(&bind, &cfg.database)
        }),
        Command::Worker => config().and_then(|cfg| worker::worker(&cfg)),
        Command::Db {
            command: DbCommand::Status { check },
        } => config().and_then(|cfg| db::status(&cfg.database, check)),
        Command::Db {
            command: DbCommand::Migrate,
        } => config().and_then(|cfg| db::migrate(&cfg.database)),
        // Returned above; reaching this arm is a programming error, reported.
        Command::Doctor { .. } => {
            Err(ForgeError::new(ErrorCode::Internal).with_context("operation", "doctor_dispatch"))
        }
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
    print_failure(err, request_id, lang)
}

/// The user-facing line and exit code, for a failure already logged.
fn print_failure(err: &ForgeError, request_id: &RequestId, lang: Lang) -> ExitCode {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("forge").chain(args.iter().copied()))
    }

    #[test]
    fn user_disable_takes_a_username_and_an_optional_force() {
        let cli = parse(&["admin", "user", "disable", "alice", "--force"]).expect("parses");
        assert!(matches!(
            cli.command,
            Command::Admin {
                command: AdminCommand::User {
                    command: UserCommand::Disable { ref username, force: true }
                }
            } if username == "alice"
        ));
        let cli = parse(&["admin", "user", "disable", "alice"]).expect("parses");
        assert!(matches!(
            cli.command,
            Command::Admin {
                command: AdminCommand::User {
                    command: UserCommand::Disable { force: false, .. }
                }
            }
        ));
    }

    #[test]
    fn repo_reprovision_takes_a_repository_id() {
        let cli = parse(&["admin", "repo", "reprovision", "42"]).expect("parses");
        assert!(matches!(
            cli.command,
            Command::Admin {
                command: AdminCommand::Repo {
                    command: RepoCommand::Reprovision { repo_id: 42 }
                }
            }
        ));
        assert!(parse(&["admin", "repo", "reprovision"]).is_err());
        assert!(parse(&["admin", "repo", "reprovision", "x"]).is_err());
        let cli =
            parse(&["admin", "repo", "record-push", "--from-log", "push.log"]).expect("parses");
        assert!(matches!(
            cli.command,
            Command::Admin {
                command: AdminCommand::Repo {
                    command: RepoCommand::RecordPush { ref from_log }
                }
            } if from_log == std::path::Path::new("push.log")
        ));
        assert!(parse(&["admin", "repo", "record-push"]).is_err());
        // The read-only repo-path command is unchanged.
        assert!(parse(&["admin", "repo-path", "42"]).is_ok());
    }

    #[test]
    fn bootstrap_admin_create_needs_an_email_and_an_email_needs_create() {
        assert!(parse(&["admin", "bootstrap-admin", "--user", "root"]).is_ok());
        assert!(parse(&[
            "admin",
            "bootstrap-admin",
            "--user",
            "root",
            "--create",
            "--email",
            "root@example.test",
            "--display-name",
            "Root",
        ])
        .is_ok());
        assert!(
            parse(&["admin", "bootstrap-admin", "--user", "root", "--create"]).is_err(),
            "--create without --email"
        );
        assert!(
            parse(&[
                "admin",
                "bootstrap-admin",
                "--user",
                "root",
                "--email",
                "root@example.test"
            ])
            .is_err(),
            "--email without --create"
        );
    }
}
