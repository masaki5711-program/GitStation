//! `git.pack.*`: the memory caps forge passes to every pack-heavy git it
//! runs (`upload-pack`, `receive-pack`, `gc`, the push mirror's `push`, the
//! long-lived `cat-file` readers; `forge_git::Git::command`). Validated at
//! load time like every other key (`FG-SYS-0002`).
//!
//! | key | git setting | default | range |
//! |---|---|---|---|
//! | `git.pack.window_memory_mib` | `pack.windowMemory` (per thread) | 256 | 1 ..= 4096 |
//! | `git.pack.threads` | `pack.threads` | 2 | 1 ..= 64 |
//! | `git.pack.delta_base_cache_mib` | `core.deltaBaseCacheLimit` | 64 | 1 ..= 4096 |
//! | `git.pack.packed_git_limit_mib` | `core.packedGitLimit` | 1024 | 16 ..= 65536 |
//! | `git.pack.packed_git_window_mib` | `core.packedGitWindowSize` | 64 | 1 ..= 1024, at most the limit |
//!
//! The trade-off (measured: `ops/bench/git-pack-memory.py`, sizing:
//! `ops/runbooks/memory-sizing.md`): a smaller delta window or fewer
//! threads only cost time when pack-objects must search deltas (objects a
//! nightly gc has not packed yet; a clone of a gc'd repository reuses the
//! stored deltas and its bitmap); a smaller base cache inflates more often;
//! smaller pack windows map and unmap more often. None of them changes what
//! is sent.

use forge_errors::ForgeError;

use super::{invalid, unsigned};

const WINDOW_MEMORY_KEY: &str = "git.pack.window_memory_mib";
const THREADS_KEY: &str = "git.pack.threads";
const DELTA_BASE_CACHE_KEY: &str = "git.pack.delta_base_cache_mib";
const PACKED_GIT_LIMIT_KEY: &str = "git.pack.packed_git_limit_mib";
const PACKED_GIT_WINDOW_KEY: &str = "git.pack.packed_git_window_mib";

/// The memory caps of forge's pack-heavy git processes (module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GitPackConfig {
    /// `pack.windowMemory` in MiB: the delta search window of one thread.
    pub window_memory_mib: u32,
    /// `pack.threads`: delta search threads of one pack-objects / index-pack.
    pub threads: u32,
    /// `core.deltaBaseCacheLimit` in MiB: inflated delta bases kept per process.
    pub delta_base_cache_mib: u32,
    /// `core.packedGitLimit` in MiB: pack bytes mapped at once per process.
    pub packed_git_limit_mib: u32,
    /// `core.packedGitWindowSize` in MiB: one mapping of a pack.
    pub packed_git_window_mib: u32,
}

impl GitPackConfig {
    /// The built-in defaults (also what [`super::load`] applies when a key
    /// is unset).
    pub const DEFAULT: GitPackConfig = GitPackConfig {
        window_memory_mib: 256,
        threads: 2,
        delta_base_cache_mib: 64,
        packed_git_limit_mib: 1024,
        packed_git_window_mib: 64,
    };

    pub(super) fn load<'a>(get: impl Fn(&str) -> Option<&'a str>) -> Result<Self, ForgeError> {
        let d = Self::DEFAULT;
        let cfg = Self {
            window_memory_mib: ranged(&get, WINDOW_MEMORY_KEY, d.window_memory_mib, (1, 4096))?,
            threads: ranged(&get, THREADS_KEY, d.threads, (1, 64))?,
            delta_base_cache_mib: ranged(
                &get,
                DELTA_BASE_CACHE_KEY,
                d.delta_base_cache_mib,
                (1, 4096),
            )?,
            packed_git_limit_mib: ranged(
                &get,
                PACKED_GIT_LIMIT_KEY,
                d.packed_git_limit_mib,
                (16, 65536),
            )?,
            packed_git_window_mib: ranged(
                &get,
                PACKED_GIT_WINDOW_KEY,
                d.packed_git_window_mib,
                (1, 1024),
            )?,
        };
        if cfg.packed_git_window_mib > cfg.packed_git_limit_mib {
            return Err(invalid(PACKED_GIT_WINDOW_KEY, "above_limit"));
        }
        Ok(cfg)
    }
}

impl Default for GitPackConfig {
    fn default() -> Self {
        Self::DEFAULT
    }
}

fn ranged<'a>(
    get: &impl Fn(&str) -> Option<&'a str>,
    key: &'static str,
    default: u32,
    (min, max): (u32, u32),
) -> Result<u32, ForgeError> {
    let n = unsigned(get, key, default)?;
    if n == 0 {
        return Err(invalid(key, "zero"));
    }
    if n < min {
        return Err(invalid(key, "below_min"));
    }
    if n > max {
        return Err(invalid(key, "above_max"));
    }
    Ok(n)
}
