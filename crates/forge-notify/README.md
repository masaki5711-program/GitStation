# forge-notify

**Responsibility**: e-mail notifications (M3 N1; plan 11 §5 N1 and §10 Q6;
ADR-0016 Amendment N1). A feature crate: `forge-web / forge-cli → forge-notify →
forge-auth / forge-db → forge-core → forge-errors`. It runs no SQL of its own (the
SQL is `forge_db::notify`), reads only domain events (the outbox) and never depends
on another feature crate.

**How it works**
- `Notify` (the `notify` subscriber, `independent`): `merge_request.approved`,
  `.merged` and `.commented` (a new note) tell the merge request's **author** —
  never for their own action. One row per (event, recipient) in
  `forge.notifications`, with the decision made then (`access::decide`):
  `pending` (+ a `notification.send` job), `skipped` (`muted`, `opted_out`) or
  `suppressed` (`recipient_inactive`, `recipient_not_person`, `no_address`,
  `address_invalid`, `not_visible`, `merge_request_gone`). Visibility is
  `forge_auth::prove(MergeRequestRead)` for the recipient as a person in a session.
- `SendHandler` (job `notification.send`, on the worker's **own** job runner): decides
  again, builds the mail in the recipient's language (`templates`: ja / en / id —
  only what the recipient can see; never branches, ids, codes, paths), sends it
  through a `Sender` and records the outcome. Retries and backoff are the job
  queue's (`notify.max_attempts`); a message that may have been handed over, or a
  run that died while sending, is `unknown` and **never sent again**; a failed send
  ends the job successfully (the notification's own state carries the result).
- `SmtpSender` (`smtp.rs`): a small SMTP submission client with explicit phases
  (why not a crate: ADR-0016 Amendment N1 §6) — connect timeout, one send deadline,
  STARTTLS required when configured and never downgraded, AUTH PLAIN over TLS only,
  roots from `notify.smtp_ca_file` only (required with TLS; never the OS store), hard reply limits.
- `Settings`: the commands behind `GET|PUT /api/v1/user/notification-preferences`
  (decided by a proof of `NotificationPreferencesRead` / `…Write` on the caller's own
  settings; nobody else's, administrators included).

**Public API**: `Notify`, `SendHandler`, `Sender` / `SmtpSender` / `SendOutcome` /
`SendFailure` / `Reason` / `Stage`, `Mail`, `Settings`, `Preferences`, `templates`,
`access::decide`.

**Owned error code**: `FG-INT-0015` (SmtpUnavailable, warn; runbook
`notification-email`). Test: `smtp_failures_are_int_0015`.

**Configuration**: `notify.*` (`forge_core::config::NotifyConfig`); nothing is
sent until `notify.enabled = true`.

**Tests**: `tests/smtp.rs` (in-process fake relay, `test_support`), `tests/pipeline.rs`
(subscriber and job against a migrated database). No real host is ever contacted.
