//! forge-checks: check runs (M3 C1, ADR-0024 §10, §11; plan 02 §5.4). A
//! feature crate beside forge-review: it validates a report, decides who
//! may report it through forge-auth's `authorize()` and records it through
//! forge-db. forge-review never depends on it (CLAUDE.md §3-9): the merge
//! conditions read the runs through `forge_core::CheckResultsSource`.
