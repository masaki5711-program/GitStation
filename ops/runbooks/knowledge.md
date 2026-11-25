# Runbook: knowledge — the project wiki (plan 20 W1/W2, ADR-0040 §5)

A project's wiki is an **ordinary project** in the same namespace, linked by
one row of `forge.project_wikis` (`project_id` = the owner project,
`wiki_project_id` = the wiki). The owner project's members get their role on
the wiki too (one way, one hop): `forge_db::roles` (`chain_role_columns!`)
raises a role on the wiki project to the role the user holds on the owner
project itself (its membership and org bindings). The ancestors' roles are
the wiki's own (it lives in the same namespace).

## Codes

| Code | Meaning | First step |
|------|---------|------------|
| FG-KNOW-0003 WikiLinkRefused | link refused (`already_linked`, `wiki_already_linked`, `chain`, `self`, `name_taken`, `not_linked`) | read `reason`; nothing was written |
| FG-KNOW-0004 WikiProvisionFailed | the wiki project was created but its repository, first commit or protection could not be made; **no link was written** | read `step` and the chained cause; see "A half-made wiki project" |

## Checks (read-only)

```sql
-- The link of a project (either side).
SELECT * FROM forge.project_wikis WHERE project_id = $1 OR wiki_project_id = $1;
-- The audit trail of links.
SELECT created_at, action, actor_user_id, details FROM forge.audit_log
 WHERE action IN ('project.wiki_linked', 'project.wiki_unlinked')
 ORDER BY created_at DESC LIMIT 20;
```

How to confirm a user's inherited role: `GET /api/v1/projects/<wiki id>` as
that user (`role`), or `forge_db::roles::load_project_roles` in a test.
Removing the user from the owner project takes effect on the next request
(roles are computed per request, nothing is cached).

## A half-made wiki project (FG-KNOW-0004)

`POST /api/v1/projects/{id}/wiki` creates the project first (its own
transaction, `project.created`), then provisions its repository, writes the
first commit (`.forge/doctypes/wiki.page.yml`, `WIKI-0001.md`) on `main`,
protects `main` (Developer may push, no force push) and only then links it.
A failure after the creation leaves an unlinked project named in the error's
`wiki_project_id`; nobody inherits a role on it.

1. Find the event by request_id: `step` is `provision`, `seed`, `protect` or
   `link`, the cause is chained (FG-GIT-*, FG-REPO-*, FG-DB-*).
2. Fix the cause (runbooks repository-provisioning, git-process-failed,
   db-unavailable).
3. The person deletes the half-made project (`DELETE /api/v1/projects/<id>`,
   human approval for production) and runs `gstn wiki create` again, or
   creates it under another path (`--path`).

## Unlink, and deleting either side

- `gstn wiki unlink` / `DELETE /api/v1/projects/{id}/wiki` removes the link
  only; the wiki project and its history stay, and the owner's members lose
  the inherited role on the next request.
- Deleting the owner project (grace period) keeps the wiki and the link; a
  restore brings both back. **The owner's members keep their inherited role on the
  wiki during the grace period**: to cut access at once, unlink the wiki
  first (or remove the members). The purge of either project removes the link
  row; the other project stays.
- Moving either project (`gstn project move`) keeps the link. A wiki moved
  out of the owner's namespace inherits only the owner project's own roles
  (its membership and org bindings), never more than the owner gives;
  unlink and link again in one namespace if that is not intended.

## Moving a GitLab wiki (Q8)

No tool: `git clone <gitlab>/<path>.wiki.git`, push it to the wiki project
(Developer), then `gstn doc fmt` adds the front matter (`doctype: wiki.page`,
`id: WIKI-NNNN`).
