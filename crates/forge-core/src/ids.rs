//! Typed database ids. A `RepoId` can never be passed where a `UserId` is
//! expected, and storage paths are derived only from `RepoId` (ADR-0009).

use std::fmt;

macro_rules! id_type {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(i64);

        impl $name {
            pub const fn new(raw: i64) -> Self {
                Self(raw)
            }

            pub const fn get(self) -> i64 {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }
    };
}

id_type!(
    /// A repository (bare git directory under the storage root).
    RepoId
);
id_type!(
    /// A user account.
    UserId
);
id_type!(
    /// A group (namespace that can nest).
    GroupId
);
id_type!(
    /// A project (owns one main repository).
    ProjectId
);

id_type!(
    /// A row of forge.namespaces (the path a user, group or project owns).
    NamespaceId
);
id_type!(
    /// A row of forge.namespace_tombstones (a released-until path block).
    TombstoneId
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_display_as_plain_numbers() {
        assert_eq!(RepoId::new(42).to_string(), "42");
        assert_eq!(UserId::new(7).get(), 7);
    }
}
