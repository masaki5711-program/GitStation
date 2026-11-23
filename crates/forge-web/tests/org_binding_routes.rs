#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! Org bindings (ADR-0038 §2, §5) through the product router, against the
//! development database (ADR-0011) with the real loaders and forge-hosting's
//! commands:
//!
//! - the group's Owner binds a department (with its descendants) in a
//!   session: a person of a team below it reads the private project in the
//!   group (Developer, inherited) and is listed as brought in by the
//!   binding; removing the binding takes it away again (404);
//! - a token is refused (`FG-AUTH-0002`), a Maintainer too (`FG-AUTH-0003`),
//!   a role above Developer is `FG-AUTH-0031`, a unit the master's copy
//!   lacks `FG-AUTH-0030`, a duplicate `FG-AUTH-0029`, a binding of another
//!   target `404`;
//! - `GET /api/v1/org-units` finds the units for the picker.
//!
//! Synthetic data only.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use forge_auth::{Credential, Principal, Scope};
use forge_core::{CursorKeys, RequestId, Role, UserId, Visibility};
use forge_db::change::{Actor, ChangeContext};
use forge_db::groups::{self, CreatedGroup, NewGroup};
use forge_db::org_master::{
    replace, LocalizedName, MasterPosition, MasterUnit, ReplacePolicy, UnitLevel,
};
use forge_db::org_profiles::{record, Observed, OrgSnapshot, OrgUnit};
use forge_db::projects::{self, CreatedProject, NewProject};
use forge_db::test_support::FreshDb;
use forge_db::users::{self, NewUser};
use forge_db::Db;
use forge_hosting::Hosting;
use forge_web::{AppConfig, AppState, Lists, Lookups};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

struct NoTokens;

impl forge_core::Authenticator for NoTokens {
    fn authenticate<'a>(
        &'a self,
        _: &'a RequestId,
        _: &'a str,
    ) -> forge_core::authenticator::AuthFuture<'a> {
        Box::pin(async {
            Err(forge_errors::ForgeError::new(
                forge_errors::ErrorCode::Unauthenticated,
            ))
        })
    }
}

/// A synthetic key (never a real one).
fn keys() -> Arc<CursorKeys> {
    Arc::new(CursorKeys::parse(&format!("7 {}\n", "5a".repeat(32))).expect("keys"))
}

fn router(db: &Db) -> axum::Router {
    let state = AppState::new(
        "test",
        Vec::new(),
        Arc::new(NoTokens),
        Lookups {
            namespaces: Arc::new(db.clone()),
            projects: Arc::new(db.clone()),
        },
    )
    .with_lists(Lists::new(
        Arc::new(db.clone()) as Arc<dyn forge_core::ProjectPageLoader>,
        Arc::new(db.clone()) as Arc<dyn forge_core::GroupPageLoader>,
        keys(),
    ))
    .with_hosting(Hosting::new(db.clone()));
    forge_web::app(state, AppConfig::default()).expect("router")
}

fn ctx() -> ChangeContext {
    ChangeContext::new(Actor::System, RequestId::generate())
}

async fn user(t: &FreshDb, name: &str) -> UserId {
    users::create(
        &t.db,
        &ctx(),
        &NewUser::human(name, name, &format!("{name}@example.test")),
    )
    .await
    .expect("user")
    .user_id
}

fn unit(code: &str, level: UnitLevel, parent: Option<&str>) -> MasterUnit {
    MasterUnit {
        plant_code: "P001".to_owned(),
        code: code.to_owned(),
        level,
        parent_code: parent.map(str::to_owned),
        name: LocalizedName {
            ja: format!("名前 {code}"),
            en: Some(format!("Name {code}")),
            id: None,
        },
        cost_center: None,
        head_staff_id: None,
        valid_from: None,
        version: 1,
    }
}

async fn sync(t: &FreshDb) {
    let units = [
        unit("DIV-1", UnitLevel::Division, None),
        unit("DEP-1", UnitLevel::Department, Some("DIV-1")),
        unit("SEC-1", UnitLevel::Section, Some("DEP-1")),
        unit("TEAM-1", UnitLevel::Team, Some("SEC-1")),
        unit("DEP-2", UnitLevel::Department, Some("DIV-1")),
    ];
    let positions = [MasterPosition {
        plant_code: "P001".to_owned(),
        code: "staff".to_owned(),
        name: LocalizedName {
            ja: "担当".to_owned(),
            en: None,
            id: None,
        },
        rank: 15,
        sort_order: 1,
        active: true,
        version: 1,
    }];
    replace(
        &t.db,
        &ctx(),
        &units,
        &positions,
        ReplacePolicy {
            allow_large_shrink: false,
        },
    )
    .await
    .expect("sync");
}

async fn in_unit(t: &FreshDb, who: UserId, code: &str) {
    let snap = OrgSnapshot {
        org_units: vec![OrgUnit {
            code: code.to_owned(),
            primary: true,
        }],
        positions: vec!["staff".to_owned()],
        position_rank: Some(15),
        plants: vec!["P001".to_owned()],
        plant_code: Some("P001".to_owned()),
    };
    let _ = record(
        &t.db,
        &ctx(),
        who,
        "http://127.0.0.1:58080/realms/synthetic",
        Observed::Valid(&snap),
    )
    .await
    .expect("snapshot");
}

struct Answer {
    status: StatusCode,
    json: Value,
}

async fn call(
    router: axum::Router,
    method: &str,
    uri: &str,
    who: Principal,
    body: Option<Value>,
) -> Answer {
    let builder = Request::builder().method(method).uri(uri);
    let mut req = match body {
        Some(b) => builder
            .header("content-type", "application/json")
            .body(Body::from(b.to_string())),
        None => builder.body(Body::empty()),
    }
    .expect("request");
    req.extensions_mut().insert(who);
    let res = router.oneshot(req).await.expect("infallible");
    let status = res.status();
    let bytes = res.into_body().collect().await.expect("body").to_bytes();
    Answer {
        status,
        json: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    }
}

fn session(user: UserId) -> Principal {
    Principal::authenticated(
        user,
        false,
        true,
        Credential::Session,
        forge_auth::AccountKind::Human,
    )
}

fn token(user: UserId) -> Principal {
    Principal::authenticated(
        user,
        false,
        true,
        Credential::Token(vec![Scope::Api]),
        forge_auth::AccountKind::Human,
    )
}

fn code(a: &Answer) -> &str {
    a.json["error_code"].as_str().unwrap_or("")
}

struct World {
    alice: UserId,
    maint: UserId,
    carol: UserId,
    acme: CreatedGroup,
    app: CreatedProject,
}

/// acme (internal; alice owns, maint is a Maintainer) ⊃ app (private);
/// carol is in TEAM-1 (below DEP-1) and holds no membership.
async fn world(t: &FreshDb) -> World {
    sync(t).await;
    let alice = user(t, "alice").await;
    let maint = user(t, "maint").await;
    let carol = user(t, "carol").await;
    in_unit(t, carol, "TEAM-1").await;
    let acme = groups::create(
        &t.db,
        &ctx(),
        &NewGroup {
            visibility: Visibility::Internal,
            segment: "acme".to_owned(),
            parent: None,
            display_name: "acme".to_owned(),
            description: String::new(),
            owner: alice,
        },
    )
    .await
    .expect("group");
    groups::add_member(&t.db, &ctx(), acme.group_id, maint, Role::Maintainer)
        .await
        .expect("maint joins");
    let app = projects::create(
        &t.db,
        &ctx(),
        &NewProject {
            visibility: Visibility::Private,
            ..NewProject::new("app", acme.namespace_id, "app", alice)
        },
    )
    .await
    .expect("project");
    World {
        alice,
        maint,
        carol,
        acme,
        app,
    }
}

#[tokio::test]
async fn the_owner_binds_a_department_and_its_people_get_developer_below() {
    let t = FreshDb::migrated().await;
    let w = world(&t).await;
    let r = || router(&t.db);
    let bindings = format!("/api/v1/groups/{}/org-bindings", w.acme.group_id.get());
    let project = format!("/api/v1/projects/{}", w.app.project_id.get());

    // Before: carol does not see the private project.
    let hidden = call(r(), "GET", &project, session(w.carol), None).await;
    assert_eq!(hidden.status, StatusCode::NOT_FOUND, "{}", hidden.json);

    let ask = json!({
        "plant_code": "P001", "org_code": "DEP-1", "include_descendants": true,
        "role": "developer", "reason": "synthetic"
    });
    let made = call(r(), "POST", &bindings, session(w.alice), Some(ask.clone())).await;
    assert_eq!(made.status, StatusCode::CREATED, "{}", made.json);
    assert_eq!(made.json["role"], "developer");
    assert_eq!(made.json["membership"], "any");
    assert_eq!(made.json["unit"]["name_ja"], "名前 DEP-1");
    let id = made.json["id"].as_i64().expect("id");

    let seen = call(r(), "GET", &project, session(w.carol), None).await;
    assert_eq!(seen.status, StatusCode::OK, "{}", seen.json);

    let listed = call(r(), "GET", &bindings, session(w.carol), None).await;
    assert_eq!(listed.status, StatusCode::OK, "{}", listed.json);
    assert_eq!(listed.json["bindings"].as_array().map(Vec::len), Some(1));
    assert_eq!(listed.json["members"][0]["username"], "carol");
    assert_eq!(listed.json["members"][0]["role"], "developer");
    assert_eq!(listed.json["viewer"]["can_manage"], false);
    let owner_view = call(r(), "GET", &bindings, session(w.alice), None).await;
    assert_eq!(owner_view.json["viewer"]["can_manage"], true);

    // The same binding again is a duplicate.
    let again = call(r(), "POST", &bindings, session(w.alice), Some(ask)).await;
    assert_eq!(
        again.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        again.json
    );
    assert_eq!(code(&again), "FG-AUTH-0029");

    // Another target's id is not found; then it is removed.
    let other = format!(
        "/api/v1/projects/{}/org-bindings/{id}",
        w.app.project_id.get()
    );
    let wrong = call(r(), "DELETE", &other, session(w.alice), None).await;
    assert_eq!(wrong.status, StatusCode::NOT_FOUND, "{}", wrong.json);
    let gone = call(
        r(),
        "DELETE",
        &format!("{bindings}/{id}"),
        session(w.alice),
        None,
    )
    .await;
    assert_eq!(gone.status, StatusCode::NO_CONTENT, "{}", gone.json);
    let hidden = call(r(), "GET", &project, session(w.carol), None).await;
    assert_eq!(hidden.status, StatusCode::NOT_FOUND, "{}", hidden.json);
    t.drop_now().await;
}

#[tokio::test]
async fn binding_changes_are_the_owners_in_a_session_and_capped_at_developer() {
    let t = FreshDb::migrated().await;
    let w = world(&t).await;
    let r = || router(&t.db);
    let bindings = format!("/api/v1/projects/{}/org-bindings", w.app.project_id.get());
    let ask = |role: &str, org: &str| json!({"plant_code": "P001", "org_code": org, "role": role, "include_descendants": false});

    let by_token = call(
        r(),
        "POST",
        &bindings,
        token(w.alice),
        Some(ask("reporter", "SEC-1")),
    )
    .await;
    assert_eq!(by_token.status, StatusCode::FORBIDDEN, "{}", by_token.json);
    assert_eq!(code(&by_token), "FG-AUTH-0002");

    let by_maintainer = call(
        r(),
        "POST",
        &bindings,
        session(w.maint),
        Some(ask("reporter", "SEC-1")),
    )
    .await;
    assert_eq!(
        by_maintainer.status,
        StatusCode::FORBIDDEN,
        "{}",
        by_maintainer.json
    );
    assert_eq!(code(&by_maintainer), "FG-AUTH-0003");

    for role in ["maintainer", "owner"] {
        let above = call(
            r(),
            "POST",
            &bindings,
            session(w.alice),
            Some(ask(role, "SEC-1")),
        )
        .await;
        assert_eq!(
            above.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{}",
            above.json
        );
        assert_eq!(code(&above), "FG-AUTH-0031");
    }
    let unknown = call(
        r(),
        "POST",
        &bindings,
        session(w.alice),
        Some(ask("reporter", "NOPE-1")),
    )
    .await;
    assert_eq!(
        unknown.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        unknown.json
    );
    assert_eq!(code(&unknown), "FG-AUTH-0030");

    // A stranger to the private project learns nothing.
    let stranger = call(r(), "GET", &bindings, session(w.carol), None).await;
    assert_eq!(stranger.status, StatusCode::NOT_FOUND, "{}", stranger.json);

    // The picker.
    let units = call(
        r(),
        "GET",
        "/api/v1/org-units?search=sec&limit=5",
        token(w.carol),
        None,
    )
    .await;
    assert_eq!(units.status, StatusCode::OK, "{}", units.json);
    assert_eq!(units.json["items"][0]["unit"]["code"], "SEC-1");
    assert_eq!(units.json["items"][0]["parent_code"], "DEP-1");
    let bad = call(
        r(),
        "GET",
        "/api/v1/org-units?limit=99",
        token(w.carol),
        None,
    )
    .await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST, "{}", bad.json);
    t.drop_now().await;
}
