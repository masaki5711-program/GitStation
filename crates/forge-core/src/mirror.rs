//! The push mirror to GitLab (M3 O2, plan 06 D-2 / D-4 §4.11, ADR-0024
//! Amendment O2): its destination policy, its credential name and the audit
//! shape of its changes.
//!
//! GitStation is the canonical copy; GitLab is the insurance. A mirror
//! sends `refs/heads/*` and `refs/tags/*` (never `refs/merge-requests/*`)
//! with `--prune` after every push. Its destination is set by an
//! administrator and must pass [`MirrorPolicy`]:
//!
//! - an absolute `https` URL (`http` only with `mirror.allow_http`, tests)
//!   of at most 2048 bytes, without user information, query or fragment —
//!   the token never goes in the URL (it is read from a file);
//! - a host and port listed in `mirror.allowed_hosts` (empty by default:
//!   nothing can be mirrored until the operator names the GitLab host).
//!
//! Every refusal is `FG-INT-0021` with `reason` and never the URL.

use forge_errors::{ErrorCode, ForgeError};

use crate::config::Secret;
use crate::ids::{ProjectId, RepoId};

/// The longest destination URL accepted.
pub const MAX_URL_BYTES: usize = 2048;
/// The longest credential name.
pub const MAX_CREDENTIAL_NAME: usize = 64;
/// `mirror.allowed_hosts`.
pub const ALLOWED_HOSTS_KEY: &str = "mirror.allowed_hosts";

fn invalid(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::PushMirrorTargetInvalid).with_context("reason", reason)
}

/// One `mirror.allowed_hosts` entry: a lowercase host name or address
/// literal and a port (443 when none is written).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct AllowedHost {
    host: String,
    port: u16,
}

impl AllowedHost {
    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn port(&self) -> u16 {
        self.port
    }
}

/// `mirror.allowed_hosts` and `mirror.allow_http`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MirrorPolicy {
    hosts: Vec<AllowedHost>,
    allow_http: bool,
}

impl MirrorPolicy {
    /// Parse the list: entries `host` or `host:port` (`[v6]:port` for an IPv6
    /// literal) separated by commas or white space. A bad entry is
    /// `FG-SYS-0002 config_key=mirror.allowed_hosts reason=invalid` with the
    /// entry's position, never a silent skip.
    pub fn parse(list: &str, allow_http: bool) -> Result<Self, ForgeError> {
        let mut hosts = Vec::new();
        for (i, entry) in list
            .split(|c: char| c == ',' || c.is_whitespace())
            .filter(|e| !e.is_empty())
            .enumerate()
        {
            let bad = || {
                ForgeError::new(ErrorCode::ConfigInvalid)
                    .with_context("config_key", ALLOWED_HOSTS_KEY)
                    .with_context("reason", "invalid")
                    .with_context("entry", i + 1)
            };
            let probe = format!("https://{entry}/");
            let url = url::Url::parse(&probe).map_err(|_| bad())?;
            if !url.username().is_empty()
                || url.password().is_some()
                || url.path() != "/"
                || url.query().is_some()
            {
                return Err(bad());
            }
            let host = match url.host() {
                Some(url::Host::Domain(d)) => d.to_ascii_lowercase(),
                Some(url::Host::Ipv4(a)) => a.to_string(),
                Some(url::Host::Ipv6(a)) => format!("[{a}]"),
                None => return Err(bad()),
            };
            let port = url.port().unwrap_or(443);
            hosts.push(AllowedHost { host, port });
        }
        hosts.sort();
        hosts.dedup();
        Ok(Self { hosts, allow_http })
    }

    pub fn hosts(&self) -> &[AllowedHost] {
        &self.hosts
    }

    pub fn allow_http(&self) -> bool {
        self.allow_http
    }

    /// Whether `url`'s host and port are listed.
    pub fn admits(&self, url: &MirrorUrl) -> bool {
        self.hosts
            .iter()
            .any(|h| h.host == url.host && h.port == url.port)
    }

    /// Parse `raw` and check it against this policy (module docs).
    pub fn check(&self, raw: &str) -> Result<MirrorUrl, ForgeError> {
        let url = MirrorUrl::parse(raw, self.allow_http)?;
        if !self.admits(&url) {
            return Err(invalid("not_allowlisted").with_context("origin", url.origin()));
        }
        Ok(url)
    }
}

/// A destination that passed the shape rules ([`MirrorPolicy::check`] also
/// applies the allowlist).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MirrorUrl {
    url: url::Url,
    host: String,
    port: u16,
}

impl MirrorUrl {
    /// The shape rules only (module docs): `FG-INT-0021` with `reason`
    /// (`invalid_url`, `scheme`, `userinfo`, `query_or_fragment`).
    pub fn parse(raw: &str, allow_http: bool) -> Result<Self, ForgeError> {
        if raw.is_empty()
            || raw.len() > MAX_URL_BYTES
            || raw
                .chars()
                .any(|c| c.is_whitespace() || c.is_control() || c == '\\')
        {
            return Err(invalid("invalid_url"));
        }
        let lower = raw.get(..8).unwrap_or(raw).to_ascii_lowercase();
        let after = if lower.starts_with("https://") {
            raw.get(8..)
        } else if lower.starts_with("http://") {
            raw.get(7..)
        } else {
            return Err(if raw.contains("://") {
                invalid("scheme")
            } else {
                invalid("invalid_url")
            });
        };
        if after.is_none_or(|rest| rest.is_empty() || rest.starts_with('/')) {
            return Err(invalid("invalid_url"));
        }
        let url = url::Url::parse(raw).map_err(|_| invalid("invalid_url"))?;
        match url.scheme() {
            "https" => {}
            "http" if allow_http => {}
            _ => return Err(invalid("scheme")),
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(invalid("userinfo"));
        }
        if url.query().is_some() || url.fragment().is_some() {
            return Err(invalid("query_or_fragment"));
        }
        let host = match url.host() {
            Some(url::Host::Domain(d)) => d.to_ascii_lowercase(),
            Some(url::Host::Ipv4(a)) => a.to_string(),
            Some(url::Host::Ipv6(a)) => format!("[{a}]"),
            None => return Err(invalid("invalid_url")),
        };
        let port = url
            .port_or_known_default()
            .ok_or_else(|| invalid("invalid_url"))?;
        // The stored form must fit the table's CHECK too.
        if url.as_str().len() > MAX_URL_BYTES {
            return Err(invalid("invalid_url"));
        }
        Ok(Self { url, host, port })
    }

    /// The URL as stored and pushed to (normalized).
    pub fn as_str(&self) -> &str {
        self.url.as_str()
    }

    pub fn is_https(&self) -> bool {
        self.url.scheme() == "https"
    }

    /// `scheme://host:port` — what logs, audit rows and the read pool show.
    pub fn origin(&self) -> String {
        format!("{}://{}:{}", self.url.scheme(), self.host, self.port)
    }
}

/// The name of a credential file in `mirror.credentials_dir`: 1 to 64 of
/// `[a-z0-9._-]`, starting with a letter or a digit (so never `.`, `..` or
/// a path). `FG-INT-0021 reason=credential_name` otherwise.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CredentialName(String);

impl CredentialName {
    pub fn parse(raw: &str) -> Result<Self, ForgeError> {
        let mut bytes = raw.bytes();
        let ok = raw.len() <= MAX_CREDENTIAL_NAME
            && bytes
                .next()
                .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
            && bytes.all(|b| {
                b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-')
            });
        if ok {
            Ok(Self(raw.to_owned()))
        } else {
            Err(invalid("credential_name"))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for CredentialName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The most bytes of a credential file.
pub const MAX_CREDENTIAL_BYTES: usize = 4096;

/// A mirror's credential, read from its file in `mirror.credentials_dir`:
/// one line `<username>:<token>` (GitLab: the mirror account and a token
/// with `write_repository`). Debug never shows the token.
pub struct MirrorCredential {
    username: String,
    token: Secret,
}

impl std::fmt::Debug for MirrorCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MirrorCredential")
            .field("username", &self.username)
            .finish_non_exhaustive()
    }
}

impl MirrorCredential {
    /// Parse the file's text (the trailing newline trimmed). `FG-INT-0013
    /// reason=credential_unreadable` with `detail` (`empty`, `no_separator`,
    /// `several_lines`, `control_characters`, `too_large`) — never the text.
    pub fn parse(text: &str) -> Result<Self, ForgeError> {
        let bad = |detail: &'static str| {
            ForgeError::new(ErrorCode::PushMirrorFailed)
                .with_context("reason", "credential_unreadable")
                .with_context("detail", detail)
        };
        if text.len() > MAX_CREDENTIAL_BYTES {
            return Err(bad("too_large"));
        }
        let line = text.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            return Err(bad("empty"));
        }
        if line.contains(['\r', '\n']) {
            return Err(bad("several_lines"));
        }
        if line.chars().any(char::is_control) {
            return Err(bad("control_characters"));
        }
        let (user, token) = line.split_once(':').ok_or_else(|| bad("no_separator"))?;
        if user.is_empty() || token.is_empty() {
            return Err(bad("no_separator"));
        }
        Ok(Self {
            username: user.to_owned(),
            token: Secret::new(token.to_owned()),
        })
    }

    pub fn username(&self) -> &str {
        &self.username
    }

    /// The `Authorization` header's value: `Basic base64(user:token)`.
    pub fn authorization(&self) -> Secret {
        use base64::Engine as _;
        let raw = format!("{}:{}", self.username, self.token.expose());
        Secret::new(format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(raw)
        ))
    }
}

/// An audited change of a push mirror (actions `push_mirror.set` and
/// `push_mirror.removed`, resource the project). No domain event: nothing
/// outside forge acts on it. Never the full URL or a token: the origin and
/// the credential's name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushMirrorChange {
    /// `push_mirror.set`: created (`created: true`) or changed.
    Set {
        project: ProjectId,
        repo: RepoId,
        origin: String,
        credential_name: String,
        enabled: bool,
        created: bool,
    },
    /// `push_mirror.removed` (its status goes with it).
    Removed {
        project: ProjectId,
        repo: RepoId,
        origin: String,
    },
}

impl PushMirrorChange {
    pub fn action(&self) -> &'static str {
        match self {
            Self::Set { .. } => "push_mirror.set",
            Self::Removed { .. } => "push_mirror.removed",
        }
    }

    pub fn to_value(&self) -> serde_json::Value {
        match self {
            Self::Set {
                project,
                repo,
                origin,
                credential_name,
                enabled,
                created,
            } => serde_json::json!({
                "project_id": project.get(),
                "repo_id": repo.get(),
                "origin": origin,
                "credential_name": credential_name,
                "enabled": enabled,
                "created": created,
            }),
            Self::Removed {
                project,
                repo,
                origin,
            } => serde_json::json!({
                "project_id": project.get(),
                "repo_id": repo.get(),
                "origin": origin,
            }),
        }
    }
}

#[cfg(test)]
#[path = "mirror_tests.rs"]
mod tests;
