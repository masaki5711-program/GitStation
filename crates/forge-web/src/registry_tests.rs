//! The registry's proof gate (ADR-0015 MR4 musts a–c), the `scoped()`
//! registration rules, the per-word public guard and the manifest fields.

use std::collections::BTreeSet;

use axum::body::Body;
use axum::http::Request;
use forge_auth::{prove, Action, Credential, InstanceCtx, Principal, ProjectCtx};
use forge_core::web_path::{GitRoute, InstanceRoute, RouteWord};
use forge_core::{TailSegments, UserId};
use forge_errors::ErrorCode;
use serde_json::Value;

use super::*;
use crate::app::{AppState, GROUP_SHELL_WORDS, PROJECT_SHELL_WORDS};
use crate::context::Ctx;

type Shell = Dispatched<InstanceCtx>;
type Project = Dispatched<ProjectCtx>;

async fn shell(_: AppState, _: Shell) -> &'static str {
    "shell"
}

async fn project(_: AppState, _: Project) -> &'static str {
    "project"
}

fn admitted() -> Admitted {
    let (parts, body) = Request::new(Body::empty()).into_parts();
    Admitted {
        ctx: Ctx::of(&parts),
        principal: Principal::anonymous(),
        parts,
        body,
        tail: TailSegments::default(),
        canonical: None,
        tombstone: None,
        project: None,
        group: None,
        repository: None,
    }
}

#[test]
fn a_proof_for_another_action_is_refused() {
    // MR4 must c: WebShell and UserCreate proofs have the same type; the
    // gate compares the action at run time, before any handler.
    let proof = prove(&Principal::anonymous(), Action::WebShell, InstanceCtx).expect("public");
    let Err(err) = admit(Action::UserCreate, proof, admitted(), ()) else {
        panic!("a WebShell proof must not admit a UserCreate route");
    };
    assert_eq!(err.code(), ErrorCode::Internal);
    let text = err.to_string();
    assert!(text.contains("reason=proof_action_mismatch"), "{text}");
    assert!(text.contains("route_action=UserCreate"), "{text}");
    assert!(text.contains("proof_action=WebShell"), "{text}");

    let admin = Principal::authenticated(UserId::new(1), true, true, Credential::Session);
    let proof = prove(&admin, Action::UserCreate, InstanceCtx).expect("admin");
    let Err(err) = admit(Action::Metrics, proof, admitted(), ()) else {
        panic!("a UserCreate proof must not admit a Metrics route");
    };
    assert_eq!(err.code(), ErrorCode::Internal);
}

#[test]
fn a_proof_for_the_route_action_is_admitted_with_its_target() {
    let proof = prove(&Principal::anonymous(), Action::WebShell, InstanceCtx).expect("public");
    let Ok(dispatched) = admit(Action::WebShell, proof, admitted(), ()) else {
        panic!("same action");
    };
    assert_eq!(dispatched.proof().action(), Action::WebShell);
    assert!(dispatched.tail().is_empty());
    assert!(dispatched.canonical_path().is_none());
}

fn problems(registry: Registry<AppState>) -> String {
    match registry.into_router() {
        Ok(_) => String::new(),
        Err(e) => {
            assert_eq!(e.code(), ErrorCode::RouteRegistryInvalid);
            e.to_string()
        }
    }
}

#[test]
fn scoped_registrations_that_do_not_fit_are_a_startup_error() {
    let word = ScopedRoute::Word(RouteWord::Raw);
    let cases: Vec<(Registry<AppState>, &str)> = vec![
        (
            Registry::new().scoped(
                RouteScope::Namespace,
                Verb::Get,
                word,
                None,
                Action::WebShell,
                "x",
                shell,
            ),
            "scope_does_not_fit_route",
        ),
        (
            Registry::new().scoped(
                RouteScope::ProjectGit,
                Verb::Get,
                ScopedRoute::Git(GitRoute::InfoRefsUploadPack),
                None,
                Action::RepoRead,
                "x",
                project,
            ),
            "query_key_does_not_fit_route",
        ),
        (
            Registry::new().scoped(
                RouteScope::ProjectGit,
                Verb::Get,
                ScopedRoute::Git(GitRoute::UploadPack),
                None,
                Action::RepoRead,
                "x",
                project,
            ),
            "verb_does_not_fit_route",
        ),
        (
            // A non-public route must decide on a resolved context.
            Registry::new().scoped(
                RouteScope::Project,
                Verb::Get,
                word,
                None,
                Action::RepoRead,
                "x",
                shell,
            ),
            "non_public_route_needs_a_resolved_context",
        ),
        (
            // A public route never resolves.
            Registry::new().scoped(
                RouteScope::Project,
                Verb::Get,
                word,
                None,
                Action::WebShell,
                "x",
                project,
            ),
            "public_route_never_resolves",
        ),
        (
            Registry::new().scoped(
                RouteScope::Group,
                Verb::Get,
                word,
                None,
                Action::RepoRead,
                "x",
                project,
            ),
            "project_context_outside_project_scope",
        ),
        (
            Registry::new()
                .scoped(
                    RouteScope::Project,
                    Verb::Get,
                    word,
                    None,
                    Action::RepoRead,
                    "a",
                    project,
                )
                .scoped(
                    RouteScope::Project,
                    Verb::Get,
                    word,
                    None,
                    Action::RepoRead,
                    "b",
                    project,
                ),
            "duplicate [project] GET raw",
        ),
    ];
    for (registry, want) in cases {
        let text = problems(registry);
        assert!(text.contains(want), "want {want}: {text}");
    }
}

#[test]
fn the_same_word_in_two_scopes_or_two_verbs_is_not_a_duplicate() {
    let settings = ScopedRoute::Word(RouteWord::Settings);
    let registry: Registry<AppState> = Registry::new()
        .scoped(
            RouteScope::Group,
            Verb::Get,
            settings,
            None,
            Action::WebShell,
            "g",
            shell,
        )
        .scoped(
            RouteScope::Project,
            Verb::Get,
            settings,
            None,
            Action::WebShell,
            "p",
            shell,
        )
        .scoped(
            RouteScope::Project,
            Verb::Head,
            settings,
            None,
            Action::WebShell,
            "h",
            shell,
        );
    assert_eq!(problems(registry), "");
}

#[test]
fn a_word_mixing_public_and_non_public_routes_is_a_startup_error() {
    // A mixed word would resolve to the group's shell after authentication
    // without a proof on the group (and ignore `moved`): an existence
    // oracle. Refused in either registration order, for that verb only.
    let raw = ScopedRoute::Word(RouteWord::Raw);
    let public_first: Registry<AppState> = Registry::new()
        .scoped(
            RouteScope::Group,
            Verb::Get,
            raw,
            None,
            Action::WebShell,
            "g",
            shell,
        )
        .scoped(
            RouteScope::Project,
            Verb::Get,
            raw,
            None,
            Action::RepoRead,
            "p",
            project,
        );
    let problems_public_first = problems(public_first);
    assert!(
        problems_public_first.contains("[project] GET raw: word_mixes_public_and_non_public"),
        "{problems_public_first}"
    );
    let non_public_first: Registry<AppState> = Registry::new()
        .scoped(
            RouteScope::Project,
            Verb::Get,
            raw,
            None,
            Action::RepoRead,
            "p",
            project,
        )
        .scoped(
            RouteScope::Group,
            Verb::Get,
            raw,
            None,
            Action::WebShell,
            "g",
            shell,
        );
    let problems_non_public_first = problems(non_public_first);
    assert!(
        problems_non_public_first.contains("[group] GET raw: word_mixes_public_and_non_public"),
        "{problems_non_public_first}"
    );
    // The same word under another verb is another word: no conflict.
    let other_verb: Registry<AppState> = Registry::new()
        .scoped(
            RouteScope::Group,
            Verb::Head,
            raw,
            None,
            Action::WebShell,
            "g",
            shell,
        )
        .scoped(
            RouteScope::Project,
            Verb::Get,
            raw,
            None,
            Action::RepoRead,
            "p",
            project,
        );
    assert!(!other_verb.is_public_word(Verb::Get, raw));
    assert!(other_verb.is_public_word(Verb::Head, raw));
    assert_eq!(problems(other_verb), "");
}

#[test]
fn a_word_is_public_only_when_every_route_with_it_is() {
    // ADR-0015 step 4.
    let raw = ScopedRoute::Word(RouteWord::Raw);
    let non_public: Registry<AppState> = Registry::new().scoped(
        RouteScope::Project,
        Verb::Get,
        raw,
        None,
        Action::RepoRead,
        "p",
        project,
    );
    assert!(!non_public.is_public_word(Verb::Get, raw));
    // Another verb of the same word has no route: not public either.
    assert!(!non_public.is_public_word(Verb::Head, raw));
    let only_public: Registry<AppState> = Registry::new()
        .scoped(
            RouteScope::Group,
            Verb::Get,
            raw,
            None,
            Action::WebShell,
            "g",
            shell,
        )
        .scoped(
            RouteScope::Project,
            Verb::Get,
            raw,
            None,
            Action::WebShell,
            "p",
            shell,
        );
    assert!(only_public.is_public_word(Verb::Get, raw));
}

#[test]
fn every_public_word_of_the_product_is_webshell_only() {
    let registry = crate::app::registry();
    let routes: BTreeSet<(Verb, ScopedRoute)> = registry
        .defs()
        .iter()
        .filter_map(|d| d.route.map(|r| (d.verb, r)))
        .collect();
    for (verb, route) in routes {
        if registry.is_public_word(verb, route) {
            for d in registry
                .defs()
                .iter()
                .filter(|d| d.verb == verb && d.route == Some(route))
            {
                assert_eq!(
                    d.action,
                    Action::WebShell,
                    "{} {}",
                    verb.as_str(),
                    route.name()
                );
            }
        }
    }
}

/// ADR-0015 dispatcher test 1: every reachable `(scope, verb, word)` of the
/// ADR's route table has a RouteDef, GET and HEAD alike, and nothing else
/// is routed (upload-pack since M1 phase 9 MR3, receive-pack since MR4,
/// the content words `raw` / `archive` — `RepoRead` — since M2 MR7).
#[test]
fn every_reachable_scope_verb_word_has_a_route_def() {
    let mut want: BTreeSet<(RouteScope, Verb, ScopedRoute)> = BTreeSet::new();
    use forge_core::web_path::GitRoute;
    for (verb, git) in [
        (Verb::Get, GitRoute::InfoRefsUploadPack),
        (Verb::Head, GitRoute::InfoRefsUploadPack),
        (Verb::Post, GitRoute::UploadPack),
        (Verb::Get, GitRoute::InfoRefsReceivePack),
        (Verb::Head, GitRoute::InfoRefsReceivePack),
        (Verb::Post, GitRoute::ReceivePack),
    ] {
        want.insert((RouteScope::ProjectGit, verb, ScopedRoute::Git(git)));
    }
    for verb in [Verb::Get, Verb::Head] {
        want.insert((RouteScope::Namespace, verb, ScopedRoute::Home));
        want.insert((
            RouteScope::Instance,
            verb,
            ScopedRoute::Instance(InstanceRoute::ProjectPermalink),
        ));
        // M2 MR2: the UI's files (WebShell, public).
        want.insert((
            RouteScope::Instance,
            verb,
            ScopedRoute::Instance(InstanceRoute::Assets),
        ));
        for &w in PROJECT_SHELL_WORDS {
            want.insert((RouteScope::Project, verb, ScopedRoute::Word(w)));
        }
        for &w in GROUP_SHELL_WORDS {
            want.insert((RouteScope::Group, verb, ScopedRoute::Word(w)));
        }
        for w in [RouteWord::Raw, RouteWord::Archive] {
            want.insert((RouteScope::Project, verb, ScopedRoute::Word(w)));
        }
    }
    let have: BTreeSet<(RouteScope, Verb, ScopedRoute)> = crate::app::registry()
        .defs()
        .iter()
        .filter_map(|d| d.route.map(|r| (d.scope, d.verb, r)))
        .collect();
    assert_eq!(have, want);
    // Every word the ADR names for pages is in one of the two lists, except
    // the content words that are RepoRead routes.
    for &w in RouteWord::ALL {
        let routed = PROJECT_SHELL_WORDS.contains(&w);
        assert_eq!(
            routed,
            !matches!(w, RouteWord::Raw | RouteWord::Archive),
            "{}",
            w.as_str()
        );
    }
}

#[test]
fn the_manifest_carries_scope_word_and_query_key() {
    let registry: Registry<AppState> = Registry::new()
        .route(Verb::Get, "/healthz", Action::Health, "h", shell)
        .scoped(
            RouteScope::ProjectGit,
            Verb::Get,
            ScopedRoute::Git(GitRoute::InfoRefsUploadPack),
            Some(("service", "git-upload-pack")),
            Action::RepoRead,
            "advertise",
            project,
        )
        .scoped(
            RouteScope::ProjectGit,
            Verb::Get,
            ScopedRoute::Git(GitRoute::InfoRefsReceivePack),
            Some(("service", "git-receive-pack")),
            Action::RepoPush,
            "advertise push",
            project,
        );
    let list: Value = serde_json::from_str(&routes_json(registry.defs(), &[])).expect("json");
    let entries = list.as_array().expect("array");
    let fixed = entries
        .iter()
        .find(|e| e["path"] == "/healthz")
        .expect("healthz");
    assert_eq!(fixed["scope"], "fixed");
    assert_eq!(fixed["word"], "fixed");
    assert!(fixed.get("query_key").is_none());
    let refs: Vec<&Value> = entries
        .iter()
        .filter(|e| e["path"] == "/{namespace}.git/info/refs")
        .collect();
    assert_eq!(refs.len(), 2, "two routes that differ only in the query");
    let upload = refs
        .iter()
        .find(|e| e["word"] == "info_refs_upload_pack")
        .expect("upload-pack advertisement");
    assert_eq!(upload["scope"], "project_git");
    assert_eq!(
        upload["query_key"],
        serde_json::json!(["service", "git-upload-pack"])
    );
    let push = refs
        .iter()
        .find(|e| e["word"] == "info_refs_receive_pack")
        .expect("receive-pack advertisement");
    assert_eq!(
        push["query_key"],
        serde_json::json!(["service", "git-receive-pack"])
    );
    // OpenAPI describes /api only: the scoped routes are not operations.
    let doc: Value = serde_json::from_str(&openapi_json(registry.defs(), "1")).expect("json");
    let paths = doc["paths"].as_object().expect("paths");
    assert_eq!(paths.keys().collect::<Vec<_>>(), ["/healthz"]);
}

#[test]
fn verbs_round_trip_through_http_methods() {
    for &verb in Verb::ALL {
        let method = Method::from_bytes(verb.as_str().as_bytes()).expect("method");
        assert_eq!(Verb::of(&method), Some(verb));
    }
    assert_eq!(Verb::of(&Method::OPTIONS), None);
    assert!(Verb::Head.is_safe() && Verb::Get.is_safe() && !Verb::Post.is_safe());
}

// ── M1 phase 8b MR6: body routes ──

/// A synthetic body type.
#[derive(Debug, serde::Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct Change {
    #[allow(dead_code)] // read by serde only
    name: String,
}

struct NoBodyLoad;

impl BodyLoader<AppState, Change> for NoBodyLoad {
    type Ctx = InstanceCtx;
    fn load<'a>(
        &'a self,
        _: &'a AppState,
        _: &'a Parts,
        _: &'a Change,
        _: Option<UserId>,
    ) -> LoadFuture<'a, InstanceCtx> {
        Box::pin(async {
            Ok(Lookup::Found {
                ctx: InstanceCtx,
                canonical: None,
                tombstone: None,
                project: None,
                group: None,
            })
        })
    }
}

async fn change(_: AppState, _: Dispatched<InstanceCtx, Change>) -> &'static str {
    "changed"
}

fn body_route(verb: Verb, action: Action, cap: usize) -> Registry<AppState> {
    Registry::new().route_with_body(verb, "/api/v1/x", action, "x", cap, NoBodyLoad, change)
}

/// MR4 musts for body routes: a safe verb, a public action, a cap out of
/// range or a path taken twice is refused at startup; a proof for another
/// action is refused before the handler, body or not.
#[test]
fn body_routes_that_do_not_fit_are_a_startup_error() {
    for (registry, reason) in [
        (
            body_route(Verb::Get, Action::UserSelfRead, 1024),
            "body_route_needs_an_unsafe_verb",
        ),
        (
            body_route(Verb::Head, Action::UserSelfRead, 1024),
            "body_route_needs_an_unsafe_verb",
        ),
        (
            body_route(Verb::Post, Action::Health, 1024),
            "body_route_must_authenticate",
        ),
        (
            body_route(Verb::Post, Action::UserSelfRead, 0),
            "body_limit_out_of_range",
        ),
        (
            body_route(Verb::Post, Action::UserSelfRead, 64 * 1024 + 1),
            "body_limit_out_of_range",
        ),
        (
            body_route(Verb::Post, Action::UserSelfRead, 1024).route(
                Verb::Post,
                "/api/v1/x",
                Action::UserSelfRead,
                "again",
                shell,
            ),
            "duplicate POST /api/v1/x",
        ),
        (
            Registry::new().responds::<Change>(Verb::Get, "/api/v1/none"),
            "responds: no route GET /api/v1/none",
        ),
    ] {
        let text = problems(registry);
        assert!(text.contains(reason), "{reason}: {text}");
    }
    assert_eq!(
        problems(body_route(Verb::Patch, Action::UserSelfRead, 64 * 1024)),
        ""
    );

    let admin = Principal::authenticated(UserId::new(1), true, true, Credential::Session);
    let proof = prove(&admin, Action::UserSelfRead, InstanceCtx).expect("signed in");
    let input = Change {
        name: "synthetic".into(),
    };
    let Err(err) = admit(Action::UserCreate, proof, admitted(), input) else {
        panic!("a UserSelfRead proof must not admit a UserCreate body route");
    };
    assert!(
        err.to_string().contains("reason=proof_action_mismatch"),
        "{err}"
    );
}

/// The OpenAPI document carries a body route's strict schema and cap, a
/// documented response, and `{id}` as a positive integer.
#[test]
fn openapi_documents_bodies_responses_and_typed_ids() {
    let registry = body_route(Verb::Post, Action::UserSelfRead, 4096)
        .route(
            Verb::Get,
            "/api/v1/x/{id}",
            Action::UserSelfRead,
            "one",
            shell,
        )
        .responds::<Change>(Verb::Get, "/api/v1/x/{id}");
    let doc: Value = serde_json::from_str(&openapi_json(registry.defs(), "1")).expect("json");
    let post = &doc["paths"]["/api/v1/x"]["post"];
    assert_eq!(
        post["requestBody"]["content"]["application/json"]["schema"]["$ref"],
        "#/components/schemas/Change"
    );
    assert_eq!(post["x-forge-max-body-bytes"], 4096);
    let get = &doc["paths"]["/api/v1/x/{id}"]["get"];
    assert_eq!(get["parameters"][0]["schema"]["type"], "integer");
    assert_eq!(get["parameters"][0]["schema"]["minimum"], 1);
    assert_eq!(
        get["responses"]["200"]["content"]["application/json"]["schema"]["$ref"],
        "#/components/schemas/Change"
    );
    let change = &doc["components"]["schemas"]["Change"];
    assert_eq!(change["additionalProperties"], false, "{change}");
    assert!(doc["components"].get("x-forge-schema-conflicts").is_none());
}

/// Every product body route and documented response is strict
/// (`deny_unknown_fields` → `additionalProperties: false`), every documented
/// schema has one shape, and the document is the same twice.
#[test]
fn the_product_openapi_is_strict_and_deterministic() {
    let registry = crate::app::registry();
    let first = openapi_json(registry.defs(), "1");
    assert_eq!(first, openapi_json(crate::app::registry().defs(), "1"));
    let doc: Value = serde_json::from_str(&first).expect("json");
    assert!(doc["components"].get("x-forge-schema-conflicts").is_none());
    for (name, schema) in doc["components"]["schemas"].as_object().expect("schemas") {
        if schema["type"] == "object" {
            assert_eq!(schema["additionalProperties"], false, "{name}");
        }
    }
    for def in registry.defs() {
        if let Some((_, cap)) = def.request {
            assert!(cap <= crate::body::MAX_JSON_BODY_BYTES, "{}", def.path);
        }
    }
    let user = &doc["paths"]["/api/v1/user"]["get"];
    assert_eq!(user["x-forge-action"], "UserSelfRead");
    assert_eq!(
        user["responses"]["200"]["content"]["application/json"]["schema"]["$ref"],
        "#/components/schemas/UserSelf"
    );
    let project = &doc["paths"]["/api/v1/projects/{id}"]["get"];
    assert_eq!(project["parameters"][0]["schema"]["type"], "integer");
    assert_eq!(
        project["responses"]["200"]["content"]["application/json"]["schema"]["$ref"],
        "#/components/schemas/ProjectBody"
    );
}
