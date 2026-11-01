//! The `gstn` binary: wires the process's real stdin/stdout/stderr and
//! environment into `forge_client::app::run`.

use forge_client::app::{run, Io};
use std::io::IsTerminal;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(str::to_string))
        .unwrap_or_else(|| "gstn".to_string());
    let env = |k: &str| std::env::var(k).ok();
    let stdin_tty = std::io::stdin().is_terminal();
    let stdout_tty = std::io::stdout().is_terminal();
    let mut stdin = std::io::stdin().lock();
    let mut stdout = std::io::stdout().lock();
    let mut stderr = std::io::stderr().lock();
    let mut io = Io {
        stdin: &mut stdin,
        stdout: &mut stdout,
        stderr: &mut stderr,
        stdin_tty,
        stdout_tty,
    };
    let code = run(&args, &env, &exe, &mut io);
    std::process::exit(code);
}
