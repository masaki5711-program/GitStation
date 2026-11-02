//! The free-text fields of a merge request (the table's CHECKs, migration
//! 20261106100000), checked before anything is looked up or written:
//! `FG-WEB-0006 reason=param_invalid` with `param` and `rule`, the shape
//! of every other request field error.

use forge_errors::{ErrorCode, ForgeError};

/// A title is 1..=255 characters.
pub const MAX_TITLE_CHARS: usize = 255;
/// A description is at most 64 KiB.
pub const MAX_DESCRIPTION_BYTES: usize = 64 * 1024;

fn invalid(param: &'static str, rule: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::RequestInvalid)
        .with_context("reason", "param_invalid")
        .with_context("param", param)
        .with_context("rule", rule)
}

/// PostgreSQL text cannot hold NUL; other control characters (except tab
/// and newline in a description) would hide text in a list.
fn has_control(value: &str, allow_newlines: bool) -> bool {
    value
        .chars()
        .any(|c| c.is_control() && !(allow_newlines && matches!(c, '\n' | '\r' | '\t')))
}

/// A title: 1..=255 characters, not blank, no control character.
pub fn check_title(title: &str) -> Result<(), ForgeError> {
    let len = title.chars().count();
    if title.trim().is_empty() {
        return Err(invalid("title", "empty"));
    }
    if len > MAX_TITLE_CHARS {
        return Err(invalid("title", "too_long").with_context("len", len));
    }
    if has_control(title, false) {
        return Err(invalid("title", "control_character"));
    }
    Ok(())
}

/// A description: at most 64 KiB, no control character but tab and line
/// breaks.
pub fn check_description(description: &str) -> Result<(), ForgeError> {
    if description.len() > MAX_DESCRIPTION_BYTES {
        return Err(invalid("description", "too_long").with_context("bytes", description.len()));
    }
    if has_control(description, true) {
        return Err(invalid("description", "control_character"));
    }
    Ok(())
}

/// A note body is at most 60 KiB (M3 R3 review): the request body's JSON
/// cap is 64 KiB (`forge_web::body::MAX_JSON_BODY_BYTES`), and the note
/// with its JSON envelope and a 4 KiB path must fit in it, so every note
/// this rule accepts can be sent. The table allows 64 KiB (the backstop,
/// migration 20261109100000).
pub const MAX_NOTE_BYTES: usize = 60 * 1024;
/// A position's path is at most 4 KiB.
pub const MAX_PATH_BYTES: usize = 4096;

/// A note body (M3 R3): plain text / Markdown stored as written — not
/// blank, at most 64 KiB, no control character but tab and line breaks.
/// Never rendered by the server, so no HTML is stripped or escaped here.
pub fn check_note_body(body: &str) -> Result<(), ForgeError> {
    if body.trim().is_empty() {
        return Err(invalid("body", "empty"));
    }
    if body.len() > MAX_NOTE_BYTES {
        return Err(invalid("body", "too_long").with_context("bytes", body.len()));
    }
    if has_control(body, true) {
        return Err(invalid("body", "control_character"));
    }
    Ok(())
}

/// An inline position's shape (whether it is in the diff is
/// FG-REVIEW-0004's): a positive version, a path of 1..=4096 bytes without
/// control characters, a positive line.
pub fn check_position_shape(version: i64, path: &str, line: i64) -> Result<(), ForgeError> {
    if version < 1 {
        return Err(invalid("version", "not_positive"));
    }
    if path.is_empty() || path.len() > MAX_PATH_BYTES {
        return Err(invalid("path", "length").with_context("bytes", path.len()));
    }
    if has_control(path, false) {
        return Err(invalid("path", "control_character"));
    }
    if line < 1 {
        return Err(invalid("line", "not_positive"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(err: ForgeError) -> String {
        err.context()
            .iter()
            .find(|(k, _)| *k == "rule")
            .map(|(_, v)| v.to_string())
            .unwrap_or_default()
    }

    #[test]
    fn titles_and_descriptions_follow_the_table_checks() {
        check_title("Add the login form").expect("plain");
        check_title(&"あ".repeat(255)).expect("255 characters");
        assert_eq!(rule(check_title("").expect_err("empty")), "empty");
        assert_eq!(rule(check_title("   ").expect_err("blank")), "empty");
        assert_eq!(
            rule(check_title(&"a".repeat(256)).expect_err("long")),
            "too_long"
        );
        assert_eq!(
            rule(check_title("a\nb").expect_err("newline")),
            "control_character"
        );
        assert_eq!(
            rule(check_title("a\0b").expect_err("nul")),
            "control_character"
        );
        let err = check_title("").expect_err("empty");
        assert_eq!(err.code(), ErrorCode::RequestInvalid);
        check_description("").expect("empty description");
        check_description("line 1\nline 2\r\n\tindented").expect("line breaks");
        check_description(&"x".repeat(MAX_DESCRIPTION_BYTES)).expect("at the cap");
        assert_eq!(
            rule(check_description(&"x".repeat(MAX_DESCRIPTION_BYTES + 1)).expect_err("long")),
            "too_long"
        );
        assert_eq!(
            rule(check_description("a\0b").expect_err("nul")),
            "control_character"
        );
    }

    #[test]
    fn note_bodies_and_positions_follow_the_table_checks() {
        check_note_body("Looks good <b>kept as is</b>\n- [ ] item").expect("markdown as is");
        check_note_body(&"x".repeat(MAX_NOTE_BYTES)).expect("at the cap");
        assert_eq!(rule(check_note_body(" \n ").expect_err("blank")), "empty");
        assert_eq!(
            rule(check_note_body(&"x".repeat(MAX_NOTE_BYTES + 1)).expect_err("long")),
            "too_long"
        );
        assert_eq!(
            rule(check_note_body("a\u{1b}[31m").expect_err("escape")),
            "control_character"
        );
        check_position_shape(1, "src/lib.rs", 1).expect("plain");
        assert_eq!(
            rule(check_position_shape(0, "a", 1).expect_err("v")),
            "not_positive"
        );
        assert_eq!(
            rule(check_position_shape(1, "", 1).expect_err("p")),
            "length"
        );
        assert_eq!(
            rule(check_position_shape(1, &"a".repeat(MAX_PATH_BYTES + 1), 1).expect_err("p")),
            "length"
        );
        assert_eq!(
            rule(check_position_shape(1, "a\nb", 1).expect_err("p")),
            "control_character"
        );
        assert_eq!(
            rule(check_position_shape(1, "a", 0).expect_err("l")),
            "not_positive"
        );
    }
}
