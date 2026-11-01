//! The client's one error type and its exit codes (plan 01 §3.11 C-3).
//!
//! A failure is `{error_code, user_message, request_id, retryable}`: the code
//! is a registered `FG-…` code (client-side `FG-CLI-…`, or the server's code
//! copied from its error body), the message is the registered `user_message`
//! (never a path, token, or server internals), the request id is the server's.
//! `detail` (a reason, a host, a config key) is for the human line on stderr.

use forge_errors::{ErrorCode, Lang};
use serde::Serialize;

/// Stable process exit codes (C-3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    Ok = 0,
    /// Any failure no other code describes.
    Other = 1,
    /// Wrong usage or configuration.
    Usage = 2,
    /// Not authenticated, or not allowed.
    Auth = 3,
    NotFound = 4,
    Conflict = 5,
    /// Temporary failure: retrying may help.
    Retryable = 6,
}

impl Exit {
    pub fn code(self) -> i32 {
        self as i32
    }
}

#[derive(Debug, Clone)]
pub struct ClientError {
    code_id: String,
    user_message: String,
    request_id: Option<String>,
    retryable: bool,
    exit: Exit,
    detail: Option<String>,
}

/// The JSON written to stderr on failure with `--json` (C-3): the four
/// contract keys plus `detail` (a `reason=…` diagnosis line, or null). The key
/// set is a contract (`json_error_shape_is_stable`).
#[derive(Serialize)]
pub struct ErrorJson<'a> {
    pub error_code: &'a str,
    pub user_message: &'a str,
    pub request_id: Option<&'a str>,
    pub retryable: bool,
    pub detail: Option<&'a str>,
}

impl ClientError {
    /// A client-side failure with its registered message.
    pub fn local(code: ErrorCode, detail: impl Into<String>) -> Self {
        let exit = if matches!(
            code,
            ErrorCode::CliUsageInvalid | ErrorCode::CliConfigInvalid
        ) {
            Exit::Usage
        } else if code == ErrorCode::CliNotLoggedIn {
            Exit::Auth
        } else if code == ErrorCode::CliServerUnreachable {
            Exit::Retryable
        } else {
            Exit::Other
        };
        let detail = detail.into();
        Self {
            code_id: code.id().to_string(),
            user_message: code.user_message(Lang::Ja).to_string(),
            request_id: None,
            retryable: code == ErrorCode::CliServerUnreachable,
            exit,
            detail: (!detail.is_empty()).then_some(detail),
        }
    }

    /// A failure the server reported: map its registered code (or, for a code
    /// this client does not know yet, the HTTP status) to an exit code.
    pub fn from_server(
        status: u16,
        code_id: &str,
        user_message: &str,
        request_id: Option<&str>,
    ) -> Self {
        // The registry's class of the code wins; an unknown code (a newer
        // server) falls back to what the response itself says.
        let class = ErrorCode::parse(code_id).map_or(status, ErrorCode::http_status);
        let exit = match class {
            401 | 403 => Exit::Auth,
            404 | 410 => Exit::NotFound,
            409 => Exit::Conflict,
            400 | 405 | 413 | 415 | 422 => Exit::Usage,
            429 | 502 | 503 | 504 => Exit::Retryable,
            _ => Exit::Other,
        };
        Self {
            code_id: code_id.to_string(),
            user_message: user_message.to_string(),
            request_id: request_id.map(str::to_string),
            retryable: exit == Exit::Retryable,
            exit,
            detail: None,
        }
    }

    pub fn code_id(&self) -> &str {
        &self.code_id
    }

    pub fn user_message(&self) -> &str {
        &self.user_message
    }

    pub fn request_id(&self) -> Option<&str> {
        self.request_id.as_deref()
    }

    pub fn retryable(&self) -> bool {
        self.retryable
    }

    pub fn exit(&self) -> Exit {
        self.exit
    }

    pub fn detail(&self) -> Option<&str> {
        self.detail.as_deref()
    }

    pub fn with_request_id(mut self, id: Option<String>) -> Self {
        self.request_id = id;
        self
    }

    pub fn as_json(&self) -> ErrorJson<'_> {
        ErrorJson {
            error_code: &self.code_id,
            user_message: &self.user_message,
            request_id: self.request_id.as_deref(),
            retryable: self.retryable,
            detail: self.detail.as_deref(),
        }
    }

    /// The human line: message, code, request id, and the detail when present.
    pub fn human(&self) -> String {
        let mut s = format!("error: {} [{}", self.user_message, self.code_id);
        if let Some(r) = &self.request_id {
            s.push_str(&format!(" request_id={r}"));
        }
        s.push(']');
        if let Some(d) = &self.detail {
            s.push_str(&format!("\n  {d}"));
        }
        s
    }
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.human())
    }
}

impl std::error::Error for ClientError {}

pub type Result<T> = std::result::Result<T, ClientError>;
