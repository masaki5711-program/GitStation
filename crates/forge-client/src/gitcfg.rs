//! `auth git-credential --install`: register the helper for the configured
//! host in the global git config (argument vector, timeout; never a shell
//! string for git itself).

use crate::config::{host_key, Config};
use crate::error::{ClientError, Result};
use forge_errors::ErrorCode;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const GIT_TIMEOUT: Duration = Duration::from_secs(10);

pub struct Installed {
    pub key: String,
    pub helper: String,
}

/// POSIX single-quote a word.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// The helper command line git runs through its shell: single-quote the
/// executable (forward slashes: Git for Windows runs `sh`) and pin the host.
pub fn helper_command(exe: &str, cfg: &Config) -> String {
    let host = host_key(&cfg.host);
    format!(
        "!{} --host {} auth git-credential",
        sh_quote(&exe.replace('\\', "/")),
        sh_quote(&host)
    )
}

/// `global_file` is `GIT_CONFIG_GLOBAL` when the caller's environment sets it
/// (tests, and users who keep the file elsewhere); otherwise git decides.
pub fn install(cfg: &Config, exe: &str, global_file: Option<&str>) -> Result<Installed> {
    let key = format!("credential.{}.helper", host_key(&cfg.host));
    let helper = helper_command(exe, cfg);
    // An empty value first resets helpers inherited from broader sections, so
    // only gstn answers for this host; then ours.
    git(
        global_file,
        &["config", "--global", "--replace-all", &key, ""],
    )?;
    git(global_file, &["config", "--global", "--add", &key, &helper])?;
    Ok(Installed { key, helper })
}

fn git(global_file: Option<&str>, args: &[&str]) -> Result<()> {
    let fail = |why: String| {
        ClientError::local(
            ErrorCode::CliConfigInvalid,
            format!("config_key=git reason={why}"),
        )
    };
    let mut cmd = Command::new("git");
    if let Some(f) = global_file {
        cmd.env("GIT_CONFIG_GLOBAL", f);
    }
    let mut child = cmd
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| fail(format!("git_not_runnable ({})", e.kind())))?;
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(st)) if st.success() => return Ok(()),
            Ok(Some(st)) => {
                return Err(fail(format!(
                    "git_config_failed (exit {})",
                    st.code().unwrap_or(-1)
                )))
            }
            Ok(None) if start.elapsed() > GIT_TIMEOUT => {
                let killed = child.kill().and_then(|()| child.wait());
                return Err(fail(format!(
                    "git_timeout (cleanup: {})",
                    if killed.is_ok() { "ok" } else { "failed" }
                )));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => return Err(fail(format!("git_wait_failed ({})", e.kind()))),
        }
    }
}
