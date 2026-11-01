//! `server.*` connection limits (M1 phase 9 MR0, connection fairness).
//! Validated at load time like every other key (`FG-SYS-0002`).

use std::time::Duration;

use forge_errors::ForgeError;

/// The limits of `forge serve`'s connection loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServerLimitsConfig {
    /// `server.max_connections`: open connections in total (default 1024).
    /// A further client is answered `FG-WEB-0008` (503) and closed.
    pub max_connections: u32,
    /// `server.max_connections_per_peer`: open connections from one socket
    /// peer address (default 64, at most `max_connections`). Behind a
    /// reverse proxy the peer is the proxy: the cap then applies to all its
    /// clients together, so size it for the proxy (there is no trusted
    /// `X-Forwarded-For` handling).
    pub max_connections_per_peer: u32,
    /// `server.header_read_timeout_ms`: time to send the request headers,
    /// also the idle time allowed between keep-alive requests (default 30 s).
    pub header_read_timeout: Duration,
    /// `server.keep_alive_max_ms`: a connection older than this is closed
    /// after its current request (default 10 min). A running request is
    /// never cut, so a long git stream finishes first.
    pub keep_alive_max: Duration,
    /// `server.keep_alive_max_requests`: requests served on one connection
    /// (default 1000); the last response carries `Connection: close`.
    pub keep_alive_max_requests: u32,
    /// `server.write_idle_timeout_ms`: a response write that makes no
    /// progress for this long (the client does not read) closes the
    /// connection (default 60 s, like `git.http.idle_timeout_ms`). Only a
    /// stalled socket counts: a git stream that keeps sending is never idle.
    pub write_idle_timeout: Duration,
}

impl ServerLimitsConfig {
    /// The built-in defaults.
    pub const DEFAULT: ServerLimitsConfig = ServerLimitsConfig {
        max_connections: 1024,
        max_connections_per_peer: 64,
        header_read_timeout: Duration::from_secs(30),
        keep_alive_max: Duration::from_secs(600),
        keep_alive_max_requests: 1000,
        write_idle_timeout: Duration::from_secs(60),
    };

    /// Cross-key rules (each key is already positive).
    pub(super) fn validate(self) -> Result<Self, ForgeError> {
        if self.max_connections_per_peer > self.max_connections {
            return Err(super::invalid(
                "server.max_connections_per_peer",
                "above_max",
            ));
        }
        if self.header_read_timeout > self.keep_alive_max {
            return Err(super::invalid(
                "server.header_read_timeout_ms",
                "above_keep_alive_max",
            ));
        }
        Ok(self)
    }
}
