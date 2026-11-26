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
| FG-KNOW-0005 NotesProjectMissing | a person's notes pointer names a project moved out of `<user>/notes` or pending deletion | see "Personal notes" |
| FG-KNOW-0006 NotesCreateRefused | notes refused (`already_exists`, `name_taken`); nothing written | read `reason` |
| FG-KNOW-0007 NotesProvisionFailed | `<user>/notes` was created but its repository or first commit could not be made; **no pointer was written** | read `step`; see "Personal notes" |

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

## Personal notes (plan 20 N1, ADR-0040 §6)

A person's notes are the **private** project `<user>/notes` in their own
namespace, pointed at by one row of `forge.user_notes` (`user_id` →
`project_id`). `POST /api/v1/user/notes` (`gstn note create`) creates the
project, provisions its repository, writes the first commit
(`.forge/doctypes/note.yml` with `owner_kind: user`, `README.md`) and only
then the pointer. A quick note (`gstn note new`) is one direct commit of
`N-YYYYMMDD-HHMMSS.md` through the simple editor's commit.

```sql
-- A person's pointer and its project's state (read-only).
SELECT n.project_id, p.deleted_at, ns.parent_id, ns.segment
  FROM forge.user_notes n
  JOIN forge.projects p ON p.id = n.project_id
  LEFT JOIN forge.namespaces ns ON ns.kind = 'project' AND ns.owner_id = p.id
 WHERE n.user_id = $1;
```

- **FG-KNOW-0005 `moved`**: the person renamed or moved the notes project
  (e.g. into a group to share it). It keeps working as an ordinary project;
  The move left a tombstone (redirect) at `<user>/notes`: the person
  releases it (`POST /api/v1/namespaces/tombstones/release`,
  `{"path": "<user>/notes"}`), then `gstn note create` makes new
  `<user>/notes` and replaces the pointer.
- **FG-KNOW-0005 `deleted`**: the project is pending deletion. Restore it
  (`gstn project restore`, within the grace period), or wait for the purge
  (it removes the pointer and frees the path) and create the notes again —
  until then the path `notes` is taken (`FG-KNOW-0006 reason=name_taken`).
- **FG-KNOW-0007**: fix the chained cause (repository-provisioning,
  git-process-failed, db-unavailable); the person deletes the half-made
  private project `<user>/notes` (human approval in production) and runs
  `gstn note create` again.
- Notes are private: nobody but the person (and administrators, who read
  every project) sees them in the project list, browse or search; sharing
  means adding members to the project.
