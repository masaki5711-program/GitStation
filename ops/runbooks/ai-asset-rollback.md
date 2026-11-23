# Runbook: ai-asset-rollback（関連: FG-DEPLOY-0005 / 0006 / 0007、PFS の `AI-*`（PFS-5 で登録））

計画 17 §1.11 の、PFS の中央 AI の資産の束を **直前に承認されたリリース** に戻す手順。PFS の監視が自動で戻した後の確認と、
人が決めて戻す手動の戻し。戻しはローダーが前の束を **追記の行** で再登録するだけで、レジストリの行は消さない。

**今の状態（2026-11）**: 自動の戻し（PFS の監視とローダー）は **PFS-5〜7**、戻しの先を決める GitStation の機能
（`rollback_targets_last_approved`、3 つの環境）は **P17-7** で入る。それまでは束を展開していないので、この runbook で戻すものは無い。
入ったらその MR でコマンドとイベント名を埋める。

## Trigger

- PFS の自動の戻し（どれか 1 つ。数字は仮、計画 17 §1.11）: 劣化の率が旧版の 24 時間平均 +5 pt を 1 時間続けた / スキーマ不合格が 3 % 超 /
  critical の p95 が 60 秒超 / 旧版が的中していた型で「誤り」が別の入力者から 2 件。PFS が記録と通知を出し、GitStation にデプロイのイベントが残る。
- 人の判断: カナリア・全体の段で品質が落ちたと approver が判断した。
- 段の展開の失敗（FG-DEPLOY-0005 / 0007、runbook `deploy`）で、ターゲットが前の版に戻らなかった（FG-DEPLOY-0006、alert）。

## Impact

- 戻しは **既に人が承認した版に戻るだけ**（計画 17 §8 の問い 2）。自動の戻しは承認済みの runbook の手順として動く。
  素アラート（AI を通らない通知）は影響を受けない。通常 SEV4。月 2 回以上の自動の戻しはゲートの数字の見直しの合図（計画 17 §5.2）。
- FG-DEPLOY-0006（戻しも失敗）は AI ノードが中途半端な束の状態。SEV3（全体の段なら SEV2）。

## Diagnose（読み取りのみ）

1. どの束からどの束へ: PFS の版ごとの指標と戻しの記録（PFS の `AI-*`）。戻しの先が **直前に承認された** リリースか
   （その前のタグ `assets/v…` と、その `pfs-ai-prod` の承認された `forge.deployments` の行）。
2. GitStation のデプロイ:
   `SELECT id, environment_id, sha, status, error_code, failed_target, previous_sha, approved_by, finished_at FROM forge.deployments WHERE project_id = <id> ORDER BY id DESC LIMIT 10;`
   と runbook `deploy` の Diagnose。
3. 戻した理由の判定: 影・カナリアの新旧の比較（同じ `event_id` の対、計画 17 §1.9）で、落ちた型と改善要求。
4. それが **既にベンチにあるケースと同じ型** なら「回帰の漏れ」（計画 17 §1.12）: どのケースが緑だったのに本番で外れたか。

## Mitigate `[MUTATING — needs approval]`

1. **自動の戻しの後**: 戻った束で指標が戻ったことを確かめる（Verify）。新しい束は展開を止めたまま（次の段の承認をしない）。
2. **手動の戻し**（approver）: 直前に承認されたリリースの SHA を、戻す段の環境に `POST /api/v1/projects/{id}/deployments` で要求し、
   別の approver が承認する（runbook `deploy` の Mitigate 5。要求者は承認できない）。承認されていない版・動いたタグへは戻さない。
3. **戻しも失敗（FG-DEPLOY-0006）**: runbook `deploy` の Mitigate 6。PFS の AI ノードの束はローダーの手順（PFS の `iotpf-ai-service` の runbook
   「ローダーの失敗」）で確かな版を再登録する。レジストリの行を手で書かない・消さない。
4. **フィードバック**: 戻しの原因を改善要求にし、外れを再現するケース（`reproduction`）を足す MR を作る。「回帰の漏れ」ならベンチのケースを足す
   （評価役の見逃しを eval にするのと同じ規則）。

## Verify

- PFS の版ごとの指標（劣化の率・スキーマ不合格・critical の p95）が、戻した束の通常の値に戻った。
- `forge.deployments` の戻しの行が `succeeded`、または自動の戻しの記録が PFS に残っている。
- 戻しの原因の改善要求と、ケースを足す MR がある。

## 関連

- 計画 17 §1.9・§1.11・§1.12・§5.2・§5.3・§8、ADR-0031（デプロイ）
- runbook `ai-asset-release`、`deploy`、`ai-eval-bench`
