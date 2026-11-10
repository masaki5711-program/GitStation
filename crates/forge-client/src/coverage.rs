//! C-1: every REST route is reachable from gstn or listed as planned.
//!
//! `docs/authz/routes.generated.json` is the route registry (generated from
//! the server's). The table below states, per `/api/v1` route, how gstn covers
//! it. The test `every_api_route_is_covered_or_planned` fails when the server
//! gains a route that is not in this table (the C-1 gate), or when the table
//! names a route the server no longer has.
//!
//! Commands that call no route at all are listed in [`LOCAL_COMMANDS`] with
//! why (e.g. `doc fmt` / `doc lint`, plan 13 stage 2: they work on the
//! working tree and never contact the server); the test
//! `commands_without_a_route_are_local` keeps that list exact.
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
    /// A route for another system's server, never a person's CLI (M3 S2:
    /// kanflow's claim of an act-as assertion, ADR-0025 §9): gstn has no
    /// command; the text says why.
    NotForCli(&'static str),
}

pub const API_ROUTES: &[(&str, &str, Coverage)] = &[
    ("GET", "/api/v1/user", Coverage::Command("auth status")),
    // M2 MR6: tokens, sessions, users.
    ("GET", "/api/v1/user/tokens", Coverage::Command("token list")),
    (
        "POST",
        "/api/v1/user/tokens",
        Coverage::BrowserOnly("sessions only: a token never creates a token (FG-AUTH-0002 session_required); create one in the browser"),
    ),
    (
        "DELETE",
        "/api/v1/user/tokens/{id}",
        Coverage::Command("token revoke"),
    ),
    // M3 N1: your own notification settings.
    (
        "GET",
        "/api/v1/user/notification-preferences",
        Coverage::Command("notifications get"),
    ),
    (
        "PUT",
        "/api/v1/user/notification-preferences",
        Coverage::Command("notifications set"),
    ),
    // M3 U4: your in-app notifications.
    (
        "GET",
        "/api/v1/user/notifications",
        Coverage::Command("notifications list"),
    ),
    (
        "GET",
        "/api/v1/user/notifications/unread-count",
        Coverage::Command("notifications count"),
    ),
    (
        "POST",
        "/api/v1/user/notifications/{id}/read",
        Coverage::Command("notifications read"),
    ),
    (
        "POST",
        "/api/v1/user/notifications/read-all",
        Coverage::Command("notifications read"),
    ),
    (
        "GET",
        "/api/v1/user/sessions",
        Coverage::BrowserOnly("your web sessions; tokens are refused (ADR-0021 §17)"),
    ),
    (
        "DELETE",
        "/api/v1/user/sessions/{public_id}",
        Coverage::BrowserOnly("ending one of your web sessions; tokens are refused (ADR-0021 §17)"),
    ),
    ("GET", "/api/v1/users", Coverage::Command("user search")),
    // M3 S0: service accounts.
    (
        "GET",
        "/api/v1/admin/service-accounts",
        Coverage::Command("admin service-account list"),
    ),
    (
        "POST",
        "/api/v1/admin/service-accounts",
        Coverage::BrowserOnly("an administrator's web session only: a token never creates a service account (FG-AUTH-0002 session_required)"),
    ),
    (
        "GET",
        "/api/v1/admin/service-accounts/{id}",
        Coverage::Command("admin service-account view"),
    ),
    (
        "PATCH",
        "/api/v1/admin/service-accounts/{id}",
        Coverage::Command("admin service-account update"),
    ),
    (
        "GET",
        "/api/v1/admin/service-accounts/{id}/tokens",
        Coverage::Command("admin service-account tokens"),
    ),
    (
        "POST",
        "/api/v1/admin/service-accounts/{id}/tokens",
        Coverage::BrowserOnly("an administrator's web session only: a token never issues a token (FG-AUTH-0002 session_required)"),
    ),
    (
        "DELETE",
        "/api/v1/admin/service-accounts/{id}/tokens/{token_id}",
        Coverage::Command("admin service-account revoke-token"),
    ),
    // M3 C1: the check namespaces of a service account.
    (
        "GET",
        "/api/v1/admin/service-accounts/{id}/check-grants",
        Coverage::Command("admin service-account check-grants"),
    ),
    (
        "PUT",
        "/api/v1/admin/service-accounts/{id}/check-grants/{project_id}/{namespace}",
        Coverage::Command("admin service-account grant-checks"),
    ),
    (
        "DELETE",
        "/api/v1/admin/service-accounts/{id}/check-grants/{project_id}/{namespace}",
        Coverage::Command("admin service-account revoke-checks"),
    ),
    // M3 I1a: integrations and project bindings.
    (
        "GET",
        "/api/v1/admin/integrations",
        Coverage::Command("admin integration list"),
    ),
    (
        "POST",
        "/api/v1/admin/integrations",
        Coverage::BrowserOnly("an administrator's web session only: a token never creates an integration (FG-AUTH-0002 session_required)"),
    ),
    (
        "GET",
        "/api/v1/admin/integrations/{id}",
        Coverage::Command("admin integration view"),
    ),
    (
        "PATCH",
        "/api/v1/admin/integrations/{id}",
        Coverage::BrowserOnly("an administrator's web session only: a token never changes an integration (FG-AUTH-0002 session_required); to stop one with a token, disable its service account (user disable)"),
    ),
    (
        "GET",
        "/api/v1/admin/integrations/{id}/bindings",
        Coverage::Command("admin integration bindings"),
    ),
    // M3 I1c: the event feed (the integration's own service account).
    (
        "GET",
        "/api/v1/integrations/{id}/events",
        Coverage::Command("integration events"),
    ),
    (
        "GET",
        "/api/v1/integrations/{id}/events/cursor",
        Coverage::Command("integration cursor"),
    ),
    // M3 I1d: the development status (the integration's own service account).
    (
        "GET",
        "/api/v1/integrations/{id}/dev-status",
        Coverage::Command("integration dev-status-list"),
    ),
    (
        "GET",
        "/api/v1/integrations/{id}/work-items/{ref}/dev-status",
        Coverage::Command("integration dev-status"),
    ),
    (
        "GET",
        "/api/v1/projects/{id}/integration-bindings",
        Coverage::Command("integration-binding list"),
    ),
    (
        "PUT",
        "/api/v1/projects/{id}/integration-bindings/{integration_id}",
        Coverage::Command("integration-binding set"),
    ),
    (
        "DELETE",
        "/api/v1/projects/{id}/integration-bindings/{integration_id}",
        Coverage::Command("integration-binding remove"),
    ),
    ("GET", "/api/v1/admin/users", Coverage::Command("user list")),
    (
        "POST",
        "/api/v1/admin/users/{id}/disable",
        Coverage::Command("user disable"),
    ),
    (
        "GET",
        "/api/v1/admin/users/{id}/sessions",
        Coverage::Command("user sessions"),
    ),
    (
        "DELETE",
        "/api/v1/admin/users/{id}/sessions",
        Coverage::Command("user end-sessions"),
    ),
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
    // M3 X1: `.forge/repo.yml` and its spec references.
    (
        "GET",
        "/api/v1/projects/{id}/repository/spec-refs",
        Coverage::Command("browse spec-refs"),
    ),
    // M3 X4: the spec API (plan 03 §7.2).
    (
        "GET",
        "/api/v1/projects/{id}/specs/requirements",
        Coverage::Command("spec requirements"),
    ),
    (
        "GET",
        "/api/v1/projects/{id}/specs/versions",
        Coverage::Command("spec versions"),
    ),
    (
        "GET",
        "/api/v1/projects/{id}/specs/diff",
        Coverage::Command("spec diff"),
    ),
    (
        "GET",
        "/api/v1/projects/{id}/specs/trace",
        Coverage::Command("spec trace"),
    ),
    // M3 X5: the context API (plan 03 §6).
    (
        "GET",
        "/api/v1/projects/{id}/context/work-item",
        Coverage::Command("context work-item"),
    ),
    (
        "GET",
        "/api/v1/projects/{id}/context/requirement",
        Coverage::Command("context requirement"),
    ),
    (
        "GET",
        "/api/v1/projects/{id}/merge-requests/{iid}/context",
        Coverage::Command("context merge-request"),
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
    // M3 R1a: merge requests.
    (
        "GET",
        "/api/v1/projects/{id}/merge-requests",
        Coverage::Command("mr list"),
    ),
    (
        "POST",
        "/api/v1/projects/{id}/merge-requests",
        Coverage::Command("mr create"),
    ),
    (
        "GET",
        "/api/v1/projects/{id}/merge-requests/{iid}",
        Coverage::Command("mr view"),
    ),
    (
        "PATCH",
        "/api/v1/projects/{id}/merge-requests/{iid}",
        Coverage::Command("mr edit"),
    ),
    (
        "POST",
        "/api/v1/projects/{id}/merge-requests/{iid}/close",
        Coverage::Command("mr close"),
    ),
    (
        "POST",
        "/api/v1/projects/{id}/merge-requests/{iid}/reopen",
        Coverage::Command("mr reopen"),
    ),
    // M3 R2 (ADR-0024 §1–§3): versions, their diffs and the mergeability.
    (
        "GET",
        "/api/v1/projects/{id}/merge-requests/{iid}/versions",
        Coverage::Command("mr versions"),
    ),
    (
        "GET",
        "/api/v1/projects/{id}/merge-requests/{iid}/versions/{version}/diff",
        Coverage::Command("mr diff"),
    ),
    (
        "GET",
        "/api/v1/projects/{id}/merge-requests/{iid}/compare",
        Coverage::Command("mr diff"),
    ),
    (
        "GET",
        "/api/v1/projects/{id}/merge-requests/{iid}/mergeability",
        Coverage::Command("mr mergeability"),
    ),
    // M3 X6 (plan 04 C-2): predicted conflicts between open merge requests.
    (
        "GET",
        "/api/v1/projects/{id}/merge-requests/{iid}/conflicts",
        Coverage::Command("conflicts"),
    ),
    // M3 R3 (ADR-0024 §6, §12): discussions.
    (
        "GET",
        "/api/v1/projects/{id}/merge-requests/{iid}/discussions",
        Coverage::Command("mr discussions"),
    ),
    (
        "POST",
        "/api/v1/projects/{id}/merge-requests/{iid}/discussions",
        Coverage::Command("mr comment"),
    ),
    (
        "POST",
        "/api/v1/projects/{id}/merge-requests/{iid}/discussions/{discussion_id}/notes",
        Coverage::Command("mr comment"),
    ),
    (
        "POST",
        "/api/v1/projects/{id}/merge-requests/{iid}/discussions/{discussion_id}/resolve",
        Coverage::Command("mr resolve"),
    ),
    (
        "POST",
        "/api/v1/projects/{id}/merge-requests/{iid}/discussions/{discussion_id}/unresolve",
        Coverage::Command("mr resolve"),
    ),
    // M3 R4 (ADR-0024 §4): merging.
    (
        "POST",
        "/api/v1/projects/{id}/merge-requests/{iid}/merge",
        Coverage::Command("mr merge"),
    ),
    (
        "GET",
        "/api/v1/projects/{id}/merge-requests/{iid}/merge",
        Coverage::Command("mr merge-status"),
    ),
    // M3 R5 (ADR-0024 §8): approvals and approval rules.
    (
        "POST",
        "/api/v1/projects/{id}/merge-requests/{iid}/approve",
        Coverage::Command("mr approve"),
    ),
    (
        "POST",
        "/api/v1/projects/{id}/merge-requests/{iid}/unapprove",
        Coverage::Command("mr unapprove"),
    ),
    (
        "GET",
        "/api/v1/projects/{id}/merge-requests/{iid}/approvals",
        Coverage::Command("mr approvals"),
    ),
    // M3 I1b (plan 02 §2): a merge request's work item links.
    (
        "GET",
        "/api/v1/projects/{id}/merge-requests/{iid}/work-items",
        Coverage::Command("mr work-items"),
    ),
    (
        "PUT",
        "/api/v1/projects/{id}/merge-requests/{iid}/work-items/{ref}",
        Coverage::Command("mr link-work-item"),
    ),
    (
        "DELETE",
        "/api/v1/projects/{id}/merge-requests/{iid}/work-items/{ref}",
        Coverage::Command("mr unlink-work-item"),
    ),
    (
        "GET",
        "/api/v1/projects/{id}/approval-rules",
        Coverage::Command("approval-rule list"),
    ),
    (
        "PUT",
        "/api/v1/projects/{id}/approval-rules/{name}",
        Coverage::Command("approval-rule set"),
    ),
    (
        "DELETE",
        "/api/v1/projects/{id}/approval-rules/{name}",
        Coverage::Command("approval-rule remove"),
    ),
    // M3 R6 (ADR-0024 §7, §4.1, §9, §12): the merge status, settings, the
    // CODEOWNERS switch and the protected branch merge rules.
    (
        "GET",
        "/api/v1/projects/{id}/merge-requests/{iid}/status",
        Coverage::Command("mr status"),
    ),
    (
        "GET",
        "/api/v1/projects/{id}/merge-settings",
        Coverage::Command("merge-settings show"),
    ),
    (
        "PUT",
        "/api/v1/projects/{id}/merge-settings",
        Coverage::Command("merge-settings set"),
    ),
    (
        "PUT",
        "/api/v1/projects/{id}/merge-settings/codeowners-enforcement",
        Coverage::Command("codeowners-enforcement suspend"),
    ),
    (
        "PUT",
        "/api/v1/projects/{id}/protected-refs/{rule_id}/merge-rule",
        Coverage::Command("merge-rule set"),
    ),
    (
        "DELETE",
        "/api/v1/projects/{id}/protected-refs/{rule_id}/merge-rule",
        Coverage::Command("merge-rule remove"),
    ),
    // M3 C1 (ADR-0024 §10): the Checks API.
    (
        "POST",
        "/api/v1/projects/{id}/commits/{sha}/checks",
        Coverage::Command("checks report"),
    ),
    (
        "GET",
        "/api/v1/projects/{id}/commits/{sha}/checks",
        Coverage::Command("checks list"),
    ),
    // M3 I2 (ADR-0016 §8): project webhooks.
    (
        "GET",
        "/api/v1/projects/{id}/webhooks",
        Coverage::Command("webhook list"),
    ),
    (
        "POST",
        "/api/v1/projects/{id}/webhooks",
        Coverage::Command("webhook create"),
    ),
    (
        "GET",
        "/api/v1/projects/{id}/webhooks/event-types",
        Coverage::Command("webhook event-types"),
    ),
    (
        "GET",
        "/api/v1/projects/{id}/webhooks/{webhook_id}",
        Coverage::Command("webhook view"),
    ),
    (
        "PATCH",
        "/api/v1/projects/{id}/webhooks/{webhook_id}",
        Coverage::Command("webhook update"),
    ),
    (
        "DELETE",
        "/api/v1/projects/{id}/webhooks/{webhook_id}",
        Coverage::Command("webhook delete"),
    ),
    (
        "POST",
        "/api/v1/projects/{id}/webhooks/{webhook_id}/secret",
        Coverage::Command("webhook rotate-secret"),
    ),
    (
        "GET",
        "/api/v1/projects/{id}/webhooks/{webhook_id}/deliveries",
        Coverage::Command("webhook deliveries"),
    ),
    (
        "POST",
        "/api/v1/projects/{id}/webhooks/{webhook_id}/deliveries/{delivery_id}/redeliver",
        Coverage::Command("webhook redeliver"),
    ),
    // Plan 13 stage 3: the document API.
    (
        "GET",
        "/api/v1/projects/{id}/documents",
        Coverage::Command("doc list"),
    ),
    (
        "GET",
        "/api/v1/projects/{id}/documents/{doc_id}",
        Coverage::Command("doc show"),
    ),
    // Plan 13 Q13 (ADR-0026): the document lint migration setting.
    (
        "GET",
        "/api/v1/projects/{id}/doc-lint/migration",
        Coverage::Command("doc migration show"),
    ),
    (
        "PUT",
        "/api/v1/projects/{id}/doc-lint/migration",
        Coverage::Command("doc migration set"),
    ),
    (
        "DELETE",
        "/api/v1/projects/{id}/doc-lint/migration",
        Coverage::Command("doc migration clear"),
    ),
    // M3 O2 (ADR-0024 Amendment O2): the push mirror to GitLab.
    (
        "GET",
        "/api/v1/projects/{id}/push-mirror",
        Coverage::Command("mirror show"),
    ),
    (
        "PUT",
        "/api/v1/projects/{id}/push-mirror",
        Coverage::Command("mirror set"),
    ),
    (
        "DELETE",
        "/api/v1/projects/{id}/push-mirror",
        Coverage::Command("mirror remove"),
    ),
    (
        "POST",
        "/api/v1/projects/{id}/push-mirror/sync",
        Coverage::Command("mirror sync"),
    ),
    // M3 R1b (ADR-0024 §2): `SessionOnly` — every token, `admin` scope
    // included, is refused; gstn holds a token.
    (
        "POST",
        "/api/v1/projects/{id}/merge-requests/{iid}/version-refs/purge",
        Coverage::BrowserOnly("secret removal: a human administrator's web session only; tokens are refused (FG-AUTH-0002 session_required); runbook secret-removal"),
    ),
    (
        "PUT",
        "/api/v1/projects/{id}/visibility",
        Coverage::Command("project visibility"),
    ),
    // M3 S2 (ADR-0025 §5): acting for a person.
    (
        "GET",
        "/api/v1/projects/{id}/merge-requests/{iid}/act-as-confirmation",
        Coverage::BrowserOnly("the act-as confirmation screen: a person's web session only; tokens are refused (FG-AUTH-0002 session_required)"),
    ),
    (
        "GET",
        "/api/v1/projects/{id}/integration-bindings/act-as-confirmation",
        Coverage::BrowserOnly("the binding confirmation screen (M3 X3): a person's web session only; tokens are refused (FG-AUTH-0002 session_required)"),
    ),
    (
        "POST",
        "/api/v1/integrations/{id}/act-as/claim",
        Coverage::NotForCli("kanflow's server claims the assertion a person confirmed (an integration's act_as token); gstn never acts for a person"),
    ),
    // M3 X3: a kanflow project's repositories (plan 03 §7.1).
    (
        "GET",
        "/api/v1/repositories",
        Coverage::NotForCli("kanflow's registration screen searches the repositories a PO / SM may bind, acting for them; in gstn a person names a project by its path"),
    ),
    (
        "GET",
        "/api/v1/integrations/{id}/bindings",
        Coverage::NotForCli("kanflow's server lists its kanflow projects' repositories (the integration's own service account); a person reads a project's bindings with integration-binding list"),
    ),
    (
        "POST",
        "/api/v1/integrations/{id}/bindings",
        Coverage::NotForCli("kanflow's server binds a repository for the PO / SM who confirmed it (act_as + binding_set assertion); a person binds with integration-binding set"),
    ),
    (
        "PATCH",
        "/api/v1/integrations/{id}/bindings/{binding_id}",
        Coverage::NotForCli("kanflow's server changes a binding's setting for the person who confirmed it (act_as + binding_set assertion)"),
    ),
    (
        "DELETE",
        "/api/v1/integrations/{id}/bindings/{binding_id}",
        Coverage::NotForCli("kanflow's server removes a binding for the person who confirmed it (act_as + binding_remove assertion); a person unbinds with integration-binding remove"),
    ),
    (
        "POST",
        "/api/v1/users/{id}/rename",
        Coverage::Command("user rename"),
    ),
];

/// Commands that call no API route, and why: local work only (C-1 counts
/// them as "local", not as uncovered).
pub const LOCAL_COMMANDS: &[(&str, &str)] = &[
    (
        "auth logout",
        "removes the token stored on this computer (never revokes it on the server)",
    ),
    (
        "auth git-credential",
        "the git credential helper: answers git from the stored token, or edits the global git config",
    ),
    (
        "doc fmt",
        "plan 13 stage 2: formats the working tree's documents with forge-doc; nothing is sent",
    ),
    (
        "doc lint",
        "plan 13 stage 2: the forge/doc-lint lint on the working tree with forge-doc; nothing is sent",
    ),
    ("help", "describes the commands from the registry"),
    ("schema", "describes one command from the registry"),
];

/// Routes outside `/api/v1` and how they are covered.
pub fn non_api_coverage(path: &str) -> &'static str {
    if path == "/healthz" || path == "/readyz" {
        "api (gstn api GET /healthz)"
    } else if path.starts_with("/-/confirm/") {
        // M3 S2: the person presses the act-as confirmation (ADR-0025 §5).
        "browser only (the act-as confirmation; a web session with CSRF, ADR-0025)"
    } else if path.starts_with("/-/auth/") {
        // M2 MR4: the browser sign-in (OIDC redirects and cookies).
        "browser only (OIDC sign-in; gstn uses a personal access token, ADR-0021)"
    } else if path.contains(".git/") {
        "git (auth git-credential makes plain git clone/push work)"
    } else {
        "web (browser pages; not an API)"
    }
}
