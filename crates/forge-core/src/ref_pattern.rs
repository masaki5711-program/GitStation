//! `RefPattern`: the pattern of a protected ref rule (forge.protected_refs,
//! ADR-0008, step D4). Parsing (FG-REPO-0013 `ProtectedRefRuleInvalid`):
//!
//! 1. at most [`MAX_PATTERN_LEN`] bytes (`too_long`);
//! 2. starts with `refs/heads/` or `refs/tags/`, case-sensitively (`prefix`);
//! 3. at most one `*`, and only in the last path component (`wildcard`);
//! 4. with the `*` read as an ordinary letter, the [`RefName`] rules — git's
//!    `check-ref-format` plus forge's "no component starts with `-`"
//!    (`check_ref_format`, with the failed `rule`).
//!
//! The rules are checked in this order. The database CHECKs of
//! forge.protected_refs.pattern repeat only rules 1 and 2
//! (`protected_refs_pattern_length`, `protected_refs_pattern_prefix`), so a
//! pattern fails the database exactly when the parser reports `too_long` or
//! `prefix` (test `the_db_check_agrees_with_the_ref_pattern_parser` in
//! forge-db). Rules 3 and 4 are the parser's job alone: every writer goes
//! through `RefPattern::parse`.
//!
//! Matching ([`RefPattern::matches`]) ignores case: both sides are folded by
//! [`fold`] — each character lowercased on its own, without context (a
//! whole-string `to_lowercase` turns a word-final `Σ` into `ς` and would miss
//! refs), and final `ς` mapped to `σ`. `MAIN` and `main` are one file on a
//! case-insensitive file system, and a rule must not be dodged by changing
//! case. Unicode normalization (NFC vs NFD) is not applied: `é` written as one
//! code point and as `e` + combining accent are different patterns and refs. A `*` matches any run of
//! characters, `/` and the empty run included — matching more refs only ever
//! protects more, since the strictest matching rule wins
//! (`RefProtection::effective`).

use std::fmt;

use forge_errors::{ErrorCode, ForgeError};

use crate::ref_name::{self, RefName};

/// The longest pattern, in bytes (the CHECK `protected_refs_pattern_length`).
pub const MAX_PATTERN_LEN: usize = 255;

/// The prefixes a pattern may start with (the CHECK
/// `protected_refs_pattern_prefix`, `^refs/(heads|tags)/`).
pub const PATTERN_PREFIXES: &[&str] = &["refs/heads/", "refs/tags/"];

/// A validated protected ref pattern.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RefPattern {
    raw: String,
    /// `raw` in lowercase: what matching and duplicate checks compare.
    key: String,
}

fn invalid(reason: &'static str, len: usize) -> ForgeError {
    ForgeError::new(ErrorCode::ProtectedRefRuleInvalid)
        .with_context("reason", reason)
        .with_context("len", len)
}

impl RefPattern {
    /// Validate `raw` (rules in the module documentation). The pattern itself
    /// is never put into the error.
    pub fn parse(raw: &str) -> Result<Self, ForgeError> {
        let len = raw.len();
        if len > MAX_PATTERN_LEN {
            return Err(invalid("too_long", len));
        }
        if !PATTERN_PREFIXES.iter().any(|p| raw.starts_with(p)) {
            return Err(invalid("prefix", len));
        }
        let mut stars = raw.match_indices('*');
        if let Some((at, _)) = stars.next() {
            let after = raw.get(at + 1..).unwrap_or_default();
            if stars.next().is_some() || after.contains('/') {
                return Err(invalid("wildcard", len));
            }
        }
        // `x` is legal everywhere a `*` may stand, so only the other
        // characters decide the check-ref-format rules.
        if let Err(rule) = ref_name::check(&raw.replacen('*', "x", 1)) {
            return Err(invalid("check_ref_format", len).with_context("rule", rule));
        }
        Ok(Self {
            raw: raw.to_owned(),
            key: fold(raw),
        })
    }

    /// The pattern as written.
    pub fn as_str(&self) -> &str {
        &self.raw
    }

    /// The pattern case-folded (see `fold`): two rules of one project must not share it
    /// (FG-REPO-0013 `duplicate_pattern`; the database backstop
    /// `protected_refs_project_pattern` folds ASCII letters only).
    pub fn key(&self) -> &str {
        &self.key
    }

    /// Whether this rule applies to `name`, ignoring case.
    pub fn matches(&self, name: &RefName) -> bool {
        let name = fold(name.as_str());
        match self.key.split_once('*') {
            None => name == self.key,
            Some((head, tail)) => {
                name.len() >= head.len() + tail.len()
                    && name.starts_with(head)
                    && name.ends_with(tail)
            }
        }
    }
}

/// The case fold of patterns and ref names: every character lowercased on its
/// own (`char::to_lowercase`, so `İ` becomes `i` + U+0307 and the Kelvin sign
/// `k`), then final sigma `ς` as `σ`. Context-free, so a pattern and a ref
/// fold the same letters the same way wherever they stand; `*` is unchanged.
fn fold(s: &str) -> String {
    s.chars()
        .flat_map(char::to_lowercase)
        .map(|c| if c == 'ς' { 'σ' } else { c })
        .collect()
}

impl fmt::Display for RefPattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.raw)
    }
}
