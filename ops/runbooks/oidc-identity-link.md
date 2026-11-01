# Runbook: oidc-identity-link（FG-AUTH-0014）

最初のログインで、Keycloak の ID（issuer + `sub`）を GitStation の利用者に結べなかった・作れなかったとき（ADR-0021 §4）と、
運用の CLI `forge admin user link-identity` で結ぶ承認制の手順。

## Trigger

- `error_code=FG-AUTH-0014` IdentityLinkConflict（warn、409）。ログの `context` に `event=forge.auth.login_failed` と `reason`:
  - `employee_number_missing`: ID トークンに社員番号の claim（`auth.oidc.employee_number_claim`、既定 `employee_number`）がない。mapper の誤り。
  - `email_missing` / `email_unverified`: `email` がない、`email_verified` が true でない。
  - `email_mismatch`: 社員番号は既存の利用者と一致したが、メールが違う。
  - `privileged_account`: 社員番号が管理者のもの（管理者は自動では結ばない）。
  - `employee_number_linked`: その利用者は、この IdP の別の `sub` に結ばれている。
  - `auto_link_disabled`: `auth.oidc.auto_link = false` で、社員番号が既存の利用者のもの。
  - `username_unavailable`: `preferred_username` がない・規則に合わない・予約語・使用中・墓標と重なる。
  - `email_taken`: そのメールは別の利用者のもの。
  - `employee_number_taken`: 同時のログインが先に利用者を作った（もう一度ログインすれば `existing` になる）。同じ人の初回のログインが
    2 つ同時に走ると（二重のクリック、2 つのタブ）、負けた方は `employee_number_taken`・`username_unavailable`・`email_taken` の
    どれか（先に当たった一意の制約による）になる。勝った方で利用者と結び付けはできているので、**もう一度ログインすれば通る**。
    同じ時刻・同じ `employee_number` の成功（`forge.auth.login_succeeded`）があれば、この場合で、対応は要らない。
  - `claim_invalid`（`detail`）: claim が利用者の表の規則に合わない。
  - CLI の `subject_linked`（その ID は別の利用者のもの）、`already_linked`（その利用者は既にこの IdP の別の ID を持つ）。

## Impact

- その人だけがログインできない。何も書かれていない。ほかの利用者、`git`、トークンは影響を受けない。

## Diagnose（読み取りだけ）

1. `request_id` でログを引き、`reason` を読む（claim の値はログにない）。
2. 本人に社員番号と Keycloak のユーザー名を確かめ、Keycloak の管理画面で `sub`（ユーザー ID）、社員番号の属性、メール、`email_verified` を見る。
3. GitStation の利用者を見る（読み取りのロール `forge_ro`）:
   `SELECT id, username, kind, state, is_admin, employee_number, email FROM forge.users WHERE employee_number = '<社員番号>' OR lower(email) = lower('<メール>');`
   `SELECT user_id, issuer, linked_at, last_login_at FROM forge.user_identities WHERE user_id = <id>;`（`forge_ro` は `subject` を読めない）
4. 誰の ID かを決める。**メールだけが一致する利用者には結ばない**（乗っ取りの防止、ADR-0021 §4）。

## Mitigate

- mapper の誤り（`employee_number_missing`、`email_unverified`）: Keycloak の管理者に直してもらう（ADR-0021 §13）。GitStation では何もしない。
- 管理者、`auto_link = false`、メールの不一致を確かめたうえでの結び付け: **承認制の手順**として次を実行する。

  `[MUTATING — needs approval]`
  ```
  forge admin user link-identity --user <username> --issuer <auth.oidc.issuer と同じ文字列> --subject <Keycloak の sub>
  ```
  - actor は `system`、監査の行 `user.identity_linked`（`via=operator`、`actor_label = forge-cli:<OS のアカウント>`）。`sub` は監査に入らない。
  - `authorize()` を通らない運用のコマンド（CLAUDE.md 契約 4 の例外、ADR-0019 §8）。HTTP からは到達できない。
  - 同じ利用者・同じ ID ならもう一度実行しても何も書かない（`already linked`）。
- 結び直し（別の `sub` に付け替える）は今はできない（`forge_app` に DELETE がない）。必要になったら追記のマイグレーションと一緒に手順を足す。

## Verify

- 本人がログインでき、ログの `forge.auth.login_succeeded` の `outcome` が `existing`（または `linked`）。
- 監査: `SELECT created_at, actor_label, details FROM forge.audit_log WHERE action = 'user.identity_linked' ORDER BY id DESC LIMIT 5;`

## Feedback

- 同じ `reason` が何人も出るなら、Keycloak の mapper か `auth.oidc.*` の設定の誤り。インシデントの記録に残し、設定を直す。
