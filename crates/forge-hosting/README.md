# forge-hosting

**Responsibility**: the hosting write commands of the API (M1 phase 8b) — group
and project create / update / visibility (MR7a), direct members of projects and
groups and protected ref rules (MR8) — each **authorized again inside its own
transaction**. A feature crate (ADR-0001 amendment): `forge-web → forge-hosting
→ forge-auth / forge-db → forge-core → forge-errors`; it runs no SQL of its own
(the `db-write-boundary` gate).

**How a command decides.** The route registry proves the action on what its
loader read and hands the proof to the handler (ADR-0015). The handler passes
the proof here; the command calls a forge-db `*_decided` command, which opens
the transaction, locks the target (and, for members, the member's row; for
rules, the rule's row), re-reads the caller's roles under `FOR SHARE` and the
caller's account row, and calls `decide` — here `Authorized::reconfirm`, i.e.
`authorize()` again for the same caller, action and target on those facts. A
role removed, a demotion, a disable or a promotion of the member committed
first is seen (denied, nothing written); one that comes later waits for the
commit. A proof for another action is `FG-SYS-0001 reason=proof_action_mismatch`;
one decided for another kind of change (an add's proof driving a change) is
`FG-SYS-0001 reason=proof_shape_mismatch`.

**Public API** (`Hosting::new(Db)`)
- Reads for the loaders (read pool): `group`, `project`, `parent`;
  `project_members` / `group_members` (keyset pages, `MAX_MEMBER_PAGE` = 100),
  `project_member` / `group_member`, `project_member_role` / `group_member_role`
  (the direct role a loader decides on), `protected_refs`, `protected_ref_state`
  (with `protection_level`: an unreadable rule counts as Owner-level, an absent
  one as none).
- MR7a writes: `create_group`, `create_subgroup`, `update_group`,
  `create_project`, `update_project`, `change_visibility`.
- MR8 writes (`MemberManage`): `add_project_member`, `change_project_member`
  (`MemberChange`), `remove_project_member`, and the same for groups. The
  member's `from` is the direct role read under the lock (a member gone
  meanwhile is `Missing`, then `FG-REPO-0010 not_member`); `removes_last_owner`
  is computed there, so the last Owner is refused by `authorize()`
  (`FG-AUTH-0003 reason=last_owner`, 403; forge-db's `FG-REPO-0010 last_owner`
  is the backstop).
- MR8 writes (`ProtectedRefManage`): `create_protected_ref` (the pattern is
  not an authorization input), `change_protected_ref` (`RuleChange`),
  `delete_protected_ref`; `from` is the level read under the rule's row lock.

**Errors raised**: whatever forge-db and `authorize()` return (FG-AUTH-0003
with `agent_token`, `above_own_role`, `last_owner`; FG-REPO-0005 / 0010 / 0013 /
0017; FG-DB-0020), plus FG-SYS-0001 (`proof_action_mismatch`,
`proof_shape_mismatch`, `proof_without_user`, `reconfirm_target_mismatch`).

**Tests**: `tests/commands.rs`, `tests/members_protection.rs` (decisions on the
locked facts, agents, last owner, stale proofs), `tests/race.rs` (a role removed
or a member promoted while a command waits on the lock it takes; a protection
deleted concurrently with a push decision is out of scope — the push decides
on one rule snapshot, ADR-0008), `tests/decided_callers.rs` (only this crate
calls forge-db's `*_decided` commands), `tests/dependency_direction.rs`.
