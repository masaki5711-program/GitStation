//! The route registry (ADR-0010): every route is registered once with its
//! method, path, authorization `Action` and a summary. From the same list come
//! the axum router, `Git/docs/api/openapi.json` and
//! `Git/docs/authz/routes.generated.json` (the `authz-coverage` gate input).
//! Never call `Router::route` directly.

use std::collections::{BTreeMap, BTreeSet};

use axum::handler::Handler;
use axum::routing::{MethodFilter, MethodRouter};
use axum::Router;
use forge_auth::Action;
use forge_errors::{ErrorCode, ForgeError};
use serde_json::{json, Map, Value};

/// HTTP methods the API uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Verb {
    Get,
    Post,
    Put,
    Patch,
    Delete,
}

impl Verb {
    pub fn as_str(self) -> &'static str {
        match self {
            Verb::Get => "GET",
            Verb::Post => "POST",
            Verb::Put => "PUT",
            Verb::Patch => "PATCH",
            Verb::Delete => "DELETE",
        }
    }

    fn filter(self) -> MethodFilter {
        match self {
            Verb::Get => MethodFilter::GET,
            Verb::Post => MethodFilter::POST,
            Verb::Put => MethodFilter::PUT,
            Verb::Patch => MethodFilter::PATCH,
            Verb::Delete => MethodFilter::DELETE,
        }
    }
}

/// One registered route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteDef {
    pub verb: Verb,
    /// axum 0.8 / OpenAPI style: `/api/v1/projects/{id}`.
    pub path: &'static str,
    pub action: Action,
    pub summary: &'static str,
}

/// Collects routes; turns into the router and the generated documents.
pub struct Registry<S> {
    defs: Vec<RouteDef>,
    routers: BTreeMap<&'static str, MethodRouter<S>>,
    problems: Vec<String>,
}

impl<S: Clone + Send + Sync + 'static> Default for Registry<S> {
    fn default() -> Self {
        Self::new()
    }
}

/// A path axum accepts and the manifests can describe: non-empty, starts
/// with '/', no empty segment.
fn valid_path(path: &str) -> bool {
    path == "/" || (path.starts_with('/') && path.split('/').skip(1).all(|seg| !seg.is_empty()))
}

impl<S: Clone + Send + Sync + 'static> Registry<S> {
    pub fn new() -> Self {
        Self {
            defs: Vec::new(),
            routers: BTreeMap::new(),
            problems: Vec::new(),
        }
    }

    /// Register `verb path` handled by `handler`, decided by `action`.
    ///
    /// Every non-public route is wrapped in token authentication
    /// ([`crate::auth`]: `Authorization: Bearer` / Basic with the token as
    /// password → `Principal`) and then the authentication guard: without
    /// an authenticated, active `Principal` in the request extensions it
    /// answers `FG-AUTH-0001` / `FG-AUTH-0004` and the handler is never
    /// entered. Public routes never consult the credentials. The guard does not decide on the resource: the handler must
    /// still call `forge_auth::authorize(.., def.action, ..)` before any side
    /// effect.
    #[must_use]
    pub fn route<H, T>(
        mut self,
        verb: Verb,
        path: &'static str,
        action: Action,
        summary: &'static str,
        handler: H,
    ) -> Self
    where
        H: Handler<T, S>,
        T: 'static,
    {
        if !valid_path(path) {
            self.problems.push(format!("invalid path {path:?}"));
            return self;
        }
        if self.defs.iter().any(|d| d.verb == verb && d.path == path) {
            // axum would panic on an overlapping route; fail at build instead.
            self.problems
                .push(format!("duplicate {} {path}", verb.as_str()));
            return self;
        }
        self.defs.push(RouteDef {
            verb,
            path,
            action,
            summary,
        });
        let mut one = axum::routing::on(verb.filter(), handler);
        if !action.is_public() {
            // The later route_layer runs first: token authentication builds
            // the principal, then the guard requires it.
            one = one
                .route_layer(axum::middleware::from_fn_with_state(
                    action,
                    crate::guard::require_principal,
                ))
                .route_layer(axum::middleware::from_fn(crate::auth::authenticate));
        }
        let merged = match self.routers.remove(path) {
            Some(existing) => existing.merge(one),
            None => one,
        };
        self.routers.insert(path, merged);
        self
    }

    pub fn defs(&self) -> &[RouteDef] {
        &self.defs
    }

    /// The router (crate-private: routes are added only through `route`).
    /// An invalid or duplicate registration is a startup failure
    /// (`FG-WEB-0004`), not a panic.
    pub(crate) fn into_router(self) -> Result<Router<S>, ForgeError> {
        if !self.problems.is_empty() {
            return Err(ForgeError::new(ErrorCode::RouteRegistryInvalid)
                .with_context("reason", "invalid_or_duplicate_route")
                .with_context("routes", self.problems.join(", ")));
        }
        Ok(self
            .routers
            .into_iter()
            .fold(Router::new(), |router, (path, mr)| router.route(path, mr)))
    }
}

/// `Git/docs/authz/routes.generated.json`: the routes, then the planned
/// actions as `{ "action": .., "planned": true }`.
pub fn routes_json(defs: &[RouteDef], planned: &[Action]) -> String {
    let mut items: Vec<&RouteDef> = defs.iter().collect();
    items.sort_by(|a, b| (a.path, a.verb).cmp(&(b.path, b.verb)));
    let list: Vec<Value> = items
        .iter()
        .map(|d| {
            json!({
                "method": d.verb.as_str(),
                "path": d.path,
                "action": d.action.name(),
                "public": d.action.is_public(),
            })
        })
        .chain(
            planned
                .iter()
                .map(|a| json!({ "action": a.name(), "planned": true })),
        )
        .collect();
    pretty(&Value::Array(list))
}

/// `Git/docs/api/openapi.json` (OpenAPI 3.1). Request / response schemas join
/// as the resource routes land (M1 phase 8); every operation documents the
/// error body and the security requirement already.
pub fn openapi_json(defs: &[RouteDef], version: &str) -> String {
    let mut paths: BTreeMap<&str, Map<String, Value>> = BTreeMap::new();
    for d in defs {
        let mut op = Map::new();
        op.insert("operationId".into(), json!(operation_id(d.verb, d.path)));
        op.insert("summary".into(), json!(d.summary));
        op.insert("x-forge-action".into(), json!(d.action.name()));
        if d.action.is_public() {
            op.insert("security".into(), json!([]));
        }
        let params: BTreeSet<&str> = path_params(d.path).collect();
        if !params.is_empty() {
            let list: Vec<Value> = params
                .iter()
                .map(|p| json!({ "name": p, "in": "path", "required": true, "schema": { "type": "string" } }))
                .collect();
            op.insert("parameters".into(), Value::Array(list));
        }
        op.insert(
            "responses".into(),
            json!({
                "default": {
                    "description": "Error: registered code, localized message, request id",
                    "content": { "application/json": { "schema": { "$ref": "#/components/schemas/Error" } } }
                }
            }),
        );
        paths
            .entry(d.path)
            .or_default()
            .insert(d.verb.as_str().to_ascii_lowercase(), Value::Object(op));
    }
    let doc = json!({
        "openapi": "3.1.0",
        "info": { "title": "GitStation API", "version": version },
        "paths": paths,
        "security": [ { "bearerToken": [] } ],
        "components": {
            "securitySchemes": {
                "bearerToken": { "type": "http", "scheme": "bearer", "description": "Personal access token" }
            },
            "schemas": {
                "Error": {
                    "type": "object",
                    "required": ["error_code", "user_message", "request_id"],
                    "additionalProperties": false,
                    "properties": {
                        "error_code": { "type": "string", "pattern": "^FG-[A-Z]+-[0-9]{4}$" },
                        "user_message": { "type": "string" },
                        "request_id": { "type": "string" }
                    }
                }
            }
        }
    });
    pretty(&doc)
}

/// A document-unique OpenAPI operationId from the method and the path
/// (`GET /api/v1/projects/{id}` → `get_api_v1_projects_id`); (verb, path)
/// is unique in the registry, so the id is too.
fn operation_id(verb: Verb, path: &str) -> String {
    let slug: String = path
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let slug = slug
        .split('_')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("_");
    format!("{}_{slug}", verb.as_str().to_ascii_lowercase())
}

fn path_params(path: &str) -> impl Iterator<Item = &str> {
    path.split('/')
        .filter_map(|seg| seg.strip_prefix('{').and_then(|s| s.strip_suffix('}')))
}

fn pretty(v: &Value) -> String {
    let mut s = serde_json::to_string_pretty(v).unwrap_or_else(|_| "null".to_string());
    s.push('\n');
    s
}
