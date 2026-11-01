//! The git credential helper protocol (gitcredentials(7)) for ONE host.
//!
//! git runs `gstn … auth git-credential get|store|erase` and writes
//! `key=value` lines ending with an empty line. We answer only when
//! protocol and host (with port) are exactly the configured server's;
//! for any other host nothing is printed and the exit code is 0, so git asks
//! the next helper. The token appears in the answer to git and nowhere else.

use crate::config::Config;
use crate::store::{Token, TokenStore};
use std::collections::BTreeMap;

pub type Request = BTreeMap<String, String>;

/// Parse the request lines. `url=` is expanded (git sends it in some flows).
pub fn parse_request(input: &str) -> Request {
    let mut m = Request::new();
    for line in input.lines() {
        if line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once('=') {
            m.insert(k.to_string(), v.to_string());
        }
    }
    if let Some(u) = m.get("url").and_then(|u| url::Url::parse(u).ok()) {
        m.insert("protocol".to_string(), u.scheme().to_string());
        if let Some(h) = u.host_str() {
            let host = match u.port() {
                Some(p) => format!("{h}:{p}"),
                None => h.to_string(),
            };
            m.insert("host".to_string(), host);
        }
    }
    m
}

/// `host` or `host:port` of the configured server, port only when not the default.
pub fn authority(cfg: &Config) -> String {
    let h = cfg.host.host_str().unwrap_or_default().to_ascii_lowercase();
    match cfg.host.port() {
        Some(p) => format!("{h}:{p}"),
        None => h,
    }
}

/// Is this request for the configured server (and only for it)?
pub fn matches_host(cfg: &Config, req: &Request) -> bool {
    let proto_ok = req
        .get("protocol")
        .is_some_and(|p| p.eq_ignore_ascii_case(cfg.host.scheme()));
    let host_ok = req
        .get("host")
        .is_some_and(|h| h.to_ascii_lowercase() == authority(cfg));
    proto_ok && host_ok
}

/// The answer to `get`; empty when this is not our host or no token is stored.
pub fn answer_get(cfg: &Config, req: &Request, token: Option<&Token>) -> String {
    let (true, Some(token)) = (matches_host(cfg, req), token) else {
        return String::new();
    };
    // The Basic user name is ignored by the server (ADR-0019 §7); git wants one.
    let user = req.get("username").map_or("gstn", String::as_str);
    format!(
        "protocol={}\nhost={}\nusername={user}\npassword={}\n",
        cfg.host.scheme(),
        authority(cfg),
        token.expose()
    )
}

/// `erase`: git reports a credential the server rejected. Remove the stored
/// token only when it is the one that was rejected (a newer login stays).
/// Returns `true` when a token was removed.
pub fn handle_erase(cfg: &Config, req: &Request, store: &TokenStore) -> crate::error::Result<bool> {
    if !matches_host(cfg, req) {
        return Ok(false);
    }
    let Some(rejected) = req.get("password") else {
        return Ok(false);
    };
    let key = cfg.host_key();
    match store.load(&key, None)? {
        Some(l) if l.token.expose() == rejected => Ok(store.delete(&key)?.removed),
        _ => Ok(false),
    }
}
