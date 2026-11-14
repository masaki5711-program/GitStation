# Runbook: ai-eval-bench（FG-AIASSET-0002。関連: FG-AIASSET-0003 / 0004 / 0006 / 0007、FG-CI-0001 `eval_report_declared_twice`）

計画 17 P17-2（ADR-0034）の AI の評価のチェック `forge/ai-eval` が `error` になる・現れない・期待と違うとき。
`.forge-ci.yml` のジョブが成果物 `eval-report.json` を宣言したパイプラインだけ、forge の worker（タスク `ci_ai_eval`、10 秒ごと）が
ベンチの報告を基準 `evals/baselines/<models>.json`（パイプラインのコミットのもの）と比べて書く。形と判定は `docs/ai-eval-report.md`。

## Trigger

- `forge worker` のログに `forge.ai_eval.check_written`（warn）`error_code=FG-AIASSET-0002`: ベンチが使える報告を残さなかった。
  `reason`: `job_failed`（ベンチのジョブが失敗）/ `job_canceled` / `job_not_run`（`detail=pipeline_<status>`: パイプラインがジョブを動かさずに終わった。
  `gpu-eval` の Runner が無く取り消された・時間切れなど）/ `report_missing`（成功したが `eval-report.json` をアップロードしていない）/
  `report_file_missing`（行はあるがファイルが保存先に無い: 期限切れの削除・保存先の事故）
- MR の画面・`gstn mr status` で `forge/ai-eval` が `error` のまま、または `required_check_missing`（行が現れない）
- `forge.ai_eval.check_failed`（warn）が続く: 報告・基準を読めず、次の回にやり直している（`error_code` が原因: FG-REPO-0016 / FG-GIT-* / FG-REPO-0003 保存先の I/O / FG-DB-*）

## Impact

- `forge/ai-eval` を必須にしたブランチ（`pfs-ai/assets` の `main`）へのマージが待つ。**`error` は成功にならない**（計画 17 §1.6 のゲート 5）。
  本番は変わらない（本番を変えるのはリリースの承認）。AI の誤りは品質の事故なので通常は SEV4。夜間のベンチが 2 晩続けて動かないなら SEV3。
- 宣言の無いリポジトリには影響しない（チェックの行そのものが無い）。

## Diagnose（読み取りのみ）

1. ログを `pipeline_id` で読む: `forge.ai_eval.check_written`（`error_code`・`reason`・`detail`・`job`）、`forge.ai_eval.check_failed`。
2. パイプラインと宣言したジョブ:
   `SELECT p.id, p.status, p.ai_eval_job, p.ai_eval_status, j.id AS job_id, j.status AS job_status, j.attempt, j.error_code, j.tags FROM forge.ci_pipelines p JOIN forge.ci_jobs j ON j.pipeline_id = p.id AND j.name = p.ai_eval_job WHERE p.id = <pipeline_id>;`
   `ai_eval_status` が `status` と違えば、worker がまだ書いていない（`ci_ai_eval` のタスクが動いているか、`forge.ai_eval.check_failed` を見る）。
3. 報告の成果物: `SELECT id, attempt, path, size_bytes, encode(sha256, 'hex'), expires_at FROM forge.ci_artifacts WHERE job_id = <job_id>;`
   今の `attempt` の `eval-report.json` が無ければ `report_missing`。ジョブの `artifacts: { paths }` に **ちょうど `eval-report.json`**（ルート）が
   あるか、ベンチがそのパスに書いたかを確かめる（`out/eval-report.json` は宣言にならない）。
4. ジョブの失敗: `gstn pipeline diagnosis --project <id> --pipeline <pipeline_id>`、ログは `gstn pipeline log`。
   GPU の Runner: `gstn runner list`（タグ `gpu-eval` の Runner が `online` か）、runbook `ci-runner`。
5. `forge.ai_eval.check_written` の他のコード:
   - FG-AIASSET-0003 BaselineMissing: `detail` が基準のパス。`git show <sha>:evals/baselines/<models>.json` が無い・形が違う・`models` が違う。
   - FG-AIASSET-0007 EvalReportInvalid: `reason` と `detail`（フィールド・ケース）。`stored_digest_mismatch` は保存したファイルが
     アップロードのときと違う（保存先の改ざん・事故の疑い: runbook `repo-storage` も見る）。
   - FG-AIASSET-0004 / 0006: ベンチは動いた。チェックの要約の比較表と所見を MR の作者に見てもらう（運用の障害ではない）。
6. 行が現れない（`required_check_missing`）: `ai_eval_job` が NULL なら、そのパイプラインは報告を宣言していない（その契機でベンチのジョブが選ばれていない:
   `rules.merge_requests` / `rules.branches`）。`gstn ci plan --branch <b> --event mr` で選ばれるジョブを確かめる。2 つのジョブが宣言すると
   パイプラインは `invalid`（FG-CI-0001 `eval_report_declared_twice`）。

## Mitigate（承認のあと）

- Runner が無い・落ちた: `gpu-eval` の Runner を戻す（runbook `ci-runner`）。戻ったらパイプラインを再実行する（`gstn pipeline retry`）。
  再実行で状態が変わると worker が新しい試行の報告で書き直す。
- 報告が無い・形が違う: ベンチのジョブ（PFS の `bench` の入口）を直す MR。直るまで MR は待つ。**人が承知して `error` を通す抜け道は無い**
  （必須のチェックを外すのは保護ブランチの設定の変更で、監査に残る。計画 17 では行わない）。
- 基準が無い: 夜間の結果から基準の MR を作る（`evals/baselines/` は保護パス: 承認者の人の承認）。
- 保存先の事故（`report_file_missing` が多数・`stored_digest_mismatch`）: runbook `repo-storage`。

## Verify

- `SELECT ai_eval_status, status FROM forge.ci_pipelines WHERE id = <pipeline_id>;` が同じになり、MR の `forge/ai-eval` が `success` か、
  理由の分かる `failure` になった。
- `forge.ai_eval.check_failed` が止まった。

## 関連

- ADR-0034、計画 17 §1.6・§5.3・§5.4、`docs/ai-eval-report.md`
- runbook `ci-runner`（Runner・成果物の保存）、`ci-pipelines`（パイプラインの作成）、`checks-reporting`（チェックとマージの条件）
