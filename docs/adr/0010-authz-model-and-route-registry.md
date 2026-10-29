# ADR-0010: 認可のモデルとルートの登録の仕組み

**Date**: 2026-10-26
**Status**: accepted（ADR-0015 が拡張: スコープ付きのルートの登録、`Authorized<R>` の証明、`Resource::Transfer` の移動先の型）
**Deciders**: Masaki Fujikawa（ユーザー）、Claude

## Context

認可は 1 か所で判定し、すべてのルートと git 操作を権限マトリクスのテストに載せる（CLAUDE.md §3-4）。
ルートを足したときに、マトリクスへの追加漏れを機械で検出したい。OpenAPI もコードから生成する。

## Decision

- `forge_auth::authorize(&AuthzContext, Action, &Resource) -> Decision` は純粋な関数。必要な情報（所属、実効ロール、
  トークンのスコープ、保護 ref のポリシー）は呼び出し前に `AuthzContext` へ読み込む。`Action` は `_` なしで網羅的に match する。
- 判定 = ロール ∩ トークンのスコープ ∩ 保護 ref のポリシー。見えないリソースは 404（`FG-REPO-0001`）で、存在を漏らさない。
- ロール: Guest / Reporter / Developer / Maintainer / Owner（GitLab 型）。実効ロールは、プロジェクトへの直接の所属と
  祖先のグループへの所属の最大値。グループの入れ子は再帰 CTE（深さの上限 20）で解決する。
- 可視性は `private` と `internal`（ログインした全員が読める）。匿名アクセスはない。
- **ルートの登録の仕組み**: すべてのルートを `(メソッド, パス, Action, ハンドラ)` で 1 か所に登録し、そこから
  ルーター、OpenAPI（utoipa）、`Git/docs/authz/routes.generated.json` を作る。マトリクスの要約は
  `matrix.generated.json`。ゲート `authz-coverage` が両者を突き合わせる。
- プロジェクトとリポジトリは別のテーブルにする。リポジトリは種類（`spec` / `system`）を持ち、プロジェクトは
  1 つの主リポジトリを持つ（Wiki や文書スペースを後から足せる）。

## Alternatives Considered

### ハンドラごとに権限を判定する
- **Why not**: 判定が散らばり、マトリクスと食い違う

### OpenAPI を手書きする
- **Why not**: コードとずれる

## Consequences

### Positive
- ルートの追加漏れがビルド（網羅的な match）とゲートで検出される

### Negative
- ルートの登録に専用の関数を通す規則が要る（`Router::route` を直接使わない）

### Risks
- 実効ロールの計算の誤り → マトリクスのテストで祖先のグループの所属を含むケースを検証する

## Amendment (2026-10-26, after the first review of M1 phase 5)

The independent review (authz-matrix-auditor, security-reviewer, rust-reviewer,
pr-test-analyzer) returned BLOCK; the decisions below refine this ADR.

- Signature: `authorize(&Principal, Action, &Resource) -> Result<(), ForgeError>`.
  `Principal` is built only through `Principal::authenticated(user, is_admin,
  active, Credential)` / `Principal::anonymous()` — no permissive defaults.
  `Credential` is `Session` or `Token(scopes)`.
- The 404 for resources the caller cannot see is **`FG-REPO-0005`**
  (`FG-REPO-0001` is `StoragePathRejected`). Another user's token is also 404.
- An administrator's **token** carries administrator rights only with the
  `admin` scope.
- **Tokens never mint tokens** (`TokenCreate` needs an interactive session).
- Membership changes carry the scope (project view or group role) and the
  change (`Add` / `Change { from, to }` / `Remove { from }`); the caller may not
  act on a role above their own — neither the granted role nor the member's
  current role. Project-scoped membership goes through the visibility check.
- Pushes carry the ref kind; refs outside `refs/heads/` and `refs/tags/` are
  rejected for everyone (`FG-GIT-0010`).
- Second review round (security re-review, BLOCK on one HIGH): changing a ref
  protection is its own resource `RefProtectionChange { project, from, to }`
  and nobody changes a protection above their own role (a Maintainer cannot
  lift an Owner-level protection and then force-push). Visibility changes are
  their own action `ProjectVisibilityChange` (Owner only). Membership changes
  carry `removes_last_owner`, and the last Owner can never be removed. Token
  creation carries the requested scopes; only users with administrator rights
  may request the `admin` scope. `RefKind::of(&RefName)` is the only way to
  derive a ref kind; `MembershipChange.from` is the effective role loaded from
  the database, never taken from the request.
- The matrix lives in `crates/forge-auth/src/matrix_rows.rs` (data) and
  `matrix.rs` (types and the public summary); `Action::ALL` is generated with
  the enum by one macro.
- Third review round (security re-review, BLOCK on one HIGH): when several
  protection rules match one ref, the **strictest rule wins**
  (`RefProtection::effective`, the maximum minimum role) — adding a rule can
  never loosen an existing protection. Pushes carry the `RefName` and
  `authorize()` derives the kind itself. Handed to the phases that build the
  lookup layer and the routes: protection patterns match ref names
  case-insensitively (the dev platform is Windows; production is Linux with a
  case-sensitive file system, but loose refs must not be the only defence); the
  `ProjectUpdate` request type has no visibility or protection fields (those go
  through `ProjectVisibilityChange` / `ProtectedRefManage`), checked by a test
  when the route lands.

## Amendment (2026-10-29, step D2: loading roles)

- **One role computation.** `forge_db::roles::{load_group_roles,
  load_namespace_roles}` are the only code that turns membership rows into a
  role; callers feed their result into `authorize()` and never compute or
  compare roles themselves. They return `MemberRoles { native, direct }`:
  `native` is the highest direct role over the group and its ancestor groups
  (one recursive query on the read pool, capped at `MAX_DEPTH` = 20 steps),
  `direct` the role in that group alone. Only **active** users hold a role (a
  disabled member stays a row, without a role). Projects join in step D4. The
  key is the `UserId`, not the credential.
- A chain longer than the cap (a cycle or corrupt data) or an unknown stored
  role is `FG-DB-0018` MembershipDataInvalid (`namespace_chain_too_deep` /
  `unknown_role`, runbook `membership-data`), never a silently shortened
  role. A personal namespace carries no group role (its owner is decided by
  `Resource::Namespace`); a project namespace gets only the ancestor groups'
  role until D4 adds project members.
- The permission matrix feeds roles by hand and cannot catch an inheritance
  bug; `crates/forge-web/tests/member_roles.rs`
  (`ancestor_owner_may_manage_subgroup_members`, `ancestor_reporter_may_not`,
  `a_direct_role_below_the_inherited_one_does_not_cap_it`,
  `a_disabled_ancestor_owner_may_not`, `a_non_member_may_not`) loads the
  caller's role from the database and decides with it.
- Two gates on one fact, both intentional: `Principal.active` refuses a
  disabled caller (`FG-AUTH-0004`), and the loader gives a disabled member
  no role — so a stale principal still gets none. An absent group is
  `FG-REPO-0005` and a group without a role is a 403: groups are visible to
  every signed-in user (`GroupRead`), so this reveals nothing. Project
  routes (D4) must answer an absent and an invisible project with the same
  404, as the visibility check already does.
  `authorize()` and the matrix rows are unchanged.
