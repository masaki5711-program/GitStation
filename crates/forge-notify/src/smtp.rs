//! A small SMTP submission client (RFC 5321) for the internal relay.
//!
//! Why not a crate: the delivery contract needs to know, for every failure,
//! whether the relay could have the message. A general client reports "the
//! connection broke" the same way before and after the message was handed
//! over, and a retry after the second would send a person the same mail
//! twice. Here the phases are explicit: every failure before the end-of-data
//! line has been written is [`SendOutcome::NotSent`] (safe to retry unless
//! the relay refused for good); a failure after it is
//! [`SendOutcome::Unknown`] and is never retried (RFC 5321 §4.1.1.4 allows a
//! relay to have accepted a message whose reply was lost).
//!
//! What it does: connect (own timeout), optional implicit TLS, greeting,
//! `EHLO`, `STARTTLS` (required when configured — never downgraded), `AUTH
//! PLAIN` (only over TLS, enforced by the configuration), `MAIL FROM`,
//! `RCPT TO`, `DATA`, the dot-stuffed message, `QUIT`. Every step shares one
//! deadline (`notify.send_timeout_ms`); replies are read with hard size
//! limits; every address and the credentials are validated or encoded so
//! nothing a person typed can start a new command.

use std::net::IpAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use base64::Engine as _;
use forge_core::config::{NotifyConfig, SmtpConfig, SmtpTls};
use forge_errors::{ErrorCode, ForgeError};
use rustls::pki_types::{pem::PemObject as _, CertificateDer, ServerName};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _, ReadBuf};
use tokio::net::TcpStream;
use tokio::time::Instant;
use tokio_rustls::TlsConnector;

use crate::mail::Mail;

/// A reply line is at most this long (RFC 5321: 512 octets; some relays
/// send more in `EHLO`).
const MAX_LINE: usize = 2048;
/// A reply is at most this many lines (an `EHLO` lists a dozen).
const MAX_REPLY_LINES: usize = 64;
/// `QUIT` is a courtesy: it never delays the result by more than this.
const QUIT_TIMEOUT: Duration = Duration::from_secs(2);

/// Where a failed send stopped (`FG-INT-0015 stage`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Connect,
    Tls,
    Auth,
    MailFrom,
    RcptTo,
    Data,
    EndOfData,
}

impl Stage {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Connect => "connect",
            Self::Tls => "tls",
            Self::Auth => "auth",
            Self::MailFrom => "mail_from",
            Self::RcptTo => "rcpt_to",
            Self::Data => "data",
            Self::EndOfData => "end_of_data",
        }
    }
}

/// Why a send did not complete (`FG-INT-0015 reason`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    ConnectFailed,
    /// The relay closed or broke the connection mid-conversation.
    ConnectionLost,
    TlsFailed,
    AuthFailed,
    Timeout,
    RejectedTemporary,
    RejectedPermanent,
    OutcomeUnknown,
}

impl Reason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ConnectFailed => "connect_failed",
            Self::ConnectionLost => "connection_lost",
            Self::TlsFailed => "tls_failed",
            Self::AuthFailed => "auth_failed",
            Self::Timeout => "timeout",
            Self::RejectedTemporary => "rejected_temporary",
            Self::RejectedPermanent => "rejected_permanent",
            Self::OutcomeUnknown => "outcome_unknown",
        }
    }
}

/// A send that did not complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SendFailure {
    pub reason: Reason,
    pub stage: Stage,
    /// The relay's reply code, when it replied.
    pub smtp_code: Option<u16>,
}

impl SendFailure {
    /// The relay refused for good: another try cannot succeed.
    pub fn is_permanent(self) -> bool {
        self.reason == Reason::RejectedPermanent
    }

    /// The `FG-INT-0015` error carrying this failure.
    pub fn error(self, notification: i64, attempt: i32, max_attempts: i32) -> ForgeError {
        let err = ForgeError::new(ErrorCode::SmtpUnavailable)
            .with_context("reason", self.reason.as_str())
            .with_context("stage", self.stage.as_str())
            .with_context("notification_id", notification)
            .with_context("attempt", attempt)
            .with_context("max_attempts", max_attempts);
        match self.smtp_code {
            Some(code) => err.with_context("smtp_code", code),
            None => err,
        }
    }
}

/// How a send ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendOutcome {
    /// The relay accepted the message (a 250 after the end of data).
    Delivered,
    /// The relay does not have the message.
    NotSent(SendFailure),
    /// The message was handed over and the answer is missing: the relay may
    /// or may not have it. Never retried.
    Unknown(SendFailure),
}

/// What sends a [`Mail`]. The worker uses [`SmtpSender`]; tests use fakes.
pub trait Sender: Send + Sync + 'static {
    fn send<'a>(
        &'a self,
        mail: &'a Mail,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = SendOutcome> + Send + 'a>>;
}

/// The SMTP implementation of [`Sender`].
pub struct SmtpSender {
    smtp: SmtpConfig,
    password: Option<String>,
    tls: Option<TlsConnector>,
    connect_timeout: Duration,
    send_timeout: Duration,
}

impl std::fmt::Debug for SmtpSender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // No credentials, no addresses.
        f.debug_struct("SmtpSender")
            .field("host", &self.smtp.host)
            .field("port", &self.smtp.port)
            .field("tls", &self.smtp.tls)
            .finish_non_exhaustive()
    }
}

fn config_error(reason: &'static str, key: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::ConfigInvalid)
        .with_context("config_key", key)
        .with_context("reason", reason)
}

impl SmtpSender {
    /// Build the sender from a validated, enabled configuration: reads the
    /// password file and the CA roots once (a failure stops the worker's
    /// start, `FG-SYS-0002`).
    pub fn new(cfg: &NotifyConfig) -> Result<Self, ForgeError> {
        Self::build(cfg, None)
    }

    /// As [`Self::new`] with the trust roots given (tests and tools).
    pub fn with_roots(
        cfg: &NotifyConfig,
        roots: rustls::RootCertStore,
    ) -> Result<Self, ForgeError> {
        Self::build(cfg, Some(roots))
    }

    fn build(cfg: &NotifyConfig, roots: Option<rustls::RootCertStore>) -> Result<Self, ForgeError> {
        let smtp = cfg
            .smtp
            .clone() // clone: the sender owns its settings
            .ok_or_else(|| config_error("missing", "notify.smtp_host"))?;
        let password = cfg.smtp_password()?.map(|s| s.expose().to_owned());
        let tls = match smtp.tls {
            SmtpTls::None => None,
            SmtpTls::StartTls | SmtpTls::Tls => {
                let roots = match roots {
                    Some(roots) => roots,
                    None => load_roots(&smtp)?,
                };
                Some(connector_for(roots)?)
            }
        };
        Ok(Self {
            smtp,
            password,
            tls,
            connect_timeout: cfg.connect_timeout,
            send_timeout: cfg.send_timeout,
        })
    }
}

/// Roots: the PEM bundle of `notify.smtp_ca_file` only; without one, the
/// operating system's trust store (an operator installs the internal CA
/// there). Never a bundled public list.
fn load_roots(smtp: &SmtpConfig) -> Result<rustls::RootCertStore, ForgeError> {
    const KEY: &str = "notify.smtp_ca_file";
    let mut roots = rustls::RootCertStore::empty();
    match &smtp.ca_file {
        Some(path) => {
            let pem = std::fs::read(path).map_err(|e| {
                config_error("unreadable", KEY)
                    .with_context("path", path.display())
                    .with_source(e)
            })?;
            let mut added = 0usize;
            for cert in CertificateDer::pem_slice_iter(&pem) {
                let cert = cert.map_err(|_| config_error("invalid", KEY))?;
                roots.add(cert).map_err(|_| config_error("invalid", KEY))?;
                added += 1;
            }
            if added == 0 {
                return Err(config_error("invalid", KEY));
            }
        }
        None => {
            let native = rustls_native_certs::load_native_certs();
            for cert in native.certs {
                // A certificate the library cannot use is skipped; having
                // none at all is the error below.
                let _ = roots.add(cert);
            }
            if roots.is_empty() {
                return Err(config_error("no_trust_roots", KEY));
            }
        }
    }
    Ok(roots)
}

fn connector_for(roots: rustls::RootCertStore) -> Result<TlsConnector, ForgeError> {
    const KEY: &str = "notify.smtp_ca_file";
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|_| config_error("tls_versions", KEY))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(TlsConnector::from(Arc::new(config)))
}

impl Sender for SmtpSender {
    fn send<'a>(
        &'a self,
        mail: &'a Mail,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = SendOutcome> + Send + 'a>> {
        Box::pin(self.deliver(mail))
    }
}

/// Where the conversation is, for classifying a failure.
struct Progress {
    stage: Stage,
    /// The end-of-data line was written and flushed.
    handed_over: bool,
}

impl SmtpSender {
    async fn deliver(&self, mail: &Mail) -> SendOutcome {
        let deadline = Instant::now() + self.send_timeout;
        let mut progress = Progress {
            stage: Stage::Connect,
            handed_over: false,
        };
        match self.conversation(mail, deadline, &mut progress).await {
            Ok(()) => SendOutcome::Delivered,
            Err(mut failure) => {
                // A timeout or a lost connection happened where the
                // conversation was; a refusal names its own stage.
                if matches!(failure.reason, Reason::Timeout | Reason::ConnectionLost) {
                    failure.stage = progress.stage;
                }
                // After the hand-over nothing is known; before it, the
                // relay cannot have the message.
                if progress.handed_over {
                    failure.reason = Reason::OutcomeUnknown;
                    failure.stage = Stage::EndOfData;
                    SendOutcome::Unknown(failure)
                } else {
                    SendOutcome::NotSent(failure)
                }
            }
        }
    }

    async fn conversation(
        &self,
        mail: &Mail,
        deadline: Instant,
        progress: &mut Progress,
    ) -> Result<(), SendFailure> {
        let smtp = &self.smtp;
        let fail = |reason, stage| SendFailure {
            reason,
            stage,
            smtp_code: None,
        };
        // Connect (its own, shorter bound).
        let connect_deadline = deadline.min(Instant::now() + self.connect_timeout);
        let tcp = tokio::time::timeout_at(
            connect_deadline,
            TcpStream::connect((smtp.host.as_str(), smtp.port)),
        )
        .await
        .map_err(|_| fail(Reason::ConnectFailed, Stage::Connect))?
        .map_err(|_| fail(Reason::ConnectFailed, Stage::Connect))?;
        // A message is a few KiB: do not wait for Nagle.
        tcp.set_nodelay(true)
            .map_err(|_| fail(Reason::ConnectFailed, Stage::Connect))?;

        let mut conn = match smtp.tls {
            SmtpTls::Tls => {
                progress.stage = Stage::Tls;
                Conn::new(self.handshake(tcp, deadline).await?)
            }
            SmtpTls::None | SmtpTls::StartTls => Conn::new(Stream::Plain(tcp)),
        };

        // Greeting and EHLO.
        progress.stage = Stage::Connect;
        let greeting = conn.reply(deadline).await?;
        expect(&greeting, 220, Stage::Connect)?;
        let helo = ehlo_name(&smtp.from_address);
        let mut caps = self
            .ehlo(&mut conn, &helo, deadline, Stage::Connect)
            .await?;

        if smtp.tls == SmtpTls::StartTls {
            progress.stage = Stage::Tls;
            if !caps.starttls {
                // Never continue in clear text when TLS was configured.
                return Err(fail(Reason::TlsFailed, Stage::Tls));
            }
            let reply = conn.command("STARTTLS", deadline).await?;
            if reply.code != 220 {
                return Err(SendFailure {
                    reason: Reason::TlsFailed,
                    stage: Stage::Tls,
                    smtp_code: Some(reply.code),
                });
            }
            let Some(plain) = conn.into_plain() else {
                // Bytes the relay sent before the upgrade would be read as
                // if they came through TLS (CVE-2011-0411): refuse.
                return Err(fail(Reason::TlsFailed, Stage::Tls));
            };
            conn = Conn::new(self.handshake(plain, deadline).await?);
            caps = self.ehlo(&mut conn, &helo, deadline, Stage::Tls).await?;
        }

        // AUTH PLAIN (the configuration allows it only over TLS).
        if let (Some(user), Some(password)) = (&smtp.username, &self.password) {
            progress.stage = Stage::Auth;
            if !caps.auth_plain {
                return Err(fail(Reason::AuthFailed, Stage::Auth));
            }
            let token =
                base64::engine::general_purpose::STANDARD.encode(format!("\0{user}\0{password}"));
            let reply = conn
                .command_secret(&format!("AUTH PLAIN {token}"), deadline)
                .await?;
            if reply.code != 235 {
                return Err(SendFailure {
                    reason: Reason::AuthFailed,
                    stage: Stage::Auth,
                    smtp_code: Some(reply.code),
                });
            }
        }

        // The envelope. `Mail` holds only addresses that passed the same
        // check as the configuration's, so no command can be injected.
        progress.stage = Stage::MailFrom;
        let reply = conn
            .command(&format!("MAIL FROM:<{}>", mail.from_address()), deadline)
            .await?;
        expect(&reply, 250, Stage::MailFrom)?;
        progress.stage = Stage::RcptTo;
        let reply = conn
            .command(&format!("RCPT TO:<{}>", mail.to_address()), deadline)
            .await?;
        if !matches!(reply.code, 250 | 251) {
            return Err(refusal(&reply, Stage::RcptTo));
        }
        progress.stage = Stage::Data;
        let reply = conn.command("DATA", deadline).await?;
        expect(&reply, 354, Stage::Data)?;

        // The message. Until the end-of-data line is written and flushed the
        // relay cannot have committed anything.
        let body = dot_stuffed(&mail.render());
        conn.write_all(&body, deadline).await?;
        progress.stage = Stage::EndOfData;
        progress.handed_over = true;
        let reply = conn.reply(deadline).await?;
        if reply.code != 250 {
            // A refusal is an answer: the relay does not have the message.
            progress.handed_over = false;
            return Err(refusal(&reply, Stage::EndOfData));
        }

        // The result is known. QUIT is a courtesy: its failure changes
        // nothing (the connection is dropped either way), so it is the one
        // result that is deliberately not looked at.
        let quit_deadline = (Instant::now() + QUIT_TIMEOUT).min(deadline);
        let _courtesy = conn.command("QUIT", quit_deadline).await;
        Ok(())
    }

    async fn ehlo(
        &self,
        conn: &mut Conn,
        name: &str,
        deadline: Instant,
        stage: Stage,
    ) -> Result<Capabilities, SendFailure> {
        let reply = conn.command(&format!("EHLO {name}"), deadline).await?;
        expect(&reply, 250, stage)?;
        Ok(Capabilities::of(&reply))
    }

    async fn handshake(&self, tcp: TcpStream, deadline: Instant) -> Result<Stream, SendFailure> {
        let fail = |reason| SendFailure {
            reason,
            stage: Stage::Tls,
            smtp_code: None,
        };
        let Some(connector) = &self.tls else {
            return Err(fail(Reason::TlsFailed));
        };
        let host = self.smtp.host.as_str();
        let name = match host.parse::<IpAddr>() {
            Ok(ip) => ServerName::IpAddress(ip.into()),
            Err(_) => ServerName::try_from(host.to_owned()).map_err(|_| fail(Reason::TlsFailed))?,
        };
        let stream = tokio::time::timeout_at(deadline, connector.connect(name, tcp))
            .await
            .map_err(|_| fail(Reason::Timeout))?
            .map_err(|_| fail(Reason::TlsFailed))?;
        Ok(Stream::Tls(Box::new(stream)))
    }
}

/// The EHLO argument: the sender's own domain (a name the relay can check).
fn ehlo_name(from_address: &str) -> String {
    from_address
        .rsplit_once('@')
        .map_or("localhost", |(_, domain)| domain)
        .to_owned()
}

fn expect(reply: &Reply, code: u16, stage: Stage) -> Result<(), SendFailure> {
    if reply.code == code {
        Ok(())
    } else {
        Err(refusal(reply, stage))
    }
}

/// A reply that is not the one expected: 4xx is temporary, anything else
/// (5xx, or a reply the protocol does not allow here) is for good.
fn refusal(reply: &Reply, stage: Stage) -> SendFailure {
    let reason = if (400..500).contains(&reply.code) {
        Reason::RejectedTemporary
    } else {
        Reason::RejectedPermanent
    };
    SendFailure {
        reason,
        stage,
        smtp_code: Some(reply.code),
    }
}

#[derive(Debug, Default)]
struct Capabilities {
    starttls: bool,
    auth_plain: bool,
}

impl Capabilities {
    fn of(reply: &Reply) -> Self {
        let mut caps = Self::default();
        for line in &reply.lines {
            let upper = line.to_ascii_uppercase();
            if upper == "STARTTLS" {
                caps.starttls = true;
            } else if let Some(mechanisms) = upper.strip_prefix("AUTH") {
                // "AUTH PLAIN LOGIN" and the old "AUTH=PLAIN LOGIN".
                caps.auth_plain |= mechanisms
                    .split([' ', '='])
                    .any(|mechanism| mechanism == "PLAIN");
            }
        }
        caps
    }
}

/// `\r\n.` becomes `\r\n..`, a leading `.` too, and the end-of-data line
/// follows. `message` uses CRLF line ends.
pub(crate) fn dot_stuffed(message: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(message.len() + 8);
    let mut at_line_start = true;
    for &byte in message {
        if at_line_start && byte == b'.' {
            out.push(b'.');
        }
        out.push(byte);
        at_line_start = byte == b'\n';
    }
    if !out.ends_with(b"\r\n") {
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b".\r\n");
    out
}

/// One reply: the code and the text of each line.
#[derive(Debug)]
struct Reply {
    code: u16,
    lines: Vec<String>,
}

/// The transport: plain, or TLS over it.
enum Stream {
    Plain(TcpStream),
    Tls(Box<tokio_rustls::client::TlsStream<TcpStream>>),
}

impl AsyncRead for Stream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Plain(s) => Pin::new(s).poll_read(cx, buf),
            Self::Tls(s) => Pin::new(s.as_mut()).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Stream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match self.get_mut() {
            Self::Plain(s) => Pin::new(s).poll_write(cx, buf),
            Self::Tls(s) => Pin::new(s.as_mut()).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Plain(s) => Pin::new(s).poll_flush(cx),
            Self::Tls(s) => Pin::new(s.as_mut()).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Plain(s) => Pin::new(s).poll_shutdown(cx),
            Self::Tls(s) => Pin::new(s.as_mut()).poll_shutdown(cx),
        }
    }
}

/// A connection with its read buffer.
struct Conn {
    io: Stream,
    buffer: Vec<u8>,
}

impl Conn {
    fn new(io: Stream) -> Self {
        Self {
            io,
            buffer: Vec::new(),
        }
    }

    /// The bare TCP stream back, for the STARTTLS upgrade; `None` when it is
    /// already TLS or when bytes are still buffered.
    fn into_plain(self) -> Option<TcpStream> {
        if !self.buffer.is_empty() {
            return None;
        }
        match self.io {
            Stream::Plain(tcp) => Some(tcp),
            Stream::Tls(_) => None,
        }
    }

    fn lost() -> SendFailure {
        SendFailure {
            reason: Reason::ConnectionLost,
            stage: Stage::Connect,
            smtp_code: None,
        }
    }

    fn timed_out() -> SendFailure {
        SendFailure {
            reason: Reason::Timeout,
            stage: Stage::Connect,
            smtp_code: None,
        }
    }

    async fn write_all(&mut self, bytes: &[u8], deadline: Instant) -> Result<(), SendFailure> {
        let write = async {
            self.io.write_all(bytes).await?;
            self.io.flush().await
        };
        tokio::time::timeout_at(deadline, write)
            .await
            .map_err(|_| Self::timed_out())?
            .map_err(|_| Self::lost())
    }

    /// One command line, then its reply.
    async fn command(&mut self, line: &str, deadline: Instant) -> Result<Reply, SendFailure> {
        self.send_line(line, deadline).await?;
        self.reply(deadline).await
    }

    /// As [`Self::command`] for a line holding a secret (named apart so a
    /// reader sees that the line is never logged).
    async fn command_secret(
        &mut self,
        line: &str,
        deadline: Instant,
    ) -> Result<Reply, SendFailure> {
        self.command(line, deadline).await
    }

    async fn send_line(&mut self, line: &str, deadline: Instant) -> Result<(), SendFailure> {
        if line.bytes().any(|b| b == b'\r' || b == b'\n') {
            // Unreachable by construction (the inputs are validated); if it
            // ever happens the command is not sent.
            return Err(SendFailure {
                reason: Reason::RejectedPermanent,
                stage: Stage::Connect,
                smtp_code: None,
            });
        }
        let mut bytes = Vec::with_capacity(line.len() + 2);
        bytes.extend_from_slice(line.as_bytes());
        bytes.extend_from_slice(b"\r\n");
        self.write_all(&bytes, deadline).await
    }

    /// One line without its end, at most [`MAX_LINE`] bytes.
    async fn line(&mut self, deadline: Instant) -> Result<String, SendFailure> {
        loop {
            if let Some(end) = self.buffer.iter().position(|b| *b == b'\n') {
                let raw: Vec<u8> = self.buffer.drain(..=end).collect();
                let text = String::from_utf8_lossy(&raw);
                return Ok(text.trim_end_matches(['\r', '\n']).to_owned());
            }
            if self.buffer.len() > MAX_LINE {
                return Err(Self::lost());
            }
            let mut chunk = [0u8; 1024];
            let n = tokio::time::timeout_at(deadline, self.io.read(&mut chunk))
                .await
                .map_err(|_| Self::timed_out())?
                .map_err(|_| Self::lost())?;
            if n == 0 {
                return Err(Self::lost());
            }
            let received = chunk.get(..n).ok_or_else(Self::lost)?;
            self.buffer.extend_from_slice(received);
        }
    }

    /// One (possibly multi-line) reply.
    async fn reply(&mut self, deadline: Instant) -> Result<Reply, SendFailure> {
        let mut lines = Vec::new();
        let mut code: Option<u16> = None;
        loop {
            let line = self.line(deadline).await?;
            let (digits, rest) = (line.get(..3), line.get(3..));
            let (Some(digits), Some(rest)) = (digits, rest) else {
                return Err(Self::lost());
            };
            let this: u16 = digits.parse().map_err(|_| Self::lost())?;
            if !(200..600).contains(&this) || code.is_some_and(|c| c != this) {
                return Err(Self::lost());
            }
            code = Some(this);
            let (separator, text) = match rest.chars().next() {
                Some(c @ (' ' | '-')) => (c, rest.get(1..).unwrap_or("")),
                None => (' ', ""),
                Some(_) => return Err(Self::lost()),
            };
            lines.push(text.to_owned());
            if separator == ' ' {
                return Ok(Reply { code: this, lines });
            }
            if lines.len() >= MAX_REPLY_LINES {
                return Err(Self::lost());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_data_is_dot_stuffed_and_ends_with_the_terminator() {
        assert_eq!(
            dot_stuffed(b"a\r\n.b\r\n..\r\nc\r\n"),
            b"a\r\n..b\r\n...\r\nc\r\n.\r\n"
        );
        assert_eq!(dot_stuffed(b".\r\n"), b"..\r\n.\r\n");
        // A last line without a line end gets one before the terminator.
        assert_eq!(dot_stuffed(b"x"), b"x\r\n.\r\n");
        assert_eq!(dot_stuffed(b""), b"\r\n.\r\n");
    }

    #[test]
    fn capabilities_are_read_from_the_ehlo_lines() {
        let reply = Reply {
            code: 250,
            lines: vec![
                "mail.example.test".to_owned(),
                "SIZE 1000".to_owned(),
                "starttls".to_owned(),
                "AUTH LOGIN PLAIN".to_owned(),
            ],
        };
        let caps = Capabilities::of(&reply);
        assert!(caps.starttls && caps.auth_plain);
        let old = Reply {
            code: 250,
            lines: vec!["AUTH=LOGIN PLAIN".to_owned()],
        };
        assert!(Capabilities::of(&old).auth_plain);
        let none = Reply {
            code: 250,
            lines: vec!["8BITMIME".to_owned(), "AUTH LOGIN".to_owned()],
        };
        let caps = Capabilities::of(&none);
        assert!(!caps.starttls && !caps.auth_plain);
    }

    #[test]
    fn a_refusal_is_temporary_only_for_4xx() {
        let reply = |code| Reply {
            code,
            lines: Vec::new(),
        };
        assert_eq!(
            refusal(&reply(451), Stage::RcptTo).reason,
            Reason::RejectedTemporary
        );
        assert_eq!(
            refusal(&reply(550), Stage::RcptTo).reason,
            Reason::RejectedPermanent
        );
        assert_eq!(
            refusal(&reply(500), Stage::Connect).reason,
            Reason::RejectedPermanent
        );
        assert!(refusal(&reply(554), Stage::Data).is_permanent());
        assert!(!refusal(&reply(421), Stage::Data).is_permanent());
    }

    #[test]
    fn the_ehlo_name_is_the_senders_domain() {
        assert_eq!(ehlo_name("forge@git.example.test"), "git.example.test");
        assert_eq!(ehlo_name("nonsense"), "localhost");
    }
}
