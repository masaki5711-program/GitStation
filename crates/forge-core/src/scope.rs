//! Token scopes (ADR-0010), shared by forge-db (which stores them with a
//! personal access token, step D3) and forge-auth (which decides with them).
//! forge-auth re-exports `Scope`, so its API is unchanged.

/// What a token may be used for. Scopes only narrow a role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Scope {
    /// git clone / fetch.
    ReadRepository,
    /// git push (implies `ReadRepository`).
    WriteRepository,
    /// REST API reads.
    ReadApi,
    /// REST API reads and writes (implies `ReadApi`).
    Api,
    /// Lets an administrator's token use administrator rights. Without it a
    /// token of an administrator acts as a normal user.
    Admin,
    /// Merge a merge request (M3 R4, ADR-0024 §8.5, ADR-0017 §4.0). A merge
    /// scope is satisfied only by itself: `api` and `admin` do not satisfy
    /// it, and it satisfies nothing else.
    MergeExecute,
}

impl Scope {
    /// Every scope, in declaration order.
    pub const ALL: [Scope; 6] = [
        Scope::ReadRepository,
        Scope::WriteRepository,
        Scope::ReadApi,
        Scope::Api,
        Scope::Admin,
        Scope::MergeExecute,
    ];

    /// The scope named `name` (its `as_str`); `None` for any other string.
    pub fn parse(name: &str) -> Option<Scope> {
        Self::ALL.into_iter().find(|s| s.as_str() == name)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Scope::ReadRepository => "read_repository",
            Scope::WriteRepository => "write_repository",
            Scope::ReadApi => "read_api",
            Scope::Api => "api",
            Scope::Admin => "admin",
            Scope::MergeExecute => "merge_execute",
        }
    }

    /// Whether holding `self` satisfies a requirement of `required`. The
    /// merge scope (`MergeExecute`) is satisfied only by itself and
    /// implies nothing (ADR-0024 §8.5: `api` / `admin` never merge).
    pub fn satisfies(self, required: Scope) -> bool {
        self == required
            || matches!(
                (self, required),
                (Scope::WriteRepository, Scope::ReadRepository) | (Scope::Api, Scope::ReadApi)
            )
    }
}
