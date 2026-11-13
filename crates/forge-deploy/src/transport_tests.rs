use std::path::Path;
use std::time::Duration;

use forge_core::deploy::{HostKey, RemoteVerb};

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
    );
    let joined = argv.join(" ");
    for must in [
        "-F none",
        "-i /etc/forge/deploy_key",
        "-o BatchMode=yes",
        "-o ConnectTimeout=10",
        "-o StrictHostKeyChecking=yes",
        "-o UserKnownHostsFile=\"/tmp/kh\"",
        "-o GlobalKnownHostsFile=none",
        "-o HostKeyAlgorithms=ssh-ed25519",
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
            "app1.internal".to_owned(),
            "web-app".to_owned(),
            "deploy".to_owned(),
            "a".repeat(40)
        ]
    );
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
        Outcome::Unreachable("connect")
    );
    assert_eq!(
        classify(
            Some(255),
            b"deploy@app1: Permission denied (publickey).\r\n"
        ),
        Outcome::Unreachable("connect")
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
    );
    assert_eq!(
        t.call(&call(RemoteVerb::Deploy)).await,
        Outcome::Unreachable("spawn")
    );
}
