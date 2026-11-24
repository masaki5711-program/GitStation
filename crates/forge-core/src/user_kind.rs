//! The kind of a user account (D1). There is no `device` kind: factory edge
//! devices are not users (plan 01 §3.10; they authenticate as devices later).

/// `human`, `service_account` or `bot` (the CHECK of forge.users.kind).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UserKind {
    Human,
    ServiceAccount,
    Bot,
}

impl UserKind {
    /// Every kind, in the order human, service_account, bot.
    pub const ALL: &'static [UserKind] = &[Self::Human, Self::ServiceAccount, Self::Bot];

    /// The database / audit string.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Human => "human",
            Self::ServiceAccount => "service_account",
            Self::Bot => "bot",
        }
    }

    /// The kind named by `raw` (exact match), `None` for anything else.
    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|k| k.as_str() == raw)
    }
}

/// Who manages an account's administrator flag (`forge.users.admin_source`,
/// ADR-0021 amendment 2026-11-24).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AdminSource {
    /// forge's own paths (`bootstrap-admin`, `admin user admin-source`,
    /// the restore's regrant): the identity provider's role never clears it
    /// (break-glass). Also every user who is not an administrator.
    Cli,
    /// Granted by `auth.oidc.admin_role`; the next sign-in or refresh check
    /// without the role clears it. Never on a personal access token.
    Oidc,
}

impl AdminSource {
    pub const ALL: &'static [AdminSource] = &[Self::Cli, Self::Oidc];

    /// The database / audit string.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cli => "cli",
            Self::Oidc => "oidc",
        }
    }

    /// The source named by `raw` (exact match), `None` for anything else.
    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|k| k.as_str() == raw)
    }
}
