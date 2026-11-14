//! One SSH connection's handler (M4 C13, ADR-0033 §2–§3): public-key
//! authentication against the people's keys (`Authenticator::
//! authenticate_ssh_key`), one session channel at a time, an `exec` of a
//! git command (anything else refused), and the command itself run by a
//! task that owns the channel ([`super::bridge`]).
//!
//! Requests and their answers: `env` — only `GIT_PROTOCOL` is accepted;
//! `exec` — always accepted, the command is judged in-band (git prints
//! `fatal: remote error: …`); `shell` — accepted, prints who the key
//! belongs to and ends with exit status 1 (`ssh -T git@host` checks a
//! key); `pty-req`, `subsystem`, X11, agent and port forwarding — refused.
//! Password, keyboard-interactive and `none` authentication are never
//! offered.
//!
//! The key is decided again (`record_use = false`) at every session
//! channel and every command, so a key deleted or an account disabled
//! after the connection authenticated stops working at its next channel
//! or command; the connection itself ends at `git.ssh.max_connection_ms`
//! (`super::run_connection`). A channel that sends no command within
//! [`EXEC_WAIT`] is closed.

use std::net::IpAddr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use forge_auth::{AccountKind, Principal};
use forge_core::ssh_key::{fingerprint_of_blob, SshKeyAlgorithm};
use forge_core::{telemetry, RequestId, SshKeyIdentity, UserKind};
use forge_errors::{ErrorCode, ForgeError};
use russh::keys::ssh_encoding::Encode as _;
use russh::keys::PublicKey;
use russh::server::{Auth, ChannelOpenHandle, Msg, PublicKeyOffer, Session, VerifiedPublicKey};
use russh::{Channel, ChannelId, ChannelMsg};
use tokio::io::AsyncWriteExt as _;

use super::bridge::{self, Caller};
use super::command::parse_command;
use super::limits::Admitted;
use super::Shared;

/// How long a session channel may wait for its `exec` (or `shell`).
pub(crate) const EXEC_WAIT: Duration = Duration::from_secs(30);

/// Session channels one connection may hold at once (git uses one; an
/// OpenSSH control master may share a connection).
const MAX_CHANNELS: usize = 4;

/// One connection.
pub(crate) struct Handler {
    shared: Arc<Shared>,
    peer: IpAddr,
    /// The proven key (its fingerprint) and who it stood for then.
    identity: Option<(SshKeyIdentity, String)>,
    /// The connection's admission: marked authenticated once a key was
    /// proven (the auth timeout, the unauthenticated cap and the failure
    /// count read it).
    admitted: Arc<Admitted>,
    channels: Arc<std::sync::atomic::AtomicUsize>,
}

impl Handler {
    pub(crate) fn new(shared: Arc<Shared>, peer: IpAddr, admitted: Arc<Admitted>) -> Self {
        Self {
            shared,
            peer,
            identity: None,
            admitted,
            channels: Arc::default(),
        }
    }

    /// The fingerprint of an accepted key type (`None`: a type forge
    /// refuses — RSA, DSA, security keys, certificates).
    fn accepted_fingerprint(key: &PublicKey) -> Option<String> {
        let name = key.algorithm().as_str().to_owned();
        SshKeyAlgorithm::parse(&name)?;
        let mut blob = Vec::new();
        key.key_data().encode(&mut blob).ok()?;
        Some(fingerprint_of_blob(&blob))
    }

    /// Look the key up; a refusal is logged here (once per attempt, at the
    /// code's level) and becomes `Reject`.
    async fn lookup(&self, key: &PublicKey, proven: bool) -> Option<(SshKeyIdentity, String)> {
        let request_id = RequestId::generate();
        let Some(fingerprint) = Self::accepted_fingerprint(key) else {
            tracing::info!(
                target: "forge_web::ssh",
                request_id = %request_id,
                algorithm = key.algorithm().as_str(),
                "forge.ssh.key_type_refused"
            );
            return None;
        };
        match self
            .shared
            .authenticator
            .authenticate_ssh_key(&request_id, &fingerprint, proven)
            .await
        {
            Ok(identity) => Some((identity, fingerprint)),
            // An unknown key, or a disabled owner's, is normal while a client
            // tries its keys: only the proven attempt is worth a line.
            Err(e)
                if !proven
                    && matches!(
                        e.code(),
                        ErrorCode::Unauthenticated | ErrorCode::AccountDisabled
                    ) =>
            {
                None
            }
            Err(e) => {
                telemetry::log_failure(&e.with_context("transport", "ssh"), &request_id);
                None
            }
        }
    }
}

fn account_kind(kind: UserKind) -> AccountKind {
    match kind {
        UserKind::Human => AccountKind::Human,
        UserKind::ServiceAccount => AccountKind::ServiceAccount,
        UserKind::Bot => AccountKind::Bot,
    }
}

impl russh::server::Handler for Handler {
    type Error = russh::Error;

    async fn auth_publickey_offered(
        &mut self,
        _user: &str,
        offer: PublicKeyOffer<'_>,
    ) -> Result<Auth, Self::Error> {
        Ok(match self.lookup(offer.public_key, false).await {
            Some(_) => Auth::Accept,
            None => Auth::reject(),
        })
    }

    async fn auth_publickey(
        &mut self,
        _user: &str,
        key: VerifiedPublicKey<'_>,
    ) -> Result<Auth, Self::Error> {
        let Some((identity, fingerprint)) = self.lookup(key.public_key, true).await else {
            return Ok(Auth::reject());
        };
        tracing::info!(
            target: "forge_web::ssh",
            ssh_key_id = identity.key_id,
            user_id = identity.user_id.get(),
            "forge.ssh.authenticated"
        );
        self.identity = Some((identity, fingerprint));
        self.admitted.authenticated();
        Ok(Auth::Accept)
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: ChannelOpenHandle,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let Some((identity, fingerprint)) = self.identity.clone() else {
            // Dropping the handle refuses the channel.
            return Ok(());
        };
        // The key is decided again: deleted, or its owner disabled, since
        // the connection authenticated: no channel, and the connection ends.
        let request_id = RequestId::generate();
        if let Err(e) = recheck(&self.shared, &request_id, &fingerprint, &identity).await {
            telemetry::log_failure(&e.with_context("at", "channel_open"), &request_id);
            drop(reply);
            session.disconnect(
                russh::Disconnect::ByApplication,
                "the SSH key is no longer valid",
                "",
            )?;
            return Ok(());
        }
        if self.channels.fetch_add(1, Ordering::SeqCst) >= MAX_CHANNELS {
            self.channels.fetch_sub(1, Ordering::SeqCst);
            return Ok(());
        }
        reply.accept().await;
        let shared = Arc::clone(&self.shared);
        let channels = Arc::clone(&self.channels);
        let caller = Caller {
            principal: Principal::ssh_key(identity.user_id, account_kind(identity.kind)),
            peer: self.peer,
            ssh_key_id: identity.key_id,
            user_id: identity.user_id,
            fingerprint,
            idle: self.shared.idle_timeout,
        };
        tokio::spawn(async move {
            run_channel(shared, caller, identity.username, channel).await;
            channels.fetch_sub(1, Ordering::SeqCst);
        });
        Ok(())
    }

    async fn env_request(
        &mut self,
        channel: ChannelId,
        name: &str,
        _value: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        if name == "GIT_PROTOCOL" {
            session.channel_success(channel)
        } else {
            session.channel_failure(channel)
        }
    }

    async fn exec_request(
        &mut self,
        channel: ChannelId,
        _data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        // Judged by the channel task, in-band.
        session.channel_success(channel)
    }

    async fn shell_request(
        &mut self,
        channel: ChannelId,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.channel_success(channel)
    }

    async fn pty_request(
        &mut self,
        channel: ChannelId,
        _term: &str,
        _col_width: u32,
        _row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        _modes: &[(russh::Pty, u32)],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.channel_failure(channel)
    }

    async fn subsystem_request(
        &mut self,
        channel: ChannelId,
        _name: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.channel_failure(channel)
    }

    async fn x11_request(
        &mut self,
        channel: ChannelId,
        _single_connection: bool,
        _x11_auth_protocol: &str,
        _x11_auth_cookie: &str,
        _x11_screen_number: u32,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.channel_failure(channel)
    }

    async fn agent_request(
        &mut self,
        _channel: ChannelId,
        _session: &mut Session,
    ) -> Result<bool, Self::Error> {
        Ok(false)
    }
}

/// Serve one session channel: wait for `exec` (or `shell`), collecting
/// `GIT_PROTOCOL`, then run it and end the channel with its exit status.
async fn run_channel(shared: Arc<Shared>, caller: Caller, username: String, channel: Channel<Msg>) {
    let (mut read, write) = channel.split();
    let mut git_protocol: Option<String> = None;
    let request_id = RequestId::generate();
    let deadline = tokio::time::Instant::now() + EXEC_WAIT;
    let status = loop {
        let message = match tokio::time::timeout_at(deadline, read.wait()).await {
            Ok(Some(message)) => message,
            // Closed before any command.
            Ok(None) => return,
            Err(_) => {
                tracing::info!(
                    target: "forge_web::ssh",
                    ssh_key_id = caller.ssh_key_id,
                    wait_ms = EXEC_WAIT.as_millis(),
                    "forge.ssh.channel_without_command"
                );
                let _ = write.close().await; // ignore: the client may be gone
                return;
            }
        };
        match message {
            ChannelMsg::SetEnv {
                variable_name,
                variable_value,
                ..
            } if variable_name == "GIT_PROTOCOL" => {
                git_protocol = Some(variable_value.chars().take(256).collect());
            }
            ChannelMsg::Exec { command, .. } => {
                break exec(
                    &shared,
                    &caller,
                    &command,
                    git_protocol.as_deref(),
                    &request_id,
                    &mut read,
                    &write,
                )
                .await;
            }
            ChannelMsg::RequestShell { .. } => {
                break shell(&shared, &caller, &username, &request_id, &write).await;
            }
            ChannelMsg::Eof | ChannelMsg::Close => return,
            // Data before a command, other environment variables (refused
            // by the handler), refused requests: nothing to do.
            ChannelMsg::Open { .. }
            | ChannelMsg::Data { .. }
            | ChannelMsg::ExtendedData { .. }
            | ChannelMsg::RequestPty { .. }
            | ChannelMsg::Signal { .. }
            | ChannelMsg::RequestSubsystem { .. }
            | ChannelMsg::RequestX11 { .. }
            | ChannelMsg::SetEnv { .. }
            | ChannelMsg::WindowChange { .. }
            | ChannelMsg::AgentForward { .. }
            | ChannelMsg::XonXoff { .. }
            | ChannelMsg::ExitStatus { .. }
            | ChannelMsg::ExitSignal { .. }
            | ChannelMsg::WindowAdjusted { .. }
            | ChannelMsg::Success
            | ChannelMsg::Failure
            | ChannelMsg::OpenFailure(_) => {}
            // ChannelMsg is non-exhaustive: a future message is ignored too.
            #[allow(clippy::wildcard_enum_match_arm)]
            _ => {}
        }
    };
    // Best effort: the client may have closed the channel already.
    let _ = write.exit_status(status).await; // ignore: see above
    let _ = write.eof().await; // ignore: see above
    let _ = write.close().await; // ignore: see above
}

async fn exec(
    shared: &Shared,
    caller: &Caller,
    raw: &[u8],
    git_protocol: Option<&str>,
    request_id: &RequestId,
    read: &mut russh::ChannelReadHalf,
    write: &russh::ChannelWriteHalf<Msg>,
) -> u32 {
    // The key is decided again before any command runs.
    let lookup = shared
        .authenticator
        .authenticate_ssh_key(request_id, &caller.fingerprint, false);
    match lookup.await {
        Ok(now) if now.user_id == caller.user_id => {}
        Ok(_) => {
            let err = ForgeError::new(ErrorCode::Unauthenticated)
                .with_context("reason", "ssh_key_owner_changed")
                .with_context("ssh_key_id", caller.ssh_key_id)
                .with_context("at", "exec");
            refuse(&err, request_id, write).await;
            return 1;
        }
        Err(err) => {
            let err = err
                .with_context("at", "exec")
                .with_context("transport", "ssh");
            refuse(&err, request_id, write).await;
            return 1;
        }
    }
    let command = match parse_command(raw) {
        Ok(command) => command,
        Err(err) => {
            let err = err
                .with_context("ssh_key_id", caller.ssh_key_id)
                .with_context("user_id", caller.user_id.get());
            refuse(&err, request_id, write).await;
            return 1;
        }
    };
    let mut out = write.make_writer();
    let outcome = bridge::run(
        &shared.router,
        caller,
        &command,
        git_protocol,
        request_id,
        read.make_reader(),
        &mut out,
    )
    .await;
    // Flush what is queued before the exit status.
    let _ = out.flush().await; // ignore: the client may be gone
    tracing::info!(
        target: "forge_web::ssh",
        request_id = %request_id,
        ssh_key_id = caller.ssh_key_id,
        user_id = caller.user_id.get(),
        service = command.service.name(),
        exit_status = outcome.exit_status,
        refused = outcome.refused.unwrap_or(""),
        http_request_ids = %outcome.http_request_ids.join(","),
        "forge.ssh.command"
    );
    outcome.exit_status
}

/// The key decided again (no use recorded): still known, its owner still
/// active and the same person as at authentication.
async fn recheck(
    shared: &Shared,
    request_id: &RequestId,
    fingerprint: &str,
    at_auth: &SshKeyIdentity,
) -> Result<(), ForgeError> {
    let now = shared
        .authenticator
        .authenticate_ssh_key(request_id, fingerprint, false)
        .await?;
    if now.user_id != at_auth.user_id {
        return Err(ForgeError::new(ErrorCode::Unauthenticated)
            .with_context("reason", "ssh_key_owner_changed")
            .with_context("ssh_key_id", at_auth.key_id));
    }
    Ok(())
}

/// Log a refusal decided here and answer it in-band.
async fn refuse(err: &ForgeError, request_id: &RequestId, write: &russh::ChannelWriteHalf<Msg>) {
    telemetry::log_failure(err, request_id);
    let line = bridge::err_packet(&format!(
        "{} {} (request_id: {})",
        err.code().id(),
        err.user_message(forge_errors::Lang::Ja),
        request_id
    ));
    let mut out = write.make_writer();
    let _ = out.write_all(&line).await; // ignore: the client may be gone
    let _ = out.flush().await; // ignore: see above
}

async fn shell(
    shared: &Shared,
    caller: &Caller,
    username: &str,
    request_id: &RequestId,
    write: &russh::ChannelWriteHalf<Msg>,
) -> u32 {
    let err = ForgeError::new(ErrorCode::SshCommandRefused)
        .with_context("reason", "shell")
        .with_context("ssh_key_id", caller.ssh_key_id)
        .with_context("user_id", caller.user_id.get());
    telemetry::log_failure(&err, request_id);
    let text = format!(
        "GitStation: {username} として SSH 鍵で認証されました。シェルは使えません。\r\n\
         clone: git clone ssh://git@{}/<group>/<project>.git\r\n",
        shared.public.clone_host
    );
    let mut stderr = write.make_writer_ext(Some(1));
    let _ = stderr.write_all(text.as_bytes()).await; // ignore: the client may be gone
    let _ = stderr.flush().await; // ignore: see above
    1
}
