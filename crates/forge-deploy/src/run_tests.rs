use std::sync::Mutex;
use std::time::Duration;

use forge_core::deploy::{DeployTier, DeploymentStatus, RemoteVerb};
use forge_core::RequestId;
use forge_db::deploy::{Claimed, RunTarget};
use forge_errors::ErrorCode;

use super::*;
use crate::transport::{CallFuture, DeployTransport, Outcome, RemoteCall};

/// A transport that answers from a script: `(target, verb) -> outcome`
/// (anything unlisted succeeds) and records every call.
pub(crate) struct Fake {
    pub answers: Vec<(&'static str, RemoteVerb, Outcome)>,
    pub calls: Mutex<Vec<(String, &'static str, String)>>,
}

impl Fake {
    pub(crate) fn new(answers: Vec<(&'static str, RemoteVerb, Outcome)>) -> Self {
        Self {
            answers,
            calls: Mutex::new(Vec::new()),
        }
    }

    pub(crate) fn calls(&self) -> Vec<(String, &'static str, String)> {
        self.calls.lock().expect("calls").clone()
    }
}

impl DeployTransport for Fake {
    fn call<'a>(&'a self, call: &'a RemoteCall) -> CallFuture<'a> {
        self.calls.lock().expect("calls").push((
            call.target.clone(),
            call.verb.as_str(),
            call.sha.clone(),
        ));
        let answer = self
            .answers
            .iter()
            // A scripted failure hits the deployed SHA (and any rollback);
            // the health check after a rollback (the previous SHA) passes.
            .filter(|_| call.verb == RemoteVerb::Rollback || call.sha == "b".repeat(40))
            .find(|(t, v, _)| *t == call.target && *v == call.verb)
            .map_or(Outcome::Ok, |(_, _, o)| o.clone());
        Box::pin(async move { answer })
    }
}

fn target(name: &str) -> RunTarget {
    RunTarget {
        name: name.to_owned(),
        host: "app1.internal".to_owned(),
        port: 22,
        login_user: "deploy".to_owned(),
        host_key_type: "ssh-ed25519".to_owned(),
        host_key: "AAAAC3NzaC1lZDI1NTE5AAAAIAkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJ".to_owned(),
        host_key_fingerprint: "SHA256:x".to_owned(),
        script: "web-app".to_owned(),
    }
}

fn claimed(tier: DeployTier, auto_rollback: bool, previous: Option<&str>) -> Claimed {
    Claimed {
        id: 7,
        project_id: 3,
        tier,
        sha: "b".repeat(40),
        previous_sha: previous.map(str::to_owned),
        auto_rollback,
        targets: vec![target("web-1"), target("web-2")],
    }
}

const T: Timeouts = Timeouts {
    script: Duration::from_secs(5),
    health: Duration::from_secs(5),
};

fn prev() -> String {
    "a".repeat(40)
}

async fn run(fake: &Fake, c: &Claimed) -> forge_db::deploy::Finish {
    execute(fake, c, T, &RequestId::generate()).await
}

#[tokio::test]
async fn a_good_deployment_deploys_every_target_then_checks_every_target() {
    let fake = Fake::new(vec![]);
    let end = run(&fake, &claimed(DeployTier::Stg, true, Some(&prev()))).await;
    assert_eq!(end.status, DeploymentStatus::Succeeded);
    assert_eq!(end.error_code, None);
    let order: Vec<(String, &str)> = fake.calls().into_iter().map(|c| (c.0, c.1)).collect();
    assert_eq!(
        order,
        vec![
            ("web-1".to_owned(), "deploy"),
            ("web-2".to_owned(), "deploy"),
            ("web-1".to_owned(), "health"),
            ("web-2".to_owned(), "health"),
        ]
    );
}

#[tokio::test]
async fn a_failed_script_on_stg_rolls_back() {
    let fake = Fake::new(vec![("web-1", RemoteVerb::Deploy, Outcome::Exit(Some(1)))]);
    let end = run(&fake, &claimed(DeployTier::Stg, true, Some(&prev()))).await;
    assert_eq!(end.status, DeploymentStatus::RolledBack);
    assert_eq!(end.error_code, Some(ErrorCode::DeployScriptFailed));
    assert_eq!(end.failed_target.as_deref(), Some("web-1"));
    assert_eq!(
        fake.calls(),
        vec![
            ("web-1".to_owned(), "deploy", "b".repeat(40)),
            ("web-1".to_owned(), "rollback", prev()),
            ("web-1".to_owned(), "health", prev()),
        ],
        "web-2 is never touched; web-1 goes back to the previous version"
    );
}

#[tokio::test]
async fn a_failed_health_check_on_prod_fails_without_rollback() {
    let fake = Fake::new(vec![("web-2", RemoteVerb::Health, Outcome::Exit(Some(1)))]);
    let end = run(&fake, &claimed(DeployTier::Prod, false, Some(&prev()))).await;
    assert_eq!(end.status, DeploymentStatus::Failed);
    assert_eq!(end.error_code, Some(ErrorCode::HealthCheckFailed));
    assert!(
        fake.calls().iter().all(|c| c.1 != "rollback"),
        "a person decides"
    );
    // On stg the same failure rolls every target back.
    let fake = Fake::new(vec![("web-2", RemoteVerb::Health, Outcome::Timeout)]);
    let end = run(&fake, &claimed(DeployTier::Stg, true, Some(&prev()))).await;
    assert_eq!(end.status, DeploymentStatus::RolledBack);
    assert_eq!(end.error_code, Some(ErrorCode::HealthCheckFailed));
    let rolled: Vec<String> = fake
        .calls()
        .into_iter()
        .filter(|c| c.1 == "rollback")
        .map(|c| c.0)
        .collect();
    assert_eq!(rolled, vec!["web-1".to_owned(), "web-2".to_owned()]);
}

#[tokio::test]
async fn a_host_key_mismatch_stops_without_retry_or_rollback() {
    let fake = Fake::new(vec![(
        "web-2",
        RemoteVerb::Deploy,
        Outcome::HostKeyMismatch,
    )]);
    let end = run(&fake, &claimed(DeployTier::Stg, true, Some(&prev()))).await;
    assert_eq!(end.status, DeploymentStatus::Failed);
    assert_eq!(end.error_code, Some(ErrorCode::HostKeyMismatch));
    assert_eq!(end.failed_target.as_deref(), Some("web-2"));
    assert_eq!(
        fake.calls()
            .iter()
            .map(|c| (c.0.as_str(), c.1))
            .collect::<Vec<_>>(),
        vec![("web-1", "deploy"), ("web-2", "deploy")],
        "one call to the mismatching host, no retry, no rollback"
    );
}

#[tokio::test]
async fn an_unreachable_target_fails_the_deployment() {
    let fake = Fake::new(vec![(
        "web-1",
        RemoteVerb::Deploy,
        Outcome::Unreachable("connect"),
    )]);
    let end = run(&fake, &claimed(DeployTier::Stg, true, Some(&prev()))).await;
    assert_eq!(
        end.status,
        DeploymentStatus::Failed,
        "nothing changed: nothing to roll back"
    );
    assert_eq!(end.error_code, Some(ErrorCode::DeployTargetUnreachable));
    assert_eq!(fake.calls().len(), 1);
}

#[tokio::test]
async fn a_failed_rollback_is_rollback_failed() {
    let fake = Fake::new(vec![
        ("web-2", RemoteVerb::Health, Outcome::Exit(Some(2))),
        ("web-1", RemoteVerb::Rollback, Outcome::Exit(Some(1))),
    ]);
    let end = run(&fake, &claimed(DeployTier::Stg, true, Some(&prev()))).await;
    assert_eq!(end.status, DeploymentStatus::RollbackFailed);
    assert_eq!(end.error_code, Some(ErrorCode::RollbackFailed));
    assert_eq!(end.failed_target.as_deref(), Some("web-1"));
    // The first deployment of an environment has nothing to go back to.
    let fake = Fake::new(vec![("web-1", RemoteVerb::Deploy, Outcome::Exit(Some(1)))]);
    let end = run(&fake, &claimed(DeployTier::Stg, true, None)).await;
    assert_eq!(end.status, DeploymentStatus::RollbackFailed);
    assert!(fake.calls().iter().all(|c| c.1 != "rollback"));
}

#[tokio::test]
async fn a_stored_value_that_could_reach_a_shell_is_never_called() {
    let mut c = claimed(DeployTier::Stg, false, None);
    c.targets[0].script = "app;reboot".to_owned();
    let fake = Fake::new(vec![]);
    let end = run(&fake, &c).await;
    assert_eq!(end.status, DeploymentStatus::Failed);
    assert!(fake.calls().is_empty());
}

#[test]
fn an_insecure_key_file_is_refused() {
    let reason = |p: Option<&std::path::Path>| {
        let e = check_key_file(p).expect_err("refused");
        assert_eq!(e.code(), ErrorCode::DeployKeyUnavailable);
        e.to_string()
    };
    assert!(reason(None).contains("unconfigured"));
    let dir = tempfile::tempdir().expect("dir");
    assert!(reason(Some(dir.path())).contains("not_a_file"));
    assert!(reason(Some(&dir.path().join("missing"))).contains("unreadable"));
    let key = dir.path().join("deploy_key");
    std::fs::write(&key, b"not read").expect("key");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        assert!(reason(Some(&key)).contains("insecure_permissions"));
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    }
    check_key_file(Some(&key)).expect("a private key file is accepted");
}

#[tokio::test]
async fn a_lost_session_counts_the_target_as_changed() {
    // ssh exited 255 without proof that the session never opened: the
    // script may have run, so stg rolls web-1 back too.
    let fake = Fake::new(vec![("web-1", RemoteVerb::Deploy, Outcome::Lost)]);
    let end = run(&fake, &claimed(DeployTier::Stg, true, Some(&prev()))).await;
    assert_eq!(end.status, DeploymentStatus::RolledBack);
    assert_eq!(end.error_code, Some(ErrorCode::DeployTargetUnreachable));
    assert!(fake
        .calls()
        .iter()
        .any(|c| c.0 == "web-1" && c.1 == "rollback"));
    // A refused address never ran anything: nothing to roll back.
    let fake = Fake::new(vec![(
        "web-1",
        RemoteVerb::Deploy,
        Outcome::NotAllowed("denied_address"),
    )]);
    let end = run(&fake, &claimed(DeployTier::Stg, true, Some(&prev()))).await;
    assert_eq!(end.status, DeploymentStatus::Failed);
    assert_eq!(end.error_code, Some(ErrorCode::DeployTargetNotAllowed));
}
