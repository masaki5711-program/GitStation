# ADR-0021: OIDC（Keycloak）のログインと Web のセッション

**Date**: 2026-11-01
**Status**: accepted（計画 10 の案 A と §7 の推奨の答えをユーザーが承認、2026-11-01。M2 の MR1 で記録。ADR-0019 と ADR-0015 を改訂する）
**Deciders**: Masaki Fujikawa（ユーザー）、Claude

## 背景

M2 で Web の画面を入れる（計画 10）。ブラウザの利用者を認証する仕組みがまだない。M1 の認証は個人アクセストークン
（ADR-0019）だけで、`git` と API と `gstn` はそれで動いている。

- IoT プラットフォームにはポータルがあり、**Keycloak の OIDC で 1 回だけログインする**形にそろえる（計画 09 §8、W17、
  ユーザーの指示 2026-10-31）。Keycloak の realm は IoT プラットフォームのものを共有し、アプリごとに OIDC のクライアントを持つ。
  Keycloak が社内の AD をまとめるので、GitStation は LDAP に直接つながない。
- `forge.users` にはパスワードの列がない。ローカルのパスワードのログインは作らない。緊急時の入口は `gstn` とトークン。
- 前提として確かめたこと（作業ツリー、2026-11-01）:
  - `forge_auth::Credential::Session` は「すべてのスコープ」として既にある（`model.rs`）。
  - forge-web の `auth.rs` は、別の仕組みが置いたプリンシパルの上に `Authorization` があると `ambiguous`（401）にする（ADR-0019 の
    Amendment D3 MR3）。セッションの席は用意されている。
  - `forge.users` には `employee_number`（UNIQUE、1〜64 文字）、`external_id`、`auth_source`（CHECK `local`|`ldap`）、
    `email`（人間は必須、`lower(email)` で UNIQUE）がある。
  - すべての 401 に `WWW-Authenticate: Basic` と `Bearer` が付く（`error.rs`）。このままではブラウザの fetch で
    パスワードの入力画面が出る（ADR-0019 の Amendment M2 で直す）。
  - 設定には `web.*` と `auth.*` の名前空間がまだない（`config.rs` の `NAMESPACES`）。ADR-0015 が参照する `web.base_url` も
    まだ実装されていない（墓標の 302 は相対パスで組んでいる）。

## 決定

### 1. 方式: GitStation のサーバー側のセッション（計画 10 の案 A）

- Keycloak の OIDC（**認可コード + PKCE（S256）、confidential client**）でログインし、GitStation が自分のセッションを発行する。
  ブラウザが持つのは HttpOnly の Cookie だけで、Keycloak のトークンを JavaScript に置かない。
- Keycloak のアクセストークンは使わない（API は Keycloak の JWT を検証しない）。リフレッシュトークンは「Keycloak のセッションが
  生きているか」の確認にだけ使う（§7）。
- Keycloak が落ちても、ログイン済みのセッション、`git`、トークンの API は動き続ける（計画 09 §8.2）。新しいログインだけができない。

### 2. クレートと依存の向き

- 新しい機能クレート **`forge-oidc`**: OIDC のクライアント（discovery、JWKS、PKCE、state、トークンのエンドポイント、
  ID トークン・ログアウトトークンの検証）、利用者の結び付けと作成の手順、セッションの発行の手順。
- 向き: `forge-web → forge-oidc → forge-auth / forge-db → forge-core`。forge-hosting と同じ段の機能クレートで、forge-hosting には
  依存しない（機能クレートどうしは依存しない）。forge-auth は純粋なまま（I/O なし）。
- forge-web がセッションの照合に使うのは forge-core のトレイト **`SessionAuthenticator`**（トークンの `Authenticator` と同じ形。
  `forge serve` が forge-db の実装を注入する）。
- HTTP のクライアントは `reqwest`（rustls、ring。forge-client と同じ 0.12 系）。
  - 社内の CA は `auth.oidc.ca_file` で渡す。公開 CA のルートは入れない（Keycloak は社内の CA の証明書を使う）。
  - `no_proxy()`（環境変数のプロキシで外へ出ない）。リダイレクトに従わない。応答の本文は 1 MiB まで。
  - 接続先は discovery の文書の値のうち、`issuer` と同じオリジンのものだけを使う（違えば FG-AUTH-0015 `reason=endpoint_origin`。
    discovery の改ざん・誤設定で別のホストへ資格情報を送らない）。

### 3. ログインの流れ

1. `GET /-/auth/login?next=`（公開、Action `WebLogin`、Resource `Instance`）
   - `state`（256 bit）、`nonce`（256 bit）、PKCE の `code_verifier`（S256）を作る。乱数は OS の CSPRNG。
   - 3 つと `next` と作成時刻を AES-256-GCM で暗号化した Cookie **`__Host-gstn_oidc`** に入れる
     （HttpOnly、Secure、SameSite=Lax、`Path=/`、Domain なし、Max-Age 600 秒）。
     - **計画 10 の `Path=/-/auth/` は採らない**: `__Host-` の接頭辞は `Path=/` を必須とし、それ以外はブラウザが Cookie を捨てる。
       `__Secure-` にして Path を絞ると、同じサイトの別のサブドメインが `Domain=` 付きで自分の state の Cookie を押し込める
       （ログイン CSRF）。10 分で消える小さな Cookie なので、全パスに付く負担は受け入れる。
   - `next` は相対パスだけ（`/` で始まり、`//` と `/\` で始まらず、CR / LF / 制御文字を含まない）。ADR-0015 の規則で解析できる
     ものだけを受け付け、それ以外はホーム（`/`）にする（拒否の理由はログの `reason=next_rejected`）。
   - Keycloak の `authorization_endpoint` へ 302。`response_mode` は `query`（`form_post` は SameSite=Lax の Cookie が
     付かないので使わない）。`scope=openid`。
   - 開始の回数を制限する（接続の相手のアドレスごと。FG-WEB-0010、429）。相手のアドレスの取り方は ADR-0019 の未決
     （プロキシの構成）と一緒に決め、それまでは TCP の相手のアドレスを使う。
2. `GET /-/auth/callback`（公開、`WebLogin`）
   - `__Host-gstn_oidc` を復号し、`state` を定数時間で比べる。Cookie がない・復号できない・期限切れ・不一致、IdP からの
     `error` は **FG-AUTH-0011**（400、`reason` で区別）。Cookie はどの結果でも消す（1 回だけ使える）。
   - トークンのエンドポイントを呼ぶ（5 秒。`client_secret_basic`、`code_verifier`）。
   - ID トークンを検証する（§9）。失敗は **FG-AUTH-0012**（401）。
   - 入場の判定: **`authorize(Principal::anonymous(), Action::SessionCreate, Resource::Admission { access_role, account })`**。
     `account` は `New`（結び付けの結果、作る）/ `Active` / `Disabled` / `NotHuman`。allow は「`access` のロールがある」
     かつ `New` か `Active` だけ。拒否は **FG-AUTH-0013**（403、`reason=no_access_role|user_disabled|not_human`）。
     判定は `authorize()` の 1 か所で、権限の表に allow と deny の行を持つ。
   - 結び付け（§4）→ セッションの発行（§5）→ `next` へ 302（`Cache-Control: no-store`）。
3. ログイン CSRF は `state` と専用の Cookie の組で防ぐ。**ログインのたびに新しいセッションを作る**（既存のセッションの Cookie が
   あれば、その行を `reason=logout` で失効させる。固定化の防止）。
4. `access` のロールは ID トークンの `resource_access.<client_id>.roles` から読む。Keycloak のクライアントには、クライアントロールを
   **ID トークンに入れる mapper** を設定してもらう（§13）。

### 4. 利用者の結び付け（`forge.user_identities`）

計画 09 §8.1 の原則: **`sub` が主キー、既存の利用者とは社員番号で結ぶ、メールアドレスだけでは結ばない。**

- 新しい表 `forge.user_identities`（マイグレーションは追記。ADR-0014）
  - 列: `id`、`user_id`（FK）、`issuer`（text、1〜255）、`subject`（text、1〜255）、`linked_at`、`last_login_at`。
  - UNIQUE: (`issuer`, `subject`) と (`user_id`, `issuer`)（1 人は 1 つの IdP に 1 つの ID だけ）。
  - 監査の対象（台帳 `tables.toml`、`audit_resource = "user"`、`audit_id_column = "user_id"`）。`last_login_at` の更新は監査しない。
  - `forge_app`: SELECT・INSERT・UPDATE(`last_login_at`)。DELETE なし（結び直しは承認制の CLI が監査付きで行う）。
    `forge_ro`: `subject` を除く列（`sub` は IdP の利用者の識別子で、読み取りの経路に要らない）。
  - 計画 09 §8.2 の「利用者の表に外部の ID を足す」は、別の表にする形で満たす。`users.auth_source` の CHECK は変えない。
    OIDC で作った利用者は `auth_source = 'local'`、`external_id` は NULL のままにする。**`auth_source` はもうログインの方式を
    表さない**（列は D1 の名残。方式は `user_identities` の行の有無で分かる）。列の整理は、LDAP を使わないことが確定した後の
    追記のマイグレーションで行う（見送り）。
- 判定の順（同じ `ChangeTx` の中。社員番号の行は `FOR UPDATE`）:
  1. (`iss`, `sub`) の行があれば、その利用者。`last_login_at` を更新する。
  2. 社員番号の claim（設定 `auth.oidc.employee_number_claim`、既定 `employee_number`）が、**まだこの IdP に結ばれていない
     人間の利用者**の `employee_number` と一致すれば結ぶ（監査 `user.identity_linked`、`details` に `issuer`、`via=employee_number`。
     `sub` は入れない）。
  3. どれでもなければ、**最初のログインで作る**（人間）。`users::create` と同じ経路（パスのロック、墓標の確認、個人の名前空間）。
     - ユーザー名: `preferred_username` を ASCII の小文字にしただけのもの。`PathSegment::parse` を通らない、予約語、既存の名前、
       墓標と重なるときは作らない（別の名前を自動では選ばない。ADR-0015 の LDAP の自動作成と同じ規則）。
     - 表示名は `name`、なければユーザー名。メール（`email`）は必須、`email_verified = true` のものだけ。社員番号を保存する。
     - 監査は `user.created`（`details.via = "oidc"`）と `user.identity_linked` を同じ変更で書く。
  4. 次のときは **FG-AUTH-0014 IdentityLinkConflict**（409、warn、runbook `oidc-identity-link`）:
     `username_unavailable`、`email_taken`（メールが別の利用者のもの。結ばない）、`employee_number_linked`（社員番号の利用者が
     この IdP の別の `sub` に結ばれている）、`employee_number_missing`、`email_missing`、`email_unverified`。
     管理者が承認制の `forge admin user link-identity --user <u> --issuer <iss> --subject <sub>`（CLI、ADR-0019 §8 の例外と同じ扱い、
     監査あり）で解く。
- **社員番号の claim は必須にする**（計画 10 にない決定）: claim がないまま作ると、mapper の設定の誤りだけで、既存の全員に
  重複した利用者ができる。作った利用者は後から結び直す手段が CLI だけなので、誤りを最初のログインで止める。
- 結び付けた利用者が無効・人間でないときは、§3 の入場の判定が拒否する（結び付けの行は書かない）。
- **`is_admin` は Keycloak から同期しない**（計画 10 §7 の Q2）。管理者は今のとおり `bootstrap-admin` と承認制の手順で決める。
  最後の管理者のガードとロックの順（ADR-0019 §10・§11）を、外からの同期で壊さないため。
- **利用者の一括の同期はしない**。作るのは最初のログインのときだけ（計画 10 §7 の Q3）。

### 5. セッションの Cookie と `forge.web_sessions`

- Cookie: **`__Host-gstn_session`** = `gss_<public_id>_<secret>`（`public_id` は 16 桁、`secret` は 64 桁の小文字の 16 進）。
  トークン（ADR-0019 §1〜§3）と同じ部品（forge-core の `token.rs`）で作り、同じ照合（`public_id` で引き、SHA-256 を `subtle` で
  定数時間に比べる。見つからないときもダミーのハッシュで比べる）にする。接頭辞 `gss_` は秘密の走査で見つけるため。
- 属性: HttpOnly、Secure、SameSite=Lax、`Path=/`、Domain なし、Max-Age は絶対の寿命まで。
  - Lax にする理由: Kanflow などからの raw・archive への直接のリンク（トップレベルの GET）を通すため。副作用のある GET は作らない
    （ADR-0015）ので、Lax で送られる GET は状態を変えない。
  - localhost は安全な文脈なので、開発でも Secure のまま（Chrome / Edge / Firefox は `http://localhost` の Secure の Cookie を受け付ける）。
- 表 `forge.web_sessions`（マイグレーションは追記）
  - 列: `id`、`public_id`（UNIQUE）、`secret_sha256`（`bytea`、32）、`user_id`（FK）、`created_at`、`last_seen_at`、
    `idle_expires_at`、`absolute_expires_at`、`revoked_at`、`revoked_reason`（CHECK `logout`|`backchannel`|`user_disabled`|`admin`|`idp_revoked`）、
    `oidc_issuer`、`oidc_sid`、`oidc_sub`、`refresh_token_enc`、`id_token_enc`、`refresh_checked_at`。
  - **`id_token_enc` は計画 10 の列に足した**: RP からのログアウトで `id_token_hint` を渡すため（§8）。ない場合 Keycloak は
    確認の画面を出す。
  - `refresh_token_enc` と `id_token_enc` は AES-256-GCM（鍵は §7）。関連データ（AAD）に用途（`refresh_token` / `id_token`）と
    `public_id` を入れ、別の行・別の用途に写した暗号文を復号できないようにする。
  - `forge_app`: SELECT・INSERT・UPDATE（`last_seen_at`、`idle_expires_at`、`revoked_*`、`refresh_*`、`id_token_enc`）。DELETE は
    期限切れの掃除のジョブだけ（下記）。`forge_ro`: `secret_sha256`、`refresh_token_enc`、`id_token_enc`、`oidc_sub` を読めない。
    `guarded_objects` とスキーマの期待値に登録する。
  - 照合は書き込みのプール（ADR-0019 §3 と同じ）。`BoundedAuthenticator` と同じ同時実行の上限を通す。
- 寿命: 無操作 **4 時間**（`web.session_idle`）、絶対 **12 時間**（`web.session_absolute`）。どちらも設定でき、絶対 ≤ 24 時間。
  `last_seen_at` と `idle_expires_at` の更新は 1 分に 1 回まで（条件付きの UPDATE）。監査しない。更新の失敗は FG-DB-0002 を warn で
  出して続ける（ADR-0019 §6 と同じ）。
- 期限切れ・失効は **FG-AUTH-0001**（401、`reason=session_expired|session_revoked`）。応答で Cookie を消す（`Max-Age=0`）。
  利用者の無効は FG-AUTH-0004。行の破損は FG-DB-0019。認証中の DB の障害は FG-DB-0001（503。Cookie は消さない）。
- 監査: ログインとログアウトは監査の行 `session.created` / `session.revoked`（リソースは利用者、`details` に `reason`、`public_id`）。
  利用者は 300 人規模なので量は小さい。ドメインイベントにはしない。
- 利用者の無効化（ADR-0019 §12）と **同じ `ChangeTx`** で、その利用者の有効なセッションをすべて `reason=user_disabled` で失効させる
  （ADR-0019 の Amendment M2）。
- 掃除: 絶対の期限から 30 日を過ぎた行は、ジョブが消す（監査の行は残る）。

### 6. CSRF

- Cookie で認証した、状態を変える要求（GET・HEAD・OPTIONS 以外）には、次の **両方** を求める。
  1. ヘッダー `X-GitStation-CSRF` = HMAC-SHA256(セッションの `secret`, `"gitstation-csrf-v1"`) の 16 進。`GET /api/v1/session` が返す。
     サーバーは秘密を保存しない（Cookie の秘密から毎回計算する）。定数時間で比べる。
  2. `Origin` が `web.base_url` のオリジン（scheme + host + port）と完全に一致する。`Origin` がない要求は拒否する。
- `Authorization` で認証した要求（トークン）は対象外（自動で付く資格情報ではない）。`/-/auth/*` も対象外（login と callback は GET で
  `state` が守り、back-channel logout は Cookie を見ない）。
- 失敗は **FG-AUTH-0016 CsrfRejected**（403、`reason=token_missing|token_mismatch|origin_missing|origin_mismatch`）。
- SameSite=Lax は同じサイトの別のサブドメインからの要求を止めないので、トークンと `Origin` の両方を必須にする（計画 10 §8 の危険）。

### 7. Keycloak の失効の反映（リフレッシュトークン）

- 鍵: **`web.session_key_file`**（ADR-0013 の秘密のファイル。`api.cursor_keys_file` と同じ形式 `<kid> <hex>`、AES-256、
  いちばん大きい kid で暗号化し、並んでいる鍵はすべて復号に使う）。state の Cookie、`refresh_token_enc`、`id_token_enc` を暗号化する
  （用途は AAD で分ける）。鍵がない・壊れているときは、`auth.oidc` を設定していれば起動で FG-SYS-0002。
- 確認の時: Cookie で認証した要求で、`refresh_checked_at` が **5 分** より古いとき（操作があったときだけ。無操作の人の Keycloak の
  セッションを延ばさない）。
- **1 つのセッションで同時に 1 回だけ**: `UPDATE … SET refresh_checked_at = now() WHERE id = $1 AND refresh_checked_at < now() - 5 min
  RETURNING refresh_token_enc` で権利を取った要求だけがリフレッシュする（ほかの要求は待たずに続ける）。計画 10 にない決定:
  Keycloak の「Revoke Refresh Token」（使い回しの禁止）が有効だと、同時の 2 回のリフレッシュの片方が `invalid_grant` になり、
  正しいセッションを `idp_revoked` で失効させてしまうため。
- リフレッシュ（3 秒）の結果:
  - 成功し、返った ID トークン（§9 の検証。`nonce` は見ない）に `access` のロールがある → 新しいリフレッシュトークンと ID トークンを
    保存して続ける。
  - `invalid_grant`、またはロールがない → セッションを `reason=idp_revoked` で失効させ、この要求を FG-AUTH-0001
    （`reason=session_revoked`）にする。
  - 届かない・時間切れ・5xx → **ログイン済みは続ける**（計画 09 §8.2）。FG-AUTH-0015 を warn でログに出す（5 分に 1 回まで。
    権利を取った要求だけが出すので、セッションごとに 5 分に 1 回）。絶対の寿命は延ばさない。
- 受け入れる遅れ: Keycloak で無効にした人が入れなくなるまで、最大 5 分（とその人の次の操作）かかる。Keycloak の管理者が
  セッションを切ればバックチャネルのログアウト（§8）ですぐ効く。計画 09 §8.5 の「次の操作から」は、バックチャネルのログアウトで満たし、
  リフレッシュは届かなかったときの補いとする。

### 8. ログアウト

- **RP からのログアウト**: `POST /api/v1/session/logout`（`SessionLogout`、セッションだけ、CSRF あり）。行を `reason=logout` で
  失効させ、Cookie を消し、本文で Keycloak の `end_session_endpoint` の URL（`id_token_hint`、`post_logout_redirect_uri` =
  `web.base_url` + `/`、`client_id`）を返す。SPA がその URL へ移る。URL はサーバーが discovery の値と設定から組み立てる。
- **バックチャネルのログアウト**: `POST /-/auth/backchannel-logout`（公開、`OidcBackchannelLogout`。Cookie も `Authorization` も見ない）。
  - 本文は `application/x-www-form-urlencoded` の `logout_token`（64 KiB まで）。
  - 検証（§9 の署名と鍵の規則）: `iss`、`aud`、`iat`（時計のずれ 60 秒、2 分より古いものは拒否）、`exp` があればその期限、
    `events` に `http://schemas.openid.net/event/backchannel-logout` があること、`sid` か `sub` があること、**`nonce` がないこと**、`jti` があること。
  - `jti` を 10 分のあいだ覚えて再送を拒む（プロセス内の上限付きの表。複数のプロセスで動かすときは DB に移す — 見送り）。
  - `sid` があれば `oidc_sid` が一致する行、なければ `oidc_sub` が一致する行を、`reason=backchannel` で失効させる。
    一致がなくても 200（Keycloak に再送させない）。
  - 不正は **FG-AUTH-0017 LogoutTokenInvalid**（400、warn）。応答は `Cache-Control: no-store`。
  - Keycloak のクライアントは「Backchannel logout session required」を有効にする（`sid` を送らせる）。

### 9. ID トークン・ログアウトトークンの検証の規則

- 署名の方式は許可リスト（`auth.oidc.signing_algs`、既定 `RS256`。`ES256` を足せる）。`none` と HS 系は拒否する。
- ヘッダーの `kid` は必須。JWKS の鍵は `use = sig`（または `use` なし）で、`alg` があれば一致すること。
- ID トークン: 署名、`iss`（discovery の `issuer` と完全一致、`auth.oidc.issuer` とも一致）、`aud` に `client_id` を含む、
  `aud` が複数なら `azp = client_id`、`exp`、`iat`、`nonce`（ログインのときだけ）、`sub`（1〜255 文字）。時計のずれは 60 秒まで。
- `sid`、`sub` は ID トークンからセッションの行に写す（§8 で使う）。

### 10. トークン・git との共存

- **Smart HTTP は Cookie を見ない**（ADR-0015）。`git` は `Authorization`（トークン）だけ。dispatcher で Cookie を受け付けるのは
  raw と archive の語だけ。
- API と raw・archive は、`Authorization` か Cookie のどちらか 1 つ。両方あれば `ambiguous`（401）で、どちらも試さない。
- セッションのプリンシパルは `Credential::Session`（すべてのスコープ）、`is_agent = false`。
- `gstn` と MCP は今の個人トークンのまま（device flow は見送り）。
- トークンの発行（`POST /api/v1/user/tokens`、`TokenCreate`）は **セッションだけ**（トークンはトークンを作らない。
  ADR-0019 の Amendment M2）。
- `/readyz` は Keycloak を見ない（`git` と API はトークンで動き続けるため）。

### 11. Keycloak への呼び出しの時間切れ

| 呼び出し | 上限 | 失敗の扱い |
|---|---|---|
| 接続（すべての呼び出し） | 2 秒 | FG-AUTH-0015 `reason=connect_timeout` |
| discovery（起動時） | 5 秒・3 回・指数の待ち（1 秒、2 秒） | 起動は続ける。最初のログインの時に 1 回だけ取り直す（同時の要求は 1 つの取り直しを待つ）。取れなければログインは FG-AUTH-0015（503） |
| JWKS | 5 秒。キャッシュ 10 分。知らない `kid` のときの取り直しは 60 秒に 1 回まで | FG-AUTH-0015。60 秒の間に知らない `kid` が来たら FG-AUTH-0012 `reason=unknown_kid` |
| トークンのエンドポイント（コールバック） | 5 秒 | FG-AUTH-0015 |
| リフレッシュ（失効の確認） | 3 秒 | 続行（§7） |
| コールバックの全体 | 要求の時間切れ（`server` の 30 秒）の中 | FG-WEB-0002 |

- FG-AUTH-0015 IdentityProviderUnavailable は **alert**（runbook `keycloak-unreachable`）。ログには `reason`
  （`connect_timeout`、`timeout`、`status`、`body_too_large`、`invalid_json`、`endpoint_origin`、`tls`）と呼び出しの種類を出す。
  Keycloak の応答の本文はログに出さない（トークンを含みうる）。
- `forge doctor` に `auth.oidc` の検査を足す: discovery、JWKS、時計のずれ（Keycloak の `Date` との差）、`issuer` の一致、
  client の資格情報（`client_credentials` は使わないので、設定の形と秘密のファイルの権限だけ）。`auth.oidc` がなければ warn。

### 12. 設定

- 新しい名前空間 `FORGE_WEB_` と `FORGE_AUTH_`（ADR-0013。下の未知の変数は失敗）。
- `web.base_url`（必須。`https://` か `http://localhost`/`127.0.0.1`。パスなし。`Origin` の比較、`redirect_uri`、
  `post_logout_redirect_uri`、ADR-0015 の `Location` の元）、`web.session_key_file`、`web.session_idle`、`web.session_absolute`、
  `web.hsts`（ADR-0022）。
- `auth.oidc.issuer`、`auth.oidc.client_id`、`auth.oidc.client_secret_file`（秘密のファイル）、`auth.oidc.ca_file`、
  `auth.oidc.employee_number_claim`、`auth.oidc.access_role`（既定 `access`）、`auth.oidc.signing_algs`。
  `redirect_uri` は `web.base_url` + `/-/auth/callback` に固定する（設定にしない）。
- `auth.oidc` がないとき: ログインの画面は「ログインが設定されていません（`gstn` とトークンを使ってください）」と `request_id` を出す。
  `/-/auth/login` は **FG-AUTH-0018 LoginNotConfigured**（503、info。計画 10 にないコードで、MR4 で登録する）。FG-SYS-0002 は使わない
  （500・alert は「設定が誤っている」の意味で、ログインを使わない構成はそれに当たらず、押すたびに alert が出てしまう）。
  doctor は warn。黙って無効にはしない。`auth.oidc` の値が一部だけある・誤っているときは、起動で FG-SYS-0002。

### 13. Keycloak の準備は IoT プラットフォームの側（計画 10 §7 の Q1）

GitStation は求める形を先に渡し、それまでは dev の Keycloak（§14）で作って、設定だけで切り替える。求める形:

- client `gitstation`: confidential、Standard flow のみ（implicit・direct access grants・service account は無効）、PKCE S256 必須。
- Valid redirect URI: `<web.base_url>/-/auth/callback` だけ（ワイルドカードなし）。Post logout redirect URI: `<web.base_url>/`。
- Backchannel logout URL: `<web.base_url>/-/auth/backchannel-logout`、「session required」を有効。Keycloak から GitStation へ届くこと。
- client role `access`。クライアントロールを ID トークンに入れる mapper（`resource_access.gitstation.roles`）。
- 社員番号の mapper（ID トークンの claim `employee_number`）。`email`、`email_verified`、`preferred_username`、`name`。
- 署名は RS256（realm の既定）。

### 14. 開発・テストの環境

- **dev の Keycloak**: `ops/dev/compose.yml` に profile **`sso`** で足す（Postgres だけのテストは今のまま）。
  - `127.0.0.1:58080`、`start-dev`。イメージはタグと digest で固定する。
  - `ops/dev/keycloak/realm-gitstation-dev.json` を取り込む: 架空の利用者（alice / bob / carol / admin、社員番号 `SYN0001`〜、
    `example.invalid` のメール）、client `gitstation`、ロール `access`（carol は持たない）、社員番号の mapper。
  - client の秘密は架空の値をファイルで渡す（`ops/dev/keycloak/client-secret.dev`、合成と明記）。
- **プロセス内の模擬の OP**: `forge-oidc` の feature `test-support` に置く（`127.0.0.1:0` の axum。discovery、JWKS、authorize、token、
  失敗の注入: 遅延、5xx、不正な JSON、鍵の差し替え）。
  - 署名の鍵: ES256 はテストごとに `ring` で作る。RS256 は合成のテスト用の鍵を `crates/forge-oidc/tests/fixtures/` に置く
    （テスト専用、ファイルの先頭に synthetic と明記。ring は RSA の鍵を作れず、`rsa` クレートは §15 の理由で入れない）。
  - nextest は Keycloak を使わない。`test-support` は forge-web の前例と同じく、`forge` のバイナリの通常の依存に入らないことを CI で確かめる。
- **E2E**（Playwright）は本物の Keycloak（`sso` の profile）で、CI のジョブとして回す（MR4・MR5）。

### 15. OIDC のクレートの選択

`search-first` と `cargo deny` / `cargo audit` で確かめた（2026-11-01。作業用のクレートで解決し、このリポジトリの `deny.toml` で検査）。

| 案 | 内容 | 保守 | ライセンス | 勧告（`cargo deny check advisories`） | 依存（通常の依存の数） |
|---|---|---|---|---|---|
| A | `openidconnect` 4.1.0（`oauth2` 5.0.0 を含む） | 1 人の保守者。4.1.0 は 2026-10-31（1 日前）、その前は 4.0.1（2025-07） | MIT | **失敗**: RUSTSEC-2023-0071（`rsa` 0.9.10、Marvin。修正版なし）が通常の依存のグラフに入る | 約 240（`rsa`、`p256`/`p384`、`ed25519-dalek`、`serde_with`、`time` など） |
| B | 自前の薄いクライアント + `jsonwebtoken` 11.1.0（`default-features = false`）+ 自前の `ring` の `CryptoProvider` | jsonwebtoken は 2026-09-16 の版、10 系から継続して保守 | MIT | 通る（`ring` は既に依存にある） | jsonwebtoken の直接の依存は `base64`・`serde`・`serde_json`・`signature`・`zeroize` だけ |
| B' | B と同じで、jsonwebtoken の `rust_crypto` | 同上 | 同上 | 失敗（`rsa` が入る） | — |
| B'' | B と同じで、jsonwebtoken の `aws_lc_rs` | 同上 | 通る（`aws-lc-sys` の複合ライセンスは許可リストで通る） | 通る | 2 つ目の暗号のライブラリ（C のビルド）が入る |
| C | `ring` だけで JWS を自前で検証 | — | — | 通る | 最小 |

- 確かめたこと:
  - `cargo deny 0.20.2`、`cargo-audit 0.22.2` が入っている。A を足した作業用のクレートで `cargo deny check advisories` が
    RUSTSEC-2023-0071 で失敗した。このリポジトリの `deny.toml` は勧告の無視を持たず、`rsa` は今の解決済みのグラフにない
    （`Cargo.lock` に `sqlx-mysql` の名残として載るだけで、`.cargo/audit.toml` がそれを理由に無視している）。A を入れると、その無視が
    「実際に使う依存」に掛かることになる。
  - `cargo audit` は、このリポジトリの `.cargo/audit.toml` の無視が効くので A でも何も出なかった。判断は `cargo deny` の結果で行う。
  - workspace の `rust-version` は 1.85 だが、`openidconnect` 4.1 も `jsonwebtoken` 10 / 11 も 1.88 を要する。ツールチェーンは
    1.99.1 なので作れるが、MR4 で `rust-version` を 1.88 以上に上げる。
  - `openidconnect` はバックチャネルのログアウトトークンの検証を持たないので、A でもログアウトには別の JWT の検証が要り、
    検証の仕組みが 2 つになる。
  - jsonwebtoken 11 は、暗号のバックエンドの feature がなく `CryptoProvider` も入れていないと、検証の時にライブラリの中で `panic!` する。
- **推奨と決定: B**。
  - discovery・JWKS・PKCE・state・トークンのエンドポイントは自前で書く（`reqwest`、`sha2`、`base64`、`getrandom`、`aes-gcm` は
    既に依存にある）。量は数百行で、時間切れ・失敗の分類（§11）・ログの規則を契約どおりにそろえやすい。
  - JWS の解析・`exp` などの検証・JWK の扱いは `jsonwebtoken` 11 に任せ、署名の検証だけを `forge-oidc` の `ring` の `CryptoProvider`
    （RS256、ES256 の検証だけ。署名の生成は持たない）で行う。
  - `panic!` の経路を閉じる: 検証器（`forge_oidc::JwtVerifier`）の作成の関数だけが `CryptoProvider::install_default()` を呼び、
    検証器を作らずに検証する経路を持たない。既に別の provider が入っていたら（自分のものでなければ）FG-SYS-0001 で起動を止める。
    テストで「作成の前に検証できない」ことを型で、「2 回目の作成が通る」ことを実行で確かめる。
  - 版は `=11.1.0` の形で固定し、上げるときは `cargo deny` と、この節の再確認を MR に書く。
- A を採らない理由: `rsa`（RUSTSEC-2023-0071、修正版なし）を実際に使う依存として入れることになり、`deny.toml` に無視を足す必要がある。
  影響は秘密鍵の操作の時間差で、公開鍵での検証には当たらないが、「勧告の無視なし」の方針を崩す。依存が約 240 増え、
  4.1.0 は公開から 1 日しか経っていない。

### 16. エラーコード（登録は使う MR で。今は登録しない）

番号は登録の時の次の空き（今は FG-AUTH-0011、FG-WEB-0010、FG-REPO-0023 が次）。`errors.toml` に cause / diagnosis / fix /
runbook / test を書き、カタログは生成する。

| コード | 名前 | HTTP | 重さ | runbook | 登録する MR |
|---|---|---|---|---|---|
| FG-AUTH-0011 | LoginStateInvalid（state・PKCE・nonce・期限、IdP からの `error`） | 400 | info | — | MR4 |
| FG-AUTH-0012 | IdTokenInvalid（署名・iss・aud・exp・azp・kid） | 401 | warn | `oidc-login` | MR4 |
| FG-AUTH-0013 | LoginNotPermitted（`SessionCreate` の拒否） | 403 | info | — | MR4 |
| FG-AUTH-0014 | IdentityLinkConflict | 409 | warn | `oidc-identity-link` | MR4 |
| FG-AUTH-0015 | IdentityProviderUnavailable | 503 | **alert** | `keycloak-unreachable` | MR4 |
| FG-AUTH-0016 | CsrfRejected | 403 | info | — | MR3 |
| FG-AUTH-0017 | LogoutTokenInvalid | 400 | warn | `oidc-login` | MR5 |
| FG-AUTH-0018 | LoginNotConfigured（`auth.oidc` がない。§12。計画 10 に足した） | 503 | info | — | MR4 |
| FG-WEB-0010 | RateLimited（ログインの開始、resolve の `prefix=true`） | 429 | info | — | MR4 |

既存のコードの再利用: FG-AUTH-0001（`session_expired`・`session_revoked`）、FG-AUTH-0004（無効の利用者）、FG-DB-0019（セッションの行の
破損）、FG-SYS-0002（設定・鍵のファイルの誤り）、FG-DB-0001（認証中の DB）、FG-DB-0002（`last_seen_at` の更新の失敗）、
FG-WEB-0002（コールバックの時間切れ）。

### 17. 新しい Action と権限の表の行（MR3〜MR6 で `matrix_rows.rs` に足す）

| Action | Resource | 主な allow / deny |
|---|---|---|
| `WebLogin` | `Instance` | 公開（匿名も可） |
| `SessionCreate` | `Admission { access_role, account }` | allow: ロールあり × `New` / `Active`。deny: ロールなし、`Disabled`、`NotHuman` |
| `OidcBackchannelLogout` | `Instance` | 公開（判定は JWT の検証の後）。資格情報を見ない |
| `SessionRead` / `SessionLogout` | `Instance` | セッションだけ。deny: トークン、匿名 |
| `SessionRevoke` | `Token { owner }` / `Instance`（管理） | 自分のものだけ。他人のものは見つからない扱い。管理の一括の失効は管理者だけ |

## 検討した案

| 案 | 内容 | 判断 |
|---|---|---|
| **A（採用）** | GitStation のサーバー側のセッション（Cookie）、Keycloak は入口と失効の確認だけ | Keycloak の障害でもログイン済みは続く。失効は DB の行で、理由がログの `reason` に出る |
| B | BFF が Keycloak のアクセストークンを持ち、API に Bearer で渡す | API がすべて JWT の検証を要する。更新に失敗すると全員が止まる |
| C | SPA が公開クライアントとしてトークンを持つ | XSS でトークンが盗まれる。CSP があってもトークンを JS に置かない |
| D | LDAP に直接つなぎ、ローカルのパスワードも持つ（計画 01 の元の M2） | ポータルの 1 回のログインにならない。パスワードの保管と総当たりの対策が要る。Keycloak が AD をまとめる |

## 結果

- ブラウザは Cookie だけを持ち、`git`・`gstn`・MCP はトークンのまま。Keycloak の障害で止まるのは新しいログインだけ。
- 依存の向きは保つ（forge-oidc は機能クレート、forge-auth は純粋なまま）。
- 失敗は FG-AUTH-0011〜0018 と既存のコードに分かれ、どれも `request_id` と `reason` で追える。

**受け入れたリスク**

- Keycloak で無効にした人のセッションは、バックチャネルのログアウトが来なければ最大 5 分と次の操作まで残る（§7）。
- 処理中のリクエストは、失効の後もそのリクエストの間は完了しうる（ADR-0019 と同じ）。
- `jti` の再送の拒否はプロセス内の表なので、プロセスの再起動の直後は 10 分以内の同じログアウトトークンを受け付けうる（結果は同じ失効なので害はない）。
- 合成の RS256 の秘密鍵をテストの固定のファイルとして持つ（秘密の走査の許可リストに載せる）。

## 見送り

- `gstn auth login` のデバイスフロー。
- Keycloak からの管理者の同期、利用者の一括の同期。
- `users.auth_source` の整理。
- `jti` の表の DB への移動（複数のプロセスで動かすとき）。
- 接続の相手ごとの回数の制限で、プロキシの後ろの送信元を使うこと（ADR-0019 の未決と一緒に決める）。

## 影響するエラーコードと Runbook

- 追加（§16、各 MR で登録）: FG-AUTH-0011〜0018、FG-WEB-0010。
- Runbook 追加: `oidc-login`（FG-AUTH-0012・0017）、`oidc-identity-link`（FG-AUTH-0014、`link-identity` の承認の手順）、
  `keycloak-unreachable`（FG-AUTH-0015）。
- Runbook 更新: `user-offboarding`（無効化でセッションも失効する。Keycloak の側の無効化との順序）。
