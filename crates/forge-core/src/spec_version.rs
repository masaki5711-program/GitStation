//! The order of published specification versions (M3 X2,
//! `forge/spec-drift`; plan 03 §5.4, ADR-0023 Amendment X2). Pure.
//!
//! A published version is a tag (plan 03 §5.1: "版の公開（タグ）"). A tag is
//! **version-shaped** when it is an optional prefix of ASCII letters, `-`
//! and `_` (at most [`MAX_PREFIX`] bytes: `v`, `spec-v`, `release_`)
//! followed by 1 to [`MAX_PARTS`] dot-separated decimal numbers of at most
//! 9 digits each, and nothing else. Prerelease and build suffixes
//! (`v2.0-rc1`, `v2.0+b1`) are not versions: a release candidate is never
//! "the latest published version".
//!
//! Tags compare only within one **series** (the same prefix, compared
//! exactly: `V1` and `v1` are two series), numbers as numbers, a missing
//! part as 0 (`v1.5` = `v1.5.0`). [`drift`] answers whether a pinned
//! version has a later one in its series among a repository's tags.

use std::cmp::Ordering;

/// The longest prefix.
pub const MAX_PREFIX: usize = 32;
/// The most numeric parts.
pub const MAX_PARTS: usize = 4;
/// The most digits of one part (fits `u32`).
const MAX_DIGITS: usize = 9;

/// A version-shaped tag (module docs).
#[derive(Debug, Clone)]
pub struct SpecVersion {
    prefix: String,
    parts: [u32; MAX_PARTS],
}

impl SpecVersion {
    /// The version `tag` spells, `None` when it is not version-shaped.
    pub fn parse(tag: &str) -> Option<Self> {
        let start = tag
            .find(|c: char| c.is_ascii_digit())
            .filter(|&i| i <= MAX_PREFIX)?;
        let (prefix, numbers) = tag.split_at(start);
        if !prefix
            .bytes()
            .all(|b| b.is_ascii_alphabetic() || b == b'-' || b == b'_')
        {
            return None;
        }
        let mut parts = [0u32; MAX_PARTS];
        for (i, part) in numbers.split('.').enumerate() {
            let slot = parts.get_mut(i)?;
            if part.is_empty()
                || part.len() > MAX_DIGITS
                || !part.bytes().all(|b| b.is_ascii_digit())
            {
                return None;
            }
            *slot = part.parse().ok()?;
        }
        Some(Self {
            prefix: prefix.to_owned(),
            parts,
        })
    }

    /// The series (the prefix as written).
    pub fn series(&self) -> &str {
        &self.prefix
    }
}

impl PartialEq for SpecVersion {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for SpecVersion {}

impl PartialOrd for SpecVersion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for SpecVersion {
    /// The series first (so two series never interleave), then the
    /// numbers (missing parts are 0).
    fn cmp(&self, other: &Self) -> Ordering {
        self.prefix
            .cmp(&other.prefix)
            .then_with(|| self.parts.cmp(&other.parts))
    }
}

/// How a pinned version stands against the published ones.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Drift {
    /// No later version of its series is published.
    Current,
    /// `newer` tags of its series are later; `latest` is the greatest
    /// (between equal versions spelled twice, the greater spelling).
    Behind { latest: String, newer: usize },
    /// The pin is not version-shaped: nothing to compare.
    Unversioned,
}

/// `pinned` against `tags` (every tag name of the referenced repository,
/// without `refs/tags/`).
pub fn drift<'a>(pinned: &str, tags: impl IntoIterator<Item = &'a str>) -> Drift {
    let Some(pin) = SpecVersion::parse(pinned) else {
        return Drift::Unversioned;
    };
    let mut latest: Option<(SpecVersion, &str)> = None;
    let mut newer = 0usize;
    for tag in tags {
        let Some(v) = SpecVersion::parse(tag) else {
            continue;
        };
        if v.series() != pin.series() || v <= pin {
            continue;
        }
        newer += 1;
        let greater = match &latest {
            None => true,
            Some((best, name)) => (&v, tag) > (best, *name),
        };
        if greater {
            latest = Some((v, tag));
        }
    }
    match latest {
        None => Drift::Current,
        Some((_, name)) => Drift::Behind {
            latest: name.to_owned(),
            newer,
        },
    }
}

#[cfg(test)]
#[path = "spec_version_tests.rs"]
mod tests;
