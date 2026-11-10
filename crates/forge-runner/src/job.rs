//! One job attempt on the runner host (M4 C4, plan 14 §3.1, ADR-0029):
//!
//! 1. the claim is checked (the SHA is 40 / 64 lower-case hex, a timeout,
//!    a definition hash, an image from an allowed registry: `FG-CI-0011`);
//! 2. the job directory is wiped and created empty ([`Workspace`]);
//! 3. git: `init`, the fetch of `refs/pipelines/<id>/head` with the job's
//!    token (its own route; the token only in git's environment), the
//!    check that the fetched commit is the pinned SHA;
//! 4. `.forge-ci.yml` at the SHA: its sha256 must be the claim's
//!    `definition_hash`, it must parse, and it must still hold the job
//!    with the claimed image and timeout (`FG-CI-0008` otherwise — no
//!    container is started);
//! 5. checkout, the script written to a read-only mount;
//! 6. Podman: one container (`create`, then `start --attach`), no
//!    network, read-only root, killed and removed at the timeout
//!    (`FG-CI-0010`) or on stop; Podman's own failure is `FG-CI-0007` (a
//!    failed create, or a 125 from a container that never started — a
//!    script's own exit 125 is a failed job);
//!    The work directory past `work_dir_max_bytes` when the container
//!    ended fails the job (`FG-CI-0012`);
//! 7. always: the container removed, the job directory removed.
//!
//! Every failure is logged once here (`forge_core::telemetry::log_failure`
//! with `job_id`, `attempt`, `pipeline_id`) and its code is the job log's
//! last line. The job's token is masked in the log and dropped with the
//! claim: it is never used for another job.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use forge_client::forge_ci;
use forge_core::{telemetry, ObjectId, RequestId};
use forge_errors::{ErrorCode, ForgeError, Lang};
use sha2::{Digest, Sha256};

use crate::api::{ClaimedJob, JobResult, RunnerApi};
use crate::git::GitPlan;
use crate::logsink::LogSink;
use crate::podman::{container_name, valid_image, Container, PodmanPlan};
use crate::process::{Cmd, Exit, ProcessRunner, Ran};
use crate::workspace::{JobDirs, Workspace};

/// Bounds of the git steps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GitLimits {
    /// `init`, `rev-parse`, `cat-file`, `checkout`.
    pub local: Duration,
    /// The fetch.
    pub fetch: Duration,
}

impl Default for GitLimits {
    fn default() -> Self {
        Self {
            local: Duration::from_secs(60),
            fetch: Duration::from_secs(600),
        }
    }
}

/// How long `podman kill` / `rm` / `unshare` may take.
const PODMAN_CLEANUP: Duration = Duration::from_secs(60);

/// What the runner reports for a job.
#[derive(Debug)]
pub struct Outcome {
    pub result: JobResult,
    pub exit_code: Option<i32>,
    /// The registered failure, when the job did not get to run its script
    /// to an exit status (logged already).
    pub error: Option<ForgeError>,
    /// The job directory could not be removed: the runner must take no
    /// further job.
    pub workspace_dirty: bool,
}

/// Runs job attempts.
pub struct Executor {
    pub proc: Arc<dyn ProcessRunner>,
    pub git: GitPlan,
    pub podman: PodmanPlan,
    pub workspace: Workspace,
    pub git_limits: GitLimits,
    /// The most bytes the job's work directory may hold when its container
    /// ends (`None`: unchecked; FG-CI-0012 past it).
    pub work_dir_max_bytes: Option<u64>,
    /// `job_id` → the job's fetch URL (the runner's configured server).
    pub fetch_url: Box<dyn Fn(i64) -> Result<String, ForgeError> + Send + Sync>,
}

/// How a script run ended.
enum ScriptEnd {
    Exited(i32),
    Stopped,
}

fn fetch_failed(step: &'static str, reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::JobFetchFailed)
        .with_context("step", step)
        .with_context("reason", reason)
}

fn mismatch(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::JobDefinitionMismatch).with_context("reason", reason)
}

fn start_failed(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::ContainerStartFailed).with_context("reason", reason)
}

/// `forge-job-<job>-<attempt>` (decimal numbers), as
/// [`container_name`] builds it.
fn is_job_container(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("forge-job-") else {
        return false;
    };
    let Some((job, attempt)) = rest.split_once('-') else {
        return false;
    };
    let number = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    number(job) && number(attempt)
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

impl Executor {
    fn run(
        &self,
        cmd: &Cmd,
        limit: Duration,
        stop: &AtomicBool,
        sink: &mut LogSink<'_>,
    ) -> std::io::Result<Ran> {
        self.proc
            .run(cmd, limit, &|| stop.load(Ordering::SeqCst), &mut |b| {
                sink.push(b)
            })
    }

    /// One git step: exit 0 or `FG-CI-0009`.
    fn git_step(
        &self,
        step: &'static str,
        cmd: &Cmd,
        limit: Duration,
        stop: &AtomicBool,
        sink: &mut LogSink<'_>,
    ) -> Result<Ran, ForgeError> {
        let ran = self
            .run(cmd, limit, stop, sink)
            .map_err(|e| fetch_failed(step, "spawn_failed").with_source(e))?;
        match ran.exit {
            Exit::Code(0) => Ok(ran),
            Exit::Code(code) => Err(fetch_failed(step, "exit")
                .with_context("exit_code", code)
                .with_context("git_stderr", ran.stderr_tail)),
            Exit::TimedOut => {
                Err(fetch_failed(step, "timeout").with_context("timeout_ms", limit.as_millis()))
            }
            Exit::Stopped => Err(fetch_failed(step, "stopped")),
        }
    }

    /// Run one claimed job (module docs). Never panics; every failure is
    /// in the outcome and logged once.
    pub fn execute(
        &self,
        job: &ClaimedJob,
        api: &dyn RunnerApi,
        stop: &Arc<AtomicBool>,
    ) -> Outcome {
        let request_id = RequestId::generate();
        let mut sink = LogSink::new(
            api,
            job.job_id,
            job.attempt,
            vec![job.job_token.expose().to_owned()],
            Arc::clone(stop),
        );
        let name = container_name(job.job_id, job.attempt);
        let mut started = false;
        let ran = self.steps(job, &name, &mut started, stop, &mut sink);
        if started {
            // `--rm` removes a container that ended; this removes one that
            // did not (killed, a lost `--rm`) — always, whatever happened.
            let removed = self.proc.run(
                &self.podman.remove(&name),
                PODMAN_CLEANUP,
                &|| false,
                &mut |_| {},
            );
            if !removed.as_ref().is_ok_and(Ran::ok) {
                tracing::warn!(
                    target: "forge_runner",
                    event = "forge.runner.container_remove_failed",
                    error_code = ErrorCode::ContainerStartFailed.id(),
                    request_id = %request_id,
                    job_id = job.job_id,
                    container = %name,
                    "the job's container could not be confirmed removed"
                );
            }
        }
        let workspace_dirty = !self.clean_workspace(&request_id, job.job_id);
        let outcome = match ran {
            Ok(ScriptEnd::Exited(0)) => Outcome {
                result: JobResult::Succeeded,
                exit_code: Some(0),
                error: None,
                workspace_dirty,
            },
            Ok(ScriptEnd::Exited(code)) => {
                sink.line(&format!("the job's script exited with {code}"));
                Outcome {
                    result: JobResult::Failed,
                    exit_code: Some(code),
                    error: None,
                    workspace_dirty,
                }
            }
            Ok(ScriptEnd::Stopped) => {
                sink.line("the job was stopped (canceled, or its lease was lost)");
                Outcome {
                    result: JobResult::Failed,
                    exit_code: None,
                    error: None,
                    workspace_dirty,
                }
            }
            Err(e) => {
                let e = e
                    .with_context("job_id", job.job_id)
                    .with_context("attempt", job.attempt)
                    .with_context("pipeline_id", job.pipeline_id)
                    .with_context("project_id", job.project_id);
                telemetry::log_failure(&e, &request_id);
                sink.line(&format!(
                    "{}: {} (request_id: {request_id})",
                    e.code().id(),
                    e.user_message(Lang::Ja)
                ));
                Outcome {
                    result: JobResult::Failed,
                    exit_code: None,
                    error: Some(e),
                    workspace_dirty,
                }
            }
        };
        sink.flush();
        outcome
    }

    fn steps(
        &self,
        job: &ClaimedJob,
        name: &str,
        started: &mut bool,
        stop: &AtomicBool,
        sink: &mut LogSink<'_>,
    ) -> Result<ScriptEnd, ForgeError> {
        // 1. The claim.
        let sha = ObjectId::parse(&job.sha).map_err(|_| mismatch("claim_invalid"))?;
        let expected = job
            .definition_hash
            .as_deref()
            .ok_or_else(|| mismatch("no_hash"))?;
        let timeout_s = u32::try_from(job.timeout_s)
            .ok()
            .filter(|t| *t > 0)
            .ok_or_else(|| mismatch("claim_invalid"))?;
        if !self.podman.image_allowed(&job.image) {
            return Err(ForgeError::new(ErrorCode::JobImageNotAllowed)
                .with_context("image", job.image.chars().take(300).collect::<String>()));
        }
        // 2. A fresh job directory.
        self.workspace
            .wipe()
            .map_err(|e| start_failed("workspace_unusable").with_source(e))?;
        let dirs = self
            .workspace
            .create()
            .map_err(|e| start_failed("workspace_unusable").with_source(e))?;
        // 3. git.
        let limits = self.git_limits;
        sink.line(&format!("fetching {} (pinned)", sha.as_str()));
        self.git_step("init", &self.git.init(&dirs.work), limits.local, stop, sink)?;
        let url = (self.fetch_url)(job.job_id)?;
        self.git_step(
            "fetch",
            &self
                .git
                .fetch(&dirs.work, &url, job.pipeline_id, &job.job_token),
            limits.fetch,
            stop,
            sink,
        )?;
        let fetched = self.git_step(
            "verify",
            &self.git.fetched(&dirs.work),
            limits.local,
            stop,
            sink,
        )?;
        if String::from_utf8_lossy(&fetched.stdout).trim() != sha.as_str() {
            return Err(fetch_failed("verify", "sha_mismatch"));
        }
        // 4. The definition at the pinned commit.
        let bytes = self.definition(&dirs, &sha, stop, sink)?;
        let actual = sha256_hex(&bytes);
        if actual != expected {
            return Err(mismatch("hash_mismatch")
                .with_context("expected", expected.to_owned())
                .with_context("actual", actual));
        }
        let def = forge_ci::check(&bytes, &forge_ci::LintContext::pinned_by_server()).map_err(
            |problems| mismatch("definition_invalid").with_context("problems", problems.len()),
        )?;
        let spec = def
            .jobs()
            .iter()
            .find(|j| j.name() == job.name)
            .ok_or_else(|| mismatch("job_missing"))?;
        if spec.image() != job.image || !valid_image(&job.image) {
            return Err(mismatch("image_mismatch"));
        }
        if spec.timeout_secs() != timeout_s {
            return Err(mismatch("timeout_mismatch"));
        }
        // 5. Checkout and the script.
        self.git_step(
            "checkout",
            &self.git.checkout(&dirs.work, &sha),
            limits.local,
            stop,
            sink,
        )?;
        Workspace::write_script(&dirs, spec.script())
            .map_err(|e| start_failed("workspace_unusable").with_source(e))?;
        // 6. The container.
        let create = self.podman.create(&Container {
            name,
            image: &job.image,
            work: &dirs.work,
            script_dir: &dirs.script,
            job_id: job.job_id,
            pipeline_id: job.pipeline_id,
            project_id: job.project_id,
            sha: sha.as_str(),
        });
        sink.line(&format!("running {} in {name}", job.image));
        *started = true;
        // The pull is part of the create: bounded by the job's timeout.
        let limit = Duration::from_secs(u64::from(timeout_s));
        let created = self
            .run(&create, limit, stop, sink)
            .map_err(|e| start_failed("spawn_failed").with_source(e))?;
        match created.exit {
            Exit::Code(0) => {}
            Exit::Code(code) => {
                return Err(start_failed("podman_exit")
                    .with_context("step", "create")
                    .with_context("exit_code", code)
                    .with_context("podman_stderr", created.stderr_tail))
            }
            Exit::TimedOut => {
                return Err(
                    ForgeError::new(ErrorCode::JobTimedOut).with_context("timeout_s", timeout_s)
                )
            }
            Exit::Stopped => return Ok(ScriptEnd::Stopped),
        }
        let ran = self
            .run(&self.podman.start(name), limit, stop, sink)
            .map_err(|e| start_failed("spawn_failed").with_source(e))?;
        match ran.exit {
            Exit::Code(125) if !self.script_exited(name) => Err(start_failed("podman_exit")
                .with_context("step", "start")
                .with_context("exit_code", 125)
                .with_context("podman_stderr", ran.stderr_tail)),
            Exit::Code(code) => {
                self.check_work_size()?;
                Ok(ScriptEnd::Exited(code))
            }
            Exit::TimedOut => {
                self.kill(name);
                Err(ForgeError::new(ErrorCode::JobTimedOut).with_context("timeout_s", timeout_s))
            }
            Exit::Stopped => {
                self.kill(name);
                Ok(ScriptEnd::Stopped)
            }
        }
    }

    /// `.forge-ci.yml` at the pinned commit (at most forge-ci's limit).
    fn definition(
        &self,
        dirs: &JobDirs,
        sha: &ObjectId,
        stop: &AtomicBool,
        sink: &mut LogSink<'_>,
    ) -> Result<Vec<u8>, ForgeError> {
        let cmd = self.git.definition(&dirs.work, sha);
        let ran = self
            .run(&cmd, self.git_limits.local, stop, sink)
            .map_err(|e| fetch_failed("read_definition", "spawn_failed").with_source(e))?;
        match ran.exit {
            Exit::Code(0) if ran.stdout.len() <= forge_ci::MAX_BYTES => Ok(ran.stdout),
            Exit::Code(0) => {
                Err(mismatch("definition_invalid").with_context("problem", "too_large"))
            }
            Exit::Code(_) => Err(mismatch("definition_missing")),
            Exit::TimedOut => Err(fetch_failed("read_definition", "timeout")),
            Exit::Stopped => Err(fetch_failed("read_definition", "stopped")),
        }
    }

    /// At startup (before the work directory is wiped): remove every job
    /// container a crashed or killed earlier run left
    /// (`forge-job-<job>-<attempt>`; other names are left alone). Returns
    /// how many were removed; a Podman failure is `FG-CI-0007
    /// reason=stale_containers` (the runner does not start).
    pub fn remove_stale_containers(&self) -> Result<usize, ForgeError> {
        let failed = |ran: Option<Ran>| {
            let e = start_failed("stale_containers");
            match ran {
                Some(r) => e.with_context("podman_stderr", r.stderr_tail),
                None => e,
            }
        };
        let listed = self
            .proc
            .run(&self.podman.stale(), PODMAN_CLEANUP, &|| false, &mut |_| {})
            .map_err(|e| failed(None).with_source(e))?;
        if !listed.ok() {
            return Err(failed(Some(listed)));
        }
        let mut removed = 0;
        for name in String::from_utf8_lossy(&listed.stdout).lines() {
            let name = name.trim();
            if !is_job_container(name) {
                continue;
            }
            let ran = self
                .proc
                .run(
                    &self.podman.remove(name),
                    PODMAN_CLEANUP,
                    &|| false,
                    &mut |_| {},
                )
                .map_err(|e| failed(None).with_source(e))?;
            if !ran.ok() {
                return Err(failed(Some(ran)).with_context("container", name.to_owned()));
            }
            tracing::warn!(
                target: "forge_runner",
                event = "forge.runner.stale_container_removed",
                container = %name,
                "removed a job container an earlier run left"
            );
            removed += 1;
        }
        Ok(removed)
    }

    /// After `podman start` exited 125: whether the container ran the
    /// script and exited (then 125 is the script's status). Anything else
    /// — still `created`, an inspect that fails — is Podman's failure.
    fn script_exited(&self, name: &str) -> bool {
        let state = self.proc.run(
            &self.podman.state(name),
            PODMAN_CLEANUP,
            &|| false,
            &mut |_| {},
        );
        match state {
            Ok(ran) if ran.ok() => String::from_utf8_lossy(&ran.stdout).trim() == "exited 125",
            _ => false,
        }
    }

    /// `work_dir_max_bytes` after the container ended: FG-CI-0012 past
    /// it (the directory is removed in [`Executor::execute`] as always). A
    /// directory that cannot be measured is logged and does not fail the
    /// job (the filesystem quota is the hard bound).
    fn check_work_size(&self) -> Result<(), ForgeError> {
        let Some(limit) = self.work_dir_max_bytes else {
            return Ok(());
        };
        match self.workspace.work_size() {
            Ok(bytes) if bytes > limit => Err(ForgeError::new(ErrorCode::JobWorkspaceTooLarge)
                .with_context("bytes", bytes)
                .with_context("limit_bytes", limit)),
            Ok(_) => Ok(()),
            Err(e) => {
                tracing::warn!(
                    target: "forge_runner",
                    event = "forge.runner.workspace_unmeasured",
                    error_code = ErrorCode::JobWorkspaceTooLarge.id(),
                    error = %e,
                    "the work directory could not be measured; the quota bounds it"
                );
                Ok(())
            }
        }
    }

    /// `podman kill` (the removal follows in [`Executor::execute`]).
    fn kill(&self, name: &str) {
        let killed = self.proc.run(
            &self.podman.kill(name),
            PODMAN_CLEANUP,
            &|| false,
            &mut |_| {},
        );
        if let Err(e) = killed {
            tracing::warn!(
                target: "forge_runner",
                event = "forge.runner.container_kill_failed",
                container = %name,
                error = %e,
                "podman kill could not be started; the removal follows"
            );
        }
    }

    /// Remove the job directory; through `podman unshare` when the runner's
    /// own user cannot. `false`: something is left.
    fn clean_workspace(&self, request_id: &RequestId, job: i64) -> bool {
        if self.workspace.wipe().is_ok() && self.workspace.is_clean() {
            return true;
        }
        let base = self.workspace.dirs().base;
        let _ = self.proc.run(
            &self.podman.unshare_remove(&base),
            PODMAN_CLEANUP,
            &|| false,
            &mut |_| {},
        ); // its result is the check below
        if self.workspace.is_clean() {
            return true;
        }
        telemetry::log_failure(
            &start_failed("workspace_dirty")
                .with_context("job_id", job)
                .with_context("path", base.display()),
            request_id,
        );
        false
    }
}
