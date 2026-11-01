#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)] // tests may fail loudly (rust/testing.md)

use std::collections::BTreeSet;

use super::*;
use crate::pkt_line::{decode_all, decode_sideband, encode_data, encode_sideband};

fn lines(items: &[&str]) -> Vec<u8> {
    let mut out = Vec::new();
    for l in items {
        encode_data(&mut out, format!("{l}\n").as_bytes()).expect("pkt");
    }
    out.extend_from_slice(pkt_line::FLUSH);
    out
}

fn sideband(progress: &str, report: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    encode_sideband(&mut out, Band::Progress, progress.as_bytes()).expect("band 2");
    encode_sideband(&mut out, Band::Data, report).expect("band 1");
    out.extend_from_slice(pkt_line::FLUSH);
    out
}

/// The report lines inside a (side-band) answer.
fn report_lines(answer: &[u8], sideband: bool) -> Vec<String> {
    let mut inner = Vec::new();
    if sideband {
        for p in decode_all(answer, PktMode::V0).expect("outer") {
            if let Packet::Data(d) = p {
                let (band, data) = decode_sideband(&d).expect("band");
                if band == Band::Data {
                    inner.extend_from_slice(data);
                }
            }
        }
    } else {
        inner = answer.to_vec();
    }
    decode_all(&inner, PktMode::V0)
        .expect("inner")
        .into_iter()
        .filter_map(|p| match p {
            Packet::Data(d) => Some(String::from_utf8(d).expect("utf-8")),
            Packet::Flush | Packet::Delim | Packet::ResponseEnd => None,
        })
        .collect()
}

fn protected() -> BTreeSet<String> {
    ["refs/heads/main".to_owned()].into_iter().collect()
}

#[test]
fn a_protected_non_fast_forward_is_rewritten_in_side_band() {
    let report = lines(&[
        "unpack ok",
        "ng refs/heads/main non-fast-forward",
        "ng refs/heads/other non-fast-forward",
        "ok refs/heads/feature",
    ]);
    let answer = sideband("error: denying non-fast-forward\n", &report);
    let mut rw = ReportRewriter::new(true, protected(), "FG-GIT-0007 no force".to_owned());
    // Byte by byte: the packets may arrive in any pieces.
    let mut out = Vec::new();
    for b in &answer {
        out.extend(rw.feed(std::slice::from_ref(b)));
    }
    let (rest, parsed) = rw.finish();
    out.extend(rest);
    let parsed = parsed.expect("readable");
    assert!(parsed.unpack_ok);
    assert_eq!(parsed.ok, ["refs/heads/feature"]);
    assert_eq!(parsed.rewritten, ["refs/heads/main"]);
    assert_eq!(parsed.ng.len(), 2);
    assert_eq!(
        report_lines(&out, true),
        [
            "unpack ok\n",
            "ng refs/heads/main FG-GIT-0007 no force\n",
            "ng refs/heads/other non-fast-forward\n",
            "ok refs/heads/feature\n",
        ]
    );
    // The progress message is forwarded unchanged, and the answer ends in a flush.
    assert!(out.ends_with(pkt_line::FLUSH));
    let text = String::from_utf8_lossy(&out);
    assert!(text.contains("error: denying non-fast-forward"), "{text}");
}

#[test]
fn a_plain_report_is_parsed_and_v2_option_lines_are_kept() {
    let report = lines(&[
        "unpack ok",
        "ok refs/heads/main",
        "option forced-update",
        "ng refs/tags/v1 already exists",
    ]);
    let mut rw = ReportRewriter::new(false, protected(), "x".to_owned());
    let mut out = rw.feed(&report);
    let (rest, parsed) = rw.finish();
    out.extend(rest);
    assert_eq!(out, report, "nothing to rewrite: byte for byte");
    let parsed = parsed.expect("readable");
    assert_eq!(parsed.ok, ["refs/heads/main"]);
    assert_eq!(
        parsed.ng,
        [("refs/tags/v1".to_owned(), "already exists".to_owned())]
    );
}

#[test]
fn an_unpack_error_and_an_incomplete_or_broken_report_are_forwarded() {
    let report = lines(&[
        "unpack index-pack abnormal exit",
        "ng refs/heads/main unpacker error",
    ]);
    let mut rw = ReportRewriter::new(false, protected(), "x".to_owned());
    let out = rw.feed(&report);
    let parsed = rw.finish().1.expect("readable");
    assert!(!parsed.unpack_ok);
    assert_eq!(
        parsed.unpack_error.as_deref(),
        Some("index-pack abnormal exit")
    );
    assert_eq!(out, report);

    // git died before the flush: what came is forwarded, unreadable.
    let cut = &report[..report.len() - 4];
    let mut rw = ReportRewriter::new(false, protected(), "x".to_owned());
    let mut out = rw.feed(cut);
    let (rest, parsed) = rw.finish();
    out.extend(rest);
    assert_eq!(out, cut);
    assert_eq!(parsed, Err("report_incomplete"));

    // Not pkt-lines at all.
    let mut rw = ReportRewriter::new(true, protected(), "x".to_owned());
    let mut out = rw.feed(b"zzzz garbage");
    let (rest, parsed) = rw.finish();
    out.extend(rest);
    assert_eq!(out, b"zzzz garbage");
    assert_eq!(parsed, Err("pkt_line"));
}

#[test]
fn the_refusal_report_names_every_ref() {
    let refs = vec![
        (
            "refs/heads/main".to_owned(),
            "FG-GIT-0009 no delete".to_owned(),
        ),
        (
            "refs/heads/x".to_owned(),
            "atomic push failure\nsecond line".to_owned(),
        ),
    ];
    let messages = vec!["FG-GIT-0009 no delete (request_id: r1)".to_owned()];
    let answer = refusal_report(true, &messages, &refs).expect("report");
    assert_eq!(
        report_lines(&answer, true),
        [
            "unpack ok\n",
            "ng refs/heads/main FG-GIT-0009 no delete\n",
            "ng refs/heads/x atomic push failure second line\n",
        ]
    );
    let text = String::from_utf8_lossy(&answer);
    assert!(
        text.contains("\u{2}FG-GIT-0009 no delete (request_id: r1)\n"),
        "{text}"
    );
    let plain = refusal_report(false, &messages, &refs).expect("report");
    assert_eq!(report_lines(&plain, false).len(), 3);
    assert!(!String::from_utf8_lossy(&plain).contains("request_id"));
}
