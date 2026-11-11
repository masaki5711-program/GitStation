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
use forge_runner::isolation;
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
    /// Check the host: configuration, token, git, rootless Podman, job
    /// isolation (FG-CI-0015).
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
/// configuration, an empty hooks directory and the isolation probe's empty
/// root directory.
fn runner_files(cfg: &RunnerConfig) -> Result<(PathBuf, PathBuf, PathBuf), ForgeError> {
    let own = cfg.work_dir.join("runner");
    let hooks = own.join("empty-hooks");
    let config = own.join("empty.gitconfig");
    let rootfs = own.join("empty-rootfs");
    let io = |e: std::io::Error| {
        ForgeError::new(ErrorCode::ContainerStartFailed)
            .with_context("reason", "workspace_unusable")
            .with_source(e)
    };
    std::fs::create_dir_all(&hooks).map_err(io)?;
    std::fs::create_dir_all(&rootfs).map_err(io)?;
    std::fs::write(&config, b"").map_err(io)?;
    Ok((config, hooks, rootfs))
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
    let (empty_config, empty_hooks, empty_rootfs) = runner_files(&cfg)?;
    let proc: Arc<dyn ProcessRunner> = Arc::new(SystemRunner);
    // Podman reads only the runner's own containers.conf (ADR-0029
    // amendment 2026-11-12, FG-CI-0015).
    let containers_conf = isolation::write_containers_conf(
        &cfg.work_dir.join("runner").join("podman"),
    )
    .map_err(|e| {
        ForgeError::new(ErrorCode::JobIsolationUnsafe)
            .with_context("reason", "conf_unwritable")
            .with_source(e)
    })?;
    let podman = PodmanPlan::new(cfg.podman.clone(), cfg.limits.clone()) // clone: the plan owns its copy
        .with_registries(cfg.image_registries.clone()) // clone: as above
        .with_containers_conf(containers_conf)
        .with_probe_rootfs(empty_rootfs);
    rootless(proc.as_ref(), &podman)?;
    // Before any job (and again before every claim): a job's container
    // gets nothing its flags did not ask for.
    isolation::remove_stale(proc.as_ref(), &podman)?;
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
            git: GitPlan::new(cfg.git.clone(), empty_config, empty_hooks)
                .with_ca_file(cfg.ca_file.clone()), // clone: the plan owns its copy
            podman,
            workspace: Workspace::new(cfg.work_dir.clone()),
            git_limits: GitLimits::default(),
            work_dir_max_bytes: cfg.work_dir_max_bytes,
            fetch_url: Box::new(move |job| fetch_cfg.fetch_url(job)),
        },
        heartbeat_every: HEARTBEAT_EVERY,
        shutdown: Arc::new(AtomicBool::new(false)),
    };
    runner.exec.check_isolation()?;
    if matches!(cli.command, Some(Command::Check)) {
        println!("ok: configuration, token, rootless podman, job isolation");
        return Ok(());
    }
    // Anything a crashed earlier run left: its containers first (they
    // hold the work directory's mount), then the directory.
    let stale = runner.exec.remove_stale_containers()?;
    runner.exec.workspace.wipe().map_err(|e| {
        ForgeError::new(ErrorCode::ContainerStartFailed)
            .with_context("reason", "workspace_dirty")
            .with_source(e)
    })?;
    tracing::info!(target: "forge_runner", event = "forge.runner.started", server = %cfg.server, stale_containers_removed = stale, "runner started");
    on_shutdown_signal(Arc::clone(&runner.shutdown))?;
    let ended = runner.run_forever();
    tracing::info!(target: "forge_runner", event = "forge.runner.stopped", "runner stopped");
    ended
}

/// Raise `flag` on SIGTERM (systemd's stop) or Ctrl-C / SIGINT, on a thread
/// of its own (a small Tokio runtime only for the signals): the loop stops
/// the running job, removes its container and exits.
fn on_shutdown_signal(flag: Arc<AtomicBool>) -> Result<(), ForgeError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()
        .map_err(|e| {
            ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "signal_runtime")
                .with_source(e)
        })?;
    #[cfg(unix)]
    let mut term = {
        let _entered = runtime.enter();
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).map_err(|e| {
            ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "signal_handler")
                .with_source(e)
        })?
    };
    std::thread::spawn(move || {
        runtime.block_on(async {
            #[cfg(unix)]
            let signal = tokio::select! {
                r = tokio::signal::ctrl_c() => r.map(|()| "interrupt"),
                _ = term.recv() => Ok("terminate"),
            };
            #[cfg(not(unix))]
            let signal = tokio::signal::ctrl_c().await.map(|()| "interrupt");
            match signal {
                Ok(signal) => tracing::info!(
                    target: "forge_runner",
                    event = "forge.runner.shutdown_requested",
                    signal,
                    "stopping: the running job is stopped and its container removed"
                ),
                Err(e) => tracing::error!(
                    target: "forge_runner",
                    event = "forge.runner.signal_failed",
                    error_code = ErrorCode::Internal.id(),
                    error = %e,
                    "the signal handler failed; stopping"
                ),
            }
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
        });
    });
    Ok(())
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
