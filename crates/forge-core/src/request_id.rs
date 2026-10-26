//! `RequestId`: the correlation id carried by every request and job, logged on
//! every event and shown to users as 問い合わせ番号.

use std::fmt;

/// A request/job correlation id. Generated per request unless an internal
/// caller supplies a valid one.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RequestId(String);

impl RequestId {
    /// Maximum accepted length of an externally supplied id.
    pub const MAX_LEN: usize = 64;

    /// A fresh random id (UUID v4, simple form — 32 hex chars).
    pub fn generate() -> Self {
        Self(uuid::Uuid::new_v4().simple().to_string())
    }

    /// Accept an id from an internal caller (e.g. `X-Request-Id`) only if it is
    /// short and log-safe (ASCII alphanumerics and `-`/`_`), so it can never
    /// inject into structured logs. Returns `None` otherwise.
    pub fn parse(raw: &str) -> Option<Self> {
        let ok = !raw.is_empty()
            && raw.len() <= Self::MAX_LEN
            && raw
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        ok.then(|| Self(raw.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn generated_ids_are_unique_and_log_safe() {
        let a = RequestId::generate();
        let b = RequestId::generate();
        assert_ne!(a, b);
        assert_eq!(a.as_str().len(), 32);
        assert!(RequestId::parse(a.as_str()).is_some());
    }

    #[test]
    fn rejects_ids_that_could_inject_into_logs() {
        for bad in ["", "a b", "x\ny", "\"quoted\"", "a=b", &"x".repeat(65)] {
            assert!(RequestId::parse(bad).is_none(), "accepted {bad:?}");
        }
        assert!(RequestId::parse("req-01_AB").is_some());
    }
}
