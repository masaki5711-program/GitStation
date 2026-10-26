//! Child-process helpers for the git wrapper: pipe I/O and reaping.

use std::io;
use std::time::Duration;

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

/// Kill and reap a child we are abandoning (bounded wait).
pub(crate) async fn reap(child: &mut tokio::process::Child) {
    // start_kill fails only if the child already exited; the wait observes it.
    let _ = child.start_kill(); // ignore: outcome observed by the wait below
    let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await; // ignore: best-effort reap after kill
}
