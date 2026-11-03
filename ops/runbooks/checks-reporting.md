# Runbook: checks-reporting（FG-DB-0022 / FG-REVIEW-0020 / FG-REVIEW-0021 / 報告できない必須のチェック）

Checks API（M3 C1、ADR-0024 §10、`docs/api/checks.md`）で、CI や kanflow のサービスアカウントがコミットにチェックの結果を報告する。
保護ブランチのマージの条件（必須のチェック）は、MR の **今の head** にその名前の `completed` / `success` の報告があるときだけ満たされる。

- 報告は `POST /api/v1/projects/{id}/commits/{sha}/checks`（`gstn checks report`）。同じコミット・同じ名前の報告は **1 行を上書きする**（PATCH は無い）。
- 名前は `<名前空間>/<名前>`。`ci/` と `kanflow/` は **名前空間を割り当てられたサービスアカウント** だけが書ける（割り当ては管理者。下の A）。
  `forge/` は forge 自身の worker だけ（HTTP からは管理者も書けない。forge 自身のチェックは X2 で入るので、それまで `forge/` の必須のチェックは
  報告されない）。`ext/` は C1 では誰も書けない（人が書ける名前でマージの条件を満たさせない）。
- サービスアカウントは、そのプロジェクトの **Developer 以上のメンバー** でなければ報告できない（メンバーでないプロジェクトは見えないので 404）。
- 結論が変わった報告だけが `check.completed` のイベントを出す（同じ結論の報告を繰り返してもイベントは増えない）。

## Trigger

- `gstn mr status --json` の `reasons` に `required_check_missing`（`name=<check>`）が出続ける。報告が届いていない。
- 保護ブランチのマージの条件の答え（`PUT …/merge-rule`、`gstn merge-rule set`）に `warnings: ["required_checks_unreportable"]` と
  `unreportable_checks: [...]` が出た: その名前の名前空間を報告できる有効なサービスアカウントが **1 つも無い**。
- `forge doctor` の `checks.reporters` が WARN: 必須のチェックに使われている名前空間に、割り当てられた有効なサービスアカウントが無い。
- 報告が `FG-AUTH-0003`（403）で拒否された（`reason=namespace_not_granted` / `not_service_account` / `forge_namespace_internal` /
  `ext_namespace_unavailable` / `agent_token`）、`FG-REPO-0005`（404、メンバーでない・コミットが無い・届かない）、`FG-REVIEW-0020`（422）、
  `FG-REVIEW-0021`（409）。
- `error_code=FG-DB-0022` CheckRunDataInvalid（alert）: 保存されたチェックの行を読めない。

## Impact

- 報告が届かない: 必須のチェックを設定した保護ブランチには **マージできない**（報告が無いことは成功ではない）。直接の push の禁止と合わせると
  ブランチが動かない。データは失われない。
- `FG-DB-0022`: そのコミットのチェックを読むすべて（マージの状態、マージのジョブ、一覧）が失敗する。マージは止まる（途中までの行で判定しない）。
  データが forge-db の外で変えられている。原因が分かるまで SEV2 として扱う。

## Diagnose（読み取りのみ）

1. マージの状態を読む: `gstn mr status --project <id> --iid <n> --json`。`checks[]` の `status: null` は「報告が無い」、
   `required_check_not_successful` は報告はあるが `completed` / `success` でない（`neutral` と実行中を含む）。
2. コミットの報告を読む: `gstn checks list --project <id> --sha <head_sha> --json`。**今の head の SHA** で読む（古い head の報告は数えない）。
3. 報告した主体と割り当てを確かめる（管理者）:
   - `gstn admin service-account check-grants <id>`（割り当て。**プロジェクトごと**: 報告先のプロジェクトの行があるか）
   - そのサービスアカウントがプロジェクトの Developer 以上のメンバーか（`gstn member list --project <id>`）
4. 拒否のログ: `error_code=FG-AUTH-0003 action=CheckWrite` の `reason`（上の Trigger の一覧）。`FG-REVIEW-0020` は `reason`（`name_invalid`、
   `too_many_annotations` など。本文・URL・注釈の文面はログに出ない）、`FG-REVIEW-0021` は `limit`・`head_sha`。
5. `FG-DB-0022` のとき（書き込み用ロールで、読み取りだけ）:

   ```sql
   SELECT id, name, status, conclusion, completed_at, revision, reported_by
   FROM forge.check_runs WHERE project_id = <project_id> AND head_sha = '<head_sha>' ORDER BY id;
   SELECT count(*) FROM forge.check_runs WHERE project_id = <project_id> AND head_sha = '<head_sha>';
   SELECT check_run_id, position, level, start_line, end_line FROM forge.check_annotations
   WHERE check_run_id IN (SELECT id FROM forge.check_runs WHERE project_id = <project_id> AND head_sha = '<head_sha>');
   SELECT conname FROM pg_constraint WHERE conrelid IN ('forge.check_runs'::regclass, 'forge.check_annotations'::regclass);
   ```

   ログの `reason`: `too_many_stored_runs`（101 行以上）、`stored_name_invalid`、`stored_status_invalid`、`stored_annotation_invalid`（`check_run_id`）。
   いつ誰が変えたか: `forge_audit.ddl_audit`（制約の変更）と `forge.audit_log` の `check_run.reported` / `check.completed` の行。

## Mitigate

本番のデータを変える手順は **人が承認したときだけ** 実行する（CLAUDE.md §3.7）。

### A. 名前空間をサービスアカウントに割り当てる（報告できる主体が無い）

1. 報告に使うサービスアカウントを用意する（Runbook `service-accounts`）。目的ごとに 1 つ（CI の報告用、kanflow 用）。
2. 名前空間を **報告先のプロジェクトについて** 割り当てる `[MUTATING — needs approval]`（ロールバック:
   `gstn admin service-account revoke-checks <id> --project <p> ci`）。割り当てはプロジェクトごとで、別のプロジェクトの割り当ては効かない:

   ```
   gstn admin service-account grant-checks <service_account_id> --project <project_id> ci
   ```

   （API: `PUT /api/v1/admin/service-accounts/{id}/check-grants/{project_id}/{namespace}`。管理者だけ。`ci` と `kanflow` だけを割り当てられる）
3. サービスアカウントを報告先のプロジェクトの Developer 以上のメンバーにする `[MUTATING — needs approval]`（ロールバック: メンバーから外す）。

### B. 報告の形を直す（FG-REVIEW-0020 / 0021）

- 名前は報告の種類ごとに固定する（`ci/gates`。`ci/gates-<実行番号>` のように毎回変えると 100 種類の上限に当たる）。1 回の実行の詳細は要約と注釈に書く。
- 注釈は 50 件まで、パスはリポジトリの中の相対パス（`/` で始まらない、`..` を含まない）、`details_url` は http / https の URL（サーバーは開かない）。

### C. 報告を待たずにマージを進める（緊急のとき）

必須のチェックの名前を外す（Runbook `merge-blocked` の「必須のチェックで動かなくなったブランチ」）`[MUTATING — needs approval]`。報告の経路が
直ったら付け直す。

### D. 読めない行（FG-DB-0022）

1. 規則に反する行を消す（報告した主体がもう一度報告すれば戻る。監査の記録に報告ごとの行が残る）`[MUTATING — needs approval]`
   （ロールバック: 報告し直す）。手で UPDATE しない、監査の行を手で書かない、トリガーを止めない。
2. 外された制約を、マイグレーション `20261115100000` の定義どおりに戻す `[MUTATING — needs approval]`。

## Verify

- `gstn checks list --project <id> --sha <head_sha>` に報告が出る。
- `gstn mr status --project <id> --iid <n> --json` の `checks[]` に `status: "completed"`, `conclusion: "success"` が出て、
  `required_check_missing` が消える。
- `forge doctor` の `checks.reporters` が OK。

## Escalate

- 誰がいつ行を変えたか分からない、監査の行の無い変更がある: セキュリティの事案として扱い（`/incident`）、DB の管理者に連絡する。
- 報告の主体ではない誰かが `ci/` や `kanflow/` の結果を書いたように見える: そのサービスアカウントのトークンの漏えいを疑い、Runbook
  `service-accounts` の失効の手順へ。
