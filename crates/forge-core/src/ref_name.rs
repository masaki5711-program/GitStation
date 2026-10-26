//! `RefName`: a fully qualified ref (`refs/heads/main`) validated with git's
//! `check-ref-format` rules plus one forge rule — no path component may start
//! with `-` (defence in depth against option injection; refs are still passed
//! after `--`).

use std::fmt;

use forge_errors::{ErrorCode, ForgeError};

/// A validated, fully qualified ref name.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RefName(String);

impl RefName {
    /// Validate `name` (must start with `refs/`).
    pub fn parse(name: &str) -> Result<Self, ForgeError> {
        match check(name) {
            Ok(()) => Ok(Self(name.to_string())),
            Err(reason) => Err(ForgeError::new(ErrorCode::InvalidRefName)
                .with_context("reason", reason)
                .with_context("len", name.len())),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The branch name for `refs/heads/<name>`, if this is a branch.
    pub fn branch(&self) -> Option<&str> {
        self.0.strip_prefix("refs/heads/")
    }

    /// The tag name for `refs/tags/<name>`, if this is a tag.
    pub fn tag(&self) -> Option<&str> {
        self.0.strip_prefix("refs/tags/")
    }
}

impl fmt::Display for RefName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The rules of `git check-ref-format` (without `--allow-onelevel` or
/// `--refspec-pattern`), returning which rule failed.
fn check(name: &str) -> Result<(), &'static str> {
    if !name.starts_with("refs/") {
        return Err("not_under_refs");
    }
    if name.len() > 1024 {
        return Err("too_long");
    }
    if name.ends_with('/') {
        return Err("trailing_slash");
    }
    if name.ends_with('.') {
        return Err("trailing_dot");
    }
    if name.contains("//") {
        return Err("empty_component");
    }
    if name.contains("..") {
        return Err("double_dot");
    }
    if name.contains("@{") {
        return Err("at_brace");
    }
    if name
        .chars()
        .any(|c| (c as u32) < 0x20 || c == '\u{7f}' || " ~^:?*[\\".contains(c))
    {
        return Err("forbidden_character");
    }
    for component in name.split('/') {
        if component.starts_with('.') {
            return Err("component_starts_with_dot");
        }
        if component.ends_with(".lock") {
            return Err("component_ends_with_lock");
        }
        if component.starts_with('-') {
            return Err("component_starts_with_dash");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_ref_names_are_rejected() {
        for bad in [
            "main",
            "refs/heads/",
            "refs/heads/a..b",
            "refs/heads/a b",
            "refs/heads/a~1",
            "refs/heads/a^",
            "refs/heads/a:b",
            "refs/heads/a?",
            "refs/heads/a*",
            "refs/heads/a[",
            "refs/heads/a\\b",
            "refs/heads/.hidden",
            "refs/heads/x.lock",
            "refs/heads/a@{1}",
            "refs/heads//a",
            "refs/heads/a.",
            "refs/heads/-rf",
            "refs/heads/a\u{7f}",
            "refs/heads/a\nb",
        ] {
            let err = RefName::parse(bad).expect_err(bad);
            assert_eq!(err.code(), ErrorCode::InvalidRefName, "{bad}");
        }
    }

    #[test]
    fn valid_ref_names_are_accepted() {
        for good in [
            "refs/heads/main",
            "refs/heads/feature/PBI-123-login-form",
            "refs/heads/fix/L2-pbi-43-e1-until-precision",
            "refs/tags/v1.4.0",
            "refs/heads/日本語",
            "refs/heads/a.b",
        ] {
            let r = RefName::parse(good).expect(good);
            assert_eq!(r.as_str(), good);
        }
        let r = RefName::parse("refs/heads/main").expect("valid");
        assert_eq!(r.branch(), Some("main"));
        assert_eq!(
            RefName::parse("refs/tags/v1").expect("valid").tag(),
            Some("v1")
        );
    }
}
