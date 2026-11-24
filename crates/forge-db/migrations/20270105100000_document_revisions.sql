-- Plan 13 stage 4 part A (§7.5, ADR-0027): one open revision per document
-- and the edit claim. Both tables are derived / operational state (no
-- audited trigger): what a merge request revises is derived from git, and
-- a claim is a short-lived hint. A Maintainer's revocation of another
-- person's claim writes its own audit row (`document.edit_claim.revoked`).
-- Nothing is ever written into a repository file.
--
-- forge.merge_request_documents: the documents a merge request revises —
-- the `.md` files whose front matter names a `doctype` and whose blob
-- differs between the merge base with the target and the head — one row
-- per document (`doc_id`) and language (`lang`, '' for the original; a
-- translation `<id>.<lang>.md` is a document of its own, ADR-0027 §1).
-- Written when the merge request is opened (forge-web) and replaced for
-- each new version by the `check.spec_index` job (forge-checks). Only rows
-- of open merge requests count; closed and merged ones are left as they
-- were (the project purge deletes them).
--
--   since     when the merge request first revised the document (kept
--             across versions): the earliest open one holds the revision.
--   head_sha  the head the row was derived from.
--
-- forge.document_edit_claims: at most one claim per document of a project
-- (originals only). A claim lapses at `expires_at` (30 minutes after it was
-- taken or last extended, the database's clock); a lapsed row is ignored
-- and replaced by the next claim. Removed when its holder releases it,
-- opens a merge request revising the document, or a Maintainer revokes it.

CREATE TABLE forge.merge_request_documents (
    merge_request_id  bigint NOT NULL REFERENCES forge.merge_requests (id),
    project_id        bigint NOT NULL REFERENCES forge.projects (id),
    doc_id            text COLLATE "C" NOT NULL CONSTRAINT merge_request_documents_doc_id
        CHECK (doc_id ~ '^[A-Za-z0-9._-]{1,128}$'),
    lang              text COLLATE "C" NOT NULL DEFAULT '' CONSTRAINT merge_request_documents_lang
        CHECK (lang ~ '^([a-z]{2}(-[A-Z]{2})?)?$'),
    head_sha          text NOT NULL CONSTRAINT merge_request_documents_head_sha
        CHECK (head_sha ~ '^([0-9a-f]{40}|[0-9a-f]{64})$'),
    since             timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (merge_request_id, doc_id, lang)
);
-- The open revisions of a document (the refusal at open, the status route).
CREATE INDEX merge_request_documents_doc
    ON forge.merge_request_documents (project_id, doc_id, lang);

CREATE TABLE forge.document_edit_claims (
    project_id         bigint NOT NULL REFERENCES forge.projects (id),
    doc_id             text COLLATE "C" NOT NULL CONSTRAINT document_edit_claims_doc_id
        CHECK (doc_id ~ '^[A-Za-z0-9._-]{1,128}$'),
    user_id            bigint NOT NULL REFERENCES forge.users (id),
    merge_request_iid  bigint CONSTRAINT document_edit_claims_iid CHECK (merge_request_iid > 0),
    claimed_at         timestamptz NOT NULL DEFAULT now(),
    expires_at         timestamptz NOT NULL,
    CONSTRAINT document_edit_claims_expiry CHECK (expires_at > claimed_at),
    PRIMARY KEY (project_id, doc_id)
);
CREATE INDEX document_edit_claims_user ON forge.document_edit_claims (user_id);

GRANT SELECT, INSERT, UPDATE, DELETE ON forge.merge_request_documents TO forge_app;
GRANT SELECT ON forge.merge_request_documents TO forge_ro;
GRANT SELECT, INSERT, UPDATE, DELETE ON forge.document_edit_claims TO forge_app;
GRANT SELECT ON forge.document_edit_claims TO forge_ro;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20270105100000, "guarded_changes": []}');
