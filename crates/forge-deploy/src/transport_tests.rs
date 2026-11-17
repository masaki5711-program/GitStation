use std::path::Path;
use std::time::Duration;

use forge_core::deploy::{HostKey, RemoteVerb, TargetPolicy};

use super::*;

pub(crate) fn key() -> HostKey {
    HostKey::parse(
        "ssh-ed25519",
        "AAAAC3NzaC1lZDI1NTE5AAAAIAkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJ",
    )
    .expect("key")
}

pub(crate) fn call(verb: RemoteVerb) -> RemoteCall {
    RemoteCall {
        target: "web-1".to_owned(),
        host: "app1.internal".to_owned(),
        port: 2222,
        user: "deploy".to_owned(),
        host_key: key(),
        script: "web-app".to_owned(),
        verb,
        sha: "a".repeat(40),
        timeout: Duration::from_secs(30),
    }
}

#[test]
fn the_argv_pins_the_host_key_and_runs_only_the_script() {
    let argv = ssh_argv(
        Path::new("/etc/forge/deploy_key"),
        Path::new("/tmp/kh"),
        Duration::from_secs(10),
        &call(RemoteVerb::Deploy),
        "10.0.0.5",
    );
    let joined = argv.join(" ");
    for must in [
        "-F none",
        "-i /etc/forge/deploy_key",
        "-o BatchMode=yes",
        "-o ConnectTimeout=10",
        "-o StrictHostKeyChecking=yes",
        "-o UserKnownHostsFile=/tmp/kh",
        "-o GlobalKnownHostsFile=none",
        "-o HostKeyAlgorithms=ssh-ed25519",
        "-o HostKeyAlias=app1.internal",
        "-o UpdateHostKeys=no",
        "-o IdentitiesOnly=yes",
        "-o IdentityAgent=none",
        "-o ForwardAgent=no",
        "-o ClearAllForwardings=yes",
        "-o PermitLocalCommand=no",
        "-o ProxyCommand=none",
        "-p 2222",
        "-l deploy",
    ] {
        assert!(joined.contains(must), "{must} missing in {joined}");
    }
    // `--`, then the host, then exactly the three remote tokens.
    let dash = argv.iter().position(|a| a == "--").expect("--");
    assert_eq!(
        &argv[dash + 1..],
        &[
            "10.0.0.5".to_owned(),
            "web-app".to_owned(),
            "deploy".to_owned(),
            "a".repeat(40)
        ]
    );
    // No option value carries quotes: on Windows the argv is rebuilt into a
    // command line and an embedded `"` reaches ssh as `\"` (`invalid quotes`).
    assert!(argv.iter().all(|a| !a.contains('"')), "{joined}");
    // Nothing accepts a new key or reads a user config.
    assert!(!joined.contains("accept-new"));
    assert!(!joined.contains("StrictHostKeyChecking=no"));
    assert!(!joined.contains("-A"));
}

#[test]
fn an_rsa_pin_offers_only_its_signature_algorithm() {
    let mut c = call(RemoteVerb::Health);
    c.host_key = HostKey::parse(
        "rsa-sha2-512",
        // "ssh-rsa" and one opaque string of 64 bytes 0x01.
        "AAAAB3NzaC1yc2EAAABAAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQ==",
    )
    .expect("rsa");
    let argv = ssh_argv(
        Path::new("/k"),
        Path::new("/kh"),
        Duration::from_secs(5),
        &c,
        "10.0.0.5",
    );
    assert!(argv.contains(&"HostKeyAlgorithms=rsa-sha2-512".to_owned()));
    assert!(argv.contains(&"health".to_owned()));
}

#[test]
fn ssh_failures_are_told_apart_from_the_script_failures() {
    assert_eq!(classify(Some(0), b""), Outcome::Ok);
    assert_eq!(classify(Some(3), b"anything"), Outcome::Exit(Some(3)));
    assert_eq!(classify(None, b""), Outcome::Exit(None));
    assert_eq!(
        classify(
            Some(255),
            b"ssh: connect to host app1 port 22: Connection refused\r\n"
        ),
        Outcome::Unreachable("refused")
    );
    assert_eq!(
        classify(
            Some(255),
            b"deploy@app1: Permission denied (publickey).\r\n"
        ),
        Outcome::Unreachable("auth")
    );
    for mismatch in [
        &b"Host key verification failed.\r\n"[..],
        b"@@@ WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED! @@@",
        b"No ED25519 host key is known for [app1]:2222 and you have requested strict checking.",
    ] {
        assert_eq!(classify(Some(255), mismatch), Outcome::HostKeyMismatch);
    }
}

#[tokio::test]
async fn a_missing_ssh_program_is_unreachable_not_a_panic() {
    let t = SshTransport::new(
        std::env::temp_dir().join("forge-no-such-ssh-program"),
        std::env::temp_dir().join("key"),
        Duration::from_secs(1),
        TargetPolicy::default(),
    );
    let mut c = call(RemoteVerb::Deploy);
    c.host = "10.0.0.5".to_owned();
    assert_eq!(t.call(&c).await, Outcome::Unreachable("spawn"));
}

#[test]
fn a_255_without_proof_that_the_session_never_opened_is_lost() {
    // The script may have run (the connection dropped, the remote side
    // killed it): the target counts as changed.
    assert_eq!(
        classify(
            Some(255),
            b"Connection to 10.0.0.5 closed by remote host.\r\n"
        ),
        Outcome::Lost
    );
    assert_eq!(classify(Some(255), b""), Outcome::Lost);
}

#[tokio::test]
async fn a_target_resolving_to_loopback_is_refused_at_connect() {
    let policy = TargetPolicy::default();
    assert_eq!(
        resolve(&policy, "127.0.0.1", 22, Duration::from_secs(2)).await,
        Err(Outcome::NotAllowed("denied_address"))
    );
    assert_eq!(
        resolve(&policy, "169.254.169.254", 22, Duration::from_secs(2)).await,
        Err(Outcome::NotAllowed("denied_address"))
    );
    // The transport refuses before ssh is started (the program does not
    // even exist here).
    let t = SshTransport::new(
        std::env::temp_dir().join("forge-no-such-ssh-program"),
        std::env::temp_dir().join("key"),
        Duration::from_secs(1),
        policy,
    );
    let mut c = call(RemoteVerb::Deploy);
    c.host = "127.0.0.1".to_owned();
    assert_eq!(t.call(&c).await, Outcome::NotAllowed("denied_address"));
}

#[test]
fn an_ssh_argument_error_is_told_apart_from_a_lost_connection() {
    // ssh refused its own command line or options before connecting: the
    // script never ran (the target is unchanged), and the cause is the
    // forge host, not the network.
    for config in [
        &b"command-line line 0: invalid quotes\r\n"[..],
        b"command-line: line 0: Bad configuration option: userknownhostsfle\n",
        b"unknown option -- Z\r\nusage: ssh [-46AaCfGgKkMNnqsTtVvXxYy] [-B bind_interface]\r\n",
        b"usage: ssh [-46AaCfGgKkMNnqsTtVvXxYy]\n",
        // Git's MSYS ssh on Windows (2026-11-17): its own key warning first.
        b"Warning: Identity file C:\\k not accessible: No such file or directory.\ncommand-line line 0: invalid quotes\r\n",
    ] {
        assert_eq!(
            classify(Some(255), config),
            Outcome::Unreachable("ssh_config"),
            "{}",
            String::from_utf8_lossy(config)
        );
    }
    // The same words later in the output (the remote script's own stderr
    // after the session opened) prove nothing: still lost.
    assert_eq!(
        classify(
            Some(255),
            b"starting web-app\ncommand-line line 0: invalid quotes\n"
        ),
        Outcome::Lost
    );
}

#[test]
fn only_paths_ssh_parses_back_unchanged_are_option_values() {
    for ok in [
        "/tmp/forge-known-hosts-AbC123",
        r"C:\Users\fujikawa\AppData\Local\Temp\forge-known-hosts-x1",
        r"C:\Users\FUJIKA~1\AppData\Local\Temp\forge-known-hosts-x1",
        "/var/tmp/\u{85e4}\u{5ddd}/kh",
    ] {
        assert!(option_safe_path(Path::new(ok)), "{ok}");
    }
    for bad in [
        "/tmp/with space/kh",
        "/tmp/tab\tkh",
        "/tmp/q\"kh",
        "/tmp/q'kh",
        "/tmp/%d/kh",
        "/tmp/${HOME}/kh",
        "/tmp/#kh",
        "~/kh",
        r"\\server\share\kh",
        r"C:\Temp\\kh",
    ] {
        assert!(!option_safe_path(Path::new(bad)), "{bad}");
    }
}

#[test]
fn the_ssh_child_gets_only_the_variables_it_needs() {
    use std::ffi::OsString;
    let parent = [
        ("PATH", "/usr/bin"),
        ("SystemRoot", r"C:\Windows"),
        ("ProgramData", r"C:\ProgramData"),
        ("SSH_AUTH_SOCK", "/run/agent.sock"),
        ("HOME", "/home/forge"),
        ("HOMEDRIVE", "C:"),
        ("HOMEPATH", r"\Users\forge"),
        ("FORGE_DATABASE_URL", "postgres://secret"),
    ]
    .map(|(k, v)| (OsString::from(k), OsString::from(v)));
    let home = Path::new("/scratch/home");
    let env = ssh_env(parent, home);
    let get = |name: &str| {
        env.iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.to_string_lossy().into_owned())
    };
    assert_eq!(get("PATH").as_deref(), Some("/usr/bin"));
    // HOME and USERPROFILE are always the scratch home, never the parent's.
    assert_eq!(get("HOME").as_deref(), Some("/scratch/home"));
    assert_eq!(get("USERPROFILE").as_deref(), Some("/scratch/home"));
    for never in [
        "SSH_AUTH_SOCK",
        "FORGE_DATABASE_URL",
        "HOMEDRIVE",
        "HOMEPATH",
    ] {
        assert_eq!(get(never), None, "{never} leaked");
    }
    // Windows OpenSSH exits 255 without a word when ProgramData is unset;
    // elsewhere neither Windows variable is passed.
    if cfg!(windows) {
        assert_eq!(get("ProgramData").as_deref(), Some(r"C:\ProgramData"));
        assert_eq!(get("SystemRoot").as_deref(), Some(r"C:\Windows"));
    } else {
        assert_eq!(get("ProgramData"), None);
        assert_eq!(get("SystemRoot"), None);
    }
    assert_eq!(env.len(), if cfg!(windows) { 5 } else { 3 });
}

/// Every ssh client of this host: on Windows the System32 build (the one
/// that needs ProgramData) and each `ssh` on PATH (Git's MSYS build).
fn host_ssh_clients() -> Vec<std::path::PathBuf> {
    let mut found = Vec::new();
    if cfg!(windows) {
        let sys = std::path::PathBuf::from(r"C:\Windows\System32\OpenSSH\ssh.exe");
        if sys.is_file() {
            found.push(sys);
        }
    }
    let name = if cfg!(windows) { "ssh.exe" } else { "ssh" };
    let path = std::env::var_os("PATH").unwrap_or_default();
    for p in std::env::split_paths(&path).map(|d| d.join(name)) {
        if p.is_file()
            && !found
                .iter()
                .any(|f| f.as_os_str().eq_ignore_ascii_case(p.as_os_str()))
        {
            found.push(p);
        }
    }
    found
}

#[tokio::test]
async fn the_real_ssh_client_accepts_the_argv_and_env() {
    // Bugs found on Windows (2026-11-17): the quoted UserKnownHostsFile and
    // the missing ProgramData both made ssh exit 255 before connecting,
    // logged as `connection_lost`. A local peer that accepts and closes at
    // once must instead be `handshake` (`kex_exchange_identification`):
    // ssh parsed every option, started and connected. (A closed port is
    // no proof: Windows OpenSSH prints its refusal below LogLevel=ERROR.)
    let clients = host_ssh_clients();
    if clients.is_empty() {
        eprintln!("SKIP the_real_ssh_client_accepts_the_argv_and_env: no ssh client on this host");
        return;
    }
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    std::thread::spawn(move || {
        // Accept and drop each connection; ends with the test process.
        for conn in listener.incoming() {
            drop(conn);
        }
    });
    let mut c = call(RemoteVerb::Health);
    c.port = port;
    c.timeout = Duration::from_secs(20);
    for program in clients {
        let t = SshTransport::new(
            program.clone(), // clone: named in the assertion below
            std::env::temp_dir().join("forge-no-such-deploy-key"),
            Duration::from_secs(5),
            TargetPolicy::default(),
        );
        assert_eq!(
            t.run_at(&c, "127.0.0.1").await,
            Outcome::Unreachable("handshake"),
            "{}",
            program.display()
        );
    }
}
