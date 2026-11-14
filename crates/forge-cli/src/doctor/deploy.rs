//! The deployment check of `forge doctor` (M4 C11, plan 14 §6, runbook
//! deploy):
//!
//! - `deploy.known_hosts` — every target with its pinned host key
//!   fingerprint (`project/tier/target host:port SHA256:...`), the deploy
//!   key file (`FG-DEPLOY-0008` when targets exist and the key is missing
//!   or readable by others), every stored pin still parses, and no
//!   deployment `running` for more than [`STUCK_MINUTES`] (a worker that
//!   stopped mid-deployment: a warning, a person decides). A skip when no
//!   target exists and no key is configured. Never a key.

use forge_core::config::Config;
use forge_core::deploy::HostKey;
use forge_db::deploy::StoredTarget;
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};

use super::{fail, ok, skip, warn, Check};

pub(super) const NAME: &str = "deploy.known_hosts";
/// A deployment running longer than this is reported.
pub(super) const STUCK_MINUTES: i32 = 120;

/// The check outcome for the targets, the key check and the stuck
/// deployments (pure: the tests feed all three).
fn line(
    targets: &[(i64, StoredTarget)],
    key: Result<(), ForgeError>,
    key_configured: bool,
    stuck: &[i64],
) -> Check {
    if targets.is_empty() && !key_configured {
        return skip(
            NAME,
            "no deployment target and no deploy.ssh_key_file (deployments are off)",
        );
    }
    if !targets.is_empty() {
        if let Err(e) = key {
            return fail(
                NAME,
                e.with_context("targets", targets.len())
                    .with_context("runbook", "deploy"),
            );
        }
    }
    if let Some((project, t)) = targets
        .iter()
        .find(|(_, t)| HostKey::parse(&t.host_key_type, &t.host_key).is_err())
    {
        return fail(
            NAME,
            ForgeError::new(ErrorCode::HostKeyMismatch)
                .with_context("reason", "stored_pin_invalid")
                .with_context("project_id", project)
                .with_context("target", t.name.clone()) // clone: the error owns it
                .with_context("runbook", "deploy"),
        );
    }
    let pins: Vec<String> = targets
        .iter()
        .map(|(p, t)| {
            format!(
                "{p}/{}/{} {}:{} {}{}",
                t.tier.as_str(),
                t.name,
                t.host,
                t.port,
                t.host_key_fingerprint,
                if t.enabled { "" } else { " (disabled)" }
            )
        })
        .collect();
    let detail = format!("{} target(s): {}", pins.len(), pins.join("; "));
    if !stuck.is_empty() {
        return warn(
            NAME,
            &format!(
                "{detail}; deployment(s) running for more than {STUCK_MINUTES} minutes: {:?} (runbook deploy)",
                stuck
            ),
        );
    }
    ok(NAME, &detail)
}

/// The check.
pub(super) async fn known_hosts_check(db: &Db, cfg: &Config) -> Check {
    let targets = match forge_db::deploy::all_targets(db).await {
        Ok(t) => t,
        Err(e) => return fail(NAME, e),
    };
    let stuck = match forge_db::deploy::stuck_running(db, STUCK_MINUTES).await {
        Ok(s) => s,
        Err(e) => return fail(NAME, e),
    };
    let key_file = cfg.deploy.ssh_key_file.as_deref();
    line(
        &targets,
        forge_deploy::run::check_key_file(key_file),
        key_file.is_some(),
        &stuck,
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly

    use forge_core::deploy::DeployTier;

    use super::super::Outcome;
    use super::*;

    fn target(key: &str) -> StoredTarget {
        StoredTarget {
            id: 1,
            tier: DeployTier::Stg,
            name: "web-1".to_owned(),
            host: "app1.internal".to_owned(),
            port: 22,
            login_user: "deploy".to_owned(),
            host_key_type: "ssh-ed25519".to_owned(),
            host_key: key.to_owned(),
            host_key_fingerprint: "SHA256:pin".to_owned(),
            script: "web-app".to_owned(),
            enabled: true,
            updated_at: "2026-11-13T00:00:00Z".to_owned(),
        }
    }

    const KEY: &str = "AAAAC3NzaC1lZDI1NTE5AAAAIAkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJ";

    #[test]
    fn targets_are_listed_with_their_pins_and_a_missing_key_fails() {
        assert!(matches!(
            line(&[], Ok(()), false, &[]).outcome,
            Outcome::Skip(_)
        ));
        let one = [(7, target(KEY))];
        match line(&one, Ok(()), true, &[]).outcome {
            Outcome::Ok(detail) => {
                assert!(
                    detail.contains("7/stg/web-1 app1.internal:22 SHA256:pin"),
                    "{detail}"
                );
            }
            Outcome::Warn(_) | Outcome::Fail(_) | Outcome::Skip(_) => {
                panic!("expected ok, got another outcome")
            }
        }
        let no_key =
            ForgeError::new(ErrorCode::DeployKeyUnavailable).with_context("reason", "unconfigured");
        match line(&one, Err(no_key), false, &[]).outcome {
            Outcome::Fail(e) => assert_eq!(e.code(), ErrorCode::DeployKeyUnavailable),
            Outcome::Ok(_) | Outcome::Warn(_) | Outcome::Skip(_) => {
                panic!("expected a failure, got another outcome")
            }
        }
        assert!(matches!(
            line(&[(7, target("AAAA"))], Ok(()), true, &[]).outcome,
            Outcome::Fail(_)
        ));
        assert!(matches!(
            line(&one, Ok(()), true, &[42]).outcome,
            Outcome::Warn(_)
        ));
    }
}
