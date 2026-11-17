-- ADR-0036: news for the PFS portal's home (the portal contract v1 §7).
--
-- forge.portal_news is the portal's outbox: one row per news item, written
-- by the business change itself in its own transaction (a deployment's end,
-- an auto-merge stop, a spec release), together with its `portal.news_send`
-- job. Only while `portal.enabled` and `portal.news_enabled` are on; off,
-- nothing is written (no backlog to replay when it is turned on).
--
--   source_id   the contract's idempotency key (one row per key: a retried
--               change or a repeated stop writes nothing new)
--   topic       deploy_failed | deploy_finished | auto_merge_stopped |
--               spec_released
--   facts       references only (merge request number, environment, version,
--               short SHA, status): never a name, an address or a body
--   audience    the users to tell (ids; their staff ids are read, and their
--               right to read the project decided, when it is sent)
--   state       pending → sent (the portal answered 202) | failed (tries used
--               up, or refused for good) | dropped (nobody left to tell)
-- Finished rows are deleted after 30 days by the worker.

CREATE TABLE forge.portal_news (
    id               bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    source_id        text NOT NULL CONSTRAINT portal_news_source_id
        CHECK (source_id ~ '^[a-z0-9][a-z0-9._-]{0,127}$'),
    topic            text NOT NULL CONSTRAINT portal_news_topic
        CHECK (topic IN ('deploy_failed', 'deploy_finished', 'auto_merge_stopped',
                         'spec_released')),
    project_id       bigint NOT NULL REFERENCES forge.projects (id),
    facts            jsonb NOT NULL DEFAULT '{}'::jsonb CONSTRAINT portal_news_facts
        CHECK (jsonb_typeof(facts) = 'object' AND octet_length(facts::text) <= 2048),
    audience         bigint[] NOT NULL CONSTRAINT portal_news_audience
        CHECK (cardinality(audience) BETWEEN 1 AND 200),
    occurred_at      timestamptz NOT NULL,
    state            text NOT NULL DEFAULT 'pending' CONSTRAINT portal_news_state
        CHECK (state IN ('pending', 'sent', 'failed', 'dropped')),
    reason           text CONSTRAINT portal_news_reason
        CHECK (reason ~ '^[a-z][a-z0-9_]{0,63}$'),
    attempts         integer NOT NULL DEFAULT 0 CONSTRAINT portal_news_attempts
        CHECK (attempts >= 0),
    last_status      integer CONSTRAINT portal_news_last_status
        CHECK (last_status BETWEEN 100 AND 599),
    last_error_code  text CONSTRAINT portal_news_error_code
        CHECK (last_error_code ~ '^FG-[A-Z]+-[0-9]{4}$'),
    request_id       text NOT NULL CONSTRAINT portal_news_request_id
        CHECK (octet_length(request_id) BETWEEN 1 AND 128),
    created_at       timestamptz NOT NULL DEFAULT now(),
    updated_at       timestamptz NOT NULL DEFAULT now(),
    sent_at          timestamptz,
    CONSTRAINT portal_news_sent CHECK ((state = 'sent') = (sent_at IS NOT NULL))
);
-- The idempotency of the contract: one row per source_id.
CREATE UNIQUE INDEX portal_news_source ON forge.portal_news (source_id);
-- What is still to send (doctor's pending count).
CREATE INDEX portal_news_pending ON forge.portal_news (id) WHERE state = 'pending';
-- The retention of finished rows and doctor's failed count.
CREATE INDEX portal_news_finished ON forge.portal_news (updated_at) WHERE state <> 'pending';
-- The project purge.
CREATE INDEX portal_news_project ON forge.portal_news (project_id);

-- The portal's "stopped auto-merges" widget walks the stopped states only.
CREATE INDEX auto_merge_states_stopped ON forge.auto_merge_states (merge_request_id)
    WHERE state = 'stopped';

GRANT SELECT, INSERT, UPDATE, DELETE ON forge.portal_news TO forge_app;
GRANT SELECT ON forge.portal_news TO forge_ro;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261225100000, "guarded_changes": []}');
