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
