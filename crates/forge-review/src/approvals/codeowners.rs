//! CODEOWNERS (ADR-0024 §9, plan 11 Q5): a subset of GitLab's grammar,
//! parsed strictly — what is not understood is an error with its line
//! number and `reason`, never ignored (fail closed).
//!
//! - One rule per line: a path glob, then one or more owners, separated by
//!   blanks. `#` starts a comment (to the end of the line); blank lines are
//!   ignored. `\#` and `\ ` put a literal `#` / blank into a pattern.
//! - Globs: `*` (any characters but `/`), `?` (one character but `/`), `**`
//!   as a whole segment (any number of segments). A leading `/` anchors the
//!   pattern at the repository root; without it the pattern matches at any
//!   depth (as if it began with `**/`). A pattern matches a path itself and
//!   everything below it (a directory); a trailing `/` matches only what is
//!   below.
//! - Owners: `@username` and `@group/path` (a full group path). A single
//!   segment names a user, or a top-level group when no such user exists.
//! - The last rule matching a path wins.
//! - Refused (`unsupported`): sections `[Section]` / `^[Section]`, e-mail
//!   owners, negations `!pattern`. Refused (`pattern_invalid`): `[`, `]`,
//!   `{`, `}`, `\` other than the escapes, `***`, `**` inside a segment.
//! - Limits (decided in R5): 64 KiB, 1 000 lines, 4 KiB per line, 50
//!   owners per rule.

/// The largest CODEOWNERS file read (bytes).
pub const MAX_BYTES: usize = 64 * 1024;
/// The most lines.
pub const MAX_LINES: usize = 1_000;
/// The longest line (bytes).
pub const MAX_LINE_BYTES: usize = 4 * 1024;
/// The most owners on one line.
pub const MAX_OWNERS_PER_RULE: usize = 50;

/// Why a CODEOWNERS file cannot be used (`codeowners_invalid{line,
/// reason}`); `line` is 1-based, 0 for the whole file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodeownersError {
    pub line: u32,
    pub reason: &'static str,
}

impl CodeownersError {
    pub(crate) fn file(reason: &'static str) -> Self {
        Self { line: 0, reason }
    }
}

/// An owner as written.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Owner {
    /// `@name`: a user, or a top-level group of that path.
    Name(String),
    /// `@a/b…`: a group by its full path.
    Group(String),
}

impl Owner {
    /// As written, with the `@`.
    pub fn display(&self) -> String {
        match self {
            Owner::Name(n) | Owner::Group(n) => format!("@{n}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Seg {
    /// `**`: any number of segments.
    Any,
    /// A segment glob (`*`, `?`, literal characters).
    Glob(String),
}

/// One rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    /// 1-based line number.
    pub line: u32,
    /// The pattern as written.
    pub pattern: String,
    pub owners: Vec<Owner>,
    segs: Vec<Seg>,
}

/// A parsed CODEOWNERS file.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Codeowners {
    pub rules: Vec<Rule>,
}

fn at(line: u32, reason: &'static str) -> CodeownersError {
    CodeownersError { line, reason }
}

/// Split a line into tokens (blank-separated), honouring `\#` and `\ `,
/// dropping a comment. `Err(reason)` for another escape.
fn tokens(line: &str) -> Result<Vec<String>, &'static str> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some(e @ ('#' | ' ')) => cur.push(e),
                _ => return Err("pattern_invalid"),
            },
            '#' => break,
            c if c.is_whitespace() => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    Ok(out)
}

fn name_ok(segment: &str) -> bool {
    !segment.is_empty()
        && segment.len() <= 255
        && segment
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}

fn owner(token: &str) -> Result<Owner, &'static str> {
    let Some(name) = token.strip_prefix('@') else {
        return Err(if token.contains('@') {
            "unsupported"
        } else {
            "owner_syntax"
        });
    };
    if name.contains('@') {
        return Err("unsupported");
    }
    let segments: Vec<&str> = name.split('/').collect();
    if !segments.iter().all(|s| name_ok(s)) {
        return Err("owner_syntax");
    }
    if segments.len() == 1 {
        Ok(Owner::Name(name.to_owned()))
    } else {
        Ok(Owner::Group(name.to_owned()))
    }
}

/// The segments of a pattern (`anchored`: it began with `/`).
fn compile(pattern: &str) -> Result<Vec<Seg>, &'static str> {
    if pattern.starts_with('!') {
        return Err("unsupported");
    }
    if pattern
        .chars()
        .any(|c| matches!(c, '[' | ']' | '{' | '}') || c.is_control())
        || pattern.contains("***")
    {
        return Err("pattern_invalid");
    }
    let anchored = pattern.starts_with('/');
    let directory = pattern.ends_with('/');
    let body = pattern.trim_start_matches('/').trim_end_matches('/');
    if body.is_empty() {
        // `/` alone: the whole repository.
        return if anchored {
            Ok(vec![Seg::Any])
        } else {
            Err("pattern_invalid")
        };
    }
    let mut segs = Vec::new();
    if !anchored {
        segs.push(Seg::Any);
    }
    for part in body.split('/') {
        if part.is_empty() || part == "." || part == ".." {
            return Err("pattern_invalid");
        }
        if part == "**" {
            segs.push(Seg::Any);
        } else if part.contains("**") {
            return Err("pattern_invalid");
        } else {
            segs.push(Seg::Glob(part.to_owned()));
        }
    }
    if directory {
        // Only what is below: at least one more segment.
        segs.push(Seg::Glob("*".to_owned()));
    }
    // A match covers everything below it.
    segs.push(Seg::Any);
    Ok(segs)
}

/// Parse a CODEOWNERS file (module docs).
pub fn parse(bytes: &[u8]) -> Result<Codeowners, CodeownersError> {
    if bytes.len() > MAX_BYTES {
        return Err(CodeownersError::file("too_large"));
    }
    let text = std::str::from_utf8(bytes).map_err(|_| CodeownersError::file("not_utf8"))?;
    let mut rules = Vec::new();
    for (index, raw) in text.lines().enumerate() {
        let number = u32::try_from(index + 1).unwrap_or(u32::MAX);
        if index >= MAX_LINES {
            return Err(at(number, "too_many_lines"));
        }
        if raw.len() > MAX_LINE_BYTES {
            return Err(at(number, "line_too_long"));
        }
        let trimmed = raw.trim_start();
        if trimmed.starts_with('[') || trimmed.starts_with("^[") {
            return Err(at(number, "unsupported"));
        }
        let toks = tokens(raw).map_err(|r| at(number, r))?;
        let Some((pattern, owner_tokens)) = toks.split_first() else {
            continue;
        };
        if owner_tokens.is_empty() {
            return Err(at(number, "syntax"));
        }
        if owner_tokens.len() > MAX_OWNERS_PER_RULE {
            return Err(at(number, "too_many_owners"));
        }
        let segs = compile(pattern).map_err(|r| at(number, r))?;
        let owners = owner_tokens
            .iter()
            .map(|t| owner(t))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|r| at(number, r))?;
        rules.push(Rule {
            line: number,
            pattern: pattern.clone(), // clone: the rule keeps the pattern as written
            owners,
            segs,
        });
    }
    Ok(Codeowners { rules })
}

/// Whether one segment glob matches one path segment.
fn glob(pattern: &[char], name: &[char]) -> bool {
    match pattern.split_first() {
        None => name.is_empty(),
        Some(('*', rest)) => (0..=name.len()).any(|i| glob(rest, name.get(i..).unwrap_or(&[]))),
        Some(('?', rest)) => name.split_first().is_some_and(|(_, tail)| glob(rest, tail)),
        Some((c, rest)) => name
            .split_first()
            .is_some_and(|(n, tail)| n == c && glob(rest, tail)),
    }
}

fn matches(segs: &[Seg], path: &[Vec<char>]) -> bool {
    match segs.split_first() {
        None => path.is_empty(),
        Some((Seg::Any, rest)) => {
            (0..=path.len()).any(|i| matches(rest, path.get(i..).unwrap_or(&[])))
        }
        Some((Seg::Glob(g), rest)) => path.split_first().is_some_and(|(first, tail)| {
            let g: Vec<char> = g.chars().collect();
            glob(&g, first) && matches(rest, tail)
        }),
    }
}

impl Codeowners {
    /// The last rule matching `path` (repository-relative, `/`-separated).
    pub fn rule_for(&self, path: &str) -> Option<&Rule> {
        let parts: Vec<Vec<char>> = path.split('/').map(|s| s.chars().collect()).collect();
        self.rules.iter().rev().find(|r| matches(&r.segs, &parts))
    }

    /// Every owner the file names (deduplicated).
    pub fn owners(&self) -> Vec<Owner> {
        let mut all: Vec<Owner> = self
            .rules
            .iter()
            .flat_map(|r| r.owners.iter().cloned()) // clone: the list owns its owners
            .collect();
        all.sort();
        all.dedup();
        all
    }
}

#[cfg(test)]
#[path = "codeowners_tests.rs"]
mod tests;
