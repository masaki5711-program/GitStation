# CLAUDE.md — GitStation (product repository)

Guidance for Claude Code sessions that open **this repository on its own**
(e.g. a cloud session cloned from GitHub). On the maintainer's PC this
repository is the `Git/` folder inside a larger "agent model" checkout whose
own CLAUDE.md, rules, hooks and evaluator agents are **not** in this repo.

**Talk to the user in Japanese.** User-facing docs under `docs/` are Japanese.

## Where to start

1. Read the newest file in `docs/handoff/` — it states the current branch, what
   is done, what is verified and the next steps.
2. Plans (snapshot of the maintainer's `docs/plan/`, 2026-10-30): `docs/plan/`
   (`05-step-d-plan.md` = the step being built; `01` product plan; `02` kanflow
   integration; `03` spec-driven development; `04` CI/CD and merge safety;
   `06` AI-driven development). If you change a plan here, say so in the
   handoff so it is copied back.
3. Design decisions: `docs/adr/` (index `docs/adr/README.md`).

## Product in one paragraph

GitStation ("forge" in code) is an internal, fully on-prem Git platform: Git
hosting, CI/CD with deployment to internal servers, knowledge, and document
versioning for non-engineers. Rust (axum + tokio + sqlx), PostgreSQL. Its core
purpose is tight integration with kanflow (the in-house PM tool) to speed up
AI-driven development. Claude is the maintainer: every structural choice
optimises for diagnosing failures from artifacts alone (logs with
`request_id` / `error_code`, the error catalog, runbooks).

## Hard contracts (MUST)

1. **Error contract.** Every failure that can reach a log or a user has a code
   `FG-<AREA>-<NNNN>` defined once in `crates/forge-errors/errors.toml`
   (cause / diagnosis / fix / runbook / test). `docs/errors/` is generated:
   `cargo run -q -p forge-errors --bin gen-catalog`. Never hand-edit it. An
   `alert` code needs a runbook in `ops/runbooks/`; every code needs the test
   named in its `test` field.
2. **No silent failures.** No swallowed `Result`, no `unwrap()` / `expect()` /
   `panic!` on production paths, no fallback that hides a failure. Every
   external call has a timeout. Failures are logged once, at the boundary, with
   `request_id` and `error_code`.
3. **Users see `user_message` + `request_id` only.**
4. **Authorization has one decision point:** `forge_auth::authorize()`; every
   HTTP route and git operation is a row (allow AND deny) in the permission
   matrix. (Documented exceptions, ADR-0019 §8: the operator CLI's token
   commands and its move / rename commands — `forge admin project|group move`,
   `forge admin user rename` — which run as `system` and cannot be reached
   from HTTP.)
5. **Git is executed, not reimplemented** (argument vector, validated path,
   `--` before user input, timeout).
6. **Migrations are append-only.** Never edit an applied migration.
7. **Production is changed only with human approval.**
8. **On-prem.** No runtime dependency on external services; no secrets in
   source; synthetic data only in tests.
9. **Dependency direction is one-way:** `forge-web / forge-cli → feature crates
   → forge-auth / forge-git / forge-db → forge-core → forge-errors`.
   The client `forge-client` (`gstn`) is outside this graph: it depends on
   `forge-errors` only and nothing depends on it (ADR-0020).

## How to work (the gates the local hooks would enforce)

- Feature flow: plan (ask the user before implementing) → tests first → implement
  → gates green → independent review → docs → commit **only when the user asks**.
- Gates before saying "done" — quote the commands and results:
  - `cargo fmt --all -- --check`
  - `cargo clippy --workspace --all-targets -- -D warnings`
  - tests: start the dev database
    `docker compose -f ops/dev/compose.yml up -d --wait` (Postgres on
    127.0.0.1:55432, synthetic credentials), then
    `cargo nextest run --workspace -j 4` (≈ 600 tests, 7–14 min; never run two
    full suites at once — it crashed the dev Postgres once). Without Docker,
    run what you can and say clearly which tests did not run.
- Branches `<type>/<area>-<topic>` (e.g. `feat/auth-personal-access-tokens`),
  Conventional Commits `<type>(<area>): <description>`, stage explicit paths
  (no `git add -A`), never `--no-verify`, never force-push `main`.
- Remotes: `origin` = GitHub (this repo). The maintainer also mirrors `main` to
  an internal GitLab from the PC.
- Severity for reviews: CRITICAL (security / authz bypass / data loss) and HIGH
  (bug, silent failure, broken contract) block a merge; MEDIUM / LOW are notes.
