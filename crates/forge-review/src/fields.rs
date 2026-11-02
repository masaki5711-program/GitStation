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
}
