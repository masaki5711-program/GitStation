#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)] // tests may fail loudly (rust/testing.md)

use forge_errors::ErrorCode;

use super::*;

const A: &str = "1111111111111111111111111111111111111111";
const B: &str = "2222222222222222222222222222222222222222";
const Z: &str = "0000000000000000000000000000000000000000";
const CAPS: &str =
    "report-status side-band-64k quiet atomic ofs-delta agent=git/2.43.0 object-format=sha1";

fn pkt(s: &[u8]) -> Vec<u8> {
    let mut out = format!("{:04x}", s.len() + 4).into_bytes();
    out.extend_from_slice(s);
    out
}

fn first(old: &str, new: &str, name: &str, caps: &str) -> Vec<u8> {
    pkt(format!("{old} {new} {name}\0{caps}\n").as_bytes())
}

fn next(old: &str, new: &str, name: &str) -> Vec<u8> {
    pkt(format!("{old} {new} {name}\n").as_bytes())
}

fn parse(body: &[u8]) -> Result<ReceivePackRequest, ForgeError> {
    match CommandParser::new().feed(body)? {
        Parsed::Done(r) => Ok(r),
        Parsed::NeedMore => panic!("incomplete"),
    }
}

fn reason(err: &ForgeError) -> String {
    err.to_string()
}

#[test]
fn a_push_section_is_parsed_with_its_capabilities() {
    let mut body = first(A, B, "refs/heads/main", CAPS);
    body.extend(next(Z, B, "refs/tags/v1"));
    body.extend(next(A, Z, "refs/heads/old"));
    body.extend_from_slice(b"0000PACK....");
    let r = parse(&body).expect("valid");
    assert_eq!(r.commands.len(), 3);
    assert_eq!(r.report_status, Some(ReportStatus::V1));
    assert!(r.sideband);
    assert_eq!(r.hex_len, 40);
    assert_eq!(r.section_len, body.len() - b"PACK....".len());
    assert!(r.commands[1].is_create());
    assert!(r.commands[2].is_delete());
    assert!(r.expects_pack());
    assert_eq!(
        r.commands[0].name.as_ref().expect("valid").as_str(),
        "refs/heads/main"
    );
}

#[test]
fn the_section_is_read_incrementally() {
    let mut body = first(A, B, "refs/heads/main", "report-status-v2");
    body.extend_from_slice(b"0000");
    let mut parser = CommandParser::new();
    for end in 0..body.len() {
        assert!(matches!(
            parser.feed(&body[..end]).expect("partial"),
            Parsed::NeedMore
        ));
    }
    let Parsed::Done(r) = parser.feed(&body).expect("complete") else {
        panic!("done");
    };
    assert_eq!(r.report_status, Some(ReportStatus::V2));
    assert!(!r.sideband);
}

#[test]
fn the_probe_request_has_no_commands() {
    let r = parse(b"0000").expect("probe");
    assert!(r.commands.is_empty());
    assert_eq!(r.report_status, None);
    assert!(!r.expects_pack());
}

#[test]
fn a_push_without_report_status_is_refused() {
    let mut body = first(A, B, "refs/heads/main", "side-band-64k");
    body.extend_from_slice(b"0000");
    let err = parse(&body).expect_err("refused");
    assert_eq!(err.code(), ErrorCode::GitProtocolInvalid);
    assert!(
        reason(&err).contains("reason=report_status_required"),
        "{err}"
    );
    let mut body = next(A, B, "refs/heads/main");
    body.extend_from_slice(b"0000");
    let err = parse(&body).expect_err("no capabilities at all");
    assert!(
        reason(&err).contains("reason=report_status_required"),
        "{err}"
    );
}

#[test]
fn a_signed_push_is_refused() {
    let mut body = pkt(format!("push-cert\0{CAPS}\n").as_bytes());
    body.extend(pkt(b"certificate version 0.1\n"));
    let err = CommandParser::new().feed(&body).expect_err("refused");
    assert!(reason(&err).contains("reason=push_cert"), "{err}");
}

#[test]
fn capabilities_are_only_on_the_first_line_and_only_known_ones() {
    let mut body = first(A, B, "refs/heads/main", "report-status");
    body.extend(first(A, B, "refs/heads/x", "report-status"));
    let err = CommandParser::new().feed(&body).expect_err("refused");
    assert!(
        reason(&err).contains("detail=capabilities_not_first"),
        "{err}"
    );
    for cap in ["push-options", "side-band", "bogus=1", "object-format=md5"] {
        let body = first(A, B, "refs/heads/main", &format!("report-status {cap}"));
        let err = CommandParser::new().feed(&body).expect_err(cap);
        assert!(
            reason(&err).contains("reason=capability_unsupported"),
            "{cap}: {err}"
        );
    }
}

#[test]
fn object_ids_must_match_the_object_format() {
    let wide_a = "a".repeat(64);
    let wide_b = "b".repeat(64);
    let mut body = first(
        &wide_a,
        &wide_b,
        "refs/heads/main",
        "report-status object-format=sha256",
    );
    body.extend_from_slice(b"0000");
    assert_eq!(parse(&body).expect("sha256").hex_len, 64);
    for (old, new) in [
        (A, &wide_b[..]),
        ("ABCDEF1111111111111111111111111111111111", B),
        (Z, Z),
        (A, A),
        ("xyz", B),
    ] {
        let body = first(old, new, "refs/heads/main", "report-status");
        let err = CommandParser::new().feed(&body).expect_err(old);
        assert!(reason(&err).contains("reason=command_invalid"), "{err}");
    }
}

#[test]
fn a_ref_twice_and_misplaced_shallow_lines_are_refused() {
    let mut body = first(A, B, "refs/heads/main", "report-status");
    body.extend(next(B, A, "refs/heads/main"));
    let err = CommandParser::new().feed(&body).expect_err("duplicate");
    assert!(reason(&err).contains("reason=duplicate_ref"), "{err}");

    let mut body = pkt(format!("shallow {A}\n").as_bytes());
    body.extend(first(A, B, "refs/heads/main", "report-status"));
    body.extend_from_slice(b"0000");
    assert_eq!(parse(&body).expect("shallow first").commands.len(), 1);

    let mut body = first(A, B, "refs/heads/main", "report-status");
    body.extend(pkt(format!("shallow {A}\n").as_bytes()));
    let err = CommandParser::new().feed(&body).expect_err("shallow after");
    assert!(reason(&err).contains("reason=shallow_misplaced"), "{err}");
}

#[test]
fn ref_names_with_control_bytes_are_a_protocol_error_and_bad_names_are_kept() {
    let body = first(A, B, "refs/heads/a\u{1}b", "report-status");
    let err = CommandParser::new().feed(&body).expect_err("control");
    assert!(reason(&err).contains("detail=ref_bytes"), "{err}");

    let mut body = first(A, B, "refs/heads/a..b", "report-status");
    body.extend(next(A, B, "main"));
    body.extend_from_slice(b"0000");
    let r = parse(&body).expect("kept with their refusal");
    for c in &r.commands {
        let err = c.name.as_ref().expect_err("invalid");
        assert_eq!(err.code(), ErrorCode::InvalidRefName);
    }
    assert_eq!(r.commands[1].raw_name, "main");
}

#[test]
fn a_hex_like_ref_name_is_ambiguous() {
    let hex40 = "abcdef0123456789abcdef0123456789abcdef01";
    let mut body = first(Z, B, &format!("refs/heads/{hex40}"), "report-status");
    body.extend(next(Z, B, &format!("refs/tags/{}", hex40.to_uppercase())));
    body.extend(next(Z, B, &format!("refs/heads/{}", "c".repeat(64))));
    body.extend(next(Z, B, "refs/heads/cafe"));
    body.extend(next(Z, B, &format!("refs/heads/x/{hex40}")));
    body.extend_from_slice(b"0000");
    let r = parse(&body).expect("parsed");
    for c in &r.commands[..3] {
        let err = c.name.as_ref().expect_err("ambiguous");
        assert_eq!(err.code(), ErrorCode::AmbiguousRefName, "{}", c.raw_name);
    }
    // Short hex words and a hex component below a directory are names.
    assert!(r.commands[3].name.is_ok());
    assert!(r.commands[4].name.is_ok());
}

#[test]
fn too_many_commands_or_bytes_are_refused() {
    let mut body = first(Z, B, "refs/heads/b0", "report-status");
    for i in 1..=MAX_PUSH_COMMANDS {
        body.extend(next(Z, B, &format!("refs/heads/b{i}")));
    }
    let err = CommandParser::new().feed(&body).expect_err("1001 commands");
    assert!(reason(&err).contains("reason=too_many_commands"), "{err}");

    let mut body = first(
        Z,
        B,
        &format!("refs/heads/{}", "x".repeat(1000)),
        "report-status",
    );
    let mut i = 0;
    while body.len() <= MAX_COMMAND_SECTION {
        body.extend(next(Z, B, &format!("refs/heads/{i}-{}", "y".repeat(1000))));
        i += 1;
    }
    let err = CommandParser::new().feed(&body).expect_err("over 1 MiB");
    assert_eq!(err.code(), ErrorCode::GitRequestTooLarge);
    assert!(reason(&err).contains("limit=command_section"), "{err}");
}
