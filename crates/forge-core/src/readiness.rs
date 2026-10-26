//! Readiness probes: the database (forge-db) and later storage or the LLM
//! gateway implement this; `GET /readyz` and `forge doctor` run them. The
//! trait lives here so forge-web does not depend on the crates it probes
//! (CLAUDE.md §3-9).

use std::future::Future;
use std::pin::Pin;

use forge_errors::ForgeError;

/// The future a probe returns (boxed: probes are stored as trait objects).
pub type ProbeFuture<'a> = Pin<Box<dyn Future<Output = Result<(), ForgeError>> + Send + 'a>>;

/// One dependency that must work for the server to serve requests.
pub trait ReadinessProbe: Send + Sync {
    /// A short, log-safe name (`database`).
    fn name(&self) -> &'static str;
    /// `Ok` when usable; the error carries the code and safe context. The
    /// caller bounds it with its own timeout.
    fn check(&self) -> ProbeFuture<'_>;
}
