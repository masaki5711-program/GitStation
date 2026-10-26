//! The TLS mode of the database connection (ADR-0013).

/// TLS to the database. There is no opportunistic mode (`prefer` / `allow`):
/// a downgrade must be written down as `require` or `disable`. There is no
/// `verify-ca` either: the driver's trust store also holds the public web
/// roots, so a CA check without the host name accepts any public
/// certificate (ADR-0013).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TlsMode {
    /// Encrypt, verify the chain and the host name (default).
    VerifyFull,
    /// Encrypt without verifying the server (logged as unverified).
    Require,
    /// Plain text (the development compose database).
    Disable,
}

impl TlsMode {
    pub(super) fn parse(text: &str) -> Option<Self> {
        match text {
            "verify-full" => Some(Self::VerifyFull),
            "require" => Some(Self::Require),
            "disable" => Some(Self::Disable),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::VerifyFull => "verify-full",
            Self::Require => "require",
            Self::Disable => "disable",
        }
    }
}
