# ADR-0038: 部・課・職位によるアクセス制限と承認

**Date**: 2026-11-20
**Status**: proposed（accepted は PFS 側の前提 §8 がそろい、利用者がこの案を確認した後）
**Deciders**: 利用者（藤川）、Claude

## Context

PFS の Keycloak（レルム `iotpf`）は、利用者ごとに所属（部・課・職場、兼務あり）と職位を持つ。利用者はこれを GitStation の
承認とアクセス制限に使うよう求めた（2026-11-20）。ADR-0021 の追記「組織の claim の写し」（Phase 1、ブランチ
`feat/auth-pfs-org-claims`）は、claim を `user_org_profiles` に写して表示するだけで、権限には使わないと決めた。本 ADR はその
Phase 2（アクセス制限）と Phase 3（承認）を決める。

PFS 側で確かめた事実（2026-11-20、`origin/main`）:

| 事実 | 出典 |
|---|---|
| トークン（アクセストークンだけ）の `org_units` は `[{code, primary}]`、最大 8 件、主所属はちょうど 1 件（兼務あり） | portal `packages/auth/src/claims.ts`、identity-api `normalise` |
| **トークンには階層（部か課か）も親も無い**。コードは `^[A-Z0-9][A-Z0-9_-]{0,19}$` で、形から親は読めない（読んではいけない） | `dev/pfsdb/05_L0020_org_position_masters.sql` |
| 組織の階層は `division / department（部）/ section（課）/ team（職場）`。親は `parent_org_code`、工場は別の列 | 同上 |
| `positions` は小文字のコード（最大 4）、`position_rank` は 0〜999 で、持っている職位の最大値 | 決定 2026-11-18(2) |
| 見本の職位ランク: operator 10、staff 15、line_leader 25、supervisor 35、assistant_manager 45、**manager 55**（課長に近い）、**general_manager 75**（部長）、director 85、president 95 | seed（spec の値とずれあり、ID-4 未決） |
| 組織の台帳 API: `GET /api/v1/org/units?plant=`（`level`、`parentCode`、`name{ja,en,id}`、`headStaffId`、`validFrom`、`version`）、`GET /api/v1/org/positions` | registry-api `src/domain/org.ts` |
| registry-api は `azp` が `REGISTRY_AUTHORIZED_PARTIES`（既定 `portal`）のトークンしか受けない | `docker-compose.yml:152` |
| （2026-11-20 追記）組織・職位の API は、環境変数 `ORG_READER_CLIENTS` に挙げたクライアントの client credentials のトークン（`client_id == azp`、`staff_id` なし、追加のスコープなし）も受ける。`gitstation` を挙げる。全工場（`?plant=` は任意）、応答は `{success, data:{items:[…]}}`。人のトークンは 403 | PFS の MR !28（未配備） |
| 異動はポータルの利用者管理で Keycloak に即時に書かれ、アクセストークン（5 分）の次の更新で届く。通知（webhook）は無い | identity-api、レルム設定 |
| 組織の改編は版を閉じて新しい版を足す（消さない） | 台帳の設計 |

GitStation 側の今の仕組み: 判定は `forge-auth` の `authorize()` 1 か所（純粋関数、読み込んだ事実だけで決める）。ロールは
Guest < Reporter < Developer < Maintainer < Owner、所属は手で付ける（グループ・プロジェクトのメンバー）。MR の承認数は
`forge-review/src/approvals/evaluate.rs` が数える（規則の承認者は利用者かグループ）。配備の承認は Maintainer 以上・
web のセッションだけ・本人と変更者は不可（ADR-0031）。文書の承認段階は ADR-0028（proposed）。

## Decision

部・課・職位を **「事実」として読み込み、判定は今の 1 か所のまま** 使う。

1. **組織の台帳を写す**: worker が registry-api から組織と職位の台帳を定期的に写し（`org_units_master`）、トークンの
   コードに階層・親・名前を与える。階層はコードの形から推測しない。
2. **アクセス制限 = 組織による所属（org binding）**: グループ・プロジェクトに「この部（配下を含む）/ この課の人は
   Reporter / Developer」という結び付けを置く。実効ロールは「手で付けた所属」と「結び付け」の高い方。上限は Developer。
3. **承認 = 組織による承認者の指定**: 承認規則の承認者に「申請者の課 / 部 / 指定した組織 の、職位ランク N 以上
   （または職位コード）」を足す。MR の承認規則、文書の承認段階、本番配備の承認の 3 か所で同じ選び方を使う。
4. **確かめられないときは閉じる**: 写しが古い・不正・台帳に無いコードなら、結び付けも承認者の資格も **無いもの** とする。
5. **本人・変更者の除外が先**: 職位がいくら高くても、自分の MR・自分が要求した配備は承認できない（今の規則のまま）。

## 1. 組織の台帳（新しい写し）

- 表 `org_units_master`（工場、コード、`level`、親のコード、名前 ja/en/id、`cost_center`、`head_staff_id`、`valid_from`、
  `version`、`changed_at`）と `org_positions_master`（工場、コード、名前 ja/en/id、`rank`、`sort_order`、`active`、`version`、
  `changed_at`）。キーは（工場, コード）。最後に成功した同期の時刻は 1 行の表 `org_master_state`。migration `20261231100000`、tables.toml では `derived`（外の台帳の写し）。
  読み取りのプール（`forge_ro`）は名前・階層・`level` を読めるが、`head_staff_id`（個人の情報）と `cost_center` は読めない。
- worker `org_master_sync` が 15 分ごとに `GET /api/v1/org/units` と `/api/v1/org/positions` を GitStation の
  クライアント（`auth.oidc.client_id`）の client credentials のトークンで読む（PFS は役割ではなく `ORG_READER_CLIENTS`
  の許可リストで受ける、§8）。タイムアウト 5 秒、失敗は指数バックオフ（1 分から倍に、間隔まで）、前の写しを保つ。
  すべての項目を確かめてから 1 つのトランザクションで置き換え（変わった行だけを書く。worker が複数でも advisory lock で
  1 つずつ）、変わったときだけ監査 `org.master_synced`（件数だけ）。組織の一覧が空の応答は受けない（registry の障害で
  写しを消さない）。書く前に一覧の全体で、親が同じ工場にあり `level` が上であることを確かめる。急な縮小（組織の半分より
  多くが消える、今ある工場の組織・職位が 0 件になる、職位が 1 件も無い）は、運用者が `org.allow_large_shrink`（既定 false）
  を一時的に有効にしない限り拒否する（FG-INT-0027）。
- 設定は新しい節 `[org]`（`org.master_sync_enabled` 既定 false、`org.master_sync_interval_secs` 900、`org.master_timeout_ms`
  5000、`org.master_max_age_hours` 24、`org.max_profile_age_hours` 24、`org.allow_large_shrink` false）。registry の URL と CA は `[portal]` のもの
  （`portal.registry_url`、`portal.ca_file`）を使う。`[portal]` の下に置かないのは、写しの古さの上限が権限の判定
  （§2・§3）にも効き、ポータルの新着・ウィジェットとは別の関心だから。
- 台帳の写しが 24 時間より古ければ `doctor` の `org.master` が FG-INT-0028 を出し、結び付けと承認者の資格は **閉じる**（§4）。
  同期の失敗は FG-INT-0025（届かない）・FG-INT-0026（拒否）・FG-INT-0027（中身が不正）。runbook `oidc-login`「組織の台帳の写し」。
- 名前（部・課の名前）は画面の表示にも使う（Phase 1b の「名前」はこれで満たす）。表示も `derive` と同じ条件（本人の写しが
  正しく新しい、台帳の写しが新しい）を確かめ、満たさないときも名前は出すが「最終確認時点」と示す（API の `stale: true`）。
  写しで求められないコードはコードのまま出す（表示だけで、判定には使わない）。

利用者ごとの **派生した所属** をこの台帳で作る:

| 派生 | 求め方 |
|---|---|
| 課（section） | 所属コードが課ならそれ。職場ならその親の課 |
| 部（department） | 課の親の部（所属が部ならそれ） |
| 主所属 | `primary: true` の 1 件。兼務は主所属以外 |

実装（段 A）: `forge_db::org_master::derive`。写しの工場（`plant_code`）と主所属のコードで台帳を引き、`parent_code` を
たどる（最大 8 段）。親はいつも上の `level` でなければならない。親が台帳に無い・まだ有効でない（`valid_from` が先）なら
`missing_parent`、同じか下の `level`・8 段を超える（循環）なら `broken_hierarchy` として **不明** を返す。ほかの不明の理由は
§4 の表のとおり（`no_snapshot`、`invalid_snapshot`、`stale_snapshot`、`master_missing`、`master_stale`、`no_primary`、
`no_plant`、`not_in_master`、`not_yet_valid`）。

## 2. アクセス制限 — 組織による所属（Phase 2）

### 2.1 結び付け（org binding）

表 `org_bindings`（新しい migration、追記のみ）:

| 列 | 意味 |
|---|---|
| `target` | グループかプロジェクト（どちらか 1 つ） |
| `plant_code` | 工場（必須。工場をまたぐ結び付けはしない） |
| `org_code` | 部・課・職場のコード（台帳にあること） |
| `include_descendants` | 配下の組織も含める（部に結び付けて、その課・職場を全部入れる） |
| `membership` | `primary`（主所属だけ）/ `any`（兼務も） 既定 `any` |
| `min_rank` | 任意。この職位ランク以上だけ（例: 管理職だけ Reporter） |
| `role` | `reporter` / `developer`（**上限 Developer**） |
| `created_by`、`created_at`、`reason` | 監査 |

- 実効ロール = max(手で付けた所属のロール, 当てはまる結び付けのロール)。グループの継承（配下のプロジェクト）は今と同じ。
- Maintainer・Owner・配備の承認は **結び付けでは付かない**（手で付ける）。権限の昇格を組織の変更だけで起こさない。
- 例: 「`pfs` グループに 生産技術部（配下を含む）= Developer」「`spec/equipment` に 品質保証課 = Reporter」。

### 2.2 判定への入れ方

- `authorize()` は変えない。ロールを読み込む所（`forge-db` の members の loader）で、結び付けを **もう 1 つのロールの出どころ**
  として足す。結果は `ProjectView.role` / `GroupSight` に入り、今の分岐で判定される。
- 誰の写しを使うか: `user_org_profiles` の行で、`invalid_since` が空、`confirmed_at` が `org.max_profile_age`（既定 24 時間）
  以内のもの。web のセッションは 5 分ごとの更新で新しく保たれる。個人トークン・エージェントのトークンは最後の写しを使い、
  24 時間 web に来なければ結び付けは効かない（手で付けた所属は効く）。
- 異動すると、次の更新（5 分以内）で写しが変わり、古い課の結び付けは外れる。キャッシュは写しの `digest` を鍵にする。

### 2.3 見せ方

- メンバーの画面に「組織から（生産技術部）」と出どころを出す。手で付けた所属と区別する。
- 結び付けで入っている人は、メンバーの画面から外せない（結び付けを変える）。

## 3. 承認 — 組織による承認者の指定（Phase 3）

### 3.1 承認者の選び方（共通の形）

```
approver_selector:
  relation:  requester_section | requester_department | unit
  unit:      <org_code>            # relation = unit のとき
  include_descendants: true|false  # relation = unit / requester_department のとき
  min_rank:  55                    # 職位ランク N 以上（例: manager = 55）
  positions: [manager]             # または職位コードで指定（どちらか）
  head_only: false                 # true = 台帳の head_staff_id の人だけ（PFS が値を入れた後）
  escalate:  parent | none         # 当てはまる人が 0 人なら 1 つ上の組織で選び直す
```

- **申請者** = MR の作者 / 配備を要求した人 / 文書の変更を出した人。申請者の **主所属** で課・部を決める（兼務は使わない。
  BankMasalah と同じ、迷ったら推測しない）。主所属が無い・台帳に無い → この規則は満たせない（「承認者を決められない」と表示）。
- 承認者の資格は **承認した時点の写し** で決め、その写し（コード・ランク・`digest`）を承認の行に残す（監査）。後で異動しても、
  その時に正しかった承認は有効。ただし承認時に写しが古い・不正なら数えない。

### 3.2 使う場所

| 承認 | 今 | 足すもの |
|---|---|---|
| MR の承認規則（`approval_rules`） | 承認者 = 利用者かグループ、空なら Developer 以上の誰でも | 承認者の種類に `selector` を足す（`approval_rule_approvers` の「1 種類だけ」の制約を 3 種類にする migration） |
| 文書の承認段階（ADR-0028） | 段階ごとに承認者グループと人数 | 段階の承認者に `selector` を使える。例: 「確認 = 申請者の課の staff 以上」「承認 = 申請者の部の general_manager 以上」 |
| 本番配備の承認（ADR-0031） | Maintainer 以上、本人・変更者は不可 | 環境の設定に `approver_selector` を任意で足す。**Maintainer 以上 かつ selector に当てはまる** 人だけ（条件を足すだけで、緩めない） |

既定の例（設定の見本、利用者が決める）:

| 対象 | 規則 |
|---|---|
| 通常の MR | 申請者の課の Developer 以上 1 名（今と同じ）＋ CODEOWNERS |
| 設定・CI の変更 | 申請者の課の `min_rank: 55`（マネージャー以上）1 名 |
| 設備仕様書の公開 | 確認 = 申請者の課の staff 以上 1 名、承認 = 申請者の部の `min_rank: 75`（部長）1 名、`escalate: parent` |
| 本番配備 | Maintainer かつ 申請者の部の `min_rank: 55` 以上 |

### 3.3 判定への入れ方

- MR・文書: 数えるのは `evaluate.rs`。`ApproverFacts` に承認者の派生した所属とランクを足し、`in_rule` で selector を評価する。
  `classify` の除外（古い head、本人、変更者、人でない）は selector より先。
- 配備: `authorize()` の `DeployApprove` の分岐に、読み込んだ事実「selector に当てはまる」を 1 つ足す（`Resource::Deployment`
  に値を足す）。判定は 1 か所のまま。権限の表に許可・拒否の行を足す。
- 画面: MR の「承認できる人」に、selector で選ばれた人を部・課の名前付きで出す。0 人なら「承認できる人がいません（品質保証課
  にマネージャー以上がいない）」と、次に何をすればよいかを出す。

## 4. 閉じる条件（fail closed）

| 状態 | 結び付け | 承認者の資格 | 表示・記録 |
|---|---|---|---|
| 写しが無い（GitStation に PFS でログインしていない） | 効かない | 無い | — |
| 写しが `invalid_since` あり（FG-AUTH-0028） | 効かない | 無い | 本人と管理者に表示 |
| 写しが `org.max_profile_age` より古い | 効かない | 無い | 「所属を確かめられません。一度 web でログインしてください」 |
| 所属コードが台帳に無い・閉じた版 | その所属は効かない | その所属では無い | `doctor` と監査に出す |
| 組織の台帳の写しが 24 時間より古い | 効かない | 無い（承認待ちのまま） | `doctor` が警告、新しいエラーコード（実装時に `errors.toml` へ） |

手で付けた所属と、selector を使わない今の承認規則は、これらの状態でも今のまま動く（組織の機能だけが止まる）。

## 5. 管理と監査

- 結び付けの管理（新しい action `OrgBindingManage`）: そのグループ・プロジェクトの Owner と管理者。人だけ、web のセッションだけ。
- 承認規則の selector の変更は今の `ApprovalRuleManage`（Maintainer）。ただし本番配備の selector は Owner と管理者。
- 結び付け・selector の変更、写しの変更（Phase 1 で済）、台帳の同期の結果はすべて監査に残す。
- 権限の表（matrix）に、新しい action と「結び付けで Developer になった人ができること / できないこと（Maintainer の操作、配備の承認）」
  の行を足す。

## 6. 個人情報の扱い

- 職位・ランクは本人と管理者だけが見る（Phase 1 のまま）。ほかの人に見せるのは部・課の名前と「承認できる/できない」だけ。
- **段 A から（2026-11-20）**: サインインした利用者は **だれでも**、ほかの利用者の主所属の部・課の名前（とコード）を見られる。
  見える場所は人を選ぶ画面（`GET /api/v1/users?search=`、`UserSearch`）と MR の承認者の一覧（`MergeRequestRead`）。
  兼務の所属、職位・ランク、工場、部署の長の社員番号（`head_staff_id`）は見せない（本人と管理者だけ。`head_staff_id` は
  読み取りのプールにも出さない）。
- 承認の行に残す写しは、コード・ランク・`digest` だけ（名前は台帳から引く）。

## 7. 進め方

| 段 | 中身 | 前提 |
|---|---|---|
| 0 | Phase 1（`feat/auth-pfs-org-claims`）のレビューとマージ | — |
| A | 組織の台帳の写し（§1）、画面に部・課の名前 | PFS: GitStation のサービスアカウントで registry-api を読めるようにする |
| B | 組織による所属（§2） | A、本 ADR の accepted |
| C | MR と配備の承認者の指定（§3） | A |
| D | 文書の承認段階で selector | ADR-0028 の accepted |

## 8. PFS 側への依頼（PFS のリポジトリはこちらでは変えない）

1. registry-api の組織・職位の API を GitStation のサービスアカウント（`azp = gitstation`）で読めるようにする
   （`REGISTRY_AUTHORIZED_PARTIES` に足すか、読み取り専用のロール）。
   **→ 2026-11-20 PFS が決定（MR !28、未配備）: ロールではなく環境変数の許可リスト `ORG_READER_CLIENTS=gitstation`。**
   `gitstation` のクライアントの id と secret で `grant_type=client_credentials`（追加のスコープなし）。トークンは
   `client_id == azp` で `staff_id` を持たない。GitStation はこの前提で段 A を作った。
2. 本物の組織コードの投入（試験サーバー）と、GitStation の既定のボードの `SYN-*` の置き換え。
3. 職位ランクの確定（spec ID-4。見本と spec の値、`test-supervisor` の 30 / 35 のずれ）。
4. `head_staff_id` を使うなら、その値の投入（使わないなら `head_only` は作らない）。
5. 社員番号（`staff_id`）の登録（68 人中 3 人だけ。無いと GitStation にログインできない）。

## Alternatives Considered

### Keycloak のグループ・クライアントロールで権限を配る
- **Pros**: GitStation 側の作りが小さい。
- **Cons**: 部・課ごとにロールを作り、異動のたびに PFS の管理者が付け替える。PFS は所属を属性で持ち、グループは使っていない。
- **Why not**: 所属の正は属性（`org_units`）で、二重管理になる。

### コードの形（接頭辞）から部・課を推測する
- **Pros**: 台帳の同期が要らない。
- **Cons**: 形は決まっていない（PFS は「読むな」としている）。改編で壊れる。
- **Why not**: 誤った権限は事故になる。台帳を正にする。

### 結び付けで Maintainer まで付ける
- **Pros**: 管理の手間がさらに減る。
- **Cons**: 組織の変更（ポータルの操作）だけで、保護ブランチ・設定・配備の権限が付く。
- **Why not**: 昇格は人が手で行う。結び付けは Developer まで。

### 兼務も申請者の課として使う
- **Pros**: 兼務の人の変更を、どちらの課の上長でも承認できる。
- **Cons**: どの課の承認か曖昧になる。
- **Why not**: 主所属だけにする（必要なら規則で `relation: unit` を明示する）。

## Consequences

### Positive
- 異動・兼務がポータルの利用者管理だけで GitStation の権限と承認者に反映される（5 分以内）。
- 「課長の承認」「部長の承認」を規則として書ける。承認時の所属が監査に残る。
- 判定は `authorize()` と `evaluate.rs` のまま。権限の表で確かめられる。

### Negative
- 組織の台帳の写しと同期の worker が増える。PFS 側の作業（§8）が前提になる。
- 個人トークン・エージェントだけで働く人は、24 時間に 1 回 web でログインしないと結び付けが効かない。

### Risks
- 職位ランクが未確定（ID-4）。規則の数値は設定で持ち、確定後に見直す。
- 台帳と Keycloak の属性がずれる（台帳に無いコード）。閉じる側に倒し、`doctor` と監査で見えるようにする。
- 承認者が 0 人になる課（管理職不在）。`escalate: parent` と画面の案内で止まったままにしない。
