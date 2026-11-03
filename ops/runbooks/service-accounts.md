# Runbook: service-accounts（FG-AUTH-0019 / FG-AUTH-0010 ownerless_service_account）

サービスアカウント（連携や CI の報告に使う、人ではないアカウント）の作成・トークンの発行・更新（期限の前の差し替え）・漏えいのときの失効・持ち主の引き継ぎの手順（M3 S0、計画 11 §5 S0、ADR-0019 Addendum S0）。

サービスアカウントは `forge.users` の `kind = service_account` の行と、`forge.service_account_profiles` の行（持ち主 = 人、目的、作った管理者）でできている。

- 個人の情報（メール・社員番号・外部 ID）を持たず、管理者にならず、IdP でサインインしない（`users_service_account_no_personal_data`、`users_service_account_not_admin`、`user_identities_human_only`）。
- **メンバーになったプロジェクトだけ** が見える（internal のプロジェクトもメンバーでなければ見えない）。読み書きはそのメンバーのロールで決まる。
- その個人の名前空間には、誰も（管理者も）プロジェクトを作れず、移せない（`non_person_namespace`）。
- 目的ごとに 1 つ作る（kanflow 用、CI の報告用、…。計画 11 Q4）。

## Trigger

- 新しい連携（kanflow、CI の報告など）のためにアカウントが要る。
- `forge doctor` の `service_accounts.tokens_expiring` が `FAIL ... FG-AUTH-0019`（warn）: 有効なサービスアカウントの有効なトークンが、すべて 14 日以内に切れる
  （`expiring_service_accounts`、`user_ids`、`soonest_expiry`、`within_days=14`）。トークンの寿命は最長 365 日。
- `forge doctor` の `service_accounts.ownerless` が `FAIL ... FG-AUTH-0010 reason=ownerless_service_account`（warn）: 有効なサービスアカウントの持ち主が
  無効化された（退職・異動の引き継ぎ漏れ）、またはプロフィールが無い（`ownerless_service_accounts`、`user_ids`）。
- サービスアカウントのトークンが漏れた・漏れた疑いがある（ログ・チャット・リポジトリに `fgp_…` が出た）。
- 連携が `FG-AUTH-0001`（トークンが無効・期限切れ）で止まった。

## Impact

- 期限切れ: 最後のトークンが切れると、その連携は止まる（kanflow の同期、CI の結果の報告）。データは失われない。
- 持ち主がいない: トークンの差し替え・失効を決める人がいない。アカウントはそのまま動く。
- 漏えい: そのトークンのスコープとロールの範囲で、誰でも操作できる（メンバーのプロジェクトだけ。管理者の権限は持てない）。**すぐに失効させる**。

## 本番での実行の承認

次の操作は本番のデータを変える。本番では **人が承認した手順としてだけ** 実行する（CLAUDE.md §3.7）。各手順に `[MUTATING — needs approval]` と書く。

- サービスアカウントの作成（`POST /api/v1/admin/service-accounts`）、トークンの発行（`POST /api/v1/admin/service-accounts/{id}/tokens`）
- `gstn admin service-account update`、`gstn admin service-account revoke-token`、`gstn user disable`
- （運用の経路、参考）`forge admin token issue` / `forge admin token revoke`

**作成とトークンの発行は管理者の Web セッションだけ**（トークンからは `FG-AUTH-0002 reason=session_required` で必ず拒否。トークンがトークンを作れないように
するため）。管理画面はまだ無い（後続の MR）。それまでは管理者がブラウザでサインインし、同じタブの開発者ツールのコンソールから API を呼ぶ（下の A・B）。
Web セッションの CSRF の値は `GET /api/v1/session` の `csrf_token` で、ヘッダー `X-GitStation-CSRF` で送る。

## 導入の前の確認（マイグレーション 20261113100000 を当てる前に一度）

20261113100000 の `users` の CHECK（`users_service_account_no_personal_data`、`users_service_account_not_admin`）は **既存の行も検証する**。
違反する行が 1 つでもあると、マイグレーションは全体が失敗する（何も適用されない）。`forge db migrate` の前に、対象の DB で読み取りで確かめる。
どちらも 0 でなければ、先に該当のサービスアカウントの扱いを決める（人に変える・個人の情報を消すのは承認した手順で）。

```sql
SELECT count(*) FILTER (WHERE email IS NOT NULL OR employee_number IS NOT NULL
         OR external_id IS NOT NULL) AS with_personal_data,
       count(*) FILTER (WHERE is_admin) AS administrators
FROM forge.users WHERE kind = 'service_account';
```

20261113100100 から、利用者の `kind` は変えられない（`users_kind_immutable`。変えようとすると SQLSTATE 23514）。

## Diagnose（読み取りのみ）

1. 一覧と、各アカウントの持ち主・有効なトークンの数・いちばん遅い期限を見る（`admin` スコープのトークン）。

   ```sh
   gstn admin service-account list --json
   gstn admin service-account view <id> --json      # owner、purpose、active_tokens、last_token_expiry
   gstn admin service-account tokens <id> --json    # 各トークンの expires_at、revoked_at、last_used_at、issued_by_user_id
   ```

2. doctor の行を見る。

   ```sh
   forge doctor
   ```

   `service_accounts.tokens_expiring` の `user_ids` と `soonest_expiry`、`service_accounts.ownerless` の `user_ids` が対象。名前は出ない（id だけ）。

3. 漏えいのとき: 漏れたトークンの `public_id`（`fgp_<public_id>_…` の真ん中の 16 桁）から、トークンの id と使われ方を調べる。

   ```sql
   SELECT id, user_id, name, scopes, created_at, expires_at, revoked_at, last_used_at, issued_by_user_id
   FROM forge.tokens WHERE public_id = '<public_id>';
   SELECT occurred_at, action, outcome, resource_type, resource_id, request_id
   FROM forge.audit_log WHERE credential_id = <token id> ORDER BY id DESC LIMIT 100;
   ```

## Mitigate

### A. 作成 `[MUTATING — needs approval]`（ロールバック: 削除は無い。不要になったら D の無効化）

1. 持ち主（その連携に責任を持つ、有効な人）の user id を決める（`gstn user search <名前>`）。目的は 1 行で書く（最大 500 文字）。
2. 管理者がブラウザでサインインし、開発者ツールのコンソールで実行する。

   ```js
   const s = await (await fetch('/api/v1/session')).json();
   const r = await fetch('/api/v1/admin/service-accounts', {
     method: 'POST',
     headers: { 'content-type': 'application/json', 'X-GitStation-CSRF': s.csrf_token },
     body: JSON.stringify({ username: 'kanflow-sync', owner_id: <owner id>, purpose: 'Kanflow が結んだプロジェクトのイベントを読む' }),
   });
   console.log(r.status, await r.json());
   ```

   `201` と、`id`・`owner`・`created_by` が返る。持ち主が人でない・無効なら `FG-AUTH-0006`（`owner_not_human` / `owner_disabled`）で何も書かない。
3. 連携が使うプロジェクトに、必要な最小のロールでメンバーとして加える（`gstn member add --project <id> --user-id <サービスアカウントの id> --role reporter`）。
   internal のプロジェクトでも、メンバーでなければ見えない。

### B. トークンの発行 `[MUTATING — needs approval]`（ロールバック: C の失効）

1. 必要なスコープを決める（読むだけなら `read_api`、clone なら `read_repository`。`admin` は付けられない）。期限は最長 365 日（既定 90 日）。
2. 管理者のブラウザのコンソールで実行する。

   ```js
   const s = await (await fetch('/api/v1/session')).json();
   const r = await fetch('/api/v1/admin/service-accounts/<id>/tokens', {
     method: 'POST',
     headers: { 'content-type': 'application/json', 'X-GitStation-CSRF': s.csrf_token },
     body: JSON.stringify({ name: 'kanflow-2026-11', scopes: ['read_api'], expires_in_days: 180 }),
   });
   console.log(r.status, await r.json());
   ```

   `201` の `token` が秘密で、**この 1 回しか出ない**。持ち主に安全な経路で渡し、連携の秘密のファイルに入れてもらう。チャット・メール・チケットに貼らない。
   `issued_by_user_id` に発行した管理者が残る（監査の `token.created` にも `issued_by_user_id`）。無効化したアカウントには発行できない（`target_disabled`）。

### C. 期限の前の差し替え（FG-AUTH-0019）

1. B の手順で新しいトークンを発行する。`[MUTATING — needs approval]`
2. 持ち主が連携の設定を新しいトークンに替え、連携が動くことを確かめる（`gstn admin service-account tokens <id> --json` で新しいトークンの `last_used_at` が入る）。
3. 古いトークンを失効させる。`[MUTATING — needs approval]`（ロールバック: 失効は戻せない。必要なら新しく発行する）

   ```sh
   gstn admin service-account revoke-token <id> <古いトークンの id> --yes
   ```

**365 日の上限より前に** 差し替える（doctor は 14 日前から警告する）。

### D. 漏えい（すぐに）

1. 漏れたトークンを失効させる。`admin` スコープのトークンでも実行できる（ブラウザを開くのを待たない）。`[MUTATING — needs approval]`（ロールバック: なし）

   ```sh
   gstn admin service-account revoke-token <id> <トークンの id> --yes
   ```

   すでに失効していても `204`。別の利用者のトークンの id を指定すると `FG-REPO-0005`（そのアカウントのトークンではない）。
2. アカウントごと止めるとき（どのトークンが漏れたか分からない、アカウントが乗っ取られた疑い）: 無効化する。有効なトークンはすべて同じ変更で失効する
   （`revoked_reason = 'user_disabled'`）。`[MUTATING — needs approval]`（ロールバック: 無効化を戻すコマンドはまだ無い。新しいアカウントを作る）

   ```sh
   gstn user disable <id> --yes
   ```

3. Diagnose の手順 3 で、漏れてから失効までに何をされたかを調べ、事案として記録する（`/incident`）。新しいトークンは B の手順で発行する。

### E. 持ち主の引き継ぎ（FG-AUTH-0010 ownerless_service_account、退職・異動）

1. 新しい持ち主（有効な人）を決め、引き継ぐ。`[MUTATING — needs approval]`（ロールバック: もう一度 `--owner` で戻す）

   ```sh
   gstn admin service-account update <id> --owner <新しい持ち主の user id>
   ```

   `service_account.updated` の監査の行に `owner_from` / `owner_to` が残る。人でない・無効な利用者は持ち主にできない（`FG-AUTH-0006`）。
2. もう要らないアカウントなら、引き継がずに D の手順 2 で無効化する。

## Verify

- `forge doctor` の `service_accounts.tokens_expiring` と `service_accounts.ownerless` が `ok`。
- 失効させたトークン: `gstn admin service-account tokens <id> --json` で `revoked_at` がある。無効化したアカウント: `gstn admin service-account view <id>` の
  `state` が `disabled`、`active_tokens` が 0。
- 監査の記録に、手順ごとに 1 行ある（`service_account.created`、`service_account.updated`、`token.created`、`token.revoked`、`user.disabled`）。

## Escalate

- 漏えいしたトークンで書き込みが行われていた: `/incident` で事案にし、影響したプロジェクトの Owner に連絡する。
- `service_accounts.ownerless` が、持ち主の無効化の記録なしに出る: データが forge-db の外で変えられた疑い。Runbook `append-only-violation` を確かめ、DB の管理者に連絡する。
- 管理画面ができるまでの作成・発行の手順（ブラウザのコンソール）が使えない環境: 運用の経路 `forge admin token issue --user <username>`（承認した手順。発行者は
  記録されない）で一時的にしのぎ、管理画面の MR を急ぐ。
