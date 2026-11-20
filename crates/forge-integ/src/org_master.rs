//! The copy of the PFS organisation master (ADR-0038 §1, step A): one
//! [`OrgMasterSync::run_once`] reads `GET {portal.registry_url}/api/v1/org/units`
//! and `/api/v1/org/positions` (every plant) with the gitstation client's
//! client-credentials token, validates **every** field of every item
//! ([`parse_units`], [`parse_positions`]) and replaces the copy in one
//! transaction (`forge_db::org_master::replace`). Any failure writes
//! nothing: the previous copy stays.
//!
//! The registry's contract (PFS MR !28): the envelope
//! `{success: true, data: {items: [...]}}`; a unit
//! `{plantCode, code, level, parentCode, name{ja,en,id}, costCenter,
//! headStaffId, validFrom, version}`; a position `{plantCode, code, name,
//! rank, sortOrder, active, version}`. PFS admits the token when
//! `ORG_READER_CLIENTS` lists `gitstation`; a person's token gets 403.
//!
//! Failures (the worker logs them, once per streak):
//! - no answer, a server error, no token → `FG-INT-0025` (`reason`);
//! - 401 / 403 / 404 → `FG-INT-0026` (401 / 403 also forget the token);
//! - a body that breaks the contract → `FG-INT-0027` (`reason`, `field`,
//!   `index`; never a value or a name).
//!
//! Transport: rustls with `portal.ca_file`'s roots only, no proxy, no
//! redirect, `org.master_timeout_ms` per request, at most
//! [`MAX_BODY_BYTES`] read.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use forge_core::portal::ServiceTokenSource;
use forge_core::RequestId;
use forge_db::change::{Actor, ChangeContext};
use forge_db::org_master::{
    self as master, LocalizedName, MasterPosition, MasterUnit, Synced, UnitLevel,
};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};
use serde_json::{Map, Value};

/// The units list below `portal.registry_url`.
pub const UNITS_PATH: &str = "/api/v1/org/units";
/// The positions list below `portal.registry_url`.
pub const POSITIONS_PATH: &str = "/api/v1/org/positions";
/// The largest body read (a larger one is `FG-INT-0027 reason=too_many`).
pub const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;
/// The most items of one list.
pub const MAX_ITEMS: usize = 10_000;
/// The longest name, in characters.
const MAX_NAME_CHARS: usize = 200;
const MAX_COST_CENTER_CHARS: usize = 64;

/// How one GET ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fetched {
    /// 200 with this body (at most [`MAX_BODY_BYTES`] + 1 bytes).
    Body(Vec<u8>),
    /// Another status.
    Status(u16),
    /// No answer: `connect`, `connect_timeout`, `timeout`, `redirect`,
    /// `request`.
    Failed(&'static str),
}

/// The boxed future of a [`MasterTransport`].
pub type FetchFuture<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = Fetched> + Send + 'a>>;

/// Reads one list from the registry (tests use a stand-in or a local server).
pub trait MasterTransport: Send + Sync {
    fn get<'a>(&'a self, url: &'a str, token: &'a str) -> FetchFuture<'a>;
}

/// The production transport: rustls with `portal.ca_file`'s roots only.
pub struct HttpMasterTransport {
    client: reqwest::Client,
}

impl std::fmt::Debug for HttpMasterTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpMasterTransport")
            .finish_non_exhaustive()
    }
}

impl HttpMasterTransport {
    /// `ca_pem`: `portal.ca_file`'s content (`None`: no root is trusted;
    /// the configuration requires it for an https registry).
    pub fn new(ca_pem: Option<&[u8]>, timeout: Duration) -> Result<Self, ForgeError> {
        let invalid = |reason: &'static str| {
            ForgeError::new(ErrorCode::ConfigInvalid)
                .with_context("config_key", "portal.ca_file")
                .with_context("reason", reason)
        };
        let mut builder = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(timeout.min(Duration::from_secs(3)))
            .timeout(timeout)
            .tls_built_in_root_certs(false)
            .user_agent(concat!("gitstation/", env!("CARGO_PKG_VERSION")));
        if let Some(pem) = ca_pem {
            let certs = reqwest::Certificate::from_pem_bundle(pem)
                .map_err(|e| invalid("invalid_pem").with_source(e))?;
            if certs.is_empty() {
                return Err(invalid("no_certificate"));
            }
            for cert in certs {
                builder = builder.add_root_certificate(cert);
            }
        }
        let client = builder.build().map_err(|e| {
            ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "http_client")
                .with_source(e)
        })?;
        Ok(Self { client })
    }
}

/// A transport error as the reason it means (never its text).
fn classify(e: &reqwest::Error) -> &'static str {
    if e.is_connect() && e.is_timeout() {
        "connect_timeout"
    } else if e.is_timeout() {
        "timeout"
    } else if e.is_connect() {
        "connect"
    } else if e.is_redirect() {
        "redirect"
    } else {
        "request"
    }
}

impl MasterTransport for HttpMasterTransport {
    fn get<'a>(&'a self, url: &'a str, token: &'a str) -> FetchFuture<'a> {
        Box::pin(async move {
            let sent = self
                .client
                .get(url)
                .bearer_auth(token)
                .header(reqwest::header::ACCEPT, "application/json")
                .send()
                .await;
            let mut response = match sent {
                Ok(r) => r,
                Err(e) => return Fetched::Failed(classify(&e)),
            };
            let status = response.status().as_u16();
            if (300..400).contains(&status) {
                return Fetched::Failed("redirect");
            }
            if status != 200 {
                return Fetched::Status(status);
            }
            let mut body = Vec::new();
            loop {
                match response.chunk().await {
                    Ok(Some(chunk)) => {
                        body.extend_from_slice(&chunk);
                        if body.len() > MAX_BODY_BYTES {
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(e) => return Fetched::Failed(classify(&e)),
                }
            }
            Fetched::Body(body)
        })
    }
}

// ------------------------------------------------------------------ parsing

/// Why a body breaks the contract (`FG-INT-0027`): a fixed reason, the
/// field and the item's index — never a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Invalid {
    pub reason: &'static str,
    pub field: Option<&'static str>,
    pub index: Option<usize>,
}

impl Invalid {
    fn of(reason: &'static str) -> Self {
        Self {
            reason,
            field: None,
            index: None,
        }
    }

    fn field(field: &'static str, index: usize) -> Self {
        Self {
            reason: "field",
            field: Some(field),
            index: Some(index),
        }
    }
}

fn items(body: &[u8]) -> Result<Vec<Value>, Invalid> {
    if body.len() > MAX_BODY_BYTES {
        return Err(Invalid::of("too_many"));
    }
    let doc: Value = serde_json::from_slice(body).map_err(|_| Invalid::of("envelope"))?;
    if doc.get("success") != Some(&Value::Bool(true)) {
        return Err(Invalid::of("envelope"));
    }
    let Some(Value::Array(items)) = doc.get("data").and_then(|d| d.get("items")) else {
        return Err(Invalid::of("envelope"));
    };
    if items.len() > MAX_ITEMS {
        return Err(Invalid::of("too_many"));
    }
    Ok(items.clone()) // clone: the items outlive the document
}

fn hidden(c: char) -> bool {
    c.is_control()
        || matches!(c,
            '\u{00AD}' | '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}' | '\u{2066}'..='\u{2069}' | '\u{FEFF}')
}

/// `^[A-Z0-9]{1,4}$`.
fn plant_ok(s: &str) -> bool {
    (1..=4).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
}

/// `^[A-Z0-9][A-Z0-9_-]{0,19}$`.
fn unit_code_ok(s: &str) -> bool {
    let b = s.as_bytes();
    (1..=20).contains(&b.len())
        && (b[0].is_ascii_uppercase() || b[0].is_ascii_digit())
        && b.iter()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || matches!(c, b'_' | b'-'))
}

/// `^[a-z][a-z0-9_]{0,63}$`.
fn position_code_ok(s: &str) -> bool {
    let b = s.as_bytes();
    (1..=64).contains(&b.len())
        && b[0].is_ascii_lowercase()
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_')
}

/// `^[A-Za-z0-9._-]{1,64}$`.
fn staff_id_ok(s: &str) -> bool {
    (1..=64).contains(&s.len())
        && s.bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
}

/// A calendar date `YYYY-MM-DD` (optionally followed by `T…`): the date.
fn date_of(s: &str) -> Option<String> {
    let day = s.get(..10)?;
    if s.len() > 10 && !s[10..].starts_with('T') {
        return None;
    }
    let b = day.as_bytes();
    if b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let num = |r: std::ops::Range<usize>| -> Option<u32> {
        let part = day.get(r)?;
        part.bytes()
            .all(|c| c.is_ascii_digit())
            .then(|| part.parse().ok())
            .flatten()
    };
    let (y, m, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let days = match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return None,
    };
    (y >= 1900 && (1..=days).contains(&d)).then(|| day.to_owned())
}

struct Item<'a> {
    map: &'a Map<String, Value>,
    index: usize,
}

impl<'a> Item<'a> {
    fn of(value: &'a Value, index: usize) -> Result<Self, Invalid> {
        value
            .as_object()
            .map(|map| Self { map, index })
            .ok_or(Invalid {
                reason: "item",
                field: None,
                index: Some(index),
            })
    }

    fn bad(&self, field: &'static str) -> Invalid {
        Invalid::field(field, self.index)
    }

    fn text(&self, field: &'static str, ok: impl Fn(&str) -> bool) -> Result<String, Invalid> {
        match self.map.get(field) {
            Some(Value::String(s)) if ok(s) => Ok(s.clone()), // clone: the row owns it
            _ => Err(self.bad(field)),
        }
    }

    /// Absent, `null` or `""` is `None`; otherwise it must pass `ok`.
    fn optional(
        &self,
        field: &'static str,
        ok: impl Fn(&str) -> bool,
    ) -> Result<Option<String>, Invalid> {
        match self.map.get(field) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) if s.is_empty() => Ok(None),
            Some(Value::String(s)) if ok(s) => Ok(Some(s.clone())), // clone: the row owns it
            Some(_) => Err(self.bad(field)),
        }
    }

    fn int(&self, field: &'static str, (min, max): (i64, i64)) -> Result<i32, Invalid> {
        self.map
            .get(field)
            .and_then(Value::as_i64)
            .filter(|n| (min..=max).contains(n))
            .and_then(|n| i32::try_from(n).ok())
            .ok_or_else(|| self.bad(field))
    }

    fn name(&self, field: &'static str) -> Result<LocalizedName, Invalid> {
        let ok = |s: &str| {
            let n = s.chars().count();
            (1..=MAX_NAME_CHARS).contains(&n) && !s.chars().any(hidden) && !s.trim().is_empty()
        };
        match self.map.get(field) {
            Some(Value::String(s)) if ok(s) => Ok(LocalizedName {
                ja: s.clone(), // clone: the row owns it
                en: None,
                id: None,
            }),
            Some(Value::Object(names)) => {
                let lang = |key: &str| match names.get(key) {
                    None | Some(Value::Null) => Ok(None),
                    Some(Value::String(s)) if s.is_empty() => Ok(None),
                    Some(Value::String(s)) if ok(s) => Ok(Some(s.clone())), // clone: owned
                    Some(_) => Err(self.bad(field)),
                };
                let ja = lang("ja")?.ok_or_else(|| self.bad(field))?;
                Ok(LocalizedName {
                    ja,
                    en: lang("en")?,
                    id: lang("id")?,
                })
            }
            _ => Err(self.bad(field)),
        }
    }
}

fn no_duplicates<T>(rows: &[T], key: impl Fn(&T) -> (&str, &str)) -> Result<(), Invalid> {
    let mut seen = HashSet::new();
    for (i, r) in rows.iter().enumerate() {
        if !seen.insert(key(r)) {
            return Err(Invalid {
                reason: "duplicate",
                field: Some("code"),
                index: Some(i),
            });
        }
    }
    Ok(())
}

/// The units of a 200 body, every field validated (see the module
/// documentation). An empty list is refused (`empty`).
pub fn parse_units(body: &[u8]) -> Result<Vec<MasterUnit>, Invalid> {
    let items = items(body)?;
    if items.is_empty() {
        return Err(Invalid::of("empty"));
    }
    let mut out = Vec::with_capacity(items.len());
    for (index, value) in items.iter().enumerate() {
        let item = Item::of(value, index)?;
        let code = item.text("code", unit_code_ok)?;
        let parent_code = item.optional("parentCode", unit_code_ok)?;
        if parent_code.as_deref() == Some(code.as_str()) {
            return Err(Invalid {
                reason: "self_parent",
                field: Some("parentCode"),
                index: Some(index),
            });
        }
        let level = match item.map.get("level") {
            Some(Value::String(s)) => UnitLevel::parse(s).ok_or_else(|| item.bad("level"))?,
            _ => return Err(item.bad("level")),
        };
        let valid_from = match item.map.get("validFrom") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) => Some(date_of(s).ok_or_else(|| item.bad("validFrom"))?),
            Some(_) => return Err(item.bad("validFrom")),
        };
        out.push(MasterUnit {
            plant_code: item.text("plantCode", plant_ok)?,
            code,
            level,
            parent_code,
            name: item.name("name")?,
            cost_center: item.optional("costCenter", |s| {
                s.chars().count() <= MAX_COST_CENTER_CHARS && !s.chars().any(hidden)
            })?,
            head_staff_id: item.optional("headStaffId", staff_id_ok)?,
            valid_from,
            version: item.int("version", (0, i64::from(i32::MAX)))?,
        });
    }
    no_duplicates(&out, |u| (u.plant_code.as_str(), u.code.as_str()))?;
    Ok(out)
}

/// The positions of a 200 body, every field validated (an empty list is
/// valid: a plant may not use positions yet).
pub fn parse_positions(body: &[u8]) -> Result<Vec<MasterPosition>, Invalid> {
    let items = items(body)?;
    let mut out = Vec::with_capacity(items.len());
    for (index, value) in items.iter().enumerate() {
        let item = Item::of(value, index)?;
        out.push(MasterPosition {
            plant_code: item.text("plantCode", plant_ok)?,
            code: item.text("code", position_code_ok)?,
            name: item.name("name")?,
            rank: item.int("rank", (0, 999))?,
            sort_order: item.int("sortOrder", (i64::from(i32::MIN), i64::from(i32::MAX)))?,
            active: item
                .map
                .get("active")
                .and_then(Value::as_bool)
                .ok_or_else(|| item.bad("active"))?,
            version: item.int("version", (0, i64::from(i32::MAX)))?,
        });
    }
    no_duplicates(&out, |p| (p.plant_code.as_str(), p.code.as_str()))?;
    Ok(out)
}

// --------------------------------------------------------------------- sync

/// One list's failure as its code: what the worker logs.
fn failed(code: ErrorCode, list: &'static str, reason: &'static str) -> ForgeError {
    ForgeError::new(code)
        .with_context("event", "forge.org.master_sync_failed")
        .with_context("list", list)
        .with_context("reason", reason)
}

fn invalid(list: &'static str, why: Invalid) -> ForgeError {
    let mut e = failed(ErrorCode::OrgMasterInvalid, list, why.reason);
    if let Some(field) = why.field {
        e = e.with_context("field", field);
    }
    if let Some(index) = why.index {
        e = e.with_context("index", index);
    }
    e
}

/// The worker task's work (ADR-0038 §1).
pub struct OrgMasterSync {
    db: Db,
    units_url: String,
    positions_url: String,
    tokens: Arc<dyn ServiceTokenSource>,
    transport: Arc<dyn MasterTransport>,
}

impl std::fmt::Debug for OrgMasterSync {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OrgMasterSync")
            .field("units_url", &self.units_url)
            .finish_non_exhaustive()
    }
}

impl OrgMasterSync {
    /// `registry_url`: `portal.registry_url` (an origin).
    pub fn new(
        db: &Db,
        registry_url: &str,
        tokens: Arc<dyn ServiceTokenSource>,
        transport: Arc<dyn MasterTransport>,
    ) -> Self {
        let origin = registry_url.trim_end_matches('/');
        Self {
            db: db.clone(), // clone: a pool handle
            units_url: format!("{origin}{UNITS_PATH}"),
            positions_url: format!("{origin}{POSITIONS_PATH}"),
            tokens,
            transport,
        }
    }

    async fn fetch(&self, list: &'static str, url: &str) -> Result<Vec<u8>, ForgeError> {
        let token = self.tokens.service_token().await.map_err(|e| {
            let detail = e
                .context()
                .iter()
                .find(|(k, _)| *k == "reason")
                .map_or_else(|| "none".to_owned(), |(_, v)| v.clone()); // clone: logged
            failed(ErrorCode::OrgMasterUnreachable, list, "service_token")
                .with_context("detail", detail)
        })?;
        match self.transport.get(url, token.expose()).await {
            Fetched::Body(body) => Ok(body),
            Fetched::Status(status @ (401 | 403)) => {
                self.tokens.forget();
                Err(failed(ErrorCode::OrgMasterRefused, list, "refused")
                    .with_context("status", status))
            }
            Fetched::Status(404) => {
                Err(failed(ErrorCode::OrgMasterRefused, list, "refused")
                    .with_context("status", 404))
            }
            Fetched::Status(status) => Err(failed(ErrorCode::OrgMasterUnreachable, list, "status")
                .with_context("status", status)),
            Fetched::Failed(reason) => Err(failed(ErrorCode::OrgMasterUnreachable, list, reason)),
        }
    }

    /// Read both lists, validate them and replace the copy. Any failure
    /// writes nothing (the previous copy stays) and is returned for the
    /// caller to log; the database's own failures keep their codes.
    pub async fn run_once(&self, request_id: &RequestId) -> Result<Synced, ForgeError> {
        let units = self.fetch("units", &self.units_url).await?;
        let units = parse_units(&units).map_err(|why| invalid("units", why))?;
        let positions = self.fetch("positions", &self.positions_url).await?;
        let positions = parse_positions(&positions).map_err(|why| invalid("positions", why))?;
        let ctx = ChangeContext::new(Actor::System, request_id.clone()); // clone: the context owns it
        master::replace(&self.db, &ctx, &units, &positions).await
    }
}

#[cfg(test)]
#[path = "org_master_tests.rs"]
mod tests;
