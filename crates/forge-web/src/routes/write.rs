//! What the group and project write routes share (M1 phase 8b MR7a): the
//! hosting commands from the state, bounded loader reads, `{id}` between a
//! prefix and a suffix, the change's request and the uncached JSON answer.

use std::future::Future;

use axum::http::header::{CACHE_CONTROL, VARY};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use forge_auth::{ResourceCtx, UnresolvedCtx, UnresolvedReason};
use forge_core::{parse_positive_id, Role};
use forge_errors::{ErrorCode, ForgeError};
use forge_hosting::{Hosting, WriteRequest};
use serde::Serialize;
use serde_json::Value;

use crate::app::AppState;
use crate::registry::{Lookup, WriteParts};

/// The hosting commands; `FG-SYS-0001 reason=hosting_unconfigured` when the
/// server was built without them (`forge serve` always sets them).
pub(crate) fn hosting(state: &AppState) -> Result<&Hosting, ForgeError> {
    state.hosting.as_deref().ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "hosting_unconfigured")
    })
}

/// One read, bounded on the client side (forge-db bounds its statement
/// too): `FG-WEB-0002 operation=<operation>` when it does not answer.
pub(crate) async fn bounded<T>(
    state: &AppState,
    operation: &'static str,
    read: impl Future<Output = Result<T, ForgeError>>,
) -> Result<T, ForgeError> {
    let limit = state.lookup_timeout;
    match tokio::time::timeout(limit, read).await {
        Ok(result) => result,
        Err(_) => Err(ForgeError::new(ErrorCode::RequestTimeout)
            .with_context("operation", operation)
            .with_context("timeout_ms", limit.as_millis())),
    }
}

/// The positive id between `prefix` and `suffix` of the raw path (never
/// decoded; the permalink's rule, [`parse_positive_id`]).
pub(crate) fn id_in(raw_path: &str, prefix: &str, suffix: &str) -> Option<i64> {
    let rest = raw_path.strip_prefix(prefix)?;
    let raw = rest.strip_suffix(suffix)?;
    if raw.contains('/') {
        return None;
    }
    parse_positive_id(raw)
}

/// Nothing the route can act on: decided by `authorize()` (`FG-REPO-0005`).
pub(crate) fn absent<R>() -> Lookup<R> {
    Lookup::Unresolved {
        ctx: UnresolvedCtx::new(UnresolvedReason::Absent),
        why: None,
    }
}

/// The request a write belongs to: its id and the token it came with.
pub(crate) fn write_request<R: ResourceCtx, B>(write: &WriteParts<R, B>) -> WriteRequest {
    WriteRequest {
        request_id: write.ctx.request_id.clone(), // clone: the context stays for the error path
        credential_id: write.credential_id,
    }
}

/// A response body as JSON (`FG-SYS-0001` if it cannot be serialized).
pub(crate) fn json<T: Serialize>(body: T) -> Result<Value, ForgeError> {
    serde_json::to_value(body).map_err(|e| {
        ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "response_serialize")
            .with_source(e)
    })
}

/// `status` with the JSON body, never cached by a shared cache (the body
/// depends on who asks).
pub(crate) fn no_store(status: StatusCode, body: Value) -> Response {
    let mut res = (status, Json(body)).into_response();
    let headers = res.headers_mut();
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
    headers.insert(VARY, HeaderValue::from_static("Authorization, Cookie"));
    res
}

/// Whether a caller holding `role` on the target could manage its members
/// or protections (Maintainer and above): only then does a loader read the
/// member's role or the rule's level (M1 phase 8b MR8 review). Everyone
/// else is decided on `from = None` — `authorize()` denies them on the
/// caller's role before the target's matters, so the answer is the same,
/// and nothing about the member or rule is read for them. Administrators
/// hold no role here (a loader does not know the caller's rights): they are
/// decided on `from = None` too, and the write path re-reads the real value
/// under the lock and decides again.
pub(crate) fn may_manage(role: Option<Role>) -> bool {
    role.is_some_and(|r| r >= Role::Maintainer)
}

/// `read()` only when [`may_manage`] (`None` otherwise).
pub(crate) async fn read_if_manager<T, F, Fut>(
    role: Option<Role>,
    read: F,
) -> Result<Option<T>, ForgeError>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<T, ForgeError>>,
{
    if may_manage(role) {
        read().await.map(Some)
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    /// A counting loader: a caller below Maintainer (or without a role)
    /// never triggers the member / rule read.
    #[tokio::test]
    async fn only_a_possible_manager_triggers_the_target_read() {
        let reads = AtomicUsize::new(0);
        let read = || async {
            reads.fetch_add(1, Ordering::SeqCst);
            Ok::<_, ForgeError>(Some(Role::Owner))
        };
        for role in [
            None,
            Some(Role::Guest),
            Some(Role::Reporter),
            Some(Role::Developer),
        ] {
            let got = read_if_manager(role, read).await.expect("no read");
            assert_eq!(got, None, "{role:?}");
        }
        assert_eq!(reads.load(Ordering::SeqCst), 0);
        for role in [Role::Maintainer, Role::Owner] {
            let got = read_if_manager(Some(role), read).await.expect("read");
            assert_eq!(got, Some(Some(Role::Owner)));
        }
        assert_eq!(reads.load(Ordering::SeqCst), 2);
    }
}
