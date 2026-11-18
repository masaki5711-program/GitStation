//! A read that failed: who gets its error.
//!
//! - **The reader's own.** Out of its own time (`FG-GIT-0002`) or refused
//!   for its place in a queue (`FG-GIT-0004`): the read ran on the
//!   leader's deadline and waited in the leader's place, so its failure
//!   says nothing about a caller that arrived later and still has time.
//!   It goes to the leader alone; the waiters wake with no outcome and
//!   one of them reads, on its own deadline.
//! - **The read's.** Anything else (git cannot be started, git failed, the
//!   repository is not on disk): the same for whoever reads next, so every
//!   caller waiting for the read gets its own copy of it.

use std::error::Error as StdError;
use std::fmt;
use std::sync::Arc;

use forge_errors::{ErrorCode, ForgeError};

/// Whether `error` is about the caller that ran the read (its time, its
/// place in a queue) and not about the repository.
pub(super) fn is_readers_own(error: &ForgeError) -> bool {
    let code = error.code();
    code == ErrorCode::GitTimeout || code == ErrorCode::GitBusy
}

/// One caller's copy of a read's error: the same code and context, and
/// the original's cause chain for the log.
pub(super) fn shared_error(original: &Arc<ForgeError>) -> ForgeError {
    let mut copy = ForgeError::new(original.code());
    for &(key, ref value) in original.context() {
        copy = copy.with_context(key, value);
    }
    if StdError::source(original.as_ref()).is_some() {
        copy = copy.with_source(SharedCause(Arc::clone(original)));
    }
    copy
}

/// The cause of a shared error: displays as, and chains to, the original
/// error's own cause.
#[derive(Debug)]
struct SharedCause(Arc<ForgeError>);

impl fmt::Display for SharedCause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match StdError::source(self.0.as_ref()) {
            Some(cause) => write!(f, "{cause}"),
            None => Ok(()),
        }
    }
}

impl StdError for SharedCause {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        StdError::source(self.0.as_ref()).and_then(|cause| cause.source())
    }
}
