# Runbook: oidc-login（FG-AUTH-0012 / FG-AUTH-0025 / FG-AUTH-0028 / FG-AUTH-0011 / FG-AUTH-0013 / FG-AUTH-0014 / FG-AUTH-0017 / FG-AUTH-0018 / FG-INT-0025 / FG-INT-0026 / FG-INT-0027 / FG-INT-0028 / FG-AUTH-0030）

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
- `error_code=FG-INT-0025`〜`FG-INT-0028`（PFS の組織の台帳の写し、ADR-0038 §1）: 下の「組織の台帳の写し」を見る。
- 「組織の結び付けで入れたはずの人が入れない」「`FG-AUTH-0029`〜`FG-AUTH-0031` で結び付けを足せない」（ADR-0038 §2）:
  下の「組織による所属（結び付け）」を見る。
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
GitStation 自身も http で開く試験サーバー（`ops/gitlab-trial/`）では、`web.allow_insecure_http = true` も要る（クッキーの名前が
`gstn_<port>_session` / `gstn_<port>_oidc` になり `Secure` が付かない。ADR-0021 Amendment 2026-11-20）。

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

## staff_id 未登録

FG-AUTH-0014 `reason=employee_number_missing`（`auth.oidc.claims_source = access_token`）: その人の PFS のアカウントに
社員番号（`staff_id`）がない。利用者の画面には「社員番号が未登録なら PFS の利用者管理の担当に登録を依頼する」旨と
問い合わせ番号だけが出る（理由ごとの文言は持たない。ログの `reason` で見分ける）。

1. `request_id` でログを引き、`forge.auth.login_failed` の `reason=employee_number_missing` を確かめる（値はログに出ない）。
2. PFS の管理者に、その人の `staff_id` の属性が空でないか、クライアントの既定のスコープに `iotpf-identity` があるかを確かめてもらう。
   GitStation 側で直すものはない（社員番号は PFS の管理者だけが書ける属性。GitStation では書き換えない）。
3. 登録されたら、その人がもう一度ログインする。`forge.auth.login_succeeded`（`outcome=created` か `linked`）を確かめる。
4. 何人も同時に出るなら、スコープ `iotpf-identity` のマッパーの変更を疑う（PFS 側の MR を確かめる）。

## PFS の組織の claim

ADR-0021 の追記「組織の claim の写し（2026-11-20）」。`claims_source = access_token` のとき、ログインと 5 分ごとの確認のたびに、
アクセストークンの `org_units`・`positions`・`position_rank`・`plants`・`plant_code` を確かめて `forge.user_org_profiles` に
1 人 1 行で写す。**表示だけ**に使い、権限の判定には使わない（Phase 2 で検討）。

- 同じ値: `confirmed_at` だけ進む（監査なし）。変わった: 値を書き、監査 `user.org_profile_changed`（変わった項目の前後）。
- 規則に合わない値: **ログインは止めない**。前の正しい値を残し `invalid_since` を立て、FG-AUTH-0028
  （`forge.auth.org_claims_invalid`、warn）を 1 回だけ出す。正しい値が来ると `invalid_since` は消える。
- claim がまったくない: 空の写し（正しい扱い）。

### 見分け方（読み取りだけ）

1. `forge doctor` の `auth.oidc.org_claims`: 写しの数、`invalid_since` のある数、いちばん古い `confirmed_at`。invalid が 1 以上で warn。
2. ログ: `error_code=FG-AUTH-0028` の `user_id`、`field`（org_units / positions / position_rank / plants / plant_code）、
   `reason`:
   - `format`: 値が形に合わない（部署コード `^[A-Z0-9][A-Z0-9_-]{0,19}$`、役職 `^[a-z][a-z0-9_]{0,63}$`、ランク 0〜999 の整数、
     工場 `^[A-Z0-9]{1,4}$`）。
   - `too_many`: 一覧が 64 件を超える。
   - `multiple_primary`: 主の部署が 2 つ以上。
   - `type`: JSON の型が違う（`org_units` の文字列が JSON でない、一覧のはずがオブジェクト、など）。
   値そのものはログに出ない。
3. DB（読み取りだけ）:
   `SELECT user_id, invalid_since, confirmed_at FROM forge.user_org_profiles WHERE invalid_since IS NOT NULL;`

### 直し方

- その人の PFS の属性（組織のマスター、役職、工場）を PFS 側で直してもらう。GitStation では何も直さない。
  次のログインか 5 分ごとの確認で `invalid_since` が消える。
- 多くの人で同時に起きたら、`iotpf-identity` のマッパーの形が変わった可能性。PFS 側の変更を確かめる。
  `org_units` の各値は `{"code": "...", "primary": true}` の JSON の文字列（またはオブジェクト）が前提。

### 見え方

- 本人: アカウントの設定の「所属」と、ユーザーメニュー（部署は主を先頭に、文字「主」で示す。工場）。
- ほかの利用者: 人を選ぶ画面と MR の承認者の一覧に**主の部署コードだけ**。役職とランクは本人と管理者だけ
  （`GET /api/v1/admin/users/{id}/org-profile`、`UserOrgAdminRead`）。

## 組織の台帳の写し

ADR-0038 §1（段 A）。worker の `org_master_sync` が PFS registry-api の組織の台帳
（`GET {portal.registry_url}/api/v1/org/units` と `/api/v1/org/positions`、全工場）を、GitStation のクライアント
（`auth.oidc.client_id`）の client credentials のトークンで読み、`forge.org_units_master` と `forge.org_positions_master` に写す。
写しは部・課の **名前** の表示に使い、後の段（組織による所属・承認者の指定）では、利用者の主所属から課・部を
**親と `level` をたどって** 求める（コードの形からは推測しない）。

- 設定（`[org]`）: `org.master_sync_enabled`（既定 false）、`org.master_sync_interval_secs`（既定 900）、
  `org.master_timeout_ms`（既定 5000）、`org.master_max_age_hours`（既定 24）、`org.max_profile_age_hours`（既定 24）。
  registry の URL と CA は `portal.registry_url`・`portal.ca_file`（ポータルの新着と同じ）。`auth.oidc` が無いと worker は起動しない
  （FG-SYS-0002 `config_key=org.master_sync_enabled reason=needs_auth_oidc`）。
- PFS 側の前提: registry-api の `ORG_READER_CLIENTS` に `gitstation` があること（PFS の MR !28）。人のトークンは 403。
- 同期は 1 つのトランザクションで置き換える（一覧に無くなった行は消える）。何か変わったときだけ監査 `org.master_synced`
  （件数だけ。名前や社員番号は残さない）。失敗したら **何も書かず、前の写しを残す**。次の試行は 1 分、2 分、… と倍にして
  `org.master_sync_interval_secs` まで待つ。
- 組織の一覧が空の応答は受け付けない（FG-INT-0027 `reason=empty`）。registry の障害で写しが消えることはない。
- 書く前に一覧の全体を確かめる: 親（`parentCode`）が同じ工場の組織に無い（`missing_parent`）、親の `level` が同じか下
  （`level_order`）は拒否。さらに `org.allow_large_shrink`（既定 false）でない限り、急な縮小も拒否する: 組織の半分より多くが
  消える（`shrink`、`before`・`after`・`removed`）、今ある工場の組織か職位が 0 件になる（`plant_emptied`、`list`・`plant`）、
  職位が 1 件も無い（`positions_empty`）。どれも FG-INT-0027 で、前の写しを残す。
- 変わった行だけを書き、同期の時刻は 1 行の表 `forge.org_master_state` に残す。worker が複数でも同期は 1 つずつ
  （advisory lock。取れなければ `forge.org.master_sync_busy` を info で出して飛ばす）。
- 写しが `org.master_max_age_hours` より古い（または一度も同期していない）と、組織から求めるもの（課・部）は
  **「不明」** として閉じる（ADR-0038 §4）。表示は写しの名前を使い続け、写しに無いコードはコードのまま出す。

### 見分け方（読み取りだけ）

1. `forge doctor` の `org.master`: 組織と職位の件数、最後の同期の時刻と経過秒。古い・未同期は FG-INT-0028（alert）。
2. ログ `forge.org.master_sync_failed`（`list` = units / positions、`reason`、`status`、`failures`。トークンと名前は出ない）:
   - FG-INT-0025（warn）: 届かない。`reason` = `connect` / `connect_timeout` / `timeout` / `redirect` / `request` /
     `status`（5xx など）/ `service_token`（トークンを取れない。`detail` に FG-INT-0024 の理由）。
   - FG-INT-0026（alert）: 401 / 403 / 404。`ORG_READER_CLIENTS` に `gitstation` が無い、別のレルム・クライアントのトークン、
     registry に組織の API がまだ無い（404）。
   - FG-INT-0027（alert）: 中身が契約に合わない。`reason` = `envelope` / `too_many` / `field`（`field` と `index`）/
     `self_parent` / `duplicate` / `item` / `empty`。値そのものはログに出ない。
   - 回復: `forge.org.master_sync_recovered`。変化: `forge.org.master_synced`（件数）。
   - 写しが古くなった: `forge.org.master_stale`（FG-INT-0028、失敗が続く間に 1 回）。
3. DB（読み取りだけ）:
   `SELECT synced_at, units, positions FROM forge.org_master_state;`（最後に成功した同期）
   `SELECT plant_code, code, level, parent_code FROM forge.org_units_master WHERE parent_code IS NOT NULL
    AND NOT EXISTS (SELECT 1 FROM forge.org_units_master p WHERE p.plant_code = org_units_master.plant_code
    AND p.code = org_units_master.parent_code);`（親が台帳に無い組織。その下の人の課・部は「不明」になる）

### 直し方

- FG-INT-0026: PFS の運用者に `ORG_READER_CLIENTS=gitstation` の設定を頼む（GitStation 側の変更は不要。次の試行で直る）。
  `auth.oidc.client_id` が realm `iotpf` の `gitstation` であることも確かめる。
- FG-INT-0025: registry の URL（`portal.registry_url`）、CA（`portal.ca_file`）、ネットワーク、PFS 側の稼働を確かめる。
  `service_token` なら runbook `portal-widgets` の FG-INT-0024 と同じ手順。
- FG-INT-0027: PFS の台帳の該当する値（`field` と `index`）を PFS 側で直してもらう。契約が合意の上で変わったなら、
  GitStation の解析（forge-integ `org_master`）を直す。
- FG-INT-0027 `shrink` / `plant_emptied` / `positions_empty`: PFS の運用者に、本当の組織改編（工場の閉鎖など）か
  を確かめる。本当なら（承認された手順として）`org.allow_large_shrink = true` で worker を再起動し、1 回の同期が通ったら
  （`forge.org.master_synced`、監査 `org.master_synced` の `allow_large_shrink: true`）すぐ false に戻して再起動する。
  本当でなければ PFS 側のデータを直してもらう。
- 同期を止めたいとき（承認された手順として）: `org.master_sync_enabled = false` にして worker を再起動する。写しは残るが、
  古くなると組織から求めるものは「不明」になる。

### 見え方

- 本人の「所属」: 写しにある部署は「部 / 課 / 職場」の名前とコード。無いものはコードだけ。
- 人を選ぶ画面と MR の承認者の一覧: 主所属の「部 / 課」の名前（画面の言語。英語・インドネシア語の名前が無ければ日本語）。
  写しに無ければ今までどおりコード。その人の写しが不正・古い、または台帳の写しが古いときは、名前の横に
  「最終確認時点」と出す（API の `stale: true`。判定には使わない）。社員番号（`head_staff_id`）は読み取りのプール（`forge_ro`）からも見えない。

## 組織による所属（結び付け）

ADR-0038 §2（段 B）。グループ・プロジェクトに「この組織（配下を含む）の人は Reporter / Developer」という結び付け
（`forge.org_bindings`）を置く。実効ロールは「手で付けたメンバーのロール（グループからの継承を含む）」と「当てはまる
結び付けのロール」の高い方。**結び付けで付くのは Developer まで**（Maintainer・Owner・配備の承認は手で付ける）。

- 管理: メンバーの画面の「組織による所属」。足す・外すはそのグループ・プロジェクトの Owner と管理者だけ、web のセッション
  だけ（`OrgBindingManage`。トークンは FG-AUTH-0002 `session_required`）。変更は監査 `org_binding.created` /
  `org_binding.deleted`（グループかプロジェクトに。理由は文字数だけ）。一覧は `gstn member org-bindings --project <id>`。
- 当てはまる人: 組織の台帳の写しで、その人の所属（主所属と兼務。`membership = primary` なら主所属だけ）から **親と
  `level` をたどって** 結び付けの組織に届くこと（`include_descendants = false` ならその組織そのものだけ）。`min_rank` が
  あれば職位ランクがそれ以上。
- **閉じる条件**（ADR-0038 §4。どれかなら結び付けは何も与えない。手で付けたメンバーはそのまま効く）:
  写しが無い・`invalid_since` がある・`org.max_profile_age_hours` より古い（個人トークン・エージェントだけで働く人は
  24 時間に 1 回 web でサインインが要る）、台帳の写しが `org.master_max_age_hours` より古い（FG-INT-0028）、その所属が
  台帳に無い・まだ有効でない・親のたどりが切れている。
- 判定は今までどおり `authorize()`。ロールを読む SQL（`forge_db::roles` の `chain_role_columns!`）が
  `forge.org_binding_role()` を呼び、上の条件をそこで確かめる。年齢の上限は `forge serve` / `forge worker` の起動時に
  設定から `forge.org_access_policy` に写される（設定を変えたら再起動する）。

### 見分け方（読み取りだけ）

1. 「入れない」: メンバーの画面の「組織から入っている人」にその人がいるか（いなければ結び付けが効いていない）。
2. その人の写し（管理者の画面、または DB）:
   `SELECT invalid_since, confirmed_at, plant_code FROM forge.user_org_profiles WHERE user_id = <id>;`
   `invalid_since` がある（FG-AUTH-0028）、`confirmed_at` が古い、`plant_code` が無い → 本人に web で一度サインインしてもらう。
3. 台帳の写しの古さ: `forge doctor` の `org.master`（上の「組織の台帳の写し」）。
4. 結び付けと条件:
   `SELECT id, plant_code, org_code, include_descendants, membership, min_rank, role FROM forge.org_bindings
    WHERE target_kind = 'project' AND target_id = <id>;`
   その人の所属が結び付けの組織の配下か: 上の「組織の台帳の写し」の親のたどり（`parent_code`）で確かめる。
5. 年齢の上限: `SELECT * FROM forge.org_access_policy;`（設定の `org.*_age_hours` と違えば、プロセスの再起動が済んでいない）。
6. 足せない:
   - FG-AUTH-0029: `reason` = `plant_code_invalid` / `org_code_invalid` / `min_rank_out_of_range` / `reason_invalid` /
     `role_not_allowed`（guest）/ `duplicate`（同じ組織・範囲・条件が既にある）/ `too_many`（100 件）。
   - FG-AUTH-0030: `reason` = `not_in_master` / `not_yet_valid` / `master_missing`（台帳の写しが一度も同期していない）。
   - FG-AUTH-0031: `role_above_cap`（Maintainer・Owner は結び付けでは付けない）。

### 直し方

- 本人の写しが古い・不正: 本人に web で一度サインインしてもらう（5 分ごとの更新で新しくなる）。不正が続くなら上の
  「PFS の組織の claim」。
- 台帳の写しが古い: 上の「組織の台帳の写し」の直し方。
- 条件の誤り: Owner が結び付けを外して、正しい条件で足し直す（変更はできない。外して足す）。
- 結び付けを今すぐ全部止めたい（事故のとき、承認された手順として）: Owner が画面で外す。DB を直接書かない。

### 見え方

- メンバーの画面: 手で付けたメンバーの表とは別に「組織から入っている人」（「組織から（<部・課の名前>）」）。ここでは外せない。
- 結び付けの一覧: 組織の名前とコード、範囲（配下を含む・主所属だけ・職位ランク）、ロール、理由。台帳に無くなった組織は
  「台帳にありません」と出る（その結び付けは何も与えない）。
