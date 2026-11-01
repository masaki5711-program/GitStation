# Runbook: keycloak-unreachable（FG-AUTH-0015）

GitStation から Keycloak（OIDC の IdP）に届かないとき（ADR-0021 §11）。

## Trigger

- `error_code=FG-AUTH-0015` IdentityProviderUnavailable（**alert**、503）。ログの `context` に `call`（`discovery`、`jwks`、`token`）と `reason`:
  `connect_timeout`、`timeout`（`timeout_ms`）、`connect`、`tls`、`status`（`status`）、`redirect`、`body_too_large`、`body`、`request`、
  `invalid_json`（`detail`）、`endpoint_origin`（`endpoint`）、`issuer_mismatch`。
- 起動のときの `forge.auth.discovery_failed`（warn、3 回まで）。起動は止まらない。
- `forge doctor` の `auth.oidc` が `FAIL`。

## Impact

- **新しいログインだけ**ができない。ログイン済みのセッション、`git`、トークンの API、`gstn` は動き続ける（`/readyz` は Keycloak を見ない）。
- 何も書かれていない。

## Diagnose（読み取りだけ）

1. `forge doctor`（サーバーのホストで）: discovery、JWKS、時計のずれ。
2. ホストから: `curl --cacert <auth.oidc.ca_file> --noproxy '*' -sS -o /dev/null -w '%{http_code}\n' <issuer>/.well-known/openid-configuration`
   （forge は環境変数のプロキシを使わない。同じ条件で試す）。
3. 理由ごとに:
   - `connect` / `connect_timeout` / `timeout`: Keycloak の稼働、DNS、ファイアウォール、経路。
   - `tls`: `auth.oidc.ca_file` が Keycloak の証明書の発行元の CA か、証明書の期限、ホスト名。
   - `status`: Keycloak の 5xx（Keycloak 側の障害）、404（issuer の realm 名の誤り）。
   - `redirect`: issuer の URL が古い（Keycloak がリダイレクトしている）。正しい URL に設定を直す。
   - `endpoint_origin` / `issuer_mismatch`: discovery の文書が issuer と別のオリジン・別の issuer を示した。Keycloak の
     `frontendUrl` / hostname の設定の誤りか、途中の改ざん。設定を合わせるまでログインは止めたままにする（資格情報を別のホストへ送らないため）。
   - `body_too_large` / `invalid_json`: Keycloak 以外のもの（プロキシのエラーページ）が答えている。

## Mitigate

- GitStation の中で直すものはない。Keycloak の担当（IoT プラットフォーム）に連絡し、復旧を待つ。
- 設定の誤り（issuer、CA）なら設定を直して再起動。`[MUTATING — needs approval]`
- 急ぎで作業が必要な人には `gstn` と個人アクセストークンを案内する（ブラウザのログインなしで使える）。

## Verify

- `forge doctor` の `auth.oidc` が `ok`。ブラウザでログインでき、`forge.auth.login_succeeded` が出る。
- 起動時の discovery に失敗していた場合も、最初のログインで取り直す（再起動は要らない）。

## Feedback

- 障害の記録（`ops/incidents/`）に、時間帯と `reason` を残す。新しい `reason` なら errors.toml の FG-AUTH-0015 に足す。
