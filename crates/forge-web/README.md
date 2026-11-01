# forge-web

**Responsibility**: the HTTP layer — the route registry (ADR-0010,
ADR-0015), the one dispatcher for namespace URLs, request ids, token
authentication (ADR-0019 §7), the single error renderer, timeouts, and
serving. **Handlers never decide**: the registry calls
`forge_auth::prove(principal, def.action, ctx)` and hands the handler the
admitted proof.

**Public API**
- `app(AppState, AppConfig) -> Result<Router, ForgeError>` — the product
  router (routes come only from `app::registry()`).
- A handler is an `Endpoint<S, R>`: `async fn(state, Dispatched<R>) -> impl
  IntoResponse`. `Dispatched<R>` (private fields, no `Clone` / `Default`,
  `compile_fail` doctests in `src/registry.rs`) is built only by the
  registry's `admit()`, which checks `proof.action() == def.action`
  (`FG-SYS-0001 reason=proof_action_mismatch` otherwise) — so a handler cannot
  be called outside the registry and a proof for another action is refused
  (ADR-0015 MR4 musts a–c). It exposes `proof()` (action, target id,
  context), `ctx()`, `principal()`, `method()`, `headers()`, `raw_query()`,
  `tail()` (the validated segments after a route word), `canonical_path()`,
  `via_tombstone()` and `project()` (the `ProjectRecord` a project was loaded
  from; revealed only after `authorize()`), `into_body()`.
- `registry::Registry::route(Verb, path, Action, summary, handler)` — a fixed
  path deciding on the instance (`InstanceCtx`); `route_with(.., loader,
  handler)` — a fixed path deciding on what a `Loader` loads (one lookup;
  `Lookup::Found { ctx, canonical, tombstone, project }` or `Lookup::Unresolved`,
  which `prove()` denies). The only way to add a route (a test fails on
  `Router::new` / `.route("..")` / `.nest` outside the registry). Every
  non-public fixed route gets token authentication and then the
  authentication guard: no authenticated `Principal` → `FG-AUTH-0001` before
  anything is loaded. Public routes never look at credentials and decide as
  anonymous. An invalid or duplicate path is a startup error (`FG-WEB-0004`).
- `registry::Registry::scoped(RouteScope, Verb, ScopedRoute, query_key,
  Action, summary, handler)` — a namespace URL for the dispatcher; unique by
  `(scope, verb, route, query_key)`. Public actions take `InstanceCtx` (they
  never resolve); non-public scoped routes are `Project` / `ProjectGit` with
  `ProjectCtx` for now (group / user contexts join with their first route).
  `is_public_word(verb, route)` is true only when every route with the word is
  public; a word (per verb) that mixes public and non-public routes is refused
  (`word_mixes_public_and_non_public`). A registration that does not fit
  (scope, query key, verb, context, mixed word) is `FG-WEB-0004`. `Verb::Head` is registered explicitly for scoped routes
  (a fixed `GET` also answers `HEAD`, axum).
- The dispatcher (`src/dispatch.rs`, the router's fallback; ADR-0015 "判定の
  順序"): (1) `parse_web_path(req.uri().path(), req.uri().query())` on the raw
  path; (2) the vocabulary — no scoped route for `(verb, route)` is
  `FG-WEB-0001` (never the shell: `/api/…`, dumb HTTP, `raw` / `archive` /
  git until routed); (3) `service` is part of the git route; (4) a public
  word answers the shell without credentials or the database, any other
  word needs an active principal; (5) **exactly one**
  `NamespaceResolver::resolve` call, the route chosen by the resolved kind +
  the word (none: `kind_mismatch`); (6) `prove()` — absent, deleted
  tombstone, kind mismatch and invisible answer byte-identical `FG-REPO-0005`
  (only the log `reason` differs); (7) a `moved` path redirects (`302`,
  `Cache-Control: no-store`, `Location` built from validated segments, query
  dropped) only after `authorize()` allowed and only for a safe verb that is
  not a push (`namespace.redirected`, warn); otherwise the admitted proof
  reaches the handler. The resolver call is bounded by
  `AppState::lookup_timeout` (`FG-WEB-0002 operation=namespace_resolve`).
- Routes (MR4): `GET /api/v1/namespaces/resolve?path=` (`PathResolve`:
  `{ namespace_id, kind, canonical_path, moved }` plus, since MR5, the
  owner's id under the kind's key — `project_id`, `group_id` or `user_id` —
  after `authorize()` allowed; `Cache-Control: private, no-store`; a
  missing / repeated / malformed `path` is answered like an absent path
  until FG-WEB-0006, phase 8b) and the SPA shell (`WebShell`,
  `src/routes/shell.rs`: the same bytes for every page — namespace home,
  `/-/tree` `blob` `blame` `commits` `commit` `compare` `branches` `tags`
  `settings` `members`, group `settings` / `members`, `/-/projects/{id}/…`;
  GET and HEAD).
- Route (MR5, `src/routes/projects.rs`): `GET|HEAD /api/v1/projects/{id}`
  (`ProjectRead`; HEAD is the GET route without the body). One
  `ProjectAccessLoader::load_project` call per request (bounded by
  `lookup_timeout`, `FG-WEB-0002 operation=project_load`), then `prove()`:
  no such id, an invisible project, pending deletion to a caller who cannot
  see the project and an id that is not a plain positive decimal
  (`lookup=id_param_invalid`, no lookup) are byte-identical `FG-REPO-0005`;
  pending deletion to a caller who can see it is `FG-REPO-0006`, a Guest
  `FG-AUTH-0003`, a token without `read_api` / `api` `FG-AUTH-0002` (on a
  visible project). The body comes from `project_view(proof, record)`, the
  one builder of a project's API shape: `{ id, namespace_id, path, name,
  display_name, description, visibility, kind, created_at, role }` (`kind`
  = repository kind, `role` = the caller's effective role or `null`);
  `Cache-Control: private, no-store`, `Vary: Authorization, Cookie`.
- `AppState::new(version, probes, authenticator, Lookups { namespaces,
  projects })` — the authenticator (`forge_core::Authenticator`) and the
  lookups (`forge_core::NamespaceResolver`, `ProjectAccessLoader` —
  `load_project`, one statement with the access facts and the details — each
  `Arc<dyn …>`) are required; `forge serve` passes the database
  (`forge_db::tokens::authenticate`, `forge_db::resolve`), so forge-web never
  depends on forge-db. `with_lookup_timeout` (serve: acquire + statement
  timeout + 1 s).
- `app::with_registry(registry, state, config)` — feature `test-support`
  only (`#[doc(hidden)]`): the product middleware around a test's own
  registry. forge-web enables the feature for its own tests through a
  dev-dependency on itself, so it is on in `cargo build --workspace
  --all-targets`; CI fails if any `test-support` feature appears in the
  `forge` binary's normal graph (`cargo tree -p forge-cli -e
  normal,features`).
- `app::PLANNED` — actions with matrix rows but no route yet; an action leaves
  the list in the commit that routes it.
- `Ctx` extractor — `request_id` and `lang` (from `Accept-Language`: ja / en /
  id, Japanese otherwise); `ctx.fail(err)` → `ApiError`.
- `ApiError` — renders `{ error_code, user_message, request_id }` only, sets
  `Content-Language`, and logs the failure once (`telemetry::log_failure`).
  Every 401 carries `WWW-Authenticate: Basic realm="GitStation",
  charset="UTF-8"` and `Bearer realm="GitStation"` (the one place for the
  challenge; never on 403 / 503).
- `server::bind(addr)` (`FG-SYS-0004`), `server::serve(listener, router,
  ServerLimits, shutdown)` on hyper directly: header-read timeout (slowloris),
  connection limit, shutdown grace (`FG-SYS-0005` when exceeded), accept
  failures logged as `FG-SYS-0007`; `server::shutdown_signal()` (SIGINT and
  SIGTERM; `FG-SYS-0006` when a handler cannot be installed).
- `generated_documents(version)` and the `gen-authz` binary write
  `Git/docs/authz/routes.generated.json`, `Git/docs/authz/matrix.generated.json`
  and `Git/docs/api/openapi.json`.

**Middleware** (outermost first): request id (always generated by the server;
a client `X-Request-Id` is logged as `client_request_id`) + access log →
request timeout (`FG-WEB-0002`) → panic catcher (`FG-SYS-0001`) → body
limit → fixed routes → the dispatcher as the fallback (`FG-WEB-0001` for a
path in no table) and `FG-WEB-0003` for a wrong method on a fixed path
(`Allow` kept). Every logged path goes through `log_safe_path` (256
characters, control / invisible characters escaped). The authenticator
travels as a request extension; on non-public fixed routes: token
authentication → guard → load → prove → handler.

**Token authentication** (`src/auth.rs`, step D3 MR3): `Authorization: Bearer
<token>`, or `Authorization: Basic` whose **password** is the token (git's
form; the username is ignored and never logged; a user password is never a
credential). A valid token inserts `Principal::authenticated(owner, is_admin,
true, Credential::Token(scopes))` — an administrator's token without the
`admin` scope has no administrator rights (`Principal::admin_rights`); the
`Authorization` header is then removed before the handler. No header → no
principal. Two headers, or a header on top of an existing
principal → `FG-AUTH-0001 reason=ambiguous`; an unreadable header →
`reason=malformed_header`; another scheme → `reason=unsupported_scheme`.
The authenticator's `FG-AUTH-0001` (malformed / unknown / mismatch / revoked
/ expired) and `FG-AUTH-0004` (owner_disabled) are logged once as
`forge.auth.token_rejected` (`request_id`, `error_code`, `reason`, `scheme`,
`public_id`); `FG-DB-0001` stays a 503 (never a 401) and is logged by the
renderer. Header values, tokens and usernames never reach a log
(`tests/token_principal.rs` captures the logs and checks). The log fields
are `reason`, `public_id` and `user` (the internal id, as FG-AUTH-0004's
diagnosis records it); there is no separate `context` string. A missing
request id is `FG-SYS-0001 reason=request_id_missing`, never a fresh id.

**Bounded authentication**: `BoundedAuthenticator::new(inner, permits)` wraps
any `Authenticator` in a semaphore (0 → 1 permit). `forge serve` uses
`max(1, database.write_max_connections / 2)`: each lookup takes a write
connection, so a flood of random well-formed tokens cannot starve writers.
Waiting for a permit is bounded by the request timeout (`FG-WEB-0002`).
Per-client (IP) rate limiting is **open** (ADR-0019 MR3 amendment).

**Error codes owned**: FG-WEB-0001 RouteNotFound, FG-WEB-0002 RequestTimeout,
FG-WEB-0003 MethodNotAllowed, FG-WEB-0004 RouteRegistryInvalid, FG-SYS-0004
ServerBindFailed, FG-SYS-0005 ServeFailed, FG-SYS-0006 SignalHandlerUnavailable,
FG-SYS-0007 AcceptFailed (runbook `server-start-failed`).

**Commands** (in `Git/`)

| Purpose | Command |
|---|---|
| Regenerate manifests and OpenAPI after changing routes or the matrix | `cargo run -p forge-web --bin gen-authz` |
| Check they are current | `cargo run -p forge-web --bin gen-authz -- --check` |
| Run the server | `cargo run -p forge-cli -- serve --bind 127.0.0.1:8080` |

**Deferred (from review, tracked)**
- Done in M1 phase 8 MR4: `authorize()` is unskippable (the registry proves
  and admits; handlers take `Dispatched<R>`). Still open: a test that
  compares the served route table with `registry().defs()` instead of the
  text scan.
- `Resource::Project` carries no `via`: an invisible project behind a moved
  path is logged `reason=not_visible` plus `via=moved` and `tombstone_id`
  (not `tombstone_not_visible`) on the dispatcher's project routes (ADR-0015
  Amendment MR4).
- A response-time floor for the resolve / dispatcher paths (ADR-0015
  Amendment MR3) and resolve rate limiting (M2).
- Connection fairness: per-peer connection cap, keep-alive lifetime, write
  timeout, and a 503 answer when full (before Smart HTTP, phase 9).
- Log-capture tests asserting `error_code` + `request_id` on failures and the
  access log; a connection-cap test; a body-limit (413) test.
- Per-route timeouts and an idle timeout for git streams (Smart HTTP, phase 9);
  the request timeout covers time-to-response only.
- JSON / path / query rejections wrapped into `ApiError` before the first
  route that takes a body (phase 8).
- `gen-authz --check` in CI and self-heal (needs cargo in the gate runner).
- OpenAPI types the `{id}` of `/api/v1/projects/{id}` as a string and has no
  response schema yet (utoipa, MR6); the repository state is not part of
  the project view (ADR-0015 Amendment MR5).

**Gotchas**
- OpenAPI is generated from the registry with `serde_json` for now; request /
  response schemas (utoipa, per ADR-0010) join with the resource routes.
- `GET /readyz` (public) runs `AppState::probes` (`forge_core::readiness::ReadinessProbe`; the database today) in order, each bounded by `probe_timeout` (3 s); the first failure answers with its own code (FG-DB-0001, 503) or FG-WEB-0005. The body is `{"status":"ok"}` or the standard error body — never host names. forge-web does not depend on forge-db: `forge serve` passes the probes.

**Tests**: `cargo nextest run -p forge-web` (no network beyond loopback;
`tests/token_principal.rs` uses a fake authenticator; `src/dispatch_tests.rs`
a scripted resolver that counts calls and the shared
`tests/fixtures/url-vectors.json`; `src/routes/projects_tests.rs` a
scripted project loader that counts calls; `tests/member_roles.rs`,
`tests/namespace_tombstone_routes.rs` and `tests/project_read.rs` need the
development database). The real-`git` token tests are named in
ADR-0019 "見送り" and land with Smart HTTP (phase 9).
