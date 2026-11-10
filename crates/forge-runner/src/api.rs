//! The runner API (M4 C3 routes, `forge-web/src/routes/runners.rs`) as the
//! runner calls it: claim (long-poll), heartbeat, log, complete. A trait so
//! the job tests run without a server; [`HttpApi`] is the real one, on
//! `forge_client`'s HTTP client (Bearer token, no redirects, timeouts, the
//! token sent only to the configured host).

use forge_client::error::ClientError;
use forge_client::http::{ApiClient, Method};
use serde::Deserialize;
use serde_json::{json, Value};

/// A job's token `fgj_…` (shown once by the claim). Never printed:
/// `Debug` is redacted, the bytes are wiped on drop.
#[derive(Clone, PartialEq, Eq)]
pub struct JobToken(zeroize::Zeroizing<String>);

impl JobToken {
    pub fn new(raw: String) -> Self {
        Self(zeroize::Zeroizing::new(raw))
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for JobToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("JobToken(<redacted>)")
    }
}

impl<'de> Deserialize<'de> for JobToken {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d).map(Self::new)
    }
}

/// A claimed job: the answer of `POST /api/v1/runner/jobs/request`.
#[derive(Debug, Clone, Deserialize)]
pub struct ClaimedJob {
    pub job_id: i64,
    pub attempt: i32,
    pub name: String,
    pub stage: String,
    pub image: String,
    pub timeout_s: i32,
    pub pipeline_id: i64,
    pub project_id: i64,
    #[serde(rename = "ref")]
    pub git_ref: String,
    pub sha: String,
    pub definition_hash: Option<String>,
    pub secrets_allowed: bool,
    #[serde(default)]
    pub secret_names: Vec<String>,
    pub job_token: JobToken,
}

#[derive(Deserialize)]
struct ClaimAnswer {
    job: Option<ClaimedJob>,
}

/// What a heartbeat answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Beat {
    Running,
    /// Stop the job, then complete it.
    Canceled,
}

/// A job's reported result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobResult {
    Succeeded,
    Failed,
}

impl JobResult {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
        }
    }
}

/// The runner's calls on forge.
pub trait RunnerApi: Send + Sync {
    fn request_job(&self, wait_s: u64) -> Result<Option<ClaimedJob>, ClientError>;
    fn heartbeat(&self, job: i64, attempt: i32) -> Result<Beat, ClientError>;
    fn append_log(&self, job: i64, attempt: i32, seq: i64, text: &str) -> Result<(), ClientError>;
    fn complete(
        &self,
        job: i64,
        attempt: i32,
        result: JobResult,
        exit_code: Option<i32>,
    ) -> Result<(), ClientError>;
}

/// [`RunnerApi`] over HTTP with the runner's token.
pub struct HttpApi {
    client: ApiClient,
}

impl HttpApi {
    pub fn new(client: ApiClient) -> Self {
        Self { client }
    }

    fn post(&self, path: &str, body: &Value) -> Result<Value, ClientError> {
        self.client
            .request(Method::Post, path, &[], Some(body))
            .map(|r| r.body)
    }
}

fn job_path(job: i64, tail: &str) -> String {
    format!("/api/v1/runner/jobs/{job}/{tail}")
}

fn malformed(what: &str) -> ClientError {
    ClientError::local(
        forge_errors::ErrorCode::CliResponseInvalid,
        format!("reason=runner_answer_malformed what={what}"),
    )
}

impl RunnerApi for HttpApi {
    fn request_job(&self, wait_s: u64) -> Result<Option<ClaimedJob>, ClientError> {
        let body = self.post("/api/v1/runner/jobs/request", &json!({ "wait_s": wait_s }))?;
        serde_json::from_value::<ClaimAnswer>(body)
            .map(|a| a.job)
            .map_err(|_| malformed("claim"))
    }

    fn heartbeat(&self, job: i64, attempt: i32) -> Result<Beat, ClientError> {
        let body = self.post(&job_path(job, "heartbeat"), &json!({ "attempt": attempt }))?;
        match body.get("state").and_then(Value::as_str) {
            Some("running") => Ok(Beat::Running),
            Some("canceled") => Ok(Beat::Canceled),
            _ => Err(malformed("heartbeat")),
        }
    }

    fn append_log(&self, job: i64, attempt: i32, seq: i64, text: &str) -> Result<(), ClientError> {
        self.post(
            &job_path(job, "log"),
            &json!({ "attempt": attempt, "seq": seq, "text": text }),
        )
        .map(|_| ())
    }

    fn complete(
        &self,
        job: i64,
        attempt: i32,
        result: JobResult,
        exit_code: Option<i32>,
    ) -> Result<(), ClientError> {
        self.post(
            &job_path(job, "complete"),
            &json!({ "attempt": attempt, "result": result.as_str(), "exit_code": exit_code }),
        )
        .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)] // tests may fail loudly

    use super::*;

    #[test]
    fn a_claim_parses_and_its_token_is_never_printed() {
        let answer = json!({"job": {
            "job_id": 7, "attempt": 1, "name": "build", "stage": "build",
            "image": format!("registry.internal/rust@sha256:{}", "a".repeat(64)),
            "timeout_s": 600, "tags": [], "pipeline_id": 3, "project_id": 2,
            "ref": "refs/heads/main", "sha": "b".repeat(40),
            "definition_hash": "c".repeat(64), "secrets_allowed": false,
            "secret_names": [], "lease_until": "x",
            "job_token": "fgj_0123_secret", "job_token_expires_at": "y"
        }});
        let claim: ClaimAnswer = serde_json::from_value(answer).expect("parses");
        let job = claim.job.expect("a job");
        assert_eq!(job.job_id, 7);
        assert_eq!(job.job_token.expose(), "fgj_0123_secret");
        assert!(!format!("{job:?}").contains("fgj_0123_secret"));
    }
}
