# Runbook: ai-model-update（関連: FG-CI-0030 ModelStoreUnverified（alert）、FG-AIASSET-0001 `model_latest` / `model_digest_missing`、FG-AIASSET-0003）

計画 17 §6 のモデルの更新（新しいモデル・Ollama の版・ベンチのイメージの版を入れる）を **普通の変更として** 行う手順と、
その途中で GPU の Runner のモデル置き場の確かめ（FG-CI-0030、**alert**）が止めたときの扱い。モデルのファイルは外から取らない
（オンプレのモデル置き場に管理者が入れる）。ホストの変更とマージはすべて人の承認のあと。

## Trigger

- 計画的な更新: PFS の approver が新しいモデル（例 `qwen3:14b` → 次の版）・Ollama の版・ベンチのイメージを入れると決めた。
- GPU の Runner のログ（`journalctl -u forge-runner`、JSON）に FG-CI-0030（alert）: モデル置き場がマニフェストと合わない。
  更新の手順の途中（ファイルを入れたがマニフェスト・`manifest_sha256` を直していない）で起きることが多い。
- `gstn ai-asset lint` / CI の lint が FG-AIASSET-0001 `model_latest`・`model_digest_missing`・`model_ref_invalid`（`models/models.toml`）、
  `image_not_pinned`（`bench/bench.toml`）。
- モデルを変えた MR の `forge/ai-eval` が FG-AIASSET-0003 `baseline_absent` / `baseline_models_mismatch`: 新しいモデルの基準がまだ無い。

## Impact

- 更新そのものは本番を変えない（本番を変えるのはリリースの承認。runbook `ai-asset-release`）。
- FG-CI-0030 の間、GPU の Runner はジョブを動かさない（起動のときなら Runner が終わる）。`forge/ai-eval` は `error`（FG-AIASSET-0002）になり、
  `pfs-ai/assets` の MR は待つ。通常 SEV4、夜間のベンチが 2 晩続けて動かないなら SEV3。
  **説明の付かないモデル置き場の変更はセキュリティの事象**（SEV2、`/incident`）。

## Diagnose（読み取りのみ）

1. FG-CI-0030 の `reason`・`file`・`expected`・`actual`（Runner のログ、ジョブのログの最後の行）。読み方は runbook `ci-runner` の
   「GPU の Runner とモデル置き場」。
2. モデル置き場を読み取りだけで確かめる:
   `sha256sum /srv/forge-models/forge-models.sha256`（`/etc/forge-runner/config.toml` の `manifest_sha256` と比べる）、
   `cd /srv/forge-models && sha256sum -c --quiet forge-models.sha256`、`find /srv/forge-models -type l -o ! -type f ! -type d`、
   `find /srv/forge-models -perm /022`。
3. 誰がいつ変えたか: ホストの監査ログ、`ls -l --time-style=full-iso`。変更の申請（この runbook の手順の記録）と突き合わせる。
   申請が無ければ **以後の手順を止め**、`/incident` を開く。
4. `pfs-ai/assets` の `main` の `models/models.toml` の `name:tag@sha256:<digest>` と、置き場の Ollama の `manifests/` が指すダイジェストが同じか。

## 手順（更新。各段は人の承認のあと） `[MUTATING — needs approval]`

1. **取得**: 承認された経路（社内のミラー・持ち込みの媒体）で、モデルのファイルを GPU の Runner のホストの **一時の場所**（置き場の外）に置く。
   ダイジェストを提供元の値と照合する。`latest` やダイジェストの無い参照は使わない（lint が拒む）。
2. **置き場への追加**（GPU の Runner のホスト。ジョブが動いていない時間に。Runner を止めてから）:
   `sudo systemctl stop forge-runner` → ファイルを `root:root`・ディレクトリ 0755・ファイル 0644 で `/srv/forge-models` に写す
   （シンボリックリンク・ハードリンクにしない）→ マニフェストを作り直す:
   ```sh
   cd /srv/forge-models && sudo sh -c 'find . -type f ! -name forge-models.sha256 -printf "%P\n" | sort | xargs -d "\n" sha256sum > forge-models.sha256'
   sha256sum /srv/forge-models/forge-models.sha256
   ```
   → その値を `/etc/forge-runner/config.toml` の `manifest_sha256` に書く → `forge-runner check`（全ファイルの sha256。1 時間まで）→
   `sudo systemctl start forge-runner`。旧モデルは新しい基準が入り、リリースが全体に出るまで **消さない**（戻しに要る）。
   戻し: 写す前の置き場とマニフェスト・`manifest_sha256` に戻して再起動。
3. **PFS の AI ノード**にも同じダイジェストのモデルを入れる（PFS の `iotpf-ai-service` の runbook。GitStation の外）。
4. **基準の MR を先に**（ADR-0034 Amendment 1: 基準はベースのブランチとの merge base から読むので、同じ MR の基準は自分の判定に効かない）:
   新しいモデルで全ケースを各 5 回動かした報告を `evals/baselines/<新しい models>.json` として入れる MR を作る。`evals/baselines/**` は保護パスなので
   approver（人）の承認が要る。マージする。
5. **モデルの MR**: `models/models.toml` の参照を `name:tag@sha256:<digest>` で変える（Ollama の版・ベンチのイメージを変えるなら `bench/bench.toml` の
   ダイジェストも。出力が変わりうるので同じ扱い）。保護パス: approver の承認。MR のベンチは新旧の両方で全ケース各 5 回（計画 17 §1.6）。
   `forge/ai-eval` は 4. の基準と比べる。比較表（指標の差・回帰と改善のケース・時間）を approver が読む。
6. **展開**: 通常の束と同じ 3 段（runbook `ai-asset-release`）。モデルの変更は影を 7 日に延ばす。

## Mitigate（FG-CI-0030 が出たとき） `[MUTATING — needs approval]`

- 変更が手順 2 の途中と確認できた: 手順 2 を最後まで行う（マニフェスト・`manifest_sha256` を直して Runner を再起動）。
- 確認できない: 置き場を **直さず** `/incident`（SEV2）。証拠（`ls -l`・`sha256sum` の出力）を残してから、承認したモデルだけで置き場を作り直す。
- 置き場を Runner の利用者が書けるようにして逃げない（FG-CI-0030 `writable` / `owner_untrusted` はそのための検査）。

## Verify

- Runner のログに FG-CI-0030 が無く、GPU のジョブのログに `model store verified: <n> file(s)`。
- 基準の MR とモデルの MR がマージされ、`main` の夜間のベンチで `forge/ai-eval` が `success`（FG-AIASSET-0003 が出ない）。
- `gstn ai-asset lint` が `models/models.toml`・`bench/bench.toml` について何も出さない。

## 関連

- 計画 17 §6・§5.3・§5.4、ADR-0034（基準と比較）、ADR-0035（lint の `model_*` の規則）、ADR-0029 の追記 2026-11-14（GPU の Runner）
- runbook `ci-runner`（GPU の Runner とモデル置き場、FG-CI-0029 / 0030）、`ai-eval-bench`（`forge/ai-eval` の `error`）、`ai-asset-release`
