#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! The PFS trial deploy (ops/gitlab-trial) stays consistent with the code:
//! its forge.toml loads with the real loader for serve / worker and for
//! the migrate service (with the environment compose.yml gives each), the
//! plain-HTTP settings are what makes the trial work, and every secret file
//! forge.toml names is mounted by compose.yml and created by
//! setup-server.sh / checked by deploy.sh.

use std::path::{Path, PathBuf};

use forge_core::config::{self, Config, Env};

const KEY_LINE: &str = "1 3f1c9a7e5b2d4c6f8091a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d5e6f7\n";
const SECRETS: &str = "/run/forge/secrets/";

fn read(rel: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(rel);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
        .replace("\r\n", "\n")
}

fn write_secret(path: &Path, text: &str) {
    use std::io::Write as _;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .unwrap_or_else(|e| panic!("create {}: {e}", path.display()));
    file.write_all(text.as_bytes()).expect("write secret");
}

/// The secret file names forge.toml refers to (below /run/forge/secrets/).
fn secret_names(toml: &str) -> Vec<String> {
    toml.lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .filter_map(|l| l.split_once(SECRETS).map(|(_, rest)| rest))
        .map(|rest| rest.trim_end_matches('"').to_owned())
        .collect()
}

/// forge.toml with the container paths moved below a temporary directory
/// and every secret it names created there.
struct Installed {
    _dir: tempfile::TempDir,
    config: PathBuf,
    privileged: PathBuf,
}

impl Installed {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        let base = dir.path().to_string_lossy().replace('\\', "/");
        let toml = read("ops/gitlab-trial/forge.toml");
        let secrets = dir.path().join("secrets");
        let privileged = dir.path().join("privileged");
        for d in [&secrets, &privileged, &dir.path().join("repositories")] {
            std::fs::create_dir_all(d).expect("create dir");
        }
        for name in secret_names(&toml) {
            let text = if name.ends_with(".pw") || name == "oidc-client-secret" {
                "synthetic\n"
            } else {
                KEY_LINE
            };
            write_secret(&secrets.join(&name), text);
        }
        write_secret(&privileged.join("db-migrate.pw"), "synthetic\n");
        let text = toml.replace(SECRETS, &format!("{base}/secrets/")).replace(
            "/var/lib/forge/repositories",
            &format!("{base}/repositories"),
        );
        let config = dir.path().join("forge.toml");
        std::fs::write(&config, text).expect("write config");
        Self {
            _dir: dir,
            config,
            privileged,
        }
    }

    fn load(&self, extra: &[(&str, &str)]) -> Config {
        // What compose.yml passes from .env (synthetic, RFC 5737 addresses).
        let mut pairs = vec![
            ("FORGE_WEB_BASE_URL", "http://192.0.2.10:3306"),
            (
                "FORGE_AUTH_OIDC_ISSUER",
                "http://192.0.2.10:18180/realms/iotpf",
            ),
        ];
        pairs.extend_from_slice(extra);
        config::load(Some(&self.config), &Env::from_pairs(pairs))
            .unwrap_or_else(|e| panic!("ops/gitlab-trial/forge.toml must load: {e:?}"))
    }
}

#[test]
fn the_trial_configuration_loads_for_serve_worker_and_migrate() {
    let installed = Installed::new();
    let serve = installed.load(&[]);
    let base = serve.web.require_base_url().expect("base url");
    assert_eq!(base.origin(), "http://192.0.2.10:3306");
    assert!(base.is_plain_http(), "the trial is plain http");
    let oidc = serve.auth.oidc.as_ref().expect("auth.oidc configured");
    assert!(oidc.allow_insecure_issuer);
    assert!(serve.web.session_keys().expect("session keys").is_some());
    serve
        .database
        .reject_privileged_logins()
        .expect("serve / worker never see the migration login (ADR-0018)");

    let migrate_pw = installed.privileged.join("db-migrate.pw");
    let migrate_pw = migrate_pw.to_string_lossy();
    let migrate = installed.load(&[
        (
            "FORGE_DATABASE_MIGRATE_URL",
            "postgres://forge_migrator@db:5432/forge",
        ),
        ("FORGE_DATABASE_MIGRATE_PASSWORD_FILE", migrate_pw.as_ref()),
    ]);
    assert!(migrate
        .database
        .migrate_password()
        .expect("readable")
        .is_some());
}

#[test]
fn every_secret_is_mounted_created_and_checked() {
    let toml = read("ops/gitlab-trial/forge.toml");
    let compose = read("ops/gitlab-trial/compose.yml");
    let setup = read("ops/gitlab-trial/setup-server.sh");
    let deploy = read("ops/gitlab-trial/deploy.sh");
    let names = secret_names(&toml);
    assert!(names.len() >= 7, "{names:?}");
    assert!(compose.contains("./secrets:/run/forge/secrets:ro"));
    for name in &names {
        assert!(
            deploy.contains(&format!("secrets/{name}")),
            "deploy.sh checks {name}"
        );
        assert!(
            name == "oidc-client-secret" || setup.contains(name.as_str()),
            "setup-server.sh creates {name}"
        );
    }
    // The privileged files: created by setup, checked by deploy, mounted
    // into their one container only.
    for name in ["db-migrate.pw", "db-maint.pw", "pg-superuser.pw"] {
        assert!(setup.contains(name), "setup-server.sh creates {name}");
        assert!(
            deploy.contains(&format!("secrets-privileged/{name}")),
            "deploy.sh checks {name}"
        );
    }
    assert_eq!(
        compose.matches("./secrets-privileged/").count(),
        2,
        "only db and migrate mount a privileged file"
    );
    // Never a destructive compose command in the deploy (comments may say so).
    let code: String = deploy
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n");
    for bad in ["down", "volume rm", "prune", "dropdb", "DROP DATABASE"] {
        assert!(!code.contains(bad), "deploy.sh must not run `{bad}`");
    }
    // Pipeline variables never redirect the deploy, and every exit after
    // the stop goes through the failure handler.
    for env in ["GS_DEPLOY_DIR", "GS_KEEP_BACKUPS"] {
        assert!(!code.contains(env), "deploy.sh must not read {env}");
    }
    assert!(code.contains("trap on_failure EXIT"));
    let stop = code.find("stop serve worker").expect("the stop");
    let trap = code.find("trap on_failure EXIT").expect("the trap");
    let shipped = code.find("SHIPPED=").expect("the bootstrap version");
    assert!(
        trap < stop && shipped < stop,
        "preconditions and the trap come before the stop"
    );
    // Every service runs without new privileges and with dropped capabilities.
    assert_eq!(
        compose.matches("cap_drop: [ALL]").count(),
        2,
        "db and x-forge"
    );
    assert_eq!(
        compose.matches("no-new-privileges:true").count(),
        2,
        "db and x-forge"
    );
}

/// The runner's bash runs with `-o pipefail`: on the first build only this
/// commit's image exists, so `grep -v` of it matches nothing and exits 1,
/// which failed `build:image` after a good build (pipeline 6766).
#[test]
fn image_cleanup_tolerates_an_empty_filter_under_pipefail() {
    let ci = read(".gitlab-ci.yml");
    let line = ci
        .lines()
        .find(|l| l.contains("grep -vF \":$CI_COMMIT_SHORT_SHA\""))
        .expect("cleanup filter line");
    assert!(
        line.contains("{ grep -vF \":$CI_COMMIT_SHORT_SHA\" || true; }"),
        "the cleanup filter must not fail the job when nothing matches: {line}"
    );
}

/// Lines that are not comments (`#`), for "never runs X" checks.
fn code_lines(text: &str) -> String {
    text.lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n")
}

/// GitStation's own CI/CD on the trial server (README 「自前の CI/CD」):
/// the worker has an ssh client and a deploy key, the allowed networks
/// come from the optional .env value, and the key is generated by
/// setup-server.sh, which prints only its public half.
#[test]
fn the_worker_is_configured_to_deploy_over_ssh() {
    let installed = Installed::new();
    let serve = installed.load(&[]);
    let key = serve
        .deploy
        .ssh_key_file
        .as_ref()
        .expect("deploy.ssh_key_file is set");
    assert!(key.ends_with("secrets/deploy-ssh-key"), "{}", key.display());
    assert!(serve.deploy.allowed_networks.is_empty());
    // compose.yml passes an unset DEPLOY_ALLOWED_NETWORKS as "".
    let unset = installed.load(&[("FORGE_DEPLOY_ALLOWED_NETWORKS", "")]);
    assert!(unset.deploy.allowed_networks.is_empty());
    let one = installed.load(&[("FORGE_DEPLOY_ALLOWED_NETWORKS", "192.0.2.10/32")]);
    assert_eq!(one.deploy.allowed_networks.len(), 1);

    let compose = read("ops/gitlab-trial/compose.yml");
    assert!(compose.contains("FORGE_DEPLOY_ALLOWED_NETWORKS: ${DEPLOY_ALLOWED_NETWORKS:-}"));
    let dockerfile = read("ops/gitlab-trial/Dockerfile");
    let install = dockerfile
        .lines()
        .find(|l| l.contains("apt-get install"))
        .expect("the runtime apt-get install");
    assert!(install.contains(" openssh-client "), "{install}");

    let setup = read("ops/gitlab-trial/setup-server.sh");
    let code = code_lines(&setup);
    assert!(code.contains("ssh-keygen -q -t ed25519 -N ''"));
    assert!(code.contains("chmod 600 \"$keydir/key\""));
    assert!(code.contains("chown \"$GS_USER:$GS_USER\" \"$keydir/key\""));
    assert!(
        code.contains("mv -n \"$keydir/key\" \"$DEPLOY_KEY\""),
        "never overwritten"
    );
    assert!(
        code.contains("ssh-keygen -y -f \"$DEPLOY_KEY\""),
        "prints the public half"
    );
    for leak in [
        "cat \"$DEPLOY_KEY\"",
        "cat \"$keydir/key\"",
        "echo \"$DEPLOY_KEY",
    ] {
        assert!(
            !code.contains(leak),
            "setup-server.sh must not print the private key: {leak}"
        );
    }
}

/// The opt-in runner keeps ops/runner's model (rootless Podman inside, no
/// Docker socket, not privileged, no published port) and stays inside its
/// own names and limits on the shared server.
#[test]
fn the_trial_runner_is_bounded_and_isolated() {
    let compose = read("ops/gitlab-trial/compose.runner.yml");
    let code = code_lines(&compose);
    assert!(code.contains("name: gitstation-trial-runner\n"));
    assert!(code.contains("user: podman"));
    for bad in [
        "docker.sock",
        "ports:",
        "privileged",
        "network_mode",
        "pid:",
        "/var/run",
        "./secrets:",
        "secrets-privileged",
    ] {
        assert!(
            !code.contains(bad),
            "compose.runner.yml must not have {bad}"
        );
    }
    for limit in ["mem_limit: ", "cpus: ", "pids_limit: "] {
        assert!(code.contains(limit), "compose.runner.yml sets {limit}");
    }
    for line in code.lines().map(str::trim) {
        for prefix in ["container_name: ", "name: "] {
            if let Some(name) = line.strip_prefix(prefix) {
                assert!(name.starts_with("gitstation-trial-runner"), "{line}");
            }
        }
    }
    assert!(code.contains("./secrets-runner/token:/etc/forge-runner/token:ro"));
    // The runner is not part of the GitStation deploy.
    assert!(!read("ops/gitlab-trial/compose.yml").contains("forge-runner"));
    let runner = code_lines(&read("ops/gitlab-trial/runner.sh"));
    for bad in [
        "prune",
        " down",
        "rm -rf",
        "docker rm",
        "volume rm",
        "GS_DEPLOY_DIR",
    ] {
        assert!(!runner.contains(bad), "runner.sh must not run `{bad}`");
    }
    assert!(runner.contains("-p \"$PROJECT\""));
    assert!(runner.contains("PROJECT=gitstation-trial-runner"));
    assert!(read("ops/runner/Dockerfile").contains("ARG RUNNER_BUILD_JOBS="));
}

/// The deploy target kit: the dispatcher's allow-list and strict match, the
/// forced command in authorized_keys, the lock shared by deploy and
/// rollback, the fixed build recipe, and no real address in any file.
#[test]
fn the_deploy_target_kit_is_restricted() {
    let kit = "ops/gitlab-trial/deploy-target/";
    let dispatch = code_lines(&read(&format!("{kit}dispatch")));
    assert!(dispatch
        .contains("re='^([a-z0-9][a-z0-9._-]{0,63}) (deploy|health|rollback) ([0-9a-f]{40})$'"));
    assert!(dispatch.contains("[[ \"$cmd\" =~ $re ]]"));
    assert!(dispatch.contains("exec \"$path\" \"$verb\" \"$sha\""));
    for bad in ["eval", "bash -c", "sh -c", "set -- $"] {
        assert!(!dispatch.contains(bad), "dispatch must not use `{bad}`");
    }
    // One allow-listed script, mapped to one fixed path.
    assert_eq!(dispatch.matches(") path=").count(), 1);
    assert!(dispatch.contains("hello-app) path=\"$BASE/scripts/hello-app\""));

    let setup = code_lines(&read(&format!("{kit}setup-target.sh")));
    assert!(setup.contains("'restrict,command=\"%s/dispatch\" %s\\n'"));
    assert!(setup.contains("BASE=/opt/forge-deploy"));
    assert!(setup.contains("install -o root -g root -m 0755 \"$KIT/dispatch\""));
    assert!(setup.contains("ssh-keygen -l -f \"$HOST_KEY\""));
    for bad in ["sshd_config", "prune", "systemctl"] {
        assert!(
            !setup.contains(bad),
            "setup-target.sh must not touch `{bad}`"
        );
    }

    let app = code_lines(&read(&format!("{kit}hello-app")));
    let lock = app.find("flock -w").expect("the lock");
    let both = app
        .find("deploy|rollback)")
        .expect("deploy and rollback share a branch");
    assert!(both < lock, "deploy and rollback take the same lock");
    assert!(
        app.contains("-f \"$KIT/Dockerfile\""),
        "the kit's Dockerfile, never the commit's"
    );
    assert!(
        app.contains("archive \"$sha\" public"),
        "only public/ of the commit"
    );
    assert!(app.contains("readonly PROJECT=gs-demo-hello"));
    for bad in [
        " prune",
        "docker rm ",
        "docker system",
        "source ",
        ". \"$CONF\"",
    ] {
        assert!(!app.contains(bad), "hello-app must not run `{bad}`");
    }
    let image = read(&format!("{kit}hello-app.Dockerfile"));
    assert!(image.contains("@sha256:"), "the demo base image is pinned");

    // The demo's CI is the test-environment one with only the image and
    // the runner tags changed.
    let normalize = |s: &str| {
        code_lines(s)
            .lines()
            .filter(|l| !l.contains("image:") && !l.contains("tags:"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert_eq!(
        normalize(&read(&format!("{kit}hello-app.forge-ci.yml"))),
        normalize(&read("ops/test-env/m4-hello-app/.forge-ci.yml"))
    );

    // No real address in the new files: only 192.0.2.x (RFC 5737) and loopback.
    for file in [
        "ops/gitlab-trial/README.md",
        "ops/gitlab-trial/compose.runner.yml",
        "ops/gitlab-trial/runner.sh",
        "ops/runbooks/gitlab-trial-deploy.md",
        "ops/gitlab-trial/deploy-target/dispatch",
        "ops/gitlab-trial/deploy-target/hello-app",
        "ops/gitlab-trial/deploy-target/setup-target.sh",
        "ops/gitlab-trial/deploy-target/hello-app.compose.yml",
    ] {
        for addr in ipv4_literals(&read(file)) {
            assert!(
                addr.starts_with("192.0.2.") || addr == "127.0.0.1" || addr == "0.0.0.0",
                "{file}: {addr} is not a documentation address"
            );
        }
    }
}

/// Dotted quads in `text`.
fn ipv4_literals(text: &str) -> Vec<String> {
    text.split(|c: char| !(c.is_ascii_digit() || c == '.'))
        .map(|w| w.trim_matches('.'))
        .filter(|w| {
            let parts: Vec<_> = w.split('.').collect();
            parts.len() == 4
                && parts
                    .iter()
                    .all(|p| !p.is_empty() && p.parse::<u8>().is_ok())
        })
        .map(str::to_owned)
        .collect()
}

/// The dispatcher's argument validation, executed (Unix: bash is there).
#[cfg(unix)]
#[test]
fn the_dispatcher_accepts_only_script_verb_sha() {
    let script = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../ops/gitlab-trial/deploy-target/test-dispatch.sh");
    let out = std::process::Command::new("bash")
        .arg(&script)
        .output()
        .expect("run bash");
    assert!(
        out.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}
