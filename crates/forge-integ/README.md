# forge-integ

**Responsibility**: integrations (M3 I1c; plan 11 §5 I1; plan 02 §5.7, §11.2;
ADR-0016 §7 and its Amendment I1c) — the event feed an integration (kanflow)
reads: `GET /api/v1/integrations/{id}/events` and `…/events/cursor`
(docs/api/integration-feed.md). A feature crate:
`forge-web / forge-cli → forge-integ → forge-auth / forge-db → forge-core →
forge-errors`. It runs no SQL of its own (the SQL is `forge_db::feed`), reads only
domain events (the outbox) and never depends on another feature crate.

**How a read decides.** The route proves `IntegrationEventRead` on an
`IntegrationFeedCtx` (only the integration's own service account; people,
administrators included, bots, agents' tokens, act-as and a disabled integration
are refused). `Feed::read_events` then opens one `REPEATABLE READ READ ONLY`
snapshot, reads the feed state and the decision's material there, and calls
`forge_auth::prove_visible`, which decides every bound project × visibility class
with `authorize()` and builds the `VisibleSet` (its digest includes each binding's
generation). The cursor (`FeedPosition`, AES-256-GCM, at most 1.5 × the retention
period old) is checked against the epoch, the head, `retained_from` and the
digest — `FG-INT-0007` 410 with `reason`, never a silent continuation. The page is
read up to `min(high_seq, work-item-sync)`, ≤ 500 events and ≤ 2 MiB.

**Public API** (`Feed::new(Db, Arc<CursorKeys>)`)
- `target(id)` — the route loader's facts (`LoadedIntegrationFeed`).
- `read_events(&Authorized<IntegrationFeedCtx>, request_id, &FeedQuery) -> EventsPage`
  (`IntegrationEventRead`): events with their `work_items`, the actor (e-mail with
  `user_email`, employee number with `user_employee_number`), per-event cursors,
  `next_cursor`, `has_more`; writes `integration_feed_reads` once a minute and logs
  `FG-INT-0009` when the consumer is behind half the retention period.
- `head_cursor(&Authorized<IntegrationFeedCtx>) -> String`
  (`IntegrationEventCursorRead`): the head position (the first step of a resync).

Only forge-web's feed routes call `read_events` / `head_cursor`; only forge-auth
builds a `VisibleSet`; only this crate reads feed pages (`tests/decided_callers.rs`).

**Owned error codes**: FG-INT-0007 FeedCursorExpired (410), FG-INT-0008
FeedCursorInvalid (400), FG-INT-0009 FeedConsumerLag (warn; also doctor
`integrations.feed_lag`). Denials are FG-REPO-0005 / FG-AUTH-0003 with `reason`
(`not_integration_account`, `foreign_integration`, `agent_token`,
`act_as_refused`, `integration_disabled`).

**Runbook**: `ops/runbooks/integration-feed-lag.md`.
