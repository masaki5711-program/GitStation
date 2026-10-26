# forge-web

**Responsibility**: the HTTP layer — the route registry (ADR-0010), request
ids, the single error renderer, timeouts, and serving. Handlers decide with
`forge_auth::authorize()` (DB-backed handlers arrive in M1 phase 8).

**Public API**
- `app(AppState, AppConfig) -> Result<Router, ForgeError>` — the product
  router; `build(Registry, ..)` for tests.
- `registry::Registry::route(Verb, path, Action, summary, handler)` — the only
  way to add a route (never `Router::route`). A duplicate is a startup error.
- `app::PLANNED` — actions with matrix rows but no route yet; an action leaves
  the list in the commit that routes it.
- `Ctx` extractor — `request_id` and `lang` (from `Accept-Language`: ja / en /
  id, Japanese otherwise); `ctx.fail(err)` → `ApiError`.
- `ApiError` — renders `{ error_code, user_message, request_id }` only, sets
  `Content-Language`, and logs the failure once (`telemetry::log_failure`).
- `server::bind(addr)` (`FG-SYS-0004` on failure), `server::serve(listener,
  router, shutdown)`.
- `generated_documents(version)` and the `gen-authz` binary write
  `Git/docs/authz/routes.generated.json`, `Git/docs/authz/matrix.generated.json`
  and `Git/docs/api/openapi.json`.

**Middleware** (outermost first): request id + access log (`http.request`
with method, path, status, duration_ms, request_id) → request timeout
(`FG-WEB-0002`) → routes → fallback (`FG-WEB-0001`).

**Error codes owned**: FG-WEB-0001 RouteNotFound, FG-WEB-0002 RequestTimeout,
FG-SYS-0004 ServerBindFailed (runbook `server-bind-failed`).

**Commands** (in `Git/`)

| Purpose | Command |
|---|---|
| Regenerate manifests and OpenAPI after changing routes or the matrix | `cargo run -p forge-web --bin gen-authz` |
| Check they are current | `cargo run -p forge-web --bin gen-authz -- --check` |
| Run the server | `cargo run -p forge-cli -- serve --bind 127.0.0.1:8080` |

**Gotchas**
- OpenAPI is generated from the registry with `serde_json` for now; request /
  response schemas (utoipa, per ADR-0010) join with the resource routes.
- `/readyz` (DB, storage) arrives with the database (M1 phase 2).

**Tests**: `cargo nextest run -p forge-web` (no DB, no network beyond loopback).
