# forge-db migrations

Forward-only, append-only (CLAUDE.md §3-6, ADR-0014). File name:
`YYYYMMDDHHMMSS_<description>.sql` (UTC timestamp: no collisions between
parallel branches). An applied migration is never edited — the
`migration-immutability` hook blocks it; fix forward with a new file.
Each file runs in one transaction. Applied with `forge db migrate`.
