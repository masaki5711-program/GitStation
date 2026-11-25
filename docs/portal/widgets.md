# PFS ポータル連携（ホームのウィジェットと新着）

GitStation が PFS のポータル（iotpf-portal）のホームに出すウィジェットと、ポータルへ送る新着の一覧。
契約は PFS の `10-pfs/14-portal-home-widgets.md`（v1、英語が正）。設計の判断は ADR-0036、運用は runbook `portal-widgets`。

## 宣言（PFS の台帳 `catalog/apps/gitstation.yaml` の `widgets:`）

`required_role` はすべて `access`（Keycloak のクライアント `gitstation` のロール）。`news: true`。

| id | type | 大きさ（`sizes`、太字が `default_size`） | 更新 | エンドポイント |
|---|---|---|---|---|
| `my-reviews` | list | 1x2、**2x2**、4x2 | 60 秒 | `/api/portal-widgets/v1/my-reviews` |
| `pipelines` | status | 2x1、**2x2**、4x2 | 60 秒 | `/api/portal-widgets/v1/pipelines` |
| `stopped-count` | kpi | **1x1**、2x1 | 120 秒 | `/api/portal-widgets/v1/stopped-count` |
| `attention-count` | kpi | **1x1**、2x1 | 120 秒 | `/api/portal-widgets/v1/attention-count`（P1） |
| `inbox` | kpi | **1x1** | 60 秒 | `/api/portal-widgets/v1/inbox`（P1） |
| `menu` | links | **1x2**、2x2 | 3600 秒 | `/api/portal-widgets/v1/menu`（P1） |
| `merge-queue` | status | 1x2、**2x2**、4x2 | 60 秒 | `/api/portal-widgets/v1/merge-queue`（P2a） |
| `spec-status` | status | 1x2、**2x2**、4x2 | 300 秒 | `/api/portal-widgets/v1/spec-status`（P2a） |
| `deploy-approvals` | list | 1x2、**2x2**、4x2 | 60 秒 | `/api/portal-widgets/v1/deploy-approvals`（P2b） |

P1・P2a・P2b は設計（[widget-design.md](widget-design.md) §8）の段。PFS の台帳に載せる形は末尾の「台帳の写し」。

### 大きさ（2026-11-23、PFS のセル方式）

PFS のポータルは大きさを「横 × 縦のセル数」で決める（Android のホーム画面のウィジェットと同じ考え方。iotpf-portal MR !33、
pfs-prototype MR !82）。セル 1 つはボードの横幅の 1/4（1440px の画面で約 330px）× 約 150px。利用者が大きさを変えるときは、
宣言した大きさにだけ吸着する。旧 S / M / L は当面 1x1 / 2x2 / 4x2 として読まれる。

| 大きさ | PFS の用途 | GitStation で選んだもの |
|---|---|---|
| 1x1 | 数字だけ | kpi 3 つ（件数と標準 0 のランプ） |
| 2x1 | 数字と小さな推移 | `stopped-count` / `attention-count`（件数と標準との差）、`pipelines`（ランプ 4 つまでの帯） |
| 1x2 | 短い一覧 | list・status・links（3 件ほど） |
| 2x2 | 一覧・グラフ | list・status の既定 |
| 4x2 | 横長の表 | list・status の広い表示（24 件まで見える） |
| 4x4 | 詳細 | 使わない（詳細は GitStation の画面へ `href` で渡す） |

- ポータルは描く大きさをヘッダー `x-portal-widget-size: <大きさ>` で渡す。**GitStation は今は大きさで中身を変えない**
  （ヘッダーは読まない。件数は `limit` の設定、項目はポータルが入る分だけ描く）。大きさごとに件数を変えるのは P3 で検討する。
- ポータルのアプリ一覧・アプリのタイル・ウィジェットのカードの見出しに出すアイコンは、**GitStation の正式なアイコン**
  [`web/src/assets/brand/gitstation-v1-icon.svg`](../../web/src/assets/brand/gitstation-v1-icon.svg)（正方形、角丸。ブラウザのタブと同じ）
  をそのまま使う（2026-11-24 利用者の指定。簡略版のアイコンは使わない）。
  PFS 側の置き場所は `portal/public/brand/apps/gitstation/icon.svg`（PFS の MR で入れる。このリポジトリから PFS は変えない）。

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
| | | `threads` | | 自分が始めた未解決のコメントのうち、いちばん新しい書き込みが自分以外のもの（返事が来た）がある、開いている MR（だれの MR でもよい）。バッジは 返事あり（`info`） |
| | | `conflicts` | | 自分が出した、開いている MR のうち 衝突（`stop`）/ 衝突の見込み（`warn`、相手の番号を `sub` に `↔ !n` で付ける）/ 調べきれていない（`neutral`: 衝突の調べがまだ・途中（`pending` / `scanning`）か `incomplete` / `failed`、マージできるかの確認がまだ・途中（`unchecked` / `checking`）か `check_failed`、相手との組が `failed`）。調べが終わっていないものを「衝突なし」とは出さない |
| | | `queue` | | 自分が出した MR のうち、マージ待ちの列に入っているもの（バッジ `n 番目`、`info`）と、列が自分で外したもの（人が外したものは出さない。`stop`）。外されたものは、もう一度列に入れるか MR を閉じる・マージするまで出し続ける |
| `sort` | select | `oldest` / `newest` | `oldest` | 古い順（待たせている順）/ 新しい順 |
| `project` | select（`options_from: options/project`） | プロジェクトの id | なし = 見られるすべて | 1 つのプロジェクトに絞る |
| `limit` | number | 3〜10 | 5 | 件数。`total` は見える件数（100 で頭打ち） |

項目: `id`（`<path>!<iid>`）、`label`（MR の題）、`sub`（`<path> !<iid>`）、`badge`、`href`（`/<path>/-/pull/<iid>`。
`conflicts` は `/<path>/-/pull/<iid>/changes`、`queue` は `/<path>/-/merge-queue?target=<ブランチ>`）。カードの `href` は `/-/merge-requests`。

### pipelines（status）

| キー | 型 | 値 | 既定 | 意味 |
|---|---|---|---|---|
| `project` | multi（`options_from: options/project`） | プロジェクトの id（繰り返す、最大 24） | なし = 自分がロールを持つプロジェクト | 表示するプロジェクト |
| `show` | select | `pipeline` / `deploy` | `pipeline` | `refs/heads/main` の最新のパイプライン / 最新の配備 |
| `only_problems` | toggle | `true` / `false` | `false` | `true` のとき `stop` と `warn` のランプだけを出す（24 件の上限より先に絞るので、新しい正常なプロジェクトの後ろにある失敗も落ちない）。無ければ空（ポータルが「問題なし」の空の状態を出す） |

ランプ: パイプライン 成功 → `ok`、失敗・不正・エラー → `stop`、取り消し → `warn`、待ち・実行中・該当なし → `info`、まだ無い → `unknown`。
配備 成功 → `ok`、失敗・戻せなかった → `stop`、戻した・止められた → `warn`、承認待ち・待ち・実行中 → `info`。`state_label` は環境（`prod` など）付き。
見られないプロジェクトを指定しても出ない（無いのと同じ）。

### stopped-count（kpi）

| キー | 型 | 値 | 既定 | 意味 |
|---|---|---|---|---|
| `scope` | select | `mine` / `reviews` | `mine` | 自分の MR だけ / 自分が承認した MR も |

`value` = 止まっている件数、`reference` = 標準 0、`delta` = 標準との差（0 なら `ok`、それ以外は `stop`）。
P1 からは `attention-count?scope=stopped` が同じものを数える（`stopped-count` は互換のため残す）。

### attention-count（kpi、P1）

| キー | 型 | 値 | 既定 | 意味 |
|---|---|---|---|---|
| `scope` | select | `reviews` | ✓ | 自分のレビュー待ち（`my-reviews?kind=review` の件数）。差は `warn` |
| | | `stopped` | | 自動マージが止まった自分の変更。差は `stop` |
| | | `spec` | | 承認待ちの仕様書。差は `warn` |
| | | `failed` | | `main` の最新のパイプラインが失敗しているプロジェクトの数（単位は「件 / projects」）。差は `stop` |
| | | `deploy` | | 自分が今承認できる配備（`deploy-approvals` の件数、すべての環境。P2b）。単位は「件 / deployments」。差は `warn` |
| `project` | multi（`options_from: options/project`） | プロジェクトの id（繰り返す、最大 24） | なし | 数えるプロジェクトを絞る。見られないプロジェクトは数えない |

`value` = 件数、`reference` = 標準 0。件数は 100 で頭打ち（`100+`）。`failed` を `project` なしで数えるときは自分がロールを持つ
プロジェクト（最大 200）から失敗しているものを 24 件まで数える（24 件あれば `n+` と出す）。カードの `href`: `reviews` / `spec` → `/-/merge-requests?scope=approvable`、
`stopped` → `/-/merge-requests`、`failed` / `deploy` → `/-/projects?tab=member`。`deploy` は待っている配備を 100 件まで読み、
それより多ければ `n+` と出す。

### inbox（kpi、P1）

設定は `locale` / `tz` だけ。`value` = 未読の通知の数（GitStation の画面で今見られるものだけ。管理者の権限は使わない。100 で頭打ち）。
`reference` / `delta` は無い。カードの `href` は `/-/notifications?unread=1`。

### menu（links、P1）— 用途別メニュー

| キー | 型 | 値 | 既定 | 意味 |
|---|---|---|---|---|
| `purpose` | select | `developer` / `reviewer` / `spec` / `operations` / `manager` | `developer` | だれのためのリンクか |
| `project` | select（`options_from: options/project`） | プロジェクトの id | なし | プロジェクトの画面（ビルドとテスト・マージ待ちの列・仕様書・承認待ちの変更）を開くプロジェクト |

| 用途 | リンク（上から。`{p}` は選んだプロジェクトのパス） |
|---|---|
| 開発 | 自分が出した変更 `/-/merge-requests?scope=authored`・対応が必要な変更 `/-/merge-requests?scope=attention`・ビルドとテスト `/{p}/-/pipelines`・マージ待ちの列 `/{p}/-/merge-queue`・通知 `/-/notifications?unread=1`・プロジェクト `/-/projects?tab=member` |
| レビュー | 自分の承認待ち `/-/merge-requests?scope=approvable`・対応が必要な変更・通知・プロジェクト |
| 仕様 | 仕様書 `/{p}/-/documents`・承認待ちの変更 `/{p}/-/pulls?state=open`・自分の承認待ち・通知 |
| 運用 | ビルドとテスト・マージ待ちの列・通知・プロジェクト |
| 管理 | 対応が必要な変更・自分の承認待ち・プロジェクト `/-/projects`・グループ `/-/groups`・最近の動き `/-/activity` |

- プロジェクトを選んでいない、または選んだプロジェクトを本人が見られないときは、プロジェクトの画面のリンクを 1 つの
  「プロジェクトを選ぶ」（`/-/projects?tab=member`）にまとめる（同じ行き先のリンクは 2 度出さない）。見られないことは答えに出ない。
- 設計 §4 の「新しい仕様を書く `/{p}/-/new`」は SPA に該当する画面が無い（`new` はブランチとパスが要る）ので入れない。
  運用の「配備」は P2（配備の画面ができてから）。
- すべての `href` が SPA の経路（`web/src/App.tsx`、`RepoViews.tsx`）にあり、問い合わせのキーと値を画面が読むことはテスト
  （forge-web `every_menu_href_is_a_screen_of_the_spa`）で確かめる。
- カードの題は「GitStation（開発）」の形、カードの `href` は `/`。
- **管理者の設定（2026-11-24、ADR-0021 の追記）**: 本人が GitStation の管理者なら、どの用途でも最後に「管理者の設定」
  `/-/admin`（SPA が本人の `/{username}/-/settings?tab=admin` へ送る）を足す。決めるのは `authorize()`
  （`forge_auth::portal_admin_menu`: その人をアカウントの管理者の旗を持つセッションとして `UserAdminList`）。旗は DB の
  `users.is_admin`（`auth.oidc.admin_role` のロールでサインインのたびに付け外しされる）で、ポータルのトークンの claim では
  決めない。リンクは何の権利も与えない（画面は GitStation のセッションを求める）。

### merge-queue（status、P2a）

| キー | 型 | 値 | 既定 | 意味 |
|---|---|---|---|---|
| `project` | multi（`options_from: options/project`） | プロジェクトの id（繰り返す、最大 24） | なし = 自分がロールを持つプロジェクト | マージ待ちの列を出すプロジェクト |

- 項目はマージ待ちの列ごと（プロジェクトとブランチの組。`main` を先に）。列の無いプロジェクトは出さない（列の無いプロジェクトが
  多くても、列のあるプロジェクトが 24 件の上限から押し出されないように、24 件そろうまで先を読む）。列が 1 つも無ければ空。
- ランプ（上ほど優先）: 列が自分で外した変更がある → `stop`（「外された変更 n 件」。外されたあと、もう一度列に入れるか閉じる・
  マージするまで数える。人が外したものは数えない。`my-reviews?kind=queue` と同じ）、一時停止中 → `warn`（待ちがあれば
  「一時停止中（n 件待ち）」）、待ちがある → `info`（「n 件待ち」）、それ以外 → `ok`（検査中の束があれば「動作中」、無ければ「待ちなし」）。
- 件数は 100 で読むのをやめる（`100+`）。
- `label` はプロジェクトのパス（`main` 以外の列は `<path> (<ブランチ>)`）、`key` は `<プロジェクトの id>:<ブランチ>`、
  `href` は `/<path>/-/merge-queue?target=<ブランチ>`。見られないプロジェクトを指定しても出ない（無いのと同じ）。

### spec-status（status、P2a）

| キー | 型 | 値 | 既定 | 意味 |
|---|---|---|---|---|
| `project` | multi（`options_from: options/project`） | プロジェクトの id（繰り返す、最大 24） | なし = 自分がロールを持ち、仕様の版を公開したことのあるプロジェクト | 出す仕様のプロジェクト |
| `show` | select | `approval` | ✓ | 承認待ちの変更: 開いていて下書きでない変更の数（新しい順に 100 件まで見る） |
| | | `undecided` | | 未決定の項目: その変更の現在の版の `forge/doc-lint` が指摘した「必須の決定がまだ open」（規則 `decisions_resolved`）の場所（ファイルと行）の数。同じ場所は 1 度だけ数える |
| | | `lint` | | 検査の指摘: その変更のうち、現在の版の `forge/spec-lint` か `forge/doc-lint` が失敗（`failure`）したものの数 |

- 仕様のプロジェクト: ADR-0023 によりリポジトリに種類は無いので、`project` を指定しないときは「仕様の版を公開したことがある」
  （`spec.released`、`my-reviews?kind=spec` と同じ）プロジェクトを出す。まだ公開していない仕様のリポジトリは `project` で選べば出る
  （選択肢は他のウィジェットと同じ、ロールを持ち読めるすべてのプロジェクト）。
- ランプ: `approval` 待ちあり → `warn`（「承認待ち n 件」）、無し → `ok`。`undecided` 未決定あり → `warn`（「未決定 n 件」）。
  `lint` 失敗あり → `stop`（「検査の指摘 n 件」）。
- 検査の結果がまだ無い（その版の `forge/spec-lint` と `forge/doc-lint` のどちらかがまだ終わっていない、または `error` で答えた）変更が
  あるときは、`undecided` / `lint` を「なし」とは出さず `unknown`（「確認中」）にする。`forge/doc-lint` の 1 回の指摘は 50 件までしか
  残らないので、50 件に達した版があれば件数を `n+` とする（0 件なら「確認中」）。
- 件数は 100 で読むのをやめる（`100+`）。
- `href`: `approval` / `lint` → 承認待ちの変更 `/<path>/-/pulls?state=open`、`undecided` → 仕様書 `/<path>/-/documents`。
  言葉は「MR」と言わず「変更」（plan 08）。見られないプロジェクトは指定しても出ない。

### deploy-approvals（list、P2b）

| キー | 型 | 値 | 既定 | 意味 |
|---|---|---|---|---|
| `env` | multi（固定の選択肢） | `dev` / `stg` / `prod`（繰り返す） | なし = すべての環境 | 出す環境 |
| `limit` | number | 3〜10 | 5 | 件数。`total` は自分が今承認できる件数（本人が要求したもの・もう承認したものを除き、待っている配備を古い順に 100 件まで読む） |

- 出すのは、承認待ち（`pending_approval`）で承認の期限（環境の `approval_ttl_minutes`）の内にある配備のうち、**本人が今
  承認できるもの**。決めるのは承認の経路（`POST …/deployments/{id}/approve`）と同じ判断で、ウィジェットだけの規則は無い
  （ADR-0038 の段 C）: 環境の承認者（名前を挙げた人・グループ・組織のセレクタ）を本人について読み、`authorize()` の
  `DeployApprove` を、管理者の権限なしの GitStation のセッションとして試す。承認者を決めていない環境は Maintainer 以上、
  名前を挙げた人は Reporter 以上、グループ・セレクタで当たった人は Maintainer 以上。要求した人と、その SHA の作者・push した人は
  出ない。
- 代理（ADR-0038 §3.5）: 本人に今有効な代理の依頼があり、環境が代理を許すときは、依頼した人の代わりに承認できる配備も出す
  （`sub` に「代理で承認」）。依頼した人も同じ判断（要求した人・作者でないこと、ロール）を通る。代理を試すのは依頼した人 10 人まで。
- 本人がもう承認した配備（だれの代わりでも）と、承認がもう数えられている人の代わりの承認は出さない（経路は
  `FG-DEPLOY-0010 already_approved` で断るため）。
- 項目: `id`（`<path>#<配備の番号>`）、`label`（`<path> → <環境>`）、`sub`（短い SHA・待ち時間（分 → 時間 → 2 日から日）、
  代理なら「代理で承認」、凍結中なら「凍結中（<終わり> UTC まで）」）、`badge`（凍結中 → `stop`「凍結中」、ほか → `warn`
  「承認待ち」）、`href`（プロジェクト `/<path>`。SPA に配備の画面ができたらそちらに替える）。古い順（待たせている順）。
- 凍結中: 環境の凍結期間が今にかかっているとき。凍結中でも承認はできる（実行は凍結が明けてから。承認の期限が先に切れると
  実行されない）。
- 出さないもの: コミットの題・作者の名前やメール、承認した人の名前。カードの `href` は無い。
- 見られないプロジェクトの配備は、件数にも入らない（プロジェクトを先に `PortalWidgetRead` で決める）。

### 選択肢

`GET /api/portal-widgets/v1/my-reviews/options/project`、`…/pipelines/options/project`、`…/attention-count/options/project`、
`…/menu/options/project`、`…/merge-queue/options/project`、`…/spec-status/options/project`: 本人がロールを持ち読めるプロジェクト
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
- 1 つのウィジェットの読み取りは 2.5 秒で打ち切る（`server.lookup_timeout` がそれより長くても。ポータルの上限は 3 秒）。
  件数だけのウィジェット（`stopped-count`、`attention-count`）は表示しない事実（衝突・検査）を読まない。
- `merge-queue` / `spec-status` はプロジェクトごとに、本人が読めるか（`PortalWidgetRead`、管理者の権限なしのセッションと同じ）を
  先に決めてから、そのプロジェクトの列・変更・検査を読む（どれもプロジェクトを読める人が画面で見られるもの）。
- `deploy-approvals` / `attention-count?scope=deploy` も同じくプロジェクトを先に決め、配備ごとに承認の経路と同じ
  `DeployApprove` を本人（管理者の権限なしのセッション）について試す。ポータルのトークンが `DeployApprove` を持つことは無い
  （ポータルのトークンは `PortalWidgetRead` だけ。承認は GitStation の画面か `gstn` で、本人のセッションで行う）。

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

## 台帳の写し（PFS チームへの引き渡し用）

PFS の `catalog/apps/gitstation.yaml` の `widgets:` に足す P1 の分（PFS 側の MR で入れる。このリポジトリから PFS は変えない）。
既存の `my-reviews` / `pipelines` は設定を足すだけ。台帳の文字列は 1 行で、`$(`・`${`・`\` を含めない（台帳の検査）。

大きさ（2026-11-23）: すべてのウィジェットの `sizes` / `default_size` を上の表のセル方式に替える。既存の 3 つ
（`my-reviews` / `pipelines` / `stopped-count`）も大きさだけ替える。

```yaml
  # my-reviews: kind に 3 つ、sort を足す（既存の kind / project / limit はそのまま）。大きさを替える
  - id: my-reviews
    sizes: [1x2, 2x2, 4x2]
    default_size: 2x2
    settings:
      - key: kind
        type: select
        label: { ja: 表示するもの, en: Show, id: Tampilkan }
        options:
          - { value: review,    label: { ja: 自分のレビュー待ち, en: Waiting for my review, id: Menunggu tinjauan saya } }
          - { value: mine,      label: { ja: 自分が出した変更, en: My open changes, id: Perubahan saya yang terbuka } }
          - { value: stopped,   label: { ja: 止まった自動マージ, en: Stopped auto-merges, id: Auto-merge yang berhenti } }
          - { value: spec,      label: { ja: 承認待ちの仕様書, en: Specs awaiting my approval, id: Spesifikasi menunggu persetujuan saya } }
          - { value: threads,   label: { ja: 返事が来た自分のコメント, en: Replies to my comments, id: Balasan untuk komentar saya } }
          - { value: conflicts, label: { ja: 衝突している自分の変更, en: My changes with conflicts, id: Perubahan saya yang berkonflik } }
          - { value: queue,     label: { ja: マージ待ちの列にある自分の変更, en: My changes in the merge queue, id: Perubahan saya di antrean merge } }
        default: review
      - key: sort
        type: select
        label: { ja: 並べ方, en: Order, id: Urutan }
        options:
          - { value: oldest, label: { ja: 古い順, en: Oldest first, id: Terlama dulu } }
          - { value: newest, label: { ja: 新しい順, en: Newest first, id: Terbaru dulu } }
        default: oldest

  # pipelines: only_problems を足す。大きさを替える
  - id: pipelines
    sizes: [2x1, 2x2, 4x2]
    default_size: 2x2
    settings:
      - key: only_problems
        type: toggle
        label: { ja: 問題のあるものだけ, en: Problems only, id: Hanya yang bermasalah }
        default: false

  # stopped-count: 大きさだけ替える
  - id: stopped-count
    sizes: [1x1, 2x1]
    default_size: 1x1

  - id: attention-count
    type: kpi
    name:        { ja: 対応が必要な件数, en: Needs my attention, id: Perlu perhatian saya }
    description: { ja: "レビュー待ち・止まった変更・承認待ちの仕様書・失敗しているプロジェクト・自分が承認できる配備のどれかを数えます。0 が標準です。", en: "Counts reviews waiting for you, stopped changes, specs awaiting approval, failing projects or deployments you can approve. The standard is 0.", id: "Menghitung tinjauan yang menunggu Anda, perubahan yang berhenti, spesifikasi yang menunggu persetujuan, proyek yang gagal atau deployment yang dapat Anda setujui. Standarnya 0." }
    sizes: [1x1, 2x1]
    default_size: 1x1
    endpoint: /api/portal-widgets/v1/attention-count
    refresh_sec: 120
    required_role: access
    settings:
      - key: scope
        type: select
        label: { ja: 数えるもの, en: Count, id: Hitung }
        options:
          - { value: reviews, label: { ja: 自分のレビュー待ち, en: Waiting for my review, id: Menunggu tinjauan saya } }
          - { value: stopped, label: { ja: 止まっている自分の変更, en: My stopped changes, id: Perubahan saya yang berhenti } }
          - { value: spec,    label: { ja: 承認待ちの仕様書, en: Specs awaiting my approval, id: Spesifikasi menunggu persetujuan saya } }
          - { value: failed,  label: { ja: 失敗しているプロジェクト, en: Failing projects, id: Proyek yang gagal } }
          - { value: deploy,  label: { ja: 自分が承認できる配備, en: Deployments I can approve, id: Deployment yang dapat saya setujui } }
        default: reviews
      - key: project
        type: multi
        label: { ja: プロジェクト, en: Projects, id: Proyek }
        options_from: options/project
        default: null

  - id: inbox
    type: kpi
    name:        { ja: GitStation の未読の通知, en: GitStation unread notifications, id: Notifikasi GitStation belum dibaca }
    description: { ja: "GitStation の通知のうち、まだ読んでいないものの数です。", en: "Number of GitStation notifications you have not read yet.", id: "Jumlah notifikasi GitStation yang belum Anda baca." }
    sizes: [1x1]
    default_size: 1x1
    endpoint: /api/portal-widgets/v1/inbox
    refresh_sec: 60
    required_role: access

  - id: merge-queue
    type: status
    name:        { ja: マージ待ちの列, en: Merge queues, id: Antrean merge }
    description: { ja: "プロジェクトごとのマージ待ちの列です。列から外された変更・一時停止・待ちの数を色で示します。", en: "The merge queue of each project: changes the queue removed, a paused queue, and how many wait.", id: "Antrean merge tiap proyek: perubahan yang dikeluarkan antrean, antrean yang dijeda, dan jumlah yang menunggu." }
    sizes: [1x2, 2x2, 4x2]
    default_size: 2x2
    endpoint: /api/portal-widgets/v1/merge-queue
    refresh_sec: 60
    required_role: access
    settings:
      - key: project
        type: multi
        label: { ja: プロジェクト, en: Projects, id: Proyek }
        options_from: options/project
        default: null

  - id: spec-status
    type: status
    name:        { ja: 仕様の状況, en: Specification status, id: Status spesifikasi }
    description: { ja: "仕様のプロジェクトごとに、承認待ちの変更・未決定の項目・検査の指摘のどれかを示します。", en: "For each specification project: changes awaiting approval, undecided items or check findings.", id: "Untuk tiap proyek spesifikasi: perubahan yang menunggu persetujuan, butir yang belum diputuskan, atau temuan pemeriksaan." }
    sizes: [1x2, 2x2, 4x2]
    default_size: 2x2
    endpoint: /api/portal-widgets/v1/spec-status
    refresh_sec: 300
    required_role: access
    settings:
      - key: project
        type: multi
        label: { ja: 仕様のプロジェクト, en: Specification projects, id: Proyek spesifikasi }
        options_from: options/project
        default: null
      - key: show
        type: select
        label: { ja: 表示するもの, en: Show, id: Tampilkan }
        options:
          - { value: approval,  label: { ja: 承認待ちの変更, en: Changes awaiting approval, id: Perubahan menunggu persetujuan } }
          - { value: undecided, label: { ja: 未決定の項目, en: Undecided items, id: Butir yang belum diputuskan } }
          - { value: lint,      label: { ja: 検査の指摘, en: Check findings, id: Temuan pemeriksaan } }
        default: approval

  - id: deploy-approvals
    type: list
    name:        { ja: 承認待ちの配備, en: Deployments awaiting my approval, id: Deployment menunggu persetujuan saya }
    description: { ja: "自分が今承認できる配備の一覧です。凍結中の環境はそう示します。承認は GitStation の画面で行います。", en: "Deployments you can approve now; a frozen environment is shown. Approve them on GitStation.", id: "Deployment yang dapat Anda setujui sekarang; lingkungan yang dibekukan ditandai. Setujui di GitStation." }
    sizes: [1x2, 2x2, 4x2]
    default_size: 2x2
    endpoint: /api/portal-widgets/v1/deploy-approvals
    refresh_sec: 60
    required_role: access
    settings:
      - key: env
        type: multi
        label: { ja: 環境, en: Environments, id: Lingkungan }
        options:
          - { value: prod, label: { ja: 本番, en: Production, id: Produksi } }
          - { value: stg,  label: { ja: ステージング, en: Staging, id: Staging } }
          - { value: dev,  label: { ja: 開発, en: Development, id: Pengembangan } }
        default: null
      - key: limit
        type: number
        label: { ja: 件数, en: Items, id: Jumlah }
        min: 3
        max: 10
        default: 5

  - id: menu
    type: links
    name:        { ja: GitStation のメニュー, en: GitStation menu, id: Menu GitStation }
    description: { ja: "開発・レビュー・仕様・運用・管理の用途ごとに、よく使う GitStation の画面へのリンクを並べます。", en: "Links to the GitStation screens each role uses most: development, review, specifications, operations or management.", id: "Tautan ke layar GitStation yang paling sering dipakai per peran: pengembangan, tinjauan, spesifikasi, operasional, atau manajemen." }
    sizes: [1x2, 2x2]
    default_size: 1x2
    endpoint: /api/portal-widgets/v1/menu
    refresh_sec: 3600
    required_role: access
    settings:
      - key: purpose
        type: select
        label: { ja: 用途, en: Purpose, id: Keperluan }
        options:
          - { value: developer,  label: { ja: 開発, en: Development, id: Pengembangan } }
          - { value: reviewer,   label: { ja: レビュー, en: Review, id: Tinjauan } }
          - { value: spec,       label: { ja: 仕様, en: Specifications, id: Spesifikasi } }
          - { value: operations, label: { ja: 運用, en: Operations, id: Operasional } }
          - { value: manager,    label: { ja: 管理, en: Management, id: Manajemen } }
        default: developer
      - key: project
        type: select
        label: { ja: プロジェクト, en: Project, id: Proyek }
        options_from: options/project
        default: null
```

`merge-queue` と `spec-status` は P2a で足した（PFS 側の MR で上の 2 つを入れる）。P2b で `deploy-approvals` を足し、
`attention-count` の `scope` に `deploy` を足した（PFS 側の MR で入れる）。`env` は環境が 3 つに決まっているので
`options_from` ではなく固定の `options`（設計 §3.5 との違い）。`limit` の形は `my-reviews` の既存の宣言に合わせる。
`widget_status` は P1 の受け入れ後に `pilot`。ボード（`catalog/boards/`）の更新は設計 §5 のとおり（`gitstation-operations` の
`attention-count S（deploy）` と `deploy-approvals M` は P2b で使えるようになった）。
