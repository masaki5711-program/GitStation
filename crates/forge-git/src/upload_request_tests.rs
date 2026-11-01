#![allow(clippy::expect_used, clippy::unwrap_used)] // tests may fail loudly

use super::*;
use crate::pkt_line::{encode, Packet};

const A: &str = "1111111111111111111111111111111111111111";
const B: &str = "2222222222222222222222222222222222222222";
const LONG: &str = "3333333333333333333333333333333333333333333333333333333333333333";

fn body(packets: &[Packet]) -> Vec<u8> {
    let mut out = Vec::new();
    for p in packets {
        encode(&mut out, p).expect("encode");
    }
    out
}

fn data(s: &str) -> Packet {
    Packet::Data(s.as_bytes().to_vec())
}

fn reason(err: &ForgeError) -> Option<&str> {
    err.context()
        .iter()
        .find(|(k, _)| *k == "reason")
        .map(|(_, v)| v.as_str())
}

#[test]
fn v0_wants_are_read_with_capabilities_and_haves_ignored() {
    let req = body(&[
        data(&format!(
            "want {A} multi_ack_detailed side-band-64k ofs-delta\n"
        )),
        data(&format!("want {B}\n")),
        data(&format!("shallow {B}\n")),
        data("deepen 1\n"),
        data("filter blob:none\n"),
        Packet::Flush,
        data(&format!("have {B}\n")),
        data("done\n"),
    ]);
    let parsed = parse_upload_pack_request(&req, ProtocolVersion::V0).expect("v0");
    assert_eq!(parsed.command, UploadPackCommand::Negotiate);
    assert_eq!(parsed.wants, [A, B]);
    let v1 = parse_upload_pack_request(&req, ProtocolVersion::V1).expect("v1");
    assert_eq!(v1, parsed);
}

#[test]
fn v2_fetch_wants_come_from_the_arguments_only() {
    let req = body(&[
        data("command=fetch\n"),
        data("agent=git/2.43.0\n"),
        data("object-format=sha1\n"),
        Packet::Delim,
        data("thin-pack\n"),
        data(&format!("want {A}\n")),
        data(&format!("want {LONG}\n")),
        data("want-ref refs/heads/main\n"),
        data(&format!("have {B}\n")),
        data("done\n"),
        Packet::Flush,
    ]);
    let parsed = parse_upload_pack_request(&req, ProtocolVersion::V2).expect("v2");
    assert_eq!(parsed.command, UploadPackCommand::Fetch);
    assert_eq!(parsed.wants, [A, LONG]);
}

#[test]
fn v2_ls_refs_wants_nothing() {
    let req = body(&[
        data("command=ls-refs\n"),
        Packet::Delim,
        data("peel\n"),
        data("ref-prefix refs/heads/\n"),
        Packet::Flush,
    ]);
    let parsed = parse_upload_pack_request(&req, ProtocolVersion::V2).expect("ls-refs");
    assert_eq!(parsed.command, UploadPackCommand::LsRefs);
    assert!(parsed.wants.is_empty());
}

/// `object-info` and `bundle-uri` answer about objects by id without a
/// `want`: only `ls-refs` and `fetch` reach git.
#[test]
fn other_v2_commands_and_second_commands_are_refused() {
    for (packets, command) in [
        (
            vec![
                data("command=object-info\n"),
                Packet::Delim,
                data(&format!("oid {A}\n")),
                Packet::Flush,
            ],
            "object-info",
        ),
        (
            vec![data("command=bundle-uri\n"), Packet::Flush],
            "bundle-uri",
        ),
        (vec![data("agent=x\n"), Packet::Flush], "none"),
        (vec![Packet::Flush], "none"),
        (vec![], "none"),
        (
            vec![
                data("command=ls-refs\n"),
                Packet::Flush,
                data("command=object-info\n"),
                Packet::Flush,
            ],
            "multiple",
        ),
    ] {
        let err =
            parse_upload_pack_request(&body(&packets), ProtocolVersion::V2).expect_err(command);
        assert_eq!(err.code(), ErrorCode::GitProtocolInvalid);
        assert_eq!(reason(&err), Some("command_unsupported"), "{command}");
        let logged = err
            .context()
            .iter()
            .find(|(k, _)| *k == "command")
            .map(|(_, v)| v.clone());
        assert_eq!(logged.as_deref(), Some(command));
    }
}

#[test]
fn a_malformed_want_is_want_invalid() {
    for line in [
        "want \n".to_owned(),
        "want 1234\n".to_owned(),
        format!("want {}\n", A.to_ascii_uppercase().replace('1', "A")),
        format!("want {A}0\n"),
        format!("want -{}\n", &A[1..]),
        format!("want {}\n", "g".repeat(40)),
    ] {
        let req = body(&[data(&line), Packet::Flush]);
        let err = parse_upload_pack_request(&req, ProtocolVersion::V0).expect_err(&line);
        assert_eq!(reason(&err), Some("want_invalid"), "{line:?}");
        let v2 = body(&[
            data("command=fetch\n"),
            Packet::Delim,
            data(&line),
            Packet::Flush,
        ]);
        let err = parse_upload_pack_request(&v2, ProtocolVersion::V2).expect_err(&line);
        assert_eq!(reason(&err), Some("want_invalid"), "{line:?}");
    }
}

#[test]
fn broken_framing_is_pkt_line() {
    let err = parse_upload_pack_request(b"00zzwant", ProtocolVersion::V0).expect_err("non hex");
    assert_eq!(reason(&err), Some("pkt_line"));
    let err = parse_upload_pack_request(b"0032want 11", ProtocolVersion::V2).expect_err("cut");
    assert_eq!(reason(&err), Some("pkt_line"));
    // v0 has no delim.
    let err = parse_upload_pack_request(b"0001", ProtocolVersion::V0).expect_err("delim");
    assert_eq!(reason(&err), Some("pkt_line"));
}

#[test]
fn err_packet_is_one_pkt_line_and_bounded() {
    assert_eq!(err_packet("no"), b"000bERR no\n");
    let long = "あ".repeat(40_000);
    let pkt = err_packet(&long);
    assert!(pkt.len() <= crate::pkt_line::MAX_PKT_LEN);
    let decoded = crate::pkt_line::decode_all(&pkt, PktMode::V0).expect("one packet");
    let [Packet::Data(payload)] = decoded.as_slice() else {
        panic!("{decoded:?}");
    };
    assert!(
        std::str::from_utf8(payload).is_ok(),
        "cut on a char boundary"
    );
    assert!(payload.ends_with(b"\n"));
}

/// MR3 review: `sparse:oid` reads a blob by id; refused in v0 and v2,
/// alone or inside `combine:`; other filters pass.
#[test]
fn sparse_filters_are_refused() {
    for spec in [
        "sparse:oid=1111111111111111111111111111111111111111",
        "SPARSE:oid=main:.gitsparse",
        "combine:blob:none+sparse%3Aoid%3D1111111111111111111111111111111111111111",
    ] {
        let line = format!("filter {spec}\n");
        let v0 = body(&[data(&format!("want {A}\n")), data(&line), Packet::Flush]);
        let err = parse_upload_pack_request(&v0, ProtocolVersion::V0).expect_err(spec);
        assert_eq!(reason(&err), Some("filter_unsupported"), "{spec}");
        let v2 = body(&[
            data("command=fetch\n"),
            Packet::Delim,
            data(&format!("want {A}\n")),
            data(&line),
            Packet::Flush,
        ]);
        let err = parse_upload_pack_request(&v2, ProtocolVersion::V2).expect_err(spec);
        assert_eq!(reason(&err), Some("filter_unsupported"), "{spec}");
    }
    for spec in ["blob:none", "blob:limit=1m", "tree:0", "object:type=commit"] {
        let req = body(&[
            data("command=fetch\n"),
            Packet::Delim,
            data(&format!("want {A}\n")),
            data(&format!("filter {spec}\n")),
            Packet::Flush,
        ]);
        parse_upload_pack_request(&req, ProtocolVersion::V2).expect(spec);
    }
}
