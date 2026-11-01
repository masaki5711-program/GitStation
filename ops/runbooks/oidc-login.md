# Runbook: oidc-login（FG-AUTH-0012 / FG-AUTH-0011 / FG-AUTH-0013 / FG-AUTH-0018）

ブラウザのログイン（Keycloak の OIDC、ADR-0021 §3・§9）が失敗するときの診断。MR5 で FG-AUTH-0017（ログアウトのトークン）もここに入る。

## Trigger

- `error_code=FG-AUTH-0012` IdTokenInvalid（warn、401）。ログの `context` に `event=forge.auth.login_failed` と `reason`:
  `malformed`、`alg_not_allowed`（`alg`）、`kid_missing`、`unknown_kid`、`key_invalid`（`detail`）、`key_alg_mismatch`（`alg`、`kty`）、
  `signature`、`issuer`、`audience`、`azp`、`expired`、`not_yet_valid`（`claim`）、`nonce`、`subject`、`missing_claim`（`claim`）。
- 利用者から「ログインできない」の問い合わせ（`request_id` 付き）。FG-AUTH-0011（state の誤り）、FG-AUTH-0013（入場の拒否）、
  FG-AUTH-0018（`auth.oidc` がない）も同じ手順で見分ける。

## Impact

- 新しいログインだけが失敗する。ログイン済みのセッション、`git`、トークンの API、`gstn` は影響を受けない。
- 何も書かれていない（利用者・結び付け・セッションのどれも）。

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

## Mitigate

- 設定の誤り（issuer、client id、signing_algs）: 設定を直して再起動する。`[MUTATING — needs approval]`（本番の設定の変更）。
- 時計: NTP を直す（ホストの運用の手順）。
- Keycloak 側（ロール、鍵、mapper）: IoT プラットフォームの Keycloak の管理者に依頼する（ADR-0021 §13 の求める形）。

## Verify

- 同じ利用者でログインが通り、ログに `forge.auth.login_succeeded`（`user_id`、`outcome`、`public_id`）が出る。
- `forge doctor` の `auth.oidc` が `ok`。

## Feedback

- 新しい失敗の形なら、errors.toml の FG-AUTH-0012 の diagnosis に `reason` を足し、forge-oidc の `tests/jwt.rs` にテストを足す。
