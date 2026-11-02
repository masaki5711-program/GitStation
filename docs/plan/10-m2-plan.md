# 10 M2 の計画 — 画面、~~文書ポータル（最小）~~、Keycloak のログイン（W17）

> **2026-11-01 利用者の決定で変更**（ADR-0023）:
> - **文書ポータル（§4.7、MR12）をやめた。** MR12 は実装して統合済み（`f95e2e8` に含まれる）だが、決定により **取り下げ（superseded）**。コードの削除は後続の MR で行う（この計画の変更は文書だけ）。画面の系統はリポジトリの画面 1 つ。
> - **リポジトリの種類 `spec` / `system` の区別をやめた。** `kind` の選択、`?kind=` の絞り込み、ホームの「仕様書（`kind=spec`）」は取り除く対象（後続のコードの MR）。
> - **M2 の完了の条件**（§9）から「文書ポータルの使いやすさの確かめ」を外した。
> - 取り消した記述は ~~打ち消し線~~ で残す。

作成: 2026-11-01（クラウドのセッション、planner）。状態: **承認済み（2026-11-01、案 A と §7 の推奨の答えで進める）**。
参照: 00 §4・§6、01 §3.2・§3.7・§7・§8、08、09 §8、ADR-0015、ADR-0019、引き継ぎ `docs/handoff/2026-11-01-phase9.md`。

## 0. 前提の確認（計画の土台）

- システムの分類: **none**（社内の Git の基盤。IoTPF のデータは扱わない。IoT プラットフォームとは Keycloak の realm を共有するだけ）。推定なので確認してほしい。
- 目標: ブラウザだけで、ポータルから 1 回のログインで入り、プロジェクトを探し、コードと仕様書を読み、メンバーと設定を管理できるようにする。~~Git を知らない人が、確定した版の仕様書を Git の言葉なしで読めるようにする。~~（2026-11-01 取り下げ。Git の基本を学んだ人がリポジトリの画面で読む）
- 計画書どうしの食い違いと、この計画の扱い:
  - 01 §3.2・§8 は「M2 はローカル + LDAP、OIDC は M8」。**00 §6 と 09 W17 の決定（Keycloak、M2）を正とする。** Keycloak が AD をまとめるので、GitStation は LDAP に直接つながない。users の表にはパスワードの列がないので、パスワードのログインは作らない。緊急時の入口は `gstn` とトークン。**01 の §3.2 と §8 の書き換えを引き継ぎに書く。**
  - 08 は `docs/brand/` を参照しているが、ロゴとアイコンは `docs/overview/logo-light.png` と `icon.png` にある。`web/src/assets/brand/` に写し、元の場所を正とする。
- コードで確かめたこと:
  - `Credential::Session` は「すべてのスコープ」として既にある。
  - forge-web の `auth.rs` は、別の仕組みが置いたプリンシパルの上にヘッダーがあると `ambiguous` にする（セッションの席は用意されている）。
  - `forge.users` には `employee_number`・`external_id`・`auth_source`（`local`|`ldap`）がある。
  - forge-git には閲覧の操作（`ls-tree`、`cat-file`、`log`、diff）がない。
  - `TokenCreate`/`TokenList`/`TokenRevoke`・`UserDisable` は `app::PLANNED` にある。
  - ルートはどれも gstn の C-1 の表（`forge-client/src/coverage.rs`）に行が要る。
  - 401 にはすべて `WWW-Authenticate: Basic` が付く（ADR-0019）。**このままでは、ブラウザの fetch でパスワードの入力画面が出る。**
- 着手の条件: M1 は完了済み（phase 9 の MR0 `feat/web-connection-fairness`・MR4 `feat/web-smart-http-push`、D5 MR1〜MR5 を統合。2026-11-01 の全体のテスト 1347 件合格）。

## 1. M2 の範囲

| 入れる | 入れない（理由） |
|---|---|
| Keycloak の OIDC ログイン（認可コード + PKCE）、GitStation のサーバー側のセッション、バックチャネルのログアウト、Keycloak の失効の反映 | ローカルのパスワードと LDAP の直接の接続（Keycloak が AD を担う）。2 要素認証（Keycloak 側の設定） |
| SPA の土台（React + TS、forge-web に埋め込み）、CSP、i18n（ja/en/id）、ブランドのトークン | SSE・通知（M3）。ダークテーマ |
| 画面: ログイン、ホーム、グループとプロジェクトの一覧、リポジトリの閲覧（ファイル・コミット・差分・ブランチ・タグ・blame）、raw・archive | MR・レビュー・チェックの画面（M3）。パイプライン（M4） |
| プロジェクトとグループの設定、メンバー、保護 ref、公開範囲、削除と復元、移動と名前の変更、トークン、管理（利用者の一覧と無効化、セッションの失効） | 監査ログの画面（`AuditRead`）、メトリクス（`Metrics`）。この 2 つは PLANNED のまま |
| ~~文書ポータルの最小版（読み取り専用。§4.7）~~ → **取り下げ（2026-11-01）** | ~~アップロード・承認の流れ・LLM の要約・Web Studio（M3 の MR と M7 が前提）~~ |
| `gstn` の `browse`・`token`・`user`（01 §3.11 の M2 の行） | `gstn auth login` のデバイスフロー（後で） |

**W15（エージェントビルダーとマーケットプレイス）と W16（プレビューの環境）は M2 に入れない。**
- W15: 09 §4 が「M4 の後」と決めている。生成した変更は MR で入れる（M3 が前提）。部品の推薦はローカル LLM（M5）。
- W16: MR ごとのデプロイ（M4）が前提。
- M2 では、画面のナビに枠を作らない（08 原則 1）。決めるのは URL の予約だけ（`/-/agent`、`/-/preview` を `web_path` の予約語に足す）。
- ADR-0015 の「プレビュー用のオリジン」は **M2 では作らない** と決め、ADR-0015 の Amendment に書く。SVG・HTML・PDF は表示せず、ダウンロードだけにする。

## 2. 選択肢（いちばん影響の大きい「ブラウザの認証」）

| 軸 | A: 依頼どおり — GitStation のサーバー側のセッション（09 §8.2） | B: BFF が Keycloak のアクセストークンを持ち、API に Bearer で渡す | C: SPA が公開クライアントとしてトークンを持つ |
|---|---|---|---|
| 目標への適合 | ◎ 1 回のログイン、すぐの失効（バックチャネル） | ○ | △ XSS でトークンが盗まれる |
| 既存の資産 | `Credential::Session`、トークンの照合の部品（`forge-core` の token）を流用 | API がすべて JWT の検証を要する | 同左 |
| 失敗の見え方 | セッションは DB の行。理由はログの `reason` | JWT の期限と失効がずれる | 同左 |
| Keycloak の障害 | ログイン済みは使い続けられる（09 §8.2 の要件） | 更新に失敗すると全員が止まる | 同左 |
| CSRF | 対策が要る（Cookie） | 要る | 不要。ただし XSS に弱い |
| 判断 | **推奨** | 不採用 | 不採用（CSP があっても、トークンを JS に置かない） |

フロントの配信は、01 §7 の「埋め込み（単一バイナリ）」を採る。別の静的サーバー（nginx）は配布物が 2 つになり、殻の「どのパスでも同じバイト」（ADR-0015）を 2 か所で守ることになるので採らない。

## 3. 設計の決定（ADR-0021 と ADR-0022 にする）

### 3.1 フロントエンド（ADR-0022）

**ツールと置き場所**
- `web/`（`Git/web/`）に置く。Vite、React、TypeScript（strict）、React Router、TanStack Query、Radix（headless）、CSS Modules と CSS 変数のトークンを使う。
- CSS-in-JS（実行時に `<style>` を差し込むもの）は使わない。CSP の `style-src 'self'` を保つため。React の `style` は CSSOM 経由なので許される。
- API の型は `docs/api/openapi.json` から `openapi-typescript` と `openapi-fetch` で生成する（手書きしない）。生成物の差分は CI で確かめる。
- npm と `package-lock.json`、`npm ci --ignore-scripts`。CI で `npm audit` とライセンスを検査する。

**配信（forge-web に埋め込む）**
- `rust-embed` を feature `embed-ui` で使う。cargo は npm を起動しない。
- CI の web のジョブが `web/dist` を作り、release のビルドが埋め込む。feature があるのに `dist` がなければ、ビルドを失敗させる。
- feature なし（開発・nextest）では今の仮の殻を返す。`forge doctor` が `web.ui=placeholder` を warn で報告する（黙らない）。
- 殻（`WebShell`）は、埋め込んだ `index.html` を **どのパスにも同じバイトで** 返す（DB も資格情報も見ない。ADR-0015 を保つ）。
- 資産は新しいインスタンスの語 `/-/assets/{file}` で配る。Action は `WebShell`（公開）。
  - 埋め込みの一覧にないものは FG-WEB-0001。
  - `Cache-Control: public, max-age=31536000, immutable`。
  - 事前に圧縮した br / gzip を出す。
- 開発: `npm run dev` の Vite が `/api`・`/-/auth` を `forge serve` へ中継する（同じオリジンなので Cookie はそのまま）。

**CSP と安全のヘッダー**（1 つのミドルウェア `security_headers` で付ける）
- 殻:
  - `default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; font-src 'self'; connect-src 'self'`
  - `object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'`
- API: `default-src 'none'; frame-ancestors 'none'`。
- raw・画像・archive: ADR-0015 のとおり `sandbox; default-src 'none'`、`nosniff`、`CORP same-origin`。
- 共通:
  - `X-Content-Type-Options: nosniff`、`Referrer-Policy: same-origin`（内部のパスを Keycloak や Kanflow に渡さない）
  - `Cross-Origin-Opener-Policy: same-origin`、`Permissions-Policy`（カメラなどを無効）
  - 認証付きの応答には `Cache-Control: private, no-store` と `Vary: Authorization, Cookie`
- HSTS は設定 `web.hsts`（TLS をどこで終えるかで決まるため）。
- インラインのスクリプトはない。`eval` も WASM も使わない（構文の色付けは highlight.js の core と遅延の言語。shiki は WASM なので使わない）。
- Markdown は `react-markdown` と `remark-gfm` で描く。HTML は描かない（`rehype-raw` は使わない）。リンクは `http(s)` と相対パスだけ。画像は同じリポジトリの raw だけ。

**フォントとブランド**
- 外部の CDN は使わない。M2 はシステムのフォント（`"Yu Gothic UI", Meiryo, "Hiragino Sans", "Noto Sans JP", sans-serif`。等幅も同じ考え方）で、配るバイトは 0。
- 自前のフォントが要るときは woff2 を `web/` に同梱する。
- 色のトークン（ロゴの緑・紺・橙）は `docs/overview/logo-light.png` から取り、`web/src/styles/tokens.css` に 1 か所で定める。Kanflow と共有できる形（08 §4）にする。

**i18n**
- `i18next` と、`web/src/i18n/{ja,en,id}.json`。サーバーのエラーの `user_message` は既に ja/en/id（`forge_errors::Lang`）。
- 規則:
  - ja と en は全部のキーが必須（vitest のゲート）。
  - id は欠けていたら en で表示し、欠けている数を CI に出す。
  - ~~文書ポータルと~~ログイン・エラーの画面は、M2 で id まで埋める（YMMA の現場が読むため）。
- ~~08 の言い換えの表は、辞書の `plain.*` のキーとして持つ。~~（2026-11-01 取り下げ。ポータルがないため。取り除くのは後続のコードの MR）JSX の直書きの文字列は eslint（`react/jsx-no-literals`）で止める。

### 3.2 Keycloak のログインとセッション（ADR-0021、ADR-0019 の Amendment）

**新しいクレートと依存の向き**
- `forge-oidc`（機能クレート）: OIDC のクライアント、利用者の結び付け、セッションの発行の手順。
- forge-web → forge-oidc → forge-auth / forge-db → forge-core の向き。forge-auth は純粋なまま（I/O なし）。
- HTTP のクライアントは `reqwest`（rustls）。社内の CA は `auth.oidc.ca_file` で渡す。
  - `no_proxy()`（環境変数のプロキシで外へ出ない）。
  - リダイレクトに従わない。応答は 1 MiB まで。
- OIDC の部品は `openidconnect` を第一の候補にする。`search-first` と `cargo deny` / `audit` で確かめてから ADR で決める。ログアウトのトークンの検証は `jsonwebtoken` と同じ JWKS を使う。

**フロー（認可コード + PKCE、confidential client）**
1. `GET /-/auth/login?next=`（公開、`WebLogin`）
   - `state`・`nonce`・PKCE の `code_verifier`（S256）を作る。
   - それらを暗号化した短い Cookie `__Host-gstn_oidc` に入れる（HttpOnly、Secure、SameSite=Lax、10 分、`Path=/-/auth/`）。
   - Keycloak へ 302。`next` は相対パスだけで、ADR-0015 の規則で検証する。
2. `GET /-/auth/callback`（公開、`WebLogin`）
   - 照合: `state`。
   - トークンのエンドポイントの呼び出しは 5 秒まで。
   - ID トークンの検証: 署名、`iss`、`aud`、`azp`、`exp`、`iat`、`nonce`（時計のずれは 60 秒まで）。
   - 入場の判定: **`authorize(Principal::anonymous(), SessionCreate, Resource::Admission{ access_role, account })`**。account は New / Active / Disabled / NotHuman。
   - 結び付け（下記）→ セッションの発行 → `next` へ 302。
3. ログイン CSRF は `state` と専用の Cookie の組で防ぐ。ログインのたびにセッションの ID を新しくする（固定化の防止）。

**利用者の結び付け**（09 §8.1: `sub` が主キー、社員番号で結ぶ、メールだけでは結ばない）
- 新しい表 `forge.user_identities`（`user_id`、`issuer`、`subject`、`linked_at`、`last_login_at`）。
  - UNIQUE は (`issuer`,`subject`) と (`user_id`,`issuer`)。
  - 監査の対象（台帳 `tables.toml`）。マイグレーションは追記。
  - `users.auth_source` の CHECK は変えない（外部の ID は別の表に持つ）。
- 判定の順:
  1. (`iss`,`sub`) があれば、その利用者。
  2. 社員番号の claim（設定 `auth.oidc.employee_number_claim`）が、まだ結ばれていない人間の利用者と一致すれば結ぶ（監査 `user.identity_linked`）。
  3. どれでもなければ、**最初のログインで作る**（人間。ユーザー名は `preferred_username` を規則に合わせて正規化。メールは必須）。
  4. ユーザー名が使えない・メールが他人のもの・社員番号が別の ID に結ばれている → **FG-AUTH-0014**。runbook `oidc-identity-link` と、承認制の `forge admin user link-identity` で解く。
- `is_admin` は Keycloak から同期しない（§7 の Q2）。

**セッションの Cookie**
- `__Host-gstn_session` = `gss_<public_id>_<secret>`（トークンと同じ形・同じ照合: SHA-256 と定数時間の比較）。
- HttpOnly、Secure、SameSite=Lax、Path=/、Domain なし。
  - Lax にする理由: Kanflow から raw や archive への直接のリンク（トップレベルの GET）を通すため。副作用のある GET は作らない（ADR-0015）ので安全。
  - localhost は安全な文脈なので、開発でも Secure のまま。
- 表 `forge.web_sessions`
  - 列: `public_id`、`secret_sha256`、`user_id`、`created_at`、`last_seen_at`、`idle_expires_at`、`absolute_expires_at`、`revoked_at`、`revoked_reason`（`logout`|`backchannel`|`user_disabled`|`admin`|`idp_revoked`）、`oidc_sid`、`oidc_sub`、`refresh_token_enc`、`refresh_checked_at`。
  - `forge_ro` は `secret_sha256` と `refresh_token_enc` を読めない。
  - `last_seen_at` は 1 分に 1 回まで書き、監査しない。
- 既定の寿命: 無操作 4 時間、絶対 12 時間（設定可）。
- 利用者の無効化（ADR-0019 §12）と同じ `ChangeTx` で、セッションもすべて失効させる。
- ログインとログアウトは監査の行（`session.created`/`session.revoked`）に残す。人の数は 300 人規模なので量は小さい。

**CSRF**
- 状態を変える要求で、Cookie で認証したものには次を求める。
  - `X-GitStation-CSRF` = HMAC-SHA256(セッションの秘密, "csrf")。`GET /api/v1/session` が返す。定数時間で比べる。
  - **かつ** `Origin` が `web.base_url` と一致すること。
- Bearer の要求は対象外（自動で付く資格情報ではないため）。
- 失敗は **FG-AUTH-0016**（403）。

**トークンの更新と Keycloak の失効の反映**
- Keycloak のアクセストークンは使わない。リフレッシュトークンだけを AES-256-GCM で暗号化して持つ。鍵は `web.session_key_file`（ADR-0013）。
- 要求の時に `refresh_checked_at` が 5 分より古ければ、リフレッシュで「Keycloak のセッションが生きていて `access` ロールがある」ことを確かめる（3 秒まで）。
  - 無操作の人の Keycloak のセッションを延ばさないよう、操作があった時だけ確かめる。
  - 拒否された（`invalid_grant`、ロールがない）→ セッションを失効させ、`reason=idp_revoked` にする。
  - 届かない → **ログイン済みは続ける**（09 §8.2）。FG-AUTH-0015 を warn で 5 分に 1 回出し、`refresh_checked_at` を進める。絶対の寿命は延ばさない。

**ログアウト**
- `POST /api/v1/session/logout`（CSRF あり）: 行を失効させ、Cookie を消す。そのあと Keycloak の `end_session_endpoint`（`id_token_hint`、`post_logout_redirect_uri`）へ移る。
- `POST /-/auth/backchannel-logout`（公開、`OidcBackchannelLogout`）:
  - `logout_token` を検証する（署名、`iss`、`aud`、`iat`、`events` の claim、`sid` か `sub`、`nonce` がないこと）。
  - `jti` を 10 分のあいだ覚えて再送を拒む。
  - 一致したセッションを失効させる。不正は **FG-AUTH-0017**（400）。
  - Keycloak のクライアントは「バックチャネルのログアウトでセッションが必須」にする。

**トークン・git との共存**
- Smart HTTP は Cookie を見ない（ADR-0015）。`Authorization`（トークン）だけ。
- API は Bearer か Cookie のどちらか 1 つ。両方あれば `ambiguous`（401）。
- `gstn` と MCP は今の個人トークンのまま。トークンの発行の画面は、セッションでだけ使える（既存の「トークンはトークンを作らない」）。
- **ADR-0019 の 401 の Amendment**:
  - `/api/v1/*` の要求に `X-GitStation-Client: web` か `Sec-Fetch-Site` があるときは、`WWW-Authenticate: Basic` を付けない（Bearer だけ）。ブラウザのパスワードの入力画面と、Basic の資格情報が覚えられることを防ぐ。
  - git のルートは今のまま。

**Keycloak への呼び出しの時間切れ**

| 呼び出し | 上限 | 失敗の扱い |
|---|---|---|
| 接続 | 2 秒 | FG-AUTH-0015 `reason=connect_timeout` |
| discovery（起動時。失敗しても起動は続ける） | 5 秒・3 回・指数の待ち | 失敗したら、最初のログインの時に 1 回だけ取り直す（単一の実行）。ログインは 503 |
| JWKS | 5 秒。キャッシュ 10 分。知らない `kid` のときの取り直しは 60 秒に 1 回 | FG-AUTH-0015 |
| トークンのエンドポイント（コールバック） | 5 秒 | FG-AUTH-0015 |
| リフレッシュ（失効の確認） | 3 秒 | 続行（上記） |
| コールバックの全体 | 要求の時間切れ（30 秒）の中 | FG-WEB-0002 |

`/readyz` は Keycloak を見ない（git と API はトークンで動き続けるため）。`forge doctor` に `auth.oidc` の検査を足す（discovery、JWKS、時計のずれ、client の設定）。

**開発・オフラインの環境**
- `ops/dev/compose.yml` に、profile `sso` で Keycloak を足す。
  - `127.0.0.1:58080`、`start-dev`。
  - `ops/dev/keycloak/realm-gitstation-dev.json` を取り込む。架空の利用者（alice / bob / carol / admin、社員番号 `SYN0001`〜）、client `gitstation`、ロール `access`、社員番号の mapper。
  - client の秘密は架空の値をファイルで渡す。
- Postgres だけのテストは今のまま。
- nextest は Keycloak を使わない。`forge-oidc` の feature `test-support` に **プロセス内の模擬の OP**（テストの鍵、discovery、JWKS、token）を置く。
- 本物の Keycloak を使う E2E（Playwright）は、`sso` の profile で CI のジョブとして回す。
- `auth.oidc` が設定されていないとき: Web のログインの画面は「ログインが設定されていません（`gstn` とトークンを使う）」と `request_id` を出す。doctor は warn。黙って無効にはしない。

**エラーコード**（番号は登録時の次の空き。`errors.toml` に cause / diagnosis / fix / runbook / test を書き、カタログは生成する）

| コード | 名前 | HTTP | 重さ | runbook |
|---|---|---|---|---|
| FG-AUTH-0011 | LoginStateInvalid（state・PKCE・nonce・期限、IdP からの `error`） | 400 | info | — |
| FG-AUTH-0012 | IdTokenInvalid（署名・iss・aud・exp・azp） | 401 | warn | `oidc-login` |
| FG-AUTH-0013 | LoginNotPermitted（`SessionCreate` の拒否: `access` がない、無効、人間でない） | 403 | info | — |
| FG-AUTH-0014 | IdentityLinkConflict | 409 | warn | `oidc-identity-link` |
| FG-AUTH-0015 | IdentityProviderUnavailable | 503 | **alert** | `keycloak-unreachable` |
| FG-AUTH-0016 | CsrfRejected | 403 | info | — |
| FG-AUTH-0017 | LogoutTokenInvalid | 400 | warn | `oidc-login` |
| FG-WEB-0010 | RateLimited（ログインの開始、resolve の `prefix=true`） | 429 | info | — |
| FG-REPO-next | RepositoryPathNotResolved（ref・パスが解決できない。認可の後でだけ返す） | 404 | info | — |

既存のコードの再利用:
- セッションの期限切れ・失効: FG-AUTH-0001 `reason=session_expired|session_revoked`
- 無効の利用者: FG-AUTH-0004
- セッションの行の破損: FG-DB-0019
- 設定の誤り: FG-SYS-0002
- 認証中の DB: FG-DB-0001
- 閲覧の出力の上限: FG-GIT-0005

## 4. 画面（08 の原則の順。どれも「止まっている理由を 1 行」と `request_id` 付きの失敗の表示）

1. **ログイン**
   - 殻に来てセッションがなければ「IoT ポータルでログイン」の 1 ボタン。`next` を保つ。
   - 失敗は理由 1 行（FG-AUTH-0013 なら「GitStation の利用権限がありません。管理者に依頼してください」）。
   - 共通のヘッダー（アプリの切り替え・利用者・ログアウト）は、ポータルの形にそろえる（09 §8.4）。
2. **ホーム**
   - 自分のプロジェクト（メンバーのもの）、最近見たもの（ブラウザの中だけ）、~~仕様書（`kind=spec`）、~~Kanflow へのリンク。
   - ~~spec のプロジェクトにしか関わらない人には、文書ポータルの形をホームにする（08 原則 4）。~~（2026-11-01 取り下げ）
   - 中身のない「準備中」の枠は出さない。
3. **グループとプロジェクトの一覧**
   - 一覧・検索・グループの下の階層、作成（ウィザードは 08 §3.1 の 3 ステップの形）。
   - GitHub の形の URL の救済: `resolve?prefix=true` で `/-/` の形に `replaceState`（ADR-0015）。
4. **リポジトリの閲覧**（読み取りだけ。git を実行する）
   - ファイルの木、ファイル（テキストは 1 MiB まで。超えたら raw へ）。
   - コミットの一覧と 1 件（差分は 1 MiB・ファイルごとの上限）、比較、blame、ブランチ・タグ、clone の URL、archive。
   - Markdown の README を描く。
5. **メンバーと設定**: 表示名・説明、公開範囲（Owner）、保護 ref、メンバー（利用者の検索で追加・役割の変更・削除）。
6. **削除と復元**: 削除（猶予の期限を表示。確定の確認は戻せない操作だけ）、自分の削除待ちの一覧と復元。
7. **移動と名前の変更**: 移動先は見えるグループから選ぶ。墓標と回数の制限（FG-REPO-0018）の理由を 1 行で出す。
8. **トークン**
   - 一覧・発行（スコープ・期限 ≤365 日・`is_agent`）・失効。
   - 発行の直後に 1 回だけ表示し、`gstn auth login --with-token` の手順を出す。
9. **管理**（管理者だけ）: 利用者の一覧・無効化（ガード FG-AUTH-0009 の理由を表示。`--force` は出さない）、セッションの失効。

### 4.7 ~~文書ポータル（M2 の最小版、読み取り専用）~~ → 取り下げ（2026-11-01、ADR-0023）

> 実装して統合済み（MR12）だが、決定により取り下げ。コードは後続の MR で取り除く。以下は経緯として残す。

- 対象は `kind=spec` のプロジェクト。画面の言葉は 08 の言い換え（仕様書、確定した版、作業中の版）だけで、Git の言葉を出さない。
- 中身:
  - 仕様書の一覧。
  - 1 つの仕様書: 左に章の目次（`docs/specs/*.md`。なければ一番上の `*.md`）、右に本文。
  - 版の選択: 「最新（作業中の版）」= 既定のブランチ、「確定した版」= タグ。日付と、確定した人のメッセージ。
  - **2 つの版の違い**: 比較の API。章ごとに、追加と削除を色で分ける。
  - ダウンロード: 1 ファイル、または 1 つの版の zip。
- 入れないもの: アップロード、承認、要約、編集、決めるべき項目、Word / PDF の出力（M3〜M7、W12）。
- 使いやすさの確かめ（08 §5 の形）: 設備の担当が「最新の確定した版を開き、前の版との違いを 1 つ見つける」を説明なしで 3 分以内。2〜3 人で行う。

## 5. UI が使う新しい API（すべて `authorize()` を通し、権限の表に allow と deny の行を持つ）

**新しい Action**
- `WebLogin`（公開）、`OidcBackchannelLogout`（公開。判定は JWT の検証の後）
- `SessionCreate`（Admission）、`SessionRead`、`SessionLogout`、`SessionRevoke`
- `UserSearch`、`UserAdminList`
- `RepoBrowse`（`read_api`。役割は `RepoRead` と同じ判定）

**PLANNED から外すもの**: `TokenCreate`・`TokenList`・`TokenRevoke`・`UserDisable`。

| メソッドとパス | Action / Resource | スコープ | 主な allow / deny の行 |
|---|---|---|---|
| GET `/-/auth/login`、`/-/auth/callback` | WebLogin / Instance | — | 匿名も可 |
| （コールバックの中） | SessionCreate / Admission | — | allow: `access` あり・有効な人間。deny: ロールなし、無効、ボット |
| POST `/-/auth/backchannel-logout` | OidcBackchannelLogout / Instance | — | 公開。Cookie や Bearer があっても見ない |
| GET `/api/v1/session` | SessionRead / Instance | セッションだけ | deny: トークン、匿名 |
| POST `/api/v1/session/logout` | SessionLogout / Instance | セッションだけ | deny: トークン、CSRF なし |
| GET・DELETE `/api/v1/user/sessions[/{id}]` | SessionRevoke / Token{owner} | セッションだけ | deny: 他人のもの（見つからない扱い） |
| GET `/api/v1/user/tokens` | TokenList / Token{owner} | read_api | deny: 他人 |
| POST `/api/v1/user/tokens` | TokenCreate / TokenRequest | **セッションだけ** | deny: トークン（トークンはトークンを作らない）、`admin` スコープを管理者以外が求める |
| DELETE `/api/v1/user/tokens/{id}` | TokenRevoke / Token{owner} | api | deny: 他人 |
| GET `/api/v1/users?search=` | UserSearch / Instance | read_api | 有効な利用者。2 文字以上、20 件、名前と表示名だけ。deny: 匿名、無効 |
| GET `/api/v1/admin/users` | UserAdminList / Instance | read_api + admin_rights | deny: 管理者以外、admin スコープのないトークン |
| POST `/api/v1/admin/users/{id}/disable` | UserDisable / Instance | api + admin_rights | `Guard::Enforce` だけ。deny: 管理者以外、エージェント |
| DELETE `/api/v1/admin/users/{id}/sessions` | SessionRevoke / Instance | api + admin_rights | deny: 管理者以外 |
| GET `/api/v1/projects?~~kind=&~~membership=&state=pending_deletion` | ProjectList（行ごとに ProjectRead。削除待ちは ProjectRestore） | read_api | カーソルに絞り込みの条件を含める |
| GET `/api/v1/namespaces/resolve?prefix=true` | PathResolve | read_api | 見えるプロジェクトの最長一致だけ（ADR-0015）。利用者ごとに回数の制限 |
| GET `/api/v1/projects/{id}/repository/refs?type=` | RepoBrowse / Project | read_api | allow: Reporter 以上・internal の利用者。deny: Guest(private)、削除待ち |
| GET `…/repository/resolve?spec=` | RepoBrowse | read_api | ADR-0015 の順（oid → ブランチの最長 → タグの最長） |
| GET `…/repository/tree?ref=&path=`、`…/blob?ref=&path=` | RepoBrowse | read_api | 同上 |
| GET `…/repository/commits?ref=&path=&cursor=`、`…/commits/{oid}`、`…/compare?from=&to=`、`…/blame?ref=&path=` | RepoBrowse | read_api | 同上 |
| GET `{path}/-/raw/{ref}/{file}`、`{path}/-/archive/{ref}.{zip\|tar.gz}` | RepoRead / Project（dispatcher） | read_repository | 同上。ADR-0015 のヘッダー |
| GET `/-/assets/{file}` | WebShell / Instance | — | 公開 |

**gstn の C-1**
- `/api/v1` のルートは、どれも `forge-client/src/coverage.rs` に行を足す。
- `browse`・`token`・`user` のコマンドは同じ MR で足す。
- セッションのルートは新しい `Coverage::BrowserOnly`。

**git の実行**（forge-git の新しいモジュール `browse`）
- `for-each-ref`、`ls-tree -z --long`、`cat-file -s`、`cat-file blob`、`log -z --format`、`diff-tree --patch`、`blame --porcelain`、`archive`。
- どれも引数のベクタ、`--end-of-options`、解決し終えた ref か oid、パスは `--` の後ろ。
- 時間切れ: 一覧は 10 秒、blame と比較は 20 秒、archive は無通信 60 秒で全体 10 分。
- 出力の上限、リポジトリごとの同時実行の上限、`for-each-ref` の数秒のキャッシュ（push で消す）。

## 6. MR の分け方（12 本の製品の MR + エージェントモデルの MR 1 本）

全体の順序:
- MR-0 → MR1 → (MR2 ∥ MR3 ∥ MR7) → MR4 → MR5
- MR6（MR3 の後）、MR8（MR7 の後）
- UI: MR9（MR2・MR4 の後）→ MR10 → (MR11 ∥ ~~MR12~~（2026-11-01 取り下げ）)
- Rust のビルドは同時に 2 つまで。全体のテストは 1 つずつ。
- 各 MR の終わり: fmt / clippy / nextest、`gen-catalog`・`gen-authz` の差分なし、`/review` で CRITICAL / HIGH なし、引き継ぎを書く。

**MR-0（エージェントモデル、先に入れる）`chore/harness-ui-evaluators`**
- 中身:
  - forge-routing の「M2 で足す UI の評価役」を入れる: `typescript-reviewer`、`a11y-architect`、`i18n-sync-auditor`、`design-reviewer`、`e2e-runner`。それぞれに eval を 1 つ以上。
  - `web/**` の rules。stop の hook に `npm run lint`・`typecheck`・`test`。
- モデル: Sonnet。

**MR1 `docs/docs-m2-design`**
- 中身:
  - ADR-0021（OIDC とセッション、§3.2）、ADR-0022（フロント、§3.1）。
  - ADR-0015 の Amendment（`/-/assets`・`/-/auth`、予約語、`RepoBrowse`、プレビュー用のオリジンは M2 で作らない）。
  - ADR-0019 の Amendment（401 の Basic のチャレンジ、`SessionCreate`、無効化でセッションを失効）。
  - 01 §3.2・§8 の書き換え、この計画。
- テスト: なし。
- レビュー: `security-reviewer`、`authz-matrix-auditor`（設計として）。
- モデル: **Opus**。

**MR2 `feat/web-spa-foundation`**
- 中身:
  - `web/` の土台: ルーター、Query、生成した API の型、i18n（3 言語の辞書とゲート）、トークンとブランドの資産、エラーの表示（`user_message` + `request_id`）。
  - forge-web: `embed-ui`、殻の埋め込み、`/-/assets`、`security_headers`。
  - CI: GitHub Actions の web のジョブ（lint・tsc・vitest・build・audit）。
- テスト:
  - 殻はどのパスでも同じバイト（ハッシュの比較）。
  - 資産の一覧にないものは 404。各応答の CSP とヘッダー。
  - i18n のキーの対応。ブラウザのコンソールに CSP の違反がないこと（Playwright の素の起動）。
- レビュー: `rust-reviewer`、`security-reviewer`、`authz-matrix-auditor`、`typescript-reviewer`。
- モデル: **Opus**。

**MR3 `feat/auth-web-sessions`**
- 中身:
  - マイグレーション `web_sessions`・`user_identities`（追記、ロールの権限、`tables.toml`、doctor の期待値）。
  - `forge_db::sessions`（発行・照合・触る・失効・一括の失効）。
  - forge-core の `SessionAuthenticator` トレイト、forge-web の Cookie の認証、CSRF と Origin の検査。
  - `GET /api/v1/session`、無効化でセッションも失効。
  - 401 のチャレンジの変更。
  - テスト用のセッションの発行（`test-support` だけ）。
- テスト:
  - 照合の定数時間とダミーのハッシュ。期限切れ・失効の reason。
  - Cookie と Bearer の両方 → ambiguous。Cookie の git のルートは無視。
  - CSRF がない・違う・Origin が違う → FG-AUTH-0016。
  - 無効化の後の次の要求 → FG-AUTH-0004。`forge_ro` が秘密を読めないこと。
- レビュー: `security-reviewer`、`database-reviewer`、`rust-reviewer`、`error-contract-auditor`、`silent-failure-hunter`、`authz-matrix-auditor`。
- モデル: **Opus**。

**MR4 `feat/auth-oidc-login`**
- 中身:
  - クレート `forge-oidc`: discovery、JWKS、PKCE、state の Cookie、コールバック、`SessionCreate` の判定、結び付けと作成。
  - `forge admin user link-identity`。設定と doctor の `auth.oidc`。
  - compose の `sso` の profile と dev の realm。模擬の OP。
  - FG-AUTH-0011〜0015、FG-WEB-0010（ログインの開始の回数の制限）。runbook `oidc-login`・`oidc-identity-link`・`keycloak-unreachable`。
- テスト:
  - 模擬の OP で: 改ざんした ID トークン、`nonce`・`aud`・`azp` の違い、期限、時計のずれ、知らない `kid`（JWKS の取り直しの回数の制限）。
  - state の再利用、`next` の外部 URL の拒否。
  - 結び付けの全部の分岐（社員番号で結ぶ、メールだけでは結ばない、新規の作成、衝突は 0014）。
  - Keycloak の時間切れ → 0015 で、ログイン済みは続く。
  - E2E: dev の Keycloak でログインからホームまで。
- レビュー: `security-reviewer`、`rust-reviewer`、`error-contract-auditor`、`silent-failure-hunter`、`database-reviewer`、`authz-matrix-auditor`、`pr-test-analyzer`。
- モデル: **Opus**。

**MR5 `feat/auth-oidc-logout-refresh`**
- 中身: RP からのログアウト、バックチャネルのログアウト（`jti` の再送の拒否）、リフレッシュトークンの暗号化と 5 分ごとの確認、FG-AUTH-0017。
- テスト:
  - ログアウトのトークンの各検証の失敗。`sid` と `sub` で失効。再送の拒否。
  - リフレッシュの拒否 → `idp_revoked`、届かない → 続行と 5 分に 1 回の warn、絶対の寿命は延びない。
  - 鍵がない・壊れている → 起動で FG-SYS-0002。
  - E2E: Keycloak で無効にした人が次の操作で入れない（09 §8.5）。
- レビュー: MR4 と同じ。
- モデル: **Opus**。

**MR6 `feat/web-ui-support-api`**
- 中身:
  - トークン・セッション・利用者の検索・管理の利用者の一覧と無効化のルート（§5）。
  - 一覧の絞り込み（~~kind・~~membership・削除待ち。`kind` は 2026-11-01 に取り下げ）。resolve の `prefix=true` と回数の制限。
  - gstn の `token`・`user`。
- テスト:
  - 権限の表の全部の行。トークンでのトークンの発行の拒否。
  - `prefix=true` で見えないグループの存在が分からないこと（応答のバイトの比較）。
  - カーソルと絞り込みの結び付き。HTTP の無効化はガードを越えられない。
- レビュー: `authz-matrix-auditor`、`security-reviewer`、`rust-reviewer`、`database-reviewer`、`error-contract-auditor`、`pr-test-analyzer`。
- モデル: **Opus**。

**MR7 `feat/git-browse-read`**
- 中身:
  - forge-git `browse`（refs、resolve、tree、blob、archive）。`RepoBrowse` とその行。
  - API の refs・resolve・tree・blob。dispatcher の raw と archive（ADR-0015 のヘッダー、画像の形式の確認）。
  - gstn の `browse`（一部）。
- テスト:
  - `-` で始まるパスと ref、`--output=x`、D/F の衝突、40 / 64 桁の名前。
  - 上限を超える木とファイル、時間切れ、同時実行の上限。
  - archive の途中の切断と子プロセスの後始末。SVG が `octet-stream` になること。
  - 実際の git の結合テスト。
- レビュー: `git-protocol-reviewer`、`security-reviewer`、`rust-reviewer`、`silent-failure-hunter`、`authz-matrix-auditor`。
- モデル: **Opus**。

**MR8 `feat/git-browse-history`**
- 中身: commits・commit（差分の上限）・compare・blame の API、gstn の `browse` の残り。
- テスト: キーセットのカーソル、マージのコミット、名前の変更の差分、バイナリ、差分の上限と `truncated`、blame の時間切れ。
- レビュー: `git-protocol-reviewer`、`rust-reviewer`、`silent-failure-hunter`、`pr-test-analyzer`。
- モデル: Sonnet（MR7 の形をなぞる）。

**MR9 `feat/ui-shell-home-browse`**
- 中身: ログインの画面、共通のヘッダー、ホーム、グループとプロジェクトの一覧と作成、プロジェクトのトップ、URL の救済、404 とエラーの画面。
- テスト: vitest と MSW、Playwright（ログイン → ホーム → プロジェクト）、axe の a11y。
- レビュー: `typescript-reviewer`、`a11y-architect`、`i18n-sync-auditor`、`design-reviewer`、`e2e-runner`。
- モデル: Sonnet。

**MR10 `feat/ui-repository-view`**
- 中身: 木・ファイル（構文の色付け、Markdown）・コミット・差分・比較・blame・ブランチ・タグ・clone の URL・archive。
- テスト: Markdown の XSS のベクタ（`<script>`、`javascript:` のリンク、`data:` の HTML）が描かれないこと、大きいファイルの切り詰めの表示、E2E。
- レビュー: MR9 と同じ + `security-reviewer`（Markdown の描画）。
- モデル: Sonnet。

**MR11 `feat/ui-settings-members-tokens`**
- 中身: 設定・公開範囲・保護 ref・メンバー（利用者の検索）・削除と復元・移動と名前の変更・トークン・管理。
- テスト: 拒否の理由の 1 行の表示（FG-AUTH-0009、FG-REPO-0010・0018）、トークンを 1 回だけ表示すること、CSRF のヘッダーの付与、E2E。
- レビュー: MR9 と同じ。
- モデル: Sonnet。

**~~MR12 `feat/ui-document-portal`~~ — superseded（実装して統合済み。2026-11-01 の決定で取り下げ。コードの削除は後続の MR。ADR-0023）**
- 中身: §4.7。言い換えの辞書、id の翻訳の完成。
- テスト: Git の言葉が画面に出ないこと（辞書の禁止語の検査）、版の比較、E2E、使いやすさの確かめの記録。
- レビュー: MR9 と同じ。
- モデル: Sonnet。

## 7. 利用者に決めてほしいこと（推奨付き、5 つ）

1. **Keycloak の準備（IoT プラットフォームの側）**
   - 推奨: GitStation が求める形を先に渡し、それまでは dev の Keycloak で作って設定だけで切り替える。
   - 求める形: client `gitstation`（confidential、PKCE S256）、client role `access`、社員番号の mapper、バックチャネルのログアウトの URL（Keycloak から GitStation へ届くこと）、リダイレクトの URL。
2. **管理者の権限を Keycloak から同期するか**
   - 推奨: M2 では同期しない。`is_admin` は今のとおり GitStation（`bootstrap-admin` と承認制の手順）で決める。
   - 理由: 最後の管理者のガードとロックの順（ADR-0019 §11）を、外からの同期で壊さないため。
3. **最初のログインでの利用者の作成と結び付け**
   - 推奨: `access` を持つ人は最初のログインで作る。既存の利用者とは社員番号だけで結び、メールだけでは結ばない。衝突は管理者が `link-identity` で解く。
4. ~~**文書ポータルの M2 の範囲**~~（2026-11-01 取り下げ）
   - 推奨: 読み取りだけ（閲覧・版・違い・ダウンロード）。アップロードと承認は M7 のまま（MR = M3 が前提）。
5. **npm のパッケージの取り寄せ**
   - 推奨: 社内のミラー（Nexus / Verdaccio）があれば CI はそこから取る。なければ当面は GitHub Actions がビルドの時だけ公開の npm から取る。実行時は外に依存しない。lockfile、`--ignore-scripts`、audit を付ける。

## 8. 危険と対策

| 危険 | 対策 |
|---|---|
| 同じサイトの別のサブドメインからの CSRF（SameSite は効かない） | CSRF のトークンと `Origin` の検査を必須にする（MR3 のテスト） |
| ブラウザが Basic の入力画面を出す・資格情報を覚える | 401 のチャレンジの Amendment（MR3）。E2E で入力画面が出ないことを確かめる |
| Keycloak の障害でログインできない | ログイン済みは続く。git と API はトークン。FG-AUTH-0015 の alert と runbook |
| M1 の残りの持ち越し | M1 は完了。CI は GitHub でまだ一度も動いていないので、MR2 の CI の追加の前に main か PR で 1 回動かす |
| UI の評価役がない | MR-0 を先に入れる |
| 画面の範囲が広がる | §1 の「入れない」を守る。W15・W16 は計画書の段のまま |

## 9. テストの方針と完了の条件

**テストの方針**
- Rust: 単体、DB 付きの結合、実際の git、模擬の OP。
- フロント: vitest、MSW、axe、Playwright（Keycloak の `sso` の profile）。
- 権限の表: 新しい Action ごとに allow と deny の行。

**完了の条件（M2）**
- ポータルの Keycloak で 1 回ログインし、ブラウザだけでプロジェクトの作成からコードの閲覧・メンバーの管理・トークンの発行まで、`gstn` なしでできる（01 §8 の M2 の条件）。
- ログアウトと Keycloak での無効化が、次の操作から効く（09 §8.5）。
- git とトークンは Keycloak がなくても動く。
- ~~文書ポータルで、§4.7 の使いやすさの確かめを満たす。~~（2026-11-01 取り下げ。この条件は不要）
- 新しいルートはすべて権限の表と gstn の C-1 の表にある。エラーのカタログ、runbook、ADR がそろっている。

**WAITING FOR CONFIRMATION** — 進める案（A: サーバー側のセッション、推奨）と §7 の答え、または「修正: …」で返してください。
