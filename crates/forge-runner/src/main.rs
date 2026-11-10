//! `forge-runner run|check [--config <path>]` (runbook `ci-runner`).

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, Subcommand};
use forge_core::telemetry::{self, LogFormat};
use forge_core::RequestId;
use forge_errors::{ErrorCode, ForgeError};
use forge_runner::api::HttpApi;
use forge_runner::config::RunnerConfig;
use forge_runner::git::GitPlan;
use forge_runner::job::{Executor, GitLimits};
use forge_runner::podman::PodmanPlan;
use forge_runner::process::{ProcessRunner, SystemRunner};
use forge_runner::runner::{Runner, HEARTBEAT_EVERY};
use forge_runner::workspace::Workspace;

#[derive(Parser)]
#[command(name = "forge-runner", version, about = "GitStation CI runner")]
struct Cli {
    /// The configuration file.
    #[arg(long, default_value = "/etc/forge-runner/config.toml")]
    config: PathBuf,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Claim and run jobs until stopped (the default).
    Run,
    /// Check the host: configuration, token, git, rootless Podman.
    Check,
}

fn not_root() -> Result<(), ForgeError> {
    #[cfg(unix)]
    if rustix::process::geteuid().is_root() {
        return Err(
            ForgeError::new(ErrorCode::ContainerStartFailed).with_context("reason", "runs_as_root")
        );
    }
    Ok(())
}

/// The runner's own files under the work directory: an empty git
/// configuration and an empty hooks directory.
fn runner_files(cfg: &RunnerConfig) -> Result<(PathBuf, PathBuf), ForgeError> {
    let own = cfg.work_dir.join("runner");
    let hooks = own.join("empty-hooks");
    let config = own.join("empty.gitconfig");
    let io = |e: std::io::Error| {
        ForgeError::new(ErrorCode::ContainerStartFailed)
            .with_context("reason", "workspace_unusable")
            .with_source(e)
    };
    std::fs::create_dir_all(&hooks).map_err(io)?;
    std::fs::write(&config, b"").map_err(io)?;
    Ok((config, hooks))
}

fn rootless(proc: &dyn ProcessRunner, podman: &PodmanPlan) -> Result<(), ForgeError> {
    let ran = proc
        .run(
            &podman.rootless(),
            Duration::from_secs(60),
            &|| false,
            &mut |_| {},
        )
        .map_err(|e| {
            ForgeError::new(ErrorCode::ContainerStartFailed)
                .with_context("reason", "spawn_failed")
                .with_source(e)
        })?;
    if ran.ok() && String::from_utf8_lossy(&ran.stdout).trim() == "true" {
        return Ok(());
    }
    Err(ForgeError::new(ErrorCode::ContainerStartFailed)
        .with_context("reason", "not_rootless")
        .with_context("podman_stderr", ran.stderr_tail))
}

fn start(cli: &Cli) -> Result<(), ForgeError> {
    not_root()?;
    let cfg = RunnerConfig::load(&cli.config)?;
    let token = cfg.token(&|k| std::env::var(k).ok())?;
    let (empty_config, empty_hooks) = runner_files(&cfg)?;
    let proc: Arc<dyn ProcessRunner> = Arc::new(SystemRunner);
    let podman = PodmanPlan::new(cfg.podman.clone(), cfg.limits.clone()); // clone: the plan owns its copy
    rootless(proc.as_ref(), &podman)?;
    let client_cfg = forge_client::config::Config {
        dir: cfg.work_dir.clone(), // clone: the client's config owns it
        host: cfg.server.clone(),  // clone: as above
        ca_file: cfg.ca_file.clone(),
        timeout: Duration::from_secs(45),
        connect_timeout: Duration::from_secs(10),
    };
    let client = forge_client::http::ApiClient::new(&client_cfg, Some(token)).map_err(|e| {
        ForgeError::new(ErrorCode::ConfigInvalid)
            .with_context("config_key", "ca_file")
            .with_context("reason", e.detail().unwrap_or("client_build").to_owned())
    })?;
    let fetch_cfg = cfg.clone(); // clone: the closure owns its copy
    let runner = Runner {
        api: Arc::new(HttpApi::new(client)),
        exec: Executor {
            proc,
            git: GitPlan::new(cfg.git.clone(), empty_config, empty_hooks),
            podman,
            workspace: Workspace::new(cfg.work_dir.clone()),
            git_limits: GitLimits::default(),
            fetch_url: Box::new(move |job| fetch_cfg.fetch_url(job)),
        },
        heartbeat_every: HEARTBEAT_EVERY,
    };
    if matches!(cli.command, Some(Command::Check)) {
        println!("ok: configuration, token, rootless podman");
        return Ok(());
    }
    // Anything a crashed earlier run left.
    runner.exec.workspace.wipe().map_err(|e| {
        ForgeError::new(ErrorCode::ContainerStartFailed)
            .with_context("reason", "workspace_dirty")
            .with_source(e)
    })?;
    tracing::info!(target: "forge_runner", event = "forge.runner.started", server = %cfg.server, "runner started");
    let shutdown = AtomicBool::new(false);
    runner.run_forever(&shutdown)
}

fn main() -> ExitCode {
    telemetry::init(LogFormat::Json, "info");
    let cli = Cli::parse();
    match start(&cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            telemetry::log_failure(&e, &RequestId::generate());
            eprintln!(
                "{}: {}",
                e.code().id(),
                e.user_message(forge_errors::Lang::Ja)
            );
            ExitCode::FAILURE
        }
    }
}
