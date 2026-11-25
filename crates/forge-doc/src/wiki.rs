//! A project wiki's first commit (plan 20 W2, ADR-0040 §5): the page type
//! `.forge/doctypes/wiki.page.yml` and the first page `WIKI-0001.md`.
//!
//! The page type asks for the four base fields only (`doctype`, `id`,
//! `title`, `owner`; no headings, no blocks): a wiki is a light place, and
//! the lint runs only plan 13's deterministic checks on it (broken
//! references, duplicate ids, raw HTML). Ids are `WIKI-0001` onwards (Q6);
//! the classification defaults to `internal`.
//!
//! [`seed`] is pure: the title and the owner in, the two files out. Both
//! values are written as YAML double-quoted scalars (JSON string escapes),
//! so a display name never changes what the front matter means.

/// The page type's file.
pub const DOCTYPE_PATH: &str = ".forge/doctypes/wiki.page.yml";
/// The first page.
pub const FIRST_PAGE_PATH: &str = "WIKI-0001.md";
/// The page type's name.
pub const DOCTYPE_NAME: &str = "wiki.page";
/// The first page's id.
pub const FIRST_PAGE_ID: &str = "WIKI-0001";
/// The longest title written into the first page (characters; longer is
/// cut, never refused: it is a starting text the owner edits).
pub const MAX_TITLE_CHARS: usize = 200;

/// The page type, as committed.
pub const DOCTYPE_YML: &str = "\
# The wiki page type (plan 20 W2, ADR-0040 §5), placed when the wiki was
# created. Only the four base fields are required; the lint checks broken
# references, duplicate ids and raw HTML. Ids: WIKI-0001 onwards.
name: wiki.page
version: 1
label: { ja: Wiki のページ, en: Wiki page, id: Halaman wiki }
id_pattern: '^WIKI-[0-9]{4,6}$'
defaults: { classification: internal }
";

/// `s` as a YAML double-quoted scalar (a JSON string: `\"`, `\\` and the
/// control characters as `\uXXXX`).
fn quoted(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if u32::from(c) < 0x20 || c == '\u{7f}' || c == '\u{2028}' || c == '\u{2029}' => {
                out.push_str(&format!("\\u{:04x}", u32::from(c)));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The first page: `title` (cut at [`MAX_TITLE_CHARS`]) and `owner` (the
/// namespace path that owns the wiki) in its front matter.
pub fn first_page(title: &str, owner: &str) -> String {
    let title: String = title.chars().take(MAX_TITLE_CHARS).collect();
    format!(
        "---\n\
         doctype: {DOCTYPE_NAME}\n\
         id: {FIRST_PAGE_ID}\n\
         title: {title}\n\
         owner: {owner}\n\
         classification: internal\n\
         ---\n\
         \n\
         ## この Wiki について / About this wiki\n\
         \n\
         プロジェクトの説明・手順・決めごとをここに書きます。\n\
         Write the project's explanations, procedures and decisions here.\n\
         Tulis penjelasan, prosedur, dan keputusan proyek di sini.\n\
         \n\
         新しいページは `WIKI-0002` から番号を付けます。\n\
         ",
        title = quoted(&title),
        owner = quoted(owner),
    )
}

/// The two files of the first commit, in commit order: (path, content).
pub fn seed(title: &str, owner: &str) -> [(&'static str, String); 2] {
    [
        (DOCTYPE_PATH, DOCTYPE_YML.to_owned()),
        (FIRST_PAGE_PATH, first_page(title, owner)),
    ]
}

#[cfg(test)]
#[path = "wiki_tests.rs"]
mod tests;
