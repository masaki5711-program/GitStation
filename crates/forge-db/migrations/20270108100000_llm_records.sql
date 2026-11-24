-- Plan 19 L5-2 (ADR-0039 §3): the local LLM's records — what was asked,
-- what came back, and what people thought of it. The queue itself lives
-- in the worker's memory: the INPUT IS NEVER STORED (plan 19 §5), only its
-- SHA-256 and the keys to rebuild it (the subject, a head SHA).
--
-- DERIVED state (tables.toml kind = "derived"): written by the gateway
-- (forge-llm, the worker) and by a person's feedback; no audit row
-- (a person's setting change on forge.project_llm is audited by the
-- command, L5-4).
--
-- forge.llm_requests — one row per request.
--   purpose / priority        why it ran and its tier at submission
--   subject_kind / subject_id what it is about (merge_request + id,
--                             deployment + id, incident without id)
--   project_id                the project, if any (purge deletes its rows)
--   requested_by              the person or event subject (NULL: system)
--   model / model_digest      `name:tag` and the digest checked at start
--   prompt_id/prompt_version  the versioned prompt
--   input_sha256 / input_class / input_tokens / output_tokens
--   attempts                  model calls (2 after one invalid answer)
--   state                     queued → running → succeeded | failed;
--                             abandoned (deadline, the worker restarted)
--   error_code                the FG code of failed / abandoned
--   request_id                the request or event that asked
--   Kept 90 days (forge_db::llm::sweep_retention).
--
-- forge.llm_advice — a checked advice (words only: no action column).
--   input_class is the request's class or higher and NEVER LOWERED
--   (forge.llm_advice_guard): a confidential advice stays confidential.
--   Only superseded_by changes after the insert. Kept 180 days.
--
-- forge.llm_advice_feedback — "useful" / "wrong" per finding and person
--   (finding_index -1 = the advice as a whole). One row per (advice,
--   finding, user). Never deleted: the adoption and false-positive rates
--   are measured from it (plan 19 §4, §6); purpose and class are copied so
--   the row keeps its meaning after the advice is swept (advice_id NULL).
--
-- forge.project_llm — a project's `llm.review` (off | advisory | required;
--   no row = off, plan 19 §3.1, §11 2).

CREATE TABLE forge.llm_requests (
    id             bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    purpose        text NOT NULL CONSTRAINT llm_requests_purpose
        CHECK (purpose IN ('mr_review', 'incident_diagnosis', 'approval_summary')),
    priority       text NOT NULL CONSTRAINT llm_requests_priority
        CHECK (priority IN ('interactive', 'review', 'autofix', 'batch')),
    subject_kind   text NOT NULL CONSTRAINT llm_requests_subject_kind
        CHECK (subject_kind ~ '^[a-z][a-z_]{0,31}$'),
    subject_id     bigint,
    project_id     bigint REFERENCES forge.projects (id),
    requested_by   bigint REFERENCES forge.users (id),
    model          text NOT NULL CONSTRAINT llm_requests_model
        CHECK (char_length(model) BETWEEN 1 AND 200),
    model_digest   text CONSTRAINT llm_requests_model_digest
        CHECK (model_digest ~ '^[0-9a-f]{64}$'),
    prompt_id      text NOT NULL CONSTRAINT llm_requests_prompt_id
        CHECK (prompt_id ~ '^[a-z0-9][a-z0-9_.-]{0,63}$'),
    prompt_version integer NOT NULL CONSTRAINT llm_requests_prompt_version
        CHECK (prompt_version >= 1),
    input_sha256   text NOT NULL CONSTRAINT llm_requests_input_sha256
        CHECK (input_sha256 ~ '^[0-9a-f]{64}$'),
    input_class    text NOT NULL CONSTRAINT llm_requests_input_class
        CHECK (input_class IN ('public', 'internal', 'confidential')),
    input_tokens   integer CONSTRAINT llm_requests_input_tokens CHECK (input_tokens >= 0),
    output_tokens  integer CONSTRAINT llm_requests_output_tokens CHECK (output_tokens >= 0),
    attempts       smallint NOT NULL DEFAULT 0 CONSTRAINT llm_requests_attempts
        CHECK (attempts BETWEEN 0 AND 2),
    state          text NOT NULL DEFAULT 'queued' CONSTRAINT llm_requests_state
        CHECK (state IN ('queued', 'running', 'succeeded', 'failed', 'abandoned')),
    error_code     text CONSTRAINT llm_requests_error_code
        CHECK (error_code ~ '^FG-[A-Z]+-[0-9]{4}$'),
    request_id     text NOT NULL CONSTRAINT llm_requests_request_id
        CHECK (char_length(request_id) BETWEEN 1 AND 128),
    queued_at      timestamptz NOT NULL DEFAULT now(),
    started_at     timestamptz,
    finished_at    timestamptz,
    CONSTRAINT llm_requests_code_when_failed
        CHECK ((state IN ('failed', 'abandoned')) = (error_code IS NOT NULL)),
    CONSTRAINT llm_requests_finished
        CHECK ((state IN ('succeeded', 'failed', 'abandoned')) = (finished_at IS NOT NULL)),
    CONSTRAINT llm_requests_started
        CHECK (state NOT IN ('running', 'succeeded') OR started_at IS NOT NULL)
);
-- The gpu-eval claim and the restart sweep look at the live rows only.
CREATE INDEX llm_requests_live ON forge.llm_requests (priority, queued_at)
    WHERE state IN ('queued', 'running');
CREATE INDEX llm_requests_queued_at ON forge.llm_requests (queued_at);
CREATE INDEX llm_requests_project ON forge.llm_requests (project_id) WHERE project_id IS NOT NULL;
CREATE INDEX llm_requests_requested_by ON forge.llm_requests (requested_by)
    WHERE requested_by IS NOT NULL;

CREATE TABLE forge.llm_advice (
    id               bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    llm_request_id   bigint REFERENCES forge.llm_requests (id) ON DELETE SET NULL,
    purpose          text NOT NULL CONSTRAINT llm_advice_purpose
        CHECK (purpose IN ('mr_review', 'incident_diagnosis', 'approval_summary')),
    subject_kind     text NOT NULL CONSTRAINT llm_advice_subject_kind
        CHECK (subject_kind ~ '^[a-z][a-z_]{0,31}$'),
    subject_id       bigint,
    project_id       bigint REFERENCES forge.projects (id),
    head_sha         text CONSTRAINT llm_advice_head_sha
        CHECK (head_sha ~ '^[0-9a-f]{40}([0-9a-f]{24})?$'),
    summary          text NOT NULL CONSTRAINT llm_advice_summary
        CHECK (char_length(summary) BETWEEN 1 AND 2000),
    findings         jsonb NOT NULL DEFAULT '[]' CONSTRAINT llm_advice_findings
        CHECK (jsonb_typeof(findings) = 'array' AND jsonb_array_length(findings) <= 10),
    deep_review      text CONSTRAINT llm_advice_deep_review
        CHECK (char_length(deep_review) BETWEEN 1 AND 2000),
    dropped_findings integer NOT NULL DEFAULT 0 CONSTRAINT llm_advice_dropped
        CHECK (dropped_findings >= 0),
    -- the summary was withheld (it stated a verdict) or findings were
    -- dropped for their evidence: shown as less trustworthy
    degraded         boolean NOT NULL DEFAULT false,
    input_class      text NOT NULL CONSTRAINT llm_advice_input_class
        CHECK (input_class IN ('public', 'internal', 'confidential')),
    model            text NOT NULL CONSTRAINT llm_advice_model
        CHECK (char_length(model) BETWEEN 1 AND 200),
    prompt_version   integer NOT NULL CONSTRAINT llm_advice_prompt_version
        CHECK (prompt_version >= 1),
    superseded_by    bigint REFERENCES forge.llm_advice (id) ON DELETE SET NULL,
    created_at       timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX llm_advice_subject ON forge.llm_advice (subject_kind, subject_id, id);
CREATE INDEX llm_advice_created_at ON forge.llm_advice (created_at);
CREATE INDEX llm_advice_request ON forge.llm_advice (llm_request_id)
    WHERE llm_request_id IS NOT NULL;
CREATE INDEX llm_advice_project ON forge.llm_advice (project_id) WHERE project_id IS NOT NULL;
CREATE INDEX llm_advice_superseded_by ON forge.llm_advice (superseded_by)
    WHERE superseded_by IS NOT NULL;

-- The class of an advice is at least its request's and never lowered;
-- after the insert only superseded_by changes (and the FK's SET NULL).
CREATE FUNCTION forge.llm_advice_guard() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, pg_temp
AS $$
DECLARE
    v_request text;
BEGIN
    IF TG_OP = 'INSERT' THEN
        IF NEW.llm_request_id IS NOT NULL THEN
            SELECT r.input_class INTO v_request FROM forge.llm_requests r
             WHERE r.id = NEW.llm_request_id;
            IF array_position(ARRAY['public', 'internal', 'confidential'], NEW.input_class)
               < array_position(ARRAY['public', 'internal', 'confidential'], v_request) THEN
                RAISE EXCEPTION USING ERRCODE = 'XF001',
                    MESSAGE = 'an advice on forge.llm_advice keeps at least its request''s input_class';
            END IF;
        END IF;
        RETURN NEW;
    END IF;
    IF NEW.id IS DISTINCT FROM OLD.id
       OR NEW.purpose IS DISTINCT FROM OLD.purpose
       OR NEW.subject_kind IS DISTINCT FROM OLD.subject_kind
       OR NEW.subject_id IS DISTINCT FROM OLD.subject_id
       OR NEW.project_id IS DISTINCT FROM OLD.project_id
       OR NEW.head_sha IS DISTINCT FROM OLD.head_sha
       OR NEW.summary IS DISTINCT FROM OLD.summary
       OR NEW.findings IS DISTINCT FROM OLD.findings
       OR NEW.deep_review IS DISTINCT FROM OLD.deep_review
       OR NEW.dropped_findings IS DISTINCT FROM OLD.dropped_findings
       OR NEW.degraded IS DISTINCT FROM OLD.degraded
       OR NEW.input_class IS DISTINCT FROM OLD.input_class
       OR NEW.model IS DISTINCT FROM OLD.model
       OR NEW.prompt_version IS DISTINCT FROM OLD.prompt_version
       OR NEW.created_at IS DISTINCT FROM OLD.created_at
       OR (NEW.llm_request_id IS DISTINCT FROM OLD.llm_request_id AND NEW.llm_request_id IS NOT NULL) THEN
        RAISE EXCEPTION USING ERRCODE = 'XF001',
            MESSAGE = 'an advice on forge.llm_advice never changes but for superseded_by (its input_class is never lowered)';
    END IF;
    -- superseded_by is set once, to a newer advice of the same subject and
    -- purpose (or cleared by its FK when that advice is deleted).
    IF NEW.superseded_by IS DISTINCT FROM OLD.superseded_by AND NEW.superseded_by IS NOT NULL
       AND (OLD.superseded_by IS NOT NULL
            OR NEW.superseded_by <= NEW.id
            OR NOT EXISTS (SELECT 1 FROM forge.llm_advice s
                            WHERE s.id = NEW.superseded_by
                              AND s.subject_kind = NEW.subject_kind
                              AND s.subject_id IS NOT DISTINCT FROM NEW.subject_id
                              AND s.purpose = NEW.purpose)) THEN
        RAISE EXCEPTION USING ERRCODE = 'XF001',
            MESSAGE = 'an advice on forge.llm_advice is superseded once, by a newer advice of its subject and purpose';
    END IF;
    RETURN NEW;
END
$$;
REVOKE ALL ON FUNCTION forge.llm_advice_guard() FROM PUBLIC;
CREATE TRIGGER llm_advice_guarded BEFORE INSERT OR UPDATE ON forge.llm_advice
    FOR EACH ROW EXECUTE FUNCTION forge.llm_advice_guard();
ALTER TABLE forge.llm_advice ENABLE ALWAYS TRIGGER llm_advice_guarded;

-- A request's identity never changes; its state only moves forward
-- (queued -> running -> succeeded | failed | abandoned, queued -> an end);
-- an ended request never changes.
CREATE FUNCTION forge.llm_requests_guard() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    IF NEW.id IS DISTINCT FROM OLD.id
       OR NEW.purpose IS DISTINCT FROM OLD.purpose
       OR NEW.priority IS DISTINCT FROM OLD.priority
       OR NEW.subject_kind IS DISTINCT FROM OLD.subject_kind
       OR NEW.subject_id IS DISTINCT FROM OLD.subject_id
       OR NEW.project_id IS DISTINCT FROM OLD.project_id
       OR NEW.requested_by IS DISTINCT FROM OLD.requested_by
       OR NEW.model IS DISTINCT FROM OLD.model
       OR NEW.model_digest IS DISTINCT FROM OLD.model_digest
       OR NEW.prompt_id IS DISTINCT FROM OLD.prompt_id
       OR NEW.prompt_version IS DISTINCT FROM OLD.prompt_version
       OR NEW.input_sha256 IS DISTINCT FROM OLD.input_sha256
       OR NEW.input_class IS DISTINCT FROM OLD.input_class
       OR NEW.request_id IS DISTINCT FROM OLD.request_id
       OR NEW.queued_at IS DISTINCT FROM OLD.queued_at THEN
        RAISE EXCEPTION USING ERRCODE = 'XF001',
            MESSAGE = 'the identity of a request on forge.llm_requests never changes';
    END IF;
    IF OLD.state IN ('succeeded', 'failed', 'abandoned') AND NEW IS DISTINCT FROM OLD THEN
        RAISE EXCEPTION USING ERRCODE = 'XF001',
            MESSAGE = 'an ended request on forge.llm_requests never changes';
    END IF;
    IF NEW.state IS DISTINCT FROM OLD.state AND NOT (
           (OLD.state = 'queued' AND NEW.state IN ('running', 'succeeded', 'failed', 'abandoned'))
           OR (OLD.state = 'running' AND NEW.state IN ('succeeded', 'failed', 'abandoned'))) THEN
        RAISE EXCEPTION USING ERRCODE = 'XF001',
            MESSAGE = 'a request on forge.llm_requests only moves forward';
    END IF;
    RETURN NEW;
END
$$;
REVOKE ALL ON FUNCTION forge.llm_requests_guard() FROM PUBLIC;
CREATE TRIGGER llm_requests_forward BEFORE UPDATE ON forge.llm_requests
    FOR EACH ROW EXECUTE FUNCTION forge.llm_requests_guard();
ALTER TABLE forge.llm_requests ENABLE ALWAYS TRIGGER llm_requests_forward;

CREATE TABLE forge.llm_advice_feedback (
    id            bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    advice_id     bigint REFERENCES forge.llm_advice (id) ON DELETE SET NULL,
    purpose       text NOT NULL CONSTRAINT llm_advice_feedback_purpose
        CHECK (purpose IN ('mr_review', 'incident_diagnosis', 'approval_summary')),
    input_class   text NOT NULL CONSTRAINT llm_advice_feedback_input_class
        CHECK (input_class IN ('public', 'internal', 'confidential')),
    finding_index smallint NOT NULL CONSTRAINT llm_advice_feedback_finding
        CHECK (finding_index BETWEEN -1 AND 9),
    user_id       bigint NOT NULL REFERENCES forge.users (id),
    verdict       text NOT NULL CONSTRAINT llm_advice_feedback_verdict
        CHECK (verdict IN ('useful', 'wrong')),
    created_at    timestamptz NOT NULL DEFAULT now(),
    updated_at    timestamptz NOT NULL DEFAULT now()
);
-- One verdict per (advice, finding, person); a person may change it.
CREATE UNIQUE INDEX llm_advice_feedback_one ON forge.llm_advice_feedback
    (advice_id, finding_index, user_id) WHERE advice_id IS NOT NULL;
CREATE INDEX llm_advice_feedback_user ON forge.llm_advice_feedback (user_id);
CREATE INDEX llm_advice_feedback_purpose ON forge.llm_advice_feedback (purpose, created_at);

-- A verdict row keeps its identity: only verdict and updated_at change
-- (and advice_id becomes NULL when the sweep deletes its advice).
CREATE FUNCTION forge.llm_advice_feedback_guard() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    IF NEW.id IS DISTINCT FROM OLD.id
       OR NEW.purpose IS DISTINCT FROM OLD.purpose
       OR NEW.input_class IS DISTINCT FROM OLD.input_class
       OR NEW.finding_index IS DISTINCT FROM OLD.finding_index
       OR NEW.user_id IS DISTINCT FROM OLD.user_id
       OR NEW.created_at IS DISTINCT FROM OLD.created_at
       OR (NEW.advice_id IS DISTINCT FROM OLD.advice_id AND NEW.advice_id IS NOT NULL) THEN
        RAISE EXCEPTION USING ERRCODE = 'XF001',
            MESSAGE = 'a verdict on forge.llm_advice_feedback changes only its verdict';
    END IF;
    RETURN NEW;
END
$$;
REVOKE ALL ON FUNCTION forge.llm_advice_feedback_guard() FROM PUBLIC;
CREATE TRIGGER llm_advice_feedback_identity BEFORE UPDATE ON forge.llm_advice_feedback
    FOR EACH ROW EXECUTE FUNCTION forge.llm_advice_feedback_guard();
ALTER TABLE forge.llm_advice_feedback ENABLE ALWAYS TRIGGER llm_advice_feedback_identity;

CREATE TABLE forge.project_llm (
    project_id bigint PRIMARY KEY REFERENCES forge.projects (id),
    review     text NOT NULL CONSTRAINT project_llm_review
        CHECK (review IN ('off', 'advisory', 'required')),
    changed_by bigint NOT NULL REFERENCES forge.users (id),
    changed_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX project_llm_changed_by ON forge.project_llm (changed_by);

GRANT SELECT, INSERT, UPDATE, DELETE ON forge.llm_requests TO forge_app;
GRANT SELECT ON forge.llm_requests TO forge_ro;
GRANT SELECT, INSERT, UPDATE, DELETE ON forge.llm_advice TO forge_app;
GRANT SELECT ON forge.llm_advice TO forge_ro;
GRANT SELECT, INSERT, UPDATE ON forge.llm_advice_feedback TO forge_app;
GRANT SELECT ON forge.llm_advice_feedback TO forge_ro;
GRANT SELECT, INSERT, UPDATE, DELETE ON forge.project_llm TO forge_app;
GRANT SELECT ON forge.project_llm TO forge_ro;

INSERT INTO forge_audit.guarded_objects (object_type, object_identity) VALUES
    ('function', 'forge.llm_advice_guard()'),
    ('trigger', 'llm_advice_guarded on forge.llm_advice'),
    ('function', 'forge.llm_requests_guard()'),
    ('trigger', 'llm_requests_forward on forge.llm_requests'),
    ('function', 'forge.llm_advice_feedback_guard()'),
    ('trigger', 'llm_advice_feedback_identity on forge.llm_advice_feedback')
ON CONFLICT DO NOTHING;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20270108100000, "guarded_changes": ["forge.llm_advice_guard", "forge.llm_advice.llm_advice_guarded", "forge.llm_requests_guard", "forge.llm_requests.llm_requests_forward", "forge.llm_advice_feedback_guard", "forge.llm_advice_feedback.llm_advice_feedback_identity"]}');
