//! The job's log as the runner sends it (`POST …/jobs/{job_id}/log`, M4
//! C3): output is buffered and sent in numbered chunks of at most
//! [`CHUNK_BYTES`] of UTF-8 text (lossy, NUL removed, the job's token
//! masked). Past the job's limit (`FG-CI-0006`) nothing more is sent; a
//! lost lease (`FG-CI-0004`) raises the stop flag. Any other failure to
//! send is logged once per job (warn, its code) and the chunk dropped: the
//! job's result does not depend on its log.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use forge_errors::ErrorCode;

use crate::api::RunnerApi;

/// The text of one chunk (the route takes 32 KiB of text: JSON escaping
/// may double it within the 64 KiB body cap).
pub const CHUNK_BYTES: usize = 32 * 1024;
/// The most chunks of one attempt (the server's limit).
pub const MAX_CHUNKS: i64 = 4096;

/// Sends one job attempt's output.
pub struct LogSink<'a> {
    api: &'a dyn RunnerApi,
    job: i64,
    attempt: i32,
    seq: i64,
    buf: Vec<u8>,
    masks: Vec<String>,
    full: bool,
    warned: bool,
    stop: Arc<AtomicBool>,
}

impl<'a> LogSink<'a> {
    pub fn new(
        api: &'a dyn RunnerApi,
        job: i64,
        attempt: i32,
        masks: Vec<String>,
        stop: Arc<AtomicBool>,
    ) -> Self {
        Self {
            api,
            job,
            attempt,
            seq: 0,
            buf: Vec::new(),
            masks,
            full: false,
            warned: false,
            stop,
        }
    }

    /// Take output; whole chunks are sent at once.
    pub fn push(&mut self, bytes: &[u8]) {
        if self.full {
            return;
        }
        self.buf.extend_from_slice(bytes);
        while self.buf.len() >= CHUNK_BYTES && !self.full {
            let cut = cut_at(&self.buf, CHUNK_BYTES);
            let chunk: Vec<u8> = self.buf.drain(..cut).collect();
            self.send(&chunk);
        }
    }

    /// One line of the runner's own (a step, the final code).
    pub fn line(&mut self, text: &str) {
        self.push(format!("[forge-runner] {text}\n").as_bytes());
    }

    /// Send what is left.
    pub fn flush(&mut self) {
        if self.buf.is_empty() || self.full {
            return;
        }
        let chunk = std::mem::take(&mut self.buf);
        self.send(&chunk);
    }

    fn send(&mut self, raw: &[u8]) {
        if self.seq >= MAX_CHUNKS {
            self.full = true;
            return;
        }
        let mut text = String::from_utf8_lossy(raw).replace('\0', "");
        for mask in &self.masks {
            if !mask.is_empty() {
                text = text.replace(mask.as_str(), "[masked]");
            }
        }
        if text.is_empty() {
            return;
        }
        match self.api.append_log(self.job, self.attempt, self.seq, &text) {
            Ok(()) => self.seq += 1,
            Err(e) if e.code_id() == ErrorCode::JobLogTooLarge.id() => self.full = true,
            Err(e) => {
                if e.code_id() == ErrorCode::JobLeaseLost.id() {
                    self.stop.store(true, Ordering::SeqCst);
                    self.full = true;
                }
                if !self.warned {
                    self.warned = true;
                    tracing::warn!(
                        target: "forge_runner",
                        event = "forge.runner.log_send_failed",
                        error_code = e.code_id(),
                        job_id = self.job,
                        attempt = self.attempt,
                        seq = self.seq,
                        request_id = e.request_id().unwrap_or(""),
                        "a log chunk was not stored"
                    );
                }
                self.seq += 1;
            }
        }
    }
}

/// The largest cut at most `max` that does not split a UTF-8 character
/// (a run of continuation bytes is at most 3 long).
fn cut_at(buf: &[u8], max: usize) -> usize {
    let mut cut = max.min(buf.len());
    let floor = cut.saturating_sub(3);
    while cut > floor && cut < buf.len() && (buf[cut] & 0xC0) == 0x80 {
        cut -= 1;
    }
    if cut == floor && cut < buf.len() && (buf[cut] & 0xC0) == 0x80 {
        max.min(buf.len())
    } else {
        cut
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cut_never_splits_a_character() {
        let text = "aé".repeat(10);
        let bytes = text.as_bytes();
        for max in 1..bytes.len() {
            let cut = cut_at(bytes, max);
            assert!(std::str::from_utf8(&bytes[..cut]).is_ok(), "{max}");
        }
    }
}
