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
//!    A job with `services` (plan 14 C5s) runs in a pod with no network
//!    (`pod create --network=none`); each service container is created
//!    and started detached in it before the job's container, which joins
//!    the pod. A service image outside `image_registries` is `FG-CI-0011`
//!    (with `service`), a pod or service Podman could not create or start
//!    is `FG-CI-0014`. The runner does not wait for readiness (the job's
//!    script does). When the job fails, the last 50 lines of each
//!    service's log are appended to the job log (`[service <name>]`,
//!    masked like the job's output);
//! 7. always: the container removed, the pod removed (`pod rm --force`),
//!    the job directory removed.
//!
//! Every failure is logged once here (`forge_core::telemetry::log_failure`
//! with `job_id`, `attempt`, `pipeline_id`) and its code is the job log's
//! last line. The job's token is masked in the log and dropped with the
//! claim: it is never used for another job.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use forge_client::forge_ci;
use forge_core::{telemetry, ObjectId, RequestId};
use forge_errors::{ErrorCode, ForgeError, Lang};
use sha2::{Digest, Sha256};

use crate::api::{ClaimedJob, JobResult, RunnerApi};
use crate::git::GitPlan;
use crate::logsink::LogSink;
use crate::podman::{
    container_name, service_container_name, valid_image, Container, PodmanPlan, ServiceContainer,
};
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

/// What a job attempt has put on the host so far (removed in
/// [`Executor::execute`] whatever happened).
#[derive(Default)]
struct Started {
    /// The job's container may exist.
    container: bool,
    /// The job's pod may exist (a job with services).
    pod: bool,
    /// `(service name, container name)` of each service container that may
    /// exist, in start order.
    services: Vec<(String, String)>,
    /// M4 C6: the work tree and the declared artifact paths of a script
    /// that exited 0 — uploaded only once the container (and its pod) is
    /// confirmed removed.
    upload: Option<(std::path::PathBuf, Vec<String>)>,
}

/// The longest all of one job's artifact transfers may take together (the
/// downloads before the script, the uploads after it, each).
pub const TRANSFER_DEADLINE: Duration = Duration::from_secs(1800);

/// The most bytes of one service's log tail kept for the job log (the
/// last ones).
const SERVICE_LOG_BYTES: usize = 64 * 1024;

/// What is left of the job's time (`None`: nothing).
fn left(deadline: Instant) -> Option<Duration> {
    Some(deadline.saturating_duration_since(Instant::now())).filter(|d| !d.is_zero())
}

fn timed_out(timeout_s: u32) -> ForgeError {
    ForgeError::new(ErrorCode::JobTimedOut).with_context("timeout_s", timeout_s)
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

fn service_failed(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::ServiceStartFailed).with_context("reason", reason)
}

/// What is left of a job's transfer time: `FG-CI-0020` when it ran out
/// (`reason=deadline`) or the job was stopped (`reason=stopped`: a cancel,
/// a lost lease, a shutdown — checked between files).
fn transfer_left(deadline: Instant, stop: &AtomicBool) -> Result<Duration, ForgeError> {
    if stop.load(Ordering::SeqCst) {
        return Err(
            ForgeError::new(ErrorCode::ArtifactTransferFailed).with_context("reason", "stopped")
        );
    }
    left(deadline).ok_or_else(|| {
        ForgeError::new(ErrorCode::ArtifactTransferFailed)
            .with_context("reason", "deadline")
            .with_context("timeout_ms", TRANSFER_DEADLINE.as_millis())
    })
}

/// `FG-CI-0020` for a failed transfer: the server's code when it answered
/// one.
fn transfer_failed(reason: &'static str, e: &forge_client::error::ClientError) -> ForgeError {
    ForgeError::new(ErrorCode::ArtifactTransferFailed)
        .with_context("reason", reason)
        .with_context("server_code", e.code_id().to_owned())
        .with_context("request_id", e.request_id().unwrap_or("").to_owned())
}

/// What the job's log is masked for: its token, and each masked secret in
/// its common encodings (M4 C6, `forge_core::ci_secret::mask_forms`).
fn log_masks(job: &ClaimedJob) -> Vec<String> {
    let mut masks = vec![job.job_token.expose().to_owned()];
    for s in job.secrets.iter().filter(|s| s.masked) {
        masks.extend(forge_core::ci_secret::mask_forms(s.value.expose()));
    }
    masks
}

/// The claim's secrets against the definition at the pinned commit (M4
/// C6): exactly the names the job asks for, each a valid name with a
/// one-line value (Podman's env file has no quoting). `FG-CI-0008
/// reason=secrets_mismatch` / `secret_invalid` otherwise (no value is
/// named).
fn check_secrets(job: &ClaimedJob, wanted: &[String]) -> Result<(), ForgeError> {
    let mut given: Vec<&str> = job.secrets.iter().map(|s| s.name.as_str()).collect();
    given.sort_unstable();
    let mut asked: Vec<&str> = wanted.iter().map(String::as_str).collect();
    asked.sort_unstable();
    // Every name the job asks for exactly once, and nothing else (the
    // server never hands out a job without what it asked for).
    if given != asked {
        return Err(mismatch("secrets_mismatch"));
    }
    for s in &job.secrets {
        if forge_core::ci_secret::check_name(&s.name).is_some()
            || forge_core::ci_secret::check_value(s.value.expose(), false).is_some()
        {
            return Err(mismatch("secret_invalid").with_context("name", s.name.clone()));
            // clone: the error names it
        }
    }
    Ok(())
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
            log_masks(job),
            Arc::clone(stop),
        );
        let name = container_name(job.job_id, job.attempt);
        let mut started = Started::default();
        let ran = self.steps(job, api, &name, &mut started, stop, &mut sink);
        // A failed job gets its services' log tails (not a stopped one: a
        // cancel or a shutdown has no failure to explain).
        let failed = matches!(ran, Ok(ScriptEnd::Exited(c)) if c != 0) || ran.is_err();
        if failed && !started.services.is_empty() {
            self.append_service_logs(&started.services, &mut sink);
        }
        let mut container_gone = true;
        if started.container {
            // `--rm` removes a container that ended; this removes one that
            // did not (killed, a lost `--rm`) — always, whatever happened.
            let removed = self.proc.run(
                &self.podman.remove(&name),
                PODMAN_CLEANUP,
                &|| false,
                &mut |_| {},
            );
            if !removed.as_ref().is_ok_and(Ran::ok) {
                container_gone = false;
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
        let pod_left = started.pod && !self.remove_pod(&request_id, job.job_id, &name);
        // M4 C6 review: a successful script's artifacts, only once nothing
        // of the job runs any more (no process can change a file while it
        // is read and sent).
        let ran = match (ran, started.upload.take()) {
            (Ok(ScriptEnd::Exited(0)), Some((work, paths))) => {
                if container_gone && !pod_left {
                    self.upload_artifacts(api, job, &work, &paths, stop, &mut sink)
                        .map(|()| ScriptEnd::Exited(0))
                } else {
                    Err(ForgeError::new(ErrorCode::ArtifactTransferFailed)
                        .with_context("reason", "container_not_removed"))
                }
            }
            (other, _) => other,
        };
        let workspace_dirty = !self.clean_workspace(&request_id, job.job_id) || pod_left;
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

    #[allow(clippy::too_many_arguments)] // the attempt's state, passed in
    fn steps(
        &self,
        job: &ClaimedJob,
        api: &dyn RunnerApi,
        name: &str,
        started: &mut Started,
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
        // The services' images: the same registries as the job's.
        for s in spec.services() {
            if !self.podman.image_allowed(s.image()) {
                return Err(ForgeError::new(ErrorCode::JobImageNotAllowed)
                    .with_context("image", s.image().chars().take(300).collect::<String>())
                    .with_context("service", s.name().to_owned()));
            }
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
        // M4 C6: the secrets — exactly the names the definition asks for,
        // each value one line — into an env file outside the mounts.
        check_secrets(job, spec.secrets())?;
        // M4 C6: the artifacts of the jobs it needs, into the work tree
        // (before the script; never over a file of the checkout).
        if !spec.needs().is_empty() {
            self.fetch_needed(api, job, &dirs.work, stop, sink)?;
        }
        let env_file = if job.secrets.is_empty() {
            None
        } else {
            let vars: Vec<(&str, &str)> = job
                .secrets
                .iter()
                .map(|s| (s.name.as_str(), s.value.expose()))
                .collect();
            Some(
                Workspace::write_env_file(&dirs, &vars)
                    .map_err(|e| start_failed("workspace_unusable").with_source(e))?,
            )
        };
        // One deadline for the whole container phase (review): the pulls
        // (part of each create), the services and the script share the
        // job's timeout.
        let deadline = Instant::now() + Duration::from_secs(u64::from(timeout_s));
        // 6a. The pod and its services.
        let pod = if spec.services().is_empty() {
            None
        } else {
            if let Some(end) =
                self.start_services(job, name, spec, (deadline, timeout_s), started, stop, sink)?
            {
                return Ok(end);
            }
            Some(name)
        };
        // 6b. The container.
        let create = self.podman.create(&Container {
            name,
            image: &job.image,
            work: &dirs.work,
            script_dir: &dirs.script,
            job_id: job.job_id,
            pipeline_id: job.pipeline_id,
            project_id: job.project_id,
            sha: sha.as_str(),
            pod,
            env_file: env_file.as_deref(),
        });
        sink.line(&format!("running {} in {name}", job.image));
        started.container = true;
        let limit = left(deadline).ok_or_else(|| timed_out(timeout_s))?;
        let created = self.run(&create, limit, stop, sink);
        // Podman has read the env file (or failed to): it goes now, not
        // with the job directory at the end.
        if let Some(file) = &env_file {
            if let Err(e) = std::fs::remove_file(file) {
                tracing::warn!(
                    target: "forge_runner",
                    event = "forge.runner.env_file_remove_failed",
                    error_code = ErrorCode::ContainerStartFailed.id(),
                    job_id = job.job_id,
                    error = %e,
                    "the job's env file stays until the job directory is removed"
                );
            }
        }
        let created = created.map_err(|e| start_failed("spawn_failed").with_source(e))?;
        match created.exit {
            Exit::Code(0) => {}
            Exit::Code(code) => {
                return Err(start_failed("podman_exit")
                    .with_context("step", "create")
                    .with_context("exit_code", code)
                    .with_context("podman_stderr", created.stderr_tail))
            }
            Exit::TimedOut => return Err(timed_out(timeout_s)),
            Exit::Stopped => return Ok(ScriptEnd::Stopped),
        }
        let limit = left(deadline).ok_or_else(|| timed_out(timeout_s))?;
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
                // M4 C6: a successful script's declared artifacts, sent
                // after the container is removed (`execute`).
                if code == 0 && !spec.artifacts().is_empty() {
                    started.upload = Some((dirs.work.clone(), spec.artifacts().to_vec()));
                    // clone: sent after the cleanup
                }
                Ok(ScriptEnd::Exited(code))
            }
            Exit::TimedOut => {
                self.kill(name);
                Err(timed_out(timeout_s))
            }
            Exit::Stopped => {
                self.kill(name);
                Ok(ScriptEnd::Stopped)
            }
        }
    }

    /// The artifacts of the jobs `job` needs, each into its path under
    /// `work` (`crate::artifacts::place`). `FG-CI-0020` when one cannot be
    /// listed, fetched or placed (the job fails: it never runs without
    /// what it needs).
    fn fetch_needed(
        &self,
        api: &dyn RunnerApi,
        job: &ClaimedJob,
        work: &std::path::Path,
        stop: &AtomicBool,
        sink: &mut LogSink<'_>,
    ) -> Result<(), ForgeError> {
        let deadline = Instant::now() + TRANSFER_DEADLINE;
        let needed = api
            .needed_artifacts(job.job_id, &job.job_token, TRANSFER_DEADLINE)
            .map_err(|e| transfer_failed("receive", &e))?;
        // The server refuses more (FG-CI-0019); never a short list here.
        if i64::try_from(needed.len()).unwrap_or(i64::MAX) > forge_core::ci_artifact::MAX_JOB_FILES
        {
            return Err(ForgeError::new(ErrorCode::ArtifactTooLarge)
                .with_context("reason", "too_many_needed_files"));
        }
        for a in &needed {
            let limit = transfer_left(deadline, stop)?;
            let target = crate::artifacts::place(work, &a.path)?;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&target)
                .map_err(|e| {
                    ForgeError::new(ErrorCode::ArtifactTransferFailed)
                        .with_context("reason", "place")
                        .with_source(e)
                })?;
            let max = u64::try_from(a.size_bytes).unwrap_or(0);
            let got = api
                .download_artifact(job.job_id, &job.job_token, a.id, &mut file, max, limit)
                .map_err(|e| transfer_failed("receive", &e).with_context("artifact_id", a.id))?;
            if got != max {
                return Err(ForgeError::new(ErrorCode::ArtifactTransferFailed)
                    .with_context("reason", "size_mismatch")
                    .with_context("artifact_id", a.id));
            }
        }
        if !needed.is_empty() {
            sink.line(&format!(
                "fetched {} artifact(s) of the jobs this job needs",
                needed.len()
            ));
        }
        Ok(())
    }

    /// Upload the regular files under the declared `paths` of `work`
    /// (`crate::artifacts::collect`): links and missing paths are named in
    /// the log, never sent. `FG-CI-0019` past a bound (nothing sent),
    /// `FG-CI-0020` when one upload fails (the job fails).
    fn upload_artifacts(
        &self,
        api: &dyn RunnerApi,
        job: &ClaimedJob,
        work: &std::path::Path,
        paths: &[String],
        stop: &AtomicBool,
        sink: &mut LogSink<'_>,
    ) -> Result<(), ForgeError> {
        let deadline = Instant::now() + TRANSFER_DEADLINE;
        let found = crate::artifacts::collect(work, paths)?;
        for missing in &found.missing {
            sink.line(&format!("artifact path not found: {missing}"));
        }
        for skipped in &found.skipped {
            sink.line(&format!(
                "artifact path skipped (a link, outside the work tree, or not a file): {skipped}"
            ));
        }
        for f in &found.files {
            let limit = transfer_left(deadline, stop)?;
            // Opened without following a link (and checked to be the file
            // `collect` saw), so a file swapped for a link is never read.
            let opened = crate::artifacts::open_regular(&f.file, f.size)?;
            api.upload_artifact(job.job_id, &job.job_token, &f.path, opened, f.size, limit)
                .map_err(|e| {
                    if e.code_id() == ErrorCode::ArtifactTooLarge.id() {
                        ForgeError::new(ErrorCode::ArtifactTooLarge)
                            .with_context("reason", "server_limit")
                            .with_context("path", f.path.clone()) // clone: the error names it
                    } else {
                        transfer_failed("send", &e).with_context("path", f.path.clone())
                        // clone: as above
                    }
                })?;
        }
        sink.line(&format!("uploaded {} artifact file(s)", found.files.len()));
        Ok(())
    }

    /// The job's pod (`--network=none`) and each service in it, created and
    /// started detached. `Some`: the job was stopped meanwhile. Podman's
    /// failure is `FG-CI-0014`; every call gets what is left before the
    /// job's deadline (`FG-CI-0010` when nothing is).
    #[allow(clippy::too_many_arguments)] // one step of `steps`, its state passed in
    fn start_services(
        &self,
        job: &ClaimedJob,
        pod: &str,
        spec: &forge_ci::Job,
        (deadline, timeout_s): (Instant, u32),
        started: &mut Started,
        stop: &AtomicBool,
        sink: &mut LogSink<'_>,
    ) -> Result<Option<ScriptEnd>, ForgeError> {
        let limit = || left(deadline).ok_or_else(|| timed_out(timeout_s));
        sink.line(&format!("creating pod {pod} (no network)"));
        started.pod = true;
        let ran = self
            .run(&self.podman.pod_create(pod), limit()?, stop, sink)
            .map_err(|e| service_failed("spawn_failed").with_source(e))?;
        match ran.exit {
            Exit::Code(0) => {}
            Exit::Code(code) => {
                return Err(service_failed("pod_create")
                    .with_context("exit_code", code)
                    .with_context("podman_stderr", ran.stderr_tail))
            }
            Exit::TimedOut => return Err(timed_out(timeout_s)),
            Exit::Stopped => return Ok(Some(ScriptEnd::Stopped)),
        }
        for s in spec.services() {
            let container = service_container_name(job.job_id, job.attempt, s.name());
            sink.line(&format!("starting service {} ({})", s.name(), s.image()));
            let failed = |reason: &'static str, step: &'static str| {
                service_failed(reason)
                    .with_context("service", s.name().to_owned())
                    .with_context("step", step)
            };
            let create = self.podman.service_create(&ServiceContainer {
                pod,
                name: &container,
                image: s.image(),
                env: s.env(),
                user: s.user(),
                tmpfs: s.tmpfs(),
            });
            for (step, cmd) in [
                ("create", create),
                ("start", self.podman.service_start(&container)),
            ] {
                let ran = self
                    .run(&cmd, limit()?, stop, sink)
                    .map_err(|e| failed("spawn_failed", step).with_source(e))?;
                match ran.exit {
                    Exit::Code(0) => {}
                    Exit::Code(code) => {
                        return Err(failed("service_start", step)
                            .with_context("exit_code", code)
                            .with_context("podman_stderr", ran.stderr_tail))
                    }
                    Exit::TimedOut => return Err(timed_out(timeout_s)),
                    Exit::Stopped => return Ok(Some(ScriptEnd::Stopped)),
                }
                if step == "create" {
                    // Created: it has a log from here on (a service that was
                    // never created has none to read).
                    // clone: kept for the log tail
                    started
                        .services
                        .push((s.name().to_owned(), container.clone()));
                }
            }
        }
        Ok(None)
    }

    /// The last lines of each service's log, into the job's log (the sink
    /// masks them like the job's own output). A log that cannot be read is
    /// one line saying so (and a warn): the job's result does not depend
    /// on it.
    fn append_service_logs(&self, services: &[(String, String)], sink: &mut LogSink<'_>) {
        for (service, container) in services {
            let mut tail: Vec<u8> = Vec::new();
            let ran = self.proc.run(
                &self.podman.logs_tail(container),
                PODMAN_CLEANUP,
                &|| false,
                &mut |b| {
                    tail.extend_from_slice(b);
                    if tail.len() > SERVICE_LOG_BYTES {
                        tail.drain(..tail.len() - SERVICE_LOG_BYTES);
                    }
                },
            );
            match &ran {
                Ok(r) if r.ok() => {}
                Ok(r) if r.stderr_tail.contains("no such container") => {
                    // Gone already (it exited and was cleaned up, or never
                    // ran): nothing to copy, nothing wrong.
                    tracing::debug!(
                        target: "forge_runner",
                        event = "forge.runner.service_log_missing",
                        service = %service,
                        container = %container,
                        "a service's container has no log to copy"
                    );
                    continue;
                }
                Ok(_) | Err(_) => {
                    // The job's result does not depend on it: no error code.
                    tracing::warn!(
                        target: "forge_runner",
                        event = "forge.runner.service_log_unread",
                        service = %service,
                        container = %container,
                        "a service's log could not be read for the job log"
                    );
                    sink.line(&format!("the log of service {service} could not be read"));
                    continue;
                }
            }
            sink.line(&format!("the last lines of service {service}:"));
            for line in String::from_utf8_lossy(&tail).lines() {
                sink.push(format!("[service {service}] {line}\n").as_bytes());
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
    /// container and job pod a crashed or killed earlier run left
    /// (`forge-job-<job>-<attempt>`; other names are left alone; a pod
    /// takes its service containers with it). Returns how many containers
    /// and pods were removed; a Podman failure is `FG-CI-0007
    /// reason=stale_containers` (the runner does not start).
    pub fn remove_stale_containers(&self) -> Result<usize, ForgeError> {
        self.remove_stale_job_containers()
    }

    /// The isolation probe ([`crate::isolation::check`], FG-CI-0015) when
    /// the plan has a probe directory ([`PodmanPlan::with_probe_rootfs`]);
    /// nothing otherwise (unit tests of the job).
    pub fn check_isolation(&self) -> Result<(), ForgeError> {
        match self.podman.probe_rootfs() {
            Some(rootfs) => crate::isolation::check(
                self.proc.as_ref(),
                &self.podman,
                &crate::isolation::probe_name(std::process::id()),
                rootfs,
            ),
            None => Ok(()),
        }
    }

    fn remove_stale_job_containers(&self) -> Result<usize, ForgeError> {
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
        // The pods (a job with services): `pod rm --force` takes their
        // service containers with them.
        let pods = self
            .proc
            .run(
                &self.podman.stale_pods(),
                PODMAN_CLEANUP,
                &|| false,
                &mut |_| {},
            )
            .map_err(|e| failed(None).with_source(e))?;
        if !pods.ok() {
            return Err(failed(Some(pods)).with_context("step", "pod_ps"));
        }
        for pod in String::from_utf8_lossy(&pods.stdout).lines() {
            let pod = pod.trim();
            if !is_job_container(pod) {
                continue;
            }
            let ran = self
                .proc
                .run(
                    &self.podman.pod_remove(pod),
                    PODMAN_CLEANUP,
                    &|| false,
                    &mut |_| {},
                )
                .map_err(|e| failed(None).with_source(e))?;
            if !ran.ok() {
                return Err(failed(Some(ran)).with_context("pod", pod.to_owned()));
            }
            tracing::warn!(
                target: "forge_runner",
                event = "forge.runner.stale_pod_removed",
                pod = %pod,
                "removed a job pod an earlier run left"
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

    /// `podman pod rm --force` of the job's pod, retried once. `false`: it
    /// may still exist — logged as `FG-CI-0007 reason=pod_remove`, and the
    /// runner takes no further job (like a dirty work directory).
    fn remove_pod(&self, request_id: &RequestId, job: i64, pod: &str) -> bool {
        let mut last = None;
        for _ in 0..2 {
            match self.proc.run(
                &self.podman.pod_remove(pod),
                PODMAN_CLEANUP,
                &|| false,
                &mut |_| {},
            ) {
                Ok(ran) if ran.ok() => return true,
                other => last = Some(other),
            }
        }
        let mut e = start_failed("pod_remove")
            .with_context("job_id", job)
            .with_context("pod", pod.to_owned());
        match last {
            Some(Ok(ran)) => e = e.with_context("podman_stderr", ran.stderr_tail),
            Some(Err(io)) => e = e.with_source(io),
            None => {}
        }
        telemetry::log_failure(&e, request_id);
        false
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
