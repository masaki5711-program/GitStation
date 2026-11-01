//! Reading the command section of a receive-pack request (`POST
//! git-receive-pack`, M1 phase 9 MR4, ADR-0008 option B) before git sees
//! it: which refs the push updates, from which object to which, and the
//! capabilities the client asked for, so forge can decide every ref with
//! `forge_auth::authorize_ref_updates` before receive-pack starts.
//!
//! The section (gitprotocol-pack, "Reference Update Request"):
//!
//! ```text
//! [shallow <oid>]...                          (a shallow client only)
//! <old> <new> <ref>\0<capabilities>           (first command)
//! <old> <new> <ref>                           (each further command)
//! 0000                                        (flush)
//! <pack>                                      (unless every command deletes)
//! ```
//!
//! Strict (`FG-GIT-0014`, `reason` below), checked as the bytes arrive
//! ([`CommandParser`]), never past what is needed:
//!
//! - at most [`MAX_PUSH_COMMANDS`] commands and [`MAX_COMMAND_SECTION`]
//!   bytes up to the flush (`too_many_commands`; over the size
//!   `FG-GIT-0013 limit=command_section`);
//! - capabilities only on the first command (`command_invalid
//!   detail=capabilities_not_first`), only those receive-pack advertises
//!   to forge's clients (`capability_unsupported`; `push-options` is not
//!   advertised, `receive.advertisePushOptions=false`), `report-status` or
//!   `report-status-v2` required (`report_status_required`: forge answers
//!   refusals in that report), `side-band-64k` the only side-band;
//! - a signed push (`push-cert`) is refused (`push_cert`);
//! - object ids of the negotiated `object-format` (40 / 64 lower-case hex),
//!   not both zero, not equal (`command_invalid`); a ref at most once
//!   (`duplicate_ref`); `shallow` lines only before the first command
//!   (`shallow_misplaced`);
//! - the ref name: UTF-8 without control characters and spaces (else
//!   `command_invalid detail=ref_bytes`). A name that fails
//!   [`RefName::parse`] (`FG-GIT-0005`) or looks like an object id
//!   (`FG-GIT-0011` AmbiguousRefName: the branch / tag name is 40 or 64
//!   hex digits, which git reads as an object id first) is **kept** with
//!   its refusal, so forge answers it in-band (`ng <ref> …`) like any
//!   other refused ref.
//!
//! A request whose first packet is the flush (git's probe before a large
//! push, `http.postBuffer`) has no commands; forge answers it without
//! running git.

use std::collections::BTreeSet;

use forge_core::push::MAX_PUSH_UPDATES;
use forge_core::RefName;
use forge_errors::{ErrorCode, ForgeError};

use crate::pkt_line::{self, Packet, PktMode};

/// The most ref updates one push may carry.
pub const MAX_PUSH_COMMANDS: usize = MAX_PUSH_UPDATES;
/// The most bytes of the command section (up to and including its flush).
pub const MAX_COMMAND_SECTION: usize = 1024 * 1024;

/// Which report the client asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportStatus {
    V1,
    V2,
}

/// One ref update of the push.
#[derive(Debug)]
pub struct RefCommand {
    /// The old object id (all zeros: the ref is created).
    pub old: String,
    /// The new object id (all zeros: the ref is deleted).
    pub new: String,
    /// The name as sent (UTF-8, no control characters or spaces): what an
    /// in-band `ng` line repeats.
    pub raw_name: String,
    /// The validated name, or why it is refused (`FG-GIT-0005` /
    /// `FG-GIT-0011`).
    pub name: Result<RefName, ForgeError>,
}

impl RefCommand {
    pub fn is_create(&self) -> bool {
        is_zero(&self.old)
    }

    pub fn is_delete(&self) -> bool {
        is_zero(&self.new)
    }
}

/// A parsed command section.
#[derive(Debug)]
pub struct ReceivePackRequest {
    /// In request order; empty for git's probe request (`0000` only).
    pub commands: Vec<RefCommand>,
    /// The first command's capabilities, as sent.
    pub capabilities: Vec<String>,
    /// `None` only without commands.
    pub report_status: Option<ReportStatus>,
    /// `side-band-64k`: the report (and forge's messages) go in side-band.
    pub sideband: bool,
    /// The object id length of the push (`object-format`, 40 = SHA-1).
    pub hex_len: usize,
    /// Bytes of the section, flush included (the pack, if any, follows).
    pub section_len: usize,
    /// Where the first command's packet is in the body: (offset, length).
    first_command: Option<(usize, usize)>,
}

impl ReceivePackRequest {
    /// `true` when git will read a pack after the section (any command
    /// that is not a delete).
    pub fn expects_pack(&self) -> bool {
        self.commands.iter().any(|c| !c.is_delete())
    }

    /// The client asked for an atomic push.
    pub fn atomic(&self) -> bool {
        self.capabilities.iter().any(|c| c == "atomic")
    }

    /// `body` (the bytes this request was parsed from, the section at its
    /// start) with `atomic` added to the first command's capabilities, so
    /// git applies all of the push or none of it (M1 phase 9 MR4: a push
    /// that git may refuse in part, `receive.denyNonFastForwards` on a
    /// protected branch). The rest of the bytes is unchanged. receive-pack
    /// advertises `atomic` (`receive.advertiseAtomic`, git's default).
    pub fn with_atomic(&self, body: &[u8]) -> Result<Vec<u8>, ForgeError> {
        if self.atomic() {
            return Ok(body.to_vec());
        }
        let Some((at, len)) = self.first_command else {
            return Err(ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "atomic_without_commands"));
        };
        let packet = body.get(at..at + len).ok_or_else(|| {
            ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "atomic_packet_out_of_range")
        })?;
        let payload = packet.get(4..).unwrap_or_default();
        let (line, newline) = match payload.strip_suffix(b"\n") {
            Some(line) => (line, true),
            None => (payload, false),
        };
        let mut new = line.to_vec();
        new.extend_from_slice(b" atomic");
        if newline {
            new.push(b'\n');
        }
        let mut out = Vec::with_capacity(body.len() + 7);
        out.extend_from_slice(body.get(..at).unwrap_or_default());
        pkt_line::encode_data(&mut out, &new)?;
        out.extend_from_slice(body.get(at + len..).unwrap_or_default());
        Ok(out)
    }
}

/// What [`CommandParser::feed`] found so far.
#[derive(Debug)]
pub enum Parsed {
    /// The section is not complete yet: read more of the body.
    NeedMore,
    /// The section is complete.
    Done(ReceivePackRequest),
}

/// Parses the command section incrementally: [`CommandParser::feed`] takes
/// the whole buffer read so far and continues where it stopped (each byte
/// is looked at once).
#[derive(Debug, Default)]
pub struct CommandParser {
    offset: usize,
    commands: Vec<RefCommand>,
    names: BTreeSet<String>,
    capabilities: Option<Vec<String>>,
    hex_len: usize,
    /// The packet being read: (offset, length).
    current: (usize, usize),
    first_command: Option<(usize, usize)>,
}

fn refused(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::GitProtocolInvalid)
        .with_context("reason", reason)
        .with_context("service", "git-receive-pack")
}

fn invalid(detail: &'static str) -> ForgeError {
    refused("command_invalid").with_context("detail", detail)
}

fn is_zero(hex: &str) -> bool {
    hex.bytes().all(|b| b == b'0')
}

fn is_hex_id(hex: &str, len: usize) -> bool {
    hex.len() == len && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// `FG-GIT-0011` when the branch or tag name (or, for any other ref, the
/// whole name after `refs/`) is 40 or 64 hex digits of either case: git
/// resolves such a name as an object id before it looks at refs, so the
/// ref could never be named on a command line.
pub fn check_unambiguous(name: &RefName) -> Result<(), ForgeError> {
    let short = name
        .branch()
        .or_else(|| name.tag())
        .unwrap_or_else(|| name.as_str().trim_start_matches("refs/"));
    let hex_like = matches!(short.len(), 40 | 64) && short.bytes().all(|b| b.is_ascii_hexdigit());
    if hex_like {
        return Err(ForgeError::new(ErrorCode::AmbiguousRefName)
            .with_context("reason", "object_id_like")
            .with_context("len", short.len()));
    }
    Ok(())
}

/// The capabilities a client may send to forge's receive-pack (what it
/// advertises, plus the client's own `agent=` / `session-id=`).
fn check_capability(cap: &str) -> Result<(), ForgeError> {
    let name = cap.split_once('=').map_or(cap, |(name, _)| name);
    match name {
        "report-status" | "report-status-v2" | "side-band-64k" | "quiet" | "atomic"
        | "ofs-delta" | "delete-refs" | "agent" | "session-id" | "object-format" => Ok(()),
        _ => Err(refused("capability_unsupported").with_context("capability", log_safe(name))),
    }
}

/// A client-chosen word for the log: at most 32 characters, anything but
/// `[a-z0-9-]` replaced by `?`.
fn log_safe(word: &str) -> String {
    word.chars()
        .take(32)
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' {
                c
            } else {
                '?'
            }
        })
        .collect()
}

/// The object id length named by `object-format=` (default SHA-1).
fn hex_len_of(capabilities: &[String]) -> Result<usize, ForgeError> {
    let mut len = 40;
    for cap in capabilities {
        if let Some(format) = cap.strip_prefix("object-format=") {
            len = match format {
                "sha1" => 40,
                "sha256" => 64,
                _ => {
                    return Err(refused("capability_unsupported")
                        .with_context("capability", "object-format"))
                }
            };
        }
    }
    Ok(len)
}

impl CommandParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Continue parsing `buf` (everything read so far, decompressed).
    pub fn feed(&mut self, buf: &[u8]) -> Result<Parsed, ForgeError> {
        loop {
            let rest = buf.get(self.offset..).unwrap_or_default();
            let Some((packet, used)) = pkt_line::decode(rest, PktMode::V0)? else {
                if buf.len() > MAX_COMMAND_SECTION {
                    return Err(ForgeError::new(ErrorCode::GitRequestTooLarge)
                        .with_context("limit", "command_section")
                        .with_context("max_bytes", MAX_COMMAND_SECTION)
                        .with_context("service", "git-receive-pack"));
                }
                return Ok(Parsed::NeedMore);
            };
            self.current = (self.offset, used);
            self.offset += used;
            if self.offset > MAX_COMMAND_SECTION {
                return Err(ForgeError::new(ErrorCode::GitRequestTooLarge)
                    .with_context("limit", "command_section")
                    .with_context("max_bytes", MAX_COMMAND_SECTION)
                    .with_context("service", "git-receive-pack"));
            }
            match packet {
                Packet::Flush => return self.done().map(Parsed::Done),
                Packet::Data(payload) => self.line(&payload)?,
                // V0 framing never yields these (`length_reserved`).
                Packet::Delim | Packet::ResponseEnd => return Err(invalid("special_packet")),
            }
        }
    }

    fn line(&mut self, payload: &[u8]) -> Result<(), ForgeError> {
        let payload = payload.strip_suffix(b"\n").unwrap_or(payload);
        if payload.starts_with(b"push-cert") {
            return Err(refused("push_cert"));
        }
        if let Some(oid) = payload.strip_prefix(b"shallow ") {
            if !self.commands.is_empty() || self.capabilities.is_some() {
                return Err(refused("shallow_misplaced"));
            }
            let ok = std::str::from_utf8(oid).is_ok_and(|o| is_hex_id(o, 40) || is_hex_id(o, 64));
            if !ok {
                return Err(invalid("shallow_id"));
            }
            return Ok(());
        }
        if self.commands.len() >= MAX_PUSH_COMMANDS {
            return Err(
                refused("too_many_commands").with_context("max_commands", MAX_PUSH_COMMANDS)
            );
        }
        let (command, caps) = match payload.iter().position(|&b| b == 0) {
            Some(nul) => (
                payload.get(..nul).unwrap_or_default(),
                Some(payload.get(nul + 1..).unwrap_or_default()),
            ),
            None => (payload, None),
        };
        match (caps, self.capabilities.is_some()) {
            (Some(caps), false) => {
                let text = std::str::from_utf8(caps).map_err(|_| invalid("capabilities_bytes"))?;
                let caps: Vec<String> = text
                    .split(' ')
                    .filter(|c| !c.is_empty())
                    .map(str::to_owned)
                    .collect();
                for cap in &caps {
                    if cap.bytes().any(|b| b.is_ascii_control()) {
                        return Err(invalid("capabilities_bytes"));
                    }
                    check_capability(cap)?;
                }
                self.hex_len = hex_len_of(&caps)?;
                self.capabilities = Some(caps);
                self.first_command = Some(self.current);
            }
            (Some(_), true) => return Err(invalid("capabilities_not_first")),
            (None, false) => return Err(refused("report_status_required")),
            (None, true) => {}
        }
        let text = std::str::from_utf8(command).map_err(|_| invalid("ref_bytes"))?;
        let mut parts = text.splitn(3, ' ');
        let (Some(old), Some(new), Some(raw)) = (parts.next(), parts.next(), parts.next()) else {
            return Err(invalid("shape"));
        };
        if !is_hex_id(old, self.hex_len) || !is_hex_id(new, self.hex_len) {
            return Err(invalid("object_id"));
        }
        if old == new {
            return Err(invalid("unchanged_ref"));
        }
        if raw.is_empty() || raw.chars().any(|c| c.is_control() || c == ' ') {
            return Err(invalid("ref_bytes"));
        }
        if !self.names.insert(raw.to_owned()) {
            return Err(refused("duplicate_ref"));
        }
        let name = RefName::parse(raw).and_then(|n| check_unambiguous(&n).map(|()| n));
        self.commands.push(RefCommand {
            old: old.to_owned(),
            new: new.to_owned(),
            raw_name: raw.to_owned(),
            name,
        });
        Ok(())
    }

    fn done(&mut self) -> Result<ReceivePackRequest, ForgeError> {
        let commands = std::mem::take(&mut self.commands);
        let capabilities = self.capabilities.take().unwrap_or_default();
        let report_status = if capabilities.iter().any(|c| c == "report-status-v2") {
            Some(ReportStatus::V2)
        } else if capabilities.iter().any(|c| c == "report-status") {
            Some(ReportStatus::V1)
        } else {
            None
        };
        if !commands.is_empty() && report_status.is_none() {
            return Err(refused("report_status_required"));
        }
        let sideband = capabilities.iter().any(|c| c == "side-band-64k");
        Ok(ReceivePackRequest {
            commands,
            capabilities,
            report_status,
            sideband,
            hex_len: if self.hex_len == 0 { 40 } else { self.hex_len },
            section_len: self.offset,
            first_command: self.first_command,
        })
    }
}

#[cfg(test)]
#[path = "receive_request_tests.rs"]
mod tests;
