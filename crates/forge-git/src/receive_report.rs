//! receive-pack's answer (M1 phase 9 MR4): reading its report-status while
//! it streams to the client, rewriting git's own refusal of a protected
//! ref into forge's error code, and building the in-band refusal forge
//! sends **instead of** running git when a ref is denied.
//!
//! The answer (gitprotocol-pack "Report Status"): pkt-lines `unpack ok` (or
//! `unpack <error>`), then per command `ok <ref>` or `ng <ref> <reason>`
//! (report-status-v2 may add `option …` lines after an `ok`), then a
//! flush. With `side-band-64k` the whole report, its flush included, is the
//! data of band 1 and the answer ends with a flush of its own; band 2
//! carries git's messages (`remote: …`), band 3 a fatal error.
//!
//! [`ReportRewriter`] forwards band 2 packets at once (the storage root
//! path replaced by `<storage>`: git relays index-pack's stderr there),
//! replaces band 3 (a fatal error) with forge's text, and holds the report
//! (band 1, or the plain pkt-lines) until it is complete, at most
//! [`MAX_REPORT_BYTES`]; then it replaces the reason of every `ng <ref>
//! non-fast-forward` whose ref is in the protected set (git's
//! `receive.denyNonFastForwards`, which forge turns on for a push touching
//! a protected branch, ADR-0008 option B) with forge's text
//! (`FG-GIT-0007 <user message>`), keeps git's other short reasons from an
//! allow-list ([`GIT_NG_REASONS`]) and replaces any other reason and an
//! `unpack <error>` text with forge's withheld text (an FG code and the
//! request id). What was replaced is returned ([`Report::withheld`],
//! band 3 in [`ReportRewriter::withheld`]) for the caller to log once.
//! Anything it cannot read is forwarded unchanged and reported as
//! unreadable — never dropped.

use std::collections::BTreeSet;

use forge_errors::ForgeError;

use crate::pkt_line::{self, Band, Packet, PktMode};

/// The most report bytes held back (1000 refs with long names fit).
pub const MAX_REPORT_BYTES: usize = 4 * 1024 * 1024;

/// git's own short `ng` reasons (receive-pack), forwarded unchanged: they
/// name no path and no internal state.
pub const GIT_NG_REASONS: &[&str] = &[
    "non-fast-forward",
    "deletion prohibited",
    "deny updating a hidden ref",
    "funny refname",
    "branch is currently checked out",
    "deletion of the current branch prohibited",
    "bad ref",
    "missing necessary objects",
    "failed to update ref",
    "failed to delete",
    "failed to lock",
    "failed to write",
    "shallow update not allowed",
    "unpacker error",
    "unpack error",
    "n/a (unpacker error)",
    "atomic push failure",
    "atomic transaction failed",
    "transaction failed to start",
    "invalid new value provided",
    "stale info",
    "hook declined",
    "pre-receive hook declined",
];

/// forge's texts for an answer (see the module docs).
#[derive(Debug, Clone, Default)]
pub struct RewriteTexts {
    /// For `ng <protected ref> non-fast-forward` (e.g. `FG-GIT-0007 <msg>`).
    pub force_refused: String,
    /// For a reason, `unpack` text or band-3 message forge does not pass on
    /// (e.g. `FG-GIT-0003 <msg> (request_id: …)`).
    pub withheld: String,
    /// Strings replaced by `<storage>` in band-2 messages (the storage root).
    pub scrub: Vec<String>,
}

/// What git reported.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Report {
    /// `unpack ok`.
    pub unpack_ok: bool,
    /// The `unpack` status text when it is not `ok` (git's words).
    pub unpack_error: Option<String>,
    /// Refs git updated (`ok <ref>`), in report order.
    pub ok: Vec<String>,
    /// Refs git refused, with git's reason (before any rewrite).
    pub ng: Vec<(String, String)>,
    /// Refs whose `non-fast-forward` refusal forge rewrote.
    pub rewritten: Vec<String>,
    /// git's texts forge replaced (`unpack …`, `ng <ref> <reason>`), for
    /// the log.
    pub withheld: Vec<String>,
}

/// Where the rewriter is.
#[derive(Debug)]
enum State {
    /// Reading packets.
    Open,
    /// The report was emitted; anything after it is forwarded unchanged.
    Done,
}

/// See the module docs.
#[derive(Debug)]
pub struct ReportRewriter {
    sideband: bool,
    protected: BTreeSet<String>,
    texts: RewriteTexts,
    /// Band-3 messages forge replaced (git's original text, for the log).
    withheld: Vec<String>,
    pending: Vec<u8>,
    report: Vec<u8>,
    state: State,
    parsed: Option<Report>,
    /// Why the report could not be read (logged by the caller).
    unreadable: Option<&'static str>,
}

impl ReportRewriter {
    /// `protected`: the refs whose `non-fast-forward` refusal is replaced
    /// by `texts.force_refused`.
    pub fn new(sideband: bool, protected: BTreeSet<String>, mut texts: RewriteTexts) -> Self {
        texts.force_refused = texts.force_refused.replace(['\n', '\r'], " ");
        texts.withheld = texts.withheld.replace(['\n', '\r'], " ");
        texts.scrub.retain(|s| !s.is_empty());
        Self {
            sideband,
            protected,
            texts,
            withheld: Vec::new(),
            pending: Vec::new(),
            report: Vec::new(),
            state: State::Open,
            parsed: None,
            unreadable: None,
        }
    }

    /// The next bytes of git's stdout; returns what to send now.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<u8> {
        if matches!(self.state, State::Done) || self.unreadable.is_some() {
            return chunk.to_vec();
        }
        self.pending.extend_from_slice(chunk);
        let mut out = Vec::new();
        let mut used_total = 0;
        loop {
            let rest = self.pending.get(used_total..).unwrap_or_default();
            let decoded = match pkt_line::decode(rest, PktMode::V0) {
                Ok(Some(d)) => d,
                Ok(None) => break,
                Err(_) => {
                    self.give_up("pkt_line", &mut out, used_total);
                    return out;
                }
            };
            let (packet, used) = decoded;
            let raw = rest.get(..used).unwrap_or_default().to_vec();
            used_total += used;
            if self.sideband {
                match packet {
                    Packet::Data(payload) => match pkt_line::decode_sideband(&payload) {
                        Ok((Band::Data, data)) => self.report.extend_from_slice(data),
                        Ok((Band::Progress, data)) => self.progress(data, &raw, &mut out),
                        Ok((Band::Error, data)) => {
                            self.withheld
                                .push(String::from_utf8_lossy(data).trim_end().to_owned());
                            let text = format!("{}\n", self.texts.withheld);
                            if pkt_line::encode_sideband(&mut out, Band::Error, text.as_bytes())
                                .is_err()
                            {
                                out.extend_from_slice(&raw);
                            }
                        }
                        Err(_) => out.extend_from_slice(&raw),
                    },
                    Packet::Flush => {
                        // The end of the answer: the report, then this flush.
                        self.complete(&mut out);
                        out.extend_from_slice(&raw);
                        self.state = State::Done;
                        let tail = self.pending.get(used_total..).unwrap_or_default().to_vec();
                        out.extend_from_slice(&tail);
                        self.pending.clear();
                        return out;
                    }
                    Packet::Delim | Packet::ResponseEnd => out.extend_from_slice(&raw),
                }
            } else {
                self.report.extend_from_slice(&raw);
                if packet == Packet::Flush {
                    self.complete(&mut out);
                    self.state = State::Done;
                    let tail = self.pending.get(used_total..).unwrap_or_default().to_vec();
                    out.extend_from_slice(&tail);
                    self.pending.clear();
                    return out;
                }
            }
            if self.report.len() > MAX_REPORT_BYTES {
                self.give_up("report_too_large", &mut out, used_total);
                return out;
            }
        }
        self.pending.drain(..used_total);
        out
    }

    /// Band 2: forwarded, the scrubbed strings replaced.
    fn progress(&self, data: &[u8], raw: &[u8], out: &mut Vec<u8>) {
        let text = String::from_utf8_lossy(data);
        if !self.texts.scrub.iter().any(|s| text.contains(s.as_str())) {
            out.extend_from_slice(raw);
            return;
        }
        let mut clean = text.into_owned();
        for s in &self.texts.scrub {
            clean = clean.replace(s.as_str(), "<storage>");
        }
        if pkt_line::encode_sideband(out, Band::Progress, clean.as_bytes()).is_err() {
            out.extend_from_slice(raw);
        }
    }

    /// git's band-3 messages forge replaced (original text, for the log).
    pub fn withheld(&self) -> &[String] {
        &self.withheld
    }

    /// git's stdout ended. Returns the bytes still held (unchanged) and
    /// what was reported: `Err(reason)` when no complete, readable report
    /// was seen.
    pub fn finish(mut self) -> (Vec<u8>, Result<Report, &'static str>) {
        let mut out = Vec::new();
        if matches!(self.state, State::Open) && self.unreadable.is_none() {
            // git ended before its report was complete.
            self.flush_report_raw(&mut out);
            out.append(&mut self.pending);
            return (out, Err("report_incomplete"));
        }
        match (self.parsed, self.unreadable) {
            (Some(report), None) => (out, Ok(report)),
            (_, Some(reason)) => (out, Err(reason)),
            (None, None) => (out, Err("report_incomplete")),
        }
    }

    /// Forward everything held, unchanged, and stop reading.
    fn give_up(&mut self, reason: &'static str, out: &mut Vec<u8>, used: usize) {
        self.unreadable = Some(reason);
        // `..used` was already handled (forwarded, or held in the report).
        self.pending.drain(..used.min(self.pending.len()));
        self.flush_report_raw(out);
        out.append(&mut self.pending);
    }

    /// The held report bytes, framed as they came.
    fn flush_report_raw(&mut self, out: &mut Vec<u8>) {
        let report = std::mem::take(&mut self.report);
        if report.is_empty() {
            return;
        }
        if self.sideband {
            // Re-framed into band 1 (git sent the same bytes there).
            if pkt_line::encode_sideband(out, Band::Data, &report).is_err() {
                out.extend_from_slice(&report);
            }
        } else {
            out.extend_from_slice(&report);
        }
    }

    /// The report is complete: parse, rewrite and emit it.
    fn complete(&mut self, out: &mut Vec<u8>) {
        let report = std::mem::take(&mut self.report);
        let packets = match pkt_line::decode_all(&report, PktMode::V0) {
            Ok(p) => p,
            Err(_) => {
                self.report = report;
                self.unreadable = Some("report_framing");
                self.flush_report_raw(out);
                return;
            }
        };
        let mut parsed = Report::default();
        let mut inner = Vec::new();
        let mut first = true;
        for packet in &packets {
            let Packet::Data(payload) = packet else {
                // The report's own flush.
                inner.extend_from_slice(pkt_line::FLUSH);
                continue;
            };
            let text = String::from_utf8_lossy(payload);
            let line = text.strip_suffix('\n').unwrap_or(&text);
            let mut replaced = None;
            if first {
                first = false;
                match line.strip_prefix("unpack ") {
                    Some("ok") => parsed.unpack_ok = true,
                    Some(err) => {
                        parsed.unpack_error = Some(err.to_owned());
                        parsed.withheld.push(format!("unpack {err}"));
                        replaced = Some(format!("unpack {}\n", self.texts.withheld));
                    }
                    None => self.unreadable = Some("report_unpack_missing"),
                }
            } else if let Some(name) = line.strip_prefix("ok ") {
                parsed.ok.push(name.to_owned());
            } else if let Some(rest) = line.strip_prefix("ng ") {
                let (name, reason) = rest.split_once(' ').unwrap_or((rest, ""));
                if reason.starts_with("non-fast-forward") && self.protected.contains(name) {
                    replaced = Some(format!("ng {name} {}\n", self.texts.force_refused));
                    parsed.rewritten.push(name.to_owned());
                } else if !GIT_NG_REASONS.contains(&reason) {
                    parsed.withheld.push(format!("ng {name} {reason}"));
                    replaced = Some(format!("ng {name} {}\n", self.texts.withheld));
                }
                parsed.ng.push((name.to_owned(), reason.to_owned()));
            } else if !line.starts_with("option ") {
                self.unreadable = Some("report_line");
            }
            let encoded = match replaced {
                Some(new) => pkt_line::encode_data(&mut inner, new.as_bytes()),
                None => pkt_line::encode_data(&mut inner, payload),
            };
            if encoded.is_err() {
                self.unreadable = Some("report_line");
            }
        }
        if self.unreadable.is_some() {
            // Forward git's own bytes; the caller logs the reason.
            self.report = report;
            self.flush_report_raw(out);
            return;
        }
        if self.sideband {
            if pkt_line::encode_sideband(out, Band::Data, &inner).is_err() {
                self.unreadable = Some("report_line");
                return;
            }
        } else {
            out.extend_from_slice(&inner);
        }
        self.parsed = Some(parsed);
    }
}

/// The in-band refusal of a whole push (git was not run): `messages` on
/// band 2 first (git prints each as `remote: <message>`, side-band
/// clients only), then `unpack ok` and one `ng <ref> <reason>` per
/// command, in request order, then the flush(es). `ng` reasons are one
/// line each (newlines replaced).
pub fn refusal_report(
    sideband: bool,
    messages: &[String],
    refs: &[(String, String)],
) -> Result<Vec<u8>, ForgeError> {
    let mut report = Vec::new();
    pkt_line::encode_data(&mut report, b"unpack ok\n")?;
    for (name, reason) in refs {
        let reason = reason.replace(['\n', '\r'], " ");
        let mut line = format!("ng {name} {reason}");
        if line.len() + 1 > pkt_line::MAX_PKT_DATA {
            let mut cut = pkt_line::MAX_PKT_DATA - 1;
            while !line.is_char_boundary(cut) {
                cut -= 1;
            }
            line.truncate(cut);
        }
        line.push('\n');
        pkt_line::encode_data(&mut report, line.as_bytes())?;
    }
    report.extend_from_slice(pkt_line::FLUSH);
    if !sideband {
        return Ok(report);
    }
    let mut out = Vec::new();
    for message in messages {
        let mut text = message.replace(['\n', '\r'], " ");
        text.push('\n');
        pkt_line::encode_sideband(&mut out, Band::Progress, text.as_bytes())?;
    }
    pkt_line::encode_sideband(&mut out, Band::Data, &report)?;
    out.extend_from_slice(pkt_line::FLUSH);
    Ok(out)
}

#[cfg(test)]
#[path = "receive_report_tests.rs"]
mod tests;
