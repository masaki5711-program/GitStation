//! The signed-in person's notification settings (M3 N1, ADR-0016 Amendment
//! N1): `GET|PUT /api/v1/user/notification-preferences`.
//!
//! - `GET` (`NotificationPreferencesRead`, `read_api`): mail on or off, the
//!   language of the mail (`null`: the instance default) and the kinds
//!   muted; `kinds` lists every kind there is, so a screen or a script
//!   needs no second source.
//! - `PUT` (`NotificationPreferencesWrite`, `api`): replace the settings as
//!   a whole. Muted kinds are sorted and de-duplicated; an unknown language
//!   or kind is `FG-WEB-0006 field=language|muted_kinds` and changes
//!   nothing.
//!
//! The route has no user in it: it is always the caller's own, and
//! `authorize()` answers a settings resource of anyone else like an absent
//! one — administrators included.

use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use forge_auth::NotificationPreferencesCtx;
use forge_core::{NotificationKind, UserId};
use forge_errors::{ErrorCode, ForgeError, Lang};
use forge_notify::Preferences;
use serde::{Deserialize, Serialize};

use crate::app::AppState;
use crate::body::request_invalid;
use crate::registry::{BodyLoader, Dispatched, LoadFuture, Loader, Lookup};
use crate::routes::write::{bounded, json, no_store};

/// `GET` / `PUT` the caller's settings.
pub const PREFERENCES: &str = "/api/v1/user/notification-preferences";
/// The largest body (it holds three words at most).
pub const MAX_BODY: usize = 1024;

/// `PUT /api/v1/user/notification-preferences`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PreferencesInput {
    /// `false` stops every mail.
    pub email_enabled: bool,
    /// `ja`, `en` or `id`; `null` (or absent) for the instance default.
    pub language: Option<String>,
    /// Kinds to stop (`GET`'s `kinds` lists them).
    pub muted_kinds: Vec<String>,
}

/// The caller's settings.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PreferencesBody {
    pub email_enabled: bool,
    #[schema(required = true)]
    pub language: Option<String>,
    pub muted_kinds: Vec<String>,
    /// Every kind of notification there is.
    pub kinds: Vec<String>,
}

fn body(prefs: &Preferences) -> PreferencesBody {
    PreferencesBody {
        email_enabled: prefs.email_enabled,
        language: prefs.language.map(|l| l.as_str().to_owned()),
        muted_kinds: prefs.muted.iter().map(|k| k.as_str().to_owned()).collect(),
        kinds: NotificationKind::ALL
            .iter()
            .map(|k| k.as_str().to_owned())
            .collect(),
    }
}

/// The input as settings; an unknown language or kind is a field error.
fn parse(input: &PreferencesInput) -> Result<Preferences, ForgeError> {
    let invalid = |field: &'static str, rule: &'static str| {
        request_invalid("field_invalid")
            .with_context("field", field)
            .with_context("rule", rule)
    };
    let language = match input.language.as_deref() {
        None => None,
        // Exactly the three codes: `Lang::parse` also takes `id-ID` and `in`.
        Some(tag @ ("ja" | "en" | "id")) => Lang::parse(tag),
        Some(_) => return Err(invalid("language", "unknown_language")),
    };
    if input.muted_kinds.len() > NotificationKind::ALL.len() * 2 {
        return Err(invalid("muted_kinds", "too_many"));
    }
    let mut muted = input
        .muted_kinds
        .iter()
        .map(|k| NotificationKind::parse(k).ok_or_else(|| invalid("muted_kinds", "unknown_kind")))
        .collect::<Result<Vec<_>, _>>()?;
    muted.sort_unstable();
    muted.dedup();
    Ok(Preferences {
        email_enabled: input.email_enabled,
        language,
        muted,
    })
}

fn settings(state: &AppState) -> Result<&forge_notify::Settings, ForgeError> {
    state.notifications.as_deref().ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "notifications_unconfigured")
    })
}

/// The caller (the registry's guard admitted an authenticated one).
fn me(viewer: Option<UserId>) -> Result<UserId, ForgeError> {
    viewer.ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "notification_route_without_user")
    })
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

/// `GET`: the caller's own settings.
pub struct OwnPreferences;

impl Loader<AppState> for OwnPreferences {
    type Ctx = NotificationPreferencesCtx;
    fn load<'a>(
        &'a self,
        _: &'a AppState,
        _: &'a Parts,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, NotificationPreferencesCtx> {
        Box::pin(async move { Ok(found(NotificationPreferencesCtx::new(me(viewer)?))) })
    }
}

/// `PUT`: the caller's own settings; the body is checked after the decision.
pub struct OwnPreferencesChange;

impl BodyLoader<AppState, PreferencesInput> for OwnPreferencesChange {
    type Ctx = NotificationPreferencesCtx;
    fn load<'a>(
        &'a self,
        _: &'a AppState,
        _: &'a Parts,
        _: &'a PreferencesInput,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, NotificationPreferencesCtx> {
        Box::pin(async move { Ok(found(NotificationPreferencesCtx::new(me(viewer)?))) })
    }
}

/// `GET /api/v1/user/notification-preferences` → `200`.
pub async fn read(state: AppState, request: Dispatched<NotificationPreferencesCtx>) -> Response {
    // The request (its body is not Sync) stays out of the await.
    let w = request.into_write();
    let built = async {
        let prefs = bounded(
            &state,
            "notification_preferences_read",
            settings(&state)?.read(&w.proof),
        )
        .await?;
        json(body(&prefs))
    }
    .await;
    match built {
        Ok(value) => no_store(StatusCode::OK, value),
        Err(e) => w.ctx.fail(e).into_response(),
    }
}

/// `PUT /api/v1/user/notification-preferences` → `200` with what is stored.
pub async fn replace(
    state: AppState,
    request: Dispatched<NotificationPreferencesCtx, PreferencesInput>,
) -> Response {
    let w = request.into_write();
    let built = async {
        let wanted = parse(&w.input)?;
        let stored = bounded(
            &state,
            "notification_preferences_write",
            settings(&state)?.replace(&w.proof, &wanted),
        )
        .await?;
        json(body(&stored))
    }
    .await;
    match built {
        Ok(value) => no_store(StatusCode::OK, value),
        Err(e) => w.ctx.fail(e).into_response(),
    }
}
