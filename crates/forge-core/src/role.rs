//! Membership roles and project visibility (ADR-0010), shared by forge-db
//! (which stores and loads them, step D2) and forge-auth (which decides with
//! them). forge-auth re-exports both, so its API is unchanged.

/// GitLab-style roles, ordered: `Guest < Reporter < Developer < Maintainer < Owner`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Role {
    Guest,
    Reporter,
    Developer,
    Maintainer,
    Owner,
}

impl Role {
    /// Every role, lowest first.
    pub const ALL: &'static [Role] = &[
        Self::Guest,
        Self::Reporter,
        Self::Developer,
        Self::Maintainer,
        Self::Owner,
    ];

    /// The database / audit string.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Guest => "guest",
            Self::Reporter => "reporter",
            Self::Developer => "developer",
            Self::Maintainer => "maintainer",
            Self::Owner => "owner",
        }
    }

    /// The role named by `raw` (exact match), `None` for anything else.
    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|r| r.as_str() == raw)
    }
}

/// Who may read a project besides its members.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visibility {
    /// Members only.
    Private,
    /// Every signed-in user may read.
    Internal,
}

impl Visibility {
    /// The database / audit string.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Private => "private",
            Self::Internal => "internal",
        }
    }

    /// The visibility named by `raw` (exact match), `None` for anything else.
    pub fn parse(raw: &str) -> Option<Self> {
        [Self::Private, Self::Internal]
            .into_iter()
            .find(|v| v.as_str() == raw)
    }
}
