//! The agent marketplace (plan 16 §6 A5, ADR-0032 Amendment A5):
//!
//! - `GET /api/v1/agent-parts` (`AgentPartList`, every signed-in account):
//!   the catalog by slug (`kind`, `channel`, `q`), each part with its
//!   newest live version.
//! - `GET /api/v1/agent-parts/{slug}` (`AgentPartRead`): every version.
//! - `GET /api/v1/agent-parts/{slug}/versions/{version}` (`AgentPartRead`):
//!   one version with its files (path, mode, sha256, size).
//! - `POST /api/v1/agent-parts/{slug}/versions` `{"tag": "<slug>/v<x.y.z>"}`
//!   (`AgentPartPublish`: a web session of a Maintainer of the publishing
//!   group who is none of the people behind the tag): `201` the version
//!   (`beta`). The loader pins the commit the tag names
//!   (`forge_marketplace::Marketplace::load_for_publish`).
//! - `POST …/versions/{version}/promote` (`AgentPartPromote`): beta →
//!   stable, `200` (also when it was stable).
//! - `POST …/versions/{version}/revoke` `{"reason"}` (`AgentPartRevoke`):
//!   `200`; final.
//! - `GET /api/v1/agent-keys` (`AgentKeyRead`): the registered public keys
//!   in the shape gstn keeps (`agent-keys.json`).
//!
//! A part, version, tag or parts project that is not there is the same
//! `FG-REPO-0005` as everywhere else.

use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use forge_auth::{AgentPartCtx, InstanceCtx};
use forge_core::UserId;
use forge_errors::{ErrorCode, ForgeError};
use forge_marketplace::{
    ListedSigningKey, Marketplace, MarketplaceRequest, StoredPartSummary as PartSummary,
    StoredPartVersion as StoredVersion, VersionDetail,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::app::AppState;
use crate::body::param_invalid;
use crate::registry::{BodyLoader, Dispatched, LoadFuture, Loader, Lookup, QueryParam, WriteParts};
use crate::routes::write::{absent, bounded, json, no_store};

/// `GET` the catalog.
pub const LIST: &str = "/api/v1/agent-parts";
/// `GET` one part.
pub const ONE: &str = "/api/v1/agent-parts/{slug}";
/// `POST` (publish) a version.
pub const VERSIONS: &str = "/api/v1/agent-parts/{slug}/versions";
/// `GET` one version.
pub const VERSION: &str = "/api/v1/agent-parts/{slug}/versions/{version}";
/// `POST` beta → stable.
pub const PROMOTE: &str = "/api/v1/agent-parts/{slug}/versions/{version}/promote";
/// `POST` revoke.
pub const REVOKE: &str = "/api/v1/agent-parts/{slug}/versions/{version}/revoke";
/// `GET` the public keys.
pub const KEYS: &str = "/api/v1/agent-keys";

const PREFIX: &str = "/api/v1/agent-parts/";

/// The body cap (a tag or a reason).
pub const MAX_BODY: usize = 4096;

/// The catalog's query.
pub const LIST_QUERY: &[QueryParam] = &[
    QueryParam {
        name: "kind",
        description: "Only parts of this kind: skill, agent, rule, hook, command, mcp or adapter",
        integer_range: None,
    },
    QueryParam {
        name: "channel",
        description: "Only parts whose newest live version is on this channel: beta or stable",
        integer_range: None,
    },
    QueryParam {
        name: "q",
        description: "Only parts whose slug contains this text (1..=64 characters)",
        integer_range: None,
    },
];

/// `POST …/versions`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PublishBody {
    /// The tag to publish: `<slug>/v<version>` in the parts repository.
    pub tag: String,
}

/// `POST …/revoke`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RevokeBody {
    /// Why (1..=500 characters, shown with the version).
    pub reason: String,
}

/// One published version.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentPartVersionBody {
    pub slug: String,
    /// skill, agent, rule, hook, command, mcp or adapter.
    pub kind: String,
    pub version: String,
    /// `beta` or `stable`.
    pub channel: String,
    /// `published` or `revoked`.
    pub state: String,
    #[schema(minimum = 1)]
    pub source_project_id: i64,
    /// The commit the version was frozen from.
    pub source_sha: String,
    /// The tag it was published from (`<slug>/v<version>`).
    pub tag: String,
    /// `sha256:<hex>` (ADR-0032 §3).
    pub digest: String,
    /// Base64 of the Ed25519 signature of the digest.
    pub signature: String,
    /// The signing key's kid (`GET /api/v1/agent-keys`).
    pub key_id: String,
    /// exec, network, permissions, fs-write.
    pub capabilities: Vec<String>,
    /// The part's manifest as published (slug, kind, version, requires,
    /// capabilities, files).
    #[schema(value_type = Object)]
    pub manifest: Value,
    #[schema(minimum = 1)]
    pub published_by: i64,
    /// The pipeline that succeeded on the commit.
    #[schema(minimum = 1)]
    pub ci_pipeline_id: i64,
    #[schema(required = true)]
    pub revoked_reason: Option<String>,
    #[schema(required = true, minimum = 1)]
    pub revoked_by: Option<i64>,
    /// UTC, RFC 3339.
    #[schema(required = true)]
    pub revoked_at: Option<String>,
    /// UTC, RFC 3339.
    pub created_at: String,
}

/// One file of a version.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentPartFileBody {
    /// The install path (`.claude/…`, `CLAUDE.md`, `.mcp.json`).
    pub path: String,
    /// `100644` or `100755`.
    pub mode: String,
    /// Lower-case hex of the content's SHA-256.
    pub sha256: String,
    #[schema(minimum = 0)]
    pub size: i64,
}

/// One version with its files.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentPartVersionDetailBody {
    pub version: AgentPartVersionBody,
    pub files: Vec<AgentPartFileBody>,
}

/// `POST …/promote` → whether it changed and the version.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentPartPromoteBody {
    /// `false`: it was on stable already.
    pub changed: bool,
    pub version: AgentPartVersionDetailBody,
}

/// One part of the catalog.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentPartSummaryBody {
    pub slug: String,
    pub kind: String,
    pub deprecated: bool,
    /// The newest version that is not revoked (on the asked channel).
    #[schema(required = true)]
    pub latest: Option<AgentPartVersionBody>,
}

/// The catalog.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentPartListBody {
    pub parts: Vec<AgentPartSummaryBody>,
}

/// One part: every version, newest first.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentPartBody {
    pub slug: String,
    pub versions: Vec<AgentPartVersionBody>,
}

/// One registered public key (`agent-keys.json`, ADR-0032 §7).
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentKeyBody {
    pub kid: String,
    /// Base64 of the 32-byte Ed25519 public key.
    pub public_key: String,
    /// UTC, RFC 3339.
    pub created_at: String,
    /// Signs nothing new; still verifies what it signed.
    #[schema(required = true)]
    pub retired_at: Option<String>,
    /// Its signatures are refused.
    #[schema(required = true)]
    pub revoked_at: Option<String>,
}

/// The registered public keys.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentKeyListBody {
    pub keys: Vec<AgentKeyBody>,
}

fn marketplace(state: &AppState) -> Result<&Marketplace, ForgeError> {
    state.marketplace.as_deref().ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "marketplace_unconfigured")
    })
}

/// The segments after [`PREFIX`] (raw, never decoded: a slug and a
/// version are plain ASCII).
fn segments(raw_path: &str) -> Vec<&str> {
    raw_path
        .strip_prefix(PREFIX)
        .map(|rest| rest.split('/').collect())
        .unwrap_or_default()
}

fn valid_slug(s: &str) -> bool {
    (1..=64).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !s.starts_with('-')
        && !s.ends_with('-')
}

fn valid_version(s: &str) -> bool {
    (1..=64).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
}

/// `{slug}` of a path with `tail` after it (`""`, `versions`).
fn slug_of<'p>(raw_path: &'p str, tail: &[&str]) -> Result<&'p str, ForgeError> {
    match segments(raw_path).as_slice() {
        [slug, rest @ ..] if rest == tail && valid_slug(slug) => Ok(slug),
        _ => Err(param_invalid("slug", "invalid")),
    }
}

/// `{slug}` and `{version}` of `…/versions/{version}` with `tail` after.
fn version_of<'p>(raw_path: &'p str, tail: &[&str]) -> Result<(&'p str, &'p str), ForgeError> {
    match segments(raw_path).as_slice() {
        [slug, "versions", version, rest @ ..] if rest == tail => {
            if !valid_slug(slug) {
                return Err(param_invalid("slug", "invalid"));
            }
            if !valid_version(version) {
                return Err(param_invalid("version", "invalid"));
            }
            Ok((slug, version))
        }
        _ => Err(param_invalid("version", "invalid")),
    }
}

fn found<R>(ctx: R) -> Lookup<R> {
    Lookup::Found {
        ctx,
        canonical: None,
        tombstone: None,
        project: None,
        group: None,
    }
}

fn lookup(ctx: Option<AgentPartCtx>) -> Lookup<AgentPartCtx> {
    match ctx {
        Some(ctx) => found(ctx),
        None => absent(),
    }
}

/// The catalog and the reads: decided on the instance.
pub struct Catalog;

impl Loader<AppState> for Catalog {
    type Ctx = InstanceCtx;
    fn load<'a>(
        &'a self,
        _: &'a AppState,
        _: &'a Parts,
        _: Option<UserId>,
    ) -> LoadFuture<'a, InstanceCtx> {
        Box::pin(async { Ok(found(InstanceCtx)) })
    }
}

/// `POST …/versions`: the tag's facts, the commit pinned.
pub struct ToPublish;

impl BodyLoader<AppState, PublishBody> for ToPublish {
    type Ctx = AgentPartCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        body: &'a PublishBody,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, AgentPartCtx> {
        Box::pin(async move {
            let slug = slug_of(parts.uri.path(), &["versions"])?;
            if body.tag.len() > 255 || body.tag.chars().any(char::is_control) {
                return Err(param_invalid("tag", "invalid"));
            }
            let loaded = bounded(
                state,
                "agent_part_publish_load",
                marketplace(state)?.load_for_publish(slug, &body.tag, viewer),
            )
            .await?;
            Ok(lookup(loaded))
        })
    }
}

/// `POST …/promote` and `…/revoke`: the stored version and the facts its
/// publication recorded.
pub struct PublishedVersion {
    tail: &'static str,
}

/// `…/promote`.
pub const TO_PROMOTE: PublishedVersion = PublishedVersion { tail: "promote" };
/// `…/revoke`.
pub const TO_REVOKE: PublishedVersion = PublishedVersion { tail: "revoke" };

impl PublishedVersion {
    async fn lookup(
        &self,
        state: &AppState,
        parts: &Parts,
        viewer: Option<UserId>,
    ) -> Result<Lookup<AgentPartCtx>, ForgeError> {
        let (slug, version) = version_of(parts.uri.path(), &[self.tail])?;
        let loaded = bounded(
            state,
            "agent_part_version_load",
            marketplace(state)?.load_version(slug, version, viewer),
        )
        .await?;
        Ok(lookup(loaded))
    }
}

impl Loader<AppState> for PublishedVersion {
    type Ctx = AgentPartCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, AgentPartCtx> {
        Box::pin(self.lookup(state, parts, viewer))
    }
}

impl BodyLoader<AppState, RevokeBody> for PublishedVersion {
    type Ctx = AgentPartCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        _: &'a RevokeBody,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, AgentPartCtx> {
        Box::pin(self.lookup(state, parts, viewer))
    }
}

fn version_body(v: StoredVersion) -> AgentPartVersionBody {
    AgentPartVersionBody {
        slug: v.slug,
        kind: v.kind,
        version: v.version,
        channel: v.channel,
        state: v.state,
        source_project_id: v.source_project_id,
        source_sha: v.source_sha,
        tag: v.tag,
        digest: v.digest,
        signature: v.signature,
        key_id: v.key_id,
        capabilities: v.capabilities,
        manifest: v.manifest,
        published_by: v.published_by,
        ci_pipeline_id: v.ci_pipeline_id,
        revoked_reason: v.revoked_reason,
        revoked_by: v.revoked_by,
        revoked_at: v.revoked_at,
        created_at: v.created_at,
    }
}

fn detail_body(d: VersionDetail) -> AgentPartVersionDetailBody {
    AgentPartVersionDetailBody {
        version: version_body(d.version),
        files: d
            .files
            .into_iter()
            .map(|f| AgentPartFileBody {
                path: f.path,
                mode: f.mode,
                sha256: f.sha256,
                size: f.size,
            })
            .collect(),
    }
}

fn summary_body(p: PartSummary) -> AgentPartSummaryBody {
    AgentPartSummaryBody {
        slug: p.slug,
        kind: p.kind,
        deprecated: p.deprecated,
        latest: p.latest.map(version_body),
    }
}

fn key_body(k: ListedSigningKey) -> AgentKeyBody {
    AgentKeyBody {
        kid: k.kid,
        public_key: k.public_key,
        created_at: k.created_at,
        retired_at: k.retired_at,
        revoked_at: k.revoked_at,
    }
}

fn respond(ctx: &crate::Ctx, status: StatusCode, built: Result<Value, ForgeError>) -> Response {
    match built {
        Ok(value) => no_store(status, value),
        Err(e) => ctx.fail(e).into_response(),
    }
}

fn request_of<R: forge_auth::ResourceCtx, B>(w: &WriteParts<R, B>) -> MarketplaceRequest {
    MarketplaceRequest {
        request_id: w.ctx.request_id.clone(), // clone: the context stays for the error path
        credential_id: w.credential_id,
    }
}

/// The catalog's filters: kind, channel, q.
type Filters = (Option<String>, Option<String>, Option<String>);

/// The catalog's filters (unknown or repeated keys are refused).
fn list_query(query: Option<&str>) -> Result<Filters, ForgeError> {
    let (mut kind, mut channel, mut q) = (None, None, None);
    for pair in query.unwrap_or_default().split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, raw) = pair.split_once('=').unwrap_or((pair, ""));
        let value = crate::routes::namespaces::percent_decode(raw)
            .ok_or_else(|| param_invalid("query", "malformed"))?;
        let (slot, name, ok) = match key {
            "kind" => (
                &mut kind,
                "kind",
                [
                    "skill", "agent", "rule", "hook", "command", "mcp", "adapter",
                ]
                .contains(&value.as_str()),
            ),
            "channel" => (
                &mut channel,
                "channel",
                ["beta", "stable"].contains(&value.as_str()),
            ),
            "q" => (
                &mut q,
                "q",
                (1..=64).contains(&value.chars().count()) && !value.chars().any(char::is_control),
            ),
            _ => return Err(param_invalid("query", "unknown_parameter")),
        };
        if !ok {
            return Err(param_invalid(name, "invalid"));
        }
        if slot.replace(value).is_some() {
            return Err(param_invalid(name, "repeated"));
        }
    }
    Ok((kind, channel, q))
}

/// `GET /api/v1/agent-parts` → `200`.
pub async fn list(state: AppState, request: Dispatched<InstanceCtx>) -> Response {
    let query = request.raw_query().map(str::to_owned);
    let w = request.into_write();
    let built = async {
        let (kind, channel, q) = list_query(query.as_deref())?;
        let parts = bounded(
            &state,
            "agent_part_list",
            marketplace(&state)?.list(kind.as_deref(), channel.as_deref(), q.as_deref()),
        )
        .await?;
        json(AgentPartListBody {
            parts: parts.into_iter().map(summary_body).collect(),
        })
    }
    .await;
    respond(&w.ctx, StatusCode::OK, built)
}

/// `GET /api/v1/agent-parts/{slug}` → `200`.
pub async fn read(state: AppState, request: Dispatched<InstanceCtx>) -> Response {
    let path = request.raw_path().to_owned();
    let w = request.into_write();
    let built = async {
        let slug = slug_of(&path, &[])?;
        let versions = bounded(&state, "agent_part_read", marketplace(&state)?.part(slug))
            .await?
            .ok_or_else(not_found)?;
        json(AgentPartBody {
            slug: slug.to_owned(),
            versions: versions.into_iter().map(version_body).collect(),
        })
    }
    .await;
    respond(&w.ctx, StatusCode::OK, built)
}

fn not_found() -> ForgeError {
    ForgeError::new(ErrorCode::NotFound).with_context("resource", "agent_part")
}

/// `GET …/versions/{version}` → `200`.
pub async fn read_version(state: AppState, request: Dispatched<InstanceCtx>) -> Response {
    let path = request.raw_path().to_owned();
    let w = request.into_write();
    let built = async {
        let (slug, version) = version_of(&path, &[])?;
        let detail = bounded(
            &state,
            "agent_part_version_read",
            marketplace(&state)?.version(slug, version),
        )
        .await?
        .ok_or_else(not_found)?;
        json(detail_body(detail))
    }
    .await;
    respond(&w.ctx, StatusCode::OK, built)
}

/// `POST …/versions` → `201` the published version.
pub async fn publish(state: AppState, request: Dispatched<AgentPartCtx, PublishBody>) -> Response {
    let w = request.into_write();
    let built = async {
        let detail = marketplace(&state)?
            .publish(&request_of(&w), &w.proof)
            .await?;
        json(detail_body(detail))
    }
    .await;
    respond(&w.ctx, StatusCode::CREATED, built)
}

/// `POST …/promote` → `200`.
pub async fn promote(state: AppState, request: Dispatched<AgentPartCtx>) -> Response {
    let w = request.into_write();
    let built = async {
        let (changed, detail) = marketplace(&state)?
            .promote(&request_of(&w), &w.proof)
            .await?;
        json(AgentPartPromoteBody {
            changed,
            version: detail_body(detail),
        })
    }
    .await;
    respond(&w.ctx, StatusCode::OK, built)
}

/// `POST …/revoke` → `200`.
pub async fn revoke(state: AppState, request: Dispatched<AgentPartCtx, RevokeBody>) -> Response {
    let w = request.into_write();
    let built = async {
        let detail = marketplace(&state)?
            .revoke(&request_of(&w), &w.proof, &w.input.reason)
            .await?;
        json(detail_body(detail))
    }
    .await;
    respond(&w.ctx, StatusCode::OK, built)
}

/// `GET /api/v1/agent-keys` → `200`.
pub async fn keys(state: AppState, request: Dispatched<InstanceCtx>) -> Response {
    let w = request.into_write();
    let built = async {
        let keys = bounded(&state, "agent_key_list", marketplace(&state)?.keys()).await?;
        json(AgentKeyListBody {
            keys: keys.into_iter().map(key_body).collect(),
        })
    }
    .await;
    respond(&w.ctx, StatusCode::OK, built)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_path_names_one_slug_and_version() {
        assert_eq!(
            slug_of("/api/v1/agent-parts/guard-hook/versions", &["versions"]).ok(),
            Some("guard-hook")
        );
        assert!(slug_of("/api/v1/agent-parts/Guard/versions", &["versions"]).is_err());
        assert!(slug_of("/api/v1/agent-parts/a/b/versions", &["versions"]).is_err());
        assert_eq!(
            version_of("/api/v1/agent-parts/g/versions/1.0.0/promote", &["promote"]).ok(),
            Some(("g", "1.0.0"))
        );
        assert!(version_of("/api/v1/agent-parts/g/versions/1.0.0%2F", &[]).is_err());
        assert!(version_of("/api/v1/agent-parts/g/versions/1.0.0/revoke", &["promote"]).is_err());
    }

    #[test]
    fn the_catalog_query_is_strict() {
        assert_eq!(
            list_query(Some("kind=hook&channel=stable&q=guard")).ok(),
            Some((
                Some("hook".to_owned()),
                Some("stable".to_owned()),
                Some("guard".to_owned())
            ))
        );
        assert!(list_query(Some("kind=virus")).is_err());
        assert!(list_query(Some("kind=hook&kind=rule")).is_err());
        assert!(list_query(Some("limit=5")).is_err());
    }
}
