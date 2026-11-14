//! The SSH git transport (M4 C13, ADR-0032): `forge serve`'s in-process
//! SSH server (russh, the ring backend), for `git clone` / `fetch` /
//! `push` only. Off unless `git.ssh.enabled`.
//!
//! - Authentication: public keys only, against the people's keys
//!   (`forge.user_ssh_keys`, by SHA-256 fingerprint; the owner must be
//!   active). The principal is `Principal::ssh_key`: the
//!   `read_repository` + `write_repository` scopes, no administrator right.
//! - Commands: `git-upload-pack` / `git-receive-pack` with one repository
//!   path ([`command`]); served by the Smart HTTP routes themselves, in
//!   process ([`bridge`]): one authorization point, one push path.
//! - Bounds ([`limits`]): connections in total and per peer, new
//!   connections and failed authentications per peer per minute
//!   (`FG-GIT-0035`); the SSH id exchange and authentication within
//!   `git.ssh.auth_timeout_ms`; `git.ssh.inactivity_timeout_ms`; at most
//!   `git.ssh.max_auth_attempts` keys per connection; the git streams'
//!   own caps and timeouts (`git.http.*`) apply unchanged.
//! - The host key ([`host_key`]): Ed25519, made once by `forge admin
//!   ssh-host-key generate` (never by `forge serve`), its fingerprint shown
//!   on the SSH keys page for the first connection's check.

pub mod command;
pub mod host_key;
pub mod limits;

mod bridge;
mod handler;

use std::borrow::Cow;
use std::future::Future;
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use forge_core::config::GitSshConfig;
use forge_core::{telemetry, Authenticator, RequestId};
use forge_errors::{ErrorCode, ForgeError};
use russh::keys::PrivateKey;
use russh::server::Config;
use russh::{MethodKind, MethodSet, SshId};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

pub use host_key::{generate_host_key, load_host_key, HostKey};
pub use limits::AdmissionLimits;

/// How long a shutdown waits for running SSH connections.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);

/// What clients see of the SSH server (the SSH keys page).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshPublicInfo {
    /// `host:port` of clone URLs.
    pub clone_host: String,
    /// `SHA256:…` of each host key.
    pub host_key_fingerprints: Vec<String>,
}

/// The bounds of the SSH server (`git.ssh.*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SshLimits {
    pub admission: AdmissionLimits,
    pub auth_timeout: Duration,
    pub max_auth_attempts: usize,
    pub inactivity_timeout: Duration,
}

impl SshLimits {
    pub fn from_config(cfg: &GitSshConfig) -> Self {
        let usize_of = |n: u32| usize::try_from(n).unwrap_or(usize::MAX);
        Self {
            admission: AdmissionLimits {
                max_connections: usize_of(cfg.max_connections),
                max_connections_per_peer: usize_of(cfg.max_connections_per_peer),
                connections_per_minute_per_peer: cfg.connections_per_minute_per_peer,
                auth_failures_per_minute_per_peer: cfg.auth_failures_per_minute_per_peer,
            },
            auth_timeout: cfg.auth_timeout,
            max_auth_attempts: usize_of(cfg.max_auth_attempts),
            inactivity_timeout: cfg.inactivity_timeout,
        }
    }
}

/// What every connection shares.
pub(crate) struct Shared {
    router: Router,
    authenticator: Arc<dyn Authenticator>,
    public: SshPublicInfo,
}

/// The SSH server: the product router (the same one `forge serve` serves
/// HTTP with), the authenticator, the host key and the bounds.
pub struct SshServer {
    shared: Arc<Shared>,
    config: Arc<Config>,
    limits: SshLimits,
}

impl SshServer {
    pub fn new(
        router: Router,
        authenticator: Arc<dyn Authenticator>,
        host_key: &HostKey,
        public: SshPublicInfo,
        limits: SshLimits,
    ) -> Self {
        let config = Config {
            server_id: SshId::Standard(Cow::Borrowed("SSH-2.0-GitStation")),
            methods: MethodSet::from(&[MethodKind::PublicKey][..]),
            auth_rejection_time: Duration::from_millis(500),
            // OpenSSH probes with `none` first: answer that at once.
            auth_rejection_time_initial: Some(Duration::ZERO),
            keys: vec![host_key.private().clone()], // clone: the config owns its keys
            max_auth_attempts: limits.max_auth_attempts,
            inactivity_timeout: Some(limits.inactivity_timeout),
            keepalive_interval: Some(Duration::from_secs(30)),
            keepalive_max: 3,
            nodelay: true,
            ..Config::default()
        };
        Self {
            shared: Arc::new(Shared {
                router,
                authenticator,
                public,
            }),
            config: Arc::new(config),
            limits,
        }
    }
}

/// Bind the SSH listener (`git.ssh.bind`); a failure is `FG-SYS-0004`
/// with `bind` and `listener=ssh` (runbook server-start-failed).
pub async fn bind(addr: &str) -> Result<TcpListener, ForgeError> {
    TcpListener::bind(addr).await.map_err(|e| {
        ForgeError::new(ErrorCode::ServerBindFailed)
            .with_context("bind", addr)
            .with_context("listener", "ssh")
            .with_source(e)
    })
}

fn failed(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::SshTransportFailed).with_context("reason", reason)
}

/// Serve SSH until `shutdown` resolves; then stop accepting, ask every
/// connection to close and wait for them up to [`SHUTDOWN_GRACE`].
pub async fn serve(
    listener: TcpListener,
    server: SshServer,
    shutdown: impl Future<Output = ()> + Send,
) -> Result<(), ForgeError> {
    let admission = limits::Admission::new(server.limits.admission);
    let (stop, _) = watch::channel(false);
    let mut shutdown = std::pin::pin!(shutdown);
    loop {
        let accepted = tokio::select! {
            () = &mut shutdown => break,
            accepted = listener.accept() => accepted,
        };
        let (stream, peer) = match accepted {
            Ok(accepted) => accepted,
            Err(e) => {
                telemetry::log_failure(&failed("accept").with_source(e), &RequestId::generate());
                tokio::select! {
                    () = &mut shutdown => break,
                    () = tokio::time::sleep(Duration::from_millis(100)) => continue,
                }
            }
        };
        let ip = crate::server::peer_key(peer.ip());
        match admission.admit(ip, Instant::now()) {
            Ok(admitted) => {
                tokio::spawn(run_connection(
                    stream,
                    peer.ip().to_canonical(),
                    Arc::clone(&server.shared),
                    Arc::clone(&server.config),
                    server.limits,
                    stop.subscribe(),
                    admitted,
                ));
            }
            Err(refusal) => {
                admission.log_refusal(&refusal, ip, Instant::now());
                drop(stream);
            }
        }
    }
    drop(listener);
    stop.send_replace(true);
    let deadline = tokio::time::Instant::now() + SHUTDOWN_GRACE;
    while admission.open() > 0 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Ok(())
}

/// One admitted connection until it ends.
async fn run_connection(
    stream: TcpStream,
    peer: IpAddr,
    shared: Arc<Shared>,
    config: Arc<Config>,
    limits: SshLimits,
    mut stop: watch::Receiver<bool>,
    admitted: limits::Admitted,
) {
    let authenticated = Arc::new(AtomicBool::new(false));
    let handler = handler::Handler::new(shared, peer, Arc::clone(&authenticated));
    let auth_deadline = tokio::time::Instant::now() + limits.auth_timeout;
    // The id exchange: bounded like authentication (a silent client
    // otherwise holds its slot).
    let started = tokio::time::timeout_at(
        auth_deadline,
        russh::server::run_stream(config, stream, handler),
    )
    .await;
    let session = match started {
        Ok(Ok(session)) => session,
        Ok(Err(e)) => {
            tracing::debug!(target: "forge_web::ssh", error = %e, "forge.ssh.handshake_failed");
            admitted.auth_failed();
            return;
        }
        Err(_) => {
            auth_timed_out(&limits);
            admitted.auth_failed();
            return;
        }
    };
    let handle = session.handle();
    let mut session = std::pin::pin!(session);
    let mut auth_timer = std::pin::pin!(tokio::time::sleep_until(auth_deadline));
    let mut timer_armed = true;
    let ended = loop {
        tokio::select! {
            ended = &mut session => break ended,
            () = &mut auth_timer, if timer_armed => {
                timer_armed = false;
                if !authenticated.load(Ordering::SeqCst) {
                    auth_timed_out(&limits);
                    let _ = handle // ignore: the session may be ending already
                        .disconnect(russh::Disconnect::ByApplication, "authentication timeout".into(), String::new())
                        .await;
                }
            }
            changed = stop.changed() => {
                if changed.is_err() || *stop.borrow() {
                    let _ = handle // ignore: the session may be ending already
                        .disconnect(russh::Disconnect::ByApplication, "server shutting down".into(), String::new())
                        .await;
                }
            }
        }
    };
    let was_authenticated = authenticated.load(Ordering::SeqCst);
    if !was_authenticated {
        admitted.auth_failed();
    }
    if let Err(e) = ended {
        // A client that leaves is not a failure; an internal error after
        // authentication is.
        if was_authenticated && !client_side(&e) {
            telemetry::log_failure(&failed("session").with_source(e), &RequestId::generate());
        } else {
            tracing::debug!(target: "forge_web::ssh", error = %e, "forge.ssh.session_ended");
        }
    }
}

fn auth_timed_out(limits: &SshLimits) {
    let err = ForgeError::new(ErrorCode::SshConnectionRefused)
        .with_context("reason", "auth_timeout")
        .with_context("timeout_ms", limits.auth_timeout.as_millis());
    // Info, not the code's warn: an idle client is routine (scanners).
    tracing::info!(
        target: "forge_web::ssh",
        error_code = err.code().id(),
        context = "reason=auth_timeout",
        "forge.ssh.auth_timeout"
    );
}

/// Errors that mean the client went away or broke the protocol.
fn client_side(e: &russh::Error) -> bool {
    matches!(
        e,
        russh::Error::Disconnect
            | russh::Error::IO(_)
            | russh::Error::HUP
            | russh::Error::ConnectionTimeout
            | russh::Error::InactivityTimeout
            | russh::Error::KeepaliveTimeout
    )
}

/// The private key's public half's fingerprint (the SSH keys page).
pub(crate) fn fingerprint_of(key: &PrivateKey) -> String {
    key.public_key()
        .fingerprint(russh::keys::HashAlg::Sha256)
        .to_string()
}
