# ADR-0036: PFS ポータルのホームのウィジェットと新着（ポータルの資格情報、ニュースの outbox）

**Date**: 2026-11-17
**Status**: proposed（accepted は `feat/web-portal-widgets` の `security-reviewer`・`authz-matrix-auditor` のレビューで CRITICAL / HIGH が無いとき）
**Deciders**: Masaki Fujikawa（ユーザー、2026-11-16 PFS のポータル設計の決定 PW-1〜PW-7、2026-11-17 の依頼）、Claude

番号について: `main` と開いている枝に 0036 が無いことを確かめた。

## Context

PFS のポータル（iotpf-portal、Keycloak のレルム `iotpf`）に、利用者ごとのホームができる。各アプリは「決まった形の JSON」を返す
ウィジェットを宣言し（契約 v1、IoT_platform_spec `10-pfs/14-portal-home-widgets.md`）、ポータルは **サーバー側から利用者本人の
アクセストークン（`azp = portal`）** を付けて呼ぶ。新着はアプリが **ポータルへ送る**（client credentials のトークン、outbox、
指数バックオフ、`source_id` で冪等）。GitStation の宣言（`my-reviews` / `pipelines` / `stopped-count`、`news: true`）は PFS と合意済み。

制約: 認可の判断点は `authorize()` 一つ（契約 4）。ポータルのトークンはウィジェットの経路だけで受け、ほかの経路は今までどおり拒む。
リンクはサインインの規則（ADR-0021 と 2026-11-17 の補足: `staff_id` だけで、大文字小文字を無視）に従い、ウィジェットの呼び出しで
利用者を作らない。新着には生のデータも、社員番号以外の個人の情報も入れない。

## Decision

1. **新しい資格情報 `Credential::Portal` と読み取りだけの行為 `PortalWidgetRead`。**
   - forge-web の認証は、行為が `PortalWidgetRead` の経路（`/api/portal-widgets/v1/**`）でだけ、`Authorization: Bearer` をポータルの
     認証器（forge-oidc `PortalWidgets`、forge-core の trait `PortalAuthenticator` 越し）に渡す。Cookie と Basic は見ない
     （`FG-AUTH-0026 reason=bearer_required`）。ほかの経路では JWT は個人のアクセストークンでないので、トークンの認証器が今までどおり
     拒む（`FG-AUTH-0001`、テスト `a_portal_token_is_refused_on_every_other_route`）。
   - 検証はサインインのアクセストークンと同じ（JWKS とアルゴリズム、`iss`、`exp` / `nbf` / `iat`、`typ`、`sub`）。`azp` は
     **forge-oidc が `portal.client_id`（既定 `portal`）と比べる**（レビュー S2）: ほかのクライアントのトークン（gitstation 自身のものを
     含む）は `FG-AUTH-0026 reason=azp` で、**DB を読む前に** 落ちる（テスト `another_clients_token_never_reaches_the_user_lookup`）。
     `resource_access.<auth.oidc.client_id>.roles` に `auth.oidc.access_role` があるかは **事実として**
     `PortalCredential { portal_party, access_role }` に載せ、`authorize()` のポータルの腕（`forge-auth/src/portal.rs`）が判断する
     （`portal_wrong_party` / `portal_no_access_role`、どちらも 403）。`portal_party` の行は多層の守りとして残す。
   - 人はサインインの規則で **読み取りだけ** で探す（`forge_db::portal::portal_user`）: `(iss, sub)` のリンク → そうでなければ社員番号
     （形が正しい、ちょうど 1 人、`auto_link` が on、どの発行者の identity も持たない、管理者でない、アカウントの種類が
     `auth.oidc.link_auth_sources` にある — レビュー S1、サインインと同じ順）。`auth.oidc.claims_source = id_token` のときサインインは
     確認済みのメールでリンクするが、アクセストークンはそれを示さないので、リンク済みの identity だけを受ける（`sign_in_required`）。
     見つからなければ `FG-AUTH-0027`（403）。リンクも作成もしない。
   - **門（レビュー S3）**: ウィジェットの経路は、Bearer が 1 つあることを見たあと、トークンを検証して人を探す **前に** 門
     （`routes::portal_widgets::WidgetGate`）を通る。呼ぶのはポータルのバックエンド 1 つなので、人ごとではなく経路全体で数える:
     同時に 16 件（`FG-WEB-0010 route=portal_widgets reason=in_flight`、`Retry-After: 1`）、1 分に 6000 件
     （`route=portal_widgets`、`Retry-After` は窓の残り）。6000 件/分は、3 つのウィジェットが 60 秒ごとに更新するホームを 2000 人が
     開いている量。既存の `InFlight` と `CallerLimiter` を使う。失敗の応答は `Cache-Control: no-store`（レビュー S4）。
   - ポータルの腕は閉じている: `PortalWidgetRead` だけ、インスタンス（ウィジェットの経路そのもの）とプロジェクトの上だけ。プロジェクトは
     **管理者の権限のないセッションの本人** として `ProjectRead` で決める（Web の画面と同じ規則）。表に出す MR は同じ本人
     （`forge_auth::portal_reader`、ウィジェットの経路の証明からだけ作れる）で `MergeRequestRead` と承認の試し判定をする。
     権限の表に許可・拒否の行（ロールなし、別の `azp`、無効の利用者、サービスアカウント、私的なプロジェクト、削除待ち、
     セッション・トークン・CI・ボットの拒否、ほかのすべての行為の拒否）を足した。
2. **ウィジェットのデータは既存の一覧の仕組みの上に作る。** SQL は forge-db だけ（候補だけ、判断はしない）、判断は forge-review
   （`portal_merge_requests`）と forge-pipelines（`portal_projects` / `portal_lamps`）。
   - `my-reviews`: `review` = 承認できて未承認（「自分の仕事」の `approvable`）、`mine` = 自分の開いた MR、`stopped` = 現在の
     head で自動マージが止まった MR のうち自分が作者か承認者、`spec` = 仕様を公開したことのあるプロジェクト（監査に `spec.released`）の
     `approvable`（ADR-0023: 仕様は種類でなく中身で決まる）。件数は見える行だけ、100 で頭打ち。
   - `pipelines`: `refs/heads/main` の最新のパイプライン、または最新の配備をランプで。最大 24。設定が無いときの候補は id だけを
     取り（1 文、最大 200）、プロジェクトの記録は 24 件ずつ、24 個のランプが揃うまでだけ読む（レビュー D2。ふつうは 1 回）。
   - `stopped-count`: 止まった自動マージの数を標準 0 と比べる。
   - 選択肢 `options/project`: 本人がロールを持ち読めるプロジェクト、最大 200。
   - 不正な値は 422 `FG-WEB-0011`、知らないキーは無視。失敗の本文は契約の `{success: false, error: {code, message, request_id}}`。
3. **新着はニュースの outbox `forge.portal_news` に、業務の変更と同じトランザクションで書く。**
   配備の終わり（`deploy::worker::finish`、`abandon_stale`）、自動マージの停止（`record_state`、状態が `stopped` に変わったとき）、
   仕様の公開（`spec_releases::record_release`）がそれぞれのトランザクションの中で `portal::stage` を呼ぶ。行と
   `portal.news_send` のジョブが一緒に書かれ、`source_id` が同じなら何も足さない。
   - 送り手は forge-integ `NewsSender`（`forge worker` の専用のジョブランナー）。送る直前に宛先を決め直す（有効な人、社員番号あり、
     プロジェクトを読める）。202 で `sent`、400 / 404 / 409 / 413 / 422 は `failed`（`FG-INT-0023`、alert）、ほかは 1 回の失敗
     （`FG-INT-0022`）としてキューのバックオフで再試行、`portal.max_attempts` の最後で `failed`。トークンは gitstation の client
     credentials（forge-oidc `ServiceTokens`、discovery の token endpoint、期限の 30 秒前まで使う）。取れなければ `FG-INT-0024`。
   - **`portal.enabled` / `portal.news_enabled` が off のあいだは outbox に何も書かない**（後から on にしても過去を流さない）。
   - **`pending` のまま残る行を作らない（レビュー H2）。** ジョブの最後の試行で DB が失敗したら、そのジョブは再試行でなく終わりにする
     （`Retry::Never`）。それでも結果を書けずにジョブが終わった行（DB の失敗、試行の上限でのリースの期限切れ）は、`forge worker` の
     1 時間ごとの掃除（`worker::portal_news::housekeep` → `forge_db::portal::sweep_orphaned`）が「`queued` / `running` のジョブを持たない
     `pending`」として見つけ、`failed`（`reason=tries_used_up`）にして、行ごとに 1 回 `FG-INT-0023`（`detail=job_ended`、行を作った変更の
     request_id）を記録する。同じ掃除が、終わった行の 30 日の削除を、1 回分（500 行）より少なくなるまで繰り返す（最大 20 回）。
4. **「レビューを頼まれた」と「承認待ちの仕様」は新着にしない（契約の穴、下の Consequences）。**

## Alternatives Considered

### ドメインイベント（`deployment.finished` など）を足して購読者でニュースを作る
- **Pros**: 既存の outbox（`forge.events`）と購読者の仕組みそのまま。
- **Cons**: 新しいイベントの種類は可視性の分類、統合のフィード、Webhook、10 余りの網羅の match に波及する。自動マージの状態は今は
  監査も持たない記録で、イベントにするには変更の形から変わる。
- **Why not**: 依頼の範囲（ポータルの新着）に対して影響が大きい。業務のトランザクションに outbox の行を書く方が「同じトランザクションで」を
  そのまま満たす。イベントが欲しくなったら、そのときに足す。

### ポータルの資格情報を個人のアクセストークン（`read_api`）として扱う
- **Pros**: 新しい資格情報が要らない。
- **Cons**: 本人のトークンでできることがすべてポータルにもできる。`authorize()` で「ウィジェットだけ」を表せない。
- **Why not**: 契約 4（判断点は一つ、表の行）と最小の権限に反する。

### off のときも outbox に書き、送り手が捨てる
- **Pros**: 業務の側に分岐が無い。
- **Cons**: 使わない行が溜まり、on にした瞬間に過去を送る。
- **Why not**: 「書かない」を選んだ（`Db::with_portal_news(None)` が既定）。

## Consequences

### Positive
- ポータルからの呼び出しは表の行で説明できる。ほかの経路は何も変わらない。
- 新着は業務の変更と一緒にしか生まれず、落ちても再送で埋まる。失敗は行の状態とコードで追える（`forge doctor` の `portal.news`）。

### Negative
- 契約の穴（PFS に報告する）:
  - GitStation に「レビュアーの指名」は無い。`review` は「承認できて未承認」で代える。新着「レビューを頼まれた」も無い。
  - 「承認待ちの仕様」の出来事が無い。ウィジェットの `spec` は `spec.released` のあるプロジェクトの承認待ちで代える。新着は出さない。
  - `tz` は形だけ確かめ、時刻は UTC（タイムゾーン付き）で返す（GitStation はタイムゾーンの表を持たない）。
  - `pipelines` は既定のブランチでなく `refs/heads/main` を見る（既定のブランチは git の HEAD で、DB に無い）。
  - トークンの `plants` では絞らない（GitStation のデータに工場の区別が無い）。
- 自動マージの状態の記録が、ニュースが on のとき短いトランザクションになる。

- 門は経路全体で 1 つなので、不正なトークンの洪水はポータルのウィジェットも 429 にする（GitStation の DB と IdP は守られ、
  ポータルは前の答えを出し続ける）。呼び出し元の区別が要るようになったら、プロキシの配置（ADR-0019 の未決）と一緒に決める。
- 索引（移行 `20261228100000`）: `audit_log (project_id) WHERE action = 'spec.released'`（`spec` の一覧の EXISTS、レビュー H1）と
  `ci_pipelines (project_id, ref, id DESC)`（最新のパイプライン、レビュー D1）。`CONCURRENTLY` でないので、作るあいだ両方の表への
  書き込みが待つ（移行 `20261111100100` と同じ扱い。静かな時間に当てる）。

### Follow-ups（2026-11-17 のレビューで決めた、この枝ではやらないこと）
- **新着の送信に専用のクライアントと audience を絞ったトークン（レビュー S5）。** 今は gitstation のクライアントの client credentials を
  そのまま使う。ニュース専用のクライアント（または `aud` をポータルのニュースの受け口に絞るマッパー）にすると、漏れたトークンで
  できることが新着の送信だけになる。PFS 側のレルムの変更（クライアントの追加、`NEWS_SENDER_CLIENTS`）が要るので、PFS の MR と
  一緒に行う。この枝ではコードを変えない。
- **`my-reviews` / `stopped-count` の人ごとの短いキャッシュ（レビュー D3）は入れなかった。** 理由: (1) 停止や承認の直後に古い数が
  30 秒残ると「次に何をするか」がずれる。応答は既に `Cache-Control: private, max-age=30` で、ポータル側が持てる。(2) 鍵に人と
  すべての設定を入れた表をプロセスに持つと、権限の変更（メンバーから外す）を 30 秒遅らせる経路が 1 つ増える。
  1 回の呼び出しの文の数（コードから数えた。計測ではない）: 候補のページ 1 文（100 行ごと、最大 10 ページ = 1000 候補）＋
  候補に出てくるプロジェクトごとに 1 文（`load_project`、呼び出しの中でキャッシュ）＋ 失敗したチェックのある行を持つプロジェクトごとに
  1 文（保護の規則、同じくキャッシュ）。`decide()` は **行ごとの文を出さない**（上の 2 つのキャッシュだけ）。ふつうの人
  （候補 100 未満、プロジェクト P 個）で 1 + P 〜 1 + 2P 文。権限の判断は SQL の LIMIT のあとなので、表示の行は見える行が `limit` に
  届くまでページをまたいで埋まり、短くなるのは候補が 1000 を超えるときだけ（そのとき `total` は `truncated`）。
  門（S3）が同時 16 件に抑えるので、まず計測（`http.request` の `duration_ms`）を見てから決める。

### Risks
- 検証の順: `azp` は署名を確かめた検証の中で `portal.client_id` と比べる（検証の前に本文を読むことはやめた）。
- 宛先が 200 人を超えるプロジェクトの仕様の公開は先頭の 200 人だけ（行の CHECK）。必要なら `role` の宛先を後で検討する。
