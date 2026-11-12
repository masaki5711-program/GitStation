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

/// A secret released with a claim (M4 C6): the container's environment
/// variable `name`. Never printed: `Debug` shows the name only, the value
/// is wiped on drop.
#[derive(Clone, PartialEq, Eq, Deserialize)]
pub struct JobSecret {
    pub name: String,
    pub value: JobToken,
    /// Mask it (and its common encodings) in the job's log.
    pub masked: bool,
}

impl std::fmt::Debug for JobSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JobSecret")
            .field("name", &self.name)
            .field("masked", &self.masked)
            .finish_non_exhaustive()
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
    /// The values of `secret_names` (M4 C6); empty for a job without.
    #[serde(default)]
    pub secrets: Vec<JobSecret>,
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

/// One artifact of a job this job needs (M4 C6): what
/// `GET …/needs/artifacts` answered.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct NeededArtifact {
    pub id: i64,
    /// The job that uploaded it.
    pub job: String,
    pub path: String,
    pub size_bytes: i64,
    pub sha256: String,
}

#[derive(Deserialize)]
struct NeededList {
    artifacts: Vec<NeededArtifact>,
}

fn unsupported() -> ClientError {
    ClientError::local(
        forge_errors::ErrorCode::CliUsageInvalid,
        "reason=artifacts_unsupported",
    )
}

/// How long one artifact transfer may take.
pub const ARTIFACT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1800);

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
    /// M4 C6: the artifacts of the jobs `job` needs, with the job's token.
    fn needed_artifacts(
        &self,
        _job: i64,
        _token: &JobToken,
        _timeout: std::time::Duration,
    ) -> Result<Vec<NeededArtifact>, ClientError> {
        Err(unsupported())
    }
    /// M4 C6: one of them into `out` (at most `max` bytes); the bytes.
    fn download_artifact(
        &self,
        _job: i64,
        _token: &JobToken,
        _artifact: i64,
        _out: &mut dyn std::io::Write,
        _max: u64,
        _timeout: std::time::Duration,
    ) -> Result<u64, ClientError> {
        Err(unsupported())
    }
    /// M4 C6: upload the opened `file` (`len` bytes) as the job's artifact
    /// `path`, within `timeout`.
    fn upload_artifact(
        &self,
        _job: i64,
        _token: &JobToken,
        _path: &str,
        _file: std::fs::File,
        _len: u64,
        _timeout: std::time::Duration,
    ) -> Result<(), ClientError> {
        Err(unsupported())
    }
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

    fn needed_artifacts(
        &self,
        job: i64,
        token: &JobToken,
        timeout: std::time::Duration,
    ) -> Result<Vec<NeededArtifact>, ClientError> {
        let mut out = Vec::new();
        self.client.download_to(
            &job_path(job, "needs/artifacts"),
            Some(token.expose()),
            &mut out,
            4 * 1024 * 1024,
            timeout.min(ARTIFACT_TIMEOUT),
        )?;
        serde_json::from_slice::<NeededList>(&out)
            .map(|l| l.artifacts)
            .map_err(|_| malformed("needed_artifacts"))
    }

    fn download_artifact(
        &self,
        job: i64,
        token: &JobToken,
        artifact: i64,
        out: &mut dyn std::io::Write,
        max: u64,
        timeout: std::time::Duration,
    ) -> Result<u64, ClientError> {
        self.client.download_to(
            &job_path(job, &format!("needs/artifacts/{artifact}")),
            Some(token.expose()),
            out,
            max,
            timeout.min(ARTIFACT_TIMEOUT),
        )
    }

    fn upload_artifact(
        &self,
        job: i64,
        token: &JobToken,
        path: &str,
        file: std::fs::File,
        len: u64,
        timeout: std::time::Duration,
    ) -> Result<(), ClientError> {
        self.client
            .upload_file(
                &job_path(job, "artifacts"),
                &[("path".to_owned(), path.to_owned())],
                token.expose(),
                file,
                len,
                timeout.min(ARTIFACT_TIMEOUT),
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
            "secret_names": ["DEPLOY_TOKEN"], "lease_until": "x",
            "secrets": [{"name": "DEPLOY_TOKEN", "value": "s3cr3t-value", "masked": true}],
            "job_token": "fgj_0123_secret", "job_token_expires_at": "y"
        }});
        let claim: ClaimAnswer = serde_json::from_value(answer).expect("parses");
        let job = claim.job.expect("a job");
        assert_eq!(job.job_id, 7);
        assert_eq!(job.job_token.expose(), "fgj_0123_secret");
        assert_eq!(job.secrets[0].value.expose(), "s3cr3t-value");
        let shown = format!("{job:?}");
        assert!(!shown.contains("fgj_0123_secret"), "{shown}");
        assert!(!shown.contains("s3cr3t"), "{shown}");
    }
}
