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
        repository: None,
    }
}

#[test]
fn a_proof_for_another_action_is_refused() {
    // MR4 must c: WebShell and UserCreate proofs have the same type; the
    // gate compares the action at run time, before any handler.
    let proof = prove(&Principal::anonymous(), Action::WebShell, InstanceCtx).expect("public");
    let Err(err) = admit(Action::UserCreate, proof, admitted()) else {
        panic!("a WebShell proof must not admit a UserCreate route");
    };
    assert_eq!(err.code(), ErrorCode::Internal);
    let text = err.to_string();
    assert!(text.contains("reason=proof_action_mismatch"), "{text}");
    assert!(text.contains("route_action=UserCreate"), "{text}");
    assert!(text.contains("proof_action=WebShell"), "{text}");

    let admin = Principal::authenticated(UserId::new(1), true, true, Credential::Session);
    let proof = prove(&admin, Action::UserCreate, InstanceCtx).expect("admin");
    let Err(err) = admit(Action::Metrics, proof, admitted()) else {
        panic!("a UserCreate proof must not admit a Metrics route");
    };
    assert_eq!(err.code(), ErrorCode::Internal);
}

#[test]
fn a_proof_for_the_route_action_is_admitted_with_its_target() {
    let proof = prove(&Principal::anonymous(), Action::WebShell, InstanceCtx).expect("public");
    let Ok(dispatched) = admit(Action::WebShell, proof, admitted()) else {
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
/// is routed yet (`raw` / `archive` and receive-pack wait for their phase;
/// upload-pack is routed since M1 phase 9 MR3).
#[test]
fn every_reachable_scope_verb_word_has_a_route_def() {
    let mut want: BTreeSet<(RouteScope, Verb, ScopedRoute)> = BTreeSet::new();
    use forge_core::web_path::GitRoute;
    for (verb, git) in [
        (Verb::Get, GitRoute::InfoRefsUploadPack),
        (Verb::Head, GitRoute::InfoRefsUploadPack),
        (Verb::Post, GitRoute::UploadPack),
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
        for &w in PROJECT_SHELL_WORDS {
            want.insert((RouteScope::Project, verb, ScopedRoute::Word(w)));
        }
        for &w in GROUP_SHELL_WORDS {
            want.insert((RouteScope::Group, verb, ScopedRoute::Word(w)));
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
