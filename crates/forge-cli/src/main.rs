//! `forge` — operator CLI and (from M1) server entry point.

use std::io::Write as _;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use forge_core::config;
use forge_core::telemetry::{self, LogFormat};
use forge_core::RequestId;
use forge_errors::{ErrorCode, ForgeError, Lang, Severity};

mod admin;
// ── D5 MR5 ──
mod admin_namespace;
// ── end D5 MR5 ──
// ── D5 MR4 ──
mod admin_project;
// ── end D5 MR4 ──
// ── D4 MR3b ──
mod admin_repo;
// ── end D4 MR3b ──
mod admin_token;
mod admin_user;
mod db;
mod doctor;
mod explain;
mod oidc;
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
    /// `bootstrap-admin`, `repo reprovision`, `repo record-push`,
    /// `project purge --now`, `project move`, `group move` and `user rename`
    /// change data (in production: approved runbook steps); the others only
    /// read.
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
    // ── D5 MR4 ──
    /// Projects (step D5).
    Project {
        #[command(subcommand)]
        command: ProjectCommand,
    },
    // ── end D5 MR4 ──
    // ── D5 MR5 ──
    /// Groups (step D5).
    Group {
        #[command(subcommand)]
        command: GroupCommand,
    },
    // ── end D5 MR5 ──
}

// ── D5 MR5 ──
#[derive(Subcommand)]
enum GroupCommand {
    /// Move or rename a group and its subtree (runbook namespace-redirects;
    /// exempt from the API's 10 moves per 24 hours). The old paths keep
    /// `moved` tombstones.
    #[command(group(clap::ArgGroup::new("target").required(true).args(["namespace", "top_level"])))]
    Move {
        /// The group id.
        group_id: i64,
        /// The new parent group's namespace id.
        #[arg(long)]
        namespace: Option<i64>,
        /// Move to (or stay at) the top level.
        #[arg(long)]
        top_level: bool,
        /// The new last path segment (the current one for a pure move).
        #[arg(long)]
        path: String,
        /// Take back an old path of this subtree (release its `moved` tombstones).
        #[arg(long)]
        reclaim: bool,
    },
}
// ── end D5 MR5 ──

// ── D5 MR4 ──
#[derive(Subcommand)]
enum ProjectCommand {
    /// Purge the projects whose grace period is over (runbook
    /// repository-purge): `--dry-run` lists them, `--now` runs one batch of
    /// the worker's purge at once (removes their repositories and rows).
    #[command(group(clap::ArgGroup::new("mode").required(true).args(["dry_run", "now"])))]
    Purge {
        /// List the projects due, change nothing.
        #[arg(long)]
        dry_run: bool,
        /// Purge them now (at most one batch).
        #[arg(long)]
        now: bool,
    },
    // ── D5 MR5 ──
    /// Move or rename a project (runbook namespace-redirects; exempt from
    /// the API's 10 moves per 24 hours). The old path keeps a `moved`
    /// tombstone.
    Move {
        /// The project id.
        project_id: i64,
        /// The new parent's namespace id (a group or a personal namespace;
        /// the current one for a rename).
        #[arg(long)]
        namespace: i64,
        /// The new last path segment (the current one for a pure move).
        #[arg(long)]
        path: String,
        /// Take back an old path of this project (release its `moved` tombstone).
        #[arg(long)]
        reclaim: bool,
    },
    // ── end D5 MR5 ──
}
// ── end D5 MR4 ──

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
    // ── D5 MR5 ──
    /// Rename a user: the username and the personal namespace's path
    /// (runbook namespace-redirects; exempt from the rate limit).
    Rename {
        /// The current username.
        username: String,
        /// The new username.
        new_username: String,
        /// Take back an old username of this user (release its `moved` tombstone).
        #[arg(long)]
        reclaim: bool,
    },
    // ── end D5 MR5 ──
    // ── M2 MR4 ──
    /// Link a sign-in identity (OIDC issuer + subject) to a user
    /// (runbook oidc-identity-link; settles FG-AUTH-0014).
    LinkIdentity {
        /// The username.
        #[arg(long)]
        user: String,
        /// The issuer, exactly as `auth.oidc.issuer`.
        #[arg(long)]
        issuer: String,
        /// The `sub` claim of the identity.
        #[arg(long)]
        subject: String,
    },
    // ── end M2 MR4 ──
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
                    command:
                        UserCommand::Disable { .. }
                        | UserCommand::Rename { .. }
                        | UserCommand::LinkIdentity { .. },
                }
                | AdminCommand::Group {
                    command: GroupCommand::Move { .. },
                }
                | AdminCommand::BootstrapAdmin { .. }
                | AdminCommand::Repo {
                    command: RepoCommand::Reprovision { .. } | RepoCommand::RecordPush { .. },
                }
                | AdminCommand::Project {
                    command: ProjectCommand::Purge { now: true, .. } | ProjectCommand::Move { .. },
                },
        } => telemetry::init(LogFormat::Json, "info"),
        Command::Explain { .. }
        | Command::Errors { .. }
        | Command::Admin {
            command:
                AdminCommand::RepoPath { .. }
                | AdminCommand::Token {
                    command: TokenCommand::List { .. },
                }
                | AdminCommand::Project {
                    command: ProjectCommand::Purge { now: false, .. },
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
        Command::Admin {
            command:
                AdminCommand::Project {
                    command: ProjectCommand::Purge { now, .. },
                },
        } => config().and_then(|cfg| admin_project::purge(&cfg, &request_id, now)),
        Command::Admin {
            command:
                AdminCommand::Project {
                    command:
                        ProjectCommand::Move {
                            project_id,
                            namespace,
                            path,
                            reclaim,
                        },
                },
        } => config().and_then(|cfg| {
            admin_namespace::move_project(
                &cfg.database,
                &request_id,
                project_id,
                &admin_namespace::MoveArgs {
                    namespace: Some(namespace),
                    path,
                    reclaim,
                },
            )
        }),
        Command::Admin {
            command:
                AdminCommand::Group {
                    command:
                        GroupCommand::Move {
                            group_id,
                            namespace,
                            top_level: _,
                            path,
                            reclaim,
                        },
                },
        } => config().and_then(|cfg| {
            // clap requires exactly one of --namespace / --top-level.
            admin_namespace::move_group(
                &cfg.database,
                &request_id,
                group_id,
                &admin_namespace::MoveArgs {
                    namespace,
                    path,
                    reclaim,
                },
            )
        }),
        Command::Admin {
            command:
                AdminCommand::User {
                    command:
                        UserCommand::Rename {
                            username,
                            new_username,
                            reclaim,
                        },
                },
        } => config().and_then(|cfg| {
            admin_namespace::rename_user(
                &cfg.database,
                &request_id,
                &username,
                &new_username,
                reclaim,
            )
        }),
        Command::Admin {
            command:
                AdminCommand::User {
                    command:
                        UserCommand::LinkIdentity {
                            user,
                            issuer,
                            subject,
                        },
                },
        } => config().and_then(|cfg| {
            admin_user::link_identity(&cfg.database, &request_id, &user, &issuer, &subject)
        }),
        Command::Serve { bind } => config().and_then(|cfg| {
            let bind = bind.unwrap_or_else(|| cfg.server.bind.clone()); // clone: cfg is passed whole below
            serve::serve(&bind, &cfg)
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
    fn user_link_identity_takes_user_issuer_and_subject() {
        let cli = parse(&[
            "admin",
            "user",
            "link-identity",
            "--user",
            "alice",
            "--issuer",
            "https://sso.example.invalid/realms/r",
            "--subject",
            "sub-1",
        ])
        .expect("parses");
        assert!(matches!(
            cli.command,
            Command::Admin {
                command: AdminCommand::User {
                    command: UserCommand::LinkIdentity { ref user, ref issuer, ref subject }
                }
            } if user == "alice" && issuer.ends_with("/realms/r") && subject == "sub-1"
        ));
        assert!(
            parse(&["admin", "user", "link-identity", "--user", "alice"]).is_err(),
            "issuer and subject are required"
        );
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
    fn project_purge_takes_exactly_one_of_dry_run_and_now() {
        for (args, now) in [
            (&["admin", "project", "purge", "--dry-run"][..], false),
            (&["admin", "project", "purge", "--now"][..], true),
        ] {
            let cli = parse(args).expect("parses");
            assert!(
                matches!(
                    cli.command,
                    Command::Admin {
                        command: AdminCommand::Project {
                            command: ProjectCommand::Purge { now: n, .. }
                        }
                    } if n == now
                ),
                "{args:?}"
            );
        }
        assert!(
            parse(&["admin", "project", "purge"]).is_err(),
            "a mode is required"
        );
        assert!(
            parse(&["admin", "project", "purge", "--dry-run", "--now"]).is_err(),
            "not both"
        );
    }

    #[test]
    fn moves_take_ids_a_target_and_a_path() {
        let cli = parse(&[
            "admin",
            "project",
            "move",
            "7",
            "--namespace",
            "3",
            "--path",
            "web",
        ])
        .expect("parses");
        assert!(matches!(
            cli.command,
            Command::Admin {
                command: AdminCommand::Project {
                    command: ProjectCommand::Move { project_id: 7, namespace: 3, ref path, reclaim: false }
                }
            } if path == "web"
        ));
        assert!(
            parse(&["admin", "project", "move", "7", "--path", "web"]).is_err(),
            "a target"
        );
        assert!(
            parse(&["admin", "project", "move", "7", "--namespace", "3"]).is_err(),
            "a path"
        );
        let cli = parse(&[
            "admin",
            "group",
            "move",
            "4",
            "--top-level",
            "--path",
            "acme",
            "--reclaim",
        ])
        .expect("parses");
        assert!(matches!(
            cli.command,
            Command::Admin {
                command: AdminCommand::Group {
                    command: GroupCommand::Move {
                        group_id: 4,
                        namespace: None,
                        top_level: true,
                        reclaim: true,
                        ..
                    }
                }
            }
        ));
        assert!(
            parse(&["admin", "group", "move", "4", "--path", "acme"]).is_err(),
            "--namespace or --top-level"
        );
        assert!(
            parse(&[
                "admin",
                "group",
                "move",
                "4",
                "--top-level",
                "--namespace",
                "2",
                "--path",
                "a",
            ])
            .is_err(),
            "not both"
        );
        let cli = parse(&["admin", "user", "rename", "bob", "robert"]).expect("parses");
        assert!(matches!(
            cli.command,
            Command::Admin {
                command: AdminCommand::User {
                    command: UserCommand::Rename { ref username, ref new_username, reclaim: false }
                }
            } if username == "bob" && new_username == "robert"
        ));
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
