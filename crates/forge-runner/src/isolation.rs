//! A job's container is isolated from the runner whatever the host's
//! containers.conf says (ADR-0029 amendment 2026-11-12, FG-CI-0015).
//!
//! Found on the test runner: the quay.io/podman/stable image ships
//! `volumes = ["/proc:/proc"]` and host IPC/UTS/cgroup namespaces in its
//! containers.conf, and every job got them (the runner's `/proc`: every
//! process of the runner, of other jobs and of their services). A
//! containers.conf can add more that no Podman flag removes — `volumes`,
//! `env`, `devices`, `default_sysctls`, `annotations`, `mounts.conf`, OCI
//! hooks — so:
//!
//! 1. Podman reads only the runner's own containers.conf
//!    ([`containers_conf`], written at every start by
//!    [`write_containers_conf`] under `<work_dir>/runner/podman/`, passed as
//!    `CONTAINERS_CONF` on every call; with it set Podman reads no system or
//!    user containers.conf and no drop-in — checked on Podman 5.3). Every
//!    Podman call runs with a cleared environment (`PATH` and
//!    `CONTAINERS_CONF`; [`crate::process`]), so `env_host` or
//!    `http_proxy` could not pass the runner's environment anyway.
//! 2. Every container gets the isolating flags itself
//!    ([`PodmanPlan`]'s confinement).
//! 3. Before every claim (and at start, and in `check`) the runner creates
//!    a probe container like a service — never started, from an empty
//!    directory, no pull — reads what Podman made of it ([`deviations`])
//!    and refuses to run jobs on any difference from what its flags ask.
//!
//! ```text
//! podman rm --force --ignore -- forge-isolation-probe-<pid>      (a leftover)
//! podman create --name=forge-isolation-probe-<pid> … --rootfs -- <empty dir> /forge-isolation-probe
//! podman inspect --type=container -- forge-isolation-probe-<pid>
//! podman rm --force --ignore -- forge-isolation-probe-<pid>      (always)
//! ```

use std::path::{Path, PathBuf};
use std::time::Duration;

use forge_errors::{ErrorCode, ForgeError};
use serde_json::Value;

use crate::podman::{PodmanPlan, SHM_SIZE, SHM_SIZE_BYTES};
use crate::process::{Exit, ProcessRunner, Ran};

/// The limit of each probe call.
const PROBE_CALL: Duration = Duration::from_secs(60);

/// The only environment variables a probe container may have: the
/// runner's containers.conf `env` and what Podman itself sets.
pub const ALLOWED_ENV: [&str; 5] = ["PATH", "TERM", "container", "HOME", "HOSTNAME"];

/// `forge-isolation-probe-<n>` (the runner's process id): not a
/// `forge-job-` name, so the job cleanup never sees it
/// ([`remove_stale`] does).
pub fn probe_name(id: u32) -> String {
    format!("forge-isolation-probe-{id}")
}

fn unsafe_isolation(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::JobIsolationUnsafe).with_context("reason", reason)
}

fn podman_failed(step: &'static str, ran: Ran) -> ForgeError {
    let e = unsafe_isolation("probe_failed").with_context("step", step);
    let e = match ran.exit {
        Exit::Code(code) => e.with_context("exit_code", code),
        Exit::TimedOut => e.with_context("timeout_ms", PROBE_CALL.as_millis()),
        Exit::Stopped => e,
    };
    e.with_context("podman_stderr", ran.stderr_tail)
}

/// A TOML string (quoted and escaped).
fn toml_str(text: &str) -> String {
    toml::Value::String(text.to_owned()).to_string()
}

/// The runner's own containers.conf for the directory `dir`: every key a
/// containers.conf could use to widen a container, set to its closed
/// value; `mounts.conf` and the OCI hooks directory are empty files of the
/// runner's own.
pub fn containers_conf(dir: &Path) -> String {
    let mounts = toml_str(&dir.join("mounts.conf").to_string_lossy());
    let hooks = toml_str(&dir.join("hooks.d").to_string_lossy());
    format!(
        r#"# forge-runner's own containers.conf (ADR-0029 amendment 2026-11-12).
# Rewritten at every start; Podman reads only this file (CONTAINERS_CONF).
[containers]
annotations = []
base_hosts_file = "none"
cgroupns = "private"
default_mounts_file = {mounts}
default_sysctls = []
devices = []
env = ["PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin", "TERM=xterm"]
env_host = false
host_containers_internal_ip = "none"
http_proxy = false
init = false
ipcns = "private"
netns = "none"
pidns = "private"
shm_size = "{SHM_SIZE}"
utsns = "private"
volumes = []

[engine]
hooks_dir = [{hooks}]
"#
    )
}

/// Writes [`containers_conf`] to `<dir>/containers.conf`, an empty
/// `<dir>/mounts.conf` and an empty `<dir>/hooks.d` (emptied when it held
/// anything); returns the containers.conf path.
pub fn write_containers_conf(dir: &Path) -> std::io::Result<PathBuf> {
    let hooks = dir.join("hooks.d");
    if hooks.exists() {
        std::fs::remove_dir_all(&hooks)?;
    }
    std::fs::create_dir_all(&hooks)?;
    std::fs::write(dir.join("mounts.conf"), b"")?;
    let conf = dir.join("containers.conf");
    std::fs::write(&conf, containers_conf(dir))?;
    Ok(conf)
}

/// What differs in a probe's `podman inspect` JSON from what the runner's
/// flags ask, as `(key, detail)`; never an environment value (only its
/// name). `Err` when the JSON is not an inspect result.
pub fn deviations(json: &[u8]) -> Result<Vec<(&'static str, String)>, String> {
    let parsed: Value = serde_json::from_slice(json).map_err(|e| format!("json: {e}"))?;
    let probe = parsed
        .as_array()
        .and_then(|a| a.first())
        .ok_or_else(|| "not an inspect result".to_owned())?;
    let host = &probe["HostConfig"];
    let config = &probe["Config"];
    if !host.is_object() || !config.is_object() {
        return Err("no HostConfig or Config".to_owned());
    }
    // An absent or null list / map is empty.
    let len = |v: &Value| match v {
        Value::Array(a) => a.len(),
        Value::Object(o) => o.len(),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => 0,
    };
    let mut found = Vec::new();
    if len(&probe["Mounts"]) > 0 {
        let mounts: Vec<String> = probe["Mounts"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|m| {
                format!(
                    "{} {}",
                    m["Type"].as_str().unwrap_or("?"),
                    m["Destination"].as_str().unwrap_or("?")
                )
            })
            .collect();
        found.push(("Mounts", mounts.join(", ")));
    }
    for key in ["Binds", "Devices", "CapAdd", "Sysctls"] {
        if len(&host[key]) > 0 {
            found.push((key, host[key].to_string()));
        }
    }
    if host["Privileged"] != Value::Bool(false) {
        found.push(("Privileged", host["Privileged"].to_string()));
    }
    if host["SecurityOpt"] != serde_json::json!(["no-new-privileges"]) {
        found.push(("SecurityOpt", host["SecurityOpt"].to_string()));
    }
    for key in ["PidMode", "IpcMode", "UTSMode", "CgroupMode"] {
        if host[key].as_str() != Some("private") {
            found.push((key, host[key].to_string()));
        }
    }
    // Rootless Podman's own user namespace (`--userns=private` needs
    // explicit mappings; `host` would be the runner's namespace).
    if host["UsernsMode"].as_str() != Some("") {
        found.push(("UsernsMode", host["UsernsMode"].to_string()));
    }
    if host["ShmSize"].as_u64() != Some(SHM_SIZE_BYTES) {
        found.push(("ShmSize", host["ShmSize"].to_string()));
    }
    if len(&host["Annotations"]) > 0 || len(&config["Annotations"]) > 0 {
        let mut names: Vec<&str> = [&host["Annotations"], &config["Annotations"]]
            .into_iter()
            .filter_map(Value::as_object)
            .flat_map(|o| o.keys().map(String::as_str))
            .collect();
        names.dedup();
        found.push(("Annotations", names.join(", ")));
    }
    let extra: Vec<&str> = config["Env"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(|e| e.split_once('=').map_or(e, |(k, _)| k))
        .filter(|k| !ALLOWED_ENV.contains(k))
        .collect();
    if !extra.is_empty() {
        found.push(("Env", extra.join(", ")));
    }
    Ok(found)
}

/// Creates the probe container `name` from the empty directory `rootfs`,
/// inspects it and removes it. `Ok` when Podman made it as asked;
/// FG-CI-0015 `reason=unexpected_config` (with `key` and `deviations`)
/// when not, `reason=probe_failed` (with `step`) when Podman failed.
pub fn check(
    proc: &dyn ProcessRunner,
    podman: &PodmanPlan,
    name: &str,
    rootfs: &Path,
) -> Result<(), ForgeError> {
    let call = |step: &'static str, cmd| {
        proc.run(&cmd, PROBE_CALL, &|| false, &mut |_| {})
            .map_err(|e| {
                unsafe_isolation("probe_failed")
                    .with_context("step", step)
                    .with_source(e)
            })
    };
    let leftover = call("remove", podman.remove(name))?;
    if !leftover.ok() {
        return Err(podman_failed("remove", leftover));
    }
    let inspected = (|| {
        let created = call("create", podman.isolation_probe(name, rootfs))?;
        if !created.ok() {
            return Err(podman_failed("create", created));
        }
        let ran = call("inspect", podman.probe_inspect(name))?;
        if !ran.ok() {
            return Err(podman_failed("inspect", ran));
        }
        Ok(ran.stdout)
    })();
    // Always removed, whatever happened (it was never started). When both
    // fail, the first error keeps the removal's.
    let removed = match call("remove", podman.remove(name)) {
        Ok(ran) if ran.ok() => Ok(()),
        Ok(ran) => Err(podman_failed("remove", ran)),
        Err(e) => Err(e),
    };
    let stdout = match (inspected, removed) {
        (Ok(stdout), Ok(())) => stdout,
        (Ok(_), Err(e)) => return Err(e),
        (Err(e), Ok(())) => return Err(e),
        (Err(e), Err(r)) => {
            let detail = r
                .context()
                .iter()
                .find(|(k, _)| *k == "podman_stderr")
                .map_or_else(|| format!("{r}"), |(_, v)| v.clone());
            return Err(e.with_context("remove_failed", detail));
        }
    };
    let found = deviations(&stdout).map_err(|reason| {
        unsafe_isolation("probe_failed")
            .with_context("step", "inspect")
            .with_context("parse", reason)
    })?;
    let Some((key, _)) = found.first() else {
        return Ok(());
    };
    let all: Vec<String> = found.iter().map(|(k, d)| format!("{k}: {d}")).collect();
    Err(unsafe_isolation("unexpected_config")
        .with_context("key", *key)
        .with_context("deviations", all.join("; ")))
}

/// Removes every `forge-isolation-probe-*` container (a crashed runner's;
/// at start). FG-CI-0015 `reason=probe_failed`, `step=stale` when Podman
/// fails.
pub fn remove_stale(proc: &dyn ProcessRunner, podman: &PodmanPlan) -> Result<usize, ForgeError> {
    let call = |cmd| {
        let ran = proc
            .run(&cmd, PROBE_CALL, &|| false, &mut |_| {})
            .map_err(|e| {
                unsafe_isolation("probe_failed")
                    .with_context("step", "stale")
                    .with_source(e)
            })?;
        if ran.ok() {
            Ok(ran)
        } else {
            Err(podman_failed("stale", ran))
        }
    };
    let listed = call(podman.stale_probes())?;
    let mut removed = 0;
    for name in String::from_utf8_lossy(&listed.stdout).lines() {
        let name = name.trim();
        let Some(id) = name.strip_prefix("forge-isolation-probe-") else {
            continue;
        };
        if id.is_empty() || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
            continue;
        }
        call(podman.remove(name))?;
        removed += 1;
    }
    Ok(removed)
}

#[cfg(test)]
#[path = "isolation_tests.rs"]
mod tests;
