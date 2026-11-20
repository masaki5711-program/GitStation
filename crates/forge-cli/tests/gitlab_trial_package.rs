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
