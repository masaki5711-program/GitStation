//! The SSH transport (ADR-0031 decisions 3, 4): one remote call
//! `<script> <verb> <sha>` on one target, behind [`DeployTransport`] so the
//! state machine is tested without a real ssh.
//!
//! [`ssh_argv`] builds the argument vector (never a shell string): no
//! configuration file, the deploy key only, batch mode, a connect timeout,
//! strict host key checking against a known_hosts file holding only the
//! pinned key, no agent, no forwarding, no proxy or local command, `--`
//! before the host. [`classify`] turns ssh's exit status and its own
//! diagnostics into an [`Outcome`]; the remote output is never kept.

use std::ffi::OsString;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::time::Duration;

use forge_core::deploy::{HostKey, RemoteVerb, TargetPolicy};
use tokio::io::AsyncReadExt as _;

/// One remote call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteCall {
    pub target: String,
    pub host: String,
    pub port: u16,
    pub user: String,
    pub host_key: HostKey,
    pub script: String,
    pub verb: RemoteVerb,
    pub sha: String,
    pub timeout: Duration,
}

/// How one remote call ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The script exited 0.
    Ok,
    /// The script ran and exited non-zero (or was killed by a signal:
    /// `None`).
    Exit(Option<i32>),
    /// The whole call passed its timeout (the ssh process is killed).
    Timeout,
    /// ssh never opened a session (its message proves it: `refused`,
    /// `timed_out`, `dns`, `auth`, `network`, `handshake`) or could not be
    /// started (`spawn`); the script never ran.
    Unreachable(&'static str),
    /// ssh failed (255) without a message that proves the session never
    /// opened: the script may have run (the target counts as changed).
    Lost,
    /// The host resolved to an address forge never connects to
    /// (`TargetPolicy`; the reason); nothing was sent.
    NotAllowed(&'static str),
    /// The host presented another key than the pinned one.
    HostKeyMismatch,
}

/// What a transport's call returns.
pub type CallFuture<'a> = Pin<Box<dyn Future<Output = Outcome> + Send + 'a>>;

/// Runs one remote call. The real one is [`SshTransport`]; tests use a
/// scripted fake.
pub trait DeployTransport: Send + Sync {
    fn call<'a>(&'a self, call: &'a RemoteCall) -> CallFuture<'a>;
}

/// The ssh options of every call, in order (before `-p`, `-l`, `--`).
/// `address` is the checked address ssh connects to; the pinned key is
/// looked up by the target's name (`HostKeyAlias`), so a DNS change
/// between the check and the connect cannot redirect the session.
pub fn ssh_argv(
    key_file: &Path,
    known_hosts: &Path,
    connect_timeout: Duration,
    call: &RemoteCall,
    address: &str,
) -> Vec<String> {
    let mut argv: Vec<String> = vec!["-F".into(), "none".into()];
    argv.push("-i".into());
    argv.push(key_file.display().to_string());
    let options = [
        "BatchMode=yes".to_owned(),
        format!("ConnectTimeout={}", connect_timeout.as_secs().max(1)),
        "StrictHostKeyChecking=yes".to_owned(),
        // No quotes: on Windows the argv becomes one command line and an
        // embedded `"` reaches ssh as `\"` (`invalid quotes`). The caller
        // checked the path with [`option_safe_path`].
        format!("UserKnownHostsFile={}", known_hosts.display()),
        "GlobalKnownHostsFile=none".to_owned(),
        format!("HostKeyAlgorithms={}", call.host_key.key_type()),
        format!("HostKeyAlias={}", call.host),
        "UpdateHostKeys=no".to_owned(),
        "CheckHostIP=no".to_owned(),
        "IdentitiesOnly=yes".to_owned(),
        "IdentityAgent=none".to_owned(),
        "PasswordAuthentication=no".to_owned(),
        "KbdInteractiveAuthentication=no".to_owned(),
        "ForwardAgent=no".to_owned(),
        "ForwardX11=no".to_owned(),
        "ClearAllForwardings=yes".to_owned(),
        "PermitLocalCommand=no".to_owned(),
        "ProxyCommand=none".to_owned(),
        "ControlMaster=no".to_owned(),
        "ControlPath=none".to_owned(),
        "ServerAliveInterval=15".to_owned(),
        "ServerAliveCountMax=3".to_owned(),
        "LogLevel=ERROR".to_owned(),
    ];
    for o in options {
        argv.push("-o".into());
        argv.push(o);
    }
    argv.push("-T".into());
    argv.push("-p".into());
    argv.push(call.port.to_string());
    argv.push("-l".into());
    argv.push(call.user.clone()); // clone: the argv owns its strings
    argv.push("--".into());
    argv.push(address.to_owned());
    // The remote command: three plain tokens (`forge_core::deploy` checks
    // that each is `[A-Za-z0-9._-]` before a call is built).
    argv.push(call.script.clone()); // clone: as above
    argv.push(call.verb.as_str().to_owned());
    argv.push(call.sha.clone()); // clone: as above
    argv
}

/// The ssh messages of a host key that does not match the pinned one.
const MISMATCH_MARKERS: &[&str] = &[
    "Host key verification failed",
    "REMOTE HOST IDENTIFICATION HAS CHANGED",
    "host key is known",
    "No matching host key type found",
];

/// How the first line of ssh's stderr starts when ssh refused its own
/// command line or options (it exits 255 before connecting). Only ssh's
/// own warnings of [`PRE_CONNECT_WARNINGS`] may come before it.
const CONFIG_PREFIXES: &[&str] = &[
    "command-line line ",
    "command-line: line ",
    "usage: ssh",
    "unknown option -- ",
];

/// ssh's own warnings printed while it reads its options, before any
/// configuration error (e.g. a missing key file, seen with Git's MSYS ssh).
const PRE_CONNECT_WARNINGS: &[&str] = &["Warning: Identity file "];

/// ssh messages that prove the session never opened, with their class.
const NEVER_OPENED: &[(&str, &str)] = &[
    ("Connection refused", "refused"),
    ("Connection timed out", "timed_out"),
    ("Operation timed out", "timed_out"),
    ("Could not resolve hostname", "dns"),
    ("Name or service not known", "dns"),
    ("Permission denied", "auth"),
    ("Too many authentication failures", "auth"),
    ("No route to host", "network"),
    ("Network is unreachable", "network"),
    ("kex_exchange_identification", "handshake"),
    ("Unable to negotiate", "handshake"),
];

/// ssh's exit status and its stderr into an [`Outcome`]: 0 is the
/// script's success; 255 is ssh's own failure — its own arguments refused
/// (`ssh_config`: only the first line counts after ssh's own
/// [`PRE_CONNECT_WARNINGS`], as ssh prints it before anything of the remote
/// side), a host key mismatch or a session that
/// provably never opened when ssh says so, otherwise
/// [`Outcome::Lost`] (the script may have run); any other status is the
/// script's.
pub fn classify(code: Option<i32>, stderr: &[u8]) -> Outcome {
    match code {
        Some(0) => Outcome::Ok,
        Some(255) => {
            let text = String::from_utf8_lossy(stderr);
            let first = text
                .lines()
                .map(str::trim)
                .find(|l| !l.is_empty() && !PRE_CONNECT_WARNINGS.iter().any(|w| l.starts_with(w)));
            if first.is_some_and(|l| CONFIG_PREFIXES.iter().any(|p| l.starts_with(p))) {
                return Outcome::Unreachable("ssh_config");
            }
            if MISMATCH_MARKERS.iter().any(|m| text.contains(m)) {
                return Outcome::HostKeyMismatch;
            }
            match NEVER_OPENED.iter().find(|(m, _)| text.contains(m)) {
                Some((_, class)) => Outcome::Unreachable(class),
                None => Outcome::Lost,
            }
        }
        other => Outcome::Exit(other),
    }
}

/// Whether ssh's option parser reads `path` back unchanged as the value of
/// `-o Name=<path>` (no quotes around it): no whitespace or control
/// character (a value list separator), no quote, no `%` / `$` token or
/// `#` comment, no leading `~` (home expansion) and no two backslashes in
/// a row (an escape; also a UNC path).
/// Single backslashes of a Windows path stay as they are.
pub fn option_safe_path(path: &Path) -> bool {
    let Some(text) = path.to_str() else {
        return false;
    };
    !text.is_empty()
        && !text.starts_with('~')
        && !text.contains("\\\\")
        && !text.chars().any(|c| {
            c.is_whitespace() || c.is_control() || matches!(c, '"' | '\'' | '%' | '$' | '#')
        })
}

/// The variables a ssh child may see, out of the parent's.
#[cfg(windows)]
const PASSED_VARS: &[&str] = &[
    "PATH",
    "SystemRoot",
    // Windows OpenSSH exits 255 without a word when it is unset.
    "ProgramData",
];
/// The variables a ssh child may see, out of the parent's.
#[cfg(not(windows))]
const PASSED_VARS: &[&str] = &["PATH"];

/// The ssh child's whole environment: [`PASSED_VARS`] of `parent` (on
/// Windows matched without case, as Windows does), then HOME and
/// USERPROFILE set to the scratch `home`. Never the parent's
/// SSH_AUTH_SOCK, home or anything else.
pub fn ssh_env(
    parent: impl IntoIterator<Item = (OsString, OsString)>,
    home: &Path,
) -> Vec<(OsString, OsString)> {
    let passed = |k: &str| {
        PASSED_VARS.iter().any(|p| {
            if cfg!(windows) {
                p.eq_ignore_ascii_case(k)
            } else {
                *p == k
            }
        })
    };
    let mut env: Vec<(OsString, OsString)> = parent
        .into_iter()
        .filter(|(k, _)| k.to_str().is_some_and(passed))
        .collect();
    env.push(("HOME".into(), home.as_os_str().to_owned()));
    env.push(("USERPROFILE".into(), home.as_os_str().to_owned()));
    env
}

/// The address ssh connects to: the target name resolved now, every
/// address checked against `policy` (one refused address refuses the
/// target: no fallback to another).
pub async fn resolve(
    policy: &TargetPolicy,
    host: &str,
    port: u16,
    limit: Duration,
) -> Result<std::net::IpAddr, Outcome> {
    let found = match tokio::time::timeout(limit, tokio::net::lookup_host((host, port))).await {
        Ok(Ok(found)) => found.map(|a| a.ip()).collect::<Vec<_>>(),
        Ok(Err(_)) => return Err(Outcome::Unreachable("dns")),
        Err(_) => return Err(Outcome::Unreachable("timed_out")),
    };
    let Some(first) = found.first().copied() else {
        return Err(Outcome::Unreachable("dns"));
    };
    for ip in &found {
        policy.check(*ip, port).map_err(Outcome::NotAllowed)?;
    }
    Ok(first)
}

/// The most of ssh's stderr read for [`classify`] (never logged).
const STDERR_CAP: usize = 16 * 1024;

/// The real transport: the ssh client as a child process.
#[derive(Debug, Clone)]
pub struct SshTransport {
    program: PathBuf,
    key_file: PathBuf,
    connect_timeout: Duration,
    policy: TargetPolicy,
}

impl SshTransport {
    pub fn new(
        program: PathBuf,
        key_file: PathBuf,
        connect_timeout: Duration,
        policy: TargetPolicy,
    ) -> Self {
        Self {
            program,
            key_file,
            connect_timeout,
            policy,
        }
    }

    async fn run(&self, call: &RemoteCall) -> Outcome {
        let address = match resolve(&self.policy, &call.host, call.port, self.connect_timeout).await
        {
            Ok(ip) => ip.to_string(),
            Err(outcome) => return outcome,
        };
        self.run_at(call, &address).await
    }

    /// The call against `address`, already checked by [`resolve`].
    async fn run_at(&self, call: &RemoteCall, address: &str) -> Outcome {
        // A scratch HOME: ssh reads no user configuration or keys of the
        // forge account (the files go with the directory).
        let Ok(home) = tempfile::Builder::new().prefix("forge-ssh-home-").tempdir() else {
            return Outcome::Unreachable("spawn");
        };
        // One known_hosts file per call holding only the pinned key; removed
        // when the guard drops.
        let known = match tempfile::Builder::new()
            .prefix("forge-known-hosts-")
            .tempfile()
        {
            Ok(f) => f,
            Err(_) => return Outcome::Unreachable("spawn"),
        };
        if !option_safe_path(known.path()) {
            return Outcome::Unreachable("temp_path");
        }
        // The pin under the target name, with and without the port
        // brackets (ssh looks up the HostKeyAlias either way).
        let line = format!(
            "{}{}",
            call.host_key.known_hosts_line(&call.host, 22),
            call.host_key.known_hosts_line(&call.host, call.port)
        );
        if tokio::fs::write(known.path(), line).await.is_err() {
            return Outcome::Unreachable("spawn");
        }
        let argv = ssh_argv(
            &self.key_file,
            known.path(),
            self.connect_timeout,
            call,
            address,
        );
        let mut cmd = tokio::process::Command::new(&self.program);
        cmd.args(&argv)
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // Only what the ssh client needs to start (`ssh_env`).
        cmd.envs(ssh_env(std::env::vars_os(), home.path()));
        let Ok(mut child) = cmd.spawn() else {
            return Outcome::Unreachable("spawn");
        };
        let stderr = child.stderr.take();
        let work = async move {
            let mut buf = Vec::new();
            if let Some(mut err) = stderr {
                // Read to the end (a chatty remote script must not block on
                // a full pipe), keep the first STDERR_CAP bytes. A read
                // error ends the read; the exit status decides.
                let mut chunk = [0u8; 8192];
                while let Ok(n) = err.read(&mut chunk).await {
                    if n == 0 {
                        break;
                    }
                    let room = STDERR_CAP.saturating_sub(buf.len());
                    buf.extend_from_slice(chunk.get(..n.min(room)).unwrap_or_default());
                }
            }
            let status = child.wait().await;
            (status, buf)
        };
        match tokio::time::timeout(call.timeout, work).await {
            // Dropping the future drops the child: kill_on_drop kills it.
            Err(_) => Outcome::Timeout,
            Ok((Err(_), _)) => Outcome::Unreachable("spawn"),
            Ok((Ok(status), stderr)) => classify(status.code(), &stderr),
        }
    }
}

impl DeployTransport for SshTransport {
    fn call<'a>(&'a self, call: &'a RemoteCall) -> CallFuture<'a> {
        Box::pin(self.run(call))
    }
}

#[cfg(test)]
#[path = "transport_tests.rs"]
mod tests;
