//! A project's protected ref rules as one snapshot (M1 phase 9 MR2,
//! ADR-0008 Amendment D4 MR5): a push loads them once
//! ([`ProtectionLoader::load_protection`]; forge-db: `protected_refs::rules_for`)
//! and decides every ref of the push against the same rules, so a rule
//! changed in between cannot apply to some refs and not to others.
//! [`ProtectionRules::levels`] answers exactly what forge-db's
//! `protected_refs::levels_for` answers for one ref (property test
//! `rules_for_matches_levels_for`); feed it to
//! `forge_auth::RefProtection::effective`, which takes the strictest.

use crate::namespace_resolver::LookupFuture;
use crate::{ProjectId, RefName, RefPattern, Role};

/// One stored rule, parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtectionRule {
    pub pattern: RefPattern,
    pub push_min_role: Role,
}

/// Every rule of one project, in rule id order.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProtectionRules {
    rules: Vec<ProtectionRule>,
}

impl ProtectionRules {
    pub fn new(rules: Vec<ProtectionRule>) -> Self {
        Self { rules }
    }

    /// The minimum push roles of **every** rule whose pattern matches
    /// `name` (ignoring case, `RefPattern::matches`), in rule id order;
    /// empty = the ref is not protected.
    pub fn levels(&self, name: &RefName) -> Vec<Role> {
        self.rules
            .iter()
            .filter(|rule| rule.pattern.matches(name))
            .map(|rule| rule.push_min_role)
            .collect()
    }

    pub fn rules(&self) -> &[ProtectionRule] {
        &self.rules
    }

    pub fn len(&self) -> usize {
        self.rules.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }
}

/// Loads a project's protection snapshot (forge-db implements it on `Db`).
pub trait ProtectionLoader: Send + Sync {
    /// All rules of `project` (none for an unknown project: the caller has
    /// loaded the project already). One bounded statement; more than 100
    /// stored rules or a row this build cannot parse is `FG-DB-0020`
    /// ProtectedRefDataInvalid — the push is refused, never decided on part
    /// of the rules. The caller bounds the call with its own timeout.
    fn load_protection<'a>(&'a self, project: ProjectId) -> LookupFuture<'a, ProtectionRules>;
}
