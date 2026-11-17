# Runbook: oidc-login（FG-AUTH-0012 / FG-AUTH-0025 / FG-AUTH-0011 / FG-AUTH-0013 / FG-AUTH-0017 / FG-AUTH-0018）

ブラウザのログイン（Keycloak の OIDC、ADR-0021 §3・§9）が失敗するとき、バックチャネルのログアウト（FG-AUTH-0017、ADR-0021 §8）が
断られるとき、5 分ごとのリフレッシュの確認でセッションが切れるとき（ADR-0021 §7、M2 MR5）の診断。

## Trigger

- `error_code=FG-AUTH-0012` IdTokenInvalid（warn、401）。ログの `context` に `event=forge.auth.login_failed` と `reason`:
  `malformed`、`alg_not_allowed`（`alg`）、`kid_missing`、`unknown_kid`、`key_invalid`（`detail`）、`key_alg_mismatch`（`alg`、`kty`）、
  `signature`、`issuer`、`audience`、`azp`、`expired`、`not_yet_valid`（`claim`）、`nonce`、`subject`、`missing_claim`（`claim`）。
- `error_code=FG-AUTH-0025` AccessTokenInvalid（warn、401）。`auth.oidc.claims_source = access_token`（PFS の Keycloak）のときだけ。
  `reason`: 0012 と同じ署名・鍵・`issuer`・`expired`・`not_yet_valid`・`missing_claim`（`claim`: exp、iss、sub、azp）に加えて
  `azp`（別のクライアントのトークン）、`typ`（`Bearer` でない: ID トークンなど）、`subject_mismatch`（ID トークンと `sub` が違う）、
  `audience`（`aud` が文字列でも文字列の並びでもない）。下の「PFS Keycloak」を見る。
- 利用者から「ログインできない」の問い合わせ（`request_id` 付き）。FG-AUTH-0011（state の誤り）、FG-AUTH-0013（入場の拒否）、
  FG-AUTH-0018（`auth.oidc` がない）も同じ手順で見分ける。
- `error_code=FG-AUTH-0017` LogoutTokenInvalid（warn、400）。`POST /-/auth/backchannel-logout` で、呼ぶのは Keycloak のサーバー
  （人は見ない）。ログの `context` に `event=forge.auth.backchannel_logout_failed` と `reason`: `body`（`detail`: content_type、
  too_large、form、token_missing）、0012 と同じ署名・鍵の理由、`issuer`、`audience`、`expired`、`not_yet_valid`、`too_old`
  （`iat` が 2 分より古い）、`missing_claim`（`claim`: iat、jti、events、sid_or_sub）、`events`、`nonce_present`、`replayed`、
  `replay_table_full`。
- 「ログインしていたのに急に出された」の問い合わせ: ログの `forge.auth.session_refresh_revoked`（`why`: invalid_grant、
  session_continue_denied、id_token_invalid、id_token_missing、refresh_token_unreadable、`revoked_reason`）か
  `forge.auth.backchannel_logout`（`revoked`）。

## Impact

- 新しいログインだけが失敗する。ログイン済みのセッション、`git`、トークンの API、`gstn` は影響を受けない。
- 何も書かれていない（利用者・結び付け・セッションのどれも）。
- FG-AUTH-0017: そのログアウトでセッションは失効していない。Keycloak で切った人が GitStation では最長 5 分（とその人の次の操作）
  まで残る（リフレッシュの確認が補う）。

## Diagnose（読み取りだけ）

1. `request_id` でログを引く: `error_code` と `reason` を読む。トークンや claim の値はログに出ない。
2. `forge doctor` の `auth.oidc`: discovery、JWKS、時計のずれ（Keycloak の `Date` との差）、`issuer` の一致。
3. 理由ごとに:
   - `issuer`: `auth.oidc.issuer` と Keycloak の realm の issuer（`/.well-known/openid-configuration` の `issuer`）が文字どおり同じか（末尾の `/`、http/https）。
   - `audience` / `azp`: client id（`auth.oidc.client_id`）と Keycloak のクライアント名が同じか。別のクライアントの audience mapper が入っていないか。
   - `expired` / `not_yet_valid`: サーバーの時計（`timedatectl`、NTP）。60 秒より大きいずれは直す。
   - `unknown_kid` / `signature`: realm の鍵の入れ替えの直後なら 60 秒（JWKS の取り直しの間隔）待つ。続くなら realm の鍵の設定
     （有効な RS256 の鍵があるか）と、GitStation と Keycloak の間の経路（中間者）を確かめる。
   - `alg_not_allowed` / `key_alg_mismatch` / `key_invalid`: realm の署名の方式（RS256 が既定）と鍵の種類。ES256 を使うなら `auth.oidc.signing_algs` に足す。
   - FG-AUTH-0011 `state_cookie_invalid`: 複数のプロセスで `web.session_key_file` が違う、またはログインの途中で鍵を変えた。
   - FG-AUTH-0013 `no_access_role`: Keycloak でクライアントロール `access` が付いていない。`user_disabled`: GitStation の利用者が無効。
   - FG-AUTH-0018: `[auth.oidc]` が設定されていない。
   - FG-AUTH-0017 `issuer` / `audience` / `signature`: Keycloak のクライアント `gitstation` の「Backchannel logout URL」が
     `web.base_url` + `/-/auth/backchannel-logout` か、realm と `auth.oidc.issuer`・client id が同じか。同じピアからの連続は
     探りの可能性（有効な署名のトークンなしには何も失効しない）。
   - FG-AUTH-0017 `missing_claim claim=sid_or_sub`: 「Backchannel logout session required」を有効にする（`sid` を送らせる）。
   - FG-AUTH-0017 `too_old` / `not_yet_valid`: 両方のサーバーの時計（`forge doctor` のずれ）か、Keycloak の配送の遅れ。
   - FG-AUTH-0017 `replayed`: Keycloak の再起動の後の再送なら無害（最初の配送で失効済み）。
   - リフレッシュでの失効 `why=session_continue_denied`: Keycloak で `access` のロールが外れた（`revoked_reason=idp_revoked`）か
     GitStation で無効化された（`user_disabled`）。`why=id_token_missing`: リフレッシュの応答に ID トークンが 4 回続けてない
     （クライアントの scope に `openid` があるか）。`why=refresh_token_unreadable`（FG-DB-0019 も出る）: `web.session_key_file`
     から古い kid を早く消した。
   - FG-AUTH-0015 `call=refresh`（warn）: リフレッシュの時に Keycloak に届かない。ログイン済みは続く（runbook keycloak-unreachable）。
   - リフレッシュでの失効 `why=invalid_grant` が多い: Keycloak の「Revoke Refresh Token」（回転）が有効だと、受け取った新しい
     リフレッシュトークンを保存できなかったセッションは次で `invalid_grant` になる。確認は要求から切り離したタスクで保存まで
     行う（ADR-0021 の 33）ので、続くなら DB の書き込みの失敗（同じ `request_id` の FG-DB-0001 / 0002）か、同じリフレッシュ
     トークンを別のプロセスが使った（複数の GitStation が同じ DB を見ている）かを確かめる。

## Mitigate

- 設定の誤り（issuer、client id、signing_algs）: 設定を直して再起動する。`[MUTATING — needs approval]`（本番の設定の変更）。
- 時計: NTP を直す（ホストの運用の手順）。
- Keycloak 側（ロール、鍵、mapper）: IoT プラットフォームの Keycloak の管理者に依頼する（ADR-0021 §13 の求める形）。

## Verify

- 同じ利用者でログインが通り、ログに `forge.auth.login_succeeded`（`user_id`、`outcome`、`public_id`）が出る。
- バックチャネル: Keycloak の管理画面でその人のセッションを切ると、ログに `forge.auth.backchannel_logout`（`revoked` ≥ 1）が出て、
  その人の次の要求が FG-AUTH-0001 `session_revoked`（`revoked_reason=backchannel`）になる。
- `forge doctor` の `auth.oidc` が `ok`。

## Feedback

- 新しい失敗の形なら、errors.toml の FG-AUTH-0012 の diagnosis に `reason` を足し、forge-oidc の `tests/jwt.rs` にテストを足す。
  ログアウトのトークンなら FG-AUTH-0017 と `tests/session.rs`（`logout_token_failures_are_logout_token_invalid`）。

## PFS Keycloak

PFS の Keycloak（realm `iotpf`、Keycloak 26.5.5）は人の claim（`name`、`email`、`locale`、`staff_id`、`plant_code`、`plants`、
`org_units`、`positions`、`position_rank`）とクライアントロール（`resource_access.gitstation.roles`）を**アクセストークンにだけ**
入れ、ID トークンには入れない（クライアントスコープ `iotpf-identity`）。`preferred_username` は `basic` から来るが GitStation は
使わない。groups はない。`email_verified` も、バックチャネルのログアウトもない（ADR-0021 の 2026-11-17 の追記）。

### 設定（この PC）

PFS の Keycloak はこの PC の `http://localhost:18180`（http の loopback なので `ca_file` は付けない、`allow_insecure_issuer` も
要らない）。クライアント `gitstation`（confidential、認可コード + PKCE S256、既定のスコープ `basic` と `iotpf-identity`、
`fullScopeAllowed: false`、クライアントロール `access`）は PFS のセッションが MR で足す。**GitStation の側から Keycloak の
クライアントを作らない。PFS のコンテナやファイルに触らない。**

```toml
[auth.oidc]
issuer = "http://localhost:18180/realms/iotpf"
client_id = "gitstation"
# 秘密は PFS の管理者から受け取り、ソースの外のファイルに置く（所有者だけが読める）。リポジトリに入れない。
client_secret_file = "D:/APP/secrets/gitstation/oidc-client-secret"
employee_number_claim = "staff_id"
access_role = "access"
claims_source = "access_token"
```

同じことを環境変数で: `FORGE_AUTH_OIDC_ISSUER`、`FORGE_AUTH_OIDC_CLIENT_ID`、`FORGE_AUTH_OIDC_CLIENT_SECRET_FILE`、
`FORGE_AUTH_OIDC_EMPLOYEE_NUMBER_CLAIM=staff_id`、`FORGE_AUTH_OIDC_ACCESS_ROLE=access`、`FORGE_AUTH_OIDC_CLAIMS_SOURCE=access_token`。
`web.base_url` + `/-/auth/callback` を、PFS の側でクライアント `gitstation` の Valid redirect URI に入れてもらう。

**Linux のサーバー**: issuer はそのサーバーの PFS の `.env`（Keycloak の公開 URL と realm）から取る。discovery の `issuer` と
**文字どおり**同じにする。https なら `ca_file`（社内 CA）が要る。loopback 以外の http は `FG-SYS-0002 reason=insecure_http` で
起動しない。試験の間だけ `auth.oidc.allow_insecure_issuer = true` で通せる（トークンが平文で流れる。本番では使わない）。

### 前提: `staff_id` は管理者だけが書ける（必ず確かめる）

`claims_source = access_token` では `staff_id` だけで既存の利用者に結び付ける。利用者が自分の `staff_id` を書き換えられると、
他人の GitStation のアカウントを乗っ取れる。PFS の realm で次を確かめる（読み取りだけ）:

- Keycloak の管理画面 → realm `iotpf` → **Realm settings → User profile → `staff_id` → Permissions**:
  **Who can edit = admin だけ**（`user` が入っていない）。Who can view は admin（と必要なら user）。
- realm の定義（PFS のリポジトリの `keycloak/realm/10-iotpf-realm.yaml`）では `staff_id` の `permissions: { view: [admin], edit: [admin] }`。
- 違っていたら GitStation の `claims_source = access_token` を使わず、PFS の管理者に直してもらう（GitStation からは変えない）。
- `forge doctor` の `auth.oidc` は `claims_source=access_token` のとき、このことを行の末尾で思い出させる。
- 結び付ける既存の利用者の種類は `auth.oidc.link_auth_sources`（`local` / `ldap`、既定は両方）で絞れる。

### 何がどう読まれるか

- ID トークン: 署名、`iss`、`aud`、`azp`、`exp`、`nonce` を確かめ、`sub` と `sid` を取る（セッションの `oidc_sub` / `oidc_sid`）。
- アクセストークン: 同じ JWKS と署名方式、`iss` = issuer、`exp` / `nbf` / `iat`（60 秒のずれ）、`azp` = `gitstation`、`typ` が
  あれば `Bearer`、`sub` が ID トークンと同じ。`aud` は Keycloak の流儀（なし・`account`・ほかのクライアント）のどれでもよい。
  ここから `staff_id`、`name`、`email`、`resource_access.gitstation.roles` を読む。トークンも claim もログに出ない。
- 結び付け: `staff_id` が社員番号。大文字と小文字を区別せずに `forge.users.employee_number` と比べる（`staff_id` は
  `[A-Za-z0-9_-]{1,32}` だけ。外れると FG-AUTH-0014 `claim_invalid detail=employee_number_format`）。e-mail の一致も
  `email_verified` も見ない。既にどこかの issuer の OIDC の結び付けを持つ利用者・管理者・`auth.oidc.link_auth_sources`
  （既定 `local,ldap`）にない `auth_source` の利用者には結び付けない（`employee_number_linked` / `privileged_account` /
  `auth_source_not_linkable`）。新しい利用者の username は `staff_id` の小文字、表示名は `name`、社員番号は送られたとおり（大文字）。新しい利用者には `email` が要る（確認済みでなくてよい。ないと FG-AUTH-0014 `email_missing`）。
- アクセストークンの `typ` は必須で `Bearer`（Keycloak は必ず付ける。ID トークンは `ID`）。
- 5 分ごとのリフレッシュの確認でも、ロールはリフレッシュで来たアクセストークンから読む。ロールが外れた・`sub` が変わった・
  アクセストークンが確かめられない（`why=access_token_invalid`）・来ない（`why=access_token_missing`）はセッションを失効させる。
- Keycloak に届かない間もセッションは続くが、ロールの最後の確認から **30 分**を過ぎると次の確認で失効する
  （FG-AUTH-0015 `call=refresh reason=unconfirmed_too_long`、`why=idp_unreachable_too_long`）。Keycloak が戻ったら入り直す。
- バックチャネルのログアウトがないので、Keycloak で切った人はリフレッシュの確認（最長 5 分とその人の次の操作）で切れる。
  無操作（`web.session_idle_secs`）と絶対（`web.session_absolute_secs`）の寿命はそのまま効く。

### 失敗の見分け

1. `request_id` でログを引き、`error_code` と `reason` を読む。
2. FG-AUTH-0012（ID トークン）が先に確かめられる。0012 なら上の「Diagnose」の手順。
3. FG-AUTH-0025:
   - `azp`: 別の PFS アプリのトークン。`auth.oidc.client_id` が `gitstation` か、PFS 側のクライアント名と同じか。
   - `missing_claim claim=sub`: クライアントの既定のスコープに `basic` がない（Keycloak 25 以降、`sub` は `basic` が入れる）。
   - `typ` / `missing_claim claim=typ`: アクセストークンの代わりに別のトークン（ID トークンなど）。Keycloak の設定の誤りか、間に入った何か。
   - `subject_mismatch`: ID トークンとアクセストークンの `sub` が違う。正しい Keycloak なら起きない。続くなら中間者を疑う。
   - `malformed`: アクセストークンが JWT でない（不透明なトークン）。その IdP は `claims_source = access_token` に向かない。
     claim が ID トークンにある IdP なら `claims_source = id_token`（既定）にする。
4. FG-AUTH-0013 `no_access_role`: アクセストークンの `resource_access.gitstation.roles` に `access` がない。PFS の管理者に
   ロールの付与を頼む（`fullScopeAllowed: false` なので、`gitstation` のロールしか入らない）。
5. FG-AUTH-0014 `employee_number_missing`: アクセストークンに `staff_id` がない（PFS の利用者の属性が空か、スコープ
   `iotpf-identity` が外れた）。`claim_invalid detail=employee_number_format`: `staff_id` に許されない文字か 32 文字超。
   `auth_source_not_linkable`: その利用者の `auth_source` が `auth.oidc.link_auth_sources` にない。
   `employee_number_linked`: その利用者は既にどこかの issuer に結び付いている（運用者の `forge admin user link-identity` で決める）。
   `employee_number_ambiguous`: 大文字と小文字だけが違う社員番号の利用者が 2 人いる。データを直す
   （承認の要る手順。runbook oidc-identity-link）。

### Verify

- この PC で PFS の利用者（`access` ロールあり）がログインでき、ログに `forge.auth.login_succeeded`（`outcome=created` か
  `linked`）が出る。`forge doctor` の `auth.oidc` が `ok`。
- その利用者の `forge.users.username` が `staff_id` の小文字、`employee_number` が `staff_id`。
