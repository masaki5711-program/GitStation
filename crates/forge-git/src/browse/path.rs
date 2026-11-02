//! Paths inside a repository tree (see [`super`]).

/// A path inside a repository's tree: `/`-separated, no empty, `.` or `..`
/// segment, no control character, at most [`RepoPath::MAX_BYTES`]. The
/// root is the empty path. Passed to git only after `--`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RepoPath(String);

impl RepoPath {
    pub const MAX_BYTES: usize = 4096;

    pub fn root() -> Self {
        Self(String::new())
    }

    /// Parse a path (one leading and one trailing `/` are ignored).
    /// `Err` names the rule: `too_long`, `empty_segment`, `dot_segment`,
    /// `control`.
    pub fn parse(raw: &str) -> Result<Self, &'static str> {
        let trimmed = raw.strip_prefix('/').unwrap_or(raw);
        let trimmed = trimmed.strip_suffix('/').unwrap_or(trimmed);
        if trimmed.is_empty() {
            return Ok(Self::root());
        }
        let segments: Vec<&str> = trimmed.split('/').collect();
        Self::from_segments(&segments)
    }

    /// A path from segments (each checked as in [`RepoPath::parse`]).
    pub fn from_segments<S: AsRef<str>>(segments: &[S]) -> Result<Self, &'static str> {
        let mut out = String::new();
        for segment in segments {
            let segment = segment.as_ref();
            if segment.is_empty() {
                return Err("empty_segment");
            }
            if segment == "." || segment == ".." {
                return Err("dot_segment");
            }
            if segment.chars().any(char::is_control) {
                return Err("control");
            }
            if !out.is_empty() {
                out.push('/');
            }
            out.push_str(segment);
            if out.len() > Self::MAX_BYTES {
                return Err("too_long");
            }
        }
        Ok(Self(out))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_root(&self) -> bool {
        self.0.is_empty()
    }

    /// The last segment (`None` for the root).
    pub fn file_name(&self) -> Option<&str> {
        self.0.rsplit('/').next().filter(|s| !s.is_empty())
    }

    /// `self/name` (`name` for the root).
    pub fn child(&self, name: &str) -> String {
        if self.is_root() {
            name.to_owned()
        } else {
            format!("{}/{name}", self.0)
        }
    }
}
