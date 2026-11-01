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

const HOSTING: &str = "feat/client-gstn-hosting";

pub const API_ROUTES: &[(&str, &str, Coverage)] = &[
    ("GET", "/api/v1/user", Coverage::Command("auth status")),
    ("GET", "/api/v1/groups", Coverage::Planned(HOSTING)),
    ("POST", "/api/v1/groups", Coverage::Planned(HOSTING)),
    ("GET", "/api/v1/groups/{id}", Coverage::Planned(HOSTING)),
    ("PATCH", "/api/v1/groups/{id}", Coverage::Planned(HOSTING)),
    (
        "GET",
        "/api/v1/groups/{id}/members",
        Coverage::Planned(HOSTING),
    ),
    (
        "POST",
        "/api/v1/groups/{id}/members",
        Coverage::Planned(HOSTING),
    ),
    (
        "PATCH",
        "/api/v1/groups/{id}/members/{user_id}",
        Coverage::Planned(HOSTING),
    ),
    (
        "DELETE",
        "/api/v1/groups/{id}/members/{user_id}",
        Coverage::Planned(HOSTING),
    ),
    (
        "POST",
        "/api/v1/groups/{id}/subgroups",
        Coverage::Planned(HOSTING),
    ),
    (
        "GET",
        "/api/v1/namespaces/resolve",
        Coverage::Planned(HOSTING),
    ),
    ("GET", "/api/v1/projects", Coverage::Planned(HOSTING)),
    ("POST", "/api/v1/projects", Coverage::Planned(HOSTING)),
    ("GET", "/api/v1/projects/{id}", Coverage::Planned(HOSTING)),
    ("PATCH", "/api/v1/projects/{id}", Coverage::Planned(HOSTING)),
    (
        "GET",
        "/api/v1/projects/{id}/members",
        Coverage::Planned(HOSTING),
    ),
    (
        "POST",
        "/api/v1/projects/{id}/members",
        Coverage::Planned(HOSTING),
    ),
    (
        "PATCH",
        "/api/v1/projects/{id}/members/{user_id}",
        Coverage::Planned(HOSTING),
    ),
    (
        "DELETE",
        "/api/v1/projects/{id}/members/{user_id}",
        Coverage::Planned(HOSTING),
    ),
    (
        "GET",
        "/api/v1/projects/{id}/protected-refs",
        Coverage::Planned(HOSTING),
    ),
    (
        "POST",
        "/api/v1/projects/{id}/protected-refs",
        Coverage::Planned(HOSTING),
    ),
    (
        "PATCH",
        "/api/v1/projects/{id}/protected-refs/{rule_id}",
        Coverage::Planned(HOSTING),
    ),
    (
        "DELETE",
        "/api/v1/projects/{id}/protected-refs/{rule_id}",
        Coverage::Planned(HOSTING),
    ),
    (
        "PUT",
        "/api/v1/projects/{id}/visibility",
        Coverage::Planned(HOSTING),
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
