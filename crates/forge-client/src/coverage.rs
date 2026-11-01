//! C-1: every REST route is reachable from gstn or listed as planned.
//!
//! `docs/authz/routes.generated.json` is the route registry (generated from
//! the server's). The table below states, per `/api/v1` route, how gstn covers
//! it. The test `every_api_route_is_covered_or_planned` fails when the server
//! gains a route that is not in this table (the C-1 gate), or when the table
//! names a route the server no longer has.
//!
//! Every route is already callable through `gstn api` (C-6); `Command` means a
//! dedicated command exists, `Planned` names the series that adds it.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Coverage {
    /// A dedicated command exists (name in the registry).
    Command(&'static str),
    /// Only via `gstn api` for now; the dedicated command arrives in the series.
    Planned(&'static str),
}

/// The series that adds the rest of the hosting routes (a protected ref's
/// role change, a project's visibility), with namespace move / transfer.
const HOSTING_MORE: &str = "feat/client-gstn-hosting-more";

/// The namespace moves of step D5 MR5 (project / group transfer and rename,
/// user rename, tombstone release): `gstn repo rename|transfer` and the
/// group / namespace commands arrive with the hosting-more series.
const NAMESPACE_MOVES: &str = HOSTING_MORE;

pub const API_ROUTES: &[(&str, &str, Coverage)] = &[
    ("GET", "/api/v1/user", Coverage::Command("auth status")),
    ("GET", "/api/v1/groups", Coverage::Command("group list")),
    ("POST", "/api/v1/groups", Coverage::Command("group create")),
    (
        "GET",
        "/api/v1/groups/{id}",
        Coverage::Command("group view"),
    ),
    (
        "PATCH",
        "/api/v1/groups/{id}",
        Coverage::Command("group edit"),
    ),
    (
        "GET",
        "/api/v1/groups/{id}/members",
        Coverage::Command("member list"),
    ),
    (
        "POST",
        "/api/v1/groups/{id}/members",
        Coverage::Command("member add"),
    ),
    (
        "PATCH",
        "/api/v1/groups/{id}/members/{user_id}",
        Coverage::Command("member set-role"),
    ),
    (
        "DELETE",
        "/api/v1/groups/{id}/members/{user_id}",
        Coverage::Command("member remove"),
    ),
    (
        "POST",
        "/api/v1/groups/{id}/subgroups",
        Coverage::Command("group create"),
    ),
    (
        "POST",
        "/api/v1/groups/{id}/transfer",
        Coverage::Planned(NAMESPACE_MOVES),
    ),
    (
        "GET",
        "/api/v1/namespaces/resolve",
        Coverage::Command("namespace resolve"),
    ),
    (
        "POST",
        "/api/v1/namespaces/tombstones/release",
        Coverage::Planned(NAMESPACE_MOVES),
    ),
    ("GET", "/api/v1/projects", Coverage::Command("project list")),
    (
        "POST",
        "/api/v1/projects",
        Coverage::Command("project create"),
    ),
    (
        "GET",
        "/api/v1/projects/{id}",
        Coverage::Command("project view"),
    ),
    (
        "PATCH",
        "/api/v1/projects/{id}",
        Coverage::Command("project edit"),
    ),
    (
        "DELETE",
        "/api/v1/projects/{id}",
        Coverage::Command("project delete"),
    ),
    (
        "POST",
        "/api/v1/projects/{id}/restore",
        Coverage::Command("project restore"),
    ),
    (
        "POST",
        "/api/v1/projects/{id}/transfer",
        Coverage::Planned(NAMESPACE_MOVES),
    ),
    (
        "GET",
        "/api/v1/projects/{id}/members",
        Coverage::Command("member list"),
    ),
    (
        "POST",
        "/api/v1/projects/{id}/members",
        Coverage::Command("member add"),
    ),
    (
        "PATCH",
        "/api/v1/projects/{id}/members/{user_id}",
        Coverage::Command("member set-role"),
    ),
    (
        "DELETE",
        "/api/v1/projects/{id}/members/{user_id}",
        Coverage::Command("member remove"),
    ),
    (
        "GET",
        "/api/v1/projects/{id}/protected-refs",
        Coverage::Command("protected-ref list"),
    ),
    (
        "POST",
        "/api/v1/projects/{id}/protected-refs",
        Coverage::Command("protected-ref add"),
    ),
    (
        "PATCH",
        "/api/v1/projects/{id}/protected-refs/{rule_id}",
        Coverage::Planned(HOSTING_MORE),
    ),
    (
        "DELETE",
        "/api/v1/projects/{id}/protected-refs/{rule_id}",
        Coverage::Command("protected-ref remove"),
    ),
    (
        "PUT",
        "/api/v1/projects/{id}/visibility",
        Coverage::Planned(HOSTING_MORE),
    ),
    (
        "POST",
        "/api/v1/users/{id}/rename",
        Coverage::Planned(NAMESPACE_MOVES),
    ),
];

/// Routes outside `/api/v1` and how they are covered.
pub fn non_api_coverage(path: &str) -> &'static str {
    if path == "/healthz" || path == "/readyz" {
        "api (gstn api GET /healthz)"
    } else if path.contains(".git/") {
        "git (auth git-credential makes plain git clone/push work)"
    } else {
        "web (browser pages; not an API)"
    }
}
