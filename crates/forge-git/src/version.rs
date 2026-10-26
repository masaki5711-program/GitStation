//! git version detection and the minimum-version check.

use std::fmt;

use forge_errors::{ErrorCode, ForgeError};

use crate::Git;

/// The oldest git forge supports: `merge-tree --write-tree` (ADR-0006).
pub const MINIMUM: GitVersion = GitVersion {
    major: 2,
    minor: 38,
    patch: 0,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct GitVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl fmt::Display for GitVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

impl GitVersion {
    /// Parse `git version 2.55.0.windows.5` / `git version 2.43.0`.
    pub fn parse(output: &str) -> Option<GitVersion> {
        let rest = output.trim().strip_prefix("git version ")?;
        let mut parts = rest.split(|c: char| !c.is_ascii_digit());
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next()?.parse().ok()?;
        let patch = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
        Some(GitVersion {
            major,
            minor,
            patch,
        })
    }

    /// `Ok` when at least [`MINIMUM`]; otherwise `FG-GIT-0001`.
    pub fn require_supported(self) -> Result<GitVersion, ForgeError> {
        if self >= MINIMUM {
            Ok(self)
        } else {
            Err(ForgeError::new(ErrorCode::GitUnavailable)
                .with_context("reason", "too_old")
                .with_context("found", self)
                .with_context("minimum", MINIMUM))
        }
    }
}

/// Run `git version` and check it against [`MINIMUM`].
pub async fn detect(git: &Git) -> Result<GitVersion, ForgeError> {
    let out = git.command("version").output().await?;
    let text = out.stdout_str();
    GitVersion::parse(&text)
        .ok_or_else(|| {
            ForgeError::new(ErrorCode::GitUnavailable)
                .with_context("reason", "unparseable_version")
                .with_context("output", text.chars().take(100).collect::<String>())
        })?
        .require_supported()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::GitConfig;

    #[test]
    fn parses_release_and_vendor_versions() {
        assert_eq!(
            GitVersion::parse("git version 2.55.0.windows.5\n"),
            Some(GitVersion {
                major: 2,
                minor: 55,
                patch: 0
            })
        );
        assert_eq!(
            GitVersion::parse("git version 2.39.5 (Apple Git-154)"),
            Some(GitVersion {
                major: 2,
                minor: 39,
                patch: 5
            })
        );
        assert_eq!(GitVersion::parse("hg version 1"), None);
        assert_eq!(GitVersion::parse(""), None);
        let big = GitVersion::parse("git version 2.100").expect("no patch");
        assert!(big > MINIMUM, "numeric, not lexicographic, comparison");
        assert!(GitVersion::parse("git version 3.0.0").expect("v3") > MINIMUM);
    }

    #[test]
    fn version_below_2_38_is_git_unavailable() {
        let old = GitVersion::parse("git version 2.34.1").expect("parse");
        let err = old.require_supported().expect_err("too old");
        assert_eq!(err.code(), ErrorCode::GitUnavailable);
        assert!(err.to_string().contains("reason=too_old"), "{err}");
        assert!(GitVersion::parse("git version 2.38.0")
            .expect("parse")
            .require_supported()
            .is_ok());
    }

    #[tokio::test]
    async fn installed_git_is_supported() {
        let home = tempfile::tempdir().expect("home");
        let git = Git::new(GitConfig::new("git", home.path()));
        let v = detect(&git).await.expect("git >= 2.38 must be installed");
        assert!(v >= MINIMUM);
    }
}
