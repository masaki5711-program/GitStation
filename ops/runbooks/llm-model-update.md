# Runbook: llm-model-update（FG-LLM-0003 alert。関連: FG-LLM-0002）

ローカル LLM のゲートウェイ（ADR-0039）は、Ollama が `llm.model` の `name:tag` で出すモデルの digest を、起動のときと 10 分ごとに
設定の `sha256:…` と比べる。違えば（またはモデルが無ければ）**推論をすべて止める**。モデルが黙って変わると、助言が評価した
モデルのものでなくなるため（計画 17 §6、計画 19 §8.1）。モデルの変更そのものもこの手順で行う。

## Trigger

- `forge worker` のログ（error）`error_code=FG-LLM-0003` `task=llm_model_check`: `model`、`expected_digest`、`actual_digest`
  （または `reason=model_missing`）。連続の最初の 1 回だけ記録する。
- その後の依頼は `error_code=FG-LLM-0003 reason=stopped`（記録は `abandoned`）。助言は出ない。

## Impact

- 助言がすべて止まる。業務は止まらない（助言だけ）。`forge/ai-review` を必須にした試行のプロジェクトはチェックが `error` で待つ。SEV3。
- 共用の Ollama なので、原因は多くの場合 Kanflow の側のモデルの更新（`ollama pull` で同じ `name:tag` の新しい版になった）。

## Diagnose（読み取りのみ）

0. `forge doctor`（worker のホストで）: `llm.model` が同じ比べ方をする（FG-LLM-0003 と `expected_digest` / `actual_digest`、
   または `reason=model_missing`）。`llm.endpoint` が `FAIL` なら Ollama に届いていない（runbook `llm-gateway`）。
1. `curl -s <llm.endpoint>/api/tags` で `name` と `digest` を見る。設定の `llm.model`（`name:tag@sha256:<digest>`）と比べる。
2. Kanflow の側に、モデルを更新したか・いつ・どの版かを聞く（`num_ctx` も変えたか）。
3. 意図した更新でなければ、Ollama のホストへの不正な操作を疑う（runbook `credential-data` の観点で確かめる）。

## Mitigate（承認のあと）

- **元の版に戻す**（意図しない変更）: Kanflow の側と合意して、評価済みの版を Ollama に戻す。gateway は 10 分以内に確かめ直し、
  一致すれば自動で再開する（`forge.llm.model_verified`）。
- **新しい版に切り替える**（意図した変更）: 1 本の MR で行う —
  1. 評価セット（`crates/forge-llm/evals/`、L5-10）を新旧のモデルで流し、`eval-report.json` を比べる（安全のケースは 100 %）。
  2. 設定の `llm.model` を新しい `name:tag@sha256:<digest>` にする（`latest` は使えない）。
  3. Kanflow と同時に切り替える（同じモデル・同じ `num_ctx` なら VRAM の入れ替えが起きない）。
  4. worker を再起動する（本番は承認された手順）。
- 戻すまで止めておきたい: `llm.paused = true`（runbook `llm-gateway`）。

## Verify

- `forge doctor` の `llm.model` が `ok`、`llm.num_ctx` が `warn` でない（Kanflow と同じ文脈の長さ）。
- `forge.llm.model_verified`（info）が出た。`forge.llm.request_finished outcome=ok` が戻った。
- `SELECT model_digest, count(*) FROM forge.llm_requests WHERE queued_at > now() - interval '1 hour' GROUP BY 1;` が設定の digest だけ。

## 関連

- ADR-0039、計画 19 §8.5、計画 17 §6（`ai-model-update` と同じ型）
- runbook `llm-gateway`、`llm-advice-quality`（モデルを変えた後に助言の質が落ちたとき）、`ai-eval-bench`
