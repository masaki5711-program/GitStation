//! The sign-in's error constructors: one code per kind of failure, the
//! cause in `reason` (never a token, a claim or a response body).

use forge_errors::{ErrorCode, ForgeError};

/// `FG-AUTH-0011 LoginStateInvalid`.
pub(crate) fn state(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::LoginStateInvalid).with_context("reason", reason)
}

/// `FG-AUTH-0012 IdTokenInvalid`.
pub(crate) fn id_token(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::IdTokenInvalid).with_context("reason", reason)
}

/// `FG-AUTH-0015 IdentityProviderUnavailable` for the call `call`.
pub(crate) fn unavailable(call: &'static str, reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::IdentityProviderUnavailable)
        .with_context("call", call)
        .with_context("reason", reason)
}

/// `FG-SYS-0001` with `reason`.
pub(crate) fn internal(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::Internal).with_context("reason", reason)
}

/// A value from outside (an OAuth error code, an algorithm name) made safe
/// for a log field: at most 64 of `[A-Za-z0-9_.-]`, otherwise `invalid`.
pub(crate) fn loggable(value: &str) -> String {
    let ok = !value.is_empty()
        && value.len() <= 64
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'));
    if ok {
        value.to_owned()
    } else {
        "invalid".to_owned()
    }
}

/// 32 bytes from the OS CSPRNG (`FG-SYS-0001 reason=rng_unavailable`).
pub(crate) fn random32() -> Result<[u8; 32], ForgeError> {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes)
        .map_err(|e| internal("rng_unavailable").with_context("source", e))?;
    Ok(bytes)
}
