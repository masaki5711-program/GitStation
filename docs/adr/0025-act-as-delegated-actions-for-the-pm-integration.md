# ADR-0025: 代理実行（act-as）— PM ツール（kanflow）が本人として行う操作

**Date**: 2026-11-05
**Status**: proposed（方式は計画 11 §3.5 の案 B で、利用者が承認済み（2026-11-02、§10 Q9 を含む）。**`security-reviewer`（必須）と `authz-matrix-auditor` のレビューを経て accepted にする**。ADR-0017 §4.0・§4.2 の代理の部分、ADR-0016 §9 の `FG-INT-0006` の範囲、ADR-0024 §12 の「代理」の列を、この ADR が具体化し、食い違う点はこの ADR が優先する（末尾「他の ADR との関係」））
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
| T2 | 侵害された kanflow が主張を作る・使い回す・別の MR / SHA に使う・本人に別の MR を押させる（ADR-0017 (e)） | 主張は forge だけが作り、操作・リソース・SHA・連携・本人に結びつき、HMAC で封じ、1 回で消える。確認の画面は forge のデータだけを表示する |
| T3 | 代理が権限の昇格の道になる（計画 11 §8） | 判定は 1 か所（`authorize()`）。許可リストは既定で閉じ、範囲はサービスアカウントの範囲と本人の権限の共通部分 |
| T4 | 自動化（kanflow の AI・自動完了・forge の自動マージ）が人間の承認を得る（計画 04 決定 8） | 主張は人の操作でしか発行されない。代理の資格情報を作れるのは HTTP の入口だけで、エージェントのトークンは代理できない |
| T5 | 社員番号（個人情報）がログ・監査・封印に流れる | 監査は HMAC だけ、ログはヘッダーを伏せる |

## Decision

**計画 11 §3.5 の案 B**: サービスアカウントのトークン + `Forge-Act-As` で本人を名指し、**状態を変える操作には、本人が forge の確認の画面で 1 回押して forge が発行した主張（`Forge-Assertion`）を必須にする**。読み取りは主張なしで、許可リストの読み取りだけを許す。判定は `authorize()` の 1 か所で、範囲は「サービスアカウントの範囲（結びつけたプロジェクト × 許可リスト ∩ トークンのスコープ）」と「本人のロール」の共通部分。M3 で代理できる変更は **承認と承認の取り消しだけ**（結びつけの変更は X3 で足す。マージ・設定は不可）。

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
  | `not_human` | 相手がサービスアカウント（自分自身を含む）かボット |
  | `disabled` | 相手が無効化されている |

  「存在しない」「無効」「人でない」を同じ応答にして、サービスアカウントに利用者の状態を区別させない（ADR-0016 §9）。ADR-0016 §9 が `FG-INT-0006` に含めていた「権限がない」は **含めない**: 相手が決まった後の権限の不足は `authorize()` の通常のコード（`FG-AUTH-0003`・`FG-REPO-0005`・`FG-REVIEW-0013`）で、どの規則で断ったかがログで分かる形にする。
- 失敗した解決はサービスアカウントごとに流量を制限する（1 分に 60 回を超えたら `FG-WEB-0010`）。社員番号の存在の当て推量を遅くする（kanflow は社員の一覧を既に持つので、守りの主目的は誤設定の早期の検出）。

### 2. 誰が代理できるか（資格情報の条件）

`Forge-Act-As` を受け付けるのは、次をすべて満たす要求だけ。満たさなければ断り、**ヘッダーを無視して本人として続けることはしない**。

| 条件 | 外れたとき |
|---|---|
| `Authorization: Bearer` の **サービスアカウント** のトークン（セッション・人の個人のトークン・ボットは不可） | `FG-AUTH-0003 reason=act_as_requires_service_account` |
| トークンが新しいスコープ **`act_as`** を持つ | `FG-AUTH-0002`（`required_scope=act_as`） |
| トークンが **エージェントと宣言されていない**（`is_agent = false`） | `FG-AUTH-0003 reason=agent_token` |
| サービスアカウントが **有効な連携** のアカウント（`integrations.service_account_id`、連携ごとに 1 つ） | `FG-AUTH-0003 reason=act_as_no_integration` / `integration_disabled` |
| 鍵（§5）が設定されている | `FG-AUTH-0003 reason=act_as_not_configured`（doctor `act_as.keys` が理由を出す） |

- **`act_as` スコープ**: 追記のマイグレーションで `tokens_scopes_known` に足す。ほかのどのスコープも満たさず、どのスコープにも満たされない（`api`・`admin` でも）。`tokens::issue` の不変条件に「`act_as` はサービスアカウントだけ」（`act_as_requires_service_account`、FG-AUTH-0008）と「`is_agent` と同時に持てない」（`act_as_agent_token`）を足し、`authorize()` も同じことを確かめる（二重の守り。ADR-0019 §8 と同じ形）。
- 代理の操作に要るスコープは「`act_as` + その Action のスコープ」。承認なら `act_as` と `merge_approve` の両方（`api` は `merge_approve` を満たさない、ADR-0017 §4.0）。

### 3. 範囲 = サービスアカウントの範囲 ∩ 本人の権限

- **サービスアカウントの側が決めるのは「どのプロジェクトで」「どの操作を」だけ**: プロジェクトは、その連携に結びつけたプロジェクト（`integration_bindings`、`via_bound`）、操作は §4 の許可リスト ∩ トークンのスコープ。**サービスアカウントのロール（結びつけで付く Reporter）は本人のロールの上限にしない**（上限にすると承認がそもそも通らない。計画 02 §5.3 の「範囲」はプロジェクトと操作の集合であってロールではない）。
- **本人の側が決めるのはロールと可視性**: `ProjectView` は本人の所属（`native_role()`。kanflow の出どころの能力（ADR-0017 §4.4）は、入ったときに `role_for` の規則どおりに数える）で読む。
- **本人の管理者の権限は通さない**: サービスアカウントのトークンは `admin` スコープを持てない（S0）ので、代理の主体の `admin_rights()` は常に false。所属のない管理者の代理は、見えないプロジェクトとして断る。
- **結びつけていないプロジェクト**は、本人が見えても見えなくても `FG-REPO-0005`（`reason=act_as_unbound`。存在を漏らさない）。本人が見えないプロジェクトは通常どおり `FG-REPO-0005`。
- 本人の判定は通常の arm をそのまま通る（自己承認の FG-REVIEW-0013、ロールの不足の FG-AUTH-0003、削除待ちの FG-REPO-0006）。経路ごとの分岐は作らない。

### 4. 代理で使える Action の許可リスト（既定で閉じる）

`Action::act_as_rule(self) -> ActAsRule`（`_` のない全列挙の `match`。新しい Action はコンパイルが `ActAsRule` を選ばせ、選ぶまで入らない）。M3（S2）の時点:

| Action | 規則 | 許す理由 |
|---|---|---|
| `ProjectRead` | 主張なしで可 | kanflow が「この人にこのプロジェクトが見えるか」を表示に使う。本人として読める範囲は、サービスアカウントが既に読める範囲（結びつけたプロジェクトの Reporter）の部分集合なので、何も広がらない |
| `MergeRequestRead` / `MergeRequestList` | 主張なしで可 | F-36 の画面に承認の状態と「この人が承認できるか」（`viewer` の欄、§9）を出すため。理由は上と同じ（読める範囲は狭まるだけ） |
| `MergeRequestApprove` | **主張（intent `approve`）が必須** | F-36（利用者の決定 2026-11-01）: 技術者でない承認者が kanflow から承認する。本人が forge の確認の画面で MR・SHA を見て押した主張があるときだけ、本人の承認として数える |
| `MergeRequestUnapprove` | **主張（intent `unapprove`）が必須** | 承認できる経路では取り消しもできないと、誤って押した人が forge の画面へ行く以外に直せない。取り消しは権限を減らす操作だが、他人の承認を消す道にしないため主張を要する（本人の承認しか消せないのは通常の arm） |

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

**X3 で足すもの（規則はここで決め、X3 の MR が許可リストと行を足す）**: `IntegrationBindingManage`（intent `binding_set` / `binding_remove`、主張が必須）。本人は対象のプロジェクトの `native_role()` の Maintainer 以上。**結びつけはまだ結びついていないプロジェクトが対象なので、§3 の `via_bound` の代わりに「主張の連携 = 要求の連携」と「結ぶ先がその連携」を範囲にする**（結びつけはサービスアカウントに Reporter を与えるメンバーの変更なので、主張なしでは決して許さない。ADR-0016 Amendment I1a の 2）。解除は結びついている先だけ。

**不変条件（テストで固定）**: 人だけの Action（`is_people_only`）で許可リストにあるものは、すべて `RequiresAssertion`（`every_people_only_act_as_action_requires_an_assertion`）。`is_session_only` の Action は `Denied`（`no_session_only_action_is_act_as`）。

### 5. 主張（assertion）

ADR-0017 §4.2 の確認の画面と claim の流れを引き継ぎ、名前・経路・鍵・コードをこの節で確定する。

```
kanflow の画面で本人が「承認」/「取り消し」を押す
  → kanflow のサーバーが 128 bit 以上の乱数の state を作り、kanflow のセッションの利用者と結びつけて保存し、ポップアップで開く:
    GET /-/confirm/projects/{id}/merge-requests/{iid}?intent=approve&sha=<head>&integration=<id>&state=<state>
  → forge のセッション（なければ SSO。戻り先は同じ origin のパスだけ）。MR が見えなければ 404（ProjectRead と同じ）
  → forge のデータだけを表示: MR の番号・タイトル・source / target、head の SHA（全桁）と target の先端、変更の要約（ファイル数・+/−・チェックの結論・blockers・承認）、
    押す内容（intent）、主張を受け取る連携の名前、「<表示名>（<利用者名>）として承認します」
    押せない（理由は MR が見える人にだけ）: 連携がこの MR のプロジェクトに結びついていない、sha が今の head と違う、本人に intent の権限がない、連携が無効
  → 本人が押す: POST /-/confirm/projects/{id}/merge-requests/{iid}/assertions（CSRF、本文 {intent, sha, integration, state}）
  → forge が主張を発行（下の表に保存。画面は「完了しました。この画面を閉じてください」だけ。主張を画面・URL・window.opener に出さない）
  → kanflow のサーバーが連携のサービスアカウントのトークンで POST /api/v1/integrations/{id}/act-as/claim {state}
    → {assertion, sub: {user_id, employee_number?}, intent, project_id, iid, head_sha, expires_at}（1 回限り。employee_number は user_employee_number のスコープのときだけ）
  → kanflow は sub が state に結びつけた kanflow の利用者と同じ人かを確かめ、違えば捨てる（共用の PC で別の人の forge のセッションで押された場合）
  → kanflow が POST /api/v1/projects/{id}/merge-requests/{iid}/approve {sha}
       Authorization: Bearer <サービスアカウントのトークン>、Forge-Act-As: user:<sub.user_id>、Forge-Assertion: <assertion>
  → forge が検証（§5.3）→ Principal::act_as → MR の行をロック → prove → 承認 → 同じ ChangeTx で主張を消費
```

#### 5.1 形と保存

- **形**: `fga_<public_id>_<secret>`（`public_id` は 16 桁の小文字の 16 進、`secret` は 64 桁。CSPRNG の 256 bit。トークンと同じ形で、秘密の走査で見つけやすい。ADR-0019 §1）。署名した JWS にはしない（forge が発行も検証もするので署名は要らない。ADR-0017 の「採らなかった案」）。型 `AssertionToken` は `Serialize` も `Display` も持たず、`Debug` は伏せ字。
- **表** `forge.act_as_assertions`（追記のマイグレーション、S2。台帳 `tables.toml` は `audited`、監査のリソースは `("act_as_assertion", id)`）:

  | 列 | 内容 |
  |---|---|
  | `id`、`public_id`（UNIQUE、16 進 16 桁の CHECK） | |
  | `mac`（`bytea`、32 バイト）、`key_id`（`smallint`） | §5.2。秘密そのものもその SHA-256 も保存しない |
  | `integration_id`（aud）、`user_id`（sub、`users.kind = human` をトリガーで守る） | |
  | `intent`（CHECK: `approve`・`unapprove`。X3 で `binding_set`・`binding_remove` を追記のマイグレーションで足す） | Action と 1 対 1 |
  | `project_id`、`merge_request_id`（MR の内部の ID。iid ではない）、`head_sha`、`target_sha` | 結びつけ。X3 の intent では `external_project_id` を足す |
  | `state_hash`（SHA-256） | claim の照合だけに使う |
  | `issued_at`、`expires_at`（CHECK `expires_at <= issued_at + interval '5 minutes'`）、`claimed_at`、`consumed_at`、`outcome`（`consumed` / `rejected`）、`reject_reason` | CHECK: 消費は claim の後、`outcome` と `consumed_at` は同時に入る |
  | `issued_request_id`、`consumed_request_id` | 診断の手がかり |

  `forge_app`: SELECT・INSERT と UPDATE(`claimed_at`, `consumed_at`, `outcome`, `reject_reason`, `consumed_request_id`) だけ。DELETE なし（保持の削除は後の MR で、O3 の保持と同じ形。1 人 1 日数十行の小さな表）。`forge_ro` は `mac`・`state_hash` を読めない。状態の変化ごとに監査の行（`act_as_assertion.issued`（主体は本人のセッション）/ `.claimed`（サービスアカウント）/ `.consumed` / `.rejected`（代理の主体））。

#### 5.2 HMAC の鍵と入れ替え

- **鍵**: 秘密のファイル `act_as_hmac_keys`（ADR-0013、ADR-0016 の「他の ADR への追記」で名前を予約済み）。行ごとに `<kid>:<base64 の 32 バイト>`、ちょうど 1 つに `active` の印、引退した鍵に `retired` の印。読めない・形が不正・`active` が 0 か 2 つ以上は起動の時点で `FG-SYS-0002`（設定の誤り。閉じる側に倒す）。ファイルが無いときは代理を無効にして起動し（§2 の `act_as_not_configured`）、doctor `act_as.keys` が「代理のスコープを持つ有効なトークンがあるのに鍵が無い」を `FG-SYS-0002` として出す。
- **用途は 2 つで、目的の文字列で分ける**（同じ鍵で別の値を作らせない）:
  1. 主張の封: `mac = HMAC-SHA256(K[kid], "forge.act_as.assertion.v1" ‖ secret ‖ 結びつけ)`。結びつけは `public_id`・`integration_id`・`user_id`・`intent`・`project_id`・`merge_request_id`・`head_sha`・`target_sha`・`expires_at` の長さつきの連結（`forge_core::act_as::binding_bytes`、1 つの関数だけが作る）。DB に書ける者（`forge_app` の資格情報）が行を挿入・書き換えても、鍵を持たなければ通る主張を作れず、行の SHA や MR を差し替えれば検証で `mac_mismatch` になる。
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
| forge-db | `public_id` の行がある（無くてもダミーの HMAC と定数時間の比較をする） | `unknown` |
| forge-db | 鍵が `retired` でない、`mac` が一致（定数時間） | `key_retired`、`mac_mismatch` |
| forge-db | 期限内、claim 済み、未消費 | `expired`、`not_claimed`、`consumed` |
| forge-db | `integration_id` = 要求のサービスアカウントの連携、`user_id` = `Forge-Act-As` で解決した本人 | `wrong_integration`、`wrong_user` |
| `authorize()` | Action の規則が `RequiresAssertion` なのに主張がない / `ReadWithoutAssertion` なのに主張がある | `missing` / `unexpected` |
| `authorize()` | `intent` = Action、`project_id`・`merge_request_id` = 証明のリソース | `wrong_action`、`wrong_resource` |
| `authorize()` | 本文の `sha` = 主張の `head_sha`、ロードした MR の head = `head_sha`、target の先端 = `target_sha` | `sha_mismatch`、`head_changed`、`target_changed` |
| ChangeTx（ロックの下） | `UPDATE … SET consumed_at = now(), outcome = 'consumed' WHERE id = $1 AND consumed_at IS NULL` が 1 行 | `consumed`（同時の 2 回目） |

- **消費**: 成功は操作と同じ ChangeTx（承認の行・監査・イベントと一緒に確定）。`VerifiedAssertion` を作った後に失敗した（`authorize()` の拒否、ロックの下の再判定の失敗、409 など）ときも、**別の確定したトランザクションで `rejected`（`reject_reason`）として消費する**。使い回せず、kanflow は確認の画面を開き直す。
- **時刻**: 期限は DB の `now()` で判定する（サーバーの時計と DB の時計の差を判定に入れない）。
- **claim の失敗**（`POST …/act-as/claim`）: その連携のアカウント以外・存在しない連携は `FG-REPO-0005`、`state` が無い・期限切れ・2 回目は `FG-AUTH-0020`（`claim_unknown`・`claim_expired`・`claim_repeated`）。`state` は 128 文字以内の `[A-Za-z0-9_-]`。
- **流量**: 発行は本人ごとに 10 分で 30 回、claim は連携ごとに 1 分で 120 回（`FG-WEB-0010`）。

#### 5.4 確認の画面（ADR-0017 §4.2 の守りを引き継ぐ）

- トップレベルのポップアップ。`Content-Security-Policy: frame-ancestors 'none'`、`X-Frame-Options: DENY`、`Cross-Origin-Opener-Policy: same-origin`。
- 押すボタンは、表示されてフォーカスを得てから 1 秒以上たち、その後に画面の中で新しいポインターかキーの操作があるまで無効。既定のフォーカスを置かず、Enter で送らない（クリックの時機を狙う攻撃）。
- 表示するのは forge のデータだけで、kanflow から来た文字（タイトル・説明）を表示しない。画面は U3（`feat/ui-webhooks-notifications-confirm`）。
- ログ・アクセスログ・トレースでは `Forge-Act-As`・`Forge-Assertion` のヘッダー、確認の画面の URL の `state`、claim の本文の `state` を伏せる。記録するのは `assertion_public_id`、`intent`、`integration_id`、`mr_id`、`head_sha`、`state_hash` の先頭 8 文字。

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
| 承認の行（ADR-0024 R5） | `credential_origin = act_as_assertion`、`direct = true`（ADR-0017 §4.2） | — |
| ログ（`forge.auth.act_as`、成功も失敗も 1 行） | `request_id`、`error_code`（失敗）、`reason`、`action`、`actor.user`（本人の ID）、**`actor.via`（必ず）**、`integration_id`、`credential_public_id`、`assertion_public_id`、`claimed_mac` の先頭 8 文字 | 同左（`assertion_public_id` なし） |
| イベント | `actor.kind = user_via_service_account`、`actor.via`（計画 02 §11.1） | — |

ヘッダーの値・社員番号・主張の秘密は、ログ・監査・イベント・ジョブに決して出さない（テスト `no_token_like_string_reaches_audit_events_or_jobs` に `fga_` を足す）。

### 7. 自動マージの既定 `full` との関係（自動化に人間の承認を作らせない）

計画 04 決定 8（2026-11-03）で、条件がそろった MR は forge が自分でマージする。保護ブランチで「人間の承認 1 件」がその条件に残るので、**代理は、その 1 件を人の操作なしに作れる道であってはならない**。次を守る。

1. **人間の承認として数えるのは、主張のある代理の承認だけ**。主張は forge のセッションで本人が押したときにだけ発行され（§5.4 の操作の待ちを含む）、代理の資格情報には承認の Action を主張なしで通す規則がない（`RequiresAssertion` の不変条件）。
2. **代理の主体を作れるのは HTTP の入口だけ**: `Principal::act_as` は forge-web の認証の層だけが呼び、forge の worker・自動マージの主体・ボット・システム・`Reserved` / `Queued` は代理の資格情報を持たない（`ActAsCredential` は forge-web の外で作れない型。呼び出し元のテスト `only_forge_web_builds_act_as_principals`）。
3. **エージェントのトークンは代理できない**（§2）。kanflow の AI の道具（計画 02 §8.4 K-8 の `propose_*`）が代理のトークンを持っても、承認には人の確認が要る。
4. **サービスアカウント自身の承認は不可のまま**（人だけの arm）。代理は「人だけ」を外す例外ではなく、「主張で人の在席を示した」ときだけ人の arm を通る。
5. **マージ・マージの設定・`merge.auto` の変更は代理で不可**（§4）。代理の承認が最後の条件になった MR が `full` で自動マージされるのは、人が forge の画面で承認したのと同じ扱いで、意図どおり。
6. AI（仕様照合）の承認（`MergeRequestAiApprove`、INTERNAL）は代理と無関係のまま（ADR-0017 §4.1）。

### 8. エラーコード（S2 で `errors.toml` に登録する。テストと同じコミット）

| コード | 名前 | HTTP | 重さ | runbook | 内容 |
|---|---|---|---|---|---|
| **`FG-INT-0006`** | ActAsTargetInvalid | 403 | info | `act-as` | §1 の解決の失敗。`reason` は §1 の表。本文は同じ |
| **`FG-AUTH-0020`** | ActAsAssertionInvalid | 403 | warn | `act-as` | §5.3 の検証と claim の失敗。`reason` は §5.3 の表。計画 11 §4.4 の「FG-AUTH-0019」は S0 が使ったので次の空き。ADR-0017 §4.2 の「401」は **403** に改める（サービスアカウントの認証は成功している。401 は kanflow にトークンを捨てさせる） |

既存のコードの再利用: 許可リストの外・資格情報の条件は `FG-AUTH-0003`（`act_as_not_allowed`・`act_as_requires_service_account`・`act_as_no_integration`・`integration_disabled`・`act_as_not_configured`・`agent_token`）、スコープは `FG-AUTH-0002`、結びつけの外は `FG-REPO-0005`（`act_as_unbound`）、自己承認は `FG-REVIEW-0013`、流量は `FG-WEB-0010`、鍵のファイルは `FG-SYS-0002`、発行の不変条件は `FG-AUTH-0008`（`act_as_requires_service_account`・`act_as_agent_token`）、DB は `FG-DB-0001`。

### 9. S2 の範囲（`feat/auth-act-as`）

- forge-web: ヘッダーの解析（各 1 つ）、`ActAsCredential` と `Principal::act_as` の構築、ログの伏せ字、`viewer` の欄（MR の読み取りに `can_approve: {allowed, reason?}`。主張があるものとして `authorize()` を試す共通のヘルパーで計算し、主張の要否を `requires_assertion: true` で示す）、claim のルート。
- forge-auth: `ActAsRule`、`Action::act_as_rule()`、代理の判定の順（§5.3 の `authorize()` の段）、行。
- forge-db: `act_as_assertions`、`act_as` スコープ（`tokens_scopes_known` の追記）、`verify_assertion` / `consume` / `reject`、本人の解決。
- forge-core: ヘッダーの文法、`AssertionToken`、`binding_bytes`、鍵の束。
- 確認の画面（`ActAsConfirmView` / `ActAsAssertionIssue`）の **サーバーの側** は S2、画面は U3。
- runbook `act-as`（新規）、doctor `act_as.keys`、`gstn` は無し（代理は kanflow のサーバーが使う。C-1 の表で `NotForCli`）。

## 失敗の場合（S2 の「先に書くテスト」）とログでの見え方

| 場合 | 応答 | ログ（`forge.auth.act_as`） | テスト |
|---|---|---|---|
| 許可リスト外の Action（例: 代理でマージ） | 403 `FG-AUTH-0003` | `reason=act_as_not_allowed action=MergeRequestMerge actor.via=…` | `act_as_merge_is_not_allowed`、生成の `act_as_denied_for_<action>` |
| 相手が無効 | 403 `FG-INT-0006` | `reason=disabled`（本文は unknown と同じ） | `act_as_disabled_target_is_int_0006` |
| 相手が人でない（サービスアカウント・ボット・自分） | 403 `FG-INT-0006` | `reason=not_human` | `act_as_service_account_or_bot_target_is_refused` |
| 相手がいない / メールの形 | 403 `FG-INT-0006` | `reason=unknown` / `email_form` | `act_as_unknown_and_disabled_look_the_same`、`act_as_email_form_is_refused` |
| 主張の再使用 | 403 `FG-AUTH-0020` | `reason=consumed assertion_public_id=…` | `assertion_is_single_use`、`concurrent_use_consumes_once` |
| 主張の期限切れ | 403 `FG-AUTH-0020` | `reason=expired` | `assertion_expires_after_five_minutes` |
| 別の SHA（本文・push 後の head・target の前進） | 403 `FG-AUTH-0020` | `reason=sha_mismatch` / `head_changed` / `target_changed` | `assertion_bound_to_head_and_target` |
| 別の MR・別の intent・別の連携・別の本人 | 403 `FG-AUTH-0020` | `reason=wrong_resource` / `wrong_action` / `wrong_integration` / `wrong_user` | `assertion_bound_to_action_resource_and_user` |
| 行の改ざん（DB で SHA を書き換え） | 403 `FG-AUTH-0020` | `reason=mac_mismatch` | `tampered_assertion_row_fails_the_mac` |
| ヘッダー 2 つ（`Forge-Act-As` / `Forge-Assertion`） | 403 `FG-INT-0006` / `FG-AUTH-0020` | `reason=multiple_headers` | `two_act_as_headers_are_refused`、`two_assertion_headers_are_refused` |
| 範囲外のプロジェクト（結びついていない） | 404 `FG-REPO-0005` | `reason=act_as_unbound` | `act_as_on_an_unbound_project_is_not_found` |
| 主張なしの承認 | 403 `FG-AUTH-0020` | `reason=missing` | `act_as_approve_without_assertion_is_refused` |
| 読み取りに主張 | 403 `FG-AUTH-0020` | `reason=unexpected` | `assertion_on_a_read_is_refused` |
| セッション・人のトークン・エージェントのトークンにヘッダー | 403 `FG-AUTH-0003` | `reason=act_as_requires_service_account` / `agent_token` | `act_as_header_needs_a_service_account_token` |
| `act_as` のないトークン | 403 `FG-AUTH-0002` | `required_scope=act_as` | `act_as_needs_the_act_as_scope` |
| 本人が作成者 | 403 `FG-REVIEW-0013` | `reason=self_approval`（`actor.via` つき） | `act_as_self_approval_is_refused` |
| 検証の後の失敗 | 4xx（その失敗のコード） | 主張は `rejected` で消費 | `assertion_is_consumed_when_the_action_fails` |

どの場合も、新しいセッションの Claude が `request_id` → `forge.auth.act_as` の行 → `reason` → runbook `act-as` の表、と辿れる。

## 権限の表に S2 が足す行（`forge-auth/src/matrix_rows_act_as.rs`）

1. **生成の deny の行**: `for a in Action::ALL` で `act_as_rule() == Denied` のすべてについて「代理（Developer 以上の本人 + 結びついたプロジェクト + 有効な主張）」の deny（`act_as_denied_for_<action>`。ADR-0024 #38 の宿題）。`is_session_only` のすべてを含む。
2. **allow の行**: `ProjectRead` / `MergeRequestRead` / `MergeRequestList` × 代理（Reporter の本人、結びついたプロジェクト、主張なし）。`MergeRequestApprove` × 代理（Developer の本人で作成者でない、`act_as` + `merge_approve`、`approve` の主張が一致）。`MergeRequestUnapprove` × 代理（承認した本人、`unapprove` の主張）。それぞれ **人の双子**（同じ本人のセッション）が allow であることを確かめる。
3. **deny の行**（承認と取り消し）: 主張なし、`unapprove` の主張で承認、別の MR の主張、本文の `sha` が違う、head・target が主張と違う、作成者の本人（FG-REVIEW-0013）、Reporter の本人、`merge_approve` のないトークン、`act_as` のないトークン、結びついていないプロジェクト（404）、無効の連携、所属のない管理者の本人（404）、エージェントのトークン、サービスアカウント自身（代理なし、既存の行を保つ）。
4. **読み取りの deny の行**: 本人が見えない・サービスアカウントが見えない（結びついていない）のどちらも 404、主張つきの読み取り（`unexpected`）。
5. **既存の行を保つ**: `matrix_rows_feed.rs`・`matrix_rows_dev_status.rs` の `act_as: true` の deny。
6. **確認の画面と claim**: `ActAsConfirmView` / `ActAsAssertionIssue`（`SessionOnly`、人だけ。intent の Action が本人のセッションで通り、連携が結びついている → allow。トークン・代理・`via_bound = false`・intent の Action の deny・見えない MR → deny）、`ActAsAssertionClaim`（その連携のサービスアカウントの `act_as` のトークン → allow。別の連携・人・代理・エージェントのトークン → deny）。
7. ADR-0017 の名前（`MergeConfirmView`・`MergeAssertionIssue`・`MergeAssertionClaim`）は、結びつけ（X3）でも使うので `ActAs…` に改める（Action はまだ作られていない）。

## Alternatives Considered

### A: サービスアカウント + `Forge-Act-As` だけ（計画 02 §5.3 の元の案、ADR-0005）
- **Pros**: 小さい。kanflow の改修も小さい。
- **Cons**: 1 つのサービスアカウントのトークンで、全員分の承認を作れる（T1）。自動マージ `full` の下では、kanflow が侵害されるか自動化が誤ると、人の承認の条件が機械に満たされる（T4）。
- **Why not**: 計画 11 §3.5 で「防げない」と判定し、利用者が B を承認した。読み取りには A の形をそのまま使う（読める範囲が広がらないため）。
- **失敗の見え方**: 不正な承認は成功として記録され、エラーコードが出ない（沈黙の失敗）。

### C: Keycloak のトークン交換（RFC 8693、本人のトークン。IoT 12 §4・§11.5）
- **Pros**: 本人の認証を IdP が保証する。ヘッダーで本人を名指す必要がない。
- **Cons**: IoT 側の Keycloak でトークン交換を有効にする必要があり、時期は **未確認**。forge が JWT のアクセストークンを API の資格情報として受ける経路（今は Web のログインだけ、ADR-0021）が新しく要る。交換されたトークンは「kanflow にログインしている」ことは示すが、「この MR のこの SHA を承認する意図」は示さない（確認の画面は結局要る）。
- **Why not now**: M3 の時期に間に合わない。B を C に替えられる形にしておく（下の移行）。

### 主張を kanflow が作る / 署名した JWS にする / URL や `window.opener` で渡す
- **Why not**: ADR-0017 の「採らなかった案」のとおり。kanflow が作れるなら T2 を防げない。JWS はアルゴリズムと `kid` の落とし穴と新しい crate を持ち込み、forge が発行も検証もする場面で得るものがない。URL は履歴・ログ・Referer に残る。

### 主張の秘密を SHA-256 だけで保存する（トークンと同じ）
- **Pros**: 鍵の管理が要らない。
- **Cons**: DB に書ける者が、自分で選んだ秘密の SHA-256 で行を挿入すれば、通る主張を作れる。行の SHA・MR を書き換えても検出できない。
- **Why not**: 主張は「人の在席」の証拠なので、DB の資格情報だけで作れないこと（鍵がファイルにあること）に意味がある。監査の `act_as_claimed` に HMAC の鍵がもともと要る（ADR-0016 §9）ので、増える運用は小さい。

### 社員番号だけ / forge の利用者 ID だけで指す
- 社員番号だけ: forge に社員番号の無い人（Keycloak の claim が無い）を指せない。利用者 ID だけ: kanflow が最初の読み取りの前に対応を知らない。**両方を受け、書き込みは主張の本人で必ず照らす**ので、どちらの形でも本人を取り違えて書くことはない。

### 代理のマージを M3 で許す（ADR-0017 §4.0 の許可リスト）
- **Why not**: 既定の `full` で forge が自分でマージするので要らず、代理の承認と組み合わさると人の関わりが 1 回のクリックに縮む経路が増える。M4 でマージキューと予約を作るときに、この ADR の Amendment で決め直す。

## Consequences

### Positive
- 1 つのサービスアカウントが人の承認を作れない（T1）。自動マージ `full` でも、人の承認の条件は人の操作でしか満たされない（T4）。
- 判定は `authorize()` の 1 か所のまま。代理は新しい `Credential` の種類と `ActAsRule` の 1 つの `match` で表され、許可リストは既定で閉じる（新しい Action は選ぶまで代理できない）。
- 代理のすべての失敗が 2 つの新しいコードと既存のコードの `reason` に分かれ、`actor.via` と `assertion_public_id` で監査とログを結べる。
- ADR-0016 で予約した監査の列をそのまま使い、監査の表のマイグレーションは要らない。

### Negative
- kanflow の承認は「押す → ポップアップで forge → もう一度押す」の 2 段になる（意図した費用。F-36 の画面の工夫は K4 と U3）。
- 鍵のファイルが 1 つ増え、入れ替えと復元の手順が runbook に増える。
- 承認の主張は head と target の両方に結びつくので、target が 5 分の間に進むと押し直しになる。

### Risks
- **kanflow の `sub` の照合の漏れ**: kanflow が `sub` を確かめないと、共用の PC で別の人の forge のセッションで押された主張が使われうる → forge は `Forge-Act-As` の本人 = 主張の本人を必ず確かめる（`wrong_user`）ので、他人として **書く** ことはできない。残るのは「押した人と kanflow の画面の人が違う」ことで、K4 のテスト（`sub` の不一致を捨てる）と runbook に書く。
- **本人を名指す読み取りの総当たり**: サービスアカウントは社員番号で人の存在を確かめられる → 読める範囲は増えない、流量の制限、失敗のログ。
- **鍵の漏えい**: 鍵と `forge_app` の資格情報の両方が漏れると主張を作れる → 鍵はファイルで別の権限、漏えい時は runbook `act-as` の入れ替え（承認つき）と、サービスアカウントの無効化（`UserDisable`）で代理を即座に止める。
- **社員番号の再利用**: 退職者の番号を新しい人に振り直す運用があると、kanflow の古いデータが新しい人を指す → forge の番号は一意で、無効の利用者は番号を持ったまま残るので、新しい人には同じ番号を付けられない（`users_employee_number`）。再利用の運用があるかは **未確認**（Keycloak の管理者に確かめる）。

## OAuth2 / トークン交換への移行

1. **S2（今）**: 案 B。`Credential::ActAs { via, via_token, via_scopes, assertion, claimed }`。
2. **IoT の Keycloak でトークン交換が使えるようになったら**（時期は未確認）: kanflow が本人の Keycloak のアクセストークンを、audience `gitstation`・`act` に kanflow のクライアントを持つトークンに交換し、`Authorization: Bearer <JWT>` で送る。forge は ADR-0021 の発行者の検証を API に広げ、`sub` を `user_identities` で本人に、`act.sub`（kanflow のクライアント）を連携に結ぶ。**`Credential` の種類は `ActAs` のまま**（`via_token` が「交換されたトークンの `jti`」になる）なので、`authorize()`・許可リスト・行・監査の列は変わらない。そのとき **`Forge-Act-As` は送らない**（本人は JWT が決める。両方あれば `FG-INT-0006 reason=multiple_identity_sources`）。
3. **主張は残す**: 交換されたトークンはログインを示すが、承認の意図と SHA を示さない。主張を外すかは、その時点で別の Amendment で決める（既定は残す）。
4. 監査の CHECK は代理の行に `credential_id` を要するので、交換されたトークンを記録する表（`jti`・連携・期限）を追記のマイグレーションで足す。
5. kanflow が移った後、`act_as` スコープの発行を止め（`tokens::issue` が `act_as_deprecated` で断る）、最長 365 日で既存のトークンが切れたら `Forge-Act-As` の解析を v2 で外す（v1 の間は `FG-INT-0006 reason=deprecated` で明示して断る）。

## 他の ADR との関係（この ADR が優先する点）

- **ADR-0017 §4.0**: `ActAs` の許可リストは §4 のとおりに狭める（M3 はマージの系統を外す）。§4.2: ヘッダーは `user:<email>` ではなく §1、検証の失敗は 401 ではなく 403 の `FG-AUTH-0020`、Action と経路の名前は `ActAs…` と `/-/confirm/projects/{id}/merge-requests/{iid}`・`/api/v1/integrations/{id}/act-as/claim`（ADR-0015・0024 のハイフンの規則）、主張は `fga_` の形で HMAC で封じる。§4.0 の「代理実行の ADR に残すこと」はこの ADR で閉じる（email の対応 → §1、退職・無効化 → §1 の `disabled`、トークン → §2、流量 → §1・§5.3、OAuth2 → 上の節、他の代理の操作 → §4 の X3）。
- **ADR-0016 §9**: `FG-INT-0006` は相手の解決の失敗だけにする（権限の不足は `authorize()` のコード）。
- **ADR-0024 §12**: 表の「代理」の列は §4 のとおり（承認・取り消しは主張つきで可、他は不可）。代理の deny の行は S2 で入れる。
- **ADR-0019**: スコープ `act_as`、`tokens::issue` の不変条件、復元での鍵の入れ替え。
- **計画 02 §5.3 / 計画 11 §4.4**: ヘッダーの形、`FG-AUTH-0019` → `FG-AUTH-0020`。

## Error codes and runbooks affected

- 追加（S2）: `FG-INT-0006` ActAsTargetInvalid（403、info）、`FG-AUTH-0020` ActAsAssertionInvalid（403、warn）。どちらも runbook `act-as`。
- 拡張（`reason` を足す、S2）: `FG-AUTH-0003`（`act_as_not_allowed`・`act_as_requires_service_account`・`act_as_no_integration`・`act_as_not_configured`）、`FG-AUTH-0008`（`act_as_requires_service_account`・`act_as_agent_token`）、`FG-REPO-0005`（`act_as_unbound`）、`FG-SYS-0002`（`act_as_hmac_keys`）。
- runbook 追加: `act-as`（`reason` ごとの原因と確かめ方、`forge admin act-as claimed-mac` での監査の突き合わせ、鍵の入れ替え（`[MUTATING — needs approval]`）、代理の即時の停止（サービスアカウントの無効化・連携の無効化）、kanflow の `sub` の照合の確かめ方）。
- runbook 更新: `backup-restore`（代理の鍵の入れ替え）、`service-accounts`（`act_as` スコープは連携のアカウントだけ、エージェントの印と同時に持てない）、`integrations`（代理を止める手順への参照）。
- doctor: `act_as.keys`（鍵の有無・`active` の数・`retired` の鍵の数、代理のスコープを持つ有効なトークンがあるのに鍵が無い）。
