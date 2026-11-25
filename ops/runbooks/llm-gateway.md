# Runbook: llm-gateway（FG-LLM-0001 / FG-LLM-0004。関連: FG-LLM-0002 / 0005 / 0006、FG-SYS-0002 `config_key=llm.*`）

計画 19（ADR-0039）のローカル LLM のゲートウェイ（`forge worker` の中、クレート `forge-llm`）が助言を作れないとき・混むとき・止めたいとき。
LLM は助言だけで、**止まっても何も止まらない**（マージ・承認・デプロイは今のまま。助言の画面は LLM なしの表示に戻る）。
GPU は Kanflow の Ollama を共用し、Kanflow の利用者が待つ間は forge は対話以外を始めない。

## Trigger

- `forge worker` のログ `forge.llm.request_finished`（warn）`error_code=FG-LLM-0001`: Ollama に届かない・時間切れ・エラーの応答。
  `reason`: `connect` / `connect_timeout`（Ollama が動いていない）、`timeout` / `gateway_timeout`（遅い）、`status_500`（多くは GPU のメモリ不足:
  夜間のベンチ `gpu-eval` と重なった）、`status_404`（モデルが無い）、`body_invalid` / `body_too_large`
- 同じく `error_code=FG-LLM-0004`: 待ちの期限（対話 2 分・MR レビュー 30 分・一括 6 時間）を過ぎた。`priority`・`deadline_s`
- `task=llm_model_check` の FG-LLM-0001: 定期のモデルの確かめで `/api/tags` を読めない（連続の最初の 1 回だけ記録）
- `forge.llm.metrics`（5 分ごと）: `waiting` が増え続ける、`forge_llm_yield_total{reason="kanflow_busy"}` が増え続ける

## Impact

- 助言（MR の一次レビュー・承認の要約・障害の診断）が出ない・遅い。**業務は止まらない**。`forge/ai-review` を必須にした試行のプロジェクトだけ、
  チェックが `error` で MR が待つ（`error` は成功にならない、ADR-0003）。通常は SEV4。必須のプロジェクトで半日続くなら SEV3。
- Kanflow の利用者への影響は無い（forge は Kanflow に譲る）。

## 設定（`[llm]`、環境変数は `FORGE_LLM_…`）

`llm.enabled`（既定 `false`。本番は GPU のホストが決まるまで無効）、`llm.endpoint`（ループバックか `llm.allowed_hosts` だけ。
それ以外は起動時に FG-SYS-0002 `reason=llm_endpoint_not_internal`）、`llm.model`（`name:tag@sha256:<digest>`）、`llm.num_ctx`
（Kanflow と同じ値。違うと Ollama がモデルを読み直す）、`llm.timeout_ms`、`llm.concurrency`（1〜2）、`llm.paused`、
`llm.kanflow_busy_url` + `llm.kanflow_token_file`（Kanflow の `GET /api/internal/ai-busy`、`x-internal-token`）、`llm.kanflow_poll_ms`。

## Diagnose（読み取りのみ）

1. ログを `request_id` か `llm_request_id` で読む: `forge.llm.request_finished`（`error_code`・`reason`・`purpose`・`model`・`prompt_version`）。
2. 記録: `SELECT id, purpose, priority, state, error_code, attempts, queued_at, started_at, finished_at FROM forge.llm_requests WHERE id = <llm_request_id>;`
   直近の傾向: `SELECT priority, state, error_code, count(*) FROM forge.llm_requests WHERE queued_at > now() - interval '1 hour' GROUP BY 1, 2, 3;`
   （入力の本文は DB に無い。`input_sha256` と対象の ID・SHA で作り直す）
3. Ollama（worker のホストから）: `curl -s <llm.endpoint>/api/tags`（モデルと digest）、`nvidia-smi`（GPU のメモリ）、
   `gstn runner list`（`gpu-eval` の Runner がベンチを動かしているか）。
4. 混み具合: `forge.llm.metrics` の `waiting` / `running`、`forge_llm_queue_wait_seconds{priority}`、`forge_llm_yield_total{reason}`、
   `forge.llm.kanflow_gate` の状態の変化（`Busy` が長い = Kanflow の利用が多い、`Unreachable` = Kanflow に届かない: 1 分に 1 件に絞っている）。
5. worker が止まった後: 前のプロセスの待ち・実行中の行は 7 時間後に `abandoned`（`error_code=FG-LLM-0001`、ログ `forge.llm.abandoned_stale`）。
   依頼した機能がやり直す。

## Mitigate（承認のあと。本番は承認された手順だけ）

- Ollama が落ちた・モデルが無い: Kanflow の側（Ollama の持ち主）に戻してもらう。forge からは Ollama を操作しない。
- GPU のメモリ不足が夜間のベンチと重なる: ベンチの時間（夜間 01:00〜05:00）を確かめる。一括処理はベンチの間は始まらない。
- 混雑が続く: `llm.concurrency` を 2 に上げるのは GPU に余裕があるときだけ。続くなら forge 専用の GPU のホスト（計画 19 §1.1 G3）を提案する。
- 止めたい: `llm.paused = true` で worker を再起動する（新しい依頼は並ばず「一時停止中」。並んでいたものは続く）。戻すときは `false`。
- 不正な答え（FG-LLM-0002）が続く: runbook `llm-model-update`（モデル・プロンプトの版を戻す）。

## Verify

- `forge.llm.request_finished` の `outcome=ok` が戻った。`forge_llm_requests_total{outcome="ok"}` が増える。
- `SELECT state, count(*) FROM forge.llm_requests WHERE queued_at > now() - interval '15 minutes' GROUP BY 1;` に `succeeded` がある。

## 関連

- ADR-0039、ADR-0017 §3（優先度・予約・昇格）、計画 19 §1.1・§8
- runbook `llm-model-update`（FG-LLM-0003、モデルの変更）、`ai-eval-bench`（`gpu-eval` のベンチ）、`config-invalid`（FG-SYS-0002）
