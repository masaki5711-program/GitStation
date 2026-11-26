//! A person's notes project (plan 20 N1, ADR-0040 §6, option N-B): the
//! first commit of `<user>/notes` and the shape of one note.
//!
//! - The page type `.forge/doctypes/note.yml` asks for the four base fields
//!   only, with `owner_kind: user` (the `owner` is one person's login,
//!   never a group path) and ids `N-YYYYMMDD-HHMMSS` (UTC); the
//!   classification defaults to `internal` (Q4).
//! - A note is `<id>.md` at the repository root (the lint wants the id to
//!   be the file's stem).
//! - A quick note is one direct commit of [`quick_note`] on `main` (the
//!   simple editor's commit; `gstn note new`).
//!
//! Everything here is pure: values in, text out. The title and the owner
//! are written as YAML double-quoted scalars, so no input changes what the
//! front matter means.

/// The page type's file.
pub const DOCTYPE_PATH: &str = ".forge/doctypes/note.yml";
/// The page type's name.
pub const DOCTYPE_NAME: &str = "note";
/// The notes project's readme (no front matter: not a document).
pub const README_PATH: &str = "README.md";
/// The notes project's path segment in the person's namespace.
pub const PROJECT_SEGMENT: &str = "notes";
/// The longest title written into a note (characters; longer is cut).
pub const MAX_TITLE_CHARS: usize = 200;

/// The page type, as committed.
pub const DOCTYPE_YML: &str = "\
# The personal note type (plan 20 N1, ADR-0040 §6), placed when the notes
# project was created. Only the four base fields are required; `owner` is
# one person's login. Ids: N-YYYYMMDD-HHMMSS (UTC).
name: note
version: 1
label: { ja: ノート, en: Note, id: Catatan }
id_pattern: '^N-[0-9]{8}-[0-9]{6}$'
owner_kind: user
defaults: { classification: internal }
";

/// The readme of a new notes project.
pub const README: &str = "\
# Notes

自分だけのノートです。ノートは `N-YYYYMMDD-HHMMSS.md`（UTC）で、`gstn note new` か画面の「すばやいメモ」で足せます。
共有したくなったら、このプロジェクトにメンバーを足してください。

Your personal notes. Each note is `N-YYYYMMDD-HHMMSS.md` (UTC); add one with
`gstn note new` or the quick note on the web. To share them, add members to
this project.

Catatan pribadi Anda. Setiap catatan adalah `N-YYYYMMDD-HHMMSS.md` (UTC).
";

/// `s` as a YAML double-quoted scalar (a JSON string).
fn quoted(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if u32::from(c) < 0x20
                || ('\u{7f}'..='\u{9f}').contains(&c)
                || matches!(
                    c,
                    '\u{2028}' | '\u{2029}' | '\u{feff}' | '\u{fffe}' | '\u{ffff}'
                ) =>
            {
                out.push_str(&format!("\\u{:04x}", u32::from(c)));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The civil date of `days` since 1970-01-01 (Howard Hinnant's algorithm).
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    // d is 1..=31 and m 1..=12 by construction.
    (
        y,
        u32::try_from(m).unwrap_or(1),
        u32::try_from(d).unwrap_or(1),
    )
}

/// The note id of `unix_seconds` (UTC): `N-YYYYMMDD-HHMMSS`. `None` before
/// the year 1000 or after 9999 (the id has four year digits).
pub fn note_id(unix_seconds: i64) -> Option<String> {
    let days = unix_seconds.div_euclid(86_400);
    let secs = unix_seconds.rem_euclid(86_400);
    let (y, m, d) = civil(days);
    if !(1000..=9999).contains(&y) {
        return None;
    }
    Some(format!(
        "N-{y:04}{m:02}{d:02}-{:02}{:02}{:02}",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    ))
}

/// The file of note `id`.
pub fn note_path(id: &str) -> String {
    format!("{id}.md")
}

/// The title a quick note gets when none is given: the body's first
/// non-empty line, without leading `#`, cut at [`MAX_TITLE_CHARS`]; `id`
/// for an empty body.
pub fn default_title(body: &str, id: &str) -> String {
    body.lines()
        .map(|l| l.trim().trim_start_matches('#').trim())
        .find(|l| !l.is_empty())
        .map_or_else(
            || id.to_owned(),
            |l| l.chars().take(MAX_TITLE_CHARS).collect(),
        )
}

/// One note: front matter (`doctype: note`, `id`, `title`, `owner`,
/// `classification: internal`) and `body` (a trailing newline added).
pub fn quick_note(id: &str, title: &str, owner: &str, body: &str) -> String {
    let title: String = title.chars().take(MAX_TITLE_CHARS).collect();
    let mut out = format!(
        "---\n\
         doctype: {DOCTYPE_NAME}\n\
         id: {id}\n\
         title: {title}\n\
         owner: {owner}\n\
         classification: internal\n\
         ---\n\
         \n",
        title = quoted(&title),
        owner = quoted(owner),
    );
    out.push_str(body.trim_end_matches(['\r', '\n']));
    out.push('\n');
    out
}

/// The files of the notes project's first commit, in commit order.
pub fn seed() -> [(&'static str, String); 2] {
    [
        (DOCTYPE_PATH, DOCTYPE_YML.to_owned()),
        (README_PATH, README.to_owned()),
    ]
}

#[cfg(test)]
#[path = "note_tests.rs"]
mod tests;
