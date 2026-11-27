# Runbook: agent-lock（`forge/agent-lock` が赤。関連: FG-AGENT-0007、FG-AGENT-0002、FG-AGENT-0006、FG-AGENT-0008）

計画 16 A7（ADR-0032 Amendment A7）の必須のチェック `forge/agent-lock` が `failure` のとき。MR の head の `.claude/**`・`CLAUDE.md`・
`.mcp.json` が `.gitstation/agent.lock.json` の固定したもの（署名した部品・公開済み・取り消していない）と違う。開発者の PC で動くコードなので、
**直すのはビルダーで作り直すことだけ**（手で lock を合わせない）。

## Trigger

- MR の画面・`gstn mr checks` で `forge/agent-lock` が `failure`。要約の各行は `- <コード> <reason> <subject>`:
  - `FG-AGENT-0007 file_changed` / `file_missing` / `unmanaged_file` / `lock_missing` / `custom_unverified` / `not_a_regular_file` /
    `tree_unreadable`: 管理するファイルが lock と違う（手で書き換えた、足した、消した、symlink にした、大きすぎて読めない）。
  - `FG-AGENT-0002 digest_mismatch` / `bad_signature` / `merge_source_mismatch` / `not_published`: lock そのものが書き換えられた疑い。
    **改ざんの疑いとして runbook `agent-supply-chain` に移る。**
  - `FG-AGENT-0006 revoked`: lock が取り消した版を指す（取り消しの後、更新の MR がまだ）。
- 自動マージの 1 行が `stopped developer_pc`（`forge/risk` の `agent_config`）。`forge/agent-lock` が緑なら `agent_config_locked` になり、
  止めずに Maintainer の承認を待つ（`waiting agent_config_approval`）。

## Diagnose（読み取りのみ）

1. 要約の 1 行目と各行（コード・reason・path）。`forge/risk` の同じ head の要約も見る（`agent_config` の path）。
2. どのコミットで変わったか: `git log -p <target>..<head> -- .claude CLAUDE.md .mcp.json .gitstation`。作者がビルダーのボット
   （`forge-agent-builder <agent-builder@forge.invalid>`）でないコミットが原因。
3. 手元で同じ判定: MR の head を checkout して `gstn agent verify --json`（手元の鍵。取り消しは見ない）。
4. `revoked` なら `gstn part view <slug>` で取り消しの理由と新しい版。

## Mitigate（変更は人の承認のあと）

1. 手の変更が原因なら、その変更を MR から外す（`git revert` か作り直し）。意図した変更なら、部品として公開するか（計画 16 A5）、
   ビルダーで作り直す: プロジェクトの設定 →「エージェント」→ Dry-Run → 「MR を作る」（`POST /api/v1/projects/{id}/agent-builder/compile`）。
2. 取り消した版なら、新しい版を選んでビルダーで作り直す（A9 で自動の更新 MR）。
3. 作り直した MR は `forge/agent-lock` が緑になり、作成者でない Maintainer の人の承認で自動マージが進む。

## Verify

- 新しい head の `forge/agent-lock` が `success`（「Verified: …」）、`forge/risk` が `agent_config_locked`（warn）。
- 開発者の PC: `gstn agent verify` が終了コード 0。

## 残る危険

- 同僚のブランチを checkout するとそこのフックが動く（git の性質、計画 16 §8）。checkout の後は `gstn agent verify`。
