//! pkt-line framing (gitprotocol-common) and side-band framing
//! (gitprotocol-pack, `side-band-64k`).
//!
//! A packet is a 4-digit hex length (counting the 4 digits) and a payload.
//! `0000` is flush; in protocol v2 `0001` is delim and `0002` response-end;
//! `0003` is never valid; lengths above 65520 are invalid. forge only frames
//! and inspects packets — the protocol itself stays git's (CLAUDE.md §3-5).

use forge_errors::{ErrorCode, ForgeError};
use tokio::io::{AsyncRead, AsyncReadExt};

/// The largest packet, length digits included.
pub const MAX_PKT_LEN: usize = 65520;
/// The largest payload of a data packet.
pub const MAX_PKT_DATA: usize = MAX_PKT_LEN - 4;
/// The largest side-band-64k payload (one byte goes to the band number).
pub const MAX_SIDEBAND_DATA: usize = MAX_PKT_DATA - 1;

pub const FLUSH: &[u8; 4] = b"0000";
pub const DELIM: &[u8; 4] = b"0001";
pub const RESPONSE_END: &[u8; 4] = b"0002";

/// Which special lengths the stream may contain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PktMode {
    /// v0 / v1 (receive-pack commands, upload-pack negotiation): only flush.
    V0,
    /// v2: flush, delim and response-end.
    V2,
}

/// One decoded packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Packet {
    Data(Vec<u8>),
    Flush,
    Delim,
    ResponseEnd,
}

fn invalid(detail: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::GitProtocolInvalid)
        .with_context("reason", "pkt_line")
        .with_context("detail", detail)
}

fn hex_digit(b: u8) -> Option<usize> {
    match b {
        b'0'..=b'9' => Some(usize::from(b - b'0')),
        b'a'..=b'f' => Some(usize::from(b - b'a') + 10),
        b'A'..=b'F' => Some(usize::from(b - b'A') + 10),
        _ => None,
    }
}

/// Parse the 4 length digits.
fn parse_len(digits: [u8; 4]) -> Result<usize, ForgeError> {
    digits.iter().try_fold(0usize, |acc, &b| {
        hex_digit(b)
            .map(|d| acc * 16 + d)
            .ok_or_else(|| invalid("non_hex"))
    })
}

/// The packet a length stands for when it carries no payload, else `None`.
fn special(len: usize, mode: PktMode) -> Result<Option<Packet>, ForgeError> {
    match (len, mode) {
        (0, _) => Ok(Some(Packet::Flush)),
        (1, PktMode::V2) => Ok(Some(Packet::Delim)),
        (2, PktMode::V2) => Ok(Some(Packet::ResponseEnd)),
        (1..=3, _) => Err(invalid("length_reserved")),
        (n, _) if n > MAX_PKT_LEN => Err(invalid("length_too_large")),
        _ => Ok(None),
    }
}

/// Decode the next packet at the start of `buf`.
///
/// `Ok(None)`: `buf` holds only part of a packet (read more). `Ok(Some((p,
/// used)))`: `p` and the number of bytes it took.
pub fn decode(buf: &[u8], mode: PktMode) -> Result<Option<(Packet, usize)>, ForgeError> {
    let Some(&[a, b, c, d]) = buf.get(..4) else {
        return Ok(None);
    };
    let len = parse_len([a, b, c, d])?;
    if let Some(p) = special(len, mode)? {
        return Ok(Some((p, 4)));
    }
    Ok(buf
        .get(4..len)
        .map(|payload| (Packet::Data(payload.to_vec()), len)))
}

/// Decode a complete buffer (e.g. a request body) into packets. A trailing
/// partial packet is an error (`detail=truncated`).
pub fn decode_all(mut buf: &[u8], mode: PktMode) -> Result<Vec<Packet>, ForgeError> {
    let mut out = Vec::new();
    while !buf.is_empty() {
        let Some((p, used)) = decode(buf, mode)? else {
            return Err(invalid("truncated"));
        };
        out.push(p);
        buf = buf.get(used..).unwrap_or_default();
    }
    Ok(out)
}

/// Append one data packet. Payloads over [`MAX_PKT_DATA`] are a forge bug
/// (`FG-SYS-0001`), never truncated.
pub fn encode_data(out: &mut Vec<u8>, payload: &[u8]) -> Result<(), ForgeError> {
    if payload.len() > MAX_PKT_DATA {
        return Err(ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "pkt_line_payload_too_large")
            .with_context("len", payload.len()));
    }
    let len = payload.len() + 4;
    out.extend_from_slice(format!("{len:04x}").as_bytes());
    out.extend_from_slice(payload);
    Ok(())
}

/// Append a packet (data or special).
pub fn encode(out: &mut Vec<u8>, packet: &Packet) -> Result<(), ForgeError> {
    match packet {
        Packet::Data(d) => encode_data(out, d)?,
        Packet::Flush => out.extend_from_slice(FLUSH),
        Packet::Delim => out.extend_from_slice(DELIM),
        Packet::ResponseEnd => out.extend_from_slice(RESPONSE_END),
    }
    Ok(())
}

/// Read packets from an async source, one at a time (bounded: one packet is
/// at most [`MAX_PKT_LEN`] bytes).
#[derive(Debug)]
pub struct PktReader<R> {
    inner: R,
    mode: PktMode,
}

impl<R: AsyncRead + Unpin> PktReader<R> {
    pub fn new(inner: R, mode: PktMode) -> Self {
        Self { inner, mode }
    }

    /// The next packet; `Ok(None)` at a clean EOF (between packets). EOF
    /// inside a packet is `detail=truncated`; an I/O error is FG-GIT-0003
    /// with the source.
    pub async fn next(&mut self) -> Result<Option<Packet>, ForgeError> {
        let mut digits = [0u8; 4];
        let mut got = 0;
        while got < 4 {
            let slot = digits.get_mut(got..).unwrap_or_default();
            match self.inner.read(slot).await {
                Ok(0) if got == 0 => return Ok(None),
                Ok(0) => return Err(invalid("truncated")),
                Ok(n) => got += n,
                Err(e) => return Err(read_failed(e)),
            }
        }
        let len = parse_len(digits)?;
        if let Some(p) = special(len, self.mode)? {
            return Ok(Some(p));
        }
        let mut payload = vec![0u8; len - 4];
        match self.inner.read_exact(&mut payload).await {
            Ok(_) => Ok(Some(Packet::Data(payload))),
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Err(invalid("truncated")),
            Err(e) => Err(read_failed(e)),
        }
    }

    pub fn into_inner(self) -> R {
        self.inner
    }
}

fn read_failed(e: std::io::Error) -> ForgeError {
    ForgeError::new(ErrorCode::GitProcessFailed)
        .with_context("reason", "pkt_line_read_failed")
        .with_context("subcommand", "pkt-line")
        .with_source(e)
}

/// Side-band channels (`side-band-64k`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Band {
    /// Pack data / the wrapped response.
    Data = 1,
    /// Progress messages (shown to the user by the client).
    Progress = 2,
    /// A fatal error message; the client aborts.
    Error = 3,
}

/// Append `data` on `band`, split into as many packets as needed.
pub fn encode_sideband(out: &mut Vec<u8>, band: Band, data: &[u8]) -> Result<(), ForgeError> {
    for chunk in data.chunks(MAX_SIDEBAND_DATA) {
        let mut payload = Vec::with_capacity(chunk.len() + 1);
        payload.push(band as u8);
        payload.extend_from_slice(chunk);
        encode_data(out, &payload)?;
    }
    Ok(())
}

/// Split a side-band data packet into its band and data.
pub fn decode_sideband(payload: &[u8]) -> Result<(Band, &[u8]), ForgeError> {
    match payload.split_first() {
        Some((1, rest)) => Ok((Band::Data, rest)),
        Some((2, rest)) => Ok((Band::Progress, rest)),
        Some((3, rest)) => Ok((Band::Error, rest)),
        _ => Err(ForgeError::new(ErrorCode::GitProtocolInvalid)
            .with_context("reason", "pkt_line")
            .with_context("detail", "bad_sideband")),
    }
}

#[cfg(test)]
#[path = "pkt_line_tests.rs"]
mod tests;
