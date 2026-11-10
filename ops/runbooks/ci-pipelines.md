# Runbook: ci-pipelines（FG-CI-0002、FG-CI-0003。関連: FG-CI-0001、FG-GIT-0002 / 0003 / 0004、FG-DB-0006、FG-REPO-0016）

M4 C2（計画 14）の CI のパイプラインの作成（ジョブ `ci.pipeline_create`、購読者 `ci-pipelines`、表 `forge.ci_pipelines` / `forge.ci_jobs`）が
できない・遅れる・期待と違うとき。

## Trigger

- `forge worker` のログに `FG-CI-0002` PipelineCreateFailed（warn、`forge.ci.pipeline_create_failed`）が出た: push・MR の版の
  `.forge-ci.yml` か保護ブランチの一覧を読めず、最後の試行でパイプラインを `error` として記録した（`cause_code` が元の失敗）
- 手動の実行（`POST /api/v1/projects/{id}/pipelines`、`gstn pipeline run`）が 503 `FG-CI-0002` を返した
- 利用者から「push したのにパイプラインが無い」「`invalid` になった」「取り消し・再実行が 409（FG-CI-0003）」と問い合わせがあった
- `forge worker` のログに `forge.ci.push_refs_not_planned`（warn）: 1 回の push で 20 を超えるブランチ、または event に載らなかった ref が
  あり、その分のパイプラインは作られていない
- `forge.ci.copy_mark_undecided`（warn）: パイプラインの SHA の機密の印を測れず `undecided` にした（C5 以降、AI はその出力を読めない）

## Impact

- `error` のパイプラインにはジョブが無い。push 自体・閲覧・マージの判定は影響を受けない（`ci/pipeline` を必須チェックにするのは X）。
- 1 件なら SEV4。多くのプロジェクトで続く、または worker が止まっているなら SEV3（git・DB の Runbook も見る）。

## Diagnose（読み取りのみ）

1. ログを `forge.ci.pipeline_create_failed` と同じ `request_id` の行で読む。`cause_code` の Runbook に進む:
   FG-GIT-0002 / 0003 / 0004 → `git-unavailable` / `git-process-failed`、FG-DB-* → `db-unavailable`、FG-REPO-0016 → `repository-provisioning`。
   それより前の試行はジョブの実行器が元のコードで記録している（`kind=ci.pipeline_create`、`job_id`）。
2. 行を読む:
   `SELECT id, source, ref, sha, status, error_code, problems, copy_mark, created_at FROM forge.ci_pipelines WHERE project_id = <id> ORDER BY id DESC LIMIT 10;`
3. ジョブを読む:
   `SELECT id, state, attempts, last_error_code, created_at FROM forge.jobs WHERE kind = 'ci.pipeline_create' ORDER BY id DESC LIMIT 10;`
   `queued` / `running` なら待つだけでよい（既定 5 回、間隔は待ち行列の backoff）。
4. 購読者の遅れ: `forge doctor --json` の `subscribers.*`（`ci-pipelines` の `lag`、`dead_letters`）。
5. 「push したのに無い」: `forge.ci.no_definition`（info）が出ていれば、その SHA に `.forge-ci.yml` が無い（ルートのファイルだけを読む）。
   `forge.ci.pipeline_created` の `status=skipped` は、定義がそのブランチでジョブを 1 つも選ばなかった（`rules.branches`）。
6. `invalid`（FG-CI-0001）: 行の `problems`（最大 20 件の `line` / `reason` / `subject`）か `gstn pipeline show`。手元で
   `gstn ci lint --protected <保護ブランチ>` / `gstn ci plan --branch <b> --event push|mr` で再現する。MR のパイプラインと手動の実行は
   保護されない扱いなので、シークレットを持つジョブが選ばれると `secret_on_unprotected_ref` になる（意図どおり）。
7. 409（FG-CI-0003）: ログの `operation`（`cancel` / `retry`）と `status`（見つけた状態）。終わったパイプラインは取り消せず、
   `failed` / `canceled` 以外は再実行できない。

## Mitigate（変更は人の承認のあと）

- 元の原因（git・DB・置き場）を、その Runbook の手順で直す。
- 直ったら、作られなかった・`error` になったパイプラインを手で作り直す: `gstn pipeline run --project <id> --ref <branch>`
  （手動の実行は重複を作る。push のパイプラインは 1 回だけで、同じ push の再配信では増えない）。
- `push_refs_not_planned` の分も同じく手で実行する。
- 行を直接書き換えない（派生の状態だが、ジョブの実行の状態と食い違う）。

## Verify

- `gstn pipeline list --project <id>` で新しいパイプラインが `pending`（または `skipped` / `invalid`）で見える。
- `forge.ci.pipeline_created` が同じ `request_id` で出る。

## 関連

- 計画 14（M4）§4.1・§5 C2、ADR-0029（決定 5: `copy_mark`）、`crates/forge-pipelines/README.md`、`crates/forge-ci/README.md`。
