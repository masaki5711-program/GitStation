//! `forge hook pre-receive` (M3 X8, ADR-0008 amendment X8): the push
//! checks, run by git's pre-receive hook script inside receive-pack, never
//! by hand. Everything it needs comes from the run directory `forge serve`
//! armed (`FORGE_PUSH_CHECK_DIR`) and from git (its update lines on stdin,
//! the quarantine variables); see `forge_git::push_hook`.
//!
//! Its stdout and stderr reach the pusher (git relays them as `remote:`),
//! so nothing is logged here: the lines are forge's user-facing ones, and
//! `forge serve` logs the outcome once from the result file. Exit 0 lets
//! git go on; 1 refuses the whole push.

use std::io::{Read as _, Write as _};
use std::process::ExitCode;

use forge_errors::{ErrorCode, Lang};
use forge_git::push_hook::{self, HookEnv, HookExit, MAX_HOOK_STDIN};

pub fn pre_receive() -> ExitCode {
    let exit = run();
    // Best effort: a pusher that went away cannot read it anyway.
    let _ = std::io::stderr().write_all(exit.stderr.as_bytes()); // ignore: see above
    if exit.accept {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn run() -> HookExit {
    let env = HookEnv {
        dir: std::env::var_os(push_hook::DIR_ENV),
        object_dir: std::env::var_os("GIT_OBJECT_DIRECTORY"),
        alternates: std::env::var_os("GIT_ALTERNATE_OBJECT_DIRECTORIES"),
    };
    let mut stdin = Vec::new();
    let read = std::io::stdin()
        .take(u64::try_from(MAX_HOOK_STDIN).unwrap_or(u64::MAX) + 1)
        .read_to_end(&mut stdin);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build();
    match (read, runtime) {
        (Ok(_), Ok(rt)) => rt.block_on(push_hook::run_pre_receive(env, &stdin)),
        (Err(_), _) => internal("stdin_unreadable"),
        (_, Err(_)) => internal("runtime"),
    }
}

/// The hook could not even start its checks: refuse (fail closed).
fn internal(reason: &str) -> HookExit {
    let code = ErrorCode::PushCheckFailed;
    HookExit {
        accept: false,
        stderr: format!(
            "{} {} (reason: {reason})\n",
            code.id(),
            code.user_message(Lang::En)
        ),
    }
}
