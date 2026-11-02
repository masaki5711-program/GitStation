//! How git is run: binary, forge-owned home, timeouts and concurrency limits.

use std::path::PathBuf;
use std::time::Duration;

/// How git is run. Loaded from configuration in `forge serve` / `worker`.
#[derive(Debug, Clone)]
pub struct GitConfig {
    /// Path to the git binary (or `git` to resolve via `PATH`).
    pub bin: PathBuf,
    /// The `HOME` git sees — a forge-owned directory; the global config is
    /// pinned to `<home>/.gitconfig`, so no user or shared config applies.
    /// Smart HTTP also uses `<home>/empty-hooks` as `core.hooksPath`.
    pub home: PathBuf,
    /// Upper bound for one captured invocation ([`crate::GitCommand::output`]).
    pub timeout_total: Duration,
    /// How long to wait for a free subprocess slot (captured or streaming).
    pub acquire_timeout: Duration,
    /// Maximum concurrent captured git subprocesses.
    pub max_concurrency: usize,
    /// Maximum captured stdout per invocation.
    pub max_output_bytes: usize,
    /// Maximum concurrent streaming subprocesses (Smart HTTP). A separate
    /// pool, so long clones cannot starve the short captured calls.
    pub max_streams: usize,
    /// Maximum concurrent streams per principal (the key the caller passes
    /// to [`crate::GitCommand::spawn_stream`]); over it the stream is refused
    /// at once, without waiting.
    pub max_streams_per_principal: usize,
    /// A stream with no bytes in either direction for this long is killed.
    pub stream_idle_timeout: Duration,
    /// Upper bound for one stream.
    pub stream_timeout_total: Duration,
}

impl GitConfig {
    pub fn new(bin: impl Into<PathBuf>, home: impl Into<PathBuf>) -> Self {
        Self {
            bin: bin.into(),
            home: home.into(),
            timeout_total: Duration::from_secs(300),
            acquire_timeout: Duration::from_secs(10),
            max_concurrency: 32,
            max_output_bytes: 16 * 1024 * 1024,
            max_streams: 16,
            max_streams_per_principal: 4,
            stream_idle_timeout: Duration::from_secs(60),
            stream_timeout_total: Duration::from_secs(3600),
        }
    }

    /// The empty, forge-owned hooks directory every Smart HTTP subprocess
    /// gets as `core.hooksPath` (ADR-0009 amendment, M1 phase 9).
    pub fn empty_hooks_dir(&self) -> PathBuf {
        self.home.join("empty-hooks")
    }
}
