# ADR-0025: 代理実行（act-as）— PM ツール（kanflow）が本人として行う操作

**Date**: 2026-11-05
**Status**: accepted（2026-11-09。accepted の条件 1〜3 を満たした — 末尾「accepted の条件の確認」。方式は計画 11 §3.5 の案 B で、利用者が承認済み（2026-11-02、§10 Q9 を含む）。ADR-0017 §4.0・§4.2 の代理の部分、ADR-0016 §9 の `FG-INT-0006` の範囲、ADR-0024 §12 の「代理」の列を、この ADR が具体化し、食い違う点はこの ADR が優先する（末尾「他の ADR との関係」））
**Deciders**: Masaki Fujikawa（ユーザー）、Claude

## Context

kanflow（社内の PM ツール）の画面で人が「承認」を押したとき、forge は **押した本人** として `authorize()` で判定し、監査にも本人を残す必要がある（計画 02 §5.3、F-36、利用者の決定 2026-11-01）。kanflow はサービスアカウント（ADR-0019 Addendum S0、`users.kind = service_account`、連携ごとに 1 つ）のトークンで forge を呼ぶので、「サービスアカウントが誰の代わりに呼んでいるか」を forge に伝える仕組みが要る。

前提として確かめたこと（作業ツリー `D:\APP\GitStation-wt-s1`、2026-11-05）:

- 監査の行には代理の列が予約済み: `actor_kind = 'user_via_service_account'`、`actor_user_id`（本人）、`actor_via_id`（サービスアカウント）、`credential_id`、`act_as_claimed`（主張された相手の鍵つき HMAC）、`act_as_key_id`、`via_assertion_id`（`20261027100000_events_and_audit_log.sql`、ADR-0016 §1・§9）。CHECK は代理の行に `credential_id`・`act_as_claimed`・`act_as_key_id` を必須にしている。
- 今の `Credential` は `Session` と `Token` だけで、代理の資格情報の種類がない。イベントの履歴（I1c）と開発状況（I1d）は `Forge-Act-As` の付いた要求を `FG-AUTH-0003 reason=act_as_refused` で断っている（`forge-web` の `ACT_AS_HEADER = "forge-act-as"`、`IntegrationFeedCtx { act_as }`、行は `matrix_rows_feed.rs`・`matrix_rows_dev_status.rs`）。
- `MergeRequestApprove` / `Unapprove` / `Merge` と、マージの条件を変える設定（`MergeSettingsManage`・`ApprovalRuleManage`・`CodeownersEnforcementToggle`）は **人だけ**（`Action::is_people_only`、ADR-0024 §8.4）。承認は `(head_sha, target)` に結びつき、作成者は FG-REVIEW-0013 で断られる（ADR-0024 R5）。
- トークンのスコープに `act_as` はまだない（`tokens_scopes_known` は `read_repository`〜`user_employee_number`、`20261118100000`）。
- `forge.users.employee_number` は Keycloak の claim から入り、一意（`users_employee_number`）。サービスアカウントとボットは持てない（`users_service_account_no_personal_data`）。連携の履歴の `actor.user.id` は forge の利用者の整数の ID。
- エラーコード: **`FG-AUTH-0019` は S0 が ServiceAccountTokenExpiring で使った**（計画 11 §4.4 の「AUTH-0019」は空いていない）。`FG-AUTH-0020` は空き。`FG-INT-0006` は空き（計画 02 §9 が「代理の相手」として予定）。`git_station/Git` の `main` の `errors.toml` にも `FG-AUTH-002x` はない。
- 自動マージの既定は `full`（2026-11-03 の利用者の決定、計画 04 M-5・決定 8、ADR-0017 Amendment 2026-11-03）。保護ブランチでは人間の承認が 1 件以上要る（ADR-0017 決定 9③）。**代理実行が、自動化が人間の承認を手に入れる道になってはならない。**

脅威（ADR-0017 の脅威のモデルから、代理に関わるもの）:

| # | 脅威 | 守り |
|---|---|---|
| T1 | 1 つのサービスアカウントが、多くの人の承認を作る（ADR-0017 (c)） | 状態を変える代理の操作には、本人が forge のセッションで押した 1 回限りの主張が要る |
| T2 | 侵害された kanflow が主張を作る・使い回す・別の MR / SHA に使う・本人に別の MR を押させる（ADR-0017 (e)） | 主張は forge だけが作り、操作・リソース・SHA・target の ref・連携・本人に結びつき、HMAC で封じ、1 回で消える。確認の画面は forge のデータだけを表示する |
| T3 | 代理が権限の昇格の道になる（計画 11 §8） | 判定は 1 か所（`authorize()`）。許可リストは既定で閉じ、読み取りはサービスアカウントのロールと本人のロールの小さい方 |
| T4 | 自動化（kanflow の AI・自動完了・forge の自動マージ）が人間の承認を得る（計画 04 決定 8） | 主張は人の操作でしか発行されない。代理の資格情報を作れるのは HTTP の入口だけで、エージェントのトークンは代理できない。代理の承認は新しい head へ持ち越さない |
| T5 | 社員番号（個人情報）がログ・監査・封印に流れる | 監査は HMAC だけ、ログはヘッダーを伏せる |
| T6 | DB を読める・書ける者が主張を取り出す・作る | 主張の秘密は claim のときに初めて作り、DB には鍵なしでは何も導けない HMAC だけを置く（§5.1） |

## Decision

**計画 11 §3.5 の案 B**: サービスアカウントのトークン + `Forge-Act-As` で本人を名指し、**状態を変える操作には、本人が forge の確認の画面で 1 回押して forge が発行した主張（`Forge-Assertion`）を必須にする**。読み取りは主張なしで、許可リストの読み取りだけを、**サービスアカウントのロールと本人のロールの小さい方** で許す。判定は `authorize()` の 1 か所。M3 で代理できる変更は **承認と承認の取り消しだけ**（結びつけの変更は X3 で足す。マージ・設定は不可）。

### 1. 本人の指し方（`Forge-Act-As`）

```
Forge-Act-As = "user:" 1*19DIGIT                                   ; forge の利用者 ID（連携の履歴の actor.user.id、claim の sub.user_id と同じ値）
             / "employee:" 1*64( ALPHA / DIGIT / "." / "_" / "-" )  ; 社員番号（forge.users.employee_number と完全一致。大文字小文字を区別する）
```

- **メールでは指さない**（ADR-0021: メールだけで利用者を結ばない）。`user:<email>` の形（計画 02 §5.3 の旧形、ADR-0017 §4.2）は `FG-INT-0006 reason=email_form` で断る（黙って別の形として読まない）。
- 計画 11 §0.3 の「`usr_…`」の不透明な ID は forge にまだない（外に出している利用者の ID は整数）。不透明な ID を外に出す版で `user:usr_…` を **足す**（整数の形は v1 の間は残す）。
- kanflow の推奨の使い方: 読み取りは `employee:`（kanflow の `User.employeeNumber`、計画 02 §8.4 K-7）、承認は claim の `sub.user_id` を `user:` で送る（主張の本人と必ず同じになる）。
- **検証**（forge-db の解決、`authorize()` の前。どれも `FG-INT-0006`、403、応答の本文は同じ。`reason` はログだけ）:

  | `reason` | 条件 |
  |---|---|
  | `missing_value` / `malformed` | 空、文法の外、前後の空白、`,` を含む（2 つの値の連結） |
  | `multiple_headers` | `Forge-Act-As` が 2 つ以上 |
  | `email_form` | `user:` の後ろに `@` を含む |
  | `unknown` | その ID・社員番号の利用者がない（forge に一度もサインインしていない人を含む） |
  | `ambiguous` | 社員番号の問い合わせが 2 行以上を返した（一意の索引が壊れた・外された場合）。**閉じる側に倒し、どちらの人としても扱わない**。alert を兼ねる（ログ `forge.auth.act_as` の `reason=ambiguous` を runbook `act-as` が「データの破損」として扱う） |
  | `not_human` | 相手がサービスアカウント（自分自身を含む）かボット |
  | `disabled` | 相手が無効化されている |

  「存在しない」「無効」「人でない」を同じ応答にして、サービスアカウントに利用者の状態を区別させない（ADR-0016 §9）。ADR-0016 §9 が `FG-INT-0006` に含めていた「権限がない」は **含めない**: 相手が決まった後の権限の不足は `authorize()` の通常のコード（`FG-AUTH-0003`・`FG-REPO-0005`・`FG-REVIEW-0013`）で、どの規則で断ったかがログで分かる形にする。
- **社員番号を別の人に移さない**（S2 のマイグレーション。**accepted の条件**）: 無効化された利用者の `employee_number` は NULL にも別の値にもならない（トリガー `users_employee_number_kept_when_disabled`、保護の対象）。一意の索引 `users_employee_number` は無効の利用者の行も数えるので、退職者の番号は新しい人に付かない。新しい人の Keycloak の claim が既存の（無効を含む）利用者の番号と衝突したら、サインインは既存の身元の衝突のコード（`FG-AUTH-0014` IdentityLinkConflict）で止まり、黙って付け替えない。代理の解決は `employee_number` を完全一致で引き、ちょうど 1 行のときだけ進む（上の `ambiguous`）。
- 失敗した解決はサービスアカウントごとに流量を制限する（1 分に 60 回を超えたら `FG-WEB-0010`）。社員番号の存在の当て推量を遅くする（kanflow は社員の一覧を既に持つので、守りの主目的は誤設定の早期の検出）。

### 2. 誰が代理できるか（資格情報の条件）

`Forge-Act-As` を受け付けるのは、次をすべて満たす要求だけ。満たさなければ断り、**ヘッダーを無視して本人として続けることはしない**。

| 条件 | 外れたとき |
|---|---|
| `Authorization: Bearer` の **サービスアカウント** のトークン（セッション・人の個人のトークン・ボットは不可） | `FG-AUTH-0003 reason=act_as_requires_service_account` |
| トークンが新しいスコープ **`act_as`** を持つ | `FG-AUTH-0002`（`required_scope=act_as`） |
| トークンが **エージェントと宣言されていない**（`is_agent = false`） | `FG-AUTH-0003 reason=agent_token` |
| サービスアカウントが **有効な連携** のアカウント（`integrations.service_account_id`、連携ごとに 1 つ） | `FG-AUTH-0003 reason=act_as_no_integration` / `integration_disabled` |
| 鍵（§5.2）が設定されている | `FG-AUTH-0003 reason=act_as_not_configured`（doctor `act_as.keys` が理由を出す） |

- **`act_as` スコープ**: 追記のマイグレーションで `tokens_scopes_known` に足す。ほかのどのスコープも満たさず、どのスコープにも満たされない（`api`・`admin` でも）。`tokens::issue` の不変条件に「`act_as` はサービスアカウントだけ」（`act_as_requires_service_account`、FG-AUTH-0008）と「`is_agent` と同時に持てない」（`act_as_agent_token`）を足し、`authorize()` も同じことを確かめる（二重の守り。ADR-0019 §8 と同じ形）。
- 代理の操作に要るスコープは「`act_as` + その Action のスコープ」。承認なら `act_as` と `merge_approve` の両方（`api` は `merge_approve` を満たさない、ADR-0017 §4.0）。

### 3. 範囲 = サービスアカウントの範囲 ∩ 本人の権限

- **プロジェクトと操作**: プロジェクトは、その連携に結びつけたプロジェクト（`integration_bindings`、`via_bound`）だけ。操作は §4 の許可リスト ∩ トークンのスコープ。
- **ロール**（Action の規則で 2 通り。`authorize()` の代理の arm が規則ごとに明示的に選ぶ）:
  - **`ReadWithoutAssertion`（主張なしの読み取り）**: 実効のロール = **min（サービスアカウントの所属のロール、本人の `native_role()`）**、可視性は **両方に見えること**。結びつけでサービスアカウントは Reporter になる（ADR-0016 Amendment I1a の 2）ので、代理の読み取りは Reporter を超えない。本人が Maintainer でも、代理の読み取りで Reporter に見えないものは見えない。これで「主張なしで本人を名指す」ことが、サービスアカウント自身が読めない物を読む道にならない（本人を取り違えた・偽った読み取りでも、読めるのはサービスアカウントが元々読める範囲の部分集合）。
  - **`RequiresAssertion`（主張が必須の変更）**: 実効のロール = 本人の `native_role()`（サービスアカウントの Reporter で上限を付けない。付けると承認が通らない）。**サービスアカウントの Reporter の上限を外すのは、本人がその操作・そのリソース・その SHA を forge のセッションで確かめた主張があるときだけ**。可視性は本人の可視性と `via_bound`。
- **本人の管理者の権限は通さない**: サービスアカウントのトークンは `admin` スコープを持てない（S0）ので、代理の主体の `admin_rights()` は常に false。所属のない管理者の代理は、見えないプロジェクトとして断る。
- **結びつけていないプロジェクト**は、本人が見えても見えなくても `FG-REPO-0005`（`reason=act_as_unbound`。存在を漏らさない）。
- 本人の判定は通常の arm をそのまま通る（自己承認の FG-REVIEW-0013、ロールの不足の FG-AUTH-0003、削除待ちの FG-REPO-0006）。経路ごとの分岐は作らない。kanflow の出どころの能力（ADR-0017 §4.4）は、入ったときに `role_for` の規則どおりに本人の側で数える（読み取りの min の規則は変わらない）。

> **例外**（Amendment X3 の 5）: `RepositorySearch`（`ListAsPerson`）は min ではなく本人として判定する（結ぶ前のリポジトリを見せるため。名前だけ）。

### 4. 代理で使える Action の許可リスト（既定で閉じる）

`Action::act_as_rule(self) -> ActAsRule`（`_` のない全列挙の `match`。新しい Action はコンパイルが `ActAsRule` を選ばせ、選ぶまで入らない）。M3（S2）の時点:

> **X3 で広げた**（Amendment X3、下）: `IntegrationBindingManage`（主張 `binding_set`）・`IntegrationBindingRemove`（主張 `binding_remove`）・
> `RepositorySearch`（主張なし、**本人として** 判定する `ListAsPerson` — §3 の min の規則の例外。名前だけ、流量の制限と監査つき）。この表は S2 の時点のもの。

| Action | 規則 | 許す理由 |
|---|---|---|
| `ProjectRead` | 主張なしで可（min のロール） | kanflow が「この人にこのプロジェクトが見えるか」を表示に使う。§3 の min の規則で、読めるのはサービスアカウントが既に読める範囲の部分集合なので、何も広がらない |
| `MergeRequestRead` / `MergeRequestList` | 主張なしで可（min のロール） | F-36 の画面に承認の状態と「この人が承認できるか」（`viewer` の欄、§9）を出すため。理由は上と同じ |
| `MergeRequestApprove` | **主張（intent `approve`）が必須**（本人のロール） | F-36（利用者の決定 2026-11-01）: 技術者でない承認者が kanflow から承認する。本人が forge の確認の画面で MR・SHA・target を見て押した主張があるときだけ、本人の承認として数える |
| `MergeRequestUnapprove` | **主張（intent `unapprove`）が必須**（本人のロール） | 承認できる経路では取り消しもできないと、誤って押した人が forge の画面へ行く以外に直せない。取り消しは権限を減らす操作だが、他人の承認を消す道にしないため主張を要する（本人の承認しか消せないのは通常の arm） |

**代理の承認は新しい head へ持ち越さない**: ADR-0024 §8.2 の `may_carry_approvals`（R6 の `reset_approvals_on_push = false` で承認を写す設定）は、`credential_origin = act_as_assertion` の承認には **プロジェクトの設定に関係なく** 効かない。主張は特定の `head_sha` を本人が見た証拠であり、別の head の承認にはならない。新しい head では本人がもう一度確認の画面で押す。テスト `act_as_approval_is_never_carried_to_a_new_head`（`reset_approvals_on_push = false` のプロジェクトで、セッションの承認は写り、代理の承認は写らない）。

**許可リストの外（代理では常に不可。理由）**:

| Action | 理由 |
|---|---|
| `MergeRequestMerge`（とマージキュー・予約・MR セット、ADR-0017 §4.0 の `is_merge_family()`） | **M3 では不可**。自動マージの既定が `full` なので、条件がそろえば forge が自分でマージする。代理のマージは、人の承認を代理で済ませた MR を代理でマージさせる二段の道になる。M4 でマージキューと一緒に要るなら、この ADR の Amendment で行と主張の intent を足して決める（ADR-0017 §4.0 の許可リストのマージの系統は、この ADR で M4 まで外す） |
| `MergeSettingsManage`・`ApprovalRuleManage`・`CodeownersEnforcementToggle`・`ProtectedRefManage`・`merge.auto` の変更・`CheckNamespaceGrantManage` | マージの基準そのものを変える操作。代理で下げられると、承認の主張を守っても意味がなくなる。人が forge の画面で行う（ADR-0017 Amendment 2026-11-03 B の「代理と kanflow の出どころでは変えられない」） |
| `MergeRequestCreate`・ブランチの作成（F-2） | M4（計画 11 §4.3）。足すときはこの ADR の Amendment |
| `MergeRequestUpdate` / `Close` / `Reopen`・`MergeRequestComment`・`DiscussionResolve` | F-36 に要らない。スレッドの解決はマージの条件を変える（ADR-0024 #2） |
| `WorkItemLink` | 参照の検出（I1b）が結ぶので要らない。手の結びつけは forge の画面で |
| `CheckWrite` / `CheckRead` | kanflow のチェックはサービスアカウント **自身** として書く（`kanflow/` の名前空間、ADR-0024 §10）。代理で書くと「誰のチェックか」が曖昧になる |
| `IntegrationEventRead` / `IntegrationEventCursorRead` / `IntegrationDevStatusRead` | ADR-0016 §7・I1c・I1d のとおり、連携のアカウント自身の読み取りで、代理は断る（今の `act_as_refused` を保つ） |
| `is_session_only()` のすべて（トークン・セッション・サービスアカウントの作成と発行・連携の管理・版の ref の削除） | 代理はセッションではない |
| 管理者の操作・利用者・トークン・サービスアカウント・所属・プロジェクトの作成と移動と削除 | 範囲の外。管理者の権限は通さない（§3） |
| `RepoRead` / `RepoPush`（git）・`RepoBrowse` | git の経路は `Forge-Act-As` を読まない。閲覧は F-36 に要らない |

**X3 で足すもの（規則はここで決め、X3 の MR が許可リストと行を足す）**: `IntegrationBindingManage`（intent `binding_set` / `binding_remove`、主張が必須、本人のロール）。本人は対象のプロジェクトの `native_role()` の Maintainer 以上。**結びつけはまだ結びついていないプロジェクトが対象なので、§3 の `via_bound` の代わりに「主張の連携 = 要求の連携」と「結ぶ先がその連携」を範囲にする**（結びつけはサービスアカウントに Reporter を与えるメンバーの変更なので、主張なしでは決して許さない。ADR-0016 Amendment I1a の 2）。解除は結びついている先だけ。

**不変条件（テストで固定）**: 人だけの Action（`is_people_only`）で許可リストにあるものは、すべて `RequiresAssertion`（`every_people_only_act_as_action_requires_an_assertion`）。`is_session_only` の Action は `Denied`（`no_session_only_action_is_act_as`）。`ReadWithoutAssertion` の Action は読み取りだけ（`every_unasserted_act_as_action_is_a_read`）。

### 5. 主張（assertion）

ADR-0017 §4.2 の確認の画面と claim の流れを引き継ぎ、名前・経路・鍵・コードをこの節で確定する。**主張は 3 つの状態を通る: `pending`（発行。秘密はまだ無い）→ `claimed`（秘密を作り、1 回だけ返した）→ `consumed` / `rejected`**。

```
kanflow の画面で本人が「承認」/「取り消し」を押す
  → kanflow のサーバーが 128 bit 以上の乱数の state を作り、kanflow のセッションの利用者と結びつけて保存し、ポップアップで開く:
    GET /-/confirm/projects/{id}/merge-requests/{iid}?intent=approve&sha=<head>&integration=<id>&state=<state>
  → forge のセッション（なければ SSO。戻り先は同じ origin のパスだけ）。MR が見えなければ 404（ProjectRead と同じ）
  → forge のデータだけを表示: MR の番号・タイトル・source / target の ref、head の SHA（全桁）と target の先端、変更の要約（ファイル数・+/−・チェックの結論・blockers・承認）、
    押す内容（intent）、主張を受け取る連携の名前、「<表示名>（<利用者名>）として承認します」
    押せない（理由は MR が見える人にだけ）: 連携がこの MR のプロジェクトに結びついていない、sha が今の head と違う、本人に intent の権限がない、連携が無効
  → 本人が押す: POST /-/confirm/projects/{id}/merge-requests/{iid}/assertions（CSRF、本文 {intent, sha, integration, state}）
  → forge が **pending** の行を書く（結びつけと state_hash だけ。秘密・public_id・mac は無い）。画面は「完了しました。この画面を閉じてください」だけ
  → kanflow のサーバーが連携のサービスアカウントのトークンで POST /api/v1/integrations/{id}/act-as/claim {state}
    → forge が **この時に初めて** public_id と秘密を CSPRNG で作り、mac を計算して行を claimed にし（同じトランザクション）、
      {assertion, sub: {user_id, employee_number?}, intent, project_id, iid, head_sha, target_ref, expires_at} を返す
      （秘密を返すのはこの 1 回だけ。employee_number は user_employee_number のスコープのときだけ）
  → kanflow は sub が state に結びつけた kanflow の利用者と同じ人かを確かめ、違えば捨てる（共用の PC で別の人の forge のセッションで押された場合）
  → kanflow が POST /api/v1/projects/{id}/merge-requests/{iid}/approve {sha}
       Authorization: Bearer <サービスアカウントのトークン>、Forge-Act-As: user:<sub.user_id>、Forge-Assertion: <assertion>
  → forge が検証（§5.3）→ Principal::act_as → MR の行をロック → prove → 承認 → 同じ ChangeTx で主張を消費
```

#### 5.1 形・生成の時点・保存

- **形**: `fga_<public_id>_<secret>`（`public_id` は 16 桁の小文字の 16 進、`secret` は 64 桁。CSPRNG の 256 bit。トークンと同じ形で、秘密の走査で見つけやすい。ADR-0019 §1）。署名した JWS にはしない（forge が発行も検証もするので署名は要らない。ADR-0017 の「採らなかった案」）。型 `AssertionToken` は `Serialize` も `Display` も持たず、`Debug` は伏せ字。
- **秘密を作るのは claim の時だけ**: 発行（確認の画面の POST）は結びつけと `state_hash` を持つ `pending` の行だけを書き、秘密も `public_id` も `mac` も作らない。claim は 1 つのトランザクションで、`state_hash` の一致・`pending`・期限内の行を `FOR UPDATE` で取り、`public_id` と秘密を作り、`mac`（§5.2）と `key_id` と `claimed_at` を書いて `claimed` にし、コミットの後に応答で秘密を返す。**秘密はメモリにだけあり、応答を返した後は forge のどこにも残らない**。応答は冪等性のために保存しない（2 回目の claim は `claim_repeated` で何も返さない）。応答が kanflow に届かなかった（ネットワークの切断など）ときは、その主張は使えないまま 5 分で切れ、kanflow は確認の画面を開き直す（受け入れた費用）。
- **DB には秘密も、鍵なしで秘密を導けるものも置かない**: 秘密の平文・SHA-256・鍵のない派生値は持たない。持つのは `mac = HMAC(K, …)` だけで、鍵 `K` は DB の外のファイル（§5.2）。`pending` の行には `mac` も無い。ログ・監査・イベント・ジョブにも秘密を出さない（`no_token_like_string_reaches_audit_events_or_jobs` に `fga_` を足す）。
- **表** `forge.act_as_assertions`（追記のマイグレーション、S2。台帳 `tables.toml` は `audited`、監査のリソースは `("act_as_assertion", id)`）:

  | 列 | 内容 |
  |---|---|
  | `id` | |
  | `state`（CHECK: `pending`・`claimed`・`consumed`・`rejected`） | 状態の遷移はトリガーで前に進むだけ（`pending → claimed → consumed / rejected`、`pending → rejected`（期限切れの片づけ）） |
  | `public_id`（UNIQUE、16 進 16 桁の CHECK、**`pending` では NULL**） | claim で作る |
  | `mac`（`bytea`、32 バイト）、`key_id`（`smallint`）（**`pending` では NULL**） | §5.2。CHECK `(state = 'pending') = (mac IS NULL)`、`(mac IS NULL) = (key_id IS NULL)`、`(mac IS NULL) = (public_id IS NULL)` |
  | `integration_id`（aud）、`user_id`（sub、`users.kind = human` をトリガーで守る） | |
  | `intent`（CHECK: `approve`・`unapprove`。X3 で `binding_set`・`binding_remove` を追記のマイグレーションで足す） | Action と 1 対 1 |
  | `project_id`、`merge_request_id`（MR の内部の ID。iid ではない）、`head_sha`、**`target_ref`**（`refs/heads/…` の完全な名前）、`target_sha` | 結びつけ。X3 の intent では `external_project_id` を足す |
  | `state_hash`（SHA-256 の state） | claim の照合だけに使う。state は kanflow が作る 128 bit 以上の乱数で、主張の秘密ではない |
  | `issued_at`、`expires_at`（CHECK `expires_at <= issued_at + interval '5 minutes'`）、`claimed_at`、`finished_at`、`reject_reason` | CHECK: `claimed_at` は `claimed` 以降だけ、`finished_at` は `consumed` / `rejected` だけ |
  | `issued_request_id`、`claimed_request_id`、`finished_request_id` | 診断の手がかり |

  `forge_app`: SELECT・INSERT と UPDATE(`state`, `public_id`, `mac`, `key_id`, `claimed_at`, `finished_at`, `reject_reason`, `claimed_request_id`, `finished_request_id`) だけ。DELETE なし（保持の削除は後の MR で、O3 の保持と同じ形。1 人 1 日数十行の小さな表）。`forge_ro` は `mac`・`state_hash` を読めない。状態の変化ごとに監査の行（`act_as_assertion.issued`（主体は本人のセッション）/ `.claimed`（サービスアカウント）/ `.consumed` / `.rejected`（代理の主体、期限切れの片づけは `system`））。監査の `details` は `public_id`（claim の後）・`intent`・`integration_id`・`merge_request_id`・`head_sha` だけで、秘密も `mac` も入れない。

- **この形を固定する S2 のテスト**:
  - `issue_writes_a_pending_row_without_secret_public_id_or_mac`
  - `claim_creates_the_secret_and_returns_it_exactly_once`
  - `a_second_claim_is_refused_and_returns_no_secret`（`claim_repeated`）
  - `claim_after_expiry_is_refused`（`claim_expired`。行は `pending` のまま、片づけで `rejected`）
  - `no_assertion_secret_or_unkeyed_derivative_is_stored`（claim の後に `act_as_assertions`・`audit_log`・`events`・`jobs` の全列から、返した秘密・その SHA-256・`fga_` の形を探して無いこと）
  - `a_row_written_without_the_key_never_verifies`（DB に任意の `mac` を書いた行で、選んだ秘密を送っても `mac_mismatch`）
  - `claim_is_atomic_under_concurrency`（同じ state の claim の同時の 2 回で、秘密を受け取るのは 1 回だけ）

#### 5.2 HMAC の鍵と入れ替え

- **鍵**: 秘密のファイル `act_as_hmac_keys`（ADR-0013、ADR-0016 の「他の ADR への追記」で名前を予約済み）。行ごとに `<kid>:<base64 の 32 バイト>`、ちょうど 1 つに `active` の印、引退した鍵に `retired` の印。読めない・形が不正・`active` が 0 か 2 つ以上は起動の時点で `FG-SYS-0002`（設定の誤り。閉じる側に倒す）。ファイルが無いときは代理を無効にして起動し（§2 の `act_as_not_configured`）、doctor `act_as.keys` が「代理のスコープを持つ有効なトークンがあるのに鍵が無い」を `FG-SYS-0002` として出す。
- **用途は 2 つで、目的の文字列で分ける**（同じ鍵で別の値を作らせない）:
  1. 主張の封: `mac = HMAC-SHA256(K[kid], "forge.act_as.assertion.v1" ‖ secret ‖ 結びつけ)`。結びつけは `public_id`・`integration_id`・`user_id`・`intent`・`project_id`・`merge_request_id`・`head_sha`・**`target_ref`**・`target_sha`・`expires_at` の長さつきの連結（`forge_core::act_as::binding_bytes`、1 つの関数だけが作る）。DB に書ける者（`forge_app` の資格情報）が行を挿入・書き換えても、鍵を持たなければ通る主張を作れず、行の SHA・target の ref・MR を差し替えれば検証で `mac_mismatch` になる。
  2. 監査の `act_as_claimed`: `hex(HMAC-SHA256(K[active], "forge.act_as.claimed.v1" ‖ ヘッダーの値))`（64 文字）と `act_as_key_id`。社員番号を監査と封印の写し（ADR-0018）に平文で出さず、短い番号の辞書で戻されないようにする。
- **入れ替え**（承認された runbook `act-as` の手順）: 新しい kid を足して `active` にし、`forge serve` を再起動する → 古い kid は **10 分**（主張の寿命 5 分 + 時計のずれ）の後に `retired` にする。`retired` の鍵の主張は `key_retired` で断るが、鍵は監査の `act_as_claimed` の突き合わせのためにファイルに残す（消すと、その鍵の行は突き合わせられなくなるだけで、判定には影響しない）。
- **突き合わせ**（診断、読み取りだけ）: 運用の CLI `forge admin act-as claimed-mac --value <ヘッダーの値> [--kid <kid>]` が HMAC を出し、監査の行を `act_as_claimed` で探せる（ADR-0019 §8 の例外と同じ「ホストの操作者が信頼の起点」。DB に書かない）。
- **復元**: `forge restore` は代理の鍵を入れ替える（ADR-0019 Amendment 計画 11 P1 の「連携の秘密」）。復元の前の主張は寿命の 5 分で既に使えないが、入れ替えで確実にする。

#### 5.3 検証（順番と `FG-AUTH-0020` の `reason`）

検証は 2 段に分ける。**DB が要る事実の確認**は forge-db（`act_as::verify_assertion`、`VerifiedAssertion` を作れる唯一の関数）、**操作とリソースとの一致**は `authorize()`（純粋。権限の表の行で確かめられる）。

| 段 | 確かめること | 外れたときの `reason` |
|---|---|---|
| ヘッダー（forge-web） | `Forge-Assertion` が 1 つ、`fga_` の形 | `multiple_headers`、`malformed` |
| ヘッダー（forge-web） | `Forge-Act-As` なしの `Forge-Assertion` | `unexpected` |
| forge-db | `public_id` の行がある（無くてもダミーの HMAC と定数時間の比較をする。`pending` の行は `public_id` を持たないので引けない） | `unknown` |
| forge-db | 鍵が `retired` でない、`mac` が一致（定数時間） | `key_retired`、`mac_mismatch` |
| forge-db | 期限内、`claimed`（`consumed` / `rejected` でない） | `expired`、`consumed` |
| forge-db | `integration_id` = 要求のサービスアカウントの連携、`user_id` = `Forge-Act-As` で解決した本人 | `wrong_integration`、`wrong_user` |
| `authorize()` | Action の規則が `RequiresAssertion` なのに主張がない / `ReadWithoutAssertion` なのに主張がある | `missing` / `unexpected` |
| `authorize()` | `intent` = Action、`project_id`・`merge_request_id` = 証明のリソース | `wrong_action`、`wrong_resource` |
| `authorize()` | 本文の `sha` = 主張の `head_sha`、ロードした MR の head = `head_sha`、MR の今の target の ref = `target_ref`、その先端 = `target_sha` | `sha_mismatch`、`head_changed`、`target_retargeted`、`target_changed` |
| ChangeTx（ロックの下） | `UPDATE … SET state = 'consumed', finished_at = now() WHERE id = $1 AND state = 'claimed'` が 1 行 | `consumed`（同時の 2 回目） |

- **消費**: 成功は操作と同じ ChangeTx（承認の行・監査・イベントと一緒に確定）。`VerifiedAssertion` を作った後に失敗した（`authorize()` の拒否、ロックの下の再判定の失敗、409 など）ときも、**別の確定したトランザクションで `rejected`（`reject_reason`）として消費する**。使い回せず、kanflow は確認の画面を開き直す。
- **時刻**: 期限は DB の `now()` で判定する（サーバーの時計と DB の時計の差を判定に入れない）。
- **claim の失敗**（`POST …/act-as/claim`）: その連携のアカウント以外・存在しない連携は `FG-REPO-0005`、`state` が無い・期限切れ・2 回目は `FG-AUTH-0020`（`claim_unknown`・`claim_expired`・`claim_repeated`）。`state` は 128 文字以内の `[A-Za-z0-9_-]`。
- **流量**: 発行は本人ごとに 10 分で 30 回、claim は連携ごとに 1 分で 120 回（`FG-WEB-0010`）。

#### 5.4 確認の画面（ADR-0017 §4.2 の守りを引き継ぐ）

- トップレベルのポップアップ。`Content-Security-Policy: frame-ancestors 'none'`、`X-Frame-Options: DENY`、`Cross-Origin-Opener-Policy: same-origin`。
- 押すボタンは、表示されてフォーカスを得てから 1 秒以上たち、その後に画面の中で新しいポインターかキーの操作があるまで無効。既定のフォーカスを置かず、Enter で送らない（クリックの時機を狙う攻撃）。
- 表示するのは forge のデータだけで、kanflow から来た文字（タイトル・説明）を表示しない。画面は U3（`feat/ui-webhooks-notifications-confirm`）。
- **S2 の実装の注記（2026-11-05）**: S2 はサーバーの側だけを入れた。確認の画面が読むデータは `GET /api/v1/projects/{id}/merge-requests/{iid}/act-as-confirmation`（`ActAsConfirmView`）、押す操作は `POST /-/confirm/projects/{id}/merge-requests/{iid}/assertions`（`ActAsAssertionIssue`）。ポップアップのページ `/-/confirm/projects/{id}/merge-requests/{iid}` そのもの、その `Content-Security-Policy: frame-ancestors 'none'`・`X-Frame-Options: DENY`・`Cross-Origin-Opener-Policy: same-origin`、押すボタンの 1 秒の待ちと新しい操作の要求、変更の要約（ファイル数・+/−・チェック・blockers・承認）の表示は **U3 の作業** として残る。
- **U3 の実装の注記（2026-11-06）**: ポップアップのページ `GET /-/confirm/projects/{id}/merge-requests/{iid}` は SPA のシェル（`WebShell`、全員に同じバイト列）で、`X-Frame-Options: DENY` はシェルだけでなく Smart HTTP と raw 以外のすべての応答に付く（`frame-ancestors 'none'` と `Cross-Origin-Opener-Policy: same-origin` は既存の表）。ボタンは「表示され、フォーカスがあり、1 秒たった後に、本人の（`isTrusted`）ボタン上のポインター押下かキー操作で有効になり、その操作から 300 ms 以上後のクリック」だけを受け付ける（ポインターの移動では有効にならない。押した直後のクリックは拒否して案内を出す）。非表示・フォーカス喪失で最初からやり直す（`aria-disabled`、既定のフォーカスなし、`<form>` なし）。送信は 1 回ずつ（ミューテーションの `isPending` が守る）。**POST は画面に出した target の先端（`target_sha`）を送り、サーバーは今の先端と比べて違えば `stale_view`（409）で断る**（git から読む値なので DB のトランザクションの中ではなく、handler で発行の直前に比べる。使うときの `target_changed` の確認は従来どおり）。確認の URL の `state` は読んだ直後に `history.replaceState` でアドレスから外し、そのタブの sessionStorage にだけ残す。押している間は 10 分の期限が来ても画面を閉じない。この画面は 10 分で期限切れとして閉じる（主張自体は押してから 5 分）。**変更の要約（ファイル数・+/−・チェック・blockers・承認）は、サーバーの API が返さないため U3 では出さず、MR・版・target の先端だけを出す**（API を足すときに追う）。「やめる」はサーバーに何も送らない（主張は押したときにだけ作られる）。
- ログ・アクセスログ・トレースでは `Forge-Act-As`・`Forge-Assertion` のヘッダー、確認の画面の URL の `state`、claim の本文の `state` と応答の `assertion` を伏せる。記録するのは `assertion_public_id`、`intent`、`integration_id`、`mr_id`、`head_sha`、`state_hash` の先頭 8 文字。

### 6. 監査とログ

| 欄 | 代理の変更（承認・取り消し） | 代理の読み取り |
|---|---|---|
| 監査の行 | 書く（操作の監査 + `act_as_assertion.consumed`） | 書かない（読み取りは監査しない。今までと同じ） |
| `actor_kind` | `user_via_service_account` | — |
| `actor_user_id` | 本人 | — |
| `actor_via_id` | サービスアカウント | — |
| `credential_id` | サービスアカウントのトークンの ID | — |
| `act_as_claimed` / `act_as_key_id` | §5.2 の HMAC と kid | — |
| `via_assertion_id` | 主張の `public_id`（秘密ではない） | — |
| `effective_scope` | `act_as` + Action のスコープ | — |
| 承認の行（ADR-0024 R5） | `credential_origin = act_as_assertion`、`direct = true`（ADR-0017 §4.2）。新しい head へ持ち越さない（§4） | — |
| ログ（`forge.auth.act_as`、成功も失敗も 1 行） | `request_id`、`error_code`（失敗）、`reason`、`action`、`act_as_rule`、`actor.user`（本人の ID）、**`actor.via`（必ず）**、`integration_id`、`credential_public_id`、`assertion_public_id`、`claimed_mac` の先頭 8 文字 | 同左（`assertion_public_id` なし。`effective_role` = min のロール） |
| イベント | `actor.kind = user_via_service_account`、`actor.via`（計画 02 §11.1） | — |

ヘッダーの値・社員番号・主張の秘密は、ログ・監査・イベント・ジョブに決して出さない。

### 7. 自動マージの既定 `full` との関係（自動化に人間の承認を作らせない）

計画 04 決定 8（2026-11-03）で、条件がそろった MR は forge が自分でマージする。保護ブランチで「人間の承認 1 件」がその条件に残るので、**代理は、その 1 件を人の操作なしに作れる道であってはならない**。次を守る。

1. **人間の承認として数えるのは、主張のある代理の承認だけ**。主張は forge のセッションで本人が押したときにだけ発行され（§5.4 の操作の待ちを含む）、代理の資格情報には承認の Action を主張なしで通す規則がない（`RequiresAssertion` の不変条件）。
2. **代理の承認は、本人が見た head にだけ効く**（§4: 新しい head へ持ち越さない）。自動修正のコミットや push の後に、古いクリックで自動マージされることはない。
3. **代理の主体を作れるのは HTTP の入口だけ**: `Principal::act_as` は forge-web の認証の層だけが呼び、forge の worker・自動マージの主体・ボット・システム・`Reserved` / `Queued` は代理の資格情報を持たない（`ActAsCredential` は forge-web の外で作れない型。呼び出し元のテスト `only_forge_web_builds_act_as_principals`）。
4. **エージェントのトークンは代理できない**（§2）。kanflow の AI の道具（計画 02 §8.4 K-8 の `propose_*`）が代理のトークンを持っても、承認には人の確認が要る。
5. **サービスアカウント自身の承認は不可のまま**（人だけの arm）。代理は「人だけ」を外す例外ではなく、「主張で人の在席を示した」ときだけ人の arm を通る。
6. **マージ・マージの設定・`merge.auto` の変更は代理で不可**（§4）。代理の承認が最後の条件になった MR が `full` で自動マージされるのは、人が forge の画面でその head を承認したのと同じ扱いで、意図どおり。
7. AI（仕様照合）の承認（`MergeRequestAiApprove`、INTERNAL）は代理と無関係のまま（ADR-0017 §4.1）。

### 8. エラーコード（S2 で `errors.toml` に登録する。テストと同じコミット）

| コード | 名前 | HTTP | 重さ | runbook | 内容 |
|---|---|---|---|---|---|
| **`FG-INT-0006`** | ActAsTargetInvalid | 403 | info | `act-as` | §1 の解決の失敗。`reason` は §1 の表（`ambiguous` は runbook でデータの破損として扱う）。本文は同じ |
| **`FG-AUTH-0020`** | ActAsAssertionInvalid | 403 | warn | `act-as` | §5.3 の検証と claim の失敗。`reason` は §5.3 の表。計画 11 §4.4 の「FG-AUTH-0019」は S0 が使ったので次の空き。ADR-0017 §4.2 の「401」は **403** に改める（サービスアカウントの認証は成功している。401 は kanflow にトークンを捨てさせる） |

既存のコードの再利用: 許可リストの外・資格情報の条件は `FG-AUTH-0003`（`act_as_not_allowed`・`act_as_requires_service_account`・`act_as_no_integration`・`integration_disabled`・`act_as_not_configured`・`agent_token`）、スコープは `FG-AUTH-0002`、結びつけの外は `FG-REPO-0005`（`act_as_unbound`）、自己承認は `FG-REVIEW-0013`、流量は `FG-WEB-0010`、鍵のファイルは `FG-SYS-0002`、発行の不変条件は `FG-AUTH-0008`（`act_as_requires_service_account`・`act_as_agent_token`）、社員番号の衝突のサインインは `FG-AUTH-0014`、DB は `FG-DB-0001`。

### 9. S2 の範囲（`feat/auth-act-as`）

- forge-web: ヘッダーの解析（各 1 つ）、`ActAsCredential` と `Principal::act_as` の構築、ログの伏せ字、`viewer` の欄（MR の読み取りに `can_approve: {allowed, reason?}`。主張があるものとして `authorize()` を試す共通のヘルパーで計算し、主張の要否を `requires_assertion: true` で示す）、claim のルート。
- forge-auth: `ActAsRule`、`Action::act_as_rule()`、代理の判定の順（§3 の min のロール、§5.3 の `authorize()` の段）、行。
- forge-db: `act_as_assertions`（§5.1 の状態と CHECK）、`act_as` スコープ（`tokens_scopes_known` の追記）、`users_employee_number_kept_when_disabled`、`issue` / `claim` / `verify_assertion` / `consume` / `reject`、期限切れの `pending` の片づけ、本人の解決、`may_carry_approvals` が代理の承認を写さないこと。
- forge-core: ヘッダーの文法、`AssertionToken`、`binding_bytes`、鍵の束。
- 確認の画面（`ActAsConfirmView` / `ActAsAssertionIssue`）の **サーバーの側** は S2、画面は U3。
- doctor `act_as.keys`、`gstn` は無し（代理は kanflow のサーバーが使う。C-1 の表で `NotForCli`）。
- **runbook `act-as`（新規）**。`reason` ごとの原因と確かめ方、`forge admin act-as claimed-mac` での監査の突き合わせ、鍵の入れ替え（`[MUTATING — needs approval]`）、代理の即時の停止（サービスアカウントの無効化・連携の無効化）、kanflow の `sub` の照合の確かめ方に加えて、**受け入れた 2 つの残る危険** を「注記」として書く:
  1. **侵害された kanflow は、本人に別の MR の確認の画面を開かせられる**（ポップアップの URL は kanflow が作る）。守りは、画面が forge のデータだけ（MR の番号・タイトル・SHA・target・受け取る連携）を表示し、kanflow の文字を出さないこと（§5.4）。本人が見ずに押せば防げない。疑わしいときは監査の `act_as_assertion.issued` を本人ごとに見て、kanflow の画面の操作の記録と突き合わせる手順を書く。
  2. **`FG-INT-0006` と、相手が決まった後のコード（`FG-AUTH-0003` など）の違いで、サービスアカウントは利用者の存在を知れる**。守りは解決の失敗の流量の制限（1 分に 60 回、`FG-WEB-0010`）とログ。`FG-INT-0006` の急増は kanflow の誤設定か探索の兆候として扱う手順を書く。

## 失敗の場合（S2 の「先に書くテスト」）とログでの見え方

| 場合 | 応答 | ログ（`forge.auth.act_as`） | テスト |
|---|---|---|---|
| 許可リスト外の Action（例: 代理でマージ） | 403 `FG-AUTH-0003` | `reason=act_as_not_allowed action=MergeRequestMerge actor.via=…` | `act_as_merge_is_not_allowed`、生成の `act_as_denied_for_<action>` |
| 相手が無効 | 403 `FG-INT-0006` | `reason=disabled`（本文は unknown と同じ） | `act_as_disabled_target_is_int_0006` |
| 相手が人でない（サービスアカウント・ボット・自分） | 403 `FG-INT-0006` | `reason=not_human` | `act_as_service_account_or_bot_target_is_refused` |
| 相手がいない / メールの形 | 403 `FG-INT-0006` | `reason=unknown` / `email_form` | `act_as_unknown_and_disabled_look_the_same`、`act_as_email_form_is_refused` |
| 社員番号が 2 行に当たる | 403 `FG-INT-0006` | `reason=ambiguous` | `act_as_ambiguous_employee_number_fails_closed` |
| 無効の利用者の社員番号を消す・変える | トリガーが拒否（23514） | — | `disabled_users_employee_number_is_kept` |
| 主張の再使用 | 403 `FG-AUTH-0020` | `reason=consumed assertion_public_id=…` | `assertion_is_single_use`、`concurrent_use_consumes_once` |
| 主張の期限切れ | 403 `FG-AUTH-0020` | `reason=expired` | `assertion_expires_after_five_minutes` |
| 別の SHA（本文・push 後の head・target の前進） | 403 `FG-AUTH-0020` | `reason=sha_mismatch` / `head_changed` / `target_changed` | `assertion_bound_to_head_and_target` |
| MR の target を別のブランチに変えた | 403 `FG-AUTH-0020` | `reason=target_retargeted` | `assertion_bound_to_target_ref` |
| 別の MR・別の intent・別の連携・別の本人 | 403 `FG-AUTH-0020` | `reason=wrong_resource` / `wrong_action` / `wrong_integration` / `wrong_user` | `assertion_bound_to_action_resource_and_user` |
| 行の改ざん（DB で SHA・target の ref を書き換え、または鍵なしで行を作る） | 403 `FG-AUTH-0020` | `reason=mac_mismatch` | `tampered_assertion_row_fails_the_mac`、`a_row_written_without_the_key_never_verifies` |
| claim の 2 回目・期限切れ | 403 `FG-AUTH-0020` | `reason=claim_repeated` / `claim_expired` | §5.1 のテスト |
| ヘッダー 2 つ（`Forge-Act-As` / `Forge-Assertion`） | 403 `FG-INT-0006` / `FG-AUTH-0020` | `reason=multiple_headers` | `two_act_as_headers_are_refused`、`two_assertion_headers_are_refused` |
| 範囲外のプロジェクト（結びついていない） | 404 `FG-REPO-0005` | `reason=act_as_unbound` | `act_as_on_an_unbound_project_is_not_found` |
| 本人は Maintainer、代理の読み取りで Reporter に見えないもの | 404 / 403 | `effective_role=reporter` | `act_as_read_is_capped_by_the_service_accounts_role` |
| 主張なしの承認 | 403 `FG-AUTH-0020` | `reason=missing` | `act_as_approve_without_assertion_is_refused` |
| 読み取りに主張 | 403 `FG-AUTH-0020` | `reason=unexpected` | `assertion_on_a_read_is_refused` |
| 代理の承認の後の push（写す設定のプロジェクトでも） | 承認が消える | `merge_request.unapproved` | `act_as_approval_is_never_carried_to_a_new_head` |
| セッション・人のトークン・エージェントのトークンにヘッダー | 403 `FG-AUTH-0003` | `reason=act_as_requires_service_account` / `agent_token` | `act_as_header_needs_a_service_account_token` |
| `act_as` のないトークン | 403 `FG-AUTH-0002` | `required_scope=act_as` | `act_as_needs_the_act_as_scope` |
| 本人が作成者 | 403 `FG-REVIEW-0013` | `reason=self_approval`（`actor.via` つき） | `act_as_self_approval_is_refused` |
| 検証の後の失敗 | 4xx（その失敗のコード） | 主張は `rejected` で消費 | `assertion_is_consumed_when_the_action_fails` |

どの場合も、新しいセッションの Claude が `request_id` → `forge.auth.act_as` の行 → `reason` → runbook `act-as` の表、と辿れる。

## 権限の表に S2 が足す行（`forge-auth/src/matrix_rows_act_as.rs`）

1. **生成の deny の行**: `for a in Action::ALL` で `act_as_rule() == Denied` のすべてについて「代理（Developer 以上の本人 + 結びついたプロジェクト + 有効な主張）」の deny（`act_as_denied_for_<action>`。ADR-0024 #38 の宿題）。`is_session_only` のすべてを含む。
2. **allow の行**: `ProjectRead` / `MergeRequestRead` / `MergeRequestList` × 代理（本人とサービスアカウントの両方が Reporter 以上、結びついたプロジェクト、主張なし）。`MergeRequestApprove` × 代理（Developer の本人で作成者でない、`act_as` + `merge_approve`、`approve` の主張が一致）。`MergeRequestUnapprove` × 代理（承認した本人、`unapprove` の主張）。それぞれ **人の双子**（同じ本人のセッション）が allow であることを確かめる。
3. **deny の行**（承認と取り消し）: 主張なし、`unapprove` の主張で承認、別の MR の主張、本文の `sha` が違う、head・target の ref・target の先端が主張と違う、作成者の本人（FG-REVIEW-0013）、Reporter の本人、`merge_approve` のないトークン、`act_as` のないトークン、結びついていないプロジェクト（404）、無効の連携、所属のない管理者の本人（404）、エージェントのトークン、サービスアカウント自身（代理なし、既存の行を保つ）。
4. **読み取りの deny の行**: 本人が見えない・サービスアカウントが見えない（結びついていない）のどちらも 404、**サービスアカウントのロールが足りない読み取りは、本人のロールが足りていても deny**（min の規則）、主張つきの読み取り（`unexpected`）。
5. **既存の行を保つ**: `matrix_rows_feed.rs`・`matrix_rows_dev_status.rs` の `act_as: true` の deny。
6. **確認の画面と claim**: `ActAsConfirmView` / `ActAsAssertionIssue`（`SessionOnly`、人だけ。intent の Action が本人のセッションで通り、連携が結びついている → allow。トークン・代理・`via_bound = false`・intent の Action の deny・見えない MR → deny）、`ActAsAssertionClaim`（その連携のサービスアカウントの `act_as` のトークン → allow。別の連携・人・代理・エージェントのトークン → deny）。
7. ADR-0017 の名前（`MergeConfirmView`・`MergeAssertionIssue`・`MergeAssertionClaim`）は、結びつけ（X3）でも使うので `ActAs…` に改める（Action はまだ作られていない）。

## Alternatives Considered

### A: サービスアカウント + `Forge-Act-As` だけ（計画 02 §5.3 の元の案、ADR-0005）
- **Pros**: 小さい。kanflow の改修も小さい。
- **Cons**: 1 つのサービスアカウントのトークンで、全員分の承認を作れる（T1）。自動マージ `full` の下では、kanflow が侵害されるか自動化が誤ると、人の承認の条件が機械に満たされる（T4）。
- **Why not**: 計画 11 §3.5 で「防げない」と判定し、利用者が B を承認した。読み取りには A の形を、min のロールの規則つきで使う（読める範囲が広がらないため）。
- **失敗の見え方**: 不正な承認は成功として記録され、エラーコードが出ない（沈黙の失敗）。

### C: Keycloak のトークン交換（RFC 8693、本人のトークン。IoT 12 §4・§11.5）
- **Pros**: 本人の認証を IdP が保証する。ヘッダーで本人を名指す必要がない。
- **Cons**: IoT 側の Keycloak でトークン交換を有効にする必要があり、時期は **未確認**。forge が JWT のアクセストークンを API の資格情報として受ける経路（今は Web のログインだけ、ADR-0021）が新しく要る。交換されたトークンは「kanflow にログインしている」ことは示すが、「この MR のこの SHA を承認する意図」は示さない（確認の画面は結局要る）。
- **Why not now**: M3 の時期に間に合わない。B を C に替えられる形にしておく（下の移行）。

### 発行の時点で秘密を作り、claim まで保存する
- **Pros**: claim が単純（保存した値を返すだけ）。
- **Cons**: 発行から claim までの間、DB（またはその暗号文と鍵）に秘密がある。DB の読み取り・バックアップ・ログから取り出されうる。
- **Why not**: 秘密を claim の時点で作れば、DB は秘密を一度も持たない（§5.1）。費用は「claim の応答を失ったら押し直し」だけ。

### 主張を kanflow が作る / 署名した JWS にする / URL や `window.opener` で渡す
- **Why not**: ADR-0017 の「採らなかった案」のとおり。kanflow が作れるなら T2 を防げない。JWS はアルゴリズムと `kid` の落とし穴と新しい crate を持ち込み、forge が発行も検証もする場面で得るものがない。URL は履歴・ログ・Referer に残る。

### 主張の秘密を SHA-256 だけで保存する（トークンと同じ）
- **Pros**: 鍵の管理が要らない。
- **Cons**: DB に書ける者が、自分で選んだ秘密の SHA-256 で行を挿入すれば、通る主張を作れる。行の SHA・MR を書き換えても検出できない。
- **Why not**: 主張は「人の在席」の証拠なので、DB の資格情報だけで作れないこと（鍵がファイルにあること）に意味がある。監査の `act_as_claimed` に HMAC の鍵がもともと要る（ADR-0016 §9）ので、増える運用は小さい。

### 代理の読み取りを本人のロールで判定する（サービスアカウントのロールで上限を付けない）
- **Why not**: 主張なしの読み取りは、サービスアカウントが本人を名指すだけで成り立つ。本人のロールで読むと、サービスアカウントは「Maintainer の人を名指す」ことで自分が読めない物（Reporter に見えない物）を読める。上限を外すのは主張のある変更だけにする。

### 社員番号だけ / forge の利用者 ID だけで指す
- 社員番号だけ: forge に社員番号の無い人（Keycloak の claim が無い）を指せない。利用者 ID だけ: kanflow が最初の読み取りの前に対応を知らない。**両方を受け、書き込みは主張の本人で必ず照らす**ので、どちらの形でも本人を取り違えて書くことはない。

### 代理のマージを M3 で許す（ADR-0017 §4.0 の許可リスト）
- **Why not**: 既定の `full` で forge が自分でマージするので要らず、代理の承認と組み合わさると人の関わりが 1 回のクリックに縮む経路が増える。M4 でマージキューと予約を作るときに、この ADR の Amendment で決め直す。

## Consequences

### Positive
- 1 つのサービスアカウントが人の承認を作れない（T1）。自動マージ `full` でも、人の承認の条件は、本人が見た head についての人の操作でしか満たされない（T4）。
- 主張の秘密を DB が一度も持たない（T6）。
- 判定は `authorize()` の 1 か所のまま。代理は新しい `Credential` の種類と `ActAsRule` の 1 つの `match` で表され、許可リストは既定で閉じる（新しい Action は選ぶまで代理できない）。主張なしの読み取りは、サービスアカウントが元々読める範囲を超えない。
- 代理のすべての失敗が 2 つの新しいコードと既存のコードの `reason` に分かれ、`actor.via` と `assertion_public_id` で監査とログを結べる。
- ADR-0016 で予約した監査の列をそのまま使い、監査の表のマイグレーションは要らない。

### Negative
- kanflow の承認は「押す → ポップアップで forge → もう一度押す」の 2 段になる（意図した費用。F-36 の画面の工夫は K4 と U3）。
- 鍵のファイルが 1 つ増え、入れ替えと復元の手順が runbook に増える。
- 承認の主張は head と target（ref と先端）に結びつくので、target が 5 分の間に進むと押し直しになる。代理の承認は push で必ず消えるので、写す設定のプロジェクトでも kanflow の承認者は押し直す。
- claim の応答を失うと押し直しになる。

### Risks
- **kanflow の `sub` の照合の漏れ**: kanflow が `sub` を確かめないと、共用の PC で別の人の forge のセッションで押された主張が使われうる → forge は `Forge-Act-As` の本人 = 主張の本人を必ず確かめる（`wrong_user`）ので、他人として **書く** ことはできない。残るのは「押した人と kanflow の画面の人が違う」ことで、K4 のテスト（`sub` の不一致を捨てる）と runbook に書く。
- **別の MR の確認の画面**: 侵害された kanflow は本人に別の MR の画面を開かせられる → 画面は forge のデータだけを表示する（§5.4）。runbook `act-as` の注記（§9）。
- **利用者の存在の推測**: `FG-INT-0006` と後のコードの違いで存在が分かる → 流量の制限とログ。runbook `act-as` の注記（§9）。
- **鍵の漏えい**: 鍵と `forge_app` の資格情報の両方が漏れると主張を作れる → 鍵はファイルで別の権限、漏えい時は runbook `act-as` の入れ替え（承認つき）と、サービスアカウントの無効化（`UserDisable`）で代理を即座に止める。
- **社員番号の再利用**: forge の側は §1 のトリガーと一意の索引で閉じる（無効の利用者の番号は残り、新しい人に付かず、衝突はサインインで止まる）。**未決（open）**: 会社として退職者の番号を新しい人に振り直す運用があるかは **未確認**（Keycloak の管理者に確かめる）。ある場合、その新しい人は forge にサインインできず（`FG-AUTH-0014`）、運用の手順が要る。

## OAuth2 / トークン交換への移行

1. **S2（今）**: 案 B。`Credential::ActAs { via, via_token, via_scopes, assertion, claimed }`。
2. **IoT の Keycloak でトークン交換が使えるようになったら**（時期は未確認）:
   - **本人自身のトークンからの交換だけを受ける**: kanflow は、本人が kanflow に OIDC でログインしたときの **本人のアクセストークン** を `subject_token` にして交換する（RFC 8693 の委任）。`subject_token` なしで `requested_subject` を指定する **Keycloak のなりすまし（impersonation）は受けない**: Keycloak の側で kanflow のクライアントのなりすましの権限を与えず（runbook と doctor で設定を確かめる）、forge の側でも、なりすましで作られたトークン（本人のログインの `sid` / `auth_time` を持たない、`act` が無い）を `FG-INT-0006 reason=impersonation_token` で断る。
   - **委任の相手を連携に絞る**: forge は交換されたトークンについて、`iss` = 設定の Keycloak（ADR-0021）、`aud` に `gitstation`、`azp` = その連携に登録した kanflow のクライアント ID、`act.sub`（RFC 8693 の行為者）= 同じクライアント、元のトークンの `may_act` がそのクライアントを許していること、を確かめ、どれかが違えば `FG-INT-0006`（`wrong_azp`・`act_missing`・`may_act_mismatch`）。連携ごとにクライアント ID を 1 つ登録する（`integrations` の追記の列）。
   - `sub` は `user_identities` で本人に結ぶ。**`Credential` の種類は `ActAs` のまま**（`via_token` が「交換されたトークンの `jti`」になる）なので、`authorize()`・許可リスト・§3 の min の規則・行・監査の列は変わらない。そのとき **`Forge-Act-As` は送らない**（本人は JWT が決める。両方あれば `FG-INT-0006 reason=multiple_identity_sources`）。
3. **主張は必須のまま**: 交換されたトークンはログインを示すが、承認の意図・SHA・target を示さない。`RequiresAssertion` の Action はトークン交換の後も主張を要する。**既定ではなく必須**であり、外すにはこの ADR を置き換える新しい ADR と `security-reviewer` のレビューが要る。
4. 監査の CHECK は代理の行に `credential_id` を要するので、交換されたトークンを記録する表（`jti`・連携・期限）を追記のマイグレーションで足す。
5. kanflow が移った後、`act_as` スコープの発行を止め（`tokens::issue` が `act_as_deprecated` で断る）、最長 365 日で既存のトークンが切れたら `Forge-Act-As` の解析を v2 で外す（v1 の間は `FG-INT-0006 reason=deprecated` で明示して断る）。

## accepted の条件

1. `security-reviewer`（必須）と `authz-matrix-auditor` のレビューで CRITICAL / HIGH が残っていない。
2. §1 の社員番号の守り（無効の利用者の番号を消さない・変えないトリガー、`ambiguous` で閉じる、サインインの衝突で止まる）を S2 の範囲に含めることを受け入れる。
3. 社員番号の再利用の運用（Keycloak の管理者への確認）は **未決のまま受理してよい**（forge の側が閉じる側に倒れるため）。答えが「再利用する」なら、この ADR の Amendment で運用の手順を足す。

## 他の ADR との関係（この ADR が優先する点）

- **ADR-0017 §4.0**: `ActAs` の許可リストは §4 のとおりに狭める（M3 はマージの系統を外す）。§4.2: ヘッダーは `user:<email>` ではなく §1、検証の失敗は 401 ではなく 403 の `FG-AUTH-0020`、Action と経路の名前は `ActAs…` と `/-/confirm/projects/{id}/merge-requests/{iid}`・`/api/v1/integrations/{id}/act-as/claim`（ADR-0015・0024 のハイフンの規則）、主張は `fga_` の形で、秘密は claim の時点で作り HMAC で封じる（§4.2 の「主張を作り、ハッシュを保存」を改める）。§4.0 の「代理実行の ADR に残すこと」はこの ADR で閉じる（email の対応 → §1、退職・無効化 → §1 の `disabled` と番号の守り、トークン → §2、流量 → §1・§5.3、OAuth2 → 上の節、他の代理の操作 → §4 の X3）。
- **ADR-0016 §9**: `FG-INT-0006` は相手の解決の失敗だけにする（権限の不足は `authorize()` のコード）。
- **ADR-0024 §8.2・§12**: `may_carry_approvals` は代理の承認に効かない。表の「代理」の列は §4 のとおり（承認・取り消しは主張つきで可、他は不可）。代理の deny の行は S2 で入れる。
- **ADR-0019**: スコープ `act_as`、`tokens::issue` の不変条件、復元での鍵の入れ替え。
- **計画 02 §5.3 / 計画 11 §4.4**: ヘッダーの形、`FG-AUTH-0019` → `FG-AUTH-0020`。

## Error codes and runbooks affected

- 追加（S2）: `FG-INT-0006` ActAsTargetInvalid（403、info）、`FG-AUTH-0020` ActAsAssertionInvalid（403、warn）。どちらも runbook `act-as`。
- 拡張（`reason` を足す、S2）: `FG-AUTH-0003`（`act_as_not_allowed`・`act_as_requires_service_account`・`act_as_no_integration`・`act_as_not_configured`）、`FG-AUTH-0008`（`act_as_requires_service_account`・`act_as_agent_token`）、`FG-REPO-0005`（`act_as_unbound`）、`FG-SYS-0002`（`act_as_hmac_keys`）。
- runbook 追加: `act-as`（§9 の内容と 2 つの残る危険の注記）。
- runbook 更新: `backup-restore`（代理の鍵の入れ替え）、`service-accounts`（`act_as` スコープは連携のアカウントだけ、エージェントの印と同時に持てない）、`integrations`（代理を止める手順への参照）、`user-offboarding`（無効化しても社員番号は残ること）。
- doctor: `act_as.keys`（鍵の有無・`active` の数・`retired` の鍵の数、代理のスコープを持つ有効なトークンがあるのに鍵が無い）。

## Amendment（2026-11-06、M3 X3 `feat/cross-multi-repo-bindings`: 結びつけの代理）

§4 の「X3 で足すもの」を実装し、**許可リストを明示的に広げた**（閉じた既定を変える決定。結びつけの規則は ADR-0016 Amendment X3）。

1. **Action と intent を 1 対 1 に保つ**: §4 は `IntegrationBindingManage` に `binding_set` / `binding_remove` の 2 つを書いたが、§5.1 の
   「intent は Action と 1 対 1」を守るため、解除を新しい Action **`IntegrationBindingRemove`**（人だけ、Maintainer、`api`）に分けた。
   `IntegrationBindingManage` = `RequiresAssertion(binding_set)`（結ぶ・設定を変える）、`IntegrationBindingRemove` = `RequiresAssertion(binding_remove)`。
2. **範囲**: 結ぶ先はまだ結びついていないことがあるので、`via_bound` の代わりに **経路の連携 = 要求の連携**（違えば `FG-REPO-0005
   reason=act_as_other_integration`、forge-db のロックの下でも主張の連携とサービスアカウントを確かめる）。ロールは **本人の** `native_role()`
   （Maintainer 以上、管理者の権限は通さない）。サービスアカウントの所属は見ない（`ViaView` は読み込まない）。
3. **主張の結びつけ**（追記のマイグレーション `20261129100000`）: `act_as_assertions` に `external_project_id` と `resolves_work_items` を足し、
   MR の列は binding の intent では NULL（CHECK `act_as_assertions_subject`）。MAC の入力（`binding_bytes`）は MR の intent では S2 と同じ
   バイト列、binding の intent では kanflow のプロジェクトと設定（`None`/`false`/`true`）。`authorize()` は intent・プロジェクト・kanflow の
   プロジェクト（`wrong_resource`）・設定（**`setting_mismatch`**、新しい `reason`）を比べる。本人が確認したのと違う設定では使えない。
4. **確認と claim**: `POST /-/confirm/projects/{id}/integration-bindings/assertions` `{intent, integration, kanflow_project_id,
   resolves_work_items?, state}`（`ActAsAssertionIssue`、本人の Web セッション、CSRF、本人ごとの流量は §5.3 と共有）。`binding_set` は未結合でも可、
   `binding_remove` はその kanflow のプロジェクトに結びついているときだけ。claim の答えの `iid`・`head_sha`・`target_ref` は binding の intent で
   `null`、`kanflow_project_id`・`resolves_work_items` を足した。確認の画面（GET の表示のデータ）は U3 で足す。
5. **一覧の代理（新しい規則 `ListAsPerson`）**: `RepositorySearch` は主張なし、**本人として** 判定する（経路は `Resource::Instance`、候補は
   1 件ずつ素の `Resource::Project`）。§3 の min の規則では結ぶ前のリポジトリが見えず、登録の画面が作れないため。広がるのは
   「本人が Maintainer 以上のリポジトリの ID・パス・表示名」だけ（本人が主張つきで結べるもの）で、内容は読めない。受け入れた残る危険:
   kanflow のサービスアカウントは、社員番号を順に指して、各人が Maintainer のリポジトリ名を列挙できる（kanflow は社員の一覧を既に持つ。
   失敗した解決の流量 §1 は効かない: 解決は成功するため）。
6. 不変条件のテストを更新: intent はすべてちょうど 1 つの Action に属する、`ListAsPerson` は `RepositorySearch` だけで `read_api`、人だけではない。

### X3 の独立レビュー（2026-11-06）で変えた点

- （security MEDIUM）代理の `RepositorySearch`（`ListAsPerson`）を絞った: 連携のサービスアカウントごとに **1 分に 30 回**（`FG-WEB-0010`）、
  検索ごとに監査の行 `integration.repository_searched`（`user_via_service_account`、`integration` のリソース、`q` と件数）。
  §1 の解決の失敗の流量は `unknown` を含むすべての `FG-INT-0006` を数える（上限の手前でも越えた後でも。テストで固定）。§5 の残る危険
  （社員番号を順に指した列挙）は、この流量と監査で検出と速度の制限を受ける。
- （LOW a）確認（`ActAsConfirmView` / `ActAsAssertionIssue`）は、本人の intent の権限を **連携の有効・結びつけの判定より先に** 見る（権限の無い人に
  連携の状態を言わない）。存在しない連携は見えないプロジェクトと同じ 404 のまま。
- （LOW b）`RepositorySearch` の経路（`Resource::Instance`）は人だけ（代理は本人として判定）。サービスアカウント・ボット・エージェントの単独は
  `FG-AUTH-0003`。

## accepted の条件の確認（2026-11-09、クラウド）

1. **条件 1 は満たした**: `main`（S2 `04262d7`・U3b `f440c77`・X3 `a7036aa` 以降）のコードを、`security-reviewer`（T1〜T5・§1〜§9）と
   `authz-matrix-auditor`（許可リストと権限の表・全経路の `authorize()`・人だけの Action・min の規則・契約 4 の例外）が読んで、どちらも
   **CRITICAL / HIGH なし**（コードと生成物を読んだ。cargo は流していない）。
   - MEDIUM（authz）: 公開の経路（`WebShell`・`Health`・ログイン）は認証の層を通らず、`Forge-Act-As` / `Forge-Assertion` を **断らずに無視** する
     （副作用は無い）。§2 の「無視せず断る」と食い違う → 後で直す（安い事前の検査で断る）か、§2 に例外として書く。
   - LOW（security）: (a) `Authorization` の無い要求の代理のヘッダーは外して匿名で続ける（匿名を許す経路だけ、昇格は無い）→ `FG-AUTH-0001` で断る、
     (b) 要求が途中で切れると検証済みの主張が `claimed` のまま 5 分残る（同じ MR・head・target に縛られ昇格は無い）→ drop の守りで `reject_unused`、
     (c) 鍵のバイトを drop で消していない・全部同じバイトの鍵を受け付ける → `zeroize` と拒否、(d) 確認の画面の MR のタイトルは作者が書いた文字
     （React が逃がす）→ 切り詰めと番号・SHA を目立たせる。
   - LOW（authz）: §4 の表が S2 の時点のまま（上に X3 の注記を足した）、`ListAsPerson` が §3 の例外（上に注記を足した）。
2. **条件 2 は満たした**: 利用者が §1 の社員番号の守りを S2 の範囲に含めることを受け入れた（2026-11-09。S2 は実装済み）→ **accepted**。
3. 条件 3 は未決のままでよい（変わらず）。

## 結びつけの確認の画面（2026-11-10、クラウド）

Amendment X3 の 4 の「確認の画面（GET の表示のデータ）は U3 で足す」を足した（kanflow の X3 の前提）。

- `GET /api/v1/projects/{id}/integration-bindings/act-as-confirmation?intent&integration&kanflow_project_id[&resolves_work_items]`
  （`ActAsConfirmView`、本人の Web セッションだけ、`Coverage::BrowserOnly`）。答えは forge が持つ事実だけ: リポジトリのパスと表示名、
  kanflow のプロジェクトは **連携が名指した ID**（kanflow が送った文字は出さない）、設定、**いま結びついているか**、連携の種類と名前、本人。
  問い合わせは厳しく読む（知らない・重なった引数、`binding_remove` の設定、`binding_set` の設定の欠けは 400）。判定は
  `ActAsAssertionIssue` と同じ関数（`binding_confirm_lookup`）を通り、権限の無い人に連携の状態を言わない順（上の LOW a）も同じ。
  `state` は GET に載せない（POST の本文だけ）。
- 画面 `/-/confirm/projects/{id}/integration-bindings`（`ActAsBindingConfirm`）: MR の確認の画面と同じ守り（フレームに入れない・`useArmed`・
  既定のフォーカスなし・`state` を読んだらアドレスから消す・不正な形のリンクは API に聞かずに断る）。結びついていない先の解除は押すものを出さない。
- 権限の表に `ActAsConfirmView` の 10 行（binding の `ActAsAssertionIssue` の行と同じ組）、`binding_routes` と web の `act-as-binding-confirm` のテスト。
- 独立レビュー（2026-11-10、`security-reviewer`）: HIGH 1 件を直した — 問い合わせの値を百分率の復号をせずに比べていたため、ブラウザーが `:` を
  `%3A` にした kanflow のプロジェクト ID が 400 になった → 値ごとに厳しく復号してから検査（壊れた `%` は 400。テストで `KF%3A7` と `KF%2F7`）。
  LOW: フレーム・COOP・殻の同一性のテストに結びつけの画面の経路を足した。残した指摘: (MEDIUM) 結べない Developer には 403、存在しない連携には 404
  なので、プロジェクトの読み手は連携の ID の有無を確かめられる（MR の確認の画面と同じ振る舞い。連携の ID は秘密ではなく、読めるのは有無だけ）→
  押せない人にはすべて 404 にするかを後で決める。(LOW) 画面は 403 をすべて「この人では行えない」と言う（連携の無効なども含む）→ 中立の文と
  `request_id` の表示を MR の画面と一緒に直す。

## レビューの残りの指摘を直した（2026-11-10、クラウド）

上の 2 つの確認で残した指摘のうち、利用者の判断が要らないものを直した（どれも先に失敗するテストを書いた）。

- （MEDIUM authz）公開の経路（`WebShell`・`Health`・`WebLogin`・`OidcBackchannelLogout`。固定の経路と振り分けの公開の語の両方）は、
  `Forge-Act-As` / `Forge-Assertion` を持つ要求を経路の前で **断る**（`FG-AUTH-0003 reason=act_as_not_allowed`、許可リストの外の経路と同じ。
  何も解析・検索しない。`forge-web/src/act_as.rs` の `refuse_on_public`）。テスト `public_routes_refuse_act_as_headers_instead_of_ignoring_them`。
- （LOW a）資格情報の無い要求の代理のヘッダーは `FG-AUTH-0001 reason=act_as_without_credential` で断る（外して匿名で続けない）。
  テスト `act_as_headers_without_a_credential_are_refused_never_stripped`。
- （LOW b）検証の後、経路が答える前に要求が切れたら、drop の守り（`act_as::Pending`）が `outcome=cancelled` の 1 行を出し、別のタスクで
  `reject_unused`（`reject_reason=request_cancelled`）を行う（1 つの短いトランザクション。プールの取得と文の時間の上限の内）。実行時が無いときは
  ログに出して 5 分で切れるのを待つ（従来どおり）。経路が答えた後の `finish` の途中で切れる狭い窓は残る。テスト
  `a_request_cut_off_after_the_verification_rejects_its_assertion`。
- （LOW c）鍵のバイトは drop で `zeroize` する（読み込みの途中の復号のバイトも）。全部同じバイトの鍵は読み込みで
  `FG-SYS-0002 config_key=auth.act_as_hmac_keys_file reason=key_degenerate` で断る（runbook `act-as` に追記）。テストの鍵も作り直した。
- （LOW d）MR の確認の画面のタイトルは 80 文字で切り、目立たない 1 行にした。番号と head の SHA を太字にした。
- （LOW、両方の確認の画面）403 は「この人では行えない」と言わず、中立の文（権限やアプリの連携の設定などが理由のことがある）と
  問い合わせ番号を出す（i18n `actAs.refusedTitle` / `refusedBody`、ja / en / id）。
- 直した後の独立レビュー（2026-11-10）の指摘も直した: (MEDIUM) drop の守りは `finish` が終わるまで外さない（`finish` の
  `reject_unused` の待ちで切れても拒否する。テスト `a_request_cut_off_while_its_assertion_is_being_rejected_still_rejects_it`）。
  (LOW) 守りはタスクを起こす前に `forge.auth.act_as_assertion_reject_scheduled` を出し（停止で失われたタスクが見える）、`cancelled` の
  行は「操作はもう確定したかもしれない」と書く。(LOW) 鍵のファイルの文字は `Zeroizing<String>` で読み、鍵の束は 1 回で確保する
  （伸ばすときに鍵の写しを解放しない。テスト `the_ring_never_reallocates_over_key_bytes`）。HMAC の内部の状態の残りは受け入れる。
  (LOW) 画面のタイトルは `Intl.Segmenter` が無いブラウザーでは code point で切り、双方向の制御文字（U+202A–202E・U+2066–2069）を
  取り除き、`unicode-bidi: isolate` で囲む。
- 残した: (MEDIUM) 押せない人に 403 か 404 か（利用者の判断を待つ）。
