//! C-1: every REST route is reachable from gstn or listed as planned.
//!
//! `docs/authz/routes.generated.json` is the route registry (generated from
//! the server's). The table below states, per `/api/v1` route, how gstn covers
//! it. The test `every_api_route_is_covered_or_planned` fails when the server
//! gains a route that is not in this table (the C-1 gate), or when the table
//! names a route the server no longer has.
//!
//! Every route is already callable through `gstn api` (C-6); `Command` means a
//! dedicated command exists, `Planned` names the series that adds it, and
//! `BrowserOnly` marks a route only a web session can use (gstn holds a
//! token, so it has no command for it; M2 MR3).

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Coverage {
    /// A dedicated command exists (name in the registry).
    Command(&'static str),
    /// Only via `gstn api` for now; the dedicated command arrives in the series.
    Planned(&'static str),
    /// A browser-session-only route (`authorize()` refuses every token):
    /// gstn never covers it; the text says why.
    BrowserOnly(&'static str),
}

pub const API_ROUTES: &[(&str, &str, Coverage)] = &[
    ("GET", "/api/v1/user", Coverage::Command("auth status")),
    (
        "GET",
        "/api/v1/session",
        Coverage::BrowserOnly("the web session and its CSRF token; tokens are refused (ADR-0021)"),
    ),
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
        Coverage::Command("group move"),
    ),
    (
        "GET",
        "/api/v1/namespaces/resolve",
        Coverage::Command("namespace resolve"),
    ),
    (
        "POST",
        "/api/v1/namespaces/tombstones/release",
        Coverage::Command("namespace release-path"),
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
        Coverage::Command("project move"),
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
        "/api/v1/projects/{id}/repository/refs",
        Coverage::Command("browse refs"),
    ),
    (
        "GET",
        "/api/v1/projects/{id}/repository/resolve",
        Coverage::Command("browse resolve"),
    ),
    (
        "GET",
        "/api/v1/projects/{id}/repository/tree",
        Coverage::Command("browse tree"),
    ),
    (
        "GET",
        "/api/v1/projects/{id}/repository/blob",
        Coverage::Command("browse file"),
    ),
    (
        "GET",
        "/api/v1/projects/{id}/repository/commits",
        Coverage::Command("browse commits"),
    ),
    (
        "GET",
        "/api/v1/projects/{id}/repository/commits/{oid}",
        Coverage::Command("browse commit"),
    ),
    (
        "GET",
        "/api/v1/projects/{id}/repository/compare",
        Coverage::Command("browse compare"),
    ),
    (
        "GET",
        "/api/v1/projects/{id}/repository/blame",
        Coverage::Command("browse blame"),
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
        Coverage::Command("protected-ref edit"),
    ),
    (
        "DELETE",
        "/api/v1/projects/{id}/protected-refs/{rule_id}",
        Coverage::Command("protected-ref remove"),
    ),
    (
        "PUT",
        "/api/v1/projects/{id}/visibility",
        Coverage::Command("project visibility"),
    ),
    (
        "POST",
        "/api/v1/users/{id}/rename",
        Coverage::Command("user rename"),
    ),
];

/// Routes outside `/api/v1` and how they are covered.
pub fn non_api_coverage(path: &str) -> &'static str {
    if path == "/healthz" || path == "/readyz" {
        "api (gstn api GET /healthz)"
    } else if path.starts_with("/-/auth/") {
        // M2 MR4: the browser sign-in (OIDC redirects and cookies).
        "browser only (OIDC sign-in; gstn uses a personal access token, ADR-0021)"
    } else if path.contains(".git/") {
        "git (auth git-credential makes plain git clone/push work)"
    } else {
        "web (browser pages; not an API)"
    }
}
