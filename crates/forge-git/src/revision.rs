//! `Revision`: the only way a revision (ref or object id) reaches git —
//! placed after `--end-of-options` by `GitCommand::revision`.

use forge_core::RefName;
use forge_errors::{ErrorCode, ForgeError};

/// A revision operand: a validated ref name or a full object id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Revision(pub(crate) String);

impl Revision {
    /// A full hex object id (40 for SHA-1, 64 for SHA-256).
    pub fn object_id(hex: &str) -> Result<Self, ForgeError> {
        let ok = matches!(hex.len(), 40 | 64) && hex.bytes().all(|b| b.is_ascii_hexdigit());
        if ok {
            Ok(Self(hex.to_ascii_lowercase()))
        } else {
            Err(ForgeError::new(ErrorCode::InvalidRefName).with_context("reason", "bad_object_id"))
        }
    }

    /// `^<id>`: the commits reachable from this id are left out (a range's
    /// lower end). Only a validated object id gets the `^`, so the operand
    /// stays a revision after `--end-of-options`.
    pub fn excluding(&self) -> Self {
        Self(format!("^{}", self.0))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&RefName> for Revision {
    fn from(r: &RefName) -> Self {
        Self(r.as_str().to_string())
    }
}
