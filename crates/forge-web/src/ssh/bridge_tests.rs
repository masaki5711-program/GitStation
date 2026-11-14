#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::{Arc, Mutex};

use axum::extract::Request;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use forge_auth::AccountKind;

use super::*;
use crate::ssh::command::parse_command;

/// What the stub route saw of one request.
#[derive(Debug, Clone)]
struct Seen {
    method: String,
    uri: String,
    git_protocol: Option<String>,
    content_type: Option<String>,
    principal_user: Option<i64>,
    peer: Option<IpAddr>,
    body: Vec<u8>,
}

type Answer = Box<dyn Fn(&Seen) -> axum::response::Response + Send + Sync>;

fn stub(answer: Answer) -> (Router, Arc<Mutex<Vec<Seen>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    let answer = Arc::new(answer);
    // A stub standing in for the product router (unit tests only; the
    // SSH server is always given `app()`'s router).
    let router = Router::default().fallback(move |req: Request| {
        let log = Arc::clone(&log);
        let answer = Arc::clone(&answer);
        async move {
            let (parts, body) = req.into_parts();
            let body = axum::body::to_bytes(body, 1 << 20).await.unwrap().to_vec();
            let header = |name: &str| {
                parts
                    .headers
                    .get(name)
                    .map(|v| v.to_str().unwrap().to_owned())
            };
            let seen = Seen {
                method: parts.method.to_string(),
                uri: parts.uri.to_string(),
                git_protocol: header("git-protocol"),
                content_type: header("content-type"),
                principal_user: parts
                    .extensions
                    .get::<Principal>()
                    .and_then(Principal::user)
                    .map(UserId::get),
                peer: parts
                    .extensions
                    .get::<crate::server::PeerAddr>()
                    .map(|p| p.0),
                body,
            };
            let res = answer(&seen);
            log.lock().unwrap().push(seen);
            res
        }
    });
    (router, seen)
}

fn caller() -> Caller {
    Caller {
        principal: Principal::authenticated(
            UserId::new(7),
            false,
            true,
            forge_auth::Credential::Token(forge_auth::SSH_KEY_SCOPES.to_vec()),
            AccountKind::Human,
        ),
        peer: IpAddr::from([192, 0, 2, 9]),
        ssh_key_id: 3,
        user_id: UserId::new(7),
        fingerprint: "SHA256:5ytgilOc3oe+dzOfNlMNUlML18fMocF7QAyCsvhkxkA".to_owned(),
        idle: std::time::Duration::from_secs(10),
    }
}

/// A receive-pack command list: one update with these capabilities.
fn commands(caps: &str) -> Vec<u8> {
    let old = "1".repeat(40);
    let new = "2".repeat(40);
    let line = format!("{old} {new} refs/heads/main\0{caps}\n");
    format!("{:04x}{line}0000", line.len() + 4).into_bytes()
}

fn ok_body(bytes: &'static [u8]) -> axum::response::Response {
    (StatusCode::OK, bytes).into_response()
}

fn rid() -> RequestId {
    RequestId::parse("req-ssh-test").unwrap()
}

/// The errors.toml test of FG-GIT-0034.
#[tokio::test]
async fn upload_pack_over_ssh_without_protocol_v2_is_refused() {
    let (router, seen) = stub(Box::new(|_| ok_body(b"")));
    let cmd = parse_command(b"git-upload-pack 'alice/web.git'").unwrap();
    for protocol in [None, Some("version=1"), Some("version=0")] {
        let mut out = Vec::new();
        let outcome = run(
            &router,
            &caller(),
            &cmd,
            protocol,
            &rid(),
            &b""[..],
            &mut out,
        )
        .await;
        assert_eq!(outcome.exit_status, 1);
        assert_eq!(outcome.refused, Some("FG-GIT-0034"));
        let text = String::from_utf8(out).unwrap();
        assert!(text[4..].starts_with("ERR FG-GIT-0034 "), "{text}");
        assert!(text.contains("(request_id: req-ssh-test)"), "{text}");
    }
    assert!(seen.lock().unwrap().is_empty(), "no route was called");
}

/// The errors.toml test of FG-GIT-0036: an error answer that is not the
/// JSON error body.
#[tokio::test]
async fn an_unreadable_bridge_answer_is_fg_git_0036() {
    let (router, _) = stub(Box::new(|_| {
        (StatusCode::INTERNAL_SERVER_ERROR, "not json").into_response()
    }));
    let cmd = parse_command(b"git-receive-pack 'alice/web.git'").unwrap();
    let mut out = Vec::new();
    let outcome = run(&router, &caller(), &cmd, None, &rid(), &b""[..], &mut out).await;
    assert_eq!(outcome.exit_status, 1);
    assert_eq!(outcome.refused, Some("FG-GIT-0036"));
    let text = String::from_utf8(out).unwrap();
    assert!(text[4..].starts_with("ERR FG-GIT-0036 "), "{text}");
}

#[tokio::test]
async fn a_route_refusal_reaches_git_as_one_err_line_with_its_own_request_id() {
    let (router, _) = stub(Box::new(|_| {
        (
            StatusCode::NOT_FOUND,
            [(REQUEST_ID_HEADER, "req-http-1")],
            axum::Json(serde_json::json!({
                "error_code": "FG-REPO-0005",
                "user_message": "見つかりません。",
                "request_id": "req-http-1",
            })),
        )
            .into_response()
    }));
    let cmd = parse_command(b"git-upload-pack 'alice/web.git'").unwrap();
    let mut out = Vec::new();
    let outcome = run(
        &router,
        &caller(),
        &cmd,
        Some("version=2"),
        &rid(),
        &b""[..],
        &mut out,
    )
    .await;
    assert_eq!(outcome.exit_status, 1);
    assert_eq!(outcome.refused, Some("FG-REPO-0005"));
    assert_eq!(outcome.http_request_ids, vec!["req-http-1".to_owned()]);
    let text = String::from_utf8(out).unwrap();
    assert_eq!(
        &text[4..],
        "ERR FG-REPO-0005 見つかりません。 (request_id: req-http-1)\n"
    );
}

#[tokio::test]
async fn upload_pack_v2_is_one_post_per_request_until_a_lone_flush() {
    let (router, seen) = stub(Box::new(|s| {
        if s.method == "GET" {
            ok_body(b"000eversion 2\n0000")
        } else {
            ok_body(b"000aanswer0000")
        }
    }));
    let cmd = parse_command(b"git-upload-pack '/alice/web.git'").unwrap();
    // ls-refs (command, delim, an argument, flush), fetch, then a lone flush.
    let input = b"0014command=ls-refs\n00010008peel0000\
0012command=fetch\n0001000cdeepen 10000\
0000"
        .to_vec();
    let mut out = Vec::new();
    let outcome = run(
        &router,
        &caller(),
        &cmd,
        Some("version=2"),
        &rid(),
        &input[..],
        &mut out,
    )
    .await;
    assert_eq!(outcome.exit_status, 0, "{outcome:?}");
    assert_eq!(outcome.refused, None);
    assert_eq!(
        String::from_utf8(out).unwrap(),
        "000eversion 2\n0000000aanswer0000000aanswer0000"
    );
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 3, "{seen:?}");
    assert_eq!(seen[0].method, "GET");
    assert_eq!(
        seen[0].uri,
        "/alice/web.git/info/refs?service=git-upload-pack"
    );
    assert_eq!(seen[1].uri, "/alice/web.git/git-upload-pack");
    assert_eq!(
        seen[1].content_type.as_deref(),
        Some("application/x-git-upload-pack-request")
    );
    assert_eq!(seen[1].body, b"0014command=ls-refs\n00010008peel0000");
    assert_eq!(seen[2].body, b"0012command=fetch\n0001000cdeepen 10000");
    for s in seen.iter() {
        assert_eq!(s.git_protocol.as_deref(), Some("version=2"));
        assert_eq!(s.principal_user, Some(7), "the SSH key's principal");
        assert_eq!(s.peer, Some(IpAddr::from([192, 0, 2, 9])));
    }
}

#[tokio::test]
async fn receive_pack_strips_the_http_prefix_and_sends_commands_and_pack_to_eof() {
    let (router, seen) = stub(Box::new(|s| {
        if s.method == "GET" {
            ok_body(b"001f# service=git-receive-pack\n00000013advertised ref\n0000")
        } else {
            ok_body(b"0012report-status\n0000")
        }
    }));
    let cmd = parse_command(b"git-receive-pack 'alice/web.git'").unwrap();
    let old = "1".repeat(40);
    let new = "2".repeat(40);
    let line = format!("{old} {new} refs/heads/main\0report-status\n");
    let mut input = format!("{:04x}{line}0000", line.len() + 4).into_bytes();
    input.extend_from_slice(b"PACK\0\0\0\x02rest-of-pack");
    let mut out = Vec::new();
    let outcome = run(&router, &caller(), &cmd, None, &rid(), &input[..], &mut out).await;
    assert_eq!(outcome.exit_status, 0, "{outcome:?}");
    assert_eq!(
        String::from_utf8(out).unwrap(),
        "0013advertised ref\n00000012report-status\n0000"
    );
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert_eq!(
        seen[0].uri,
        "/alice/web.git/info/refs?service=git-receive-pack"
    );
    assert_eq!(seen[1].uri, "/alice/web.git/git-receive-pack");
    assert_eq!(seen[1].body, input, "the command list and the whole pack");
    assert_eq!(seen[1].git_protocol, None);
}

#[tokio::test]
async fn a_delete_only_push_needs_no_eof_from_the_client() {
    let (router, seen) = stub(Box::new(|s| {
        if s.method == "GET" {
            ok_body(b"000fadvertised\n0000")
        } else {
            ok_body(b"000eunpack ok\n0000")
        }
    }));
    let cmd = parse_command(b"git-receive-pack 'alice/web.git'").unwrap();
    let old = "1".repeat(40);
    let zero = "0".repeat(40);
    let line = format!("{old} {zero} refs/heads/gone\0report-status\n");
    let commands = format!("{:04x}{line}0000", line.len() + 4).into_bytes();
    // The client keeps its side open (as git does after deletes only).
    let (mut client, server) = tokio::io::duplex(1 << 16);
    client.write_all(&commands).await.unwrap();
    let mut out = Vec::new();
    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        run(&router, &caller(), &cmd, None, &rid(), server, &mut out),
    )
    .await
    .expect("no wait for EOF");
    assert_eq!(outcome.exit_status, 0, "{outcome:?}");
    assert_eq!(seen.lock().unwrap()[1].body, commands);
    drop(client);
}

#[tokio::test]
async fn nothing_to_push_ends_without_a_post() {
    let (router, seen) = stub(Box::new(|_| ok_body(b"000fadvertised\n0000")));
    let cmd = parse_command(b"git-receive-pack 'alice/web.git'").unwrap();
    let mut out = Vec::new();
    let outcome = run(
        &router,
        &caller(),
        &cmd,
        None,
        &rid(),
        &b"0000"[..],
        &mut out,
    )
    .await;
    assert_eq!(outcome.exit_status, 0);
    assert_eq!(seen.lock().unwrap().len(), 1, "the advertisement only");
}

#[tokio::test]
async fn a_broken_pkt_line_from_the_client_ends_the_command_quietly() {
    let (router, seen) = stub(Box::new(|_| ok_body(b"000eversion 2\n0000")));
    let cmd = parse_command(b"git-upload-pack 'alice/web.git'").unwrap();
    for input in [&b"zzzz"[..], &b"0003"[..], &b"00ff short"[..], &b"00"[..]] {
        let mut out = Vec::new();
        let outcome = run(
            &router,
            &caller(),
            &cmd,
            Some("version=2"),
            &rid(),
            input,
            &mut out,
        )
        .await;
        assert_eq!(outcome.exit_status, 1, "{input:?}");
        assert_eq!(outcome.refused, None, "a client fault, not a refusal");
    }
    // Only the advertisements: no POST with a broken body.
    assert!(seen.lock().unwrap().iter().all(|s| s.method == "GET"));
}

#[test]
fn deletes_are_recognized_only_in_their_exact_shape() {
    let pkt = |line: &str| format!("{:04x}{line}", line.len() + 4).into_bytes();
    let old = "a".repeat(40);
    let zero = "0".repeat(40);
    assert!(is_delete(&pkt(&format!("{old} {zero} refs/heads/x\n"))));
    assert!(is_delete(&pkt(&format!(
        "{old} {zero} refs/heads/x\0report-status\n"
    ))));
    assert!(is_delete(&pkt(&format!(
        "{} {} refs/heads/x",
        "a".repeat(64),
        "0".repeat(64)
    ))));
    assert!(!is_delete(&pkt(&format!("{zero} {old} refs/heads/x\n"))));
    assert!(!is_delete(&pkt(&format!("shallow {old}\n"))));
    assert!(!is_delete(&pkt("push-cert\0push-options\n")));
    assert!(!is_delete(b"00"));
}

#[test]
fn the_service_prefix_is_stripped_only_when_present() {
    assert_eq!(
        strip_service_prefix(b"001f# service=git-receive-pack\n00000008abcd").unwrap(),
        b"0008abcd"
    );
    assert_eq!(
        strip_service_prefix(b"000eversion 2\n").unwrap(),
        b"000eversion 2\n"
    );
    assert_eq!(
        strip_service_prefix(b"001f# service=git-rec"),
        None,
        "need more"
    );
    assert_eq!(strip_service_prefix(b"00"), None);
    assert_eq!(strip_service_prefix(b"zzzzz").unwrap(), b"zzzzz");
}

#[test]
fn only_version_2_counts_as_v2() {
    assert!(wants_v2(Some("version=2")));
    assert!(wants_v2(Some("object-format=sha1:version=2")));
    assert!(!wants_v2(Some("version=20")));
    assert!(!wants_v2(None));
}

#[tokio::test]
async fn push_options_are_refused_on_side_band_3_without_a_post() {
    let (router, seen) = stub(Box::new(|_| ok_body(b"000fadvertised\n0000")));
    let cmd = parse_command(b"git-receive-pack 'alice/web.git'").unwrap();
    let mut input = commands("report-status side-band-64k push-options");
    input.extend_from_slice(b"000bopt=1\n0000");
    let mut out = Vec::new();
    let outcome = run(&router, &caller(), &cmd, None, &rid(), &input[..], &mut out).await;
    assert_eq!(outcome.exit_status, 1);
    assert_eq!(outcome.refused, Some("FG-GIT-0033"));
    let answer = &out[b"000fadvertised\n0000".len()..];
    assert_eq!(answer[4], 3, "side-band 3: {answer:?}");
    assert!(String::from_utf8_lossy(&answer[5..]).starts_with("FG-GIT-0033 "));
    assert_eq!(seen.lock().unwrap().len(), 1, "no POST");
}

#[tokio::test]
async fn a_push_refusal_after_the_pack_goes_to_side_band_3() {
    let (router, _) = stub(Box::new(|s| {
        if s.method == "GET" {
            ok_body(b"000fadvertised\n0000")
        } else {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                axum::Json(serde_json::json!({
                    "error_code": "FG-GIT-0004",
                    "user_message": "busy",
                    "request_id": "req-http-9",
                })),
            )
                .into_response()
        }
    }));
    let cmd = parse_command(b"git-receive-pack 'alice/web.git'").unwrap();
    let mut input = commands("report-status side-band-64k");
    input.extend_from_slice(b"PACK-bytes");
    let mut out = Vec::new();
    let outcome = run(&router, &caller(), &cmd, None, &rid(), &input[..], &mut out).await;
    assert_eq!(outcome.refused, Some("FG-GIT-0004"));
    let answer = &out[b"000fadvertised\n0000".len()..];
    assert_eq!(answer[4], 3);
    assert_eq!(&answer[5..], b"FG-GIT-0004 busy (request_id: req-http-9)\n");
    // Without a side-band it stays an ERR line.
    let mut plain = commands("report-status");
    plain.extend_from_slice(b"PACK-bytes");
    let mut out = Vec::new();
    run(&router, &caller(), &cmd, None, &rid(), &plain[..], &mut out).await;
    assert!(out[b"000fadvertised\n0000".len() + 4..].starts_with(b"ERR FG-GIT-0004"));
}

#[tokio::test]
async fn the_route_gets_exactly_version_2_never_the_clients_text() {
    let (router, seen) = stub(Box::new(|_| ok_body(b"000eversion 2\n0000")));
    let cmd = parse_command(b"git-upload-pack 'alice/web.git'").unwrap();
    let mut out = Vec::new();
    run(
        &router,
        &caller(),
        &cmd,
        Some("version=2:object-format=sha1:x=\u{1}"),
        &rid(),
        &b"0000"[..],
        &mut out,
    )
    .await;
    assert_eq!(
        seen.lock().unwrap()[0].git_protocol.as_deref(),
        Some("version=2")
    );
}

#[tokio::test]
async fn a_client_idle_inside_a_request_ends_the_command() {
    let (router, seen) = stub(Box::new(|_| ok_body(b"000eversion 2\n0000")));
    let cmd = parse_command(b"git-upload-pack 'alice/web.git'").unwrap();
    let mut idle = caller();
    idle.idle = std::time::Duration::from_millis(200);
    // A request is begun and then nothing more comes; the side stays open.
    let (mut client, server) = tokio::io::duplex(1 << 16);
    tokio::io::AsyncWriteExt::write_all(&mut client, b"0014command=ls-refs\n")
        .await
        .unwrap();
    let mut out = Vec::new();
    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        run(
            &router,
            &idle,
            &cmd,
            Some("version=2"),
            &rid(),
            server,
            &mut out,
        ),
    )
    .await
    .expect("the idle bound ends it");
    assert_eq!(outcome.exit_status, 1);
    drop(client);
    let _ = seen; // the POST may or may not have started; its body never ends
}
