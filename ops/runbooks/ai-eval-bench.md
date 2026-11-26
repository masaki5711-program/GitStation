# Runbook: ai-eval-bench（FG-AIASSET-0002。関連: FG-AIASSET-0003 / 0004 / 0006 / 0007、FG-CI-0001 `eval_report_declared_twice`）

計画 17 P17-2（ADR-0034）の AI の評価のチェック `forge/ai-eval` が `error` になる・現れない・期待と違うとき。
`.forge-ci.yml` のジョブが成果物 `eval-report.json` を宣言したパイプラインだけ、forge の worker（タスク `ci_ai_eval`、10 秒ごと）が
ベンチの報告を基準 `evals/baselines/<models>.json`（パイプラインのコミットのもの）と比べて書く。形と判定は `docs/ai-eval-report.md`。

**夜間の基準の候補**（計画 17 P17-6）: 定期（C12）のパイプラインが既定のブランチで通ると、forge が `ai-eval/baseline-<models>` から
基準の候補の **下書きの MR を 1 本** 保つ（毎晩更新、人が承認）。形と止め方は `docs/ai-eval-report.md` の「夜間のベンチと基準の候補」。
候補が出ない・MR が増えないとき: `forge serve` のログ `forge.ai_eval.baseline_proposed`（`outcome`・`reason`: `unchanged` /
`baseline_moved`（夜のコミットの後に基準が変わった）/ `branch_exists`（閉じた候補のブランチが残っている: 消せば再開）/
`owner_not_allowed`（定期の持ち主が MR を開けない・push できない: `forge.ai_eval.baseline_refused` の `error_code`。持ち主を Maintainer に）/
`target_missing` / `ai_assets_disabled`（プロジェクトの `ai_assets.enabled` が off）/ `too_many_candidates`（開いた候補が 10 本）/
`branch_missing`（開いた候補の枝が消えた））と `forge.ai_eval.baseline_skipped`（`not_scheduled` / `superseded`（新しい定期の実行がある）/ `not_default_branch` / `no_report` / `not_passing` / `unchanged` など）。
ジョブ: `SELECT state, attempts, last_error_code FROM forge.jobs WHERE kind = 'ai_eval.baseline_candidate' ORDER BY id DESC LIMIT 5;`
（`dead` なら `last_error_code` の runbook）。

## Trigger

- `forge worker` のログに `forge.ai_eval.check_written`（warn）`error_code=FG-AIASSET-0002`: ベンチが使える報告を残さなかった。
  `reason`: `job_failed`（ベンチのジョブが失敗）/ `job_canceled` / `job_not_run`（`detail=pipeline_<status>`: パイプラインがジョブを動かさずに終わった。
  `gpu-eval` の Runner が無く取り消された・時間切れなど）/ `report_missing`（成功したが `eval-report.json` をアップロードしていない）/
  `report_file_missing`（行はあるがファイルが保存先に無い: 期限切れの削除・保存先の事故）/ `pipeline_stale`（24 時間を越えて待ち・実行中）/
  `read_failed`（報告・基準を 5 回続けて読めなかった。`detail` の `cause` が元のコード。それまでの失敗は `forge.ai_eval.read_failed`（info））
- MR の画面・`gstn mr status` で `forge/ai-eval` が `error` のまま、または `required_check_missing`（行が現れない）
- `forge.ai_eval.read_failed`（info）が続く: 報告・基準を読めず、間をあけてやり直している（`error_code` が原因: FG-REPO-0016 / FG-GIT-* / FG-REPO-0003 保存先の I/O）。
  5 回で `read_failed` の `error` になる。`forge.ai_eval.check_failed`（warn）は DB に書けなかった（FG-DB-*、runbook `db-unavailable`）
- `forge doctor`（下の「doctor の項目」）: `FAIL ai_eval.runner`（FG-AIASSET-0002 `reason=gpu_runner_unavailable`）、
  `FAIL ai_eval.baseline_age`（FG-AIASSET-0002 `reason=nightly_not_passing`）、`warn ai_eval.runner` / `warn ai_eval.baseline_age`

## Impact

- `forge/ai-eval` を必須にしたブランチ（`pfs-ai/assets` の `main`）へのマージが待つ。**`error` は成功にならない**（計画 17 §1.6 のゲート 5）。
  本番は変わらない（本番を変えるのはリリースの承認）。AI の誤りは品質の事故なので通常は SEV4。夜間のベンチが 2 晩続けて動かないなら SEV3。
- 宣言の無いリポジトリには影響しない（チェックの行そのものが無い）。

## doctor の項目（計画 17 P17-8。`forge doctor` をサーバーのホストで。DB の読み取りのプールだけ、件数と id だけ）

| 項目 | 見るもの | 失敗のとき |
|---|---|---|
| `ai_eval.runner` | タグ `gpu-eval` の有効な Runner（5 分以内に連絡があれば生きている）、10 分を越えて `pending` のタグ `gpu-eval` のジョブ、7 日以内に `eval-report.json` を宣言したパイプラインの数 | `FAIL` FG-AIASSET-0002 `reason=gpu_runner_unavailable`: ジョブが待ち、生きた `gpu-eval` の Runner が無い（`oldest_job_id`・`waiting`・`gpu_runners`・`live_gpu_runners`）→ 下の Diagnose 4 と runbook `ci-runner`。`warn`（生きた Runner があるのに待つ）: Runner が別のベンチで塞がっている（同時 1）か、プロジェクトが `ci.gpu_eval_projects` に無い（`ci.gpu_eval_max_running` に達している）。`warn`（宣言があり生きた Runner が無い、まだ待ちは無い）: 次のベンチが待つ前に Runner を戻す |
| `ai_eval.baseline_age` | 有効な定期（C12）を持ち、30 日以内に定期のパイプラインが報告を宣言したプロジェクトごとの、最後に `succeeded` になった定期のパイプライン。開いている基準の候補の MR（ブランチ `ai-eval/baseline-*`）の最古。`ai_assets.enabled` なのに夜間のベンチが無いプロジェクト | `FAIL` FG-AIASSET-0002 `reason=nightly_not_passing`: 48 時間（2 晩）通った夜間のベンチが無い（`project_ids`。最初の定期のパイプラインから 48 時間は数えない）→ 基準の候補が出ず、基準が古くなる。SEV3（上の Impact）。`warn`: 候補の MR が 14 日を越えて開いたまま（週 1 回の確認を 2 回逃した。MR の判定が古い基準と比べている）→ approver に差分を読んでもらい、マージか閉じる。`warn`: `ai_assets.enabled` のプロジェクトに 30 日夜間のベンチが無い → 定期を作る（上の「夜間の基準の候補」） |

`forge/ai-eval` は `/readyz` に入れない（ベンチが動かなくてもサーバーは動く。MR が待つだけ）。

## Diagnose（読み取りのみ）

0. `forge doctor`（上の表）。`ai_eval.runner` の `FAIL` は `forge admin runner list` でタグ `gpu-eval` の Runner の最後の連絡を、
   `ai_eval.baseline_age` の `FAIL` は `project_ids` ごとに
   `SELECT id, status, error_code, created_at FROM forge.ci_pipelines WHERE project_id = <id> AND source = 'schedule' AND ai_eval_job IS NOT NULL ORDER BY id DESC LIMIT 5;`
   と定期の最後の結果 `SELECT id, active, last_run_at, last_outcome, last_error_code FROM forge.ci_pipeline_schedules WHERE project_id = <id>;`
   （`refused`（FG-CI-0026）は持ち主の権限、`busy` は前の夜のパイプラインがまだ動いている、`capped` は FG-CI-0028）。
   失敗した夜のパイプラインは下の 2〜4 で読む。
1. ログを `pipeline_id` で読む: `forge.ai_eval.check_written`（`error_code`・`reason`・`detail`・`job`）、`forge.ai_eval.check_failed`。
2. パイプラインと宣言したジョブ:
   `SELECT p.id, p.status, p.ai_eval_job, p.ai_eval_status, j.id AS job_id, j.status AS job_status, j.attempt, j.error_code, j.tags FROM forge.ci_pipelines p JOIN forge.ci_jobs j ON j.pipeline_id = p.id AND j.name = p.ai_eval_job WHERE p.id = <pipeline_id>;`
   `ai_eval_status` が `status` と違えば、worker がまだ書いていない（`ci_ai_eval` のタスクが動いているか、`forge.ai_eval.check_failed` を見る）。
3. 報告の成果物: `SELECT id, attempt, path, size_bytes, encode(sha256, 'hex'), expires_at FROM forge.ci_artifacts WHERE job_id = <job_id>;`
   今の `attempt` の `eval-report.json` が無ければ `report_missing`。ジョブの `artifacts: { paths }` に **ちょうど `eval-report.json`**（ルート）が
   あるか、ベンチがそのパスに書いたかを確かめる（`out/eval-report.json` は宣言にならない）。
4. ジョブの失敗: `gstn pipeline diagnosis --project <id> --pipeline <pipeline_id>`、ログは `gstn pipeline log`。
   GitStation 自身の `llm-eval`（計画 19 L5-10、`llm-bench`）はログの最後の行が `llm-bench: error_code=… reason=…`:
   FG-LLM-0001（サービス `ollama` に届かない・時間切れ）、FG-LLM-0003（`reason=model_missing` / `digest_mismatch`: モデル置き場に
   `crates/forge-llm/evals/bench.toml` の `model` が無い・違う）、FG-SYS-0002 `config_key=llm_bench.*`（評価セット・`bench.toml` の誤り。
   `detail` がファイルとケースの番号）。
   GPU の Runner: `forge admin runner list`（タグ `gpu-eval` の Runner が `online` か）、runbook `ci-runner`。
   ジョブが FG-CI-0029（GPU の無い Runner がタグ `gpu-eval` を持つ）・FG-CI-0030（モデル置き場がマニフェストと合わない、**alert**）で
   終わったなら、runbook `ci-runner`（GPU の Runner とモデル置き場）と、モデルを入れ替えている途中なら runbook `ai-model-update`。
5. `forge.ai_eval.check_written` の他のコード:
   - FG-AIASSET-0003 BaselineMissing: `detail` が基準のパスと merge base のコミット。基準は **ベースのブランチとの merge base** から読む
     （MR の中で足した基準は使わない）。`git merge-base <sha> <ターゲットの先端>` と `git show <merge base>:evals/baselines/<models>.json` で確かめる。
     `base_unresolved` はベースのブランチが無いか履歴を共有しない。
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
- 基準が無い: 夜間の結果から基準の MR を作り、**先にマージする**（`evals/baselines/` は保護パス: 承認者の人の承認）。その後でベンチの MR を再実行する。
- 読めない失敗が続く: `cause` のコードの runbook（`repo-storage`・`git-unavailable`・`repository-provisioning`）で直してから再実行する。
- 保存先の事故（`report_file_missing` が多数・`stored_digest_mismatch`）: runbook `repo-storage`。
- 夜間のベンチが 2 晩通らない（doctor `nightly_not_passing`）: 定期が止まった（`active = false`、FG-CI-0026 `refused`）なら持ち主を
  権限のある人（Maintainer）に替えて再開する（定期の変更は監査に残る）。パイプラインの失敗は上の手順で直し、次の夜を待つか `gstn pipeline retry`。
- 基準の候補の MR が古い（doctor `warn`）: approver が差分を読み、マージする（保護パスの承認）。要らない候補は閉じる（ブランチを消すと次の夜に新しい候補）。

## Verify

- `forge doctor` の `ai_eval.runner`・`ai_eval.baseline_age` が `ok`（夜間のベンチが次に通るまで、最大 24 時間 `baseline_age` は `FAIL` のまま）。
- `SELECT ai_eval_status, status FROM forge.ci_pipelines WHERE id = <pipeline_id>;` が同じになり、MR の `forge/ai-eval` が `success` か、
  理由の分かる `failure` になった。
- `forge.ai_eval.check_failed` が止まった。

## 関連

- ADR-0034、計画 17 §1.6・§5.3・§5.4、`docs/ai-eval-report.md`
- runbook `ci-runner`（Runner・成果物の保存）、`ci-pipelines`（パイプラインの作成）、`checks-reporting`（チェックとマージの条件）
- runbook `ai-model-update`（モデル・ベンチのイメージの更新、新しいモデルの基準を先に入れる）、`ai-asset-release`（リリースの前の確かめ）、
  `ai-asset-rollback`（展開した束の戻し）
