//! The job's log as the runner sends it (`POST …/jobs/{job_id}/log`, M4
//! C3): output is buffered and sent in numbered chunks of at most
//! [`CHUNK_BYTES`] of UTF-8 text (lossy, NUL removed, the job's token and
//! its masked secrets — M4 C6, with their common encodings — replaced by
//! `[masked]`). Masking runs on the buffer before a chunk is cut, and the
//! last `longest mask − 1` bytes stay buffered until more output or the
//! flush, so a value split across two writes or two chunks is still
//! found. Past the job's limit (`FG-CI-0006`) nothing more is sent; a
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
    /// Longest first, none empty.
    masks: Vec<String>,
    /// Bytes kept back at the buffer's end (the longest mask − 1).
    holdback: usize,
    full: bool,
    warned: bool,
    stop: Arc<AtomicBool>,
}

impl<'a> LogSink<'a> {
    pub fn new(
        api: &'a dyn RunnerApi,
        job: i64,
        attempt: i32,
        mut masks: Vec<String>,
        stop: Arc<AtomicBool>,
    ) -> Self {
        masks.retain(|m| !m.is_empty());
        masks.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
        masks.dedup();
        let holdback = masks.first().map_or(0, |m| m.len().saturating_sub(1));
        Self {
            api,
            job,
            attempt,
            seq: 0,
            buf: Vec::new(),
            masks,
            holdback,
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
        if self.buf.len() < CHUNK_BYTES + self.holdback {
            return;
        }
        self.mask_buffer();
        while self.buf.len() >= CHUNK_BYTES + self.holdback && !self.full {
            let cut = cut_at(&self.buf, CHUNK_BYTES);
            let chunk: Vec<u8> = self.buf.drain(..cut).collect();
            self.send(&chunk);
        }
    }

    /// Replace every mask found in the buffer (whole occurrences only; a
    /// partial one at the end waits for more bytes).
    fn mask_buffer(&mut self) {
        for mask in &self.masks {
            replace_all(&mut self.buf, mask.as_bytes(), MASK.as_bytes());
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
        self.mask_buffer();
        while !self.buf.is_empty() && !self.full {
            let cut = cut_at(&self.buf, CHUNK_BYTES);
            let chunk: Vec<u8> = self.buf.drain(..cut).collect();
            self.send(&chunk);
        }
    }

    fn send(&mut self, raw: &[u8]) {
        if self.seq >= MAX_CHUNKS {
            self.full = true;
            return;
        }
        let mut text = String::from_utf8_lossy(raw).replace('\0', "");
        // The buffer was masked already; this pass is a second wall.
        for mask in &self.masks {
            if text.contains(mask.as_str()) {
                text = text.replace(mask.as_str(), MASK);
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

/// What a mask becomes (as forge's own masking of stored chunks).
const MASK: &str = forge_core::ci_secret::MASK;

/// Replace every occurrence of `pat` in `buf` by `with` (left to right,
/// not overlapping).
fn replace_all(buf: &mut Vec<u8>, pat: &[u8], with: &[u8]) {
    if pat.is_empty() || buf.len() < pat.len() {
        return;
    }
    let Some(first) = buf.windows(pat.len()).position(|w| w == pat) else {
        return;
    };
    let mut out = Vec::with_capacity(buf.len());
    out.extend_from_slice(&buf[..first]);
    let mut i = first;
    while i < buf.len() {
        if buf[i..].starts_with(pat) {
            out.extend_from_slice(with);
            i += pat.len();
        } else {
            out.push(buf[i]);
            i += 1;
        }
    }
    *buf = out;
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

    use std::sync::Mutex;

    use forge_client::error::ClientError;

    use crate::api::{Beat, ClaimedJob, JobResult};

    /// Records every chunk sent.
    #[derive(Default)]
    struct Recorder(Mutex<Vec<String>>);

    impl RunnerApi for Recorder {
        fn request_job(&self, _: u64) -> Result<Option<ClaimedJob>, ClientError> {
            Ok(None)
        }
        fn heartbeat(&self, _: i64, _: i32) -> Result<Beat, ClientError> {
            Ok(Beat::Running)
        }
        fn append_log(&self, _: i64, _: i32, _: i64, text: &str) -> Result<(), ClientError> {
            self.0
                .lock()
                .map_err(|_| ClientError::local(ErrorCode::Internal, "lock".to_owned()))?
                .push(text.to_owned());
            Ok(())
        }
        fn complete(
            &self,
            _: i64,
            _: i32,
            _: JobResult,
            _: Option<i32>,
        ) -> Result<(), ClientError> {
            Ok(())
        }
    }

    fn sent(api: &Recorder) -> String {
        api.0.lock().expect("lock").concat()
    }

    #[test]
    fn a_secret_split_across_writes_and_chunks_is_masked() {
        let secret = "s3cr3t-value-0123456789";
        let api = Recorder::default();
        let mut sink = LogSink::new(
            &api,
            7,
            1,
            forge_core::ci_secret::mask_forms(secret),
            Arc::new(AtomicBool::new(false)),
        );
        // The value straddles the first chunk's end, written in two parts.
        let pad = "x".repeat(CHUNK_BYTES - 5);
        sink.push(pad.as_bytes());
        sink.push(&secret.as_bytes()[..9]);
        sink.push(&secret.as_bytes()[9..]);
        sink.push(b" and base64 ");
        sink.push(forge_core::ci_secret::mask_forms(secret)[1].as_bytes());
        sink.push(&vec![b'y'; CHUNK_BYTES]);
        sink.flush();
        let all = sent(&api);
        assert!(!all.contains(secret), "the value leaked");
        assert!(!all.contains("s3cr3t"), "a part leaked");
        assert!(
            all.contains("[masked] and base64 [masked]"),
            "{}",
            &all[CHUNK_BYTES - 10..CHUNK_BYTES + 40]
        );
        assert!(api
            .0
            .lock()
            .expect("lock")
            .iter()
            .all(|c| c.len() <= CHUNK_BYTES));
    }

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
