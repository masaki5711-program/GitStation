# PFS ポータル連携（ホームのウィジェットと新着）

GitStation が PFS のポータル（iotpf-portal）のホームに出すウィジェットと、ポータルへ送る新着の一覧。
契約は PFS の `10-pfs/14-portal-home-widgets.md`（v1、英語が正）。設計の判断は ADR-0036、運用は runbook `portal-widgets`。

## 宣言（PFS の台帳 `catalog/apps/gitstation.yaml` の `widgets:`）

`required_role` はすべて `access`（Keycloak のクライアント `gitstation` のロール）。`news: true`。

| id | type | 大きさ | 更新 | エンドポイント |
|---|---|---|---|---|
| `my-reviews` | list | M, L（既定 M） | 60 秒 | `/api/portal-widgets/v1/my-reviews` |
| `pipelines` | status | S, M, L（既定 M） | 60 秒 | `/api/portal-widgets/v1/pipelines` |
| `stopped-count` | kpi | S | 120 秒 | `/api/portal-widgets/v1/stopped-count` |

## 設定（「何を表示するか」）

すべてのウィジェットは `locale`（`ja` / `en` / `id`、無ければ `en`）と `tz`（IANA の名前。形だけ確かめ、時刻は UTC で返す）を受ける。
知らないキーは無視する。値が不正なら 422（`FG-WEB-0011`、本文 `{success: false, error: {code, message, request_id}}`）。

### my-reviews（list）

| キー | 型 | 値 | 既定 | 意味 |
|---|---|---|---|---|
| `kind` | select | `review` | ✓ | 自分のレビュー待ち: 開いていて下書きでない、自分が承認でき、現在の head をまだ承認していない MR（自分の MR は出ない） |
| | | `mine` | | 自分が出した、開いている MR。バッジは 検査が失敗 / 衝突 / 下書き / 進行中 |
| | | `stopped` | | 現在の head で自動マージが止まった MR のうち、自分が作者か承認者のもの |
| | | `spec` | | 承認待ちの仕様書: 仕様を公開したことのあるプロジェクト（`spec.released`）の `review` |
| `project` | select（`options_from: options/project`） | プロジェクトの id | なし = 見られるすべて | 1 つのプロジェクトに絞る |
| `limit` | number | 3〜10 | 5 | 件数。`total` は見える件数（100 で頭打ち） |

項目: `id`（`<path>!<iid>`）、`label`（MR の題）、`sub`（`<path> !<iid>`）、`badge`、`href`（`/<path>/-/pull/<iid>`）。カードの `href` は `/-/merge-requests`。

### pipelines（status）

| キー | 型 | 値 | 既定 | 意味 |
|---|---|---|---|---|
| `project` | multi（`options_from: options/project`） | プロジェクトの id（繰り返す、最大 24） | なし = 自分がロールを持つプロジェクト | 表示するプロジェクト |
| `show` | select | `pipeline` / `deploy` | `pipeline` | `refs/heads/main` の最新のパイプライン / 最新の配備 |

ランプ: パイプライン 成功 → `ok`、失敗・不正・エラー → `stop`、取り消し → `warn`、待ち・実行中・該当なし → `info`、まだ無い → `unknown`。
配備 成功 → `ok`、失敗・戻せなかった → `stop`、戻した・止められた → `warn`、承認待ち・待ち・実行中 → `info`。`state_label` は環境（`prod` など）付き。
見られないプロジェクトを指定しても出ない（無いのと同じ）。

### stopped-count（kpi）

| キー | 型 | 値 | 既定 | 意味 |
|---|---|---|---|---|
| `scope` | select | `mine` / `reviews` | `mine` | 自分の MR だけ / 自分が承認した MR も |

`value` = 止まっている件数、`reference` = 標準 0、`delta` = 標準との差（0 なら `ok`、それ以外は `stop`）。

### 選択肢

`GET /api/portal-widgets/v1/my-reviews/options/project` と `…/pipelines/options/project`: 本人がロールを持ち読めるプロジェクト
（新しい順、最大 200）。`{success: true, data: {options: [{value: "<id>", label: "<path>"}]}}`。

## 認証と認可

- ポータルは本人のアクセストークン（`azp = portal`）を `Authorization: Bearer` で付ける。GitStation はレルムの鍵で検証し、
  `azp` が `portal.client_id` でなければその場で 401（`FG-AUTH-0026 reason=azp`）。`resource_access.gitstation.roles` に `access` が
  あることは `authorize()`（`PortalWidgetRead`）で確かめる。
- 人は社員番号（`staff_id`）でサインインと同じ規則で探す（読み取りだけ。ウィジェットの呼び出しで利用者を作らない。
  `auth.oidc.link_auth_sources` に無い種類のアカウントや、ほかの IdP に結び付いたアカウントは社員番号では見つからない）。
  GitStation に一度もログインしていない人は 403 `FG-AUTH-0027`。
- 呼び出しの上限（全員で 1 つ）: 同時に 16 件、1 分に 6000 件。超えると 429（`FG-WEB-0010`、`Retry-After` 付き）。ポータルは
  次の更新まで前の答えを出す。
- 成功の応答は `Cache-Control: private, max-age=30`、失敗の応答は `Cache-Control: no-store`。
- 見せるのは本人が GitStation の画面で見られるものだけ（管理者の権限は使わない）。
- このトークンはウィジェットの経路でだけ受ける。ほかの API は 401 のまま。

## 新着（ポータルへ送る）

`portal.enabled` と `portal.news_enabled` が on のときだけ。業務の変更と同じトランザクションで outbox（`forge.portal_news`）に書き、
`forge worker` が `POST {portal.registry_url}/api/v1/news` で送る（202 で完了、失敗は指数バックオフで再送、`source_id` で冪等）。

| 出来事 | kind | severity | 宛先（`audience.staff_ids`） | href | 表示の期限 |
|---|---|---|---|---|---|
| 配備が失敗した（失敗・戻した・戻せなかった、止まった worker の配備も） | `alert` | `stop` | 要求した人、承認した人、MR の作者 | `/<path>` | 7 日 |
| 配備が終わった（成功） | `notice` | `ok` | 同上 | `/<path>` | 7 日 |
| 自分の MR の自動マージが止まった | `alert` | `warn` | MR の作者 | `/<path>/-/pull/<iid>` | 7 日 |
| 仕様の版を公開した | `release` | `info` | プロジェクトにロールを持つ人（最大 200） | `/<path>/-/tags` | 30 日 |

- 題と本文は ja / en / id。中身はプロジェクトのパス、MR の番号、環境、版、短い SHA だけ（人の名前・メール・MR の題・本文は入れない）。
- 送る直前に宛先を決め直す: 有効な人で、社員番号があり、プロジェクトを読める人だけ。誰も残らなければ送らない（`dropped`）。
- 「レビューを頼まれた」「承認待ちの仕様」の新着は出さない（GitStation にその出来事が無い。ADR-0036 の契約の穴）。
