# Runbook: checks-reporting（FG-DB-0022 / FG-REVIEW-0020 / FG-REVIEW-0021 / 報告できない必須のチェック / `ci/pipeline` / CI のログと診断の FG-AUTH-0022）

Checks API（M3 C1、ADR-0024 §10、`docs/api/checks.md`）で、CI や kanflow のサービスアカウントがコミットにチェックの結果を報告する。
保護ブランチのマージの条件（必須のチェック）は、MR の **今の head** にその名前の `completed` / `success` の報告があるときだけ満たされる。

- 報告は `POST /api/v1/projects/{id}/commits/{sha}/checks`（`gstn checks report`）。同じコミット・同じ名前の報告は **1 行を上書きする**（PATCH は無い）。
- 名前は `<名前空間>/<名前>`。`ci/` と `kanflow/` は **名前空間を割り当てられたサービスアカウント** だけが書ける（割り当ては管理者。下の A）。
  ただし **`ci/pipeline` は forge 自身の worker だけ**（計画 14 C5、ADR-0029 決定 7。HTTP からは `ci` の割り当てを持つサービスアカウントも
  管理者も書けない: `FG-AUTH-0003 reason=ci_namespace_internal`）。読み方は下の G。
  `forge/` は forge 自身の worker だけ（HTTP からは管理者も書けない。forge 自身のチェックは X2 で入るので、それまで `forge/` の必須のチェックは
  報告されない）。`ext/` は C1 では誰も書けない（人が書ける名前でマージの条件を満たさせない）。
- サービスアカウントは、そのプロジェクトの **Developer 以上のメンバー** でなければ報告できない（メンバーでないプロジェクトは見えないので 404）。
- 結論が変わった報告だけが `check.completed` のイベントを出す（同じ結論の報告を繰り返してもイベントは増えない）。
- forge 自身のチェック（`forge/spec-drift`、`forge/risk`、`forge/spec-lint`、`forge/spec-trace`、`forge/doc-lint`、`forge/ci-lint`）は
  `forge worker` が MR の版の head に書く。`forge/ci-lint`（計画 14 C1）の読み方は下の F。

## Trigger

- `gstn mr status --json` の `reasons` に `required_check_missing`（`name=<check>`）が出続ける。報告が届いていない。
- 保護ブランチのマージの条件の答え（`PUT …/merge-rule`、`gstn merge-rule set`）に `warnings: ["required_checks_unreportable"]` と
  `unreportable_checks: [...]` が出た: その名前の名前空間を報告できる有効なサービスアカウントが **1 つも無い**。
- `forge doctor` の `checks.reporters` が WARN: 必須のチェックに使われている名前空間に、割り当てられた有効なサービスアカウントが無い。
- 報告が `FG-AUTH-0003`（403）で拒否された（`reason=namespace_not_granted` / `not_service_account` / `forge_namespace_internal` /
  `ci_namespace_internal` / `ext_namespace_unavailable` / `agent_token`）、`FG-REPO-0005`（404、メンバーでない・コミットが無い・届かない）、`FG-REVIEW-0020`（422）、
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

### E. 保持（量が気になるとき）

実行と注釈は、`forge worker` のタスク `check_run_retention`（1 時間ごと。満杯の批は 0.5 秒おいてすぐ続ける）が消す:
`updated_at` が `review.check_run_retention_days`（既定 **180**、30〜3650。`FORGE_REVIEW_CHECK_RUN_RETENTION_DAYS` か
`[review] check_run_retention_days`）より古く、**開いている MR の今の head のコミットでない** 実行を、注釈ごと、1 回に最大 1 000 件、
古い順に消す。1 回は短い 1 つのトランザクション（バッチをまたいでロックを持たない）。ロックは報告・マージのゲートと同じ向き:
プロジェクトの行を `FOR SHARE`（報告と同じ強さ。ゲート／完全削除が持つ `FOR UPDATE` のプロジェクトは **待たずに飛ばす**）、
実行の行を `FOR UPDATE SKIP LOCKED`（報告が書き換え中の行は待たずに飛ばす）。次の周期で拾い直す。

消した批ごとに、プロジェクトごとに 1 行の監査 `check_run.retention`（actor `system`、label `forge-worker`、resource `project`）が残る。
details は個数だけ: `{"project_id","runs","annotations","older_than_days"}`。何も消さない批は何も書かない。

量と、保持が追いついているかは読み取りで見る:

```sql
SELECT project_id, count(*) AS runs, min(updated_at) FROM forge.check_runs GROUP BY project_id ORDER BY runs DESC LIMIT 20;
SELECT count(*) FROM forge.check_annotations;
-- 消されるはずのもの（期限切れで、開いている MR の head でない）の数
SELECT count(*) FROM forge.check_runs r
 WHERE r.updated_at < now() - interval '180 days'
   AND NOT EXISTS (SELECT 1 FROM forge.merge_requests m WHERE m.project_id = r.project_id
                   AND m.state = 'open' AND m.head_sha = r.head_sha);
-- 直近の保持の記録
SELECT created_at, resource_id AS project_id, details FROM forge.audit_log
 WHERE action = 'check_run.retention' ORDER BY id DESC LIMIT 20;
```

`forge doctor` の `checks.retention` は、期限切れで消されるはずの実行が残っていれば WARN（件数と一番古い実行の経過日数）。
ワーカーが止まっている、ゲートや報告が長く続いてプロジェクトが飛ばされ続けている、のどれか: `forge.worker` のログの
`task=check_run_retention`（`error_code` つき。DB の失敗は `FG-DB-0001` / `FG-DB-0002`）を `request_id` で追う。

- 期限切れの MR の head の実行が消えた後で、その MR がまたその head になった（古いコミットへブランチを戻した）場合、必須のチェックは
  「報告なし」（`required_check_missing`）としてマージを止める（安全側）。報告し直せば戻る。
- 手で消さない（監査される表で、消すには監査の行が要る）。窓を短くしたい／長くしたいときは設定を変えてワーカーを再起動する
  `[MUTATING — needs approval]`（ロールバック: 値を戻す。消えた実行は戻らない）。

### F. `forge/ci-lint` が `failure` / `error`（計画 14 C1）

`check.spec_index` のジョブが `forge/doc-lint` と一緒に書く。head の `.forge-ci.yml` を forge-ci の lint（`gstn ci lint` と同じ関数）に、
**そのプロジェクトの保護ブランチの一覧**（`forge.protected_refs` の `refs/heads/…`）を渡してかける。

- `.forge-ci.yml` が無い: `success`（「nothing to check」）。パイプラインを定義していないだけ。
- `failure`（`FG-CI-0001` CiConfigInvalid、info）: 要約に `.forge-ci.yml:<行> <理由> — <対象> [FG-CI-0001]` が並ぶ（forge-ci の上限:
  100 件と `too_many_problems`、対象は 64 文字で切る、形の崩れたシークレット名は出さない）。サーバーの障害ではなく、リポジトリの中身の問題。
  - `too_large`: 64 KiB を超えた（ファイルは読まない）。`not_a_file`: ディレクトリ・シンボリックリンク・サブモジュール（リンクはたどらない）。
  - `secret_on_unprotected_ref — <ブランチのパターン>`: シークレットを持つジョブが、保護されていないブランチか MR で動く。保護の一覧は
    `refs/heads/<パターン>` の規則から作る。タグの規則は数えない。`*` が末尾以外にある規則（`refs/heads/v*-lts`）は forge-ci が比べられないので
    **数えない**（保護されていない扱い。誤って通すことはない）。大文字小文字はそのままと小文字の両方を数える。
  - ログ: `event=forge.ci_lint.invalid error_code=FG-CI-0001`（`reason`・`line`・`problems` と `request_id`・`project_id`・`head_sha`）。
  - 手元で同じ答え: `gstn ci lint --protected main,release/*`（一覧はプロジェクトの保護ブランチ: `gstn protected-ref list --project <id>`）。
- `error`: ジョブが最後の試みで失敗した（`FG-GIT-*` の git の時間切れ・上限、`FG-DB-*`、保存された保護の規則を読めない `FG-DB-0020`）。
  要約の `request` で worker のログを追う。4 つ（spec-lint / spec-trace / doc-lint / ci-lint）が同じ `error` になる。死んだジョブは
  `spec_index_sweep` が `error` で答える。
- 直し方: `.forge-ci.yml` を直して push する（新しい版がもう一度 lint される）。シークレットのジョブなら `merge_requests: false` にし、
  `branches` を保護されたブランチだけにするか、保護の規則を足す（`gstn protected-ref add`、Maintainer。`[MUTATING — needs approval]`、ロールバック: `gstn protected-ref remove`）。

### G. `ci/pipeline`（計画 14 C5、ADR-0029 決定 7）

forge の CI のパイプラインの状態を、そのコミットのチェックとして `forge worker` のタスク `ci_outputs`（5 秒ごと）が書く。actor は `system`、
label は `forge-ci`、報告者なし（`reported_by` は NULL）。同じコミットに複数のパイプラインがあれば **最新のもの（id の大きい方）だけ** が書く。

| パイプライン | `ci/pipeline` |
|---|---|
| `pending` | `queued` |
| `running` | `in_progress` |
| `succeeded`、`skipped`（選ばれたジョブが無い） | `completed` / `success` |
| `failed`、`invalid`（FG-CI-0001） | `completed` / `failure` |
| `error`（FG-CI-0002） | `completed` / `error` |
| `canceled` | `completed` / `neutral`（必須のチェックを満たさない） |

- 要約: `Pipeline #<id> <status>: <n> jobs, … [<code>]`、失敗があれば `gstn pipeline diagnosis --project <p> --pipeline <id>`。
- 書かれない・古いまま: パイプラインの状態とチェックの記録を比べる（読み取りのみ）。

  ```sql
  SELECT id, sha, status, check_status FROM forge.ci_pipelines
   WHERE project_id = <project_id> AND check_status IS DISTINCT FROM status ORDER BY id LIMIT 20;
  ```

  残り続けるなら worker が止まっている（`forge doctor`、ログの `task=ci_outputs` と `error_code`）か、1 つのパイプラインの書き込みが
  失敗している（`event=forge.ci.pipeline_check_failed` の `pipeline_id`・`error_code`。次の周期でやり直す）。同じコミットの古い
  パイプラインは書かない（`check_status` だけが進む）。
- サービスアカウントが C5 の前に `ci/pipeline` を書いていた行（と注記）は、C5 の移行が消す（プロジェクトの監査ログに
  `check_run.reserved_removed`、件数だけ）。古い成功が必須チェックを満たすことはない。forge のパイプラインがあるコミットは
  worker が書き直す。パイプラインのないコミットは `ci/pipeline` が無いまま（必須ならマージは待つ。パイプラインを走らせる）。
- 手で直さない。パイプラインを再実行すれば（`gstn pipeline retry`）チェックも追う。

### H. CI のログと診断が読めない（FG-AUTH-0022、計画 14 C5、ADR-0029 決定 5）

`GET …/pipelines/{id}/jobs/{job_id}/log`（`gstn pipeline log`、MCP `get_job_log`）と `GET …/pipelines/{id}/diagnosis`
（`gstn pipeline diagnosis`、MCP `get_pipeline_diagnosis`）は、AI の相手（`is_agent` のトークン、サービスアカウント、ボット、
`Forge-Context-Audience: agent` を送る要求 = `gstn mcp`）には、**パイプラインのコミットの印**（作成のときの `copy_mark`）と
**既定のブランチの先端の印** の両方が `open` のときだけ答える。人は印で断られない。

- ログ: `error_code=FG-AUTH-0022` の `route`（`ci_log` / `ci_diagnosis`）、`reason`（`pipeline_confidential` / `pipeline_undecided` /
  `tip_confidential` / `tip_undecided` / `default_branch_unborn` / `tip_not_measured`）、`mark_of`（`pipeline` / `tip`）、`audience`
  （`agent_token` / `service_account` / `bot` / `header`）、`pipeline_id`・`job_id`、先端なら `tip`・`paths`・`check_error`。
- パイプラインの印を読む: `gstn pipeline show --project <id> --pipeline <pipeline_id>`（`copy_mark`）。先端は Runbook `smart-http` の
  「エージェントと機密の文書」と同じ。
- 想定どおりの拒否: 人のアカウントで読む。パイプラインの印は変わらない（作成のときに測った値）ので、機密の文書の無いコミットで新しい
  パイプラインを走らせる。`*_undecided` は測定の失敗（閉じる）: やり直し、続くなら文書の上限・git の時間切れを Runbook `smart-http` で見る。
- マスク: ログと抜粋の forge のトークン（`fgp_` / `fgr_` / `fgj_` / `gss_`）、`Authorization:` の値、URL のパスワード、PEM の秘密鍵は
  誰にも `[MASKED]` で返す。ページの前後 512 バイトを見て、ページの端で切れたものも隠す。それ以外の秘密をジョブが出力した場合は
  隠れない: そのジョブのスクリプトを直し、秘密を失効させる（漏えいとして `/incident`）。
- 診断の `pending: true`: worker（`ci_outputs`）がまだ指紋を作っていない（数秒）。続くなら worker のログ `task=ci_outputs`。
  指紋は `failure_fingerprints`（`occurrences`・`first_seen_at`・`fixed_by_merge_request_id`）。読み取りのみ:

  ```sql
  SELECT f.fingerprint, f.occurrences, f.first_seen_at, f.last_seen_at, f.fixed_by_merge_request_id
    FROM forge.failure_fingerprints f WHERE f.project_id = <project_id> ORDER BY f.last_seen_at DESC LIMIT 20;
  SELECT job_id, attempt, fingerprint_id, error_codes, first_line, last_line
    FROM forge.ci_job_failures WHERE pipeline_id = <pipeline_id>;
  ```

  （`excerpt` とログの本文は読み取り用の接続では読めない。ADR-0029 決定 5）

## Verify

- `gstn checks list --project <id> --sha <head_sha>` に報告が出る。
- `gstn mr status --project <id> --iid <n> --json` の `checks[]` に `status: "completed"`, `conclusion: "success"` が出て、
  `required_check_missing` が消える。
- `forge doctor` の `checks.reporters` が OK。
- `forge/ci-lint`: 直した head の `gstn checks list` で `forge/ci-lint` が `success`。
- `ci/pipeline`: `gstn checks list --project <id> --sha <sha>` の `ci/pipeline` がパイプラインの状態（G の表）と一致する。

## Escalate

- 誰がいつ行を変えたか分からない、監査の行の無い変更がある: セキュリティの事案として扱い（`/incident`）、DB の管理者に連絡する。
- 報告の主体ではない誰かが `ci/` や `kanflow/` の結果を書いたように見える: そのサービスアカウントのトークンの漏えいを疑い、Runbook
  `service-accounts` の失効の手順へ。
