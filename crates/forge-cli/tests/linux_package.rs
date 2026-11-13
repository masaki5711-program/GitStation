#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! The Linux production package (ops/linux) stays consistent with the code:
//! the example configuration loads with the real loader for every process
//! that uses it, the units run the packaged binary with that configuration
//! and hide the privileged credentials, and the proxy, directories and
//! PostgreSQL settings match the configured values.

use std::path::{Path, PathBuf};

use forge_core::config::{self, Config, Env};

/// A synthetic, non-degenerate key line (`<id> <64 hex>`).
const KEY_LINE: &str = "1 3f1c9a7e5b2d4c6f8091a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d5e6f7\n";
const BACKUP_KEY: &str = "9e8d7c6b5a49382716051f2e3d4c5b6a798897a6b5c4d3e2f1a0b9c8d7e6f5a4\n";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn read(rel: &str) -> String {
    let path = repo_root().join(rel);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
        .replace("\r\n", "\n")
}

/// Write a secret file readable by its owner only.
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

/// The example configuration with every absolute path moved below a
/// temporary directory, and the files it names created there.
struct Installed {
    _dir: tempfile::TempDir,
    /// The temporary root, forward slashes (what replaced the leading `/`).
    base: String,
    config: PathBuf,
}

impl Installed {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        let base = dir.path().to_string_lossy().replace('\\', "/");
        let text = read("ops/linux/etc/forge.toml.example").replace("\"/", &format!("\"{base}/"));
        let at = |p: &str| PathBuf::from(format!("{base}{p}"));
        for d in [
            "/etc/forge/secrets",
            "/etc/forge/secrets-privileged",
            "/etc/forge/ca",
            "/var/lib/forge/repositories",
            "/var/lib/forge/state-copy",
            "/srv/forge-backup",
        ] {
            std::fs::create_dir_all(at(d)).expect("create dir");
        }
        write_secret(&at("/etc/forge/secrets/db-app.pw"), "synthetic-app\n");
        write_secret(
            &at("/etc/forge/secrets/oidc-client-secret"),
            "synthetic-oidc\n",
        );
        for name in [
            "cursor-keys",
            "audit-seal-keys",
            "web-session-keys",
            "webhook-secret-keys",
            "ci-secret-keys",
        ] {
            write_secret(&at(&format!("/etc/forge/secrets/{name}")), KEY_LINE);
        }
        write_secret(&at("/etc/forge/secrets/backup.key"), BACKUP_KEY);
        write_secret(
            &at("/etc/forge/secrets-privileged/db-migrate.pw"),
            "synthetic-migrate\n",
        );
        write_secret(
            &at("/etc/forge/secrets-privileged/db-maint.pw"),
            "synthetic-maint\n",
        );
        std::fs::write(at("/etc/forge/ca/internal-ca.pem"), "synthetic\n").expect("write ca");
        let config = dir.path().join("forge.toml");
        std::fs::write(&config, text).expect("write config");
        Self {
            _dir: dir,
            base,
            config,
        }
    }

    fn load(&self, env: &Env) -> Config {
        config::load(Some(&self.config), env)
            .unwrap_or_else(|e| panic!("the example configuration must load: {e:?}"))
    }

    /// A configured path as written in the example (the temporary root removed).
    fn original(&self, path: &Path) -> String {
        let p = path.to_string_lossy().replace('\\', "/");
        p.strip_prefix(&self.base).unwrap_or(&p).to_owned()
    }
}

/// `KEY=VALUE` lines of forgectl.env.example, read as forgectl reads them.
fn forgectl_env() -> Vec<(String, String)> {
    read("ops/linux/etc/forgectl.env.example")
        .lines()
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| {
            let (k, v) = l.split_once('=').expect("KEY=VALUE");
            (k.to_owned(), v.to_owned())
        })
        .collect()
}

#[test]
fn the_example_configuration_loads_for_serve_worker_and_backup() {
    let installed = Installed::new();
    let cfg = installed.load(&Env::default());

    // serve / worker refuse the migrate and maint password files.
    cfg.database
        .reject_privileged_logins()
        .expect("no privileged password file in forge.toml");
    cfg.storage_root().expect("storage.root");
    cfg.database.url().expect("database.url");
    // The files each process reads on start.
    cfg.api.require_cursor_keys().expect("serve: cursor keys");
    cfg.web.require_base_url().expect("serve: base url");
    assert!(
        cfg.web
            .session_keys()
            .expect("serve: session keys")
            .is_some(),
        "web.session_key_file"
    );
    assert!(cfg.auth.oidc.is_some(), "auth.oidc is configured");
    cfg.audit.require_seal_keys().expect("worker: seal keys");
    assert!(cfg.ci.secret_keys().expect("ci keys").is_some());
    assert!(cfg.webhook.secret_keys().expect("webhook keys").is_some());
    cfg.backup
        .read_key()
        .expect("backup.key_file is configured")
        .expect("backup: key");
}

#[test]
fn the_server_port_avoids_the_defaults_pfs_is_likely_to_hold() {
    let cfg = Installed::new().load(&Env::default());
    let (host, port) = cfg.server.bind.rsplit_once(':').expect("host:port");
    assert_eq!(host, "127.0.0.1", "loopback only: TLS is the proxy's job");
    for taken in ["8080", "3000", "5432", "443", "80"] {
        assert_ne!(port, taken, "server.bind must not use {taken}");
    }
}

#[test]
fn forgectl_adds_the_privileged_logins_through_the_environment() {
    let installed = Installed::new();
    let mut pairs = forgectl_env();
    let keys: Vec<&str> = pairs.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(
        keys,
        ["FORGE_DATABASE_MIGRATE_URL", "FORGE_DATABASE_MAINT_URL"],
        "forgectl accepts exactly these keys"
    );
    let staged = |name: &str| format!("{}/etc/forge/secrets-privileged/{name}", installed.base);
    pairs.push((
        "FORGE_DATABASE_MIGRATE_PASSWORD_FILE".into(),
        staged("db-migrate.pw"),
    ));
    pairs.push((
        "FORGE_DATABASE_MAINT_PASSWORD_FILE".into(),
        staged("db-maint.pw"),
    ));
    let env = Env::from_pairs(pairs.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    let cfg = installed.load(&env);

    cfg.database.migrate_url().expect("migrate url");
    assert!(cfg.database.migrate_password().expect("read").is_some());
    cfg.database.maint_url().expect("maint url");
    assert!(cfg.database.maint_password().expect("read").is_some());
    // The same environment would stop serve / worker: forgectl must never
    // hand it to the services.
    assert!(cfg.database.reject_privileged_logins().is_err());
}

fn directive<'a>(unit: &'a str, name: &str) -> Vec<&'a str> {
    let prefix = format!("{name}=");
    unit.lines()
        .filter_map(|l| l.strip_prefix(prefix.as_str()))
        .collect()
}

#[test]
fn the_units_run_the_packaged_binary_and_hide_the_privileged_credentials() {
    let installed = Installed::new();
    let cfg = installed.load(&Env::default());
    let storage = installed.original(cfg.storage_root().expect("root"));
    let state_copy = installed.original(cfg.backup.state_copy_dir.as_deref().expect("state copy"));
    let backup_dir = installed.original(cfg.backup.dir.as_deref().expect("backup dir"));

    for (file, sub) in [
        ("forge-serve.service", "serve"),
        ("forge-worker.service", "worker"),
        ("forge-backup.service", "backup"),
    ] {
        let unit = read(&format!("ops/linux/systemd/{file}"));
        assert_eq!(
            directive(&unit, "ExecStart"),
            [format!(
                "/opt/gitstation/current/bin/forge --config /etc/forge/forge.toml {sub}"
            )],
            "{file}"
        );
        assert_eq!(directive(&unit, "User"), ["forge"], "{file}");
        assert_eq!(directive(&unit, "NoNewPrivileges"), ["yes"], "{file}");
        assert_eq!(directive(&unit, "ProtectSystem"), ["strict"], "{file}");
        assert_eq!(directive(&unit, "PrivateTmp"), ["yes"], "{file}");
        let hidden = directive(&unit, "InaccessiblePaths").join(" ");
        for path in ["/etc/forge/secrets-privileged", "/run/forge-admin"] {
            assert!(hidden.contains(path), "{file} must hide {path}");
        }
        let writable = directive(&unit, "ReadWritePaths").join(" ");
        for path in [&storage, &state_copy] {
            assert!(
                writable
                    .split_whitespace()
                    .any(|w| path.starts_with(w.trim_start_matches('-'))),
                "{file}: {path} must be writable"
            );
        }
        if sub == "backup" {
            assert!(
                writable
                    .split_whitespace()
                    .any(|w| w.trim_start_matches('-') == backup_dir),
                "{file}: backup.dir {backup_dir} must be writable"
            );
        }
    }

    // forgectl runs the same binary and configuration, and stages into the
    // directory the units hide.
    let forgectl = read("ops/linux/bin/forgectl");
    for needle in [
        "/opt/gitstation/current/bin/forge",
        "/etc/forge/forge.toml",
        "/etc/forge/secrets-privileged",
        "/run/forge-admin",
    ] {
        assert!(forgectl.contains(needle), "forgectl: {needle}");
    }

    // tmpfiles creates every directory the configuration names.
    let tmpfiles = read("ops/linux/systemd/forge.tmpfiles");
    let created: Vec<(&str, &str, &str)> = tmpfiles
        .lines()
        .filter(|l| l.starts_with("d "))
        .map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            (f[1], f[2], f[3])
        })
        .collect();
    let mode = |path: &str| {
        created
            .iter()
            .find(|(p, _, _)| *p == path)
            .map(|(_, m, owner)| (*m, *owner))
            .unwrap_or_else(|| panic!("tmpfiles must create {path}"))
    };
    assert_eq!(mode(&storage).1, "forge");
    assert_eq!(mode(&state_copy), ("0700", "forge"));
    assert_eq!(mode(&backup_dir), ("0700", "forge"));
    assert_eq!(mode("/etc/forge/secrets"), ("0700", "forge"));
    assert_eq!(mode("/etc/forge/secrets-privileged"), ("0700", "root"));
}

/// The first number in `text` after `key` (e.g. `client_max_body_size 2100m`).
fn number_after(text: &str, key: &str) -> u64 {
    let line = text
        .lines()
        .map(str::trim_start)
        .find(|l| l.starts_with(key))
        .unwrap_or_else(|| panic!("{key} missing"));
    line[key.len()..]
        .trim_start_matches([' ', '='])
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .unwrap_or_else(|_| panic!("{key}: no number in {line:?}"))
}

#[test]
fn the_proxy_carries_the_configured_push_size_and_timeout_to_the_server() {
    let cfg = Installed::new().load(&Env::default());
    let nginx = read("ops/linux/nginx/gitstation.conf");
    let upstream = nginx
        .lines()
        .map(str::trim)
        .find_map(|l| l.strip_prefix("server ").filter(|s| s.contains(':')))
        .expect("upstream server");
    assert!(
        upstream.starts_with(&format!("{};", cfg.server.bind)),
        "nginx upstream {upstream:?} must be server.bind {}",
        cfg.server.bind
    );
    let body_mib = number_after(&nginx, "client_max_body_size");
    assert!(
        body_mib * 1024 * 1024 >= u64::from(cfg.git_http.push_max_bytes),
        "client_max_body_size {body_mib}m below git.http.push_max_bytes"
    );
    for key in ["proxy_read_timeout", "proxy_send_timeout", "send_timeout"] {
        assert!(
            number_after(&nginx, key) >= cfg.git_http.timeout.as_secs(),
            "{key} below git.http.timeout_ms"
        );
    }
    assert!(nginx.contains("proxy_request_buffering off;"));
    assert!(nginx.contains("proxy_buffering off;"));
}

#[test]
fn the_dedicated_postgresql_fits_the_configured_pools() {
    let cfg = Installed::new().load(&Env::default());
    let conf = read("ops/linux/postgresql/forge-postgresql.conf");
    let max = number_after(&conf, "max_connections");
    let reserved = number_after(&conf, "superuser_reserved_connections");
    let usable = u32::try_from(max - reserved).expect("small");
    let budget = forge_db::bootstrap::ConnectionBudget::new(&cfg.database, usable);
    assert!(
        budget.fits(),
        "forge doctor db.connections: needs {} of {usable}",
        budget.needed
    );
    let compose = read("ops/linux/postgresql/compose.yml");
    assert!(
        compose.contains(&format!("max_connections={max}")),
        "compose.yml and forge-postgresql.conf agree on max_connections"
    );
    // forge.toml and forgectl.env point at the same server.
    // `host:port/dbname` (the Display of a database URL).
    let target = cfg.database.url().expect("url").to_string();
    for (_, url) in forgectl_env() {
        let host_port_db = url.rsplit_once('@').expect("user@host").1;
        assert_eq!(host_port_db, target, "forgectl.env {url} vs database.url");
    }
    assert!(
        target.ends_with(&format!(":{}/forge", number_after(&conf, "port"))),
        "database.url {target} vs the dedicated instance's port"
    );
}
