//! The CI tokens (M4 C3, ADR-0029 §1): a registered runner's token
//! `fgr_<public_id>_<secret>` and a job's short-lived token
//! `fgj_<public_id>_<secret>`, in the personal access token's shape
//! (ADR-0019: 16 hex public id, 256-bit secret, only `sha256(secret)` is
//! stored, compared in constant time). The prefix tells the authentication
//! which table to look in before anything is looked up; a personal access
//! token (`fgp_`) never matches.
//!
//! The secret never reaches a log: [`CiToken`] prints as `[redacted]` and is
//! not `Serialize`.

use std::fmt;

use crate::token::{self, PublicId, TokenError, TokenSecret};

/// The prefix of a runner's token.
pub const RUNNER_TOKEN_PREFIX: &str = "fgr";
/// The prefix of a job's token.
pub const JOB_TOKEN_PREFIX: &str = "fgj";

/// Which CI credential a token is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CiTokenKind {
    Runner,
    Job,
}

impl CiTokenKind {
    pub fn prefix(self) -> &'static str {
        match self {
            Self::Runner => RUNNER_TOKEN_PREFIX,
            Self::Job => JOB_TOKEN_PREFIX,
        }
    }

    /// The log `credential`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Runner => "runner",
            Self::Job => "job",
        }
    }

    /// The kind a presented string claims by its prefix alone (`None`: not
    /// a CI token; it may be a personal access token). Never panics.
    pub fn of(presented: &str) -> Option<Self> {
        [Self::Runner, Self::Job].into_iter().find(|k| {
            presented
                .strip_prefix(k.prefix())
                .is_some_and(|rest| rest.starts_with('_'))
        })
    }
}

/// A CI token as issued (shown once) or as presented.
#[derive(Clone)]
pub struct CiToken {
    kind: CiTokenKind,
    public_id: PublicId,
    secret: TokenSecret,
}

impl CiToken {
    /// A fresh token from the OS RNG.
    pub fn generate(kind: CiTokenKind) -> Result<Self, TokenError> {
        let (public_id, secret) = token::random_parts()?;
        Ok(Self {
            kind,
            public_id,
            secret,
        })
    }

    /// Parse a presented string of `kind`. Never panics.
    pub fn parse(kind: CiTokenKind, input: &str) -> Result<Self, TokenError> {
        let (public_id, secret) = token::parse_prefixed(input, kind.prefix())?;
        Ok(Self {
            kind,
            public_id,
            secret,
        })
    }

    pub fn kind(&self) -> CiTokenKind {
        self.kind
    }

    pub fn public_id(&self) -> &PublicId {
        &self.public_id
    }

    pub fn secret(&self) -> &TokenSecret {
        &self.secret
    }

    /// `sha256(secret)`: what the database stores.
    pub fn secret_sha256(&self) -> [u8; 32] {
        token::sha256(&self.secret)
    }

    /// The `<prefix>_<public_id>_<secret>` string: only to hand the token
    /// to its runner once.
    pub fn reveal(&self) -> String {
        format!(
            "{}_{}_{}",
            self.kind.prefix(),
            self.public_id.as_str(),
            token::to_hex(self.secret.as_bytes())
        )
    }
}

impl fmt::Debug for CiToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CiToken({}_[redacted])", self.kind.prefix())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ci_token_round_trips_and_never_prints_its_secret() {
        for kind in [CiTokenKind::Runner, CiTokenKind::Job] {
            let t = CiToken::generate(kind).expect("rng");
            let shown = t.reveal();
            assert!(shown.starts_with(&format!("{}_", kind.prefix())));
            assert_eq!(CiTokenKind::of(&shown), Some(kind));
            let back = CiToken::parse(kind, &shown).expect("parse");
            assert_eq!(back.public_id(), t.public_id());
            assert!(token::verify(&t.secret_sha256(), back.secret()));
            assert!(!format!("{t:?}").contains(&shown[4..]));
        }
    }

    #[test]
    fn the_prefix_alone_names_the_kind() {
        assert_eq!(CiTokenKind::of("fgp_0123456789abcdef_00"), None);
        assert_eq!(CiTokenKind::of("fgr"), None);
        assert_eq!(CiTokenKind::of("fgrx_1"), None);
        assert_eq!(CiTokenKind::of("fgj_x"), Some(CiTokenKind::Job));
        let runner = CiToken::generate(CiTokenKind::Runner).expect("rng");
        assert!(CiToken::parse(CiTokenKind::Job, &runner.reveal()).is_err());
        assert!(CiToken::parse(CiTokenKind::Runner, "fgr_short_00").is_err());
    }
}
