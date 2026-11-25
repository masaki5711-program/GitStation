# Runbook: llm-advice-quality（FG-LLM-0002 / FG-LLM-0006。関連: FG-LLM-0003、計画 19 §4・§6）

ローカル LLM の助言（MR の一次レビュー・承認の要約・障害の診断）の **質** が落ちたとき。ゲートウェイは動いていて答えも返るが、
人が「誤り」を押す割合が増えた・「役に立った」が減った・形の崩れた答え（FG-LLM-0002）や根拠の無い指摘（FG-LLM-0006）が増えた。
多くはプロンプトかモデルの変更の後に起きる。**直し方は「戻す」**: プロンプトの版を前の版に戻す（モデルなら runbook `llm-model-update`）。
助言は助言だけなので、質が落ちても業務は止まらない。

届かない・混む・遅い（FG-LLM-0001 / 0004）は runbook `llm-gateway`。モデルの digest の不一致（FG-LLM-0003）は `llm-model-update`。

## Trigger

- 採用率・誤検知率が目標を外れた（計画 19 §6 の R2 の目標: 採用率 ≥ 30 %、誤検知（「誤り」の割合）≤ 20 %）。下の Diagnose 2 の集計で週に 1 回見る。
- `forge_llm_requests_total{purpose,outcome="FG-LLM-0002"}` / `forge_llm_output_invalid_total{purpose}` が増え続ける
  （単発は問題ない: 1 回やり直した後の失敗だけが数えられる）。
- `forge_llm_evidence_dropped_total{purpose}` が増える、または worker のログ（info）`error_code=FG-LLM-0006` の `dropped` が多い
  （助言は書かれるが `degraded`、画面は「信頼度が低い」と出す）。
- 利用者からの声: 「AI の指摘が的外れ」「同じ誤りばかり」。

## Impact

- 助言が読まれなくなる・誤った指摘に時間を取られる。マージ・承認・デプロイは変わらない（`forge/ai-review` は既定で必須でない）。
- `llm.review = required` にした試行のプロジェクトだけ、FG-LLM-0002 の間はチェックが `error` で MR が待つ（ADR-0003: `error` は成功にならない）。
- 通常 SEV4。`required` のプロジェクトで半日続くなら SEV3。

## Diagnose（読み取りのみ）

1. **いつから・どの版から**: 版ごとの依頼と結果（直近 14 日）。プロンプトの版かモデルが変わった日と、悪化した日を合わせる。
   ```sql
   SELECT purpose, prompt_id, prompt_version, model, state, error_code, count(*)
     FROM forge.llm_requests
    WHERE queued_at > now() - interval '14 days'
    GROUP BY 1, 2, 3, 4, 5, 6 ORDER BY 1, 3, 4;
   ```
2. **採用率と誤検知率**（版ごと。`finding_index = -1` は助言全体への評価）:
   ```sql
   SELECT a.purpose, a.prompt_version, a.model,
          count(*) FILTER (WHERE f.verdict = 'useful') AS useful,
          count(*) FILTER (WHERE f.verdict = 'wrong')  AS wrong,
          round(100.0 * count(*) FILTER (WHERE f.verdict = 'wrong') / NULLIF(count(*), 0), 1) AS wrong_pct
     FROM forge.llm_advice_feedback f
     JOIN forge.llm_advice a ON a.id = f.advice_id
    WHERE f.created_at > now() - interval '14 days'
    GROUP BY 1, 2, 3 ORDER BY 1, 2;
   ```
   採用率は「役に立った」の付いた指摘 ÷ 表示した指摘（`jsonb_array_length(a.findings)` の合計）。評価が 30 件に満たない版は判断しない
   （数が少ないと 1 人の評価で大きく動く）。助言は 180 日で消えるが評価は残る。消えた助言の評価（`advice_id` が NULL）は版が分からないので、
   長い期間の傾向は目的ごとに数える:
   ```sql
   SELECT purpose, date_trunc('week', created_at) AS week,
          count(*) FILTER (WHERE verdict = 'useful') AS useful,
          count(*) FILTER (WHERE verdict = 'wrong')  AS wrong
     FROM forge.llm_advice_feedback
    GROUP BY 1, 2 ORDER BY 1, 2;
   ```
3. **形の崩れ（FG-LLM-0002）**: worker のログ `forge.llm.request_finished` の `reason`（2 回目）と `first_reason` を `llm_request_id` で読む。
   同じ `reason`（例 `summary_missing`・`finding_severity`）が並ぶなら、プロンプトの出力の指示とモデルが合っていない。
4. **根拠の無い指摘（FG-LLM-0006）**: 版ごとの割合。
   ```sql
   SELECT purpose, prompt_version, model, count(*) AS advice,
          sum(dropped_findings) AS dropped, count(*) FILTER (WHERE degraded) AS degraded
     FROM forge.llm_advice
    WHERE created_at > now() - interval '14 days'
    GROUP BY 1, 2, 3 ORDER BY 1, 2;
   ```
   特定の MR に集中しているなら、差分・題・本文に指示を書いた注入の試みを疑う（捨てられているので害は無い。security の観点で記録する）。
5. **入力の側の変化**: 同じ時期に入力の組み立て（`forge-review` の MR の入力、生成物・ロックファイルの除外）が変わっていないか、
   `git -C <GitStation のリポジトリ> log --since=<悪化した日> -- crates/forge-llm crates/forge-review crates/forge-core/src/llm`。
   入力の本文は DB に無い（`input_sha256` だけ）。作り直すときは対象の ID と `head_sha` から。
6. `forge doctor` の `llm.model`・`llm.num_ctx` も見る（モデルが黙って替わった・文脈の長さが足りずに入力が切れた、を除く）。

## Mitigate（承認のあと。本番は承認された手順だけ）

- **プロンプトの版を戻す**（最初に選ぶ）: 悪化の前の版（`crates/forge-llm/prompts/<purpose>.v<N>.toml`）に戻す MR を 1 本作る。
  版の番号は戻さず、前の版の中身で **新しい版** を作る（記録の `prompt_version` で前後を区別できるように）。
  MR では評価セット（`crates/forge-llm/evals/<purpose>/`、`llm-bench`、計画 19 §6）を流し、安全のケース 100 % と基準以上を確かめる。
- **モデルを戻す**: モデルの変更の後に悪化したなら runbook `llm-model-update`（評価済みの digest に戻す。Kanflow と合わせる）。
- **すぐ止めたい**（戻す MR を待てない、誤った指摘が業務を邪魔している）:
  - プロジェクトの単位: 影響のあるプロジェクトの `llm.review` を `advisory` か `off` に（Maintainer 以上の人が Web で。監査に残る）。
  - 全体: `llm.paused = true` で worker を再起動（runbook `llm-gateway`。新しい依頼は並ばない）。
- **再発の防止**（フィードバックの行動、必須）: 誤った指摘・崩れた形・根拠の無い指摘の例を合成のデータにして、その目的の評価セットに足す
  （合成だけ。実際の差分やログを入れない、契約 8）。評価セットが拾えなかったなら、それが評価の穴。

## Verify

- 戻した版での 1 週間の集計（Diagnose 2）で、誤検知率が戻した前の水準に戻った。
- `forge_llm_output_invalid_total{purpose}`・`forge_llm_evidence_dropped_total{purpose}` の増え方が元に戻った。
- `SELECT prompt_version, count(*) FROM forge.llm_requests WHERE purpose = '<purpose>' AND queued_at > now() - interval '1 day' GROUP BY 1;`
  が新しい（戻した）版だけ。

## 関連

- ADR-0039、計画 19 §3.1・§4・§6・§8
- runbook `llm-gateway`（FG-LLM-0001 / 0004、一時停止）、`llm-model-update`（FG-LLM-0003、モデルの変更と戻し）、`ai-eval-bench`（`gpu-eval` のベンチ）
