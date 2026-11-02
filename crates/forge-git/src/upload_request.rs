//! Reading an upload-pack request body (`POST git-upload-pack`) before git
//! sees it (ADR-0007 amendment, option (a), M1 phase 9 MR3): which command
//! it is and which objects it wants, so forge can check every `want` is
//! reachable from a ref before starting upload-pack.
//!
//! - protocol v0 / v1 (stateless RPC): `want <oid>[ <capabilities>]` lines,
//!   then `shallow` / `deepen*` / `filter`, flush, `have` lines, `done`.
//! - protocol v2: `command=<name>`, capabilities, delim, arguments
//!   (`want <oid>` among them), flush. Only `ls-refs` and `fetch` are
//!   accepted: any other command (`object-info`, `bundle-uri`, …) would answer
//!   about objects by id without a want (refused, `command_unsupported`).
//!
//! A `filter` naming `sparse` (`sparse:oid=<blob>`, also inside
//! `combine:`) is refused (`filter_unsupported`): it makes upload-pack read
//! a blob by id as the sparse pattern, reachable or not (upload-pack is
//! also run with `uploadpack.filter.sparse:oid.allow=false`).
//!
//! `have` and `shallow` ids are not checked: git answers them with `ACK` /
//! shallow updates, which reveal only whether an object exists, never its
//! content (accepted residual, ADR-0007 amendment MR3).
//!
//! The body is the decompressed request; the original bytes go to git
//! unchanged. Refusals are `FG-GIT-0014` (`pkt_line` from the framing,
//! `want_invalid`, `command_unsupported`, `filter_unsupported`).

use forge_errors::{ErrorCode, ForgeError};

use crate::pkt_line::{self, Packet, PktMode};
use crate::smart_http::ProtocolVersion;

/// What an upload-pack request asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UploadPackCommand {
    /// v0 / v1 negotiation (wants, haves, done).
    Negotiate,
    /// v2 `command=ls-refs` (lists refs; wants nothing).
    LsRefs,
    /// v2 `command=fetch`.
    Fetch,
}

impl UploadPackCommand {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Negotiate => "negotiate",
            Self::LsRefs => "ls-refs",
            Self::Fetch => "fetch",
        }
    }
}

/// A parsed upload-pack request: its command and every wanted object id
/// (validated: 40 or 64 lower-case hex digits, in request order, duplicates
/// kept).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadPackRequest {
    pub command: UploadPackCommand,
    pub wants: Vec<String>,
}

fn refused(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::GitProtocolInvalid).with_context("reason", reason)
}

/// A full object id as git writes it: 40 (SHA-1) or 64 (SHA-256)
/// lower-case hex digits.
pub(crate) fn is_object_id(hex: &str) -> bool {
    matches!(hex.len(), 40 | 64) && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The object id of a `want` packet, or `None` when the packet is not a
/// `want` (other arguments such as `want-ref`, `have`, `shallow`).
/// `want <oid>`, optionally followed by ` <capabilities>` (v0, first line)
/// and the newline.
fn want_of(payload: &[u8]) -> Result<Option<String>, ForgeError> {
    let Some(rest) = payload.strip_prefix(b"want ") else {
        return Ok(None);
    };
    let rest = rest.strip_suffix(b"\n").unwrap_or(rest);
    let oid = rest.split(|&b| b == b' ').next().unwrap_or_default();
    match std::str::from_utf8(oid) {
        Ok(oid) if is_object_id(oid) => Ok(Some(oid.to_owned())),
        _ => Err(refused("want_invalid")),
    }
}

/// `filter <spec>` naming `sparse` anywhere (case-insensitive, so an
/// encoded `combine:` member is caught too): `filter_unsupported`.
fn check_filter(payload: &[u8]) -> Result<(), ForgeError> {
    let Some(spec) = payload.strip_prefix(b"filter ") else {
        return Ok(());
    };
    let lower = spec.to_ascii_lowercase();
    if lower.windows(6).any(|w| w == b"sparse") {
        return Err(refused("filter_unsupported"));
    }
    Ok(())
}

/// Parse a complete (decompressed) upload-pack request body for the
/// protocol `version` the request asked for (`Git-Protocol`, the value git
/// receives as well).
pub fn parse_upload_pack_request(
    body: &[u8],
    version: ProtocolVersion,
) -> Result<UploadPackRequest, ForgeError> {
    match version {
        ProtocolVersion::V0 | ProtocolVersion::V1 => parse_v0(body),
        ProtocolVersion::V2 => parse_v2(body),
    }
}

fn parse_v0(body: &[u8]) -> Result<UploadPackRequest, ForgeError> {
    let mut wants = Vec::new();
    for packet in pkt_line::decode_all(body, PktMode::V0)? {
        if let Packet::Data(payload) = packet {
            check_filter(&payload)?;
            if let Some(oid) = want_of(&payload)? {
                wants.push(oid);
            }
        }
    }
    Ok(UploadPackRequest {
        command: UploadPackCommand::Negotiate,
        wants,
    })
}

fn parse_v2(body: &[u8]) -> Result<UploadPackRequest, ForgeError> {
    let packets = pkt_line::decode_all(body, PktMode::V2)?;
    let mut iter = packets.iter();
    let command = match iter.next() {
        Some(Packet::Data(first)) => {
            let line = first.strip_suffix(b"\n").unwrap_or(first);
            match line.strip_prefix(b"command=") {
                Some(b"fetch") => UploadPackCommand::Fetch,
                Some(b"ls-refs") => UploadPackCommand::LsRefs,
                Some(other) => {
                    return Err(
                        refused("command_unsupported").with_context("command", bounded_name(other))
                    )
                }
                None => return Err(refused("command_unsupported").with_context("command", "none")),
            }
        }
        // A git client always names its command.
        None | Some(_) => {
            return Err(refused("command_unsupported").with_context("command", "none"))
        }
    };
    let mut wants = Vec::new();
    let mut in_args = false;
    let mut ended = false;
    for packet in iter {
        if ended {
            // One command per stateless request: anything after its flush
            // would be a second command forge did not look at.
            return Err(refused("command_unsupported").with_context("command", "multiple"));
        }
        match packet {
            Packet::Delim => in_args = true,
            Packet::Flush => ended = true,
            Packet::ResponseEnd => {
                return Err(refused("command_unsupported").with_context("command", "response_end"))
            }
            Packet::Data(payload) if in_args && command == UploadPackCommand::Fetch => {
                check_filter(payload)?;
                if let Some(oid) = want_of(payload)? {
                    wants.push(oid);
                }
            }
            Packet::Data(_) => {}
        }
    }
    Ok(UploadPackRequest { command, wants })
}

/// A command name for the log: at most 32 bytes, printable ASCII only.
fn bounded_name(raw: &[u8]) -> String {
    raw.iter()
        .take(32)
        .map(|&b| {
            if b.is_ascii_graphic() {
                char::from(b)
            } else {
                '?'
            }
        })
        .collect()
}

/// An `ERR` packet carrying `message` (git prints `remote error: <message>`
/// and stops). Used to refuse an upload-pack request in-band, before git
/// starts. The message is cut to fit one packet.
pub fn err_packet(message: &str) -> Vec<u8> {
    let mut text = format!("ERR {message}\n");
    if text.len() > pkt_line::MAX_PKT_DATA {
        let mut cut = pkt_line::MAX_PKT_DATA - 1;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        text.push('\n');
    }
    let mut out = Vec::with_capacity(text.len() + 4);
    out.extend_from_slice(format!("{:04x}", text.len() + 4).as_bytes());
    out.extend_from_slice(text.as_bytes());
    out
}

#[cfg(test)]
#[path = "upload_request_tests.rs"]
mod tests;
