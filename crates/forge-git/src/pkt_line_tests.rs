use forge_errors::ErrorCode;
use proptest::prelude::*;

use super::*;

fn detail(err: &ForgeError) -> String {
    err.to_string()
}

#[test]
fn pkt_line_rejects_invalid_lengths() {
    for (bytes, mode, want) in [
        (&b"0001"[..], PktMode::V0, "length_reserved"),
        (b"0002", PktMode::V0, "length_reserved"),
        (b"0003", PktMode::V0, "length_reserved"),
        (b"0003", PktMode::V2, "length_reserved"),
        (b"fff1", PktMode::V0, "length_too_large"),
        (b"ffff", PktMode::V2, "length_too_large"),
        (b"00g5x", PktMode::V0, "non_hex"),
        (b"+005x", PktMode::V0, "non_hex"),
        (b"0005", PktMode::V0, "truncated"),
    ] {
        let err = decode_all(bytes, mode).expect_err("invalid");
        assert_eq!(err.code(), ErrorCode::GitProtocolInvalid);
        let text = detail(&err);
        assert!(text.contains("reason=pkt_line"), "{text}");
        assert!(text.contains(want), "{bytes:?}: {text}");
    }
}

#[test]
fn v2_specials_and_flush_decode() {
    let packets = decode_all(b"0000000100020008abcd", PktMode::V2).expect("v2");
    assert_eq!(
        packets,
        [
            Packet::Flush,
            Packet::Delim,
            Packet::ResponseEnd,
            Packet::Data(b"abcd".to_vec())
        ]
    );
    assert_eq!(
        decode_all(b"0000", PktMode::V0).expect("flush"),
        [Packet::Flush]
    );
}

#[test]
fn uppercase_hex_lengths_are_accepted_like_git() {
    let mut body = b"000A".to_vec();
    body.extend_from_slice(b"abcdef");
    assert_eq!(
        decode_all(&body, PktMode::V0).expect("upper"),
        [Packet::Data(b"abcdef".to_vec())]
    );
}

#[test]
fn the_largest_packet_round_trips_and_one_more_byte_is_refused() {
    let payload = vec![b'x'; MAX_PKT_DATA];
    let mut out = Vec::new();
    encode_data(&mut out, &payload).expect("max");
    assert_eq!(out.get(..4), Some(&b"fff0"[..]));
    assert_eq!(
        decode_all(&out, PktMode::V0).expect("decode"),
        [Packet::Data(payload)]
    );
    let err = encode_data(&mut Vec::new(), &vec![b'x'; MAX_PKT_DATA + 1]).expect_err("too big");
    assert_eq!(err.code(), ErrorCode::Internal);
}

#[test]
fn partial_input_asks_for_more() {
    assert_eq!(decode(b"00", PktMode::V0).expect("partial"), None);
    assert_eq!(decode(b"0009abc", PktMode::V0).expect("partial"), None);
}

#[test]
fn sideband_splits_large_data_and_decodes_bands() {
    let data = vec![7u8; MAX_SIDEBAND_DATA * 2 + 10];
    let mut out = Vec::new();
    encode_sideband(&mut out, Band::Data, &data).expect("encode");
    encode_sideband(&mut out, Band::Progress, b"Counting\r").expect("progress");
    encode_sideband(&mut out, Band::Error, b"fatal\n").expect("error");
    let packets = decode_all(&out, PktMode::V0).expect("decode");
    assert_eq!(packets.len(), 5);
    let mut joined = Vec::new();
    let mut bands = Vec::new();
    for p in &packets {
        let Packet::Data(payload) = p else {
            panic!("data packets only")
        };
        assert!(payload.len() <= MAX_PKT_DATA);
        let (band, rest) = decode_sideband(payload).expect("band");
        bands.push(band);
        if band == Band::Data {
            joined.extend_from_slice(rest);
        }
    }
    assert_eq!(joined, data);
    assert_eq!(
        bands,
        [
            Band::Data,
            Band::Data,
            Band::Data,
            Band::Progress,
            Band::Error
        ]
    );
    assert!(decode_sideband(b"\x04x").is_err());
    assert!(decode_sideband(b"").is_err());
}

#[tokio::test]
async fn reader_reads_packets_and_reports_truncation() {
    let mut body = Vec::new();
    encode_data(&mut body, b"want abc\n").expect("encode");
    body.extend_from_slice(FLUSH);
    encode_data(&mut body, b"done\n").expect("encode");
    let mut r = PktReader::new(&body[..], PktMode::V0);
    assert_eq!(
        r.next().await.expect("1"),
        Some(Packet::Data(b"want abc\n".to_vec()))
    );
    assert_eq!(r.next().await.expect("2"), Some(Packet::Flush));
    assert_eq!(
        r.next().await.expect("3"),
        Some(Packet::Data(b"done\n".to_vec()))
    );
    assert_eq!(r.next().await.expect("eof"), None);

    let cut = body.get(..body.len() - 2).expect("cut");
    let mut r = PktReader::new(cut, PktMode::V0);
    let _ = r.next().await.expect("1");
    let _ = r.next().await.expect("2");
    let err = r.next().await.expect_err("truncated");
    assert!(err.to_string().contains("truncated"), "{err}");

    let mut r = PktReader::new(&b"00"[..], PktMode::V0);
    assert!(r
        .next()
        .await
        .expect_err("half a length")
        .to_string()
        .contains("truncated"));
}

fn packet() -> impl Strategy<Value = Packet> {
    prop_oneof![
        Just(Packet::Flush),
        Just(Packet::Delim),
        Just(Packet::ResponseEnd),
        proptest::collection::vec(any::<u8>(), 0..300).prop_map(Packet::Data),
    ]
}

proptest! {
    #[test]
    fn round_trip(packets in proptest::collection::vec(packet(), 0..20)) {
        let mut out = Vec::new();
        for p in &packets {
            encode(&mut out, p).expect("encode");
        }
        prop_assert_eq!(decode_all(&out, PktMode::V2).expect("decode"), packets);
    }

    #[test]
    fn lengths_one_to_three_are_rejected_in_v0(len in 1usize..=3, tail in proptest::collection::vec(any::<u8>(), 0..8)) {
        let mut body = format!("{len:04x}").into_bytes();
        body.extend(tail);
        let err = decode_all(&body, PktMode::V0).expect_err("reserved");
        prop_assert_eq!(err.code(), ErrorCode::GitProtocolInvalid);
    }

    #[test]
    fn lengths_over_65520_are_rejected(len in 65521usize..=0xffff, mode in prop_oneof![Just(PktMode::V0), Just(PktMode::V2)]) {
        let body = format!("{len:04x}").into_bytes();
        let err = decode(&body, mode).expect_err("too large");
        prop_assert!(err.to_string().contains("length_too_large"));
    }

    #[test]
    fn a_non_hex_digit_anywhere_is_rejected(pos in 0usize..4, bad in any::<u8>().prop_filter("not hex", |b| !b.is_ascii_hexdigit())) {
        let mut body = b"0010abcdefghijkl".to_vec();
        body[pos] = bad;
        let err = decode_all(&body, PktMode::V0).expect_err("non hex");
        prop_assert!(err.to_string().contains("non_hex"));
    }

    #[test]
    fn decode_never_panics_and_never_reads_past_the_input(bytes in proptest::collection::vec(any::<u8>(), 0..64)) {
        if let Ok(Some((_, used))) = decode(&bytes, PktMode::V2) {
            prop_assert!(used <= bytes.len());
        }
    }
}
