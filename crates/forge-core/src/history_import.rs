//! The one-time history import of an empty repository (2026-11-12,
//! ADR-0008 amendment "history import"; runbook push-checks).
//!
//! The push secret check (FG-GIT-0022) scans every object a push brings, so
//! an existing project whose old commits hold (synthetic or real) key
//! material could never be imported. An instance administrator may allow
//! **one** import into an **empty** repository (`forge admin repo
//! allow-import`, operator CLI only, no HTTP route). The first push into
//! that repository while the allowance is open (not used, not expired):
//!
//! - the **tip tree** of every pushed ref is still checked: a secret there
//!   refuses the push with FG-GIT-0022 as always;
//! - a secret only in history (a blob no pushed tip tree holds) does not
//!   refuse: each is an audit row `repository.history_secret_imported`
//!   (rule, path, commit, blob — never the value) and one warn log line.
//!
//! The first accepted push uses the allowance up (`repository.history_imported`).
//! A real secret in the imported history is the importer's to rotate.

use std::time::Duration;

use crate::ids::{ProjectId, RepoId};
use crate::namespace_resolver::LookupFuture;
use crate::push::Pusher;

/// The allowance's default lifetime.
pub const DEFAULT_TTL: Duration = Duration::from_secs(24 * 3600);
/// The longest lifetime an operator may give (7 days).
pub const MAX_TTL: Duration = Duration::from_secs(7 * 24 * 3600);
/// The most history findings written as audit rows for one import (all are
/// counted on `repository.history_imported`; every one is logged).
pub const MAX_AUDITED_FINDINGS: usize = 500;

/// A secret found only in the imported history (never the value).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryFinding {
    pub rule: String,
    pub blob: String,
    pub path: String,
    /// The oldest pushed commit that adds the blob (none when git could not
    /// say, e.g. a blob only a tag names).
    pub commit: Option<String>,
}

/// What an accepted import push brought besides its refs.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HistoryImportUse {
    /// At most [`MAX_AUDITED_FINDINGS`] kept (the caller cuts).
    pub findings: Vec<HistoryFinding>,
    /// Every history finding counted.
    pub total: usize,
}

/// What recording an import did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryImportRecorded {
    /// The allowance was open and is used up now.
    Consumed,
    /// No open allowance was left (another push used it meanwhile); the
    /// audit rows are written all the same.
    AlreadyUsed,
}

/// A change of a repository's history import allowance (audit rows on the
/// repository's project; audit only, no event).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryImportChange {
    /// `repository.history_import_allowed`: an operator allowed one import
    /// until `expires_at` (RFC 3339, UTC).
    Allowed {
        project: ProjectId,
        repository: RepoId,
        expires_at: String,
        ttl_secs: u64,
    },
    /// `repository.history_imported`: the first accepted push used it.
    Imported {
        project: ProjectId,
        repository: RepoId,
        history_findings: usize,
        audited_findings: usize,
        consumed: bool,
    },
    /// `repository.history_secret_imported`: one finding only in history.
    SecretImported {
        project: ProjectId,
        repository: RepoId,
        finding: HistoryFinding,
    },
}

impl HistoryImportChange {
    pub fn action(&self) -> &'static str {
        match self {
            Self::Allowed { .. } => "repository.history_import_allowed",
            Self::Imported { .. } => "repository.history_imported",
            Self::SecretImported { .. } => "repository.history_secret_imported",
        }
    }

    pub fn project(&self) -> ProjectId {
        match self {
            Self::Allowed { project, .. }
            | Self::Imported { project, .. }
            | Self::SecretImported { project, .. } => *project,
        }
    }

    pub fn to_value(&self) -> serde_json::Value {
        match self {
            Self::Allowed {
                project,
                repository,
                expires_at,
                ttl_secs,
            } => serde_json::json!({
                "project_id": project.get(),
                "repository_id": repository.get(),
                "expires_at": expires_at,
                "ttl_secs": ttl_secs,
            }),
            Self::Imported {
                project,
                repository,
                history_findings,
                audited_findings,
                consumed,
            } => serde_json::json!({
                "project_id": project.get(),
                "repository_id": repository.get(),
                "history_findings": history_findings,
                "audited_findings": audited_findings,
                "consumed": consumed,
            }),
            Self::SecretImported {
                project,
                repository,
                finding,
            } => serde_json::json!({
                "project_id": project.get(),
                "repository_id": repository.get(),
                "severity": "warning",
                "error_code": "FG-GIT-0022",
                "rule": finding.rule,
                "path": finding.path,
                "blob": finding.blob,
                "commit": finding.commit,
            }),
        }
    }
}

/// The allowance as the push path sees it (forge-db implements it on `Db`;
/// `forge serve` injects it into forge-web).
pub trait HistoryImports: Send + Sync {
    /// Whether `repo` has an open allowance (not used, not expired).
    fn import_allowed<'a>(&'a self, repo: RepoId) -> LookupFuture<'a, bool>;

    /// After an accepted push into `repo` under the allowance: use it up
    /// and write the audit rows, in one transaction.
    fn record_import<'a>(
        &'a self,
        pusher: &'a Pusher,
        repo: RepoId,
        used: &'a HistoryImportUse,
    ) -> LookupFuture<'a, HistoryImportRecorded>;
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::*;

    #[test]
    fn a_history_finding_is_audited_without_any_value() {
        let change = HistoryImportChange::SecretImported {
            project: ProjectId::new(3),
            repository: RepoId::new(4),
            finding: HistoryFinding {
                rule: "private_key".to_owned(),
                blob: "a".repeat(40),
                path: "deploy/id_rsa".to_owned(),
                commit: None,
            },
        };
        assert_eq!(change.action(), "repository.history_secret_imported");
        let v = change.to_value();
        assert_eq!(v["rule"], "private_key");
        assert_eq!(v["path"], "deploy/id_rsa");
        assert_eq!(v["severity"], "warning");
        assert!(v["commit"].is_null());
        let keys: Vec<&String> = v.as_object().expect("object").keys().collect();
        assert_eq!(keys.len(), 8, "{keys:?}");
    }
}
