//! `git.http.*`: the Smart HTTP limits (ADR-0007 amendment, M1 phase 9
//! MR3). Validated at load time like every other key (`FG-SYS-0002`).

use std::time::Duration;

use forge_errors::ForgeError;

/// The limits of `forge serve`'s Smart HTTP routes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GitHttpConfig {
    /// `git.http.fetch_request_max_bytes`: one upload-pack request body, raw
    /// and after gzip (default 16 MiB, at most 1 GiB).
    pub fetch_request_max_bytes: u32,
    /// `git.http.idle_timeout_ms`: no byte in either direction (request body
    /// or git stream) for this long ends the request (default 60 s).
    pub idle_timeout: Duration,
    /// `git.http.timeout_ms`: one git POST from its first byte to the end of
    /// the response, and git's stream (default 1 h). Replaces the request
    /// timeout for git POSTs.
    pub timeout: Duration,
    /// `git.http.body_timeout_ms`: reading one request body completely
    /// (default 5 min, at most `timeout`). Bounds how long a slow client
    /// holds a body-read slot.
    pub body_timeout: Duration,
    /// `git.http.max_object_wants`: distinct non-commit wants (trees, blobs,
    /// tags) one upload-pack request may ask for (default 10 000): each is
    /// looked up in a full object walk.
    pub max_object_wants: u32,
    /// `git.http.want_check_timeout_ms`: the reachability check of an
    /// upload-pack request's wants (default 30 s, at most `timeout`).
    pub want_check_timeout: Duration,
    /// `git.http.max_streams`: concurrent git streams (default 16); also
    /// the number of request bodies read at once.
    pub max_streams: u32,
    /// `git.http.max_streams_per_principal`: per user (default 4, at most
    /// `max_streams`); also per-user concurrent request-body reads.
    pub max_streams_per_principal: u32,
    /// `git.http.push_max_bytes`: one push's pack (git's
    /// `receive.maxInputSize`, enforced by index-pack) and, with the
    /// command list, the request body forge forwards (default 2 GiB; a
    /// `u32`, so at most 4 GiB − 1).
    pub push_max_bytes: u32,
}

/// The largest `git.http.fetch_request_max_bytes` (1 GiB).
pub const FETCH_REQUEST_MAX_LIMIT: u32 = 1 << 30;

impl GitHttpConfig {
    /// The built-in defaults (also what [`super::load`] applies when a key is
    /// unset).
    pub const DEFAULT: GitHttpConfig = GitHttpConfig {
        fetch_request_max_bytes: 16 * 1024 * 1024,
        idle_timeout: Duration::from_secs(60),
        timeout: Duration::from_secs(3600),
        body_timeout: Duration::from_secs(300),
        max_object_wants: 10_000,
        want_check_timeout: Duration::from_secs(30),
        max_streams: 16,
        max_streams_per_principal: 4,
        push_max_bytes: 2 * 1024 * 1024 * 1024,
    };

    /// Cross-key rules (each key is already positive).
    pub(super) fn validate(self) -> Result<Self, ForgeError> {
        if self.fetch_request_max_bytes > FETCH_REQUEST_MAX_LIMIT {
            return Err(super::invalid(
                "git.http.fetch_request_max_bytes",
                "above_max",
            ));
        }
        if self.want_check_timeout > self.timeout {
            return Err(super::invalid(
                "git.http.want_check_timeout_ms",
                "above_timeout",
            ));
        }
        if self.body_timeout > self.timeout {
            return Err(super::invalid("git.http.body_timeout_ms", "above_timeout"));
        }
        if self.idle_timeout > self.timeout {
            return Err(super::invalid("git.http.idle_timeout_ms", "above_timeout"));
        }
        if self.max_streams_per_principal > self.max_streams {
            return Err(super::invalid(
                "git.http.max_streams_per_principal",
                "above_max",
            ));
        }
        Ok(self)
    }
}
