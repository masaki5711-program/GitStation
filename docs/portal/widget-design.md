# PFS ポータル用ウィジェットの設計（v2 案）

GitStation を PFS ポータル（iotpf-portal）のホームに置くためのウィジェットの設計。現行の契約（3 個）は
[widgets.md](widgets.md)、判断の記録は ADR-0036。本書は **何を増やし、誰に、どこへつなぐか** を決める。

## 1. 前提（PFS の契約で決まっていること）

| 項目 | 決まり | 出典 |
|---|---|---|
| 描画 | ポータルが JSON を描く。iframe・アプリのスクリプトは不可 | PFS `14-portal-home-widgets.md` PW-1 |
| 型 | `kpi` / `series` / `status` / `list` / `links`（表・グラフは無い） | portal `widget-data.ts` |
| 大きさ | S = 3 列、M = 6 列、L = 12 列（12 列グリッド） | 同 §2.2 |
| 設定 | `select` / `multi` / `toggle` / `number`、選択肢はアプリから（`options_from`） | portal `widget-settings.ts` |
| リンク | アプリ内のパスのみ。**同じタブ**で開く。カード題名（`href`）・`status` 項目・`list` 項目・`links` 項目 | portal `isAppPath` |
| ボタン・操作 | **無い**（POST 不可）。操作は GitStation の画面で行う | 同 |
| 呼び出し | ポータルのサーバーから本人のトークン（`azp = portal`）で。3 秒、256 KB まで | portal `widget-proxy.ts` |
| 既定の配置 | `catalog/boards/*.yaml`。所属（`org_units`）・職位（`positions` / `min_rank`）・アプリのロールで選ぶ | PFS MR !24 |

このため「表示用途ごとのメニュー」は **`links` 型のウィジェット（用途別メニュー）** と、各カード・各項目のリンクで作る。

## 2. 利用者と、反応が必要な状態

Agent Ver2 / TMUX の流れ（仕様 → PBI → MR → 検査 → レビュー → マージ／キュー → 配備）で、**人が手を動かさないと止まる** 状態だけを出す。

| 用途（persona） | 主な人 | 反応が必要な状態 |
|---|---|---|
| 開発（developer） | 開発者、エージェントを動かす人 | 自分の MR が進まない理由（衝突・検査失敗・承認待ち・下書き）、パイプライン失敗、キューから外れた |
| レビュー（reviewer） | レビュアー、コードオーナー、メンテナー | 自分の承認待ち、返事が付いた未解決スレッド |
| 仕様（spec） | PM、PO、設備仕様を書く非エンジニア | 承認待ちの仕様書、未決定の項目、仕様の検査（spec-lint / spec-drift）の指摘 |
| 運用（operations） | 配備承認者、運用担当 | 本番配備の承認待ち、配備の失敗・戻し、凍結中 |
| 管理（manager） | 課長以上（`min_rank: 50`） | 止まっている件数の全体、承認待ちの滞留 |

## 3. ウィジェット一覧

✔ = 既存（PFS 台帳に登録済み）、＋ = 拡張、新 = 新規。すべて `required_role: access`。

| id | 型 | 大きさ | 更新 | 状態 | 内容 |
|---|---|---|---|---|---|
| `menu` | links | S, M（既定 S） | 3600 秒 | 新 | 用途別メニュー（§4） |
| `my-reviews` | list | M, L | 60 秒 | ✔＋ | 自分の仕事の一覧。`kind` に `threads` / `conflicts` / `queue` を追加 |
| `attention-count` | kpi | S | 120 秒 | 新（`stopped-count` を一般化） | 「対応が必要」の件数。`scope` で数える対象を選ぶ |
| `stopped-count` | kpi | S | 120 秒 | ✔ | 互換のため残す（中身は `attention-count?scope=stopped`） |
| `pipelines` | status | S, M, L | 60 秒 | ✔＋ | プロジェクトごとのランプ。`only_problems` を追加 |
| `deploy-approvals` | list | M, L | 60 秒 | 新 | 自分が承認できる本番配備の承認待ち、凍結中の表示 |
| `merge-queue` | status | M, L | 60 秒 | 新 | プロジェクトごとのマージキュー（動作中・一時停止・外された） |
| `spec-status` | status | M, L | 300 秒 | 新 | 仕様リポジトリごと: 承認待ち・未決定・検査の指摘 |
| `inbox` | kpi | S | 60 秒 | 新 | GitStation の通知の未読数 |

将来（設計のみ。データの出どころが GitStation に無い）: `agent-sessions`（TMUX の作業者の状態。`.claude/state/team` は手元だけ）、
`setup-health`（`gstn` のサインイン、トークンの期限、kanflow の結び付け）。

### 3.1 menu（links）— 用途別メニュー

| キー | 型 | 値 | 既定 |
|---|---|---|---|
| `purpose` | select | `developer` / `reviewer` / `spec` / `operations` / `manager` | `developer` |
| `project` | select（`options_from: options/project`） | プロジェクト | なし |

`project` を選ぶと、プロジェクト付きのリンク（パイプライン・キュー・仕様書）がそのプロジェクトを指す。選ばなければ全体の画面を指す。
中身は §4。カードの `href` は `/`（GitStation のホーム）。

### 3.2 my-reviews（list）— 拡張

既存の `kind`（`review` / `mine` / `stopped` / `spec`）に次を足す。`project` / `limit` はそのまま。

| `kind` | 出すもの | バッジ | 項目の href |
|---|---|---|---|
| `threads` | 自分が書いた未解決スレッドのうち、相手が返事を付けた MR | 返事あり（`info`） | `/<path>/-/pull/<iid>` |
| `conflicts` | 自分の MR の衝突・予測される衝突（相手の MR の番号付き） | 衝突（`stop`）/ 予測（`warn`） | `/<path>/-/pull/<iid>/changes` |
| `queue` | キューに入れた自分の MR（順番）と外された MR | `n 番目`（`info`）/ 外された（`stop`） | `/<path>/-/merge-queue?target=<branch>` |

追加の設定: `sort`（select: `oldest` / `newest`、既定 `oldest` = 待たせている順）。

### 3.3 attention-count（kpi）

| キー | 型 | 値 | 既定 |
|---|---|---|---|
| `scope` | select | `stopped`（止まった自分の MR）/ `reviews`（自分の承認待ち）/ `spec`（承認待ちの仕様）/ `deploy`（承認待ちの配備）/ `failed`（main の失敗パイプライン） | `reviews` |
| `project` | multi（`options_from`） | プロジェクト | なし |

`value` = 件数、`reference` = 0、`delta` が 0 なら `ok`。カードの `href` は `scope` ごとの一覧画面（§5）。

### 3.4 pipelines（status）— 拡張

追加: `only_problems`（toggle、既定 off）。on のときは `stop` / `warn` のプロジェクトだけを出し、無ければ空の状態（「問題なし」）。

### 3.5 deploy-approvals（list）

| キー | 型 | 値 | 既定 |
|---|---|---|---|
| `env` | multi | `prod` / `staging` / …（`options_from: options/env`） | なし = 自分が承認できるすべて |
| `limit` | number | 3〜10 | 5 |

項目: 題 = `<path> → <env>`、sub = MR の番号と待ち時間、バッジ = 承認待ち（`warn`）/ 凍結中（`stop`）。
href = 配備の画面（§6 の不足 2）。承認は GitStation の画面で行う（ウィジェットから承認はできない）。

### 3.6 merge-queue（status）

| キー | 型 | 値 | 既定 |
|---|---|---|---|
| `project` | multi（`options_from`） | プロジェクト | なし = 自分がロールを持つプロジェクト |

ランプ: 動作中 → `ok`、待ちあり → `info`、一時停止 → `warn`、外された MR がある → `stop`。`state_label` は「3 件待ち」など。
href = `/<path>/-/merge-queue?target=main`。

### 3.7 spec-status（status）

| キー | 型 | 値 | 既定 |
|---|---|---|---|
| `project` | multi（仕様の種類のプロジェクトだけ、`options_from`） | プロジェクト | なし |
| `show` | select | `approval`（承認待ち）/ `undecided`（未決定の項目）/ `lint`（検査の指摘） | `approval` |

ランプ: 0 件 → `ok`、承認待ち・未決定あり → `warn`、検査の失敗 → `stop`。href = 仕様書の一覧 `/<path>/-/documents`。
言葉は非エンジニア向け（「MR」と言わず「承認待ちの変更」）。

### 3.8 inbox（kpi）

設定なし。`value` = 未読数、href = `/-/notifications?unread=1`。

## 4. 用途別メニュー（menu の中身）

リンクは GitStation の画面のパス（`web/src/App.tsx`、`RepoViews.tsx`）。`{p}` は `project` 設定のパス。

| 用途 | リンク（上から） |
|---|---|
| 開発 | 自分の MR `/-/merge-requests?scope=authored`・対応が必要 `/-/merge-requests?scope=attention`・パイプライン `/{p}/-/pipelines`・マージキュー `/{p}/-/merge-queue`・通知 `/-/notifications?unread=1`・プロジェクト `/-/projects?tab=member` |
| レビュー | 自分の承認待ち `/-/merge-requests?scope=approvable`・対応が必要 `/-/merge-requests?scope=attention`・通知・プロジェクト |
| 仕様 | 仕様書 `/{p}/-/documents`・承認待ちの変更 `/{p}/-/pulls?state=open`・新しい仕様を書く `/{p}/-/new`・通知 |
| 運用 | パイプライン `/{p}/-/pipelines`・配備（§6 の不足 2）・マージキュー・通知 |
| 管理 | 対応が必要・承認待ち・プロジェクト `/-/projects`・グループ `/-/groups`・最近の動き `/-/activity` |

`{p}` が無いときは、プロジェクト付きのリンクを `/-/projects?tab=member`（プロジェクトを選ぶ画面）に置き換える。

## 5. 用途別の既定の配置（PFS `catalog/boards/`）

PFS MR !24 の 4 ボードを次の形に更新する（PFS 側の MR。所属コードは PFS の組織コードが決まってから）。

| ボード | 条件 | 配置（左上から、大きさ） |
|---|---|---|
| `gitstation-manager`（300） | `min_rank: 50` | menu S（manager）・attention-count S（reviews）・attention-count S（stopped）・inbox S／my-reviews M（review）・pipelines M（only_problems） |
| `gitstation-operations`（250） | 運用の所属 | menu S（operations）・attention-count S（deploy）・attention-count S（failed）・inbox S／deploy-approvals M・pipelines M（deploy） |
| `gitstation-developer`（200） | 開発の所属 | menu S（developer）・attention-count S（stopped）・attention-count S（reviews）・inbox S／my-reviews M（mine）・my-reviews M（review）／pipelines M（only_problems）・merge-queue M |
| `gitstation-spec-approver`（100） | gitstation の `access` だけ | menu S（spec）・attention-count S（spec）・inbox S／spec-status M（approval）・my-reviews M（spec） |

S は 4 つで 1 行（12 列）、M は 2 つで 1 行。

## 6. 足りないもの（実装で作る）

| # | 不足 | 対応 |
|---|---|---|
| 1 | ウィジェットの新しい経路（`menu`、`attention-count`、`deploy-approvals`、`merge-queue`、`spec-status`、`inbox`、`my-reviews` の新しい `kind`） | `portal-widgets/v1/*` に追加。`PortalWidgetRead` と権限表の行、エラーコードは既存（`FG-WEB-0011` など） |
| 2 | 配備の承認待ちを **人ごと** に引く API と、配備の画面のパス | forge-deploy に「自分が承認できる待ち」の問い合わせ、SPA に配備の画面 |
| 3 | 仕様のリンクの食い違い（新着は `/-/tags`、画面は `documents`） | 新着の href を `documents` にそろえる |
| 4 | 新着の穴（「レビューを頼まれた」「承認待ちの仕様」） | 出来事を足して `approval` の新着を送る（ADR-0036 の追記） |
| 5 | PFS 台帳 | `gitstation.yaml` に新しいウィジェットと設定、`widget_status: pilot`、ボードの更新（PFS の MR） |

## 7. 守ること

- 本人が GitStation の画面で見られるものだけを出す（管理者の権限は使わない）。機密のプロジェクトは件数にも入れない。
- 検査の結果が無い（`null`）ことを「成功」と描かない。衝突の調べが途中・失敗なら「衝突なし」と描かない（`unknown`）。
- 3 秒以内に答える（ポータルの上限）。重い集計は事前に作るか、件数の上限で打ち切る（`total` は 100 で頭打ち）。
- 言葉は Git を知らない人にも分かる形（plan 08）。ja / en / id をそろえる。

## 8. 進め方

1. **P1（GitStation だけで完結）**: `menu`、`inbox`、`attention-count`（`stopped` / `reviews` / `spec` / `failed`）、`my-reviews` の `threads` / `conflicts` / `queue`、`pipelines` の `only_problems`。
   実装済み（契約は [widgets.md](widgets.md)。§4 の「新しい仕様を書く `/{p}/-/new`」は SPA に画面が無いので入れず、代わりに「自分の承認待ち」。`attention-count?scope=deploy` は P2 まで 422）。
2. **P2**: 不足 2（配備の承認待ち）→ `deploy-approvals`、`attention-count?scope=deploy`、`merge-queue`、`spec-status`。
3. **P3**: 新着の穴（不足 4）、`agent-sessions` と `setup-health` の検討。
4. 各段の終わりに PFS セッションへ台帳とボードの更新を依頼する（PFS のリポジトリはこちらでは変えない）。
