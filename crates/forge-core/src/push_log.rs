//! The `forge.git.push_applied` log line (M1 phase 9 MR2): what forge-web
//! (MR4) logs once git has updated the refs, **before** it records the push,
//! and what `forge admin repo record-push --from-log` replays when the
//! record could not be written (`FG-GIT-0012`, runbook
//! `push-record-failed`). Both sides use this one type, so the replay reads
//! exactly what was logged.
//!
//! The line is an ordinary JSON log event (`forge_core::telemetry`) with
//! `event = "forge.git.push_applied"`, `request_id`, `repo_id`,
//! `project_id`, `user_id`, `refs` and `push`: the [`PushApplied`] JSON as
//! a string. It holds ids, ref names and object ids only — no IP, no user
//! agent, no names or emails. Version [`PUSH_APPLIED_VERSION`]; a reader
//! refuses other versions and unknown fields.

use forge_errors::{ErrorCode, ForgeError};
use serde::{Deserialize, Serialize};

use crate::push::{ObjectId, PushRecord, PushUpdateKind, PushedRef, Pusher};
use crate::{ProjectId, RefName, RepoId, RequestId, UserId};

/// The `event` field of the line.
pub const PUSH_APPLIED_EVENT: &str = "forge.git.push_applied";
/// The version of the `push` JSON.
pub const PUSH_APPLIED_VERSION: u32 = 1;

/// One updated ref as logged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PushAppliedRef {
    #[serde(rename = "ref")]
    pub name: String,
    pub old: String,
    pub new: String,
    /// `create` / `fast_forward` / `force` / `delete`.
    pub kind: String,
    pub commits: Vec<String>,
    pub total_commits: u64,
    /// `total_commits` is a lower bound (counting timed out, MR4). Absent
    /// (and written only when `true`) so lines of the first MR2 layout
    /// still read.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub total_commits_partial: bool,
}

/// The `push` field of the line: everything `record_push` needs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PushApplied {
    pub v: u32,
    pub request_id: String,
    pub user_id: i64,
    pub credential_id: Option<i64>,
    pub project_id: i64,
    pub repository_id: i64,
    pub updates: Vec<PushAppliedRef>,
}

/// `FG-GIT-0012` for a log line that cannot be replayed (`field` names
/// what is wrong; the value is not quoted).
fn unreadable(field: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::PushRecordFailed)
        .with_context("reason", "push_log_invalid")
        .with_context("field", field)
}

fn positive(raw: i64, field: &'static str) -> Result<i64, ForgeError> {
    if raw > 0 {
        Ok(raw)
    } else {
        Err(unreadable(field))
    }
}

impl PushApplied {
    /// The line of `push` by `pusher` to a repository of `project`.
    pub fn new(pusher: &Pusher, project: ProjectId, push: &PushRecord) -> Self {
        Self {
            v: PUSH_APPLIED_VERSION,
            request_id: pusher.request_id.as_str().to_owned(),
            user_id: pusher.user.get(),
            credential_id: pusher.credential_id,
            project_id: project.get(),
            repository_id: push.repository_id.get(),
            updates: push
                .updates
                .iter()
                .map(|u| PushAppliedRef {
                    name: u.name.as_str().to_owned(),
                    old: u.old.as_str().to_owned(),
                    new: u.new.as_str().to_owned(),
                    kind: u.kind.as_str().to_owned(),
                    commits: u.commits.iter().map(|c| c.as_str().to_owned()).collect(),
                    total_commits: u.total_commits,
                    total_commits_partial: u.total_commits_partial,
                })
                .collect(),
        }
    }

    /// The `push` JSON (`FG-SYS-0001` if serde fails, which these plain
    /// types never do).
    pub fn to_json(&self) -> Result<String, ForgeError> {
        serde_json::to_string(self).map_err(|e| {
            ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "push_log_serialize")
                .with_source(e)
        })
    }

    /// Log the line (info) as the final record of the push.
    pub fn log(&self) -> Result<(), ForgeError> {
        self.log_as("final", "report")
    }

    /// Log the line (info) with `stage` and `source` fields (MR4): the push
    /// path logs a `minimal` line (kinds from the zero sides, no commits)
    /// as soon as git exits, then the `final` one with kinds and commits,
    /// before `record_push`. `source` says how the applied refs were known
    /// (`report`, or a reconciliation such as `report_unreadable`). A
    /// replay uses the `final` line of a request id when there is one
    /// (runbook push-record-failed); both have the same dedup key.
    pub fn log_as(&self, stage: &'static str, source: &'static str) -> Result<(), ForgeError> {
        let push = self.to_json()?;
        tracing::info!(
            event = PUSH_APPLIED_EVENT,
            request_id = %self.request_id,
            repo_id = self.repository_id,
            project_id = self.project_id,
            user_id = self.user_id,
            refs = self.updates.len(),
            stage,
            source,
            push = %push,
            "push applied; recording it"
        );
        Ok(())
    }

    /// The `push` JSON of one log line, or `None` when the line is not a
    /// `forge.git.push_applied` event (another event, or not JSON: a log
    /// excerpt may hold anything). The fields are read at the top level
    /// (`flatten_event`, production) or under `fields`. A push line whose
    /// `push` is missing, not valid JSON of this version, or names another
    /// request id than the line is `FG-GIT-0012 reason=push_log_invalid`.
    pub fn from_log_line(line: &str) -> Result<Option<Self>, ForgeError> {
        let Ok(serde_json::Value::Object(map)) = serde_json::from_str(line.trim()) else {
            return Ok(None);
        };
        let fields = match map.get("fields") {
            Some(serde_json::Value::Object(fields)) => fields,
            Some(_) | None => &map,
        };
        if fields.get("event").and_then(serde_json::Value::as_str) != Some(PUSH_APPLIED_EVENT) {
            return Ok(None);
        }
        let push = fields
            .get("push")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| unreadable("push"))?;
        let applied: Self = serde_json::from_str(push).map_err(|_| unreadable("push_json"))?;
        if applied.v != PUSH_APPLIED_VERSION {
            return Err(unreadable("v").with_context("v", applied.v));
        }
        let line_request = fields.get("request_id").and_then(serde_json::Value::as_str);
        if line_request != Some(applied.request_id.as_str()) {
            return Err(unreadable("request_id"));
        }
        Ok(Some(applied))
    }

    /// The pusher, project and record of this line, checked
    /// ([`PushRecord::check`]). IP and user agent are not logged, so a
    /// replayed record has none.
    pub fn to_parts(&self) -> Result<(Pusher, ProjectId, PushRecord), ForgeError> {
        let request_id =
            RequestId::parse(&self.request_id).ok_or_else(|| unreadable("request_id"))?;
        let user = UserId::new(positive(self.user_id, "user_id")?);
        let credential_id = match self.credential_id {
            Some(id) => Some(positive(id, "credential_id")?),
            None => None,
        };
        let project = ProjectId::new(positive(self.project_id, "project_id")?);
        let repository_id = RepoId::new(positive(self.repository_id, "repository_id")?);
        let updates = self
            .updates
            .iter()
            .map(|u| {
                Ok(PushedRef {
                    name: RefName::parse(&u.name).map_err(|_| unreadable("ref"))?,
                    old: ObjectId::parse(&u.old).map_err(|_| unreadable("old"))?,
                    new: ObjectId::parse(&u.new).map_err(|_| unreadable("new"))?,
                    kind: PushUpdateKind::parse(&u.kind).ok_or_else(|| unreadable("kind"))?,
                    commits: u
                        .commits
                        .iter()
                        .map(|c| ObjectId::parse(c).map_err(|_| unreadable("commits")))
                        .collect::<Result<_, _>>()?,
                    total_commits: u.total_commits,
                    total_commits_partial: u.total_commits_partial,
                })
            })
            .collect::<Result<Vec<_>, ForgeError>>()?;
        let record = PushRecord {
            repository_id,
            updates,
        };
        record.check()?;
        Ok((
            Pusher {
                request_id,
                user,
                credential_id,
                source_ip: None,
                user_agent: None,
            },
            project,
            record,
        ))
    }
}
