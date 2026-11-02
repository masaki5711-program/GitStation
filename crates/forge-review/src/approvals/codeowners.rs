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
    /// A segment glob (`*`, `?`, literal characters), ASCII-folded,
    /// consecutive `*` collapsed.
    Glob(Vec<char>),
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

/// The most wildcards (`*`, `?`, `**`) one pattern may hold (R5 review:
/// bounds the matching work of a hostile file).
pub const MAX_WILDCARDS: usize = 16;

fn at(line: u32, reason: &'static str) -> CodeownersError {
    CodeownersError { line, reason }
}

/// Unicode format characters (category Cf): invisible, so a pattern or an
/// owner holding one would read as another (R5 review: refused, never
/// matched as written).
fn is_format(c: char) -> bool {
    matches!(
        u32::from(c),
        0x00AD
            | 0x0600..=0x0605
            | 0x061C
            | 0x06DD
            | 0x070F
            | 0x0890..=0x0891
            | 0x08E2
            | 0x180E
            | 0x200B..=0x200F
            | 0x202A..=0x202E
            | 0x2060..=0x2064
            | 0x2066..=0x206F
            | 0xFEFF
            | 0xFFF9..=0xFFFB
            | 0x110BD
            | 0x110CD
            | 0x13430..=0x1343F
            | 0x1BCA0..=0x1BCA3
            | 0x1D173..=0x1D17A
            | 0xE0001
            | 0xE0020..=0xE007F
    )
}

/// Split a line into tokens (ASCII-blank-separated), honouring `\#` and
/// `\ `, dropping a comment. `Err(reason)` for another escape, a non-ASCII
/// blank (U+3000 …) or a format character outside the comment.
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
            c if is_format(c) || (c.is_whitespace() && !c.is_ascii()) => {
                return Err("pattern_invalid")
            }
            c if c.is_ascii_whitespace() => {
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

/// One segment glob, ASCII-folded, consecutive `*` collapsed; its
/// wildcard count.
fn segment_glob(part: &str) -> (Vec<char>, usize) {
    let mut out: Vec<char> = Vec::with_capacity(part.len());
    let mut wild = 0;
    for c in part.chars() {
        if c == '*' && out.last() == Some(&'*') {
            continue;
        }
        if matches!(c, '*' | '?') {
            wild += 1;
        }
        out.push(c.to_ascii_lowercase());
    }
    (out, wild)
}

/// The segments of a pattern (`anchored`: it began with `/`), consecutive
/// `**` collapsed, at most [`MAX_WILDCARDS`] wildcards.
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
    let mut wildcards = 0;
    for part in body.split('/') {
        if part.is_empty() || part == "." || part == ".." {
            return Err("pattern_invalid");
        }
        if part == "**" {
            wildcards += 1;
            if segs.last() != Some(&Seg::Any) {
                segs.push(Seg::Any);
            }
        } else if part.contains("**") {
            return Err("pattern_invalid");
        } else {
            let (glob, wild) = segment_glob(part);
            wildcards += wild;
            segs.push(Seg::Glob(glob));
        }
    }
    if wildcards > MAX_WILDCARDS {
        return Err("pattern_invalid");
    }
    if directory {
        // Only what is below: at least one more segment.
        segs.push(Seg::Glob(vec!['*']));
    }
    // A match covers everything below it.
    if segs.last() != Some(&Seg::Any) {
        segs.push(Seg::Any);
    }
    Ok(segs)
}

/// Parse a CODEOWNERS file (module docs). A leading UTF-8 byte order mark
/// is dropped.
pub fn parse(bytes: &[u8]) -> Result<Codeowners, CodeownersError> {
    if bytes.len() > MAX_BYTES {
        return Err(CodeownersError::file("too_large"));
    }
    let text = std::str::from_utf8(bytes).map_err(|_| CodeownersError::file("not_utf8"))?;
    let text = text.strip_prefix('\u{FEFF}').unwrap_or(text);
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

/// Whether one segment glob matches one (folded) path segment: the
/// two-pointer wildcard match (one remembered `*`), at most
/// `pattern.len() × name.len()` steps — never exponential.
fn glob(pattern: &[char], name: &[char]) -> bool {
    let (mut p, mut n) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while n < name.len() {
        match pattern.get(p) {
            Some('*') => {
                star = Some((p, n));
                p += 1;
            }
            Some(c) if *c == '?' || Some(c) == name.get(n) => {
                p += 1;
                n += 1;
            }
            _ => match star {
                Some((sp, sn)) => {
                    p = sp + 1;
                    n = sn + 1;
                    star = Some((sp, sn + 1));
                }
                None => return false,
            },
        }
    }
    pattern.get(p..).unwrap_or(&[]).iter().all(|c| *c == '*')
}

/// Whether `segs` matches the whole `path`: a table over (segment, path
/// position), `segs.len() × path.len()` glob checks at most.
fn matches(segs: &[Seg], path: &[Vec<char>]) -> bool {
    let (s_len, p_len) = (segs.len(), path.len());
    // ok[i][j]: segs[i..] matches path[j..].
    let mut ok = vec![vec![false; p_len + 1]; s_len + 1];
    if let Some(last) = ok.get_mut(s_len).and_then(|row| row.get_mut(p_len)) {
        *last = true;
    }
    for i in (0..s_len).rev() {
        for j in (0..=p_len).rev() {
            let next =
                |ii: usize, jj: usize| ok.get(ii).and_then(|r| r.get(jj)).copied().unwrap_or(false);
            let value = match segs.get(i) {
                Some(Seg::Any) => next(i + 1, j) || (j < p_len && next(i, j + 1)),
                Some(Seg::Glob(g)) => {
                    j < p_len && path.get(j).is_some_and(|name| glob(g, name)) && next(i + 1, j + 1)
                }
                None => false,
            };
            if let Some(cell) = ok.get_mut(i).and_then(|r| r.get_mut(j)) {
                *cell = value;
            }
        }
    }
    ok.first().and_then(|r| r.first()).copied().unwrap_or(false)
}

/// A path split into ASCII-folded segments (R5 review: matching ignores
/// ASCII case, so `SRC/auth.rs` is owned like `src/auth.rs`).
fn folded(path: &str) -> Vec<Vec<char>> {
    path.split('/')
        .map(|s| s.chars().map(|c| c.to_ascii_lowercase()).collect())
        .collect()
}

/// [`Codeowners::owned_lines`].
pub fn owned_lines(
    file: &Codeowners,
    paths: &[String],
    deadline: Option<std::time::Instant>,
) -> Option<std::collections::BTreeMap<u32, usize>> {
    let mut lines = std::collections::BTreeMap::new();
    for path in paths {
        if deadline.is_some_and(|d| std::time::Instant::now() >= d) {
            return None;
        }
        if let Some(rule) = file.rule_for(path) {
            *lines.entry(rule.line).or_insert(0) += 1;
        }
    }
    Some(lines)
}

impl Codeowners {
    /// The last rule matching `path` (repository-relative, `/`-separated,
    /// ASCII case ignored).
    pub fn rule_for(&self, path: &str) -> Option<&Rule> {
        let parts = folded(path);
        self.rules.iter().rev().find(|r| matches(&r.segs, &parts))
    }

    /// The lines the changed `paths` need: for each path its last matching
    /// rule, counted per line. `None` once `deadline` passes (checked per
    /// path; the caller fails closed).
    pub fn owned_lines(
        &self,
        paths: &[String],
        deadline: Option<std::time::Instant>,
    ) -> Option<std::collections::BTreeMap<u32, usize>> {
        owned_lines(self, paths, deadline)
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
