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
- `forge doctor`（下の「doctor の項目」）: `FAIL llm.endpoint`（FG-LLM-0001）、`FAIL llm.queue_age`（FG-LLM-0004 `reason=queued_past_deadline`）、
  `warn llm.queue_age`（直近 1 時間に期限切れ）、`warn llm.num_ctx`、`warn llm.kanflow_gate`

## Impact

- 助言（MR の一次レビュー・承認の要約・障害の診断）が出ない・遅い。**業務は止まらない**。`forge/ai-review` を必須にした試行のプロジェクトだけ、
  チェックが `error` で MR が待つ（`error` は成功にならない、ADR-0003）。通常は SEV4。必須のプロジェクトで半日続くなら SEV3。
- Kanflow の利用者への影響は無い（forge は Kanflow に譲る）。

## 設定（`[llm]`、環境変数は `FORGE_LLM_…`）

`llm.enabled`（既定 `false`。本番は GPU のホストが決まるまで無効）、`llm.endpoint`（ループバックか `llm.allowed_hosts` だけ。
それ以外は起動時に FG-SYS-0002 `reason=llm_endpoint_not_internal`）、`llm.model`（`name:tag@sha256:<digest>`）、`llm.num_ctx`
（Kanflow と同じ値。違うと Ollama がモデルを読み直す）、`llm.timeout_ms`、`llm.concurrency`（1〜2）、`llm.paused`、
`llm.kanflow_busy_url` + `llm.kanflow_token_file`（Kanflow の `GET /api/internal/ai-busy`、`x-internal-token`）、`llm.kanflow_poll_ms`。

## doctor の項目（計画 19 §8.4。`forge doctor` を worker のホストで。`llm.enabled = false` の間は `llm.queue_age` も含めてすべて `skip`）

| 項目 | 見るもの | 失敗のとき |
|---|---|---|
| `llm.endpoint` | Ollama が `/api/tags` に 5 秒で答えるか（社内の行き先かは設定の読み込みで確かめ済み） | `FAIL` FG-LLM-0001 と `reason` → 下の Diagnose 3 |
| `llm.model` | `llm.model` の `name:tag` が固定した digest であるか | `FAIL` FG-LLM-0003（alert）→ runbook `llm-model-update` |
| `llm.num_ctx` | 読み込まれているモデルの `context_length`（`/api/ps`）が `llm.num_ctx` と同じか | `warn`: Kanflow と値が違う → 下の「Kanflow との取り合い」 |
| `llm.kanflow_gate` | Kanflow の `ai-busy` が答えるか（`busy` / `idle`） | `warn` `no answer (reason=…)`: forge は Kanflow を空きとみなし 1 分に 1 件だけ始める（止まらない） |
| `llm.queue_age` | `queued` の行の層ごとの最古の待ち、直近 1 時間の期限切れ（DB、読み取り。FG-LLM-0004 は warn の重さだが、`queued_past_deadline` は worker の停止を意味するので、助言を必須にしたプロジェクトがあれば SEV3 として扱う） | `FAIL` FG-LLM-0004 `reason=queued_past_deadline`: 期限 + 2 分を過ぎても `queued` = 列を処理するゲートウェイがいない（worker が止まった・固まった）。`warn`: 期限切れがあった → 混雑（下の Mitigate） |

LLM は `/readyz` に入れない（助言なので、LLM の停止でサーバーを準備未了にしない）。

## Diagnose（読み取りのみ）

0. `forge doctor`（上の表）。`llm.queue_age` が `queued_past_deadline` なら、まず `systemctl status forge-worker` とログの
   `forge.llm.gateway_started` / `forge.llm.gateway_disabled`（worker が動いているか、ゲートウェイを有効にして起動したか）。
1. ログを `request_id` か `llm_request_id` で読む: `forge.llm.request_finished`（`error_code`・`reason`・`purpose`・`model`・`prompt_version`）。
2. 記録: `SELECT id, purpose, priority, state, error_code, attempts, queued_at, started_at, finished_at FROM forge.llm_requests WHERE id = <llm_request_id>;`
   直近の傾向: `SELECT priority, state, error_code, count(*) FROM forge.llm_requests WHERE queued_at > now() - interval '1 hour' GROUP BY 1, 2, 3;`
   （入力の本文は DB に無い。`input_sha256` と対象の ID・SHA で作り直す）
3. Ollama（worker のホストから）: `curl -s <llm.endpoint>/api/tags`（モデルと digest）、`nvidia-smi`（GPU のメモリ）、
   `forge admin runner list`（`gpu-eval` の Runner がベンチを動かしているか）。
4. 混み具合: `forge.llm.metrics` の `waiting` / `running`、`forge_llm_queue_wait_seconds{priority}`、`forge_llm_yield_total{reason}`、
   `forge.llm.kanflow_gate` の状態の変化（`Busy` が長い = Kanflow の利用が多い、`Unreachable` = Kanflow に届かない: 1 分に 1 件に絞っている）。
5. worker が止まった後: 前のプロセスの待ち・実行中の行は 7 時間後に `abandoned`（`error_code=FG-LLM-0001`、ログ `forge.llm.abandoned_stale`）。
   依頼した機能がやり直す。

## Kanflow との取り合い

GPU と Ollama は Kanflow と共用する。forge は Kanflow の利用者を待たせない側に寄せてある:

- Kanflow の `ai-busy` が `busy` の間、forge は対話（障害の診断・承認の要約）以外を始めない。`forge_llm_yield_total{reason="kanflow_busy"}` が増える。
  昼に MR レビューが 30 分の期限（FG-LLM-0004）を超えるなら、利用の重なりが大きい。
- `ai-busy` に届かない（`llm.kanflow_gate` が `warn`、ログ `forge.llm.kanflow_gate` が `Unreachable`）: forge は 1 分に 1 件だけ始める。
  Kanflow の内部トークン（`llm.kanflow_token_file`）が Kanflow の側で変わったときにも起きる（`reason=status_401` / `status_403`）。
- `num_ctx` が Kanflow と違う（`llm.num_ctx` が `warn`）: Ollama は同じモデルでも文脈の長さが変わると読み直す。forge と Kanflow が
  交互に使うたびに VRAM の入れ替えが起き、どちらも遅くなる（FG-LLM-0001 `timeout` の原因にもなる）。Kanflow の値を聞いて
  `llm.num_ctx` を合わせる（設定の変更と worker の再起動。本番は承認された手順）。

## Mitigate（承認のあと。本番は承認された手順だけ）

- 列を処理するゲートウェイがいない（`llm.queue_age` の `queued_past_deadline`）: worker を起動・再起動する（`systemctl start forge-worker`）。
  残った行は 7 時間で `abandoned` になり、依頼した機能がやり直す。
- Ollama が落ちた・モデルが無い: Kanflow の側（Ollama の持ち主）に戻してもらう。forge からは Ollama を操作しない。
- GPU のメモリ不足が夜間のベンチと重なる: ベンチの時間（夜間 01:00〜05:00）を確かめる。一括処理はベンチの間は始まらない。
- 混雑が続く: `llm.concurrency` を 2 に上げるのは GPU に余裕があるときだけ。続くなら forge 専用の GPU のホスト（計画 19 §1.1 G3）を提案する。
- 止めたい: 管理者が `gstn llm pause`（`POST /api/v1/admin/llm/pause`、`api` と `admin` のスコープ、人だけ）。worker は
  `forge.llm_gateway` を 10 秒ごとに読み、`forge.llm.pause_followed`（`paused=true`）を 1 回出して止まる（新しい依頼は並ばず
  「一時停止中」。並んでいたもの・動いているものは続く）。戻すときは `gstn llm resume`。監査は `llm.paused` / `llm.resumed`。
  再起動しても止めたままにしたいとき・DB に届かないときは、従来どおり `llm.paused = true` で worker を再起動する
  （設定の一時停止は API の再開より強い。`llm.paused = true` の間は `gstn llm resume` しても止まったまま）。
- 同じ MR の「もう一度レビュー」が多い（FG-WEB-0010 `route=ai_review_rerun`）: 1 MR 1 時間 3 回の上限が GPU を守っている。
  `SELECT merge_request_id, requested_by, count(*) FROM forge.llm_review_reruns WHERE requested_at > now() - interval '1 day'
  GROUP BY 1, 2 ORDER BY 3 DESC LIMIT 10;` で誰がどの MR を押しているかを見る。上限は変えない（計画 19 §8.2）。
- 不正な答え（FG-LLM-0002）が続く: runbook `llm-advice-quality`（プロンプトの版を戻す）か `llm-model-update`（モデルを戻す）。

## Verify

- `forge.llm.request_finished` の `outcome=ok` が戻った。`forge_llm_requests_total{outcome="ok"}` が増える。
- `SELECT state, count(*) FROM forge.llm_requests WHERE queued_at > now() - interval '15 minutes' GROUP BY 1;` に `succeeded` がある。
- `forge doctor` の `llm.*` がすべて `ok`（`llm.num_ctx` は読み込まれていなければ `not loaded now` の `ok`）。

## 関連

- ADR-0039、ADR-0017 §3（優先度・予約・昇格）、計画 19 §1.1・§8
- runbook `llm-model-update`（FG-LLM-0003、モデルの変更）、`llm-advice-quality`（FG-LLM-0002 / 0006、助言の質・プロンプトの版を戻す）、`ai-eval-bench`（`gpu-eval` のベンチ）、`config-invalid`（FG-SYS-0002）
