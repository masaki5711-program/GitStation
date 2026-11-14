//! `forge` — operator CLI and (from M1) server entry point.

use std::io::Write as _;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use forge_core::config;
use forge_core::telemetry::{self, LogFormat};
use forge_core::RequestId;
use forge_errors::{ErrorCode, ForgeError, Lang, Severity};

mod admin;
// M3 I1c: `forge admin events new-epoch`.
mod admin_events;
// ── D5 MR5 ──
mod admin_namespace;
// ── end D5 MR5 ──
// ── D5 MR4 ──
mod admin_project;
// ── end D5 MR4 ──
// ── D4 MR3b ──
mod admin_repo;
// M4 C4: `forge admin runner`.
mod admin_runner;
// ── end D4 MR3b ──
mod admin_token;
mod admin_user;
// M3 O1: `forge backup` / `forge restore`.
mod backup;
// M4 C2: the confidential mark of a pipeline's commit.
mod ci_marks;
mod db;
mod doctor;
mod explain;
// M3 X8: the push checks' pre-receive hook.
mod hook;
mod oidc;
mod restore;
mod runtime;
mod serve;
mod serve_merge_queue;
mod serve_merges;
mod state_copy;
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
    /// Internal: run by git's hooks inside receive-pack (M3 X8), never by
    /// hand.
    #[command(hide = true)]
    Hook {
        #[command(subcommand)]
        command: HookCommand,
    },
    /// Back up the database (pgBackRest) and then every repository into a
    /// new encrypted generation in `backup.dir` (runbook backup-restore).
    Backup,
    /// Restore from a backup (runbook backup-restore; production: approved
    /// runbook steps). `verify` only reads.
    Restore {
        #[command(subcommand)]
        command: RestoreCommand,
    },
}

// ── M3 O1 ──
#[derive(Subcommand)]
enum RestoreCommand {
    /// Mark a restore in progress: forge serve and forge worker refuse to
    /// start until `finish`.
    Begin,
    /// Write the repositories of a generation back into `storage.root`.
    Repositories {
        /// The generation (`YYYYMMDDTHHMMSSZ`, a directory of `backup.dir`).
        #[arg(long)]
        generation: String,
        /// Move existing repository directories aside
        /// (`<storage.root>/pre-restore-<generation>/`) instead of refusing.
        #[arg(long)]
        replace: bool,
    },
    /// Compare the database's expectations with the repositories (version
    /// refs, `git fsck --connectivity-only`). Changes nothing.
    Verify,
    /// Verify, revoke every credential, re-apply the removed permissions,
    /// give the event feed a new epoch, end the restore.
    Finish {
        /// The generation that was restored.
        #[arg(long)]
        generation: String,
        /// Go on when the database is older than the repositories (the
        /// unknown refs and repositories are listed).
        #[arg(long)]
        accept_db_behind: bool,
        /// Use an older state copy when the newest one does not open
        /// (otherwise every administrator flag is removed).
        #[arg(long)]
        allow_older_copy: bool,
    },
    /// Give the administrator flag back to a user the restore removed it
    /// from (after checking the person).
    RegrantAdmin {
        /// The username.
        username: String,
    },
}
// ── end M3 O1 ──

#[derive(Subcommand)]
enum HookCommand {
    /// The push checks (large files, secrets) of one push.
    PreReceive,
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
    // ── M3 I1c ──
    /// The event feed (ADR-0016 §7).
    Events {
        #[command(subcommand)]
        command: EventsCommand,
    },
    // ── end M3 I1c ──
    // ── M3 S2 ──
    /// Acting for a person (ADR-0025): diagnosis only.
    ActAs {
        #[command(subcommand)]
        command: ActAsCommand,
    },
    // ── end M3 S2 ──
    // ── M4 C4 ──
    /// CI runners (runbook ci-runner): register one and print its token
    /// once, list them, disable one.
    Runner {
        #[command(subcommand)]
        command: RunnerCommand,
    },
    // ── end M4 C4 ──
    // ── M4 C13 ──
    /// The SSH git transport's host key (runbook ssh-transport): make it
    /// once, or show its fingerprint.
    SshHostKey {
        #[command(subcommand)]
        command: SshHostKeyCommand,
    },
    // ── end M4 C13 ──
}

// ── M4 C13 ──
#[derive(Subcommand)]
enum SshHostKeyCommand {
    /// Make the Ed25519 host key at `git.ssh.host_key_file` (never replaces
    /// an existing file) and print its fingerprint and `known_hosts` line.
    Generate,
    /// Print the fingerprint and `known_hosts` line of the configured key.
    Show,
}
// ── end M4 C13 ──

// ── M4 C4 ──
#[derive(Subcommand)]
enum RunnerCommand {
    /// Register a runner and print its token `fgr_…` once (put it in the
    /// runner host's token file).
    Register {
        /// `[a-z0-9][a-z0-9._-]{0,62}`, unique.
        #[arg(long)]
        name: String,
        /// Comma-separated tags (a job runs on a runner whose tags cover all of its own).
        #[arg(long)]
        tags: Option<String>,
        /// At most 1024 bytes.
        #[arg(long)]
        description: Option<String>,
        /// The administrator recorded as registering it (an active human administrator).
        #[arg(long)]
        admin: String,
    },
    /// List the runners (never a token).
    List,
    /// Disable a runner: its token and its jobs' tokens are refused from now on.
    Disable {
        /// The runner id (from `runner list`).
        runner_id: i64,
        /// The administrator recorded as disabling it (an active human administrator).
        #[arg(long)]
        admin: String,
    },
}
// ── end M4 C4 ──

// ── M3 S2 ──
#[derive(Subcommand)]
enum ActAsCommand {
    /// Print the keyed HMAC an act-as audit row records for a
    /// `Forge-Act-As` value (`act_as_claimed`, ADR-0025 §5.2), to find the
    /// rows with SQL. Reads the key file only; writes nothing.
    ClaimedMac {
        /// The header's value as kanflow sent it (`user:<id>` or
        /// `employee:<number>`).
        #[arg(long)]
        value: String,
        /// The key id (default: the active key; an older row names its
        /// `act_as_key_id`).
        #[arg(long)]
        kid: Option<i16>,
    },
}
// ── end M3 S2 ──

// ── M3 I1c ──
#[derive(Subcommand)]
enum EventsCommand {
    /// Give the event feed a new epoch after a database restore or rebuild
    /// (an approved runbook step; needs database.maint_url): every
    /// integration's cursor answers 410 epoch_mismatch and the consumers
    /// resync.
    NewEpoch,
}
// ── end M3 I1c ──

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
    // ── M3 O3 ──
    /// Enqueue one maintenance of a ready repository (gc with the prune
    /// grace, pack-refs, commit-graph, fsck, counts; runbook
    /// repo-maintenance). A running `forge worker` runs it at once.
    Maintain {
        /// The repository id.
        repo_id: i64,
    },
    // ── end M3 O3 ──
    /// Allow the one-time history import into an EMPTY repository
    /// (2026-11-12, runbook push-checks "history import"): its first
    /// accepted push may carry secrets in old commits (audited, never in
    /// the tip). Refused when the repository has refs (FG-GIT-0025).
    AllowImport {
        /// The repository id (`forge admin repo-path` names its directory).
        repo_id: i64,
        /// How long the allowance stays open (1 to 168 hours).
        #[arg(long, default_value_t = 24)]
        ttl_hours: u64,
    },
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
    // The hook's output goes to the pusher: no log subscriber, no config.
    if let Command::Hook {
        command: HookCommand::PreReceive,
    } = cli.command
    {
        return hook::pre_receive();
    }
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
                    command:
                        RepoCommand::Reprovision { .. }
                        | RepoCommand::RecordPush { .. }
                        | RepoCommand::Maintain { .. }
                        | RepoCommand::AllowImport { .. },
                }
                | AdminCommand::Project {
                    command: ProjectCommand::Purge { now: true, .. } | ProjectCommand::Move { .. },
                }
                // M3 I1c: a mutating runbook step (its FG-DB-0014 line is
                // the trail besides the audit row).
                | AdminCommand::Events {
                    command: EventsCommand::NewEpoch,
                }
                | AdminCommand::Runner {
                    command: RunnerCommand::Register { .. } | RunnerCommand::Disable { .. },
                }
                | AdminCommand::SshHostKey {
                    command: SshHostKeyCommand::Generate,
                },
        }
        // M3 O1: backup and the mutating restore steps.
        | Command::Backup
        | Command::Restore {
            command:
                RestoreCommand::Begin
                | RestoreCommand::Repositories { .. }
                | RestoreCommand::Finish { .. }
                | RestoreCommand::RegrantAdmin { .. },
        } => telemetry::init(LogFormat::Json, "info"),
        Command::Restore {
            command: RestoreCommand::Verify,
        } => telemetry::init(LogFormat::Pretty, "warn"),
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
                }
                | AdminCommand::ActAs {
                    command: ActAsCommand::ClaimedMac { .. },
                }
                | AdminCommand::Runner {
                    command: RunnerCommand::List,
                }
                | AdminCommand::SshHostKey {
                    command: SshHostKeyCommand::Show,
                },
        }
        | Command::Doctor { .. }
        | Command::Db {
            command: DbCommand::Status { .. },
        } => telemetry::init(LogFormat::Pretty, "warn"),
        // Returned above, before any subscriber.
        Command::Hook { .. } => false,
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
                AdminCommand::Events {
                    command: EventsCommand::NewEpoch,
                },
        } => config().and_then(|cfg| admin_events::new_epoch(&cfg.database, &request_id)),
        Command::Admin {
            command:
                AdminCommand::ActAs {
                    command: ActAsCommand::ClaimedMac { value, kid },
                },
        } => config().and_then(|cfg| admin::act_as_claimed_mac(&cfg, &value, kid)),
        Command::Admin {
            command: AdminCommand::SshHostKey { command },
        } => config().and_then(|cfg| match command {
            SshHostKeyCommand::Generate => admin::ssh_host_key_generate(&cfg),
            SshHostKeyCommand::Show => admin::ssh_host_key_show(&cfg),
        }),
        Command::Admin {
            command: AdminCommand::Runner { command },
        } => config().and_then(|cfg| match command {
            RunnerCommand::Register {
                name,
                tags,
                description,
                admin,
            } => admin_runner::register(
                &cfg.database,
                &request_id,
                &admin_runner::RegisterArgs {
                    name,
                    tags,
                    description,
                    admin,
                },
            ),
            RunnerCommand::List => admin_runner::list(&cfg.database),
            RunnerCommand::Disable { runner_id, admin } => {
                admin_runner::disable(&cfg.database, &request_id, runner_id, &admin)
            }
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
                AdminCommand::Repo {
                    command: RepoCommand::Maintain { repo_id },
                },
        } => config().and_then(|cfg| admin_repo::maintain(&cfg.database, &request_id, repo_id)),
        Command::Admin {
            command:
                AdminCommand::Repo {
                    command: RepoCommand::AllowImport { repo_id, ttl_hours },
                },
        } => {
            config().and_then(|cfg| admin_repo::allow_import(&cfg, &request_id, repo_id, ttl_hours))
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
        // ── M3 O1 ──
        Command::Backup => config().and_then(|cfg| backup::backup(&cfg, &request_id)),
        Command::Restore { command } => config().and_then(|cfg| match command {
            RestoreCommand::Begin => restore::begin(cfg.storage_root()?, &request_id),
            RestoreCommand::Repositories {
                generation,
                replace,
            } => {
                let prepared = backup::prepare(&cfg.backup)?;
                let root = forge_git::StorageRoot::open(cfg.storage_root()?)?;
                restore::repositories(
                    &prepared,
                    &root,
                    &generation,
                    replace,
                    cfg.backup.repo_timeout,
                    &request_id,
                )
            }
            RestoreCommand::Verify => {
                let storage = worker::provision::Storage::from_config(&cfg)?;
                admin_token::with_db(&cfg.database, async |db| {
                    restore::verify_command(db, &storage.root, &storage.git, &request_id).await
                })
            }
            RestoreCommand::Finish {
                generation,
                accept_db_behind,
                allow_older_copy,
            } => restore::finish(
                &cfg,
                &restore::FinishArgs {
                    generation: &generation,
                    accept_db_behind,
                    allow_older_copy,
                },
                &request_id,
            ),
            RestoreCommand::RegrantAdmin { username } => {
                admin_token::with_db(&cfg.database, async |db| {
                    restore::regrant_admin_with(db, &username, &request_id).await
                })
            }
        }),
        // ── end M3 O1 ──
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
        Command::Hook { .. } => {
            Err(ForgeError::new(ErrorCode::Internal).with_context("operation", "hook_dispatch"))
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
    fn events_new_epoch_takes_no_arguments() {
        assert!(matches!(
            parse(&["admin", "events", "new-epoch"])
                .expect("parses")
                .command,
            Command::Admin {
                command: AdminCommand::Events {
                    command: EventsCommand::NewEpoch
                }
            }
        ));
        assert!(parse(&["admin", "events", "new-epoch", "--force"]).is_err());
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
        // M3 O3: enqueue one maintenance.
        let cli = parse(&["admin", "repo", "maintain", "7"]).expect("parses");
        assert!(matches!(
            cli.command,
            Command::Admin {
                command: AdminCommand::Repo {
                    command: RepoCommand::Maintain { repo_id: 7 }
                }
            }
        ));
        assert!(parse(&["admin", "repo", "maintain"]).is_err());
        // 2026-11-12: the one-time history import (24 hours by default).
        let cli = parse(&["admin", "repo", "allow-import", "5"]).expect("parses");
        assert!(matches!(
            cli.command,
            Command::Admin {
                command: AdminCommand::Repo {
                    command: RepoCommand::AllowImport {
                        repo_id: 5,
                        ttl_hours: 24
                    }
                }
            }
        ));
        let cli =
            parse(&["admin", "repo", "allow-import", "5", "--ttl-hours", "2"]).expect("parses");
        assert!(matches!(
            cli.command,
            Command::Admin {
                command: AdminCommand::Repo {
                    command: RepoCommand::AllowImport { ttl_hours: 2, .. }
                }
            }
        ));
        assert!(parse(&["admin", "repo", "allow-import"]).is_err());
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
