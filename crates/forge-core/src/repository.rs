//! The provisioning state of a repository (step D4; the CHECK of
//! forge.repositories.state). There is no repository kind any more
//! (ADR-0023): the `forge.repositories.kind` column stays (migrations are
//! append-only) and holds the constant `'system'`, written by
//! `forge-db` at creation; nothing reads it.

/// Whether the bare repository exists yet: `provisioning` from the creating
/// transaction until the provisioning job has run `init_bare` and committed
/// `mark_ready` (step D4 option A); then `ready`. The database keeps
/// `ready_at` set exactly when the state is `ready`. `purging` (D5 MR4):
/// the purge job has started removing the repository of a project whose
/// grace period is over; nothing comes back from it (the database's guard),
/// and the rows are deleted once the bare directory is gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RepositoryState {
    Provisioning,
    Ready,
    Purging,
}

impl RepositoryState {
    /// Every state, in the order provisioning, ready, purging.
    pub const ALL: &'static [RepositoryState] = &[Self::Provisioning, Self::Ready, Self::Purging];

    /// The database string.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Provisioning => "provisioning",
            Self::Ready => "ready",
            Self::Purging => "purging",
        }
    }

    /// The state named by `raw` (exact match), `None` for anything else.
    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|s| s.as_str() == raw)
    }
}
