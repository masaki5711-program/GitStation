//! Shaping git's stderr for the log: bounded, and with credentials removed
//! (git prints remote URLs and HTTP details in errors). Logs only — stderr
//! never reaches a user (CLAUDE.md §3-3).

/// Query parameters whose values are secrets.
const SECRET_PARAMS: &[&str] = &[
    "private_token",
    "access_token",
    "token",
    "password",
    "secret",
];

/// The first lines of stderr for the log (bounded, credentials removed).
pub(crate) fn stderr_head(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut head: String = text.lines().take(20).collect::<Vec<_>>().join(" | ");
    if head.len() > 2000 {
        let mut cut = 2000;
        while !head.is_char_boundary(cut) {
            cut -= 1;
        }
        head.truncate(cut);
        head.push('…');
    }
    redact(&head)
}

/// Remove URL `user:password@`, secret query values and `Authorization:`
/// header values.
pub(crate) fn redact(text: &str) -> String {
    redact_authorization(&redact_query(&redact_userinfo(text)))
}

fn redact_userinfo(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(idx) = rest.find("://") {
        let (before, after) = rest.split_at(idx + 3);
        out.push_str(before);
        let end = after
            .find(|c: char| c == '/' || c == '?' || c.is_whitespace() || c == '\'')
            .unwrap_or(after.len());
        let (authority, tail) = after.split_at(end);
        match authority.rfind('@') {
            Some(at) => {
                out.push_str("***@");
                out.push_str(authority.get(at + 1..).unwrap_or_default());
            }
            None => out.push_str(authority),
        }
        rest = tail;
    }
    out.push_str(rest);
    out
}

fn redact_query(text: &str) -> String {
    let mut out = text.to_string();
    for param in SECRET_PARAMS {
        for sep in ['?', '&'] {
            let needle = format!("{sep}{param}=");
            let mut from = 0;
            while let Some(pos) = out.get(from..).and_then(|s| s.find(&needle)) {
                let start = from + pos + needle.len();
                let end = out
                    .get(start..)
                    .and_then(|s| s.find(|c: char| c == '&' || c.is_whitespace() || c == '\''))
                    .map_or(out.len(), |e| start + e);
                out.replace_range(start..end, "***");
                from = start + 3;
            }
        }
    }
    out
}

fn redact_authorization(text: &str) -> String {
    let marker = "Authorization:";
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(pos) = rest.find(marker) {
        let (before, after) = rest.split_at(pos + marker.len());
        out.push_str(before);
        out.push_str(" ***");
        // The value runs to the end of the (joined) line.
        let end = after.find(" | ").unwrap_or(after.len());
        rest = after.get(end..).unwrap_or_default();
    }
    out.push_str(rest);
    out
}
