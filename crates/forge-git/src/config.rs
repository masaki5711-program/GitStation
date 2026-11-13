//! How git is run: binary, forge-owned home, timeouts and concurrency limits.

use std::path::PathBuf;
use std::time::Duration;

pub use forge_core::config::GitPackConfig;

/// The subcommands that read or write packs in bulk and get the memory caps
/// of [`GitConfig::pack`] as `-c` (first, so a command's own `-c` of the same
/// key wins): Smart HTTP's `upload-pack` (→ `pack-objects`) and
/// `receive-pack` (→ `index-pack`), maintenance's `gc` (→ `repack` →
/// `pack-objects`), the push mirror's `push` (→ `pack-objects`), and
/// `cat-file`, whose `--batch-command` readers are long-lived
/// (`batch.rs`: up to `batch_per_repo × batch_max_repos` at once). `-c`
/// reaches git's child processes too (`GIT_CONFIG_PARAMETERS`).
pub(crate) const PACK_HEAVY_SUBCOMMANDS: &[&str] = &[
    "upload-pack",
    "receive-pack",
    "pack-objects",
    "index-pack",
    "repack",
    "gc",
    "push",
    "fetch",
    "cat-file",
];

/// The `-c` pairs of the memory caps, in this order (argv tests pin it).
pub(crate) fn pack_cap_overrides(pack: &GitPackConfig) -> [(&'static str, String); 5] {
    [
        ("pack.windowMemory", format!("{}m", pack.window_memory_mib)),
        ("pack.threads", pack.threads.to_string()),
        (
            "core.deltaBaseCacheLimit",
            format!("{}m", pack.delta_base_cache_mib),
        ),
        (
            "core.packedGitLimit",
            format!("{}m", pack.packed_git_limit_mib),
        ),
        (
            "core.packedGitWindowSize",
            format!("{}m", pack.packed_git_window_mib),
        ),
    ]
}

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
    /// Maximum concurrent captured git subprocesses (short calls: rev-list,
    /// diff, merge-tree, …; each a few MiB to tens of MiB, the pack-heavy
    /// ones bounded by [`Self::pack`]). Memory budget: see [`Self::pack`].
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
    /// The memory caps of the pack-heavy subcommands
    /// ([`PACK_HEAVY_SUBCOMMANDS`], `git.pack.*`).
    ///
    /// Memory budget of git under one `forge serve` (sizing:
    /// `ops/runbooks/memory-sizing.md`): at most `max_streams` clones /
    /// fetches / pushes at once, each one `pack-objects` (or `index-pack`)
    /// using about `objects × ~200 B + threads × window_memory +
    /// delta_base_cache + mapped pack ≤ packed_git_limit`; plus at most
    /// `max_concurrency` short captured calls (each a few MiB to tens of
    /// MiB) and the browse `cat-file` readers (`batch_per_repo ×
    /// batch_max_repos`, each ≤ `delta_base_cache` + its mapped packs).
    pub pack: GitPackConfig,
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
            pack: GitPackConfig::DEFAULT,
        }
    }

    /// The empty, forge-owned hooks directory every Smart HTTP subprocess
    /// gets as `core.hooksPath` (ADR-0009 amendment, M1 phase 9).
    pub fn empty_hooks_dir(&self) -> PathBuf {
        self.home.join("empty-hooks")
    }
}
