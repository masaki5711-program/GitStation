use super::*;

/// An SSH public key blob: the algorithm name, then one opaque string.
pub(crate) fn blob(alg: &str, body: &[u8]) -> String {
    let mut b = Vec::new();
    b.extend_from_slice(&u32::try_from(alg.len()).expect("len").to_be_bytes());
    b.extend_from_slice(alg.as_bytes());
    b.extend_from_slice(&u32::try_from(body.len()).expect("len").to_be_bytes());
    b.extend_from_slice(body);
    base64::engine::general_purpose::STANDARD.encode(b)
}

#[test]
fn a_status_only_moves_forward() {
    use DeploymentStatus as S;
    assert!(S::PendingApproval.may_become(S::Queued));
    assert!(S::PendingApproval.may_become(S::Blocked));
    assert!(S::Queued.may_become(S::Running));
    assert!(S::Queued.may_become(S::Blocked));
    for end in [S::Succeeded, S::Failed, S::RolledBack, S::RollbackFailed] {
        assert!(S::Running.may_become(end));
        assert!(end.is_final());
        for next in S::ALL {
            assert!(!end.may_become(next), "{end:?} -> {next:?}");
        }
    }
    assert!(
        !S::PendingApproval.may_become(S::Running),
        "no run without approval"
    );
    assert!(!S::Running.may_become(S::Queued), "never back to the queue");
    for s in S::ALL {
        assert_eq!(S::parse(s.as_str()), Some(s));
    }
    assert_eq!(DeployTier::parse("prod"), Some(DeployTier::Prod));
    assert_eq!(DeployTier::parse("Prod"), None);
}

#[test]
fn only_plain_tokens_reach_the_remote_command_line() {
    for ok in ["app", "deploy-app", "app.sh", "release_2"] {
        assert!(check_script_name(ok).is_ok(), "{ok}");
    }
    for bad in [
        "", "-x", "a b", "a;b", "a/b", "../x", "$(id)", "a`b", "A", "a\nb", ".hidden",
    ] {
        assert!(check_script_name(bad).is_err(), "{bad:?}");
    }
    assert!(check_script_name(&"a".repeat(65)).is_err());
    for ok in ["web-1", "a"] {
        assert!(check_target_name(ok).is_ok());
    }
    for bad in ["web.1", "-a", "Web", ""] {
        assert!(check_target_name(bad).is_err(), "{bad:?}");
    }
    for ok in ["deploy", "_svc", "app-1"] {
        assert!(check_user(ok).is_ok());
    }
    for bad in ["", "-oProxyCommand=x", "root@x", "Deploy", "a b"] {
        assert!(check_user(bad).is_err(), "{bad:?}");
    }
    for ok in ["app1.internal.example", "10.0.0.5"] {
        assert!(check_host(ok).is_ok(), "{ok}");
    }
    for bad in [
        "",
        "-oProxyCommand=x",
        "a..b",
        "host-",
        "user@host",
        "host:22",
        "[::1]",
        "Host",
        "a b",
    ] {
        assert!(check_host(bad).is_err(), "{bad:?}");
    }
    assert_eq!(check_port(22), Ok(22));
    assert!(check_port(0).is_err());
    assert!(check_port(70000).is_err());
}

#[test]
fn a_host_key_is_pinned_with_its_type_and_fingerprint() {
    let b64 = blob("ssh-ed25519", &[7u8; 32]);
    let key = HostKey::parse("ssh-ed25519", &b64).expect("key");
    assert!(key.fingerprint().starts_with("SHA256:"));
    assert_eq!(key.fingerprint().len(), "SHA256:".len() + 43);
    assert_eq!(
        key.known_hosts_line("app1.example", 22),
        format!("app1.example ssh-ed25519 {b64}\n")
    );
    assert_eq!(
        key.known_hosts_line("app1.example", 2222),
        format!("[app1.example]:2222 ssh-ed25519 {b64}\n")
    );
    // Another key, another fingerprint.
    let other = HostKey::parse("ssh-ed25519", &blob("ssh-ed25519", &[8u8; 32])).expect("key");
    assert_ne!(key.fingerprint(), other.fingerprint());
    // An RSA pin names its signature algorithm; the blob is ssh-rsa.
    let rsa = HostKey::parse("rsa-sha2-512", &blob("ssh-rsa", &[1u8; 64])).expect("rsa");
    assert!(rsa.known_hosts_line("h", 22).starts_with("h ssh-rsa "));
    assert_eq!(
        HostKey::parse("ssh-ed25519", &blob("ssh-rsa", &[1u8; 64])),
        Err("host_key_type_mismatch")
    );
    assert_eq!(
        HostKey::parse("ssh-dss", &b64),
        Err("unsupported_host_key_type")
    );
    assert_eq!(
        HostKey::parse("ssh-ed25519", "not base64!"),
        Err("invalid_host_key")
    );
    assert_eq!(
        HostKey::parse("ssh-ed25519", "AAAA"),
        Err("invalid_host_key")
    );
    // A newline cannot be smuggled into the known_hosts file.
    assert!(HostKey::parse("ssh-ed25519", &format!("{b64}\nevil")).is_err());
}

#[test]
fn a_deploy_change_never_carries_a_key_or_output() {
    let t = DeployChange::TargetSet {
        project: ProjectId::new(7),
        tier: DeployTier::Stg,
        name: "web-1".to_owned(),
        created: true,
        host: "app1.example".to_owned(),
        port: 22,
        user: "deploy".to_owned(),
        script: "app".to_owned(),
        fingerprint: "SHA256:abc".to_owned(),
        enabled: true,
    };
    assert_eq!(t.action(), "deploy_target.created");
    let v = t.to_value();
    assert_eq!(v["host_key_fingerprint"], "SHA256:abc");
    assert!(v.get("host_key").is_none());
    let a = DeployChange::Approved {
        project: ProjectId::new(7),
        deployment: 3,
        tier: DeployTier::Prod,
        sha: "a".repeat(40),
    };
    assert_eq!(a.action(), "deployment.approved");
    assert_eq!(a.project(), ProjectId::new(7));
}

#[test]
fn a_shell_is_never_a_target_script() {
    for bad in [
        "sh",
        "bash",
        "python3",
        "python3.11",
        "env",
        "sudo",
        "perl",
        "node",
    ] {
        assert_eq!(
            check_script_name(bad),
            Err("interpreter_script_name"),
            "{bad}"
        );
    }
    assert!(check_script_name("web-app").is_ok());
    assert!(check_script_name("deploy-shop.sh").is_ok());
    assert_eq!(check_host("localhost"), Err("denied_address"));
}

#[test]
fn forge_never_connects_to_its_own_host_or_the_metadata_service() {
    let open = TargetPolicy {
        allowed: Vec::new(),
        forge_ports: vec![8080],
    };
    let ip = |s: &str| s.parse::<std::net::IpAddr>().expect("ip");
    for bad in [
        "127.0.0.1",
        "127.1.2.3",
        "0.0.0.0",
        "169.254.169.254",
        "::1",
        "fe80::1",
        "::ffff:127.0.0.1",
        "224.0.0.1",
    ] {
        assert_eq!(open.check(ip(bad), 22), Err("denied_address"), "{bad}");
    }
    assert_eq!(open.check(ip("10.0.0.5"), 8080), Err("forge_port"));
    assert!(open.check(ip("10.0.0.5"), 22).is_ok());
    let fenced = TargetPolicy {
        allowed: vec![IpNet::parse("10.20.0.0/16").expect("net")],
        forge_ports: Vec::new(),
    };
    assert!(fenced.check(ip("10.20.3.4"), 22).is_ok());
    assert_eq!(
        fenced.check(ip("10.21.3.4"), 22),
        Err("outside_allowed_networks")
    );
    assert_eq!(
        open.check_registered("127.0.0.1", 22),
        Err("denied_address")
    );
    assert!(open.check_registered("app1.internal", 22).is_ok());
    assert_eq!(
        open.check_registered("app1.internal", 8080),
        Err("forge_port")
    );
    assert!(IpNet::parse("10.0.0.0/33").is_err());
    assert!(IpNet::parse("fd00::/8")
        .expect("v6")
        .contains(ip("fd12::1")));
}
