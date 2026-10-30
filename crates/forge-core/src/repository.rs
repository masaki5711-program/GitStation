//! The kind and the provisioning state of a repository (step D4; ADR-0004,
//! the CHECKs of forge.repositories.kind / state). The kind is fixed when the
//! repository is created (M1, step D plan Q3): forge_app has no UPDATE
//! privilege on the column.

/// `spec` (specifications) or `system` (system development), ADR-0004.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RepositoryKind {
    Spec,
    System,
}

impl RepositoryKind {
    /// Every kind, in the order spec, system.
    pub const ALL: &'static [RepositoryKind] = &[Self::Spec, Self::System];

    /// The database / audit string.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Spec => "spec",
            Self::System => "system",
        }
    }

    /// The kind named by `raw` (exact match), `None` for anything else.
    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|k| k.as_str() == raw)
    }
}

/// Whether the bare repository exists yet: `provisioning` from the creating
/// transaction until the provisioning job has run `init_bare` and committed
/// `mark_ready` (step D4 option A); then `ready`. The database keeps
/// `ready_at` set exactly when the state is `ready`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RepositoryState {
    Provisioning,
    Ready,
}

impl RepositoryState {
    /// Every state, in the order provisioning, ready.
    pub const ALL: &'static [RepositoryState] = &[Self::Provisioning, Self::Ready];

    /// The database string.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Provisioning => "provisioning",
            Self::Ready => "ready",
        }
    }

    /// The state named by `raw` (exact match), `None` for anything else.
    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|s| s.as_str() == raw)
    }
}
