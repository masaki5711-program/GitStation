//! Versions and ranges of `requires` (plan 16 A7, ADR-0032 Amendment A7):
//! the builder resolves `slug@range` against the published versions.
//!
//! A version is Semantic Versioning 2.0.0 without build metadata (what
//! `part.toml` accepts). A range is one or more comparators separated by
//! `,` or spaces, all of which must hold:
//!
//! | Comparator | Means |
//! |---|---|
//! | `*` | any release |
//! | `^1.2.3`, `1.2.3` | `>=1.2.3, <2.0.0` (`^0.2.3`: `<0.3.0`; `^0.0.3`: `<0.0.4`) |
//! | `~1.2.3` | `>=1.2.3, <1.3.0` |
//! | `=1.2.3` | exactly |
//! | `>=`, `>`, `<=`, `<` | ordered |
//!
//! Missing minor / patch count as 0 (`^1` is `^1.0.0`). A pre-release
//! version matches only an `=` comparator naming it exactly: a range never
//! slides onto a pre-release by itself. Anything else is not a range
//! (`None`), which the builder reports — never "matches everything".

use std::cmp::Ordering;

/// A parsed version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    /// The pre-release identifiers (empty for a release).
    pub pre: Vec<String>,
}

impl Version {
    /// Parse `M.m.p` or `M.m.p-pre.release` (what `part.toml` accepts).
    pub fn parse(s: &str) -> Option<Version> {
        if !crate::manifest::valid_semver(s) {
            return None;
        }
        let (core, pre) = match s.split_once('-') {
            Some((c, p)) => (c, p.split('.').map(str::to_owned).collect()),
            None => (s, Vec::new()),
        };
        let mut nums = core.split('.').map(|n| n.parse::<u64>().ok());
        let (Some(Some(major)), Some(Some(minor)), Some(Some(patch))) =
            (nums.next(), nums.next(), nums.next())
        else {
            return None;
        };
        Some(Version {
            major,
            minor,
            patch,
            pre,
        })
    }

    fn triple(&self) -> (u64, u64, u64) {
        (self.major, self.minor, self.patch)
    }
}

fn cmp_pre(a: &[String], b: &[String]) -> Ordering {
    match (a.is_empty(), b.is_empty()) {
        (true, true) => return Ordering::Equal,
        // A release is above its pre-releases.
        (true, false) => return Ordering::Greater,
        (false, true) => return Ordering::Less,
        (false, false) => {}
    }
    for (x, y) in a.iter().zip(b) {
        let ord = match (x.parse::<u64>(), y.parse::<u64>()) {
            (Ok(m), Ok(n)) => m.cmp(&n),
            (Ok(_), Err(_)) => Ordering::Less,
            (Err(_), Ok(_)) => Ordering::Greater,
            (Err(_), Err(_)) => x.cmp(y),
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    a.len().cmp(&b.len())
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        self.triple()
            .cmp(&other.triple())
            .then_with(|| cmp_pre(&self.pre, &other.pre))
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Any,
    Caret,
    Tilde,
    Exact,
    Ge,
    Gt,
    Le,
    Lt,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Comparator {
    op: Op,
    /// The bound, partial parts as 0; pre-release identifiers only for `=`.
    at: Version,
    /// How many of major / minor / patch were written (1..=3).
    written: usize,
}

fn parse_partial(s: &str) -> Option<(Version, usize)> {
    if let Some(v) = Version::parse(s) {
        return Some((v, 3));
    }
    let parts: Vec<&str> = s.split('.').collect();
    if parts.is_empty() || parts.len() > 2 {
        return None;
    }
    let mut nums = [0u64; 2];
    for (slot, p) in nums.iter_mut().zip(&parts) {
        if p.is_empty()
            || !p.bytes().all(|c| c.is_ascii_digit())
            || (p.len() > 1 && p.starts_with('0'))
        {
            return None;
        }
        *slot = p.parse().ok()?;
    }
    let minor = if parts.len() == 2 { nums[1] } else { 0 };
    Some((
        Version {
            major: nums[0],
            minor,
            patch: 0,
            pre: Vec::new(),
        },
        parts.len(),
    ))
}

fn parse_comparator(s: &str) -> Option<Comparator> {
    if s == "*" {
        return Some(Comparator {
            op: Op::Any,
            at: Version {
                major: 0,
                minor: 0,
                patch: 0,
                pre: Vec::new(),
            },
            written: 0,
        });
    }
    let (op, rest) = [
        (">=", Op::Ge),
        ("<=", Op::Le),
        (">", Op::Gt),
        ("<", Op::Lt),
        ("=", Op::Exact),
        ("^", Op::Caret),
        ("~", Op::Tilde),
    ]
    .iter()
    .find_map(|(p, op)| s.strip_prefix(p).map(|r| (*op, r)))
    .unwrap_or((Op::Caret, s));
    let (at, written) = parse_partial(rest)?;
    // Only an exact comparator may name a pre-release.
    if !at.pre.is_empty() && op != Op::Exact {
        return None;
    }
    Some(Comparator { op, at, written })
}

/// The exclusive upper bound of `^` / `~` for `c`.
fn upper(c: &Comparator) -> Version {
    let v = &c.at;
    // Only `^` and `~` have an upper bound; anything else is read as `^`.
    let tilde = c.op == Op::Tilde;
    let (major, minor, patch) = if tilde && c.written >= 2 {
        (v.major, v.minor + 1, 0)
    } else if tilde || v.major > 0 || c.written == 1 {
        (v.major + 1, 0, 0)
    } else if v.minor > 0 || c.written == 2 {
        (0, v.minor + 1, 0)
    } else {
        (0, 0, v.patch + 1)
    };
    Version {
        major,
        minor,
        patch,
        pre: Vec::new(),
    }
}

impl Comparator {
    fn holds(&self, v: &Version) -> bool {
        match self.op {
            Op::Any => true,
            Op::Exact => v == &self.at,
            Op::Ge => v >= &self.at,
            Op::Gt => v > &self.at,
            Op::Le => v <= &self.at,
            Op::Lt => v < &self.at,
            Op::Caret | Op::Tilde => v >= &self.at && v < &upper(self),
        }
    }
}

/// A parsed range (module docs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Range(Vec<Comparator>);

impl Range {
    /// Parse a range; `None` when it is not one (module docs).
    pub fn parse(s: &str) -> Option<Range> {
        let comparators: Option<Vec<Comparator>> = s
            .split([',', ' '])
            .filter(|t| !t.is_empty())
            .map(parse_comparator)
            .collect();
        let comparators = comparators?;
        if comparators.is_empty() {
            return None;
        }
        Some(Range(comparators))
    }

    /// Whether `v` satisfies every comparator (a pre-release only an
    /// exact one naming it).
    pub fn matches(&self, v: &Version) -> bool {
        if !v.pre.is_empty() && !self.0.iter().any(|c| c.op == Op::Exact && &c.at == v) {
            return false;
        }
        self.0.iter().all(|c| c.holds(v))
    }
}

/// `slug@range` (or a bare `slug`: any release) split into its slug and
/// range; `None` when the range is not one.
pub fn split_require(require: &str) -> Option<(&str, Range)> {
    match require.split_once('@') {
        Some((slug, range)) => Range::parse(range).map(|r| (slug, r)),
        None => Range::parse("*").map(|r| (require, r)),
    }
}
