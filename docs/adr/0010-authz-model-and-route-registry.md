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

## Amendment（2026-10-29、手順 D3: トークンと認証）

`forge-auth` README の引き継ぎ事項 7 と 9 は ADR-0019 で決まった。

- **引き継ぎ事項 7（無効化するユーザーが最後の Owner／管理者の場合）**: 既定で拒否する（`FG-AUTH-0009`
  AdministrationConflict、409、`reason` = `last_admin` / `last_owner` / `admin_exists`）。CLI の
  `forge admin user disable --force` だけが通し、その結果の孤立は `forge doctor` が `FG-AUTH-0010`
  AdministrationOrphaned（warn）で報告する。「最後の管理者」は有効な人間の管理者だけを数える。最上位のグループは
  有効な直接の Owner を要し、サブグループは継承してよい（ADR-0019 §10〜12）。
- **引き継ぎ事項 9（Basic 認証のパスワード）**: Basic のパスワードは **トークンだけ**。ユーザー名は無視し、ログに出さない。
  認証のヘッダーが 2 つあれば 401（`ambiguous`）（ADR-0019 §7）。**D3 MR3（2026-10-30）で実装して閉じた**: `forge-web` の
  `auth.rs` が公開でない経路ごとにガードの前で Bearer／Basic のトークンをプリンシパル（`Credential::Token(scopes)`）にする。
  パスワードによる Basic（`Credential::Session` 相当）の経路は存在しない。細部は ADR-0019 の Amendment（2026-10-30、MR3）。
- CLI によるトークンの発行は HTTP の経路ではないので `authorize()` を通らない（ホストの操作者が信頼の起点）。
  不変条件（`admin` スコープは管理者だけ、ボットと無効化済みのユーザーには発行しない）は `forge_db::tokens::issue` が
  強制する。「トークンはトークンを発行しない」は変わらない。`authorize()` と権限のマトリクスは変わらない。

## Amendment（2026-10-31、手順 D4 MR2: プロジェクトの役割の読み込み）

- **`forge_db::roles::load_project_roles(db, ProjectId, UserId) -> MemberRoles`** を足す。役割の計算は引き続き `roles` の 1 つの再帰の問い合わせだけで行う。
  - `direct` は `project_members` の役割（直接のメンバー）。
  - `native` は、`direct`、プロジェクトの名前空間の祖先のグループでの直接の役割、そして **個人の名前空間の持ち主には Owner**（回答 2）の、最も高いもの。
    個人の名前空間の持ち主が Owner になるのは、その名前空間の **下のプロジェクト** についてだけで、名前空間そのものには役割を持たない（D2 のとおり、
    `Resource::Namespace` で決める）。レビュー後の修正: この Owner は、個人の名前空間が **プロジェクトの直接の親**（鎖の 2 段目で、起点が
    プロジェクト）のときだけ与え、bot には与えない。D5 の移動で個人の名前空間の下にグループなどが入っても、その下には継承しない
    （テスト `a_user_namespace_above_a_group_gives_no_role`、`a_bots_personal_namespace_gives_the_bot_no_role`）。
  - 有効なユーザーだけが役割を持つ（直接・継承・個人の名前空間のいずれも）。上限 20 段、壊れた鎖や未知の役割は FG-DB-0018（D2 と同じ）。
  - プロジェクトの名前空間がなければ FG-REPO-0005（`resource=project`、`reason=absent`）。
- `load_namespace_roles` をプロジェクトの名前空間に使うと、`load_project_roles` と同じ答えになる（D2 の「祖先のグループだけ」の暫定は終わり）。
- `forge_db::projects::access(db, ProjectId) -> ProjectAccess { visibility, pending_deletion }` と `load_project_roles` の `native` で、
  forge-auth の `ProjectView { visibility, role, pending_deletion }` を組み立てる。`pending_deletion` は `deleted_at` があること（D5 が書く）。
  forge-db は forge-auth に依存しない（依存の向き）ので、組み立ては呼び出し側（プロジェクトの経路を入れる後の手順）が行う。`authorize()` と権限のマトリクスは変わらない。
- 継承された Owner がいる間は、最後の直接の Owner を外すことを拒まない（回答 2、MR4 で実装）。

## Amendment（2026-10-30、M1 phase 8 MR2: 認可の証明と新しい Action）

- 判定の点は引き続き `authorize()` の 1 つだけである。その上に `prove()` → `Authorized<R>`（ADR-0015）を足したが、`prove()` は `authorize()` を
  呼ぶだけで、規則を持たない。matrix のテストは引き続き `authorize()` を直接呼び、`prove_denies_exactly_like_authorize` が両者の一致を確かめる。
- 新しい Action `PathResolve`（`read_api`）と `WebShell`（公開）を足し、`Resource::Unresolved` / `Resource::Resolved` を足した（細部は ADR-0015 の
  同じ日付の Amendment）。公開の Action は `Health` と `WebShell` の 2 つになった。どちらもルートがまだないので `app::PLANNED` に入れ、
  `gen-authz` で `docs/authz/*.generated.json` を作り直した（OpenAPI は変わらない）。

## Amendment（2026-11-01、M1 phase 8b MR6: 本文のあるルート、`UserSelfRead`、OpenAPI の schema）

書き込みの API（MR7a / MR8）は、認可の入力（与えるロール、保護の段階、可視性、親）が本文にある。そこで、本文を読み込みの **前** に読み、
大きさを抑え、型に直す登録の口を足した（`feat/web-api-request-body`）。決めた点は次のとおり。

- **`Registry::route_with_body::<L, B>`**（`route_with` と同じ並びに、本文の上限を足した形）。順序は **認証 → 本文 → 読み込み → `prove()` →
  `admit()` → ハンドラ** で、前の段で止まれば後の段は動かない。本文は `forge_web::body::read_json` が読む:
  `Content-Type: application/json`（`charset=utf-8` だけ可）でなければ FG-WEB-0006 `reason=content_type`、ルートの上限（1 〜 64 KiB、登録時に
  検査。サーバー全体の `body_limit_bytes` も超えない）を `Content-Length` で超えれば読まずに、流れの途中で超えればそこで FG-WEB-0009
  `RequestTooLarge`（413、`declared=true|false`）、止まった本文はサーバーの読み取りの無通信の上限（FG-WEB-0002 `operation=request_read`）、
  JSON は厳格（`#[serde(deny_unknown_fields)]`）で、誤りは FG-WEB-0006 の `reason=json_syntax | unknown_field | missing_field | type`。
  ログに出すのは欄の名前と位置だけで、**値は出さない**（serde のメッセージは値を引用するので使わない）。
- **MR4 の must はそのまま**: 本文は `Dispatched<R, B>` の中でだけハンドラに届き（`input()`）、`Dispatched` は引き続き `admit()` だけが作る
  （`compile_fail` の doctest を 1 つ足した）。`admit()` は本文があっても `proof.action() == def.action` を確かめる。安全な動詞（GET / HEAD）、
  公開の Action、範囲外の上限、重複は起動時の FG-WEB-0004。`BodyLoader::load(state, parts, &B, viewer)` は判定しない（本文が見えないものを
  指していても、ないものと同じく `authorize()` が答える）。読み込みと判定の道は `route_with` と同じ 1 つの関数（`registry::decide`）にまとめた。
- **新しい Action `UserSelfRead`**（`GET /api/v1/user`、`read_api`）。`Resource::Instance` に対して、有効なサインイン済みの利用者すべてを許す。
  matrix の行: 許可 5（Developer、管理者、エージェントのトークン、`read_api` / `api` のトークン）、拒否 5（匿名 FG-AUTH-0001、無効 2 行
  FG-AUTH-0004、`read_repository` / `write_repository` だけのトークン FG-AUTH-0002）。`is_agent` はまだ判定の入力ではない（MR8）ので、
  エージェントの行は人のトークンと同じに決まる。応答の `{id, username, display_name, is_admin, is_agent, token_scopes}` は、認証が同じ文で
  読んだ値（`TokenIdentity` に `username` / `display_name` を足した）で、2 回目の照会はしない。
- **OpenAPI**: 登録に schema を足した（`RouteDef::request` / `response`、utoipa の `ToSchema` から）。本文のあるルートは `requestBody` と
  `x-forge-max-body-bytes`、`Registry::responds::<T>(verb, path)` を付けたルートは `200` の本文を持つ。`{id}` は正の int64。
  `ProjectBody`（`project_view` が組み立てる唯一の形）、`UserSelf`、`Resolution` を載せた。生成は決定的（すべての map を並べ替える）で、
  同じ名前に 2 つの形があれば `x-forge-schema-conflicts` として出し、テストが落ちる。utoipa（5.x）と aes-gcm（0.10.3）は
  `cargo deny check` と `cargo audit` を通してから入れた。

## Amendment（2026-11-01、M1 phase 8b MR7b: 一覧 `ProjectList` / `GroupList`）

- **新しい Action `ProjectList` / `GroupList`**（`GET /api/v1/projects`、`GET /api/v1/groups`、`read_api`）。`Resource::Instance` に対して、
  有効なサインイン済みの利用者すべてを許す（匿名 FG-AUTH-0001、無効 FG-AUTH-0004、`read_repository` / `write_repository` だけのトークン
  FG-AUTH-0002。matrix の行は許可 6 / 5、拒否 5 / 3）。一覧そのものを許すだけで、**行ごとにもう一度 `authorize()` が決める**:
  プロジェクトは `prove(ProjectRead)`（`project_view` と同じ証明。private の Guest、削除待ち、見えないものは出ない）、グループは
  `authorize(GroupRead, Resource::Group { role })`。SQL では絞らない（計画の L1）ので、一覧と 1 件のルートの判定は食い違えない。
  `GroupRead` の行を 3 つ足した（`api` のトークン許可、無効 FG-AUTH-0004、`read_repository` FG-AUTH-0002）。`GroupRead` 自身のルートは
  MR7a なので `PLANNED` に残る。
- **読み込み**: forge-core の `ProjectPageLoader::project_page(after, viewer, max_rows)` / `GroupPageLoader::group_page`（id の昇順、
  `after` より後、見えるかどうかに関わらずすべて）。forge-db は 1 ページ 1 文（`forge_db::lists`、行の形は 1 件の読み込みと同じ）。
  1 ページで読むのは最大 1000 行（走査の予算、1001 行目は続きがあるかだけを示す）。予算が先に尽きれば短い（空のこともある）ページを
  `has_more: true` で返す。`limit` は 1 〜 100（既定 20）。
- **カーソル**: 最後に消費した id（見せた行か、走査した行）を AES-256-GCM で封じる（`purpose = "projects.list:user=<id>"` /
  `"groups.list:user=<id>"`）。見えない行の id は漏れず、他人や他の一覧のカーソルは FG-WEB-0006 `cursor_invalid rule=tag`。
  `forge serve` は `api.cursor_keys_file` がなければ起動しない（FG-SYS-0002、Runbook `config-invalid`）。
- **同時実行の上限と期限**（レビュー指摘の修正）: 一覧は `limit` に関わらず最大 1001 行を走査するので、同時に走る数を抑える。全体で
  `api.list_max_concurrent`（既定 16）、1 人あたり `api.list_max_concurrent_per_principal`（既定 2、総数以下）。超えたら待たせず
  FG-WEB-0008 `reason=list_concurrency`（`scope=global|principal`）を 503 + `Retry-After: 1` で返す（既存の ServerBusy を使う。枠は読み込みの間だけ
  持ち、成功・失敗どちらでも返す）。設定は起動時に検証（FG-SYS-0002）。カーソルには発行時刻（秒）を封じた本文の中に入れ（形式の版は 2、
  AEAD の対象）、`api.cursor_max_age_s`（既定 86400）より古いものは FG-WEB-0006 `cursor_invalid rule=expired`。遅い `projects.page` /
  `groups.page` の手当ては runbook `connections`。
- 残る漏れ: ページの件数から「この区間に見えない行がある」ことは分かる（L1 で受け入れた。SQL の上位集合 L2 への切り替えは M2 で
  1 走査あたり ~10k を超えたときに、この ADR の Amendment で決める）。
- OpenAPI: `ProjectPage` / `GroupPage` / `ListedGroup` の schema と、`Registry::queries` で `limit` / `cursor` の query の parameter を載せた。

## Amendment（2026-11-01、M1 phase 8b MR8: メンバーと保護 ref のルート）

- 新しい Action: `MemberList`（プロジェクトは `ProjectRead` と同じ判定、グループはサインインした全員）と `ProtectedRefList`
  （`ProjectRead` と同じ）。どちらも `read_api`。`MemberManage` / `ProtectedRefManage` をルートに載せ、`app::PLANNED` から外した。
- 新しい文脈: `ProjectMemberCtx` / `GroupMemberCtx`（id はプロジェクトかグループとメンバーの組）、`RefProtectionCtx`（id は
  プロジェクトと規則の組）。`MembershipChange::from` はメンバーの**直接の**役割（変更の対象の行）で、データベースから読む。
- `MembershipChange::Missing { to }`: 直接のメンバーでない利用者の変更・削除。`to`（削除では Guest）を与える最小の変更として判定し
  （Maintainer が必要）、メンバーを管理できる人だけが「メンバーでない」（FG-REPO-0010 `not_member`）を知る。
- `removes_last_owner` は書き込みのトランザクションの中で計算する（レジストリの判定では常に `false`）。最後の Owner の削除・降格は
  `authorize()` が FG-AUTH-0003 `last_owner`（403）で拒否し、forge-db の FG-REPO-0010 `last_owner` は予備の守り。
- 権限の表: 自分の昇格（Developer が自分を Owner に、Maintainer が自分を Owner に）、最後の Owner の削除と降格、Maintainer の
  エージェント、Guest のメンバー一覧、保護より低い Developer、`read_api` / `api` / `write_repository` のトークン、無効な
  アカウント、削除待ちのプロジェクトの許可と拒否の行を足した。

## Amendment（2026-11-01、ADR-0023）

Context の「リポジトリは種類（`spec` / `system`）を持ち」は、ADR-0023 で置き換えた。リポジトリに種類の区別はない（`repositories.kind` の列は残るが使わない）。
プロジェクトが 1 つの主リポジトリを持つ構造は変わらない。

## Amendment（2026-11-15、計画 15 MR 3: グループの可視性、一番狭いものを取る規則）

- **実際に効く可視性**: グループは `groups.visibility`（`private` / `internal`、計画 15 MR 2 の列）を持つ。グループとプロジェクトに効く
  可視性は、自分から最上位までの鎖で**一番狭いもの**（`internal` のグループやプロジェクトも、上に `private` のグループがあれば非公開）。
  読み込みの層（`forge_db::resolve` / `lists` / `groups::load` / プロジェクトの親、書き込みのトランザクションのロック）が鎖を 1 文で
  たどり、forge-core の `ProjectAccess::visibility`（実際の値）と `GroupSight { visibility, descendant }` を返す。判定は
  `authorize()` だけ。鎖の値が読めない・グループの行がないときは非公開とみなす（漏らさない側に倒す）。
- **見える人**: 鎖のどこかで役割を持つ人（直接・継承）、管理者、実際の可視性が `internal` ならサインインした人とボット（サービス
  アカウントは役割があるときだけ。プロジェクトの `sees_internal` と同じ。今まで全員に見えていた internal のグループも、役割のない
  サービスアカウントには見えなくなった）。判定は `authorize::group_seen`（`Full` / `PassThrough` / `Hidden`）の 1 か所。
- **通り道**: ほかに見えないが、下のグループか削除されていないプロジェクトに直接の役割を持つ人には、グループの身元だけ見せる
  （`GroupRead` と、そのパスの `PathResolve` だけ。本文は `pass_through: true` で、id・親・パス・名前・可視性・作成日時は出し、
  説明は空、役割は null）。メンバー、ほかの子、書き込みは見えないものと同じ 404。
- **見えないとき**: 見えないプロジェクトと同じ `FG-REPO-0005`（404、`reason=group_not_visible` / `group_pass_through` /
  `service_account_not_member`）。`GroupRead`、`MemberList`、`MemberManage`、`SubgroupCreate`、`ProjectCreate`、`GroupUpdate`、
  `AuditRead`、パスの解決、移動先のグループ（`TransferTarget::Group` は `sight` を持ち、通り道でしか見えない先は
  `target_not_visible`）。一覧は行ごとに `GroupRead` で決める（SQL では絞らない）。プロジェクト・リポジトリ（clone を含む）は
  実際の可視性の `ProjectView` で今までどおり決まる。
- **書き込みの中の再判定**: 書き込みのトランザクションは役割だけをロックして読み直し、可視性はルートが読んだもの
  （`GroupCtx::sight`、プロジェクトの作成は親の `sight`）のまま `reconfirm` する。メンバーと移動の書き込みは
  `GroupSight::ROLE_ONLY`（非公開・下に役割なし）で決める。書き込みはどれもグループの役割（か管理者）を要るので、許すかどうかは
  変わらない（競合で役割を失った人への答えが 403 でなく 404 になることがあるだけ）。
- **新しい Action `GroupVisibilityChange`**（`PATCH /api/v1/groups/{id}/visibility`）: グループの Owner（管理者は通る）、人だけ、
  **web のセッションだけ**（トークンは `FG-AUTH-0002 reason=session_required`、エージェント・サービスアカウント・ボットは
  `FG-AUTH-0003`）。`preview: true` は判定して件数を数えるだけで書かない（画面が狭める前に「n 件のプロジェクトが非公開として
  扱われます」と確かめる）。書いたら `group.visibility_changed` の監査の行（`from` / `to` / `affected_projects`、イベントなし）。
  狭めても下の行は書き換えない。
- **親より広げない**: 鎖の上が非公開のところに `internal` を求める変更（グループの可視性、`internal` のサブグループ・プロジェクトの
  作成、プロジェクトの可視性）は、書き込みのトランザクションの中で判定のあとに `FG-REPO-0033`（VisibilityAboveParent、409、
  操作の誤りで runbook なし）で断る。
- **移動で広げない**（レビューの指摘）: プロジェクトかグループが親を変えるとき、元の鎖が非公開なら、動かす行の保存値を
  移動の前に `private` に狭める（`roles::narrow_before_move`、元の鎖での実際の値）。そうしないと、非公開のグループの下に
  `internal` で保存された行が、internal の親へ移ると急に見えるようになる。下の子孫は保存値のまま（狭めた行の下なので、
  一番狭いものを取る規則で非公開のまま）。名前だけの変更（親が同じ）は何も変えない。
- **上の鎖のロック**（レビューの指摘）: グループとプロジェクトの可視性の変更は、`internal` を書く前に上のグループの行を
  `FOR SHARE` でロックしてから鎖を読む（`roles::chain_private_locked_on`）。上を狭める変更（その行の `FOR UPDATE`）と
  同時に走っても、待つか先の結果を見るので、非公開の親の下に `internal` の子が新しく書かれることはない。サブグループと
  プロジェクトの作成は親の名前空間を `FOR SHARE` でロックするだけで、上のグループの行はロックしない（同時に親を狭めたとき、
  作成した行が親より広く保存されることがある。実際の値は非公開のままで、`forge doctor groups.visibility` が数える）。
- **作成**: グループを作るときは `visibility` を必ず明示して書く（列の既定 `internal` は広いほうなので頼らない）。API の既定は
  `private`（計画 15 §6 の 2）。
- **運用**: `forge doctor` `groups.visibility`（info、FG コードなし）が、非公開のグループの下に `internal` で保存された
  グループと生きているプロジェクトの数を出す（上を広げると一度に見えるようになる）。
- 権限の表: `matrix_rows_group_visibility.rs`（private / 通り道 / internal のサービスアカウント、各 Action の許可と拒否、
  `GroupVisibilityChange` の Owner・管理者・Maintainer・トークン・エージェント・サービスアカウント・ボット）。
