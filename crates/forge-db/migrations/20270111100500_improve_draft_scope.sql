-- Plan 17 P17-5 (ADR-0035 Amendment P17-5): the token scope `improve_draft`
-- of `pfs-ai-bot`, PFS's integration (a service account whose only
-- membership is the `pfs-ai` group). authorize() decides such a token in
-- its own arm: read its member projects, push refs/heads/improve/*, open
-- draft merge requests from such a branch, comment — nothing else.
--
-- 1. tokens_scopes_known gains it (the variant and its CHECK land in the
--    same MR; replaced at once, the table is small).
-- 2. tokens_improve_draft_alone: the scope is never issued with another
--    one (forge-db's tokens::issue refuses it first,
--    `improve_draft_not_alone`; this keeps a writer that skips forge-db
--    from widening it).
ALTER TABLE forge.tokens DROP CONSTRAINT tokens_scopes_known;
ALTER TABLE forge.tokens ADD CONSTRAINT tokens_scopes_known CHECK (
    cardinality(scopes) >= 1
    AND scopes <@ ARRAY['read_repository', 'write_repository', 'read_api', 'api', 'admin',
        'merge_execute', 'merge_approve', 'user_email', 'user_employee_number', 'act_as',
        'improve_draft']::text[]);
ALTER TABLE forge.tokens ADD CONSTRAINT tokens_improve_draft_alone CHECK (
    NOT ('improve_draft' = ANY (scopes)) OR scopes = ARRAY['improve_draft']::text[]);

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20270111100500, "guarded_changes": []}');
