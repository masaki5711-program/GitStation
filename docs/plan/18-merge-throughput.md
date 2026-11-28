# 計画 18 — マージ経路のスループット（30 倍と、限界に向けて）

> **2026-11-17 利用者の言葉（根拠）**
> - 「30倍くらいいけると良いが」「限界性能を目指して」: 測った値の 30 倍以上、その先は安全が許す限り。
> - 「推奨で進めて」（常設の指示）: §5 の 4 つの決定を推奨で決めた。
> - 「1日のマージ数が1万でも耐えられるってことですか」: §1 の注と、S0 の耐久の場面 E にした。
> - 最終の配備は PFS の Linux サーバ。この PC（Windows）は試験の段。

状態: ドラフト 2026-11-17。決定: ADR-0037。測定: `docs/handoff/2026-11-17-merge-load-test.md`。負荷試験: `ops/loadtest/`。

## 1. 場面ごとの目標

基準 = 測定した run1r（handoff §1）。目標 = 同じ試験環境（Windows ホスト、serve 1 つ、worker 1 つ、ランナー 1 つ）で S7 の後。「最終」= S10 の後。

| 場面 | 基準（測定） | S7 の後の目標 | 最終（S10、推定） | 30 倍に届くか |
|------|--------------|---------------|-------------------|---------------|
| A: 文書 200 件、50 件の波を 30 s 間隔で 4 回 | 3.8/分。作成からマージまで p50 2467 s、p95 3123 s。全体 3196 s | 全体 ≤ 150 s、p50 ≤ 15 s、p95 ≤ 40 s | 同じ（到着で決まる） | p50: 160 倍。全体は 90 s の到着の予定で決まる（≥ 21 倍）— だから A-burst |
| A-burst（新規、S0）: 文書 200 件を一度に | —（≈ A） | 捌く速さ ≥ 200/分（最後の作成から 60 s 以内に 200 件がマージ） | ≥ 400/分 | 3.8/分の ≥ 50 倍 |
| A-burst 実測（S9・S10 の後、2026-11-28、開発の PC、`docs/handoff/2026-11-28-merge-load-after-s10.md`） | 深さ 1: 捌く速さ 63.3/分、深さ 2: 56.3/分。200/200 マージ、`main` の完全性 ok | **未達**（≥ 200/分） | **未達**（≥ 400/分）。1 件あたり `lock_wait_ms` + `land_ms` 252 / 273 ms（目標 ≤ 15 ms）。文書のキューでは深さ 2 のバッチが 1 つもできない | 3.8/分の約 16 倍 |
| B: 1 つの文書に MR 30 件（15 件が重なる） | 16 件マージ、14 件停止。p50 222 s。3.9/分 | 16/14 の内訳は同じ。p50 ≤ 15 s | ≤ 10 s | 遅延で 15〜22 倍（14 件の衝突は入力で、スループットではない） |
| C: システム MR 40 件、1 秒の CI、原因 3 件、ランナー 1 つ | 37/40、2.5/分、p50 659 s、MR パイプライン 291 本 | ≥ 25/分。p50 ≤ 90 s。MR パイプライン ≤ 45。37/3 の内訳は同じ。main は赤にならない | ≥ 60/分 **ランナーが 3 件以上を同時に動かす場合だけ**（投機の深さ 3） | 直列のランナー 1 つでは **届かない** — CI の周期で決まる（§3） |
| D: 同じ主体の並列の push 20 件 | 4/20 が成功 | 20/20。p95 ≤ 8 s。`FG-GIT-0016` なし | 同じ | 該当なし |
| MR の作成、同じリポジトリに 50 並列 | p50 200 ms、p95 661 ms、84/200 が 503 | p95 ≤ 100 ms。503 なし | 同じ | 6.6 倍（p95） |
| E: 耐久（新規、S0）: 8 時間の割合で 10 000 マージ | —（未測定） | **目標**: 持続 ≥ 21/分、100/分の突発。滞留が増えない | 同じ | 目標であり、測定ではない |

注（2026-11-17 の利用者の問い）: 「1日のマージ数が1万でも耐えられるってことですか」。8 時間で 10 000 件は 10 000 / 480 分 ≈ 20.8 件/分 = 持続 ≥ 21/分。S7 の目標（≥ 200/分）の約 10 分の 1 なので、速さとしては収まる見込みだが、これは見込みである。場面 E を S0 に足し、実際に 10 000 件を流して確かめる（§3 S0）。E の数字（≥ 21/分、100/分の突発、滞留が増えない）は **目標** で、測定値ではない。

毎回の実行で、handoff と同じ完全性の検査も確かめる: すべての revision が `main` にある、衝突マーカーがない、`main` の先端で `forge/doc-lint` が success（新規: `main` のすべての first-parent コミットで）、赤の `main` パイプラインがない。

## 2. モデルと限界

### 2.1 コードから読んだ事実（file:line、この worktree）

- タイマー: キューの `waiting` のポール 10 s（`crates/forge-review/src/merges/queue/worker.rs:99`、適用は `:241-247`）。キューの idle 2 s、逐次の手番のループ 1 本（`crates/forge-cli/src/serve_merge_queue.rs:25`、`:104-131`）。自動マージ 5 s（`crates/forge-cli/src/serve_auto_merge.rs:24`）、再確認 20 s、1 手番 50 件（`crates/forge-review/src/merges/auto.rs:51-53`）。ジョブのポール 1 s、回収 30 s（`crates/forge-cli/src/worker/jobs.rs:45`、`:47`）。マージ可能性の掃除 30 s（`crates/forge-cli/src/worker/mergeability_sweep.rs:69`）。sequencer 200 ms（`crates/forge-cli/src/worker/mod.rs:146`）。ランナーのロングポールの確認 1 s（`crates/forge-pipelines/src/runners.rs:50`）。
- LISTEN/NOTIFY は outbox にだけある（`crates/forge-db/src/sequencer/mod.rs:138`、`crates/forge-db/src/subscribers/wake.rs`）。
- target への push ごとのファンアウト: ジョブ 50 件 + 残りは `unchecked`（`crates/forge-db/src/review/refresh.rs:144-211`、`crates/forge-core/src/config/review.rs:44`）。衝突なしの再確認ごとにマージ結果を書き、MR パイプラインを積む（`crates/forge-db/src/review/merge_results.rs:194`、`:278`、`:320-353`）。
- 1 件ごとの git 呼び出し: 組み立て `merge_base` + `changed_paths` + `merge_tree` + `commit_tree`（`worker.rs:508-647`）。着地 `read_ref` + 承認の git の事実 + トランザクション 2 つ、バッチごとに CAS 1 回（`worker.rs:1245-1519`、`:1522-1593`）。
- バッチの上限はコードもスキーマも 4（`crates/forge-core/src/merge_queue.rs:10`、`crates/forge-db/migrations/20261213100000_merge_queue.sql:55-56`）。
- 列車の判定はパイプラインだけ。doc-lint は MR の head にだけ（`worker.rs:740-799`、`crates/forge-checks/src/doc_lint.rs:1-11`）。
- MR の作成は、トランザクションの前に browse のリミッタで両方のブランチの先端を読む（`crates/forge-review/src/lib.rs:549-588`、`crates/forge-git/src/browse/mod.rs:168`、`:412-416`）。

### 2.2 推定（S0 の測定で置き換える）

| Id | 量 | Windows | Linux |
|----|----|---------|-------|
| E1 | `git` の起動 1 回（小さなリポジトリ、キャッシュが温かい） | 25 ms | 4 ms |
| E2 | 起動以外の `merge-tree` の仕事（小さな diff） | 20 ms | 15 ms |
| E3 | SQL 文 1 つ、localhost の PG | 0.5 ms | 0.3 ms |
| E4 | 1 件ごと、組み立て（起動 4 回 + E2） | ≈ 120 ms | ≈ 31 ms |
| E5 | 1 件ごと、ロックの下の着地（起動 3 回 + 文 ≈ 25） | ≈ 90 ms | ≈ 20 ms |
| E6 | バッチごとの固定費（先端の読み、列車の ref、バッチの行、ロック、CAS、後始末） | ≈ 160 ms | ≈ 35 ms |
| E7 | MR ごとの `prepare` ジョブ（起動 ≈ 12 回） | ≈ 0.4 s | ≈ 0.1 s |
| E8 | 列車の doc-lint、索引が温かい、変更した文書 32 件 | ≈ 0.3 s | ≈ 0.1 s |
| E9 | CI の周期、1 秒のジョブ、podman の起動（試験環境） | ≈ 5 s（起動 2〜3 s + ジョブ 1 s + 段） | 同じ |

### 2.3 限界

- **文書、1 つのリポジトリ**（マージのロック `merge:<repo>`）: cycle(B) = E6 + B·(E4 + E5) + E8。パイプラインなしで、B=32 → 0.16 + 32·0.21 + 0.3 ≈ 7.2 s → **≈ 265/分 Windows**、0.035 + 32·0.051 + 0.1 ≈ 1.8 s → **≈ 1070/分 Linux**。S10（ロックの区間が起動なし、E5 → ≈ 10 ms。バッチ k+1 の組み立てを k の着地と並べる）では、1 件あたり `1 / max(E4, E5)` が上限: **≈ 500/分 Windows、≈ 1900/分 Linux**。
- **何が縛るか**: (1) プロセスの起動コスト（E1）— Windows は Linux のおよそ 6 倍。(2) 1 件ごとの `merge-tree`（変更した entry で増え、リポジトリの大きさでは増えない）。(3) 単一のマージのロックと、リポジトリごとにバッチ 1 つにつき CAS 1 回（CAS 自体はバッチごとに起動 1 回 — 無視できる）。(4) PG の往復は起動がなくなった後だけ（1 件あたり文 ≈ 25 ≈ 10 ms）。(5) 自動マージの enqueue、MR ごとに git の事実 1 回 + トランザクション 1 つ（≈ 0.1 s Windows）— S3 で ×4 に並列化。
- **本番の OS**: 本番は Linux（PFS の Linux サーバ）で、Windows の試験の段は限界を示さない。限界は Linux でも測る（S0 の `spawn-bench` と場面 A-burst・E を Linux のホストで動かす）。上の Linux の列は、測るまでは推定である。
- **リポジトリをまたぐとき**: `merge_queue.workers` に対して線形で、CPU に当たるまで（今は最悪の時点で 1 コアの 62 % 以下）。
- **CI のあるリポジトリ**: スループット ≈ min(D, R/J) · B · s / (T_ci + T_o)（ADR-0037 d3）。S5 の後の試験環境 C: B=4、原因 3 件の 40 MR で列車 16 本（緑の経路 10 + 原因ごとに ≈ 2）、16 · E9 ≈ 80 s → **≈ 30/分**。実 CI（5 分、ランナー 4 つ、D=4）: ≈ 2.3/分 — ここでの限界は、ランナー × バッチ / CI の時間。

## 3. 段

各段は `main` への 1 つの MR で、独立にマージでき、挙動が変わるところは設定キーの後ろに置く（戻せる）。「先に失敗させる」試験はコードの前に書く（tdd-guide）。エラーコードの番号は暫定: 実装のとき次の空き番号を取る。名前は拘束する。

### S0 — 測る（挙動の変更なし）

- **範囲**: 既存のログ行（`forge.merge_queue.batch_built` / `batch_landed`）に、バッチごとの段階の時間（`build_ms`、`lint_ms`、`lock_wait_ms`、`land_ms`、`git_spawns`）。ジョブの runner の結果のログに、ジョブの claim の遅延（`ready_to_claim_ms`）。負荷試験: 場面 `a-burst`、`spawn-bench`（試験用リポジトリで `git rev-parse HEAD` を 100 回と `merge-tree` を 100 回）、`facts` の `merge_queue_batches` のタイムスタンプからバッチごとの所要時間。**場面 E（耐久）: 8 時間の割合（≥ 21/分 の持続、100/分の突発）で 10 000 マージを流し、滞留が増えないことを確かめ、DB の大きさとリポジトリの大きさの増え方を記録する。E の数字は目標であり、測定ではない。**
- **ファイル**: `crates/forge-review/src/merges/queue/worker.rs`（ログの項目だけ）、`crates/forge-db/src/jobs/reports.rs`、`ops/loadtest/merge-load.mjs`、`ops/loadtest/lib.mjs`、`ops/loadtest/README.md`。
- **先に書く試験**: `batch_landed_log_carries_phase_timings`（ログを捕まえる試験）、`job_outcome_log_carries_claim_latency`。
- **コード / キー / マイグレーション**: なし。
- **証明**: `a`、`a-burst`、`c`、`spawn-bench`、`e` を動かし、§2.2 の E1〜E9 をこのファイルの測定値に置き換える。本番と同じ Linux のホストでも動かす。
- **見込みのスループット**: 変わらない（進行中のブランチの後の基準: 文書は定常で 8〜16/分）。

### S1 — 列車の doc-lint の関門（安全、全件の lint、バッチ 4）

- **範囲**: `TrainDocLint` trait（forge-core）、最後の段の木に `forge_doc::lint::lint` を使う forge-checks の実装（全件を読む、キャッシュはまだなし）、`forge serve` での注入。判定は「土台に対して新しい問題がない」。リポジトリごとのキー `merge_queue.doc_lint_strict`（既定 false）が true なら「きれいであること」（QMS のリポジトリ向け）。赤 → 列車の段に対する接頭辞の二分探索 → 最初の悪い member を FG-CI-0032 で外す。列車の sha に `forge/doc-lint` のチェック実行。
- **ファイル**: `crates/forge-core/src/train_lint.rs`（新規）、`crates/forge-core/src/lib.rs`、`crates/forge-checks/src/doc_lint.rs`（+ `train_lint.rs`）、`crates/forge-review/src/merges/queue/worker.rs`、`crates/forge-cli/src/serve*.rs`（配線）、`crates/forge-errors/errors.toml`、`ops/runbooks/merge-queue.md`。
- **先に書く試験**: `a_train_whose_members_break_a_reference_together_never_lands`（MR1 が文書 X を消し、MR2 が `[[X]]` を足す。head はどちらも緑 — F8 の穴）、`the_first_bad_step_is_removed_and_the_rest_land_in_order`、`a_main_with_old_problems_still_moves_when_nothing_new_breaks`、`a_strict_repository_does_not_land_while_the_base_has_problems`（`merge_queue.doc_lint_strict`）、`an_unlintable_train_is_abandoned_and_reaped_on_the_third_time`（FG-DOC-0002 `scope=train`、その後 FG-CI-0024）、`the_train_lint_answers_like_gstn_doc_lint`（同じバイト列、同じ報告）。
- **コード**: FG-CI-0032 MergeQueueDocLintFailed（info）。FG-DOC-0002 / FG-CI-0024 のカタログの文に `scope=train` を足す。**キー**: `merge_queue.train_lint_budget_ms`、`merge_queue.doc_lint_strict`。**マイグレーション**: なし。
- **証明**: 負荷試験の新しいケース `a-xref`（単独では緑で、一緒だと参照を壊す MR の組）: `main` のどの first-parent コミットにも壊れた参照が 0。原因ごとに FG-CI-0032 が 1 回。
- **見込みのスループット**: doc のバッチごとに −0.3 s〜−1 s（全件の lint、E8 が冷たい）→ まだ 8〜16/分。バッチが大きくなる前に正しさの穴を塞ぐので、最初に着地させる。

### S2 — 起床のハブとジョブの通知

- **範囲**: `forge_db::wake::WakeHub`（`subscribers/wake.rs` を一般化、プロセスごとに LISTEN 接続 1 本、チャネル Jobs/MergeQueue/Refs/Events）。`jobs::enqueue_on` が `pg_notify('forge_jobs', kind)` を送る。ジョブの runner（4 スロットすべて）が `jobs.safety_poll_ms` で Jobs を待つ。ランナーのロングポールが `ci.job` の種類で起きる。安全網の発見で `FG-DB-0024`。通知キューの使用量の doctor 検査（`FG-DB-0023`）。
- **ファイル**: `crates/forge-db/src/wake.rs`（新規）、`crates/forge-db/src/subscribers/wake.rs`（委譲）、`crates/forge-db/src/jobs/queue.rs`、`crates/forge-db/src/jobs/doctor.rs`、`crates/forge-cli/src/worker/jobs.rs`、`crates/forge-pipelines/src/runners.rs`、`crates/forge-core/src/config/keys.rs`、`crates/forge-errors/errors.toml`、`ops/runbooks/job-queue-backlog.md`、`ops/runbooks/db-unavailable.md`。
- **先に書く試験**: `an_enqueued_job_is_claimed_without_waiting_for_the_poll`（安全網の周期 10 s で claim < 200 ms）、`a_rolled_back_enqueue_wakes_no_one`、`a_reconnected_listener_looks_at_once`、`a_safety_net_find_logs_wakeup_missed_once_per_minute`、`doctor_alerts_when_the_notify_queue_is_half_full`（使用量はスタブ）。
- **コード**: FG-DB-0023 NotifyQueueBacklog（alert）、FG-DB-0024 WakeupMissed（warn）。**キー**: `jobs.safety_poll_ms`（10000）。**マイグレーション**: なし。
- **証明**: `a`: `ready_to_claim_ms` の p95 ≤ 50 ms（S0 の項目から）。ジョブのスループットは変わらないか良くなる。
- **見込みのスループット**: ジョブの段ごとに平均 −0.5 s（ポール 1 s / 2）。doc の経路でマージごとにジョブの段が ≈ 3 つ → 周期 15〜30 s → ≈ 13〜28 s。**≈ 9〜18/分**。これだけでは小さい。大きなタイマーは S3 が取り除く。

### S3 — キューと自動マージの起床。並行するキューの worker

- **範囲**: `MergeQueue::wake(queue_id)`（プロセス内の `Notify`）を enqueue/dequeue/resume のコミットの後に呼ぶ。列車のパイプラインが終わったとき、またはその作成のジョブがパイプラインを見つけなかったときの `forge_merge_queue` の NOTIFY。キューの手番のループは起床か安全網の周期（idle 30 s、waiting 60 s）で待つ。`merge_queue.workers` 個の手番タスク。自動マージは `forge_events_sequenced` を購読し、名指しされた MR だけを判定（同時に 4 件まで）、全件の走査は 30 s ごと。
- **ファイル**: `crates/forge-review/src/merges/queue/worker.rs`（`tick` の `next_in`、`wake`）、`crates/forge-review/src/merges/queue/mod.rs`、`crates/forge-review/src/merges/auto.rs`、`crates/forge-cli/src/serve_merge_queue.rs`、`crates/forge-cli/src/serve_auto_merge.rs`、`crates/forge-db/src/pipelines/`（終わりの状態の通知）、`crates/forge-core/src/config/keys.rs`、`ops/runbooks/merge-queue.md`、`ops/runbooks/auto-merge.md`。
- **先に書く試験**: `an_enqueue_builds_a_batch_without_waiting_for_the_idle_period`、`a_finished_train_pipeline_lands_without_waiting_for_the_poll`、`a_lost_wake_is_recovered_by_the_safety_net`（起床を抑える → 安全網の周期内に着地 + FG-DB-0024）、`auto_merge_decides_a_merge_request_right_after_its_check_completes`（S3-T3: チェック実行の完了が起床を生むことも証明する。イベントがなければ、この段でチェック実行の書き手に通知を足す）、`two_queues_progress_concurrently`。
- **コード**: 新規なし。**キー**: `merge_queue.safety_poll_ms`（30000）、`merge_queue.waiting_safety_poll_ms`（60000）、`merge_queue.workers`（2）、`auto_merge.safety_poll_ms`（30000）。**マイグレーション**: なし。
- **証明**: `a` + `a-burst`: 4 件のバッチで `build→landed` の p50 ≤ 3 s。doc のリポジトリで、`built` から判定までバッチが 1 s を超えて待たない。
- **見込みのスループット**: cycle(4) = E6 + 4·(E4+E5) ≈ 仕事 1.0 s。ただし着地のたびに開いた MR を 50 件まで再確認し（≈ 50 · 0.15 s / スロット 4 ≈ 1.9 s）、新しいマージ可能性がないと MR は enqueue できない → enqueue が行きつ戻りつする。推定 **40〜80/分**（10〜20 倍）。行きつ戻りつは S4 の標的。

### S4 — 変更パスを見たマージ可能性（持ち越し）

- **範囲**: 版の段が head のパスの集合を保存。キューと通常のマージの着地が、着地したパスの集合を `repository.pushed` のペイロードに書く。refresh の購読側は `may_interact` な MR だけを再確認し、残りは 1 つの文で持ち越す。MR のページのラベル（ja/en/id）。`review.path_filter` の停止スイッチ。
- **ファイル**: `crates/forge-core/src/merge_paths.rs`（新規、純粋）、`crates/forge-db/migrations/<ts>_merge_request_mergeability_paths.sql`、`crates/forge-db/src/review/refresh.rs`、`crates/forge-db/src/review/mergeability/{mod,request}.rs`、`crates/forge-review/src/mergeability.rs`、`crates/forge-review/src/merges/queue/worker.rs`（着地したパス: バッチごとに `diff --name-only --no-renames` 1 回）、`crates/forge-review/src/merges/` の通常のマージの記録、`crates/forge-git/src/review_inputs.rs`（必要なら削除パスの版）、`web/src/` の MR のページと i18n のキー、`ops/runbooks/merge-request-mergeability.md`。
- **先に書く試験**: 性質 `may_interact_matches_its_sql_twin`、`a_file_dir_clash_is_an_interaction`（`a` と `a/b`）、`a_landed_directory_deletion_rechecks_mrs_adding_under_it`、`a_gitattributes_change_rechecks_everything`、`disjoint_mrs_are_carried_forward_with_their_computed_tip`、`a_carried_mr_that_conflicts_in_the_train_is_removed_with_fg_review_0006`、`an_incomplete_path_set_is_rechecked`、`old_payloads_without_paths_recheck_like_today`。
- **コード**: 新規なし（FG-REVIEW-0006 の診断の文）。**キー**: `review.path_filter`（true）、`review.paths_max`（5000）。**マイグレーション**: `merge_request_mergeability_paths`（列 `head_files text[]`、`head_dirs text[]`、`paths_complete boolean`、`computed_target_sha text`。追加のみ、null 可 → null =「不明 = 再確認」）。
- **証明**: `a`: 着地ごとのマージ可能性のジョブ ≈ 0（DB の `facts`: マージ可能性の種類のジョブ / 着地）。`b`: 重なる 14 件は今も止まり、行が重ならない 15 件は今もマージされる（同じファイル → 常に再確認: 正しい）。
- **見込みのスループット**: doc の着地がファンアウトしなくなる。cycle(4) ≈ 1.0〜1.5 s → **≈ 160〜240/分**。控えめに **≥ 120/分（≥ 30 倍）** — A-burst で 30 倍に届く最初の段。

### S5 — MR パイプラインは新しい head のときだけ

- **範囲**: マージ結果の書き込みは、head が変わったときだけ MR パイプラインを積む（target の移動では積まない）。`ci/pipeline` が古い直接のマージは FG-CI-0013 `hint=merge_queue` を返す。そのような target での web の「マージ」は「準備ができたらマージ」になる。「今の target でパイプラインを動かす」操作（認可: 既存のパイプライン実行の Action、権限マトリクスに行）。
- **ファイル**: `crates/forge-db/src/review/merge_results.rs`、`crates/forge-review/src/merges/evaluate.rs`（hint）、`crates/forge-web/src/`（要求に応じた実行の route が新規なら、とマトリクスの行）、`web/src/` の MR のページ、`crates/forge-errors/errors.toml`（FG-CI-0013 の文）、`ops/runbooks/merge-blocked.md`。
- **先に書く試験**: `a_target_move_creates_no_merge_request_pipeline`、`a_new_head_creates_one`、`ci_mr_pipeline_on_target_move_restores_the_old_behavior`、`direct_merge_on_a_stale_pipeline_hints_the_queue`、要求に応じた実行の権限マトリクスの行。
- **コード**: 新規なし。**キー**: `ci.mr_pipeline_on_target_move`（false）。**マイグレーション**: なし。
- **証明**: `c`: MR パイプライン ≤ 45（基準 291）。37/40 がマージされ、同じ 3 件が FG-CI-0023 で外れる。`main` は赤にならない。
- **見込みのスループット（C）**: ランナーが MR パイプラインで飽和しなくなる → 列車の周期 ≈ E9 ≈ 5 s。16 本の列車 → 40 件で ≈ 80 s → **≈ 30/分（12 倍）**。

### S6 — 1 つの `merge_request.prepare` ジョブ。conflict-scan の前段の絞り込み

- **範囲**: forge-cli の合成ハンドラ（refresh → mergeability → risk → spec_index（doc-lint を含む）→ MR パイプラインの判断）、head の一覧 1 回 + `cat-file --batch` 1 回、head の sha ごとに冪等な小段。版の購読側は 4 つのジョブの代わりに `prepare` を積む。`conflict_scan` の相手は `may_interact` で絞る（S4 が要る）、低優先度、影響する相手が上限を超えたらログを出して飛ばす。
- **ファイル**: `crates/forge-cli/src/worker/{jobs,prepare}.rs`（新規 `prepare.rs`）、`crates/forge-cli/src/worker/{review,risk,spec_index}.rs`（小段の関数を公開）、`crates/forge-db/src/review/prepare.rs`（新規: ジョブの種類、重複排除のキー `prepare:<mr>:<head>`）、`crates/forge-db/src/review/refresh.rs` と版の購読側、`crates/forge-db/src/review/conflicts/`、`ops/runbooks/job-dead-lettered.md`。
- **先に書く試験**: `a_version_enqueues_one_prepare_job`、`a_failed_risk_step_retries_only_risk`、`prepare_writes_the_same_check_runs_as_the_four_jobs`（golden の比較）、`conflict_scan_tries_only_interacting_peers`、`a_doc_mr_without_forge_ci_yml_gets_no_pipeline_job`。
- **コード**: 新規なし（小段は自分のコードを保ち、`step` の文脈を足す）。**キー**: なし。**マイグレーション**: なし。
- **証明**: `a-burst`: 作成から enqueue まで p95 ≤ 5 s（基準 2971 s）。MR あたりのジョブ ≤ 2（prepare + spec_drift）。
- **見込みのスループット**: MR あたり claim ≈ 3 回と木の読み 3 回を除く（Windows で MR あたり worker の時間 ≈ 0.6 s）→ キューの前の容量 ≈ スロット 4 / E7 ≈ **600 MR/分**。S7 の速さより下では詰まりにならない。

### S7 — 文書の速い経路: バッチ 32、増分の lint、同じ手番での着地

- **範囲**: スキーマの CHECK を 1..64 に。土台にも列車にも `.forge-ci.yml` がないキューは `doc_batch_max` 32、そうでなければ `ci_batch_max` 4。赤の doc バッチは半分にせず接頭辞の二分（S1）。内容アドレスの文書の索引（LRU）と逆引きの参照、全件の `lint()` に対して性質試験。組み立て → lint → マージ結果の判定 → 着地を 1 手番で。`assess_train` による 1 回のリスク判定。
- **ファイル**: `crates/forge-db/migrations/<ts>_merge_queue_batch_limit_64.sql`、`crates/forge-core/src/merge_queue.rs`、`crates/forge-core/src/auto_merge.rs`（`assess_train`）、`crates/forge-checks/src/{train_lint,risk}.rs`、`crates/forge-review/src/merges/queue/worker.rs`、`crates/forge-db/src/review/merge_queue/`、`crates/forge-core/src/config/keys.rs`。
- **先に書く試験**: 性質 `incremental_lint_equals_full_lint`（追加・削除・改名・参照のランダムな編集）、`a_doc_queue_builds_batches_of_32_and_a_ci_queue_of_4`、`a_doc_batch_lands_in_the_tick_that_built_it`、`a_restart_mid_landing_recovers_a_same_tick_batch`（CAS のフックの後で異常終了）、`assess_train_equals_per_step_assess`、`a_32_member_judgement_stays_within_the_budget`、マイグレーションの試験 `batch_limit_accepts_64_rejects_65`。
- **コード**: 新規なし。**キー**: `merge_queue.doc_batch_max`（32）、`merge_queue.ci_batch_max`（4）、`merge_queue.doc_index_bytes`（64 MiB）。**マイグレーション**: `merge_queue_batch_limit_64`（`NOT VALID` + `VALIDATE`、ADR-0030 のレビューの型）。
- **証明**: `a-burst` で ≥ 200/分を捌く。`a` で p50 ≤ 15 s、p95 ≤ 40 s、全体 ≤ 150 s。`a-xref` は今も壊れた参照が 0。`b` は内訳が同じで p50 ≤ 15 s。
- **見込みのスループット**: cycle(32) ≈ 7.2 s Windows → **≈ 265/分（70 倍）**。Linux ≈ 1.8 s → ≈ 1070/分。A（50 件の波）: 波ごとにバッチ 2 つ（32 + 18）≈ 11 s + prepare（50 · 0.4 / 4 ≈ 5 s）→ 波は到着の ≈ 16 s 後にマージされる → 全体 ≈ 90 + 16 ≈ 110 s。

### S8 — MR の作成 < 0.1 s。push の並行

- **範囲**: `RefSnapshotCache`（リポジトリごとに `for-each-ref refs/heads/` の single-flight、serve の ref の書き込みと `forge_refs` で無効にする）、肯定のヒットはキャッシュから、否定は git で確かめる。`prepare` を通知つきで積む。push（D）: 進行中の短い待ちのブランチの後で確かめる。`principal_streams` か push の前の ref のスナップショットがなお即座に断るなら、両方に同じ上限つきの待ちを適用する。
- **ファイル**: `crates/forge-git/src/refs_cache.rs`（新規）、`crates/forge-git/src/browse/mod.rs`（無効化のフック）、`crates/forge-review/src/lib.rs`（`require_branches`）、`crates/forge-web/src/`（push のリミッタ、必要なときだけ）、`crates/forge-errors/errors.toml`（MR の作成についての FG-GIT-0004 の診断 — handoff F9）。
- **先に書く試験**: `fifty_concurrent_creates_on_one_repo_never_answer_503`、`a_branch_pushed_after_the_snapshot_is_found`、`a_deleted_branch_in_the_snapshot_yields_source_branch_missing_after_prepare`、`another_process_ref_write_invalidates_the_cache`、（D）`the_fifth_push_of_one_principal_waits_instead_of_503`。
- **コード**: 新規なし。**キー**: なし（git の待ちのキーを使う）。**マイグレーション**: なし。
- **証明**: `a-burst` の作成 p95 ≤ 100 ms、503 が 0 回。`d` が 20/20。
- **見込みのスループット**: マージの速さは変わらない。クライアントの再試行がなくなる（run1r: 作成ごとに最大 4 回の再試行）。

### S9 — 投機の列車（CI のあるリポジトリ）

- **範囲**: キューごとに `speculation_depth` 個までの生きたバッチ。それぞれ前の列車の先端の上に作る。順にだけ、target が自分の土台のときにだけ着地する。前のものが赤か捨てられたら、後ろは `predecessor_failed` で捨てる。doc のキューは、着地と並べて組み立てるために深さ 2 を使ってよい。
- **ファイル**: `crates/forge-db/migrations/<ts>_merge_queue_speculation.sql`、`crates/forge-db/src/review/merge_queue/`、`crates/forge-review/src/merges/queue/worker.rs`、`crates/forge-core/src/merge_queue.rs`、`ops/runbooks/merge-queue.md`。
- **先に書く試験**: `a_second_train_lands_only_after_the_first`、`a_red_first_train_abandons_the_second`、`a_target_moved_by_a_direct_push_abandons_every_live_train`、`recovery_with_two_live_batches_records_only_what_git_shows`、`depth_1_behaves_like_adr_0030`。
- **コード**: 新規なし。**キー**: `merge_queue.speculation_depth`（1）。**マイグレーション**: `merge_queue_speculation`（`UNIQUE (queue_id, depth) WHERE live`、`parent_batch_id`、`depth` を作った後に、生きたバッチが 1 つの部分一意索引を落とす。同じトランザクション）。
- **証明**: 深さ 3 で、ランナーが 3 件を並行できる `c`: ≥ 60/分。深さ 1 は S5 と同じ。
- **見込みのスループット（C）**: ≈ min(3, R) · 4 · 0.73 / 5 s ≈ **S5 の ≈ 2 倍 → ≈ 60/分**（R ≥ 3 のとき）。R = 1 なら変わらない。

### S10 — 限界: 起動のない着地の区間

- **範囲**: マージのロックの下で、バッチごとに `for-each-ref` 1 回ですべての source の先端を読み、承認の git の事実は組み立てのときに先に計算して（ロックの下で sha の一致で再検証）、`queue_merge_mr_decided` すべてを 1 つのトランザクションに、`finish_merge` すべてを 1 つのトランザクションにまとめる（MR ごとの監査の行とイベントは変えず、順も保つ）。
- **ファイル**: `crates/forge-review/src/merges/queue/worker.rs`、`crates/forge-db/src/review/merges/`（まとめた版）、`crates/forge-review/src/approvals.rs`。
- **先に書く試験**: `a_batched_landing_writes_the_same_audit_rows_and_events_as_per_member`（golden）、`a_source_moved_after_build_is_refused_under_the_lock`、`one_refusal_in_a_batched_decision_rebuilds_without_it`。
- **コード**: 新規なし。**キー**: なし。**マイグレーション**: なし。
- **証明**: `a-burst` で Windows ≥ 400/分。1 件あたり `lock_wait_ms` + `land_ms` ≤ 15 ms。
- **見込みのスループット**: E5 ≈ 90 → ≈ 10 ms。深さ 2 で組み立てを着地と並べると、上限 = 1/E4 ≈ リポジトリごとに **≈ 500/分 Windows、≈ 1900/分 Linux**。

## 4. 順序と並列の worktree

順序: S0 → S1 → (S2 → S3) ∥ S4 ∥ S8 → S5 → S6 → S7 → S9 → S10。

| レーン | 段 | 持つファイル | 衝突する相手 |
|--------|----|--------------|--------------|
| Q（キューの worker） | S1 → S7 → S9 → S10 | `merges/queue/worker.rs`、`forge-core/src/merge_queue.rs`、`forge-checks/src/train_lint.rs` | S3（`tick` の 241〜247 行）と S4（着地したパス）: rebase、小さな hunk |
| W（起床） | S2 → S3 | `forge-db/src/wake.rs`、`jobs/`、`forge-cli/src/{worker/jobs,serve_merge_queue,serve_auto_merge}.rs`、`merges/auto.rs` | Q（上を参照） |
| P（パス） | S4 → S5 → S6 | `forge-db/src/review/{refresh,mergeability,merge_results}`、`forge-review/src/mergeability.rs`、`forge-cli/src/worker/{prepare,review,risk,spec_index}.rs`、web の MR のページ | Q（S4 が着地に呼び出しを 1 つ足す） |
| C（作成） | S8 | `forge-git/src/refs_cache.rs`、`forge-review/src/lib.rs`（`open`、`require_branches`） | なし |

ファイルの衝突なしに並列の worktree で動かせる: **W ∥ P ∥ C**、そして **Q ∥ C**。S0 は、すべての時間のログに触れるので、単独で最初に行く。並列のレーンのマイグレーションは別のタイムスタンプを取り、どれも追加のみ。

## 5. 決定（2026-11-17）

利用者の常設の指示「推奨で進めて」により、次のとおり決めた。

1. **CI が必須の target への直接のマージ**（ADR-0037 d2）: キューへ誘導する（選んだ案。キーで戻せる）。推奨のとおり決定。
2. **テスト用ランナー `docker-test-m4` の並行数**（C の 60/分には 3 件以上の並行が要る）: 不明。コードからは見えない。S0 で測る。S9 の目標は条件つきのまま。
3. **本番ホストの OS**: Linux（利用者: 最終の配備は PFS の Linux サーバ。この PC（Windows）は試験の段）。Windows と Linux では起動のコストで限界がおよそ 4 倍違う（§2.2）。限界は Linux でも測る（S0）。
4. **すでに問題を抱えた `main` での列車の doc-lint**: 既定は「新しい問題がないこと」。QMS のリポジトリには、リポジトリごとのキー（`merge_queue.doc_lint_strict`）で厳しい規則（「きれいであること」）を選べる。推奨のとおり決定。
