# Runbook: oidc-login（FG-AUTH-0012 / FG-AUTH-0011 / FG-AUTH-0013 / FG-AUTH-0017 / FG-AUTH-0018）

ブラウザのログイン（Keycloak の OIDC、ADR-0021 §3・§9）が失敗するとき、バックチャネルのログアウト（FG-AUTH-0017、ADR-0021 §8）が
断られるとき、5 分ごとのリフレッシュの確認でセッションが切れるとき（ADR-0021 §7、M2 MR5）の診断。

## Trigger

- `error_code=FG-AUTH-0012` IdTokenInvalid（warn、401）。ログの `context` に `event=forge.auth.login_failed` と `reason`:
  `malformed`、`alg_not_allowed`（`alg`）、`kid_missing`、`unknown_kid`、`key_invalid`（`detail`）、`key_alg_mismatch`（`alg`、`kty`）、
  `signature`、`issuer`、`audience`、`azp`、`expired`、`not_yet_valid`（`claim`）、`nonce`、`subject`、`missing_claim`（`claim`）。
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
