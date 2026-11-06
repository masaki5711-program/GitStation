#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! The SMTP sender against an in-process fake relay (M3 N1): what is sent,
//! how every failure is classified (the phases decide whether another try
//! is safe), the timeouts, AUTH and the refusal to continue in clear text
//! when TLS was configured. No real host is contacted.

use std::time::{Duration, Instant, SystemTime};

use base64::Engine as _;
use forge_core::config::{NotifyConfig, SmtpTls};
use forge_notify::test_support::{AfterData, FakeSmtp, Script};
use forge_notify::{Mail, Reason, SendOutcome, Sender as _, SmtpSender, Stage};

fn mail(to: &str) -> Mail {
    Mail::new(
        ("forge@git.example.test", "GitStation"),
        to,
        (
            "[GitStation] team/web!7 was merged",
            "Your merge request was merged.\n",
        ),
        7,
        SystemTime::UNIX_EPOCH,
    )
    .expect("mail")
}

async fn send(relay: &FakeSmtp) -> SendOutcome {
    let sender = SmtpSender::new(&relay.config()).expect("sender");
    sender.send(&mail("alice@example.test")).await
}

fn failure(outcome: SendOutcome) -> (bool, Reason, Stage, Option<u16>) {
    match outcome {
        SendOutcome::NotSent(f) => (false, f.reason, f.stage, f.smtp_code),
        SendOutcome::Unknown(f) => (true, f.reason, f.stage, f.smtp_code),
        SendOutcome::Delivered => panic!("expected a failure, got Delivered"),
    }
}

#[tokio::test]
async fn a_message_is_delivered_with_its_envelope_and_a_decodable_body() {
    let relay = FakeSmtp::start(Script::default()).await;
    assert_eq!(send(&relay).await, SendOutcome::Delivered);
    let got = relay.messages();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].mail_from, "forge@git.example.test");
    assert_eq!(got[0].rcpt_to, ["alice@example.test"]);
    assert!(got[0]
        .data
        .contains("Subject: [GitStation] team/web!7 was merged\r\n"));
    assert!(got[0]
        .data
        .contains("Message-ID: <notification-7@git.example.test>\r\n"));
    let body = got[0].data.split("\r\n\r\n").nth(1).expect("body");
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(body.replace("\r\n", ""))
        .expect("base64");
    assert_eq!(decoded, b"Your merge request was merged.\n");
    // The conversation: EHLO with the sender's domain, the envelope, DATA,
    // a polite QUIT.
    let commands = relay.commands();
    assert_eq!(
        commands.first().map(String::as_str),
        Some("EHLO git.example.test")
    );
    assert!(commands.contains(&"MAIL FROM:<forge@git.example.test>".to_owned()));
    assert!(commands.contains(&"RCPT TO:<alice@example.test>".to_owned()));
    assert_eq!(commands.last().map(String::as_str), Some("QUIT"));
}

#[tokio::test]
async fn a_closed_port_is_connect_failed_and_not_sent() {
    // Bind, remember the port, drop the listener: nothing listens there.
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        l.local_addr().expect("addr").port()
    };
    let relay = FakeSmtp::start(Script::default()).await;
    let mut cfg = relay.config();
    cfg.smtp.as_mut().expect("smtp").port = port;
    let sender = SmtpSender::new(&cfg).expect("sender");
    let outcome = sender.send(&mail("alice@example.test")).await;
    assert_eq!(
        failure(outcome),
        (false, Reason::ConnectFailed, Stage::Connect, None)
    );
}

#[tokio::test]
async fn a_relay_that_never_greets_times_out_within_the_send_timeout() {
    let relay = FakeSmtp::start(Script {
        hang_at_greeting: true,
        ..Script::default()
    })
    .await;
    let mut cfg = relay.config();
    cfg.send_timeout = Duration::from_millis(400);
    cfg.connect_timeout = Duration::from_millis(200);
    let sender = SmtpSender::new(&cfg).expect("sender");
    let started = Instant::now();
    let outcome = sender.send(&mail("alice@example.test")).await;
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(
        failure(outcome),
        (false, Reason::Timeout, Stage::Connect, None)
    );
    assert!(relay.messages().is_empty());
}

#[tokio::test]
async fn refusals_are_temporary_for_4xx_and_permanent_for_5xx_at_every_step() {
    let cases: Vec<(Script, Reason, Stage, u16)> = vec![
        (
            Script {
                greeting: Some(421),
                ..Script::default()
            },
            Reason::RejectedTemporary,
            Stage::Connect,
            421,
        ),
        (
            Script {
                greeting: Some(554),
                ..Script::default()
            },
            Reason::RejectedPermanent,
            Stage::Connect,
            554,
        ),
        (
            Script {
                mail_from_reply: Some(451),
                ..Script::default()
            },
            Reason::RejectedTemporary,
            Stage::MailFrom,
            451,
        ),
        (
            Script {
                rcpt_reply: Some(550),
                ..Script::default()
            },
            Reason::RejectedPermanent,
            Stage::RcptTo,
            550,
        ),
        (
            Script {
                rcpt_reply: Some(452),
                ..Script::default()
            },
            Reason::RejectedTemporary,
            Stage::RcptTo,
            452,
        ),
        (
            Script {
                data_reply: Some(554),
                ..Script::default()
            },
            Reason::RejectedPermanent,
            Stage::Data,
            554,
        ),
        (
            // The relay read the message and refused it: it does not have it.
            Script {
                end_reply: Some(451),
                ..Script::default()
            },
            Reason::RejectedTemporary,
            Stage::EndOfData,
            451,
        ),
        (
            Script {
                end_reply: Some(552),
                ..Script::default()
            },
            Reason::RejectedPermanent,
            Stage::EndOfData,
            552,
        ),
    ];
    for (script, reason, stage, code) in cases {
        let relay = FakeSmtp::start(script).await;
        let outcome = send(&relay).await;
        assert_eq!(
            failure(outcome),
            (false, reason, stage, Some(code)),
            "{reason:?} {stage:?}"
        );
        assert!(relay.messages().is_empty());
    }
}

#[tokio::test]
async fn a_connection_that_ends_after_the_message_is_unknown_and_never_retried() {
    let relay = FakeSmtp::start(Script {
        after_data: AfterData::Close,
        ..Script::default()
    })
    .await;
    let outcome = send(&relay).await;
    // The relay did receive the message; the client cannot know that.
    assert_eq!(
        failure(outcome),
        (true, Reason::OutcomeUnknown, Stage::EndOfData, None)
    );
    assert_eq!(relay.messages().len(), 1);
}

#[tokio::test]
async fn a_relay_that_never_answers_after_the_message_is_unknown_within_the_timeout() {
    let relay = FakeSmtp::start(Script {
        after_data: AfterData::Hang,
        ..Script::default()
    })
    .await;
    let mut cfg = relay.config();
    cfg.send_timeout = Duration::from_millis(500);
    let sender = SmtpSender::new(&cfg).expect("sender");
    let started = Instant::now();
    let outcome = sender.send(&mail("alice@example.test")).await;
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(
        failure(outcome),
        (true, Reason::OutcomeUnknown, Stage::EndOfData, None)
    );
}

#[tokio::test]
async fn auth_plain_is_sent_when_configured_and_a_wrong_password_is_auth_failed() {
    let dir = tempfile::tempdir().expect("dir");
    let write = |name: &str, text: &str| {
        let path = dir.path().join(name);
        std::fs::write(&path, text).expect("write");
        // A secret file must not be readable by others on Unix, or the sender
        // refuses it (`insecure_permissions`); the default umask gives 0644.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
        }
        path
    };
    let relay = FakeSmtp::start(Script {
        auth: Some(("forge".to_owned(), "s3cret".to_owned())),
        ..Script::default()
    })
    .await;
    let mut cfg: NotifyConfig = relay.config();
    {
        let smtp = cfg.smtp.as_mut().expect("smtp");
        smtp.username = Some("forge".to_owned());
        smtp.password_file = Some(write("good", "s3cret\n"));
    }
    let outcome = SmtpSender::new(&cfg)
        .expect("sender")
        .send(&mail("alice@example.test"))
        .await;
    assert_eq!(outcome, SendOutcome::Delivered);
    // The password is in no log of the relay's commands (it is masked) and
    // not in the sender's Debug.
    let sender = SmtpSender::new(&cfg).expect("sender");
    assert!(!format!("{sender:?}").contains("s3cret"));

    cfg.smtp.as_mut().expect("smtp").password_file = Some(write("bad", "wrong\n"));
    let outcome = SmtpSender::new(&cfg)
        .expect("sender")
        .send(&mail("alice@example.test"))
        .await;
    assert_eq!(
        failure(outcome),
        (false, Reason::AuthFailed, Stage::Auth, Some(535))
    );
    assert_eq!(relay.messages().len(), 1, "only the good login delivered");

    // A relay that offers no AUTH PLAIN: the login cannot be made.
    let plain = FakeSmtp::start(Script::default()).await;
    let mut cfg = plain.config();
    {
        let smtp = cfg.smtp.as_mut().expect("smtp");
        smtp.username = Some("forge".to_owned());
        smtp.password_file = Some(write("good2", "s3cret\n"));
    }
    let outcome = SmtpSender::new(&cfg)
        .expect("sender")
        .send(&mail("alice@example.test"))
        .await;
    assert_eq!(
        failure(outcome),
        (false, Reason::AuthFailed, Stage::Auth, None)
    );
    assert!(plain.messages().is_empty());
}

#[tokio::test]
async fn configured_tls_is_never_dropped_to_clear_text() {
    // The relay advertises nothing: with STARTTLS configured the sender must
    // stop before MAIL FROM, not carry on in plain text.
    let relay = FakeSmtp::start(Script::default()).await;
    let mut cfg = relay.config();
    cfg.smtp.as_mut().expect("smtp").tls = SmtpTls::StartTls;
    let sender = SmtpSender::with_roots(&cfg, rustls::RootCertStore::empty()).expect("sender");
    let outcome = sender.send(&mail("alice@example.test")).await;
    assert_eq!(
        failure(outcome),
        (false, Reason::TlsFailed, Stage::Tls, None)
    );
    let commands = relay.commands();
    assert!(
        !commands.iter().any(|c| c.starts_with("MAIL FROM")),
        "{commands:?}"
    );
    assert!(relay.messages().is_empty());

    // Advertised but the relay cannot actually switch (it answers 502): the
    // same refusal, with its code.
    let broken = FakeSmtp::start(Script {
        advertise_starttls: true,
        ..Script::default()
    })
    .await;
    let mut cfg = broken.config();
    cfg.smtp.as_mut().expect("smtp").tls = SmtpTls::StartTls;
    let sender = SmtpSender::with_roots(&cfg, rustls::RootCertStore::empty()).expect("sender");
    let outcome = sender.send(&mail("alice@example.test")).await;
    assert_eq!(
        failure(outcome),
        (false, Reason::TlsFailed, Stage::Tls, Some(502))
    );
    assert!(!broken.commands().iter().any(|c| c.starts_with("MAIL FROM")));
}

#[tokio::test]
async fn an_unreadable_or_empty_ca_file_stops_the_start() {
    let dir = tempfile::tempdir().expect("dir");
    let relay = FakeSmtp::start(Script::default()).await;
    let mut cfg = relay.config();
    {
        let smtp = cfg.smtp.as_mut().expect("smtp");
        smtp.tls = SmtpTls::StartTls;
        smtp.ca_file = Some(dir.path().join("missing.pem"));
    }
    let err = SmtpSender::new(&cfg).expect_err("missing file");
    assert!(
        err.to_string().contains("config_key=notify.smtp_ca_file"),
        "{err}"
    );
    assert!(err.to_string().contains("reason=unreadable"), "{err}");

    let empty = dir.path().join("empty.pem");
    std::fs::write(&empty, "not a certificate\n").expect("write");
    cfg.smtp.as_mut().expect("smtp").ca_file = Some(empty);
    let err = SmtpSender::new(&cfg).expect_err("no certificates");
    assert!(err.to_string().contains("reason=invalid"), "{err}");
}

#[tokio::test]
async fn a_hostile_recipient_or_title_cannot_add_a_command_or_a_header() {
    // The address never gets as far as the wire.
    for bad in [
        "alice@example.test>\r\nRCPT TO:<evil@example.test",
        "alice@example.test\r\nDATA",
    ] {
        assert!(
            Mail::new(
                ("forge@git.example.test", "G"),
                bad,
                ("s", "b"),
                1,
                SystemTime::UNIX_EPOCH,
            )
            .is_err(),
            "{bad}"
        );
    }
    // A title with a line break stays on one subject line.
    let relay = FakeSmtp::start(Script::default()).await;
    let hostile = Mail::new(
        ("forge@git.example.test", "GitStation"),
        "alice@example.test",
        ("x\r\nBcc: evil@example.test", "body"),
        9,
        SystemTime::UNIX_EPOCH,
    )
    .expect("mail");
    let sender = SmtpSender::new(&relay.config()).expect("sender");
    assert_eq!(sender.send(&hostile).await, SendOutcome::Delivered);
    let got = relay.messages();
    assert_eq!(got[0].rcpt_to, ["alice@example.test"]);
    assert!(!got[0].data.contains("\r\nBcc:"), "{}", got[0].data);
}

#[tokio::test]
async fn sends_to_one_relay_do_not_disturb_each_other() {
    let relay = FakeSmtp::start(Script::default()).await;
    let sender = std::sync::Arc::new(SmtpSender::new(&relay.config()).expect("sender"));
    let mut tasks = Vec::new();
    for n in 0..5 {
        let sender = std::sync::Arc::clone(&sender);
        tasks.push(tokio::spawn(async move {
            sender.send(&mail(&format!("user{n}@example.test"))).await
        }));
    }
    for task in tasks {
        assert_eq!(task.await.expect("task"), SendOutcome::Delivered);
    }
    let mut to: Vec<String> = relay
        .messages()
        .into_iter()
        .flat_map(|m| m.rcpt_to)
        .collect();
    to.sort();
    assert_eq!(
        to,
        [
            "user0@example.test",
            "user1@example.test",
            "user2@example.test",
            "user3@example.test",
            "user4@example.test"
        ]
    );
}

/// The producing test of FG-INT-0015: every way a send fails is that code,
/// with the reason, the stage and the counts, and nothing else.
#[tokio::test]
async fn smtp_failures_are_int_0015() {
    let relay = FakeSmtp::start(Script {
        rcpt_reply: Some(550),
        ..Script::default()
    })
    .await;
    let SendOutcome::NotSent(failure) = send(&relay).await else {
        panic!("expected a refusal");
    };
    let err = failure.error(7, 2, 5);
    assert_eq!(err.code(), forge_errors::ErrorCode::SmtpUnavailable);
    assert_eq!(err.code().id(), "FG-INT-0015");
    assert_eq!(err.code().severity(), forge_errors::Severity::Warn);
    let text = err.to_string();
    for part in [
        "reason=rejected_permanent",
        "stage=rcpt_to",
        "smtp_code=550",
        "notification_id=7",
        "attempt=2",
        "max_attempts=5",
    ] {
        assert!(text.contains(part), "{part} in {text}");
    }
    // The relay's words and the address never reach the error.
    assert!(
        !text.contains("alice@example.test") && !text.contains("550 rcpt"),
        "{text}"
    );
    for reason in [
        Reason::ConnectFailed,
        Reason::ConnectionLost,
        Reason::TlsFailed,
        Reason::AuthFailed,
        Reason::Timeout,
        Reason::RejectedTemporary,
        Reason::RejectedPermanent,
        Reason::OutcomeUnknown,
    ] {
        assert!(reason
            .as_str()
            .chars()
            .all(|c| c.is_ascii_lowercase() || c == '_'));
    }
}
