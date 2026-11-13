//! The `deploy_runner` task of `forge worker` (M4 C11, ADR-0031): every
//! [`PERIOD`] one pass of `forge_deploy::run::run_once` — unapproved
//! requests past their window end `blocked`, automatic requests are made,
//! then one deployment is taken and run over SSH to its end. A pass that
//! ran or blocked a deployment runs again at once (more may wait).
//!
//! Started only with `deploy.ssh_key_file` configured. A failed pass (the
//! key: `FG-DEPLOY-0008`; the database) is returned and logged once per
//! failure streak by the supervisor with `error_code` and
//! `task=deploy_runner`; the remote failures belong to the deployment and
//! are logged by forge-deploy with `deployment_id` and `target`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use forge_core::config::DeployConfig;
use forge_db::Db;
use forge_deploy::run::{run_once, Pass, Timeouts};
use forge_deploy::transport::{DeployTransport, SshTransport};
use forge_errors::ForgeError;

use super::supervise::Next;

/// The task name in the supervisor logs.
pub(crate) const NAME: &str = "deploy_runner";
/// How often it looks for work.
pub(crate) const PERIOD: Duration = Duration::from_secs(10);

/// The deployment lane: the transport, the key and the timeouts.
#[derive(Clone)]
pub(crate) struct DeployLane {
    pub(crate) transport: Arc<dyn DeployTransport>,
    pub(crate) key_file: PathBuf,
    pub(crate) timeouts: Timeouts,
}

impl DeployLane {
    /// The lane of `deploy.*`; `None` without `deploy.ssh_key_file`.
    pub(crate) fn from_config(cfg: &DeployConfig) -> Option<Self> {
        let key_file = cfg.ssh_key_file.clone()?; // clone: the lane owns its path
        Some(Self {
            transport: Arc::new(SshTransport::new(
                cfg.ssh_program.clone(), // clone: the transport owns its path
                key_file.clone(),        // clone: as above
                cfg.connect_timeout,
            )),
            key_file,
            timeouts: Timeouts {
                script: cfg.script_timeout,
                health: cfg.health_timeout,
            },
        })
    }
}

/// One run.
pub(crate) async fn run_deploy(db: &Db, lane: &DeployLane) -> Result<Next, ForgeError> {
    match run_once(
        db,
        lane.transport.as_ref(),
        Some(&lane.key_file),
        lane.timeouts,
    )
    .await?
    {
        Pass::Idle => Ok(Next::AfterPeriod),
        Pass::Blocked(_) | Pass::Ran(..) => Ok(Next::Immediately),
    }
}

#[cfg(test)]
mod tests {
    use forge_db::test_support::FreshDb;

    use super::*;

    /// Without a key file there is no lane; with one, a pass with nothing
    /// waiting waits for the period (the key is checked first: a missing
    /// file is the run error).
    #[tokio::test]
    async fn a_deploy_run_with_nothing_waiting_waits_for_the_period() {
        assert!(DeployLane::from_config(&DeployConfig::default()).is_none());
        let t = FreshDb::migrated().await;
        let dir = tempfile::tempdir().expect("dir");
        let key = dir.path().join("deploy_key");
        let cfg = DeployConfig {
            ssh_key_file: Some(key.clone()),
            ..DeployConfig::default()
        };
        let lane = DeployLane::from_config(&cfg).expect("lane");
        let missing = run_deploy(&t.db, &lane).await.expect_err("no key file yet");
        assert_eq!(
            missing.code(),
            forge_errors::ErrorCode::DeployKeyUnavailable
        );
        std::fs::write(&key, b"x").expect("key");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).expect("chmod");
        }
        assert_eq!(
            run_deploy(&t.db, &lane).await.expect("run"),
            Next::AfterPeriod
        );
        t.drop_now().await;
    }
}
