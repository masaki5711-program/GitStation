# forge-errors

**Responsibility**: the error registry. Every FG error code is defined once in
`errors.toml`; this crate turns it into the `ErrorCode` enum (via `build.rs`),
the `ForgeError` type, and the catalog pages in `Git/docs/errors/`.

**Public API**: `ErrorCode` (generated; `id()`, `http_status()`, `severity()`,
`user_message()`, `entry()`, `parse()`, `all()`), `ForgeError` (`new`,
`with_context`, `with_source`, `code`, `context`, `user_message`),
`CatalogEntry`, `Severity`, `Lang`, `catalog_files()`.

**Commands** (run in `Git/`):

| Purpose | Command |
|---|---|
| Regenerate the catalog after editing `errors.toml` | `cargo run -p forge-errors --bin gen-catalog` |
| Check the catalog is current (no writes) | `cargo run -p forge-errors --bin gen-catalog -- --check` |
| Explain a code | `cargo run -p forge-cli -- explain FG-SYS-0001` |

The `catalog_on_disk_matches_registry` test fails when the committed catalog
is stale, so the Stop test gate catches drift.

**Error codes owned**: `FG-SYS-0001` Internal, `FG-SYS-0002` ConfigInvalid,
`FG-SYS-0003` UnknownErrorCode (the registry itself is shared by all crates).

**Runbooks**: `internal-error`, `config-invalid`.

**Gotchas**
- `build.rs` validates the registry and fails the build with the offending
  code named — read the cargo output.
- `ErrorCode` is intentionally **not** `#[non_exhaustive]`: adding a code must
  break every exhaustive `match` so each boundary decides how to handle it.
- Never edit `Git/docs/errors/` by hand (`pre:edit:generated-guard`).
