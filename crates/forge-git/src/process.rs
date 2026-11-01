//! Child-process helpers for the git wrapper: pipe I/O and reaping.

use std::io;
use std::time::Duration;

use forge_errors::ForgeError;
use tokio::io::{AsyncRead, AsyncReadExt};

/// git may exit before reading all of stdin; its exit status (and stderr)
/// then explains the failure better than the pipe error.
pub(crate) fn ignore_broken_pipe(r: io::Result<()>) -> io::Result<()> {
    match r {
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        other => other,
    }
}

/// Read up to `cap` bytes; returns `(bytes, overflowed)`.
pub(crate) async fn read_capped<R: AsyncRead + Unpin>(
    reader: Option<R>,
    cap: usize,
) -> io::Result<(Vec<u8>, bool)> {
    let Some(reader) = reader else {
        return Ok((Vec::new(), false));
    };
    let mut buf = Vec::new();
    let limit = u64::try_from(cap).unwrap_or(u64::MAX).saturating_add(1);
    reader.take(limit).read_to_end(&mut buf).await?;
    let overflow = buf.len() > cap;
    buf.truncate(cap);
    Ok((buf, overflow))
}

/// How long [`reap`] waits for a killed child.
const REAP_TIMEOUT: Duration = Duration::from_secs(5);

/// Why [`reap`] could not confirm the child is gone. The child may still be
/// running (and holding repository locks): callers attach it to the error they
/// return ([`ReapFailure::attach`]), so the boundary logs it once, with
/// `request_id` and the error code.
#[derive(Debug)]
pub(crate) struct ReapFailure {
    kind: &'static str,
    detail: String,
}

impl ReapFailure {
    /// `reap_failed=<kill_failed|wait_failed|wait_timeout>` (+ `reap_error`).
    pub(crate) fn attach(&self, err: ForgeError) -> ForgeError {
        err.with_context("reap_failed", self.kind)
            .with_context("reap_error", &self.detail)
    }
}

/// Attach a reap failure, if any, to `err`.
pub(crate) fn with_reap(err: ForgeError, reap: Result<(), ReapFailure>) -> ForgeError {
    match reap {
        Ok(()) => err,
        Err(f) => f.attach(err),
    }
}

/// Kill and reap a child we are abandoning (bounded wait).
pub(crate) async fn reap(child: &mut tokio::process::Child) -> Result<(), ReapFailure> {
    let kill = child.start_kill();
    match tokio::time::timeout(REAP_TIMEOUT, child.wait()).await {
        Ok(Ok(_)) => Ok(()),
        // An exited child refuses the kill; the wait above is what counts.
        Ok(Err(e)) => Err(ReapFailure {
            kind: "wait_failed",
            detail: e.to_string(),
        }),
        Err(_) => Err(ReapFailure {
            kind: match kill {
                Err(_) => "kill_failed",
                Ok(()) => "wait_timeout",
            },
            detail: match kill {
                Err(e) => e.to_string(),
                Ok(()) => format!("not reaped within {} ms", REAP_TIMEOUT.as_millis()),
            },
        }),
    }
}
