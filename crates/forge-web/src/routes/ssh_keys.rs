//! The signed-in user's SSH public keys (M4 C13, ADR-0032 §2):
//! `GET|POST /api/v1/user/ssh-keys` and `DELETE /api/v1/user/ssh-keys/{id}`.
//!
//! - `GET` (`SshKeyList`, `read_api`): the caller's own keys, newest first,
//!   and the SSH server's facts for the settings page — whether SSH is on,
//!   the host and port clone URLs use, and the host key fingerprints to
//!   check on the first connection (`ssh` is `null` while SSH is off).
//! - `POST` (`SshKeyAdd`): one key line for the caller. **Sessions only**
//!   (`authorize()`: `FG-AUTH-0002 reason=session_required`): a token
//!   never adds a key. The line is parsed after the decision
//!   (`FG-AUTH-0023`: malformed, unsupported, duplicate, limit, title).
//! - `DELETE` (`SshKeyDelete`, `api`): the loader reads the key's owner;
//!   another user's key is the same `FG-REPO-0005` as an absent one
//!   (administrators included). `204`, also when it was deleted meanwhile.

use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use forge_auth::{TokenCtx, TokenOwnerCtx};
use forge_core::ssh_key::parse_public_key;
use forge_core::{parse_positive_id, UserId};
use forge_errors::ForgeError;
use forge_hosting::accounts::SshKeyInfo;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::app::AppState;
use crate::body::param_invalid;
use crate::registry::{Dispatched, LoadFuture, Loader, Lookup};
use crate::routes::write::{absent, bounded, hosting, json, no_store, own, write_request};

/// `GET` / `POST` the caller's keys.
pub const COLLECTION: &str = "/api/v1/user/ssh-keys";
/// `DELETE` one of them.
pub const ONE: &str = "/api/v1/user/ssh-keys/{id}";
const PREFIX: &str = "/api/v1/user/ssh-keys/";
/// The largest add body (a key line is at most 8 KiB).
pub const MAX_BODY: usize = 10 * 1024;

/// Why a caller without a user (a runner, a job) finds no keys.
const WITHOUT_USER: &str = "ssh_key_route_without_user";

/// `POST /api/v1/user/ssh-keys`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AddSshKey {
    /// One public key line: `ssh-ed25519 AAAA… [comment]` (Ed25519 or
    /// ECDSA P-256 / P-384 / P-521).
    pub key: String,
    /// 1..=100 characters; empty or absent = the key's comment.
    pub title: Option<String>,
}

/// One key as the API shows it.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SshKeyBody {
    #[schema(minimum = 1)]
    pub id: i64,
    pub title: String,
    pub algorithm: String,
    /// `SHA256:…` (what `ssh-keygen -l -E sha256 -f <key>.pub` prints).
    pub fingerprint: String,
    /// `<algorithm> <base64>` (no comment).
    pub public_key: String,
    /// UTC, RFC 3339.
    pub created_at: String,
    #[schema(required = true)]
    pub last_used_at: Option<String>,
}

/// The SSH server as clients see it (`null` while `git.ssh.enabled` is
/// off).
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SshServerBody {
    /// `host:port` of clone URLs: `ssh://git@<clone_host>/<path>.git`.
    pub clone_host: String,
    /// The host key fingerprints (`SHA256:…`) to compare on the first
    /// connection.
    pub host_key_fingerprints: Vec<String>,
}

/// The caller's keys and the SSH server's facts.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SshKeyList {
    pub items: Vec<SshKeyBody>,
    #[schema(required = true)]
    pub ssh: Option<SshServerBody>,
}

fn body_of(k: &SshKeyInfo) -> SshKeyBody {
    SshKeyBody {
        id: k.key_id,
        title: k.title.clone(), // clone: the row is borrowed
        algorithm: k.algorithm.clone(),
        fingerprint: k.fingerprint.clone(),
        public_key: k.public_key.clone(),
        created_at: k.created_at.clone(),
        last_used_at: k.last_used_at.clone(),
    }
}

/// `GET` / `POST /api/v1/user/ssh-keys`: the caller's keys as a whole.
pub struct OwnSshKeys;

impl Loader<AppState> for OwnSshKeys {
    type Ctx = TokenOwnerCtx;
    fn load<'a>(
        &'a self,
        _: &'a AppState,
        _: &'a Parts,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, TokenOwnerCtx> {
        Box::pin(async move { Ok(own(viewer, WITHOUT_USER, TokenOwnerCtx::new)) })
    }
}

/// `POST`: the same context, with the body (parsed after the decision).
impl crate::registry::BodyLoader<AppState, AddSshKey> for OwnSshKeys {
    type Ctx = TokenOwnerCtx;
    fn load<'a>(
        &'a self,
        _: &'a AppState,
        _: &'a Parts,
        _: &'a AddSshKey,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, TokenOwnerCtx> {
        Box::pin(async move { Ok(own(viewer, WITHOUT_USER, TokenOwnerCtx::new)) })
    }
}

/// `DELETE /api/v1/user/ssh-keys/{id}`: the key and its owner.
pub struct SshKeyToDelete;

impl Loader<AppState> for SshKeyToDelete {
    type Ctx = TokenCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        _: Option<UserId>,
    ) -> LoadFuture<'a, TokenCtx> {
        Box::pin(async move {
            let id = parts
                .uri
                .path()
                .strip_prefix(PREFIX)
                .filter(|rest| !rest.contains('/'))
                .and_then(parse_positive_id)
                .ok_or_else(|| param_invalid("id", "not_positive_decimal"))?;
            let hosting = hosting(state)?;
            Ok(
                match bounded(state, "ssh_key_owner_load", hosting.ssh_key_owner(id)).await? {
                    Some(owner) => Lookup::Found {
                        ctx: TokenCtx::new(id, owner),
                        canonical: None,
                        tombstone: None,
                        project: None,
                        group: None,
                    },
                    None => absent(),
                },
            )
        })
    }
}

fn respond(ctx: &crate::Ctx, status: StatusCode, body: Result<Value, ForgeError>) -> Response {
    match body {
        Ok(body) => no_store(status, body),
        Err(e) => ctx.fail(e).into_response(),
    }
}

/// `GET /api/v1/user/ssh-keys` → `200`.
pub async fn list(state: AppState, request: Dispatched<TokenOwnerCtx>) -> Response {
    let w = request.into_write();
    let built = async {
        let hosting = hosting(&state)?;
        let keys = bounded(&state, "ssh_key_list", hosting.ssh_keys(&w.proof)).await?;
        json(SshKeyList {
            items: keys.iter().map(body_of).collect(),
            ssh: state.ssh_info.as_deref().map(|info| SshServerBody {
                clone_host: info.clone_host.clone(), // clone: the state is shared
                host_key_fingerprints: info.host_key_fingerprints.clone(),
            }),
        })
    }
    .await;
    respond(&w.ctx, StatusCode::OK, built)
}

/// `POST /api/v1/user/ssh-keys` → `201` with the stored key.
pub async fn add(state: AppState, request: Dispatched<TokenOwnerCtx, AddSshKey>) -> Response {
    let w = request.into_write();
    let built = async {
        let key = parse_public_key(&w.input.key)
            .map_err(|e| e.with_context("user", w.proof.id().get()))?;
        let title = w.input.title.clone().unwrap_or_default(); // clone: the body stays with the request
        let added = hosting(&state)?
            .add_ssh_key(&w.proof, &write_request(&w), title, key)
            .await?;
        json(body_of(&added))
    }
    .await;
    respond(&w.ctx, StatusCode::CREATED, built)
}

/// `DELETE /api/v1/user/ssh-keys/{id}` → `204` (also when already gone).
pub async fn delete(state: AppState, request: Dispatched<TokenCtx>) -> Response {
    let w = request.into_write();
    let done = async {
        hosting(&state)?
            .delete_ssh_key(&w.proof, &write_request(&w))
            .await
            .map(|_| ())
    }
    .await;
    match done {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => w.ctx.fail(e).into_response(),
    }
}
