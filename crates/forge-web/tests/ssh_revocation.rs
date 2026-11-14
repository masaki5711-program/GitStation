#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)] // tests may fail loudly (rust/testing.md)
#![allow(clippy::wildcard_enum_match_arm)] // ChannelMsg is non-exhaustive; the test reads what it needs
//! M4 C13 review (HIGH): a live SSH connection does not outlive its key.
//! The key is decided again at every session channel and every command,
//! and the connection ends at `git.ssh.max_connection_ms`. The server is
//! the real `forge_web::ssh` server; the client is russh's; the
//! authenticator is a stub whose answer the test flips (the database's
//! answers to a deleted key and a disabled owner are forge-db's tests:
//! `ssh_keys::tests`). The router stands in for the product router.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::response::IntoResponse;
use forge_core::authenticator::AuthFuture;
use forge_core::{Authenticator, RequestId, SshKeyAuthFuture, SshKeyIdentity, UserId, UserKind};
use forge_errors::{ErrorCode, ForgeError};
use forge_web::ssh::{generate_host_key, AdmissionLimits, SshLimits, SshPublicInfo, SshServer};
use russh::keys::ssh_key::private::{Ed25519Keypair, KeypairData};
use russh::keys::{HashAlg, PrivateKey, PrivateKeyWithHashAlg};
use russh::{client, ChannelMsg};

const VALID: u8 = 0;
const DELETED: u8 = 1;
const DISABLED: u8 = 2;

/// Answers for one fingerprint as the test says.
struct Keys {
    fingerprint: String,
    state: Arc<AtomicU8>,
}

impl Authenticator for Keys {
    fn authenticate<'a>(&'a self, _: &'a RequestId, _: &'a str) -> AuthFuture<'a> {
        Box::pin(async { Err(ForgeError::new(ErrorCode::Unauthenticated)) })
    }

    fn authenticate_ssh_key<'a>(
        &'a self,
        _: &'a RequestId,
        fingerprint: &'a str,
        _record_use: bool,
    ) -> SshKeyAuthFuture<'a> {
        Box::pin(async move {
            if fingerprint != self.fingerprint {
                return Err(ForgeError::new(ErrorCode::Unauthenticated)
                    .with_context("reason", "ssh_key_unknown"));
            }
            match self.state.load(Ordering::SeqCst) {
                VALID => Ok(SshKeyIdentity {
                    key_id: 5,
                    user_id: UserId::new(7),
                    username: "alice".to_owned(),
                    kind: UserKind::Human,
                }),
                DELETED => Err(ForgeError::new(ErrorCode::Unauthenticated)
                    .with_context("reason", "ssh_key_unknown")),
                _ => Err(ForgeError::new(ErrorCode::AccountDisabled)
                    .with_context("reason", "owner_disabled")),
            }
        })
    }
}

struct Client;

impl client::Handler for Client {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        _: &russh::keys::PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }
}

struct Fixture {
    port: u16,
    key: Arc<PrivateKey>,
    state: Arc<AtomicU8>,
    _dir: tempfile::TempDir,
    _stop: tokio::sync::oneshot::Sender<()>,
}

async fn fixture(max_connection: Duration) -> Fixture {
    let dir = tempfile::tempdir().expect("tempdir");
    let host = generate_host_key(&dir.path().join("host")).expect("host key");
    let key = PrivateKey::new(
        KeypairData::Ed25519(Ed25519Keypair::from_seed(&[7u8; 32])),
        "synthetic",
    )
    .expect("client key");
    let fingerprint = key.public_key().fingerprint(HashAlg::Sha256).to_string();
    let state = Arc::new(AtomicU8::new(VALID));
    let keys = Arc::new(Keys {
        fingerprint,
        state: Arc::clone(&state),
    });
    // A stand-in for the product router: every request gets the v2
    // capability advertisement.
    let router = axum::Router::default().fallback(|| async {
        (axum::http::StatusCode::OK, &b"000eversion 2\n0000"[..]).into_response()
    });
    let server = SshServer::new(
        router,
        keys,
        &host,
        SshPublicInfo {
            clone_host: "127.0.0.1:0".to_owned(),
            host_key_fingerprints: vec![host.fingerprint().to_owned()],
        },
        SshLimits {
            admission: AdmissionLimits {
                max_connections: 8,
                max_connections_per_peer: 8,
                max_unauthenticated: 8,
                connections_per_minute_per_peer: 100,
                auth_failures_per_minute_per_peer: 100,
            },
            auth_timeout: Duration::from_secs(10),
            max_auth_attempts: 6,
            inactivity_timeout: Duration::from_secs(60),
            max_connection,
            idle_timeout: Duration::from_secs(10),
        },
    );
    let listener = forge_web::ssh::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        let stop = async move {
            let _ = rx.await; // ignore: a dropped sender also stops
        };
        forge_web::ssh::serve(listener, server, stop)
            .await
            .expect("ssh serve");
    });
    Fixture {
        port,
        key: Arc::new(key),
        state,
        _dir: dir,
        _stop: tx,
    }
}

async fn connect(f: &Fixture) -> client::Handle<Client> {
    let mut handle = client::connect(
        Arc::new(client::Config::default()),
        ("127.0.0.1", f.port),
        Client,
    )
    .await
    .expect("connect");
    let auth = handle
        .authenticate_publickey("git", PrivateKeyWithHashAlg::new(Arc::clone(&f.key), None))
        .await
        .expect("auth");
    assert!(auth.success(), "{auth:?}");
    handle
}

/// Run `command` on a fresh channel: (stdout, exit status).
async fn exec(channel: &mut russh::Channel<client::Msg>, command: &str) -> (Vec<u8>, Option<u32>) {
    channel
        .set_env(false, "GIT_PROTOCOL", "version=2")
        .await
        .expect("env");
    channel.exec(true, command).await.expect("exec");
    // The client ends its requests at once (a lone flush).
    channel.data(&b"0000"[..]).await.expect("data");
    let mut out = Vec::new();
    let mut status = None;
    while let Some(msg) = channel.wait().await {
        match msg {
            ChannelMsg::Data { data } => out.extend_from_slice(&data),
            ChannelMsg::ExitStatus { exit_status } => status = Some(exit_status),
            ChannelMsg::Close => break,
            _ => {}
        }
    }
    (out, status)
}

/// The errors of the review: a key deleted, or its owner disabled, while
/// the connection is open — the next channel is refused (and the
/// connection ends); a command on a channel opened before is refused
/// in-band with the authenticator's code.
#[tokio::test]
async fn a_revoked_key_gets_no_new_channel_and_no_new_command() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let f = fixture(Duration::from_secs(3600)).await;

        // Valid: a command runs.
        let handle = connect(&f).await;
        let mut channel = handle.channel_open_session().await.expect("channel");
        let (out, status) = exec(&mut channel, "git-upload-pack 'alice/web.git'").await;
        assert_eq!(status, Some(0), "{}", String::from_utf8_lossy(&out));
        assert!(out.starts_with(b"000eversion 2\n"));

        // Deleted while connected: the next channel is refused.
        f.state.store(DELETED, Ordering::SeqCst);
        assert!(
            handle.channel_open_session().await.is_err(),
            "a deleted key opens no channel"
        );

        // A channel opened while valid, then the owner is disabled: the
        // command is refused in-band (FG-AUTH-0004), exit status 1.
        f.state.store(VALID, Ordering::SeqCst);
        let handle = connect(&f).await;
        let mut channel = handle.channel_open_session().await.expect("channel");
        f.state.store(DISABLED, Ordering::SeqCst);
        let (out, status) = exec(&mut channel, "git-upload-pack 'alice/web.git'").await;
        assert_eq!(status, Some(1));
        let text = String::from_utf8_lossy(&out);
        assert!(text.contains("ERR FG-AUTH-0004"), "{text}");
    })
    .await
    .expect("no hang");
}

/// The connection ends at its maximum age (`git.ssh.max_connection_ms`),
/// kept alive or not.
#[tokio::test]
async fn a_connection_ends_at_its_maximum_age() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let f = fixture(Duration::from_millis(1500)).await;
        let handle = connect(&f).await;
        assert!(handle.channel_open_session().await.is_ok());
        tokio::time::sleep(Duration::from_millis(2500)).await;
        assert!(
            handle.channel_open_session().await.is_err(),
            "the aged-out connection is closed"
        );
    })
    .await
    .expect("no hang");
}
