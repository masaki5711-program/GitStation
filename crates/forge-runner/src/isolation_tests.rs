//! The isolation probe (ADR-0029 amendment 2026-11-12): the runner's own
//! containers.conf, the reading of a probe's `podman inspect`, the probe's
//! calls with a fake Podman, and — `#[ignore]`, on a rootless Podman host —
//! the same against the real one. Synthetic data only.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // tests may fail loudly

use std::path::PathBuf;
use std::sync::Mutex;

use serde_json::json;

use super::*;
use crate::podman::Limits;
use crate::process::Cmd;

/// What the probe looks like on Podman 5.3 with the runner's
/// containers.conf (trimmed to the fields read).
fn clean() -> Value {
    json!([{
        "Mounts": [],
        "Config": {
            "Env": ["TERM=xterm", "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"],
            "Annotations": null
        },
        "HostConfig": {
            "Binds": [],
            "Annotations": null,
            "CapAdd": [],
            "CgroupMode": "private",
            "Devices": [],
            "IpcMode": "private",
            "PidMode": "private",
            "Privileged": false,
            "SecurityOpt": ["no-new-privileges"],
            "ShmSize": 67108864,
            "UTSMode": "private",
            "UsernsMode": ""
        }
    }])
}

fn keys(v: &Value) -> Vec<&'static str> {
    deviations(v.to_string().as_bytes())
        .expect("parsed")
        .into_iter()
        .map(|(k, _)| k)
        .collect()
}

#[test]
fn a_probe_made_as_asked_has_no_deviation() {
    assert!(keys(&clean()).is_empty());
    // Absent lists and maps are empty.
    let mut v = clean();
    let host = v[0]["HostConfig"].as_object_mut().unwrap();
    host.remove("Binds");
    host.remove("Devices");
    host.remove("CapAdd");
    v[0]["HostConfig"]["Sysctls"] = json!({});
    assert!(keys(&v).is_empty());
}

/// Review HIGH 1: everything a containers.conf can add that the runner's
/// flags do not ask for is a deviation, each with its own key.
#[test]
fn every_widening_a_containers_conf_can_add_is_a_deviation() {
    let cases: [(&str, Value, &str); 14] = [
        (
            "/0/Mounts",
            json!([{"Type": "bind", "Destination": "/proc"}]),
            "Mounts",
        ),
        ("/0/HostConfig/Binds", json!(["/proc:/proc:rw"]), "Binds"),
        (
            "/0/HostConfig/Devices",
            json!([{"PathOnHost": "/dev/fuse"}]),
            "Devices",
        ),
        ("/0/HostConfig/CapAdd", json!(["CAP_SYS_ADMIN"]), "CapAdd"),
        ("/0/HostConfig/Privileged", json!(true), "Privileged"),
        (
            "/0/HostConfig/SecurityOpt",
            json!(["no-new-privileges", "seccomp=unconfined"]),
            "SecurityOpt",
        ),
        ("/0/HostConfig/IpcMode", json!("host"), "IpcMode"),
        ("/0/HostConfig/PidMode", json!("host"), "PidMode"),
        ("/0/HostConfig/UTSMode", json!("host"), "UTSMode"),
        ("/0/HostConfig/CgroupMode", json!("host"), "CgroupMode"),
        ("/0/HostConfig/UsernsMode", json!("host"), "UsernsMode"),
        ("/0/HostConfig/ShmSize", json!(1_000_000_000), "ShmSize"),
        (
            "/0/HostConfig/Annotations",
            json!({"evil": "1"}),
            "Annotations",
        ),
        (
            "/0/HostConfig/Sysctls",
            json!({"net.ipv4.ping_group_range": "0 0"}),
            "Sysctls",
        ),
    ];
    for (pointer, value, key) in cases {
        let mut v = clean();
        if let Some(slot) = v.pointer_mut(pointer) {
            *slot = value;
        } else {
            let (parent, field) = pointer.rsplit_once('/').unwrap();
            v.pointer_mut(parent).unwrap()[field] = value;
        }
        assert_eq!(keys(&v), [key], "{pointer}");
    }
    let mut v = clean();
    v[0]["Config"]["Annotations"] = json!({"evil": "1"});
    assert_eq!(keys(&v), ["Annotations"]);
    // A mode Podman did not report cannot be verified.
    let mut v = clean();
    v[0]["HostConfig"]
        .as_object_mut()
        .unwrap()
        .remove("IpcMode");
    assert_eq!(keys(&v), ["IpcMode"]);
}

/// The runner's environment (`env_host`) or a configured `env`: only the
/// names are reported, never the values.
#[test]
fn an_unexpected_environment_variable_is_named_but_its_value_never_shown() {
    let mut v = clean();
    v[0]["Config"]["Env"] = json!([
        "PATH=/bin",
        "FORGE_RUNNER_TOKEN=fgr_synthetic_secret",
        "EXTRA=1"
    ]);
    let found = deviations(v.to_string().as_bytes()).expect("parsed");
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].0, "Env");
    assert_eq!(found[0].1, "FORGE_RUNNER_TOKEN, EXTRA");
    assert!(deviations(b"not json").is_err());
    assert!(deviations(b"[]").is_err());
}

#[test]
fn the_runners_containers_conf_closes_every_key() {
    let dir = tempfile::tempdir().expect("dir");
    let conf = write_containers_conf(dir.path()).expect("written");
    assert_eq!(conf, dir.path().join("containers.conf"));
    let text = std::fs::read_to_string(&conf).expect("read");
    let parsed: toml::Table = text.parse().expect("valid TOML");
    let c = parsed["containers"].as_table().expect("[containers]");
    for key in ["annotations", "default_sysctls", "devices", "volumes"] {
        assert_eq!(c[key].as_array().map(Vec::len), Some(0), "{key}");
    }
    for (key, value) in [
        ("cgroupns", "private"),
        ("ipcns", "private"),
        ("pidns", "private"),
        ("utsns", "private"),
        ("netns", "none"),
        ("base_hosts_file", "none"),
        ("host_containers_internal_ip", "none"),
        ("shm_size", "64m"),
    ] {
        assert_eq!(c[key].as_str(), Some(value), "{key}");
    }
    for key in ["env_host", "http_proxy", "init"] {
        assert_eq!(c[key].as_bool(), Some(false), "{key}");
    }
    let env: Vec<&str> = c["env"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e.as_str())
        .collect();
    assert!(
        env.iter()
            .all(|e| ALLOWED_ENV.contains(&e.split('=').next().unwrap())),
        "{env:?}"
    );
    let mounts = PathBuf::from(c["default_mounts_file"].as_str().unwrap());
    assert_eq!(std::fs::read(&mounts).expect("mounts.conf"), b"");
    let hooks = parsed["engine"]["hooks_dir"].as_array().unwrap()[0]
        .as_str()
        .unwrap()
        .to_owned();
    // A hook left in the runner's directory is gone after the next start.
    std::fs::write(PathBuf::from(&hooks).join("evil.json"), b"{}").unwrap();
    write_containers_conf(dir.path()).expect("rewritten");
    assert_eq!(std::fs::read_dir(&hooks).unwrap().count(), 0);
}

/// Answers `create`, `inspect`, `ps` and `rm` and records every call.
struct Fake {
    calls: Mutex<Vec<Vec<String>>>,
    /// What `inspect` prints.
    inspect: String,
    /// What `ps` prints.
    ps: &'static str,
    /// The calls (`create` / `inspect` / `rm` / `ps`) that exit 125.
    fails: Vec<&'static str>,
}

impl Fake {
    fn new(inspect: &Value, fails: &[&'static str]) -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            inspect: inspect.to_string(),
            ps: "",
            fails: fails.to_vec(),
        }
    }

    fn subcommands(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .map(|a| a[0].clone())
            .collect()
    }
}

impl ProcessRunner for Fake {
    fn run(
        &self,
        cmd: &Cmd,
        _limit: Duration,
        _stop: &dyn Fn() -> bool,
        _out: &mut dyn FnMut(&[u8]),
    ) -> std::io::Result<Ran> {
        let argv = cmd.argv();
        self.calls.lock().unwrap().push(argv.clone());
        if self.fails.contains(&argv[0].as_str()) {
            return Ok(Ran {
                exit: Exit::Code(125),
                stdout: Vec::new(),
                stderr_tail: format!("Error: {} went wrong", argv[0]),
            });
        }
        let stdout = match argv[0].as_str() {
            "inspect" => self.inspect.clone().into_bytes(),
            "ps" => self.ps.as_bytes().to_vec(),
            _ => Vec::new(),
        };
        Ok(Ran {
            exit: Exit::Code(0),
            stdout,
            stderr_tail: String::new(),
        })
    }
}

fn plan() -> PodmanPlan {
    PodmanPlan::new(PathBuf::from("podman"), Limits::default())
}

fn context<'a>(err: &'a ForgeError, key: &str) -> Option<&'a str> {
    err.context()
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v.as_str())
}

#[test]
fn a_clean_probe_passes_and_is_removed() {
    let fake = Fake::new(&clean(), &[]);
    let rootfs = Path::new("/r/runner/empty-rootfs");
    check(&fake, &plan(), "forge-isolation-probe-42", rootfs).expect("isolated");
    assert_eq!(fake.subcommands(), ["rm", "create", "inspect", "rm"]);
    let calls = fake.calls.lock().unwrap();
    assert_eq!(
        calls[1],
        plan()
            .isolation_probe("forge-isolation-probe-42", rootfs)
            .argv()
    );
    assert_eq!(
        calls[2],
        plan().probe_inspect("forge-isolation-probe-42").argv()
    );
    assert_eq!(
        calls[3],
        [
            "rm",
            "--force",
            "--ignore",
            "--",
            "forge-isolation-probe-42"
        ]
    );
    assert_eq!(probe_name(42), "forge-isolation-probe-42");
}

/// The finding: the podman/stable image's `volumes = ["/proc:/proc"]`
/// gave every job the runner's processes. The runner refuses to run jobs.
#[test]
fn configured_default_volumes_stop_the_runner_before_any_job() {
    let mut v = clean();
    v[0]["Mounts"] = json!([{"Type": "bind", "Destination": "/proc"}]);
    v[0]["Config"]["Env"] = json!(["PATH=/bin", "EXTRA=1"]);
    let fake = Fake::new(&v, &[]);
    let err =
        check(&fake, &plan(), "forge-isolation-probe-42", Path::new("/e")).expect_err("refused");
    assert_eq!(err.code(), ErrorCode::JobIsolationUnsafe);
    assert_eq!(err.code().id(), "FG-CI-0015");
    assert_eq!(context(&err, "reason"), Some("unexpected_config"));
    assert_eq!(context(&err, "key"), Some("Mounts"));
    assert_eq!(
        context(&err, "deviations"),
        Some("Mounts: bind /proc; Env: EXTRA")
    );
    // Removed all the same.
    assert_eq!(fake.subcommands(), ["rm", "create", "inspect", "rm"]);
}

#[test]
fn a_podman_failure_is_reported_with_its_step_and_the_probe_is_removed() {
    for (fails, step, calls) in [
        ("create", "create", vec!["rm", "create", "rm"]),
        ("inspect", "inspect", vec!["rm", "create", "inspect", "rm"]),
        ("rm", "remove", vec!["rm"]),
    ] {
        let fake = Fake::new(&clean(), &[fails]);
        let err =
            check(&fake, &plan(), "forge-isolation-probe-42", Path::new("/e")).expect_err(fails);
        assert_eq!(err.code(), ErrorCode::JobIsolationUnsafe, "{fails}");
        assert_eq!(context(&err, "reason"), Some("probe_failed"), "{fails}");
        assert_eq!(context(&err, "step"), Some(step), "{fails}");
        assert_eq!(
            context(&err, "podman_stderr"),
            Some(format!("Error: {fails} went wrong").as_str()),
            "{fails}"
        );
        assert_eq!(fake.subcommands(), calls, "{fails}");
    }
    // Inspect AND the final removal fail: the inspect error keeps the
    // removal's stderr. (The leftover removal passes: `rm` fails only on
    // its second call here.)
    struct Second(Fake, Mutex<u32>);
    impl ProcessRunner for Second {
        fn run(
            &self,
            cmd: &Cmd,
            l: Duration,
            s: &dyn Fn() -> bool,
            o: &mut dyn FnMut(&[u8]),
        ) -> std::io::Result<Ran> {
            if cmd.argv()[0] == "rm" {
                let mut n = self.1.lock().unwrap();
                *n += 1;
                if *n == 2 {
                    return Ok(Ran {
                        exit: Exit::Code(125),
                        stdout: Vec::new(),
                        stderr_tail: "Error: rm went wrong".to_owned(),
                    });
                }
            }
            self.0.run(cmd, l, s, o)
        }
    }
    let both = Second(Fake::new(&clean(), &["inspect"]), Mutex::new(0));
    let err = check(&both, &plan(), "forge-isolation-probe-42", Path::new("/e")).expect_err("both");
    assert_eq!(context(&err, "step"), Some("inspect"));
    assert_eq!(context(&err, "remove_failed"), Some("Error: rm went wrong"));
    // Not an inspect result: unverified, refused.
    let fake = Fake {
        inspect: "exited 0".to_owned(),
        ..Fake::new(&clean(), &[])
    };
    let err =
        check(&fake, &plan(), "forge-isolation-probe-42", Path::new("/e")).expect_err("garbage");
    assert_eq!(context(&err, "reason"), Some("probe_failed"));
    assert_eq!(context(&err, "step"), Some("inspect"));
}

/// Review LOW: a crashed runner's probes are removed at start; another
/// name is left alone.
#[test]
fn stale_probes_are_removed_at_startup() {
    let fake = Fake {
        ps: "forge-isolation-probe-17\nforge-job-7-1\nforge-isolation-probe-\nforge-isolation-probe-test-1\n",
        ..Fake::new(&clean(), &[])
    };
    assert_eq!(remove_stale(&fake, &plan()).expect("removed"), 2);
    let calls = fake.calls.lock().unwrap();
    assert_eq!(calls[0], plan().stale_probes().argv());
    assert_eq!(
        calls[1],
        [
            "rm",
            "--force",
            "--ignore",
            "--",
            "forge-isolation-probe-17"
        ]
    );
    assert_eq!(
        calls[2],
        [
            "rm",
            "--force",
            "--ignore",
            "--",
            "forge-isolation-probe-test-1"
        ]
    );
    let fake = Fake::new(&clean(), &["ps"]);
    let err = remove_stale(&fake, &plan()).expect_err("ps failed");
    assert_eq!(context(&err, "step"), Some("stale"));
}

// ── real Podman (`--run-ignored only`, as the runner's user) ─────────────

/// [`crate::process::SystemRunner`] with one more variable on every call:
/// `XDG_CONFIG_HOME` points Podman at a test's own user configuration.
#[cfg(unix)]
struct WithEnv(Vec<(&'static str, PathBuf)>);

#[cfg(unix)]
impl ProcessRunner for WithEnv {
    fn run(
        &self,
        cmd: &Cmd,
        l: Duration,
        s: &dyn Fn() -> bool,
        o: &mut dyn FnMut(&[u8]),
    ) -> std::io::Result<Ran> {
        let mut cmd = cmd.clone();
        for (k, v) in &self.0 {
            cmd = cmd.env(k, v.as_os_str());
        }
        crate::process::SystemRunner.run(&cmd, l, s, o)
    }
}

/// A user drop-in like the podman/stable image's — `/proc` mounted, an
/// extra variable, the runner's environment, an annotation, host IPC —
/// reaches the probe without the runner's containers.conf (FG-CI-0015),
/// and does not with it.
#[cfg(unix)]
#[test]
#[ignore = "needs rootless Podman (runbook ci-runner)"]
fn real_podman_user_drop_ins_reach_jobs_only_without_the_runners_conf() {
    let tmp = tempfile::tempdir().expect("dir");
    let dropins = tmp.path().join("xdg/containers/containers.conf.d");
    std::fs::create_dir_all(&dropins).unwrap();
    std::fs::write(
        dropins.join("99-forge-test.conf"),
        "[containers]\nvolumes = [\"/proc:/proc\"]\nenv = [\"FORGE_TEST_LEAK=1\"]\nenv_host = true\nannotations = [\"forge.test=1\"]\nipcns = \"host\"\n",
    )
    .unwrap();
    let rootfs = tmp.path().join("rootfs");
    std::fs::create_dir_all(&rootfs).unwrap();
    let proc = WithEnv(vec![
        ("XDG_CONFIG_HOME", tmp.path().join("xdg")),
        ("FORGE_TEST_RUNNER_SECRET", PathBuf::from("synthetic")),
    ]);
    let podman = PathBuf::from(
        std::env::var_os("FORGE_RUNNER_TEST_PODMAN").unwrap_or_else(|| "podman".into()),
    );
    let bare = PodmanPlan::new(podman.clone(), Limits::default());
    let err = check(&proc, &bare, "forge-isolation-probe-test-1", &rootfs)
        .expect_err("the drop-in reaches the probe");
    assert_eq!(err.code(), ErrorCode::JobIsolationUnsafe, "{err:?}");
    assert_eq!(
        context(&err, "reason"),
        Some("unexpected_config"),
        "{err:?}"
    );
    let found = context(&err, "deviations").unwrap_or("");
    for key in ["Mounts", "Env: FORGE_TEST_LEAK", "Annotations"] {
        assert!(found.contains(key), "{key}: {found}");
    }
    let conf = write_containers_conf(&tmp.path().join("podman")).expect("conf");
    let closed = PodmanPlan::new(podman, Limits::default()).with_containers_conf(conf);
    check(&proc, &closed, "forge-isolation-probe-test-1", &rootfs)
        .expect("the runner's conf ignores the drop-in");
}

/// The runner's own containers.conf, edited after the start: `volumes`
/// is refused (no flag removes it); `env_host = true` is neutralised by
/// `--env-host=false` and the cleared environment — the probe stays clean.
#[cfg(unix)]
#[test]
#[ignore = "needs rootless Podman (runbook ci-runner)"]
fn real_podman_a_tampered_runner_conf_is_refused_and_env_host_never_reaches_a_job() {
    let tmp = tempfile::tempdir().expect("dir");
    let rootfs = tmp.path().join("rootfs");
    std::fs::create_dir_all(&rootfs).unwrap();
    let podman = PathBuf::from(
        std::env::var_os("FORGE_RUNNER_TEST_PODMAN").unwrap_or_else(|| "podman".into()),
    );
    let proc = WithEnv(vec![(
        "FORGE_TEST_RUNNER_SECRET",
        PathBuf::from("synthetic"),
    )]);
    let conf = write_containers_conf(&tmp.path().join("podman")).expect("conf");
    let text = std::fs::read_to_string(&conf).unwrap();
    std::fs::write(
        &conf,
        text.replace("volumes = []", "volumes = [\"/proc:/proc\"]"),
    )
    .unwrap();
    let plan = PodmanPlan::new(podman, Limits::default()).with_containers_conf(conf.clone());
    let err =
        check(&proc, &plan, "forge-isolation-probe-test-2", &rootfs).expect_err("volumes refused");
    assert_eq!(context(&err, "key"), Some("Mounts"), "{err:?}");
    std::fs::write(&conf, text.replace("env_host = false", "env_host = true")).unwrap();
    check(&proc, &plan, "forge-isolation-probe-test-2", &rootfs).expect("env_host neutralised");
}
