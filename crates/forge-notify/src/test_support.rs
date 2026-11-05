//! An in-process fake SMTP relay for tests (feature `test-support`): a
//! listener on `127.0.0.1:0` that speaks enough SMTP to receive a message,
//! records what it was sent, and can be told to misbehave at each step.
//! Synthetic data only; no real host is ever contacted.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use forge_core::config::{NotifyConfig, SmtpConfig, SmtpTls};
use forge_errors::Lang;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::{TcpListener, TcpStream};

/// What the relay does after the message was received.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AfterData {
    /// Answer with the configured end-of-data reply (default `250`).
    #[default]
    Reply,
    /// Close the connection without answering.
    Close,
    /// Keep the connection open and never answer.
    Hang,
}

/// How the fake relay behaves. The default is a well-behaved relay.
#[derive(Debug, Clone, Default)]
pub struct Script {
    /// Reply with this instead of `220` and close.
    pub greeting: Option<u16>,
    /// Accept the connection and say nothing.
    pub hang_at_greeting: bool,
    /// Advertise `STARTTLS` in `EHLO` (it is never actually available).
    pub advertise_starttls: bool,
    /// Require `AUTH PLAIN` with this user and password.
    pub auth: Option<(String, String)>,
    pub mail_from_reply: Option<u16>,
    pub rcpt_reply: Option<u16>,
    pub data_reply: Option<u16>,
    pub end_reply: Option<u16>,
    pub after_data: AfterData,
}

/// What the relay was sent over one accepted message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Received {
    pub mail_from: String,
    pub rcpt_to: Vec<String>,
    /// The message with the transparency dots removed, CRLF line ends.
    pub data: String,
}

/// A running fake relay (stopped when dropped).
pub struct FakeSmtp {
    addr: SocketAddr,
    messages: Arc<Mutex<Vec<Received>>>,
    commands: Arc<Mutex<Vec<String>>>,
    connections: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for FakeSmtp {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl FakeSmtp {
    pub async fn start(script: Script) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let messages = Arc::new(Mutex::new(Vec::new()));
        let commands = Arc::new(Mutex::new(Vec::new()));
        let connections = Arc::new(AtomicUsize::new(0));
        let (m, c, n) = (
            Arc::clone(&messages),
            Arc::clone(&commands),
            Arc::clone(&connections),
        );
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                n.fetch_add(1, Ordering::SeqCst);
                let (script, m, c) = (script.clone(), Arc::clone(&m), Arc::clone(&c));
                tokio::spawn(async move {
                    // A broken client connection ends only that session.
                    let _ = session(stream, script, m, c).await;
                });
            }
        });
        Self {
            addr,
            messages,
            commands,
            connections,
            task,
        }
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Messages received so far, oldest first.
    pub fn messages(&self) -> Vec<Received> {
        self.messages.lock().expect("messages").clone()
    }

    /// Every command line received (the AUTH token masked), in order.
    pub fn commands(&self) -> Vec<String> {
        self.commands.lock().expect("commands").clone()
    }

    pub fn connections(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }

    /// A configuration that sends to this relay in plain text (the
    /// relay is local; production configs refuse credentials without TLS).
    pub fn config(&self) -> NotifyConfig {
        NotifyConfig {
            enabled: true,
            smtp: Some(SmtpConfig {
                host: self.addr.ip().to_string(),
                port: self.addr.port(),
                tls: SmtpTls::None,
                ca_file: None,
                username: None,
                password_file: None,
                from_address: "forge@git.example.test".to_owned(),
                from_name: "GitStation".to_owned(),
            }),
            default_language: Lang::En,
            connect_timeout: Duration::from_millis(500),
            send_timeout: Duration::from_secs(3),
            max_attempts: 3,
        }
    }
}

fn reply_line(code: u16, text: &str) -> String {
    format!("{code} {text}\r\n")
}

async fn session(
    stream: TcpStream,
    script: Script,
    messages: Arc<Mutex<Vec<Received>>>,
    commands: Arc<Mutex<Vec<String>>>,
) -> std::io::Result<()> {
    let (read, mut write) = stream.into_split();
    let mut read = BufReader::new(read);
    if script.hang_at_greeting {
        std::future::pending::<()>().await;
    }
    if let Some(code) = script.greeting {
        write
            .write_all(reply_line(code, "go away").as_bytes())
            .await?;
        return Ok(());
    }
    write.write_all(b"220 fake.example.test ESMTP\r\n").await?;
    let mut mail_from = String::new();
    let mut rcpt_to: Vec<String> = Vec::new();
    let mut authed = script.auth.is_none();
    let mut line = String::new();
    loop {
        line.clear();
        if read.read_line(&mut line).await? == 0 {
            return Ok(());
        }
        let cmd = line.trim_end().to_owned();
        let upper = cmd.to_ascii_uppercase();
        let shown = if upper.starts_with("AUTH PLAIN") {
            "AUTH PLAIN <masked>".to_owned()
        } else {
            cmd.clone() // clone: the log keeps its own copy
        };
        commands.lock().expect("commands").push(shown);
        if upper.starts_with("EHLO") {
            let mut out = String::from("250-fake.example.test\r\n250-8BITMIME\r\n");
            if script.advertise_starttls {
                out.push_str("250-STARTTLS\r\n");
            }
            if script.auth.is_some() {
                out.push_str("250-AUTH PLAIN\r\n");
            }
            out.push_str("250 SIZE 10485760\r\n");
            write.write_all(out.as_bytes()).await?;
        } else if let Some(token) = upper.strip_prefix("AUTH PLAIN ") {
            let _ = token;
            let given = cmd.split_whitespace().nth(2).unwrap_or("");
            let ok = script.auth.as_ref().is_some_and(|(user, password)| {
                use base64::Engine as _;
                base64::engine::general_purpose::STANDARD
                    .decode(given)
                    .ok()
                    .is_some_and(|raw| raw == format!("\0{user}\0{password}").into_bytes())
            });
            authed = ok;
            let reply = if ok {
                reply_line(235, "authenticated")
            } else {
                reply_line(535, "bad credentials")
            };
            write.write_all(reply.as_bytes()).await?;
        } else if upper.starts_with("MAIL FROM:") {
            if !authed {
                write
                    .write_all(reply_line(530, "authenticate first").as_bytes())
                    .await?;
                continue;
            }
            let code = script.mail_from_reply.unwrap_or(250);
            if code == 250 {
                mail_from = angle(&cmd);
            }
            write.write_all(reply_line(code, "mail").as_bytes()).await?;
        } else if upper.starts_with("RCPT TO:") {
            let code = script.rcpt_reply.unwrap_or(250);
            if code == 250 {
                rcpt_to.push(angle(&cmd));
            }
            write.write_all(reply_line(code, "rcpt").as_bytes()).await?;
        } else if upper == "DATA" {
            let code = script.data_reply.unwrap_or(354);
            write.write_all(reply_line(code, "data").as_bytes()).await?;
            if code != 354 {
                continue;
            }
            let mut data = String::new();
            loop {
                line.clear();
                if read.read_line(&mut line).await? == 0 {
                    return Ok(());
                }
                if line == ".\r\n" {
                    break;
                }
                data.push_str(
                    line.strip_prefix('.')
                        .filter(|_| line.starts_with(".."))
                        .unwrap_or(&line),
                );
            }
            let received = Received {
                mail_from: mail_from.clone(), // clone: the session keeps its envelope
                rcpt_to: std::mem::take(&mut rcpt_to),
                data,
            };
            match script.after_data {
                AfterData::Close => {
                    messages.lock().expect("messages").push(received);
                    return Ok(());
                }
                AfterData::Hang => {
                    messages.lock().expect("messages").push(received);
                    std::future::pending::<()>().await;
                }
                AfterData::Reply => {
                    let code = script.end_reply.unwrap_or(250);
                    if code == 250 {
                        messages.lock().expect("messages").push(received);
                    }
                    write
                        .write_all(reply_line(code, "queued").as_bytes())
                        .await?;
                }
            }
        } else if upper == "QUIT" {
            write.write_all(reply_line(221, "bye").as_bytes()).await?;
            return Ok(());
        } else {
            write
                .write_all(reply_line(502, "not implemented").as_bytes())
                .await?;
        }
    }
}

/// `MAIL FROM:<a@b>` → `a@b`.
fn angle(command: &str) -> String {
    command
        .split_once('<')
        .and_then(|(_, rest)| rest.split_once('>'))
        .map(|(inside, _)| inside.to_owned())
        .unwrap_or_default()
}
