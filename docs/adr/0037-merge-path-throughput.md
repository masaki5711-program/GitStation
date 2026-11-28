# ADR-0037: マージ経路のスループット — イベントで起こす待ち、プロセス内の文書の列車、変更パスを見た再計算

**Date**: 2026-11-17
**Status**: proposed（accepted は計画 18 の S1〜S4 が `/review` で CRITICAL / HIGH なしに通り、負荷試験が各段の目標を示したとき）
**Deciders**: Masaki Fujikawa（ユーザー、2026-11-17「推奨で進めて」（常設の指示）、「30倍くらいいけると良いが」「限界性能を目指して」: 測った値の 30 倍以上、その先は安全が許す限り）、Claude

## Context

測定（`docs/handoff/2026-11-17-merge-load-test.md`、試験環境、Windows ホスト、serve 1 つと worker 1 つ）: 文書の MR 200 件 → 3.8 マージ/分、作成からマージまでの p50 2467 s。ジョブの滞留が消えた後の定常状態は 8〜16/分（4 件のバッチが 15〜30 s ごと）。1 つの文書への MR 30 件は 3.9/分。1 秒の CI ジョブを持つシステム MR 40 件は 2.5/分、MR パイプライン 291 本。並列の push は 4/20。MR 作成の p95 661 ms、200 件中 84 件が即 503。CPU は 1 コアの 62 % 以下、メモリは 110 MB 以下。限界は計算ではなく、順序と待ちにある。

コードから読んだ事実（この worktree、土台は main 59ca1d27）:

| # | 事実 | 場所 |
|---|------|------|
| F1 | バッチを待つ間のキューのポールは 10 s。`waiting`/`paused`/`idle`/エラーは `next_in = poll` | `crates/forge-review/src/merges/queue/worker.rs:99`、`:241-247` |
| F2 | キューの idle 間隔は 2 s。serve ごとに手番のループは 1 本で逐次 | `crates/forge-cli/src/serve_merge_queue.rs:25`、`:104-131` |
| F3 | 自動マージの手番 5 s、再確認 20 s、1 手番 50 MR | `crates/forge-cli/src/serve_auto_merge.rs:24`、`crates/forge-review/src/merges/auto.rs:51-53` |
| F4 | ジョブの実行は 1 度に 1 件、idle のポール 1 s、回収 30 s | `crates/forge-cli/src/worker/jobs.rs:1-4`、`:45`、`:47` |
| F5 | LISTEN/NOTIFY は **outbox にだけある**: sequencer がコミット時に `pg_notify` を送り、購読側は LISTEN し、再接続は「今すぐ見る」（`Woken::Connected`）、dispatcher の予備のポールは 5 s、sequencer のポールは 200 ms。ジョブ・キュー・自動マージには通知がない（調査の「LISTEN/NOTIFY がない」は、これらについてだけ正しい） | `crates/forge-db/src/sequencer/mod.rs:138`、`crates/forge-db/src/subscribers/wake.rs`、`crates/forge-db/src/subscribers/dispatch.rs:26`、`crates/forge-cli/src/worker/mod.rs:146` |
| F6 | target への push のたびに、更新が新しい開いた MR 50 件（`review.mergeability_fanout_max` = 50）のマージ可能性を再要求し、残りを `unchecked` にする（30 s ごとに 50 件掃く）。変更パスの絞り込みはない | `crates/forge-db/src/review/refresh.rs:144-211`、`crates/forge-core/src/config/review.rs:44`、`crates/forge-db/src/review/mergeability/request.rs:17`、`:171` |
| F7 | 衝突なしのマージ可能性は (head, target の先端) ごとにマージ結果を書き、その上で MR パイプラインを積む → target が動くたびに、衝突のない開いた MR 1 件につき MR パイプライン 1 本 | `crates/forge-review/src/mergeability.rs:23-29`、`crates/forge-db/src/review/merge_results.rs:194`、`:278`、`:320-353` |
| F8 | キューの判定は列車のパイプラインだけ。`forge/doc-lint` は `check.spec_index` ジョブが MR の **head** にだけ書く。結合した列車の木は一度も lint されない | `worker.rs:740-799`、`crates/forge-checks/src/doc_lint.rs:1-11` |
| F9 | `forge_doc::lint::lint(doctypes, documents)` は純粋で、参照を調べる前にリポジトリ全体の索引（id、ブロック）を作る | `crates/forge-doc/src/lint.rs:58-140` |
| F10 | バッチの大きさはコードもスキーマも 4 まで（`CHECK (batch_limit BETWEEN 1 AND 4)`）。キューごとに生きたバッチは 1 つ | `crates/forge-core/src/merge_queue.rs:10`、`crates/forge-db/migrations/20261213100000_merge_queue.sql:55-56` |
| F11 | 1 件ごとに、組み立て = `merge_base` + `changed_paths`（隔離）+ `merge_tree` + `commit_tree`。着地 = `read_ref`（source）+ 承認の git の事実 + トランザクション 2 つ。バッチごとに `merge:<repo>` の下で `update-ref` の CAS 1 回 | `worker.rs:508-647`、`:1245-1519` |
| F12 | 自動マージの entry のマージ結果は、列車の段ごと（base → 段）に 90 s の予算で判定する | `worker.rs:49`、`:807-877` |
| F13 | MR の作成は、トランザクションの前に両方のブランチの先端を browse のリミッタ（`max_per_repo` 8）で読む | `crates/forge-review/src/lib.rs:431-450`、`:549-588`、`crates/forge-git/src/browse/mod.rs:168`、`:412-416` |
| F14 | doc-lint の既定の上限: `.md` 2000 件、32 MiB、木の entry 50 000 | `crates/forge-checks/src/doc_lint.rs:64-73` |

マージ済みと仮定する（別のブランチ、ここでは再設計しない）: ジョブの重複排除 + 優先度 + 種類ごとの上限つき 4 worker スロット。`.forge-ci.yml` のないリポジトリでは列車のパイプラインのジョブを作らない。git の上限に当たったとき、即 503 ではなく短く待つ。doc-lint の失敗は自動マージを止める。

制約: main を赤にしない（文書の参照を含む）。変更を失わない。認可の判断点は 1 つ。待ちはすべて有限。失敗はすべて登録済みのコードと `request_id` を持つ。工場は、互いにパスが重ならない小さな文書の MR を多く、実 CI を持つコードの MR を少なく作る。ホストは Windows（プロセスの起動が高い）か Linux。本番は Linux（PFS の Linux サーバ）で、この PC（Windows）は試験の段である。したがって限界は Linux でも測る。

## Decision

7 つの決定（a〜g）。それぞれに、守る不変条件、ログに現れる失敗の形、退けた案を書く。設定キー、マイグレーション、コードは最後にまとめる。

### (a) イベントで起こす待ち。タイマーは遅い安全網にする

1. `forge_db::wake::WakeHub` が `subscribers/wake.rs` を一般化する。プロセスごとに **1 本の** LISTEN 接続で、チャネル `forge_jobs`（ペイロード: ジョブの種類）、`forge_merge_queue`（キューの id）、`forge_refs`（リポジトリの id）、既存の `forge_events_sequenced` を聞く。通知はプロセス内の `tokio::sync::watch` の受け手へ配る。
2. **書き手はトランザクションの中で通知する**（`pg_notify` は COMMIT で届く。ロールバックされた書き込みは誰も起こさない）: `jobs::enqueue_on`（`forge_jobs`、種類）、entry の入れる・外す・再開、列車のパイプラインが終わりの状態か「パイプラインなし」になったとき（`forge_merge_queue`）、serve が記録するブランチの ref の書き込みすべて（`forge_refs`）。通知はまとめられる: 1 つのトランザクションの同じ (チャネル, ペイロード) は PostgreSQL が 1 回だけ届けるので、同じ種類のジョブ 50 件のファンアウトは 1 回の起床になる。これは ADR-0016 §7 の「書き手は通知しない」への意図した例外である。ドメインイベントについては、その規則は変わらない。
3. **プロセス内の起床**: 書き手と待つ側が serve を共有するところ。自動マージ worker の enqueue と enqueue の route が、コミットの後に `MergeQueue::wake(queue_id)`（`tokio::sync::Notify`）を呼ぶ。PostgreSQL への往復はない。
4. **自動マージはイベントで起きる**: 自動マージ worker は `forge_events_sequenced` を購読し、メモリ上のカーソルより後のイベントのうち、型がマージリクエストを指すもの（版、マージ可能性の確認、チェック実行の完了、承認、議論、下書き）を読み、**その MR だけ** を判定する。全件の走査は安全網として残す。（実装でチェック実行の完了がイベントを出すか確かめる。出さなければ、チェック実行の書き手が、プロジェクトの id つきで `forge_events_sequenced` 相当の `forge_merge_queue` に通知する — 試験 S3-T3。）
5. **安全網**（設定）: ジョブのポール 1 s → 10 s。キューの idle 2 s → 30 s。キューの `waiting` 10 s → 60 s。自動マージ 5 s → 30 s（再確認 20 s は変えない）。掃除は変えない（30〜60 s）。ランナーのロングポール（`forge-pipelines/src/runners.rs:50`、1 s）も、種類 `ci.job` の `forge_jobs` で起きる。
6. **通知の取りこぼし**: 再接続は `Reconnected` を返し、すべての受け手がすぐに全件を見る（既存の `Woken::Connected` の規則）。再接続なしの取りこぼしは安全網が拾う。安全網の見回りで、安全網の周期より前に準備ができていたのに起床がなかった仕事を見つけたら、`FG-DB-0024` WakeupMissed を出す（warn、チャネルごとに 1 分に 1 回、`channel`、`oldest_ready_ms`）。足される遅れの最悪は安全網の 1 周期。状態は通知ではなく常にテーブルにあるので、何も失われない。
7. 複数のキューを並行して扱う: `merge_queue.workers` 個の手番タスク（既定 2）が、既存の `FOR UPDATE SKIP LOCKED` の貸し出しで取る（今の F2 は全キューで逐次の 1 ループ）。

- **不変条件**: 通知は状態を持たない（id だけ）。取る側が行を読むので、偽造や重複の NOTIFY は余分な見回りを起こすだけ。待ちはすべて有限（安全網の周期）。認可の変更なし。
- **失敗の形とログ**: リスナーの接続が切れる → `operation=wake_listen` つきの `FG-DB-0001`/`FG-DB-0002`、その後 `Reconnected` と全件の見回り。PostgreSQL の通知キューが満ちる（リスナーが詰まる）→ NOTIFY するコミットが失敗しうる: `forge doctor` が `pg_notification_queue_usage()` を調べ、0.5 を超えたら `FG-DB-0023` NotifyQueueBacklog を出す。起床の嵐 → まとめと、取るのが冪等であることで抑える。
- **退けた案**: タイマーを短くするだけ（段ごとの下限が残る。N プロセス × 短いポールが PG に負荷をかける）。serve と worker を 1 つのプロセスにする（プロセスをまたぐ段を消すが、git を配る遅延とジョブの実行が結びつき、大きく戻しにくい。S0 が LISTEN の段が効くと示したときだけ見直す）。ブローカー（Redis/NATS — 新しい実行時依存で、オンプレの最小主義に反する）。

### (b) 文書の速い経路: プロセス内の結合した木の lint、大きなバッチ、同じ手番での着地

1. **列車の doc-lint の関門**（安全の修正を先に、速さとは独立）: forge-core の trait `TrainDocLint`。実装は forge-checks で `forge_doc::lint::lint`（`forge/doc-lint` と `gstn doc lint` と同じ関数）を使い、forge-cli が注入する — `MergeResultAssessor` の型（F12）で、forge-review が forge-checks に依存しないようにする。列車の **最後の段の木** で動く。対象は、どれかの member が `.md` か `.forge/doctypes/**` を変えるとき、または `forge/doc-lint` が target の必須チェックのとき。
2. **判定 = 土台に対して新しい問題がないこと**（問題の同一性 = パス、規則、参照先の id）: すでに問題を抱えた `main` のリポジトリも動き続け、main は悪くならない。きれい → バッチは着地してよい。新しい問題 → 赤。**リポジトリごとのキー `merge_queue.doc_lint_strict`（既定 false）を true にすると、規則は「きれいであること」になり、土台に問題が残る間は着地しない（QMS のリポジトリ向け）。**
3. **増分、内容アドレス**: リポジトリごとのメモリ上の索引を blob id で引く（解析した文書の entry: id、doctype、ブロック、外向きの参照）。serve の中にあり（LRU、`merge_queue.doc_index_bytes`、既定 64 MiB）。列車の lint はキャッシュにない blob だけを読み（段の木の `ls-tree -r` 1 回 + `cat-file --batch` 1 回）、変わった文書 **と、その文書が足す・消す・改名する id を参照する文書**（逆引き索引）を調べ直す。答えは同じ木に対する全件の `lint()` と等しくなければならず、性質試験で守る。大きさの上限は F14 の 2000 件ではなく時間の予算（`merge_queue.train_lint_budget_ms`、10 s）で、大きな QMS リポジトリも対象になる。キャッシュが冷たいときは木を 1 回読む。
4. **文書の二分の規則**: 列車の段はすでにコミットなので（F11）、赤の列車は **新しい列車なしで** 原因を特定する。段に対する二分探索で、木に新しい問題が出る最初の段 k を探す（接頭辞 k−1 はきれい、接頭辞 k は赤。土台は相対的にきれいで最後の段は赤なので存在する）。member k を `FG-CI-0032` MergeQueueDocLintFailed（info、`entry_id`、`merge_request_id`、`step`、最初の 20 件の問題を `rule:path:id` で）で外し、残りを同じ手番で作り直す。原因 1 件あたり最大 ⌈log2 32⌉ = 5 回の lint で、プロセス内で行う。後ろの member が前の member の壊れを直すのはかまわない: 着地した先端だけが main になる。
5. **判定がプロセス内にあるところだけ大きなバッチ**: 土台と列車の木に `.forge-ci.yml` がないキューは `merge_queue.doc_batch_max` = **32** を使う。CI のあるキューは `ci_batch_max` = 4 と ADR-0030 の半分への二分を保つ。隔離の規則（マイグレーションのパス、`forge/risk` の stop、fast-forward）は変えない。32 の理由: 1 件あたりの逐次の仕事は Windows でおよそ 0.2 s（推定、S0 が測る）。32 件で 1 周期 6〜7 s となり、バッチごとの固定費を薄めるに足る長さで、新しい MR が 1 周期（< 10 s）だけ待つに収まる。後で調整するために、スキーマは 64 を許す。
6. **同じ手番での着地**: 判定がプロセス内にあるので、worker は組み立て、lint、自動マージのマージ結果の判定、着地を **1 手番** で行う（状態は復旧と監査のため `testing → landing → landed` のまま。`ci.pipeline_create` のジョブもポールもない）。列車の lint は列車の sha に `forge/doc-lint` のチェック実行（注釈つき）として保存し、外された MR のページが証拠にリンクできるようにする。
7. 自動マージのマージ結果の判定（F12）は列車全体への 1 回の呼び出し（`assess_train`）になる。各段の diff を前の段から 1 回だけ読んで積み上げるので、32 段のコストは O(n²) でなく O(変更パスの合計) になる。90 s の予算は変えない。

- **不変条件**: main に新しい doc-lint の問題が入らない（今は入りうる: F8）。試した木が着地する木である（ADR-0030）。着地時の member ごとの `authorize(MergeRequestMerge)` と `evaluate_merge()` は変えない。バッチごとに CAS 1 回。
- **失敗の形とログ**: lint が予算を超える、または読み取りの失敗 → `scope=train`、`reason=train_budget|read_failed` つきの `FG-DOC-0002`。バッチは `reason=doc_lint_unavailable` で捨て、回収と同じに数える。3 回目で entry は `FG-CI-0024`（alert、runbook `merge-queue`）で外れる — 閉じる側に倒し、lint していないものを着地させない。キャッシュの汚染は作りとして起きない（キー = blob id）。原因の取り違えは、きれいな接頭辞に足したら新しい問題を出した MR を外すだけで、その MR は組み合わせで赤であることが定義による。
- **退けた案**: バッチ 4 のまま起床だけに頼る（30 倍には足りるが限界には足りない）。毎回木全体を lint する（正しいがバッチごとに O(リポジトリ)。性質試験の基準として残す）。lint を CI ジョブとして動かす（純粋な関数のためにバッチごとにコンテナ）。

### (c) 変更パスを見た再計算

1. 版の段が、head の merge base に対する変更パス（ファイル、その祖先のディレクトリ、完全かの旗。`review.paths_max` = 5000 まで、超えたら不完全 =「すべてに触れる」）をマージ可能性の行に保存する。
2. 着地（キューまたは通常のマージ）は、着地した範囲のパスを 1 回だけ計算し（`diff --name-only --no-renames base..new`、起動 1 回、同じ上限）、`repository.pushed` のペイロード（`changed_files`、`changed_dirs`、`deleted_parent_dirs`、`paths_complete`）に書く。target への直接の push にはパスがなく、今の挙動になる。
3. refresh の購読側は、**影響しうる** MR についてだけマージ可能性を再要求する: `mr.files ∩ (landed.files ∪ landed.dirs) ≠ ∅`、または `mr.dirs ∩ landed.files ≠ ∅`（ファイルとディレクトリの衝突）、または `mr.dirs ∩ landed.deleted_parent_dirs ≠ ∅`（ディレクトリの改名の検出）、またはどちらかの側が不完全、または着地したパスに全体に効くパス（どこかの `.gitattributes`、`.forge/**`、`.forge-ci.yml`、`CODEOWNERS`）がある。述語は forge-core の純粋な関数で SQL の対応物を持ち、試験で両者の一致を固定する。
4. 重ならない MR は同じトランザクションで **持ち越す**: 状態はそのまま、`checked_target_sha` := 新しい先端、`computed_target_sha` は実際にマージを計算した先端のまま。ジョブも git もマージ結果の ref も MR パイプラインもない。
5. **MR のページの表示**: 「`main` @`<computed_target_sha[..8]>` に対して確認済み。それ以降 `main` は N コミット進んだが、このマージリクエストのファイルには触れていない。何かが着地する前に、マージキューが結合した結果を試す。」（ja/en/id のキー）。マージ結果の ref とその上の MR パイプラインには「`<sha>` 時点」と表示する。
6. **列車が権威であり続ける**: 持ち越した MR は列車で `merge-tree` により再マージされる。そこで衝突したら今と同じく `FG-REVIEW-0006` で外す。「重ならない」の誤りの最悪は、後からの衝突のメッセージであり、悪い main ではない。

- **不変条件**: 変更を失わない（列車がすべての member を再マージする）。黙って古くならない（持ち越した行は表示され、数えられる: 着地ごとに info ログ `forge.review.mergeability_carried`、`carried`、`rechecked`、`source_seq`）。マイグレーションは追記のみ。
- **失敗の形とログ**: 述語の誤り → MR は clean と出て、列車が衝突を見つける（`computed_target_sha ≠ checked_target_sha` の行で `FG-REVIEW-0006 reason=conflict` — runbook `merge-request-mergeability` にこの診断の行を足す）。古いペイロードにパスがない → 全件を再計算（`paths_complete=false` をログ）。
- **退けた案**: 全件を再計算するが速くする（O(開いた MR × 着地) が残る）。文書のリポジトリでマージ可能性を捨てる（MR のページと自動マージが、enqueue の前に衝突の答えを要る）。ファイルの内容のハッシュを MR ごとに取る（より正確だが保存が増え、文書ではパスの重なりに対して得がない）。

### (d) MR パイプラインは新しい head のときだけ。列車が CI の権威

1. MR パイプラインは、MR が **新しい head**（版）を得たときに作り、target が動いたときには作らない。target の移動による持ち越し・再計算の行へのマージ結果の書き込み（F7）はパイプラインを積まない。`ci.mr_pipeline_on_target_move`（既定 false）で古い挙動に戻せる。
2. 規則が `ci/pipeline` を必須とする target への直接のマージ（`POST …/merge`）は、MR パイプラインが古い target 上のものなら、既存の `FG-CI-0013`（`required_check_stale`）に `hint=merge_queue` をつけて答える。そのような target では、web の「マージ」ボタンは「準備ができたらマージ」（キュー）になる。「今の target でパイプラインを動かす」操作で、明示的な、要求に応じた MR パイプラインを残す。
3. **CI のあるリポジトリのスループットの上限（正直に）**: 生きた列車が 1 つのとき、スループット ≈ B·s / (T_ci + T_o)。B = バッチの大きさ、s = 生き残る割合の期待値、T_ci = パイプラインの実時間、T_o = forge の固定費（(a) の後でおよそ 2〜5 s）。二分は原因 1 件あたり約 2〜3 本の余分な列車を使う（測定: 原因 3 件で失敗した列車 11 本）。**投機の列車**（下の決定）の深さ D では: ≈ min(D, R/J) · B·s / (T_ci + T_o)。R = ランナーの並行数、J = パイプラインあたりのジョブ数。赤の列車はその後ろの D−1 本を捨てる。例: T_ci = 5 分、R = 4、J = 1、B = 4、p(悪い MR) = 7.5 % → ≈ 4·4·0.73/5 ≈ 2.3 MR/分。**実 CI の上限は、ランナー × バッチ / CI の時間である。forge の固定費は、今の 1 秒 CI の結果のおよそ 10 倍を超えると効かなくなる。** システムのコードで 30 倍にするには、ランナーが 3 スロット以上で並行し CI が短いことが要り、forge の仕事を増やしても効かない。
4. **投機**: `merge_queue.speculation_depth` D（既定 1 = 今のまま）。バッチ k+1 はバッチ k の列車の先端の上に作り、並行して試す。k が着地し、target が k の列車の先端になってから着地する。k が赤か捨てられたら、後ろの生きたバッチはすべて `reason=predecessor_failed` で捨て、作り直す。二分は最初の赤のバッチにだけ適用する。

- **不変条件**: main は赤にならない（土台が今の先端である列車だけが着地する — CAS）。パイプラインが減ればシークレットの露出も減る（増えない。列車のパイプラインは保護なしのまま、ADR-0029）。
- **失敗の形とログ**: 投機の連鎖が繰り返し無効になる → `depth` つきの `forge.merge_queue.batch_abandoned reason=predecessor_failed`。ランナーの飽和は、保留の列車のパイプラインとして現れる（期限で今のとおり `FG-CI-0024`）。
- **退けた案**: target が動くたびに MR パイプラインを回し続ける（2 乗: 40 MR で 291 本）。MR パイプラインを完全にやめる（作者はレビューの前に head のフィードバックが要る）。

### (e) 重ならないパスの並列の列車か、大きな単一のバッチか → **大きな単一のバッチ**

1 つの target 上のパスの群ごとの並列の列車も、target の CAS とリポジトリごとのマージのロックで逐次になる。2 本目の列車は着地の前に 1 本目の結果の上へ再マージされるが、その再マージは、重ならないことが独立を証明しない限り試されていない。文書では証明しない（文書をまたぐ参照、doctype の変更）。コードでも同じ（意味の衝突）。大きな単一のバッチは、結合した木をそのまま試し、1 件あたりの逐次のコストは同じで、新しい並行の規則を要さない。投機（d4）が、並列が得をする唯一の場合（長い CI）を扱う。**退けた案**: パスの群ごとの並列の列車（新しい競合の面、保証が弱い、1 つのロックの下ではスループットの得がない）。

### (f) キューの前の MR ごとの仕事

| 仕事 | 今（ジョブ） | 決定 |
|------|--------------|------|
| `merge_request.refresh` + `mergeability` + `check.risk` + `check.spec_index`（`forge/doc-lint`、`spec-lint`、`spec-trace`、`ci-lint`） | 4 つのジョブ。それぞれ head を読み直す | **1 つのジョブ `merge_request.prepare`**（forge-cli の合成ハンドラが既存の関数を順に呼び、head の一覧 1 回と `cat-file --batch` 1 回を共有する）。各小段は head の sha ごとに冪等（その head の結果があれば飛ばす）なので、再試行は失敗した分だけやり直す。各小段は自分のコードをログに出す。 |
| `check.spec_drift` | ジョブ | 別の低優先度のジョブのまま（助言で、マージの経路にない）。 |
| `merge_request.conflict_scan` | ジョブ、相手は 50 件まで、`peers_capped` で 0 組のことが多い（5〜15 s） | 相手 = パスが **影響しうる** 開いた MR（(c) の述語）。重ならない相手は衝突しえない。低優先度。マージの経路には置かない。影響する相手が上限を超えたら飛ばす（ログ）。 |
| `ci.pipeline_create`（MR の head） | ジョブ | head の木に `.forge-ci.yml` があるときだけ（`prepare` の中で決め、なければジョブなし）。 |
| 文書の MR | 上のすべて | `prepare` と、経路にはそれ以外なし。 |

- **不変条件**: 同じチェック名、結論、コードを書く（利用側は変えない）。少なくとも 1 回のジョブ配送は、小段が冪等なので動き続ける。
- **失敗の形とログ**: 小段の失敗 → 既存のコード（`FG-REVIEW-0003`、`FG-DOC-0001..3`、…）に `job_kind=merge_request.prepare`、`step=<名前>` をつける。`prepare` のジョブが死ぬ → runbook `job-dead-lettered` に、失敗した小段を書く。
- **退けた案**: 版のときに serve の中で全部をその場で動かす（push の経路に git の仕事が乗る）。4 つのジョブのまま優先度だけにする（MR ごとに木の読み 4 回と claim の往復 4 回）。

### (g) MR の作成を 0.1 s 未満に

1. ブランチの存在は、serve の中のリポジトリごとの **ref のスナップショットのキャッシュ**（`forge_git::RefSnapshotCache`: ミスごとに `for-each-ref refs/heads/` 1 回、single-flight、serve のすべての ref の書き込みと、他のプロセスからの `forge_refs` の通知で無効にする）から答える。ヒットは起動も browse のスロットも使わない。
2. 肯定の答えはキャッシュから出す。**否定の答えは必ず git で確かめる**（キャッシュしない）ので、直前に push したブランチが断られない。
3. トランザクション（`open_mr_decided`）と、`forge_jobs` の通知つきの `prepare` の enqueue は変えない。`head_sha` はこれまでどおり `prepare` が書き（0.3〜0.5 s 後）、キャッシュから推測しない。

- **不変条件**: 認可は変えない（`MergeRequestCreate` の証明はトランザクションの中で再確認する）。ないブランチには今も `FG-REPO-0016`/`0023` を返す。
- **失敗の形とログ**: 古い肯定（スナップショットの後にブランチが消えた）→ MR は作られ、`prepare` が `source_branch_missing` を保存する（見える状態、既存）。キャッシュの充填の失敗 → browse の経路に戻る（既存のコード、短い待ちの後に `FG-GIT-0004`）。
- **退けた案**: ref の DB の写し（receive-pack に正確に追従すべき 2 つ目の真実の源）。ブランチを調べずに MR を受ける（API の契約が変わる）。

## Alternatives Considered

### 設計はそのまま、数字だけ調整する（ポールを短く、バッチ 8）
- **Pros**: 変更が小さい。
- **Cons**: 段ごとにポールの下限が残る。再計算は O(開いた MR × 着地) のまま。文書の参照は結合した木で未検査のまま。
- **Why not**: せいぜい 3〜5 倍で限界に届かず、F8 を開けたままにする。

### 1 つのプロセス（serve + worker）
- **Pros**: どこでもプロセス内の起床になる。
- **Cons**: ジョブの異常終了と重い git の仕事が HTTP のプロセスを共有する。大きく、戻しにくい。
- **Why not**: LISTEN/NOTIFY はプロセスをまたいでも同じ遅延（≈ 1 ms）を与える。

### パスの群ごとの並列の列車
- **Why not**: (e) を参照。

## Consequences

### Positive
- 文書のマージは、タイマーではなく git と PostgreSQL の仕事で決まる（計画 18 の目標: ≥ 200/分で捌く、作成からマージまでの p50 ≤ 15 s）。
- 文書をまたぐ壊れた参照が main に入らなくなる。
- MR あたりのパイプラインが線形になる（head ごとに 1 本 + 列車）。

### Negative
- 2 つの新しい通知のチャネルと書き手側の NOTIFY（ADR-0016 §7 の例外）。通知キューの doctor の検査。
- マージ可能性の行が「持ち越し」になりうる。MR のページで説明が要る。
- CI が必須の target への直接のマージは、キューへ誘導される。この点は推奨のとおり決定した（2026-11-17、キーで戻せる）。
- serve が文書の索引を持つ（上限つきの LRU、既定 64 MiB）。

### Risks
- **NOTIFY は、通知するコミットのトランザクションを直列にする**（PostgreSQL は通知キューのために、コミットで全体のロックを取る）: まとめで抑える。S0/S2 が MR 200 件の同時の負荷でコミットの遅延を測る。逃げ道 = 書き手でのチャネルごとの debounce。
- **Windows の起動コストが 1 件あたりの仕事を支配する**: S0 で測る。S10 でロックを持つ区間から起動を取り除く。
- **投機が赤の連鎖で CI を無駄にする**: 既定の深さは 1。ランナーの容量があるプロジェクトだけ上げる。
- **本番は Linux、試験は Windows**: 限界の数字は Windows の試験の段の推定である。Linux でも測り直す（計画 18 S0）。

## Ceiling（全部の段の後に、マージの経路を何が縛るか）

リポジトリごと（マージのロック `merge:<repo>` はそのすべての target を覆う）、文書: 1 件あたりの組み立て ≈ 起動 4 回 + merge-tree の仕事。ロックの下の 1 件あたりの着地 ≈ 起動 3 回 + SQL 文 ≈ 20〜30。バッチごとに CAS 1 回。組み立てを着地と並べて走らせる（プロセス内の判定のキューで投機の深さ 2）と、限界はリポジトリごとに `1 / max(T_build, T_lock)`。異なるリポジトリは並列に着地する（`merge_queue.workers`）。数字は計画 18 §2 にある（S0 が起動のコストを測るまでは推定）。

## Error codes and runbooks affected

| Code | Name | Severity | New / changed | Runbook |
|------|------|----------|---------------|---------|
| FG-DB-0023 | NotifyQueueBacklog | alert | new | `db-unavailable`（新しい節「notify queue」） |
| FG-DB-0024 | WakeupMissed | warn | new | `job-queue-backlog` |
| FG-CI-0032 | MergeQueueDocLintFailed | info | new | `merge-queue` |
| FG-DOC-0002 | (incomplete lint) | — | 文脈 `scope=train`、`reason=train_budget\|read_failed` を記す | `merge-queue` |
| FG-CI-0024 | MergeQueueStalled | alert | 診断に `doc_lint_unavailable` を足す（Amendment: 3 回目のエントリーの理由にもなる） | `merge-queue` |
| FG-REPO-0024 | (repo.yml invalid) | — | Amendment: 文脈 `reason=doc_lint_repo_config_invalid`（キューの除外、warn）を記す | `merge-queue` |
| FG-CI-0013 | (required check stale) | — | 文脈 `hint=merge_queue` を記す | `merge-blocked` |
| FG-REVIEW-0006 | (conflict) | — | 診断に「持ち越した行」を足す | `merge-request-mergeability` |

番号は暫定: 実装のとき `errors.toml` の次の空き番号を取る（他のブランチが FG-DB-0023/0024、FG-CI-0032 を先に取るかもしれない）。名前は拘束する。

設定キー（すべて戻せる）: `jobs.safety_poll_ms`（10000）、`merge_queue.safety_poll_ms`（30000）、`merge_queue.waiting_safety_poll_ms`（60000）、`auto_merge.safety_poll_ms`（30000）、`merge_queue.workers`（2）、`merge_queue.doc_batch_max`（32、1〜64）、`merge_queue.ci_batch_max`（4、1〜64）、`merge_queue.speculation_depth`（1、1〜8）、`merge_queue.train_lint_budget_ms`（10000）、`merge_queue.doc_index_bytes`（67108864）、`merge_queue.doc_lint_strict`（false、リポジトリごと）、`review.path_filter`（true）、`review.paths_max`（5000）、`ci.mr_pipeline_on_target_move`（false）。

マイグレーション（追記のみ）: `merge_queue_batch_limit_64`（CHECK 1..64、`NOT VALID` + `VALIDATE`）、`merge_request_mergeability_paths`（head のパスの配列、`paths_complete`、`computed_target_sha`）、`merge_queue_speculation`（(queue, depth) ごとの生きたバッチの一意性、`parent_batch_id`）。

## Interface sketch

```rust
// forge-db/src/wake.rs
pub enum Channel { Jobs, MergeQueue, Refs, Events }
pub enum Woken { Notified(Vec<String>), SafetyNet, Reconnected }
pub struct WakeHub;
pub struct WakeRx;
impl WakeHub {
    pub fn spawn(db: &Db, channels: &[Channel]) -> Result<WakeHub, ForgeError>;
    pub fn subscribe(&self, channel: Channel) -> WakeRx;
}
impl WakeRx { pub async fn wait(&mut self, safety_net: Duration) -> Woken; }
pub(crate) async fn notify_on(conn: &mut PgConnection, channel: Channel, payload: &str) -> Result<(), ForgeError>;

// forge-core/src/merge_paths.rs (pure)
pub struct PathSet { pub files: Vec<String>, pub dirs: Vec<String>, pub deleted_parent_dirs: Vec<String>, pub complete: bool }
pub fn path_set(changed: &[String], deleted: &[String], max: usize) -> PathSet;
pub fn may_interact(mr: &PathSet, landed: &PathSet) -> bool;
pub fn is_global_path(path: &str) -> bool;

// forge-core/src/train_lint.rs (trait; forge-checks implements, forge-cli injects)
pub struct TrainLintInput<'a> { pub project: ProjectId, pub repo: RepoId, pub base: &'a ObjectId, pub steps: &'a [ObjectId], pub budget: Duration }
pub enum TrainLintVerdict { NotApplicable, Clean, NewProblems { first_bad_step: usize, problems: Vec<TrainLintProblem> }, Unavailable { reason: &'static str } }
pub struct TrainLintProblem { pub path: String, pub rule: String, pub reference: Option<String> }
pub trait TrainDocLint: Send + Sync {
    fn lint_train<'a>(&'a self, input: TrainLintInput<'a>) -> TrainLintFuture<'a>;
}
// forge-core/src/auto_merge.rs (addition)
pub trait MergeResultAssessor: Send + Sync {
    fn assess<'a>(&'a self, range: MergeResultRange<'a>) -> MergeResultFuture<'a>;
    fn assess_train<'a>(&'a self, train: TrainRange<'a>) -> TrainResultFuture<'a>; // one pass, per-step verdicts
}

// forge-review/src/merges/queue/worker.rs
pub struct QueueLimits { /* existing */ pub doc_batch_max: usize, pub ci_batch_max: usize, pub speculation_depth: usize, pub safety_poll: Duration, pub waiting_safety_poll: Duration, pub train_lint_budget: Duration }
pub enum VerdictSource { InProcess, Pipeline }
impl MergeQueue {
    pub fn with_train_lint(self, lint: Arc<dyn TrainDocLint>) -> Self;
    pub fn wake(&self, queue_id: i64);
    pub async fn tick(&self) -> Result<Option<TickReport>, ForgeError>; // unchanged signature
}

// forge-db/src/review/mergeability
pub(crate) async fn carry_forward_on(conn: &mut PgConnection, project: ProjectId, targets: &[String], landed: &PathSet, new_tip: &ObjectId) -> Result<CarryReport, ForgeError>;
pub struct CarryReport { pub carried: u64, pub rechecked: Vec<MergeRequestId> }

// forge-db/src/review/prepare.rs
pub const PREPARE_JOB: &str = "merge_request.prepare";
pub fn prepare_job(mr: MergeRequestId, head: Option<&ObjectId>, origin: RequestId) -> Result<NewJob, ForgeError>;

// forge-git/src/refs_cache.rs
pub struct RefSnapshotCache;
impl RefSnapshotCache {
    pub async fn branch_tip(&self, git: &Git, storage: &StorageRoot, repo: RepoId, name: &RefName, timeout: Duration) -> Result<Option<ObjectId>, ForgeError>;
    pub fn invalidate(&self, repo: RepoId);
}
```

## 追記（2026-11-18、計画 18 S8）— (g) の実装: 意図した 2 つの違いと、既知の限界 1 つ

上の Decision (g) の文は変えない。実装（`forge_git::refs_cache`、`Browser::require_branch`、`forge-review` の `require_branches_exist`）は次の 2 点で (g) の文と違う。どちらも意図したもの。

1. **スナップショットはブランチの名前だけを持つ。** `branch_tip -> ObjectId` は持たない（上の Interface sketch の `branch_tip` は実装していない。実際の API は `RefSnapshotCache::branch(repo, 名前, 読む関数) -> BranchAnswer`）。
   - 理由: 古いかもしれないスナップショットから、sha に依存する判断（MR の head、version、mergeability、マージ）が出ないようにするため。sha を持っていなければ、誤って使う呼び出しは書けない。(g) 3 の「`head_sha` は `prepare` が書き、キャッシュから推測しない」を構造で守る。sha が要る操作（reopen、approve、merge、merge status）は今も生きた ref を読む（`resolve_ref` / `find_ref`、version のジョブ）。
2. **スナップショットの読み取りの失敗は、リクエストごとの browse の経路に戻らず、その失敗を呼び出しに返す。**（(g) の「キャッシュの充填の失敗 → browse の経路に戻る」とは違う。）
   - 理由: 読み取りが失敗するのは、リポジトリの browse のスロットが埋まっているとき（`FG-GIT-0004 reason=repo_browse`）か、git が遅い・動かないとき。そのとき待っていた作成が 1 件ずつ browse の経路に戻ると、混んでいるリポジトリへの呼び出しが待っていた数だけ増える（S8 が消したかった 2026-11-17 の F1 の形）。失敗を「ブランチがない」「ある」と答えることもしない。
   - 待っていた呼び出しの扱い（S8 のレビューの指摘で 2026-11-18 に決めた）: 失敗が**読んだ本人の時間か順番についてのもの**（`FG-GIT-0002` のタイムアウト、`FG-GIT-0004` の混雑）なら、本人だけに返す。待っていた呼び出しはそれを受け取らず、そのうちの 1 件が自分の期限で読み直し、残りはそれを待つ。自分の期限（`BrowseLimits::timeout`、10 s）が過ぎた呼び出しは、自分の `FG-GIT-0002 operation=browse` で終わる。それ以外の失敗（git を起動できない `FG-GIT-0001`、git の異常終了 `FG-GIT-0003`、リポジトリがディスクにない、など。誰が読んでも同じもの）は、待っていた全員に返す。
   - 上限: 読み取りはリポジトリごとに同時に 1 つのまま（browse のスロットは 1 つ。ゲートは変えていない）。1 つの呼び出しが読むのは高々 1 回で、読んだ呼び出しは答えか登録済みのエラーで必ず終わるので、読み直しの回数は呼び出しの数を超えない。埋まり続けるリポジトリでは、`repo_wait`（5 s）ごとに 1 件が `FG-GIT-0004` で終わり、残りは自分の期限で `FG-GIT-0002` になる。終わらない待ちはない。

既知の限界（古さ）:

- `forge serve` の中の ref の書き込み（push、Web のファイルのコミット、マージ、revert、autofix、version の ref、仕様の公開）は、すべて `Browser::invalidate` を呼ぶ。
- **他のプロセス**（今は worker のマージ）が書いた・消したブランチは、計画 18 S2 の `forge_refs` のリスナーが `Browser::invalidate` を呼ぶようになるまで、serve からは最長 `refs_ttl`（5 s、読み取りの開始から数える）見えない。作られたブランチは影響を受けない（否定の答えは必ず git で確かめる）。起こりうるのは、消されたブランチへの古い肯定だけ。
- 古い肯定の見える結果: MR は open で作られるが、version がなく（`prepare` の結果は `source_missing`）、「マージできない」と表示される。データは壊れず、黙って先へ進むこともない。
- **`forge serve` の外で `refs/heads/` を書く処理を新しく足すときは、このリスナー（`forge_refs` の通知 → `Browser::invalidate`）を必ず一緒に入れる。**

## Amendment（2026-11-18、計画 18 S1 の実装）: 列車の doc-lint の関門

S1 で (b) の 1・2 を作った。上の Interface sketch と違うところ、そして sketch が決めていなかったところ:

- **「lint できなかった」は判定ではなく失敗にした**: `lint_train` は `Result<TrainLintVerdict, ForgeError>` を返し、`Unavailable { reason }` は無い。
  `Err` は必ず `FG-DOC-0002`（`scope=train`、`reason=train_budget` か `read_failed`、元のコードは `cause`）で、キューは `Err` で着地しない。
  判定の値を 1 つ読み落として着地する、という誤りを型で起こせなくするため（hard contract 2）。
- `TrainLintInput` に `request_id`（バッチのもの。チェックの記録と info のログに使う）を足した。
- `TrainLintVerdict::BaseNotClean { problems }` を足した: 厳格なリポジトリで、土台にすでに問題があるとき。どの段も原因ではないので、
  段を指す `NewProblems` では表せない。列車の MR は全部 `FG-CI-0032`（`reason=doc_lint_base_not_clean`）で外れる。
- **キーの置き場所**: `merge_queue.train_lint_budget_ms` はサーバーの設定（既定 10000、100〜60000、**0 で関門を切る** = S1 の前の動き）。
  `merge_queue.doc_lint_strict` はリポジトリの `.forge/repo.yml`（`merge_queue:` の下）で、**列車の土台**の版を読む（MR の中で値を変えても、
  その MR の着地には効かない）。マイグレーションは無い。
- **土台の `.forge/repo.yml` が正しくないとき**（レビューの後で変えた）: はじめは「厳格として扱う」だけだったが、土台に古い問題が
  あると `.forge/repo.yml` を**直す MR まで** `doc_lint_base_not_clean` で外れ、リポジトリが詰まった。そこで、土台が正しくないときだけ
  **列車の先端**の `.forge/repo.yml` で決める: 正しければその値（直す MR は着地できる）、無ければ厳格でない、やはり正しくなければ厳格。
  土台が正しく読める限り土台の値だけを使う、は変えない（列車の MR は自分の列車の規則を緩められない）。両方で正しくなく、土台の問題が
  残るときは `TrainLintVerdict::RepoConfigInvalid { problems, config }`（足した値）で、列車の MR は全部 **`FG-REPO-0024`**
  （`reason=doc_lint_repo_config_invalid`、warn、`config_line` / `config_reason`）で外れる。「あなたの文書が壊れている」とは読めない
  ようにするため、`FG-CI-0032` とは別のコードと理由にした。
- **lint できずに 3 回目で外れる MR の理由**（レビューの後で変えた）: コードは `FG-CI-0024` のまま、エントリーの理由を `stalled` ではなく
  `doc_lint_unavailable` にした（`stalled` は「試験が期限までに終わらなかった」で、原因を隠す）。その `entry_removed` に lint の
  `lint_reason` / `cause` / `bound` / `budget_ms` を付ける。forge-db は変えていない（3 回目の MR を `BatchEnd.removed` に名指しで入れる）。
  自動マージは `doc_lint_unavailable` を `stopped`（`not_assessed`、`merge_queue_doc_lint_unavailable`）、`doc_lint_repo_config_invalid` を
  `held`（`merge_queue_repo_config_invalid`）にする（`stalled` のままだと入れ直して同じ 3 回を繰り返す）。
- **ログの重さ**: `FG-CI-0032` の除外はカタログ（info）のとおり info。warn は設定が原因の除外（`FG-REPO-0024`）と 3 回目の `FG-CI-0024` だけ。
  対象でない列車は debug の `train_lint_skipped`（関門を切っているときは何も出ない）。`lint_ms` は関門が答えを待った時間で、対象でない
  列車でも 0 ではない（`.forge/repo.yml` の読み取りと変わったパスの一覧）。
- **予算は「待つのをやめる」だけ**（既知の制限）: 木の lint は blocking のスレッドの上の 1 回の `forge_doc::lint::lint` で、途中で
  止める点が無い（forge-doc は純粋なまま）。時間切れのあとも走り切り、結果は捨てる。読む上限が 1 本の大きさを抑え、MR は 3 回で
  外れるので、残る本数は限られる。取り消しの旗は入れていない（S7 の増分の索引で木の全部を読まなくなるときに見直す）。
- **対象**: どれかの MR が `.md` か `.forge/doctypes/**` を変えるとき、または厳格なリポジトリ。「`forge/doc-lint` が target の必須チェックのとき」は
  別には見ていない: 文書も文書型も変えない列車の木は、土台と同じ lint の答えになるので、「増えていない」の判定は変わらない
  （違いは、その列車の先端に `forge/doc-lint` のチェックが記録されないことだけ）。
- **いつ動くか**: 列車のパイプラインが緑になった後、マージ結果の危険の判定（M4 の締め）の前。パイプラインのあるリポジトリでは、lint で
  赤になる列車も先にパイプラインを待つ（S7 で列車を作った手番に動かすときに変える）。
- **先端の段だけを lint する**（二分のときだけ途中の段を読む）: 同じ列車の中で、ある段が壊した参照を後ろの段が直すと、途中の段の
  コミット（`main` の first-parent）には壊れた参照が残る。先端がきれいなら着地する、が (b) の決定で、S1 はそのまま。
- **`forge/doc-lint` のチェック**は列車の**先端の sha だけ**に、lint が対象のときだけ記録する（木の全体の評価 + 先頭に「土台に無い問題は N 件」の
  行）。途中の段と、文書を変えない列車には無い。負荷試験の `GS_LT_DOC_LINT_MISSING_FATAL` の既定を 0 のままにした理由。
- 読む上限は head の `forge/doc-lint` より広い（`.md` 20,000 件、64 MiB）。上限に当たると「一部だけの答え」ではなく失敗（`bound`）。
  S7 の増分の索引までは、毎回、木の全部を読む。

## 追記（2026-11-18、計画 18 S2 の実装）— (a) のうち作ったもの、文との違い、既知の限界、S3 が引き継ぐもの

上の Decision (a) の文は変えない。S2 が作ったのは (a) の 1・2（`forge_jobs` の分）・5（ジョブとランナーの分）・6 で、3・4・7 と
`forge_merge_queue` / `forge_refs` の送り手は S3 以降。

**作ったもの**

- `forge_db::wake::WakeHub`（`crates/forge-db/src/wake.rs`）: プロセスごとに 1 本の LISTEN の接続（プールの外の専用の接続）で
  `forge_jobs` / `forge_merge_queue` / `forge_refs` / `forge_events_sequenced` を聞き、プロセス内の待つ側へ配る。
  `subscribers/wake.rs` はこれに委ねる。接続は上限つきで張り、5 秒ごとの上限つきの読み取りで生死を確かめ、切れたら 250 ms〜5 s の
  間隔で張り直す。張り直すたびに待つ側は全員 1 回見る（`Woken::Reconnected`）。
- 書き手は同じトランザクション（同じ文）で `pg_notify('forge_jobs', <種類>)` を送る: ジョブの投入・返却・失敗や回収の後の再試行の予約、
  CI のジョブが取れるようになる変更（計画されたパイプライン、再試行、手動ジョブの開始、取り消し、回収で戻った貸し出し、
  ランナーのタグの変更と再開）。ジョブの終わりは、依存先が pending になったときと、動いていた `gpu-eval` のジョブが GPU の枠を
  空けたときだけ送る（レビューの後で変えた。それまでは終わるたびに送り、待っているランナーを全部起こしていた）。
  ペイロードはジョブの種類だけ。
- 待つ側: ジョブの実行役（worker のスロット、レーンの実行役、serve 側のマージと revert の実行役）は、今取れる種類の通知・
  次の予約されたジョブの期限・`jobs.safety_poll_ms`（既定 10 s）のどれかまで待つ。ランナーのロングポーリングは `ci.job` で起きる。
  起床は合図にすぎず、起きた側は必ず表を読み直す。
- 見えるようにしたもの: LISTEN の接続の失敗は `FG-DB-0001` / `FG-DB-0002`（`operation=wake_listen`）。LISTEN のタスクが頼まれずに
  終わったら `FG-SYS-0001`（`task=wake_hub`、`reason=task_panicked` / `task_ended` / `task_cancelled`）をすぐに出し、5 秒後に
  起動し直す（その間、待つ側は 1 秒ごとの確認）。`FG-DB-0024` WakeupMissed（warn）と `FG-DB-0023` NotifyQueueBacklog（alert）。
  どれも、続いている間は 1 分に 1 回まで。

**(a) の文との違い**（どれも意図したもの）

- **切るスイッチは無い。** NOTIFY と LISTEN を止める設定は入れていない。通知が届かないときに計画 18 より前の速さへ戻す手段は
  `jobs.safety_poll_ms = 1000` だけ（送る側の NOTIFY は残る）。理由: 起床は合図で、状態は表にあるので、切っても正しさは変わらない。
  スイッチを足すと、試験する組み合わせが倍になる。
- **`forge_refs` はチャネルを用意しただけで、送り手も聞き手もつないでいない**（下の既知の限界）。`forge_merge_queue` も同じで、S3 が使う。
- **プロセスごとに接続が 1 本増える。** 接続の予算（ADR-0018、`bootstrap::ConnectionBudget`）は +3 から +4。プールの外なので、
  `database.max_connections` を増やしても減らしても変わらない。
- **`FG-DB-0023` は `forge doctor` だけでなく、hub も出す**（レビューの後で足した）。(a) の文は doctor だけだったが、待ち行列が満ちると
  投入・失敗・返却・パイプラインの確定の COMMIT が全部失敗するので、誰かが doctor を走らせるまで見えないのでは遅い。hub が LISTEN の
  接続（必ずプライマリ）で 5 秒ごとに `pg_notification_queue_usage()` を読む（接続の生死の確認を兼ねる）。doctor は書き込み用の
  プールで読む（スタンバイではこの関数は 0 を返す）。
- **`FG-DB-0024` の `oldest_ready_ms` の意味**: ジョブは「実行可能になってから」。CI のジョブは「そのランナーが取れるようになってから」=
  pending になってからの時間と、ランナーが最後に変えられてからの時間の短いほう（DB の時計）。どちらも安全網の周期以上のときだけ
  出す（レビューの後で CI の側に足した。それまでは、タグを変えた・再開したランナーが「取りこぼし」と誤って出していた）。
  1 分の制限で出さなかった件数は、次の 1 件の `suppressed` に入る。

**既知の限界**

- **起きるたびに、次の期限を書き込み用のプールで 1 回読む**（`jobs::idle::next_due`）。待ちに入る実行役 1 つにつき 1 文で、索引
  `jobs_ready_kind` の先頭を見るだけ。プロセスで共有するキャッシュは後の最適化。
- **先の時刻に予約される行（`fail` / `release` / 回収の再試行）もすぐに通知する。** 待っている側が次の期限を読み直すために要る。
  その起床で取れるものは無いので、空の読み取りが 1 回増える。
- **取った直後と期限の読み取りの間に期限が来たジョブ**は、期限の読み取りが 1 秒さかのぼって見つける（`jobs::idle::LOOK_BACK`）。
  見つけたら待ちをすぐ終えてもう 1 回取る。同じジョブで 2 回は終えない（`looked_until`: すでに知らせた一番新しい期限）ので、
  取れない期限切れのジョブ（順序のキーが使用中、種類が上限）でも空の読み取りは 1 回で、回り続けることはない。
  もう 1 回取る案ではなくさかのぼる案にした理由: 待つ関数はジョブを実行できず、もう 1 回取る案は待ちに入るたびに空の読み取りを足す。
  プロセスか DB が 1 秒より長く止まっていた場合は安全網が拾い、`FG-DB-0024` が出る。
- **`gpu-eval` のジョブの `oldest_ready_ms`** は pending になってからの時間で、GPU の枠が空いてからの時間ではない。枠の空きは通知するので
  ふつうは問題にならないが、その通知を取りこぼしたときの値は実際より長く出る。ランナーの説明だけの変更でも「変えられてから」の
  時計は戻る（取りこぼしを出さない側に倒れる）。
- **hub のタスクが終わり続ける場合**は、5 秒ごとに起動し直し、ログは 1 分に 1 回。その間は 1 秒ごとの確認なので仕事は止まらないが、
  自分では直らない（原因は欠陥）。`forge doctor` は別のプロセスなので、hub の状態は doctor には出ない。ログで見る。
- **知らないチャネルの通知は捨てる**（ログなし）。聞いているチャネルは自分で LISTEN したものだけなので、届くのは欠陥のときだけ。
- カタログの 2 つのコードの `retryable` と、`FG-DB-0024` の `http_status` は、他の「ログだけのコード」と同じ値のままにしてある
  （利用者には返らない）。
- **トランザクション単位のプーラーを通すと LISTEN は働かない。NOTIFY は COMMIT を直列にする。** 運用の注意は
  `ops/runbooks/db-unavailable.md` の「notify queue」と `crates/forge-db/README.md`。
- **`forge_refs` の送り手と聞き手が無いので、S8 の「古い肯定」の限界はそのまま残る**（上の S8 の追記: 他のプロセスが消したブランチが、
  serve から最長 `refs_ttl` = 5 s 見える）。つなぐ段は、次の書き手の全部で、ref を書いたトランザクション（か、その直後）に
  `forge_refs` を送り、serve の聞き手が `Browser::invalidate(repo)` を呼ぶこと。S2 の時点で見つけた書き手（`invalidate` を呼ぶ場所）:
  - `forge-web/src/routes/git_push.rs`（push の後の 2 か所）、`forge-web/src/routes/file_commit.rs`（Web のファイルのコミット）
  - `forge-review/src/merges/execute.rs`（2 か所）、`merges/mod.rs`、`merges/reconcile.rs`、`merges/source.rs`、
    `merges/queue/worker.rs`（2 か所。**worker のプロセスで動く**: 今の限界の出どころ）
  - `forge-review/src/revert.rs`（3 か所）、`autofix.rs`（2 か所）、`version_refs.rs`
  - `forge-pipelines/src/spec_publish.rs`、`forge-checks/src/risk.rs`
  - キャッシュの側は `forge-git/src/browse/mod.rs` の `Browser::invalidate`。forge-git は forge-db に依存しないので、聞き手は
    forge-web（か機能クレート）に置き、hub の `subscribe(Channel::Refs)` から `invalidate` を呼ぶ。

**S3 が引き継ぐもの**

- hub はもう `forge_merge_queue` と `forge_events_sequenced` を聞いている。S3 は送り手（entry の入れる・外す・再開、列車の
  パイプラインの終わり）に `wake::notify_on(conn, Channel::MergeQueue, <キューの id>)` を同じトランザクションで足し、待つ側は
  `Db::wake()` の `subscribe(Channel::MergeQueue)` と `WakeRx::wait` / `wait_matching` を使う。プロセス内の起床は `WakeHub::nudge`。
- 決まり: 通知は id だけ（ペイロードは 255 バイトまで）。起きた側は必ず表を読む。待ちは必ず有限（安全網の周期）。hub が無い・
  接続が切れているとき（`WakeHub::is_live` が false）は自分の短い確認に戻る。`Woken::Reconnected` では全件を見る。
- 取りこぼしを出すとき（`WakeHub::missed`）は、ジョブと同じ規則にする: 安全網の周期を丸ごと待った後に見つけ、**その仕事が安全網の
  周期以上前から取れる状態だった**ときだけ。年齢は DB の時計で測り、`oldest_ready_ms` に入れる。
- 送る条件は「何かが取れるようになったとき」に絞る（起床 1 回が、待っている全員の読み取り 1 回になる）。
- 安全網の設定（キューの idle 30 s、`waiting` 60 s、自動マージ 30 s）と `merge_queue.workers` は S3。ジョブの側は
  `jobs.safety_poll_ms` だけ。

## Amendment（2026-11-18、計画 18 S4 の実装）: 変更パスを見た再計算と持ち越し

S4 で (c) を作った。上の Decision (c) の文は変えない。文と違うところ、(c) が決めていなかったところ、残る限界:

- **head のパスの集合は、版の段ではなく判定のジョブが書く**（(c) 1 と違う）。結果と同じ 1 つの UPDATE（`record_check`）で書くので、
  保存された集合は必ず `checked_head_sha` のもの。「集合は古い head のもの」という状態を行の上で作れない。新しい head では行が
  `unchecked` に戻り、持ち越しは `checked_head_sha = MR の今の head` の決まった行だけを取る。集合の中身は、merge base → head の
  `diff-tree -z --no-renames` と、衝突のない結果ではさらに target の先端 → マージした木の差分（マージがパスを動かした場合を拾う）。
- **列を 1 つ足した**: `head_deleted_dirs`（head が何かを消したディレクトリ。すべての祖先）。(c) 3 は着地の側の
  `deleted_parent_dirs` だけだったが、逆向き（MR がディレクトリを消し・改名し、着地がその下に足す）も確かめ直すために要る。
  消した側はすべての祖先を持つので、`docs/` の 1 件の削除で `docs/` に触れる MR が全部確かめ直される（広すぎる側に倒した）。
- **規則（`forge_core::merge_paths::may_interact`、規則の版 1）**。確かめ直すのは: どちらかの集合が無い・不完全。同じファイル。
  片方のファイルがもう片方のディレクトリ（`a` と `a/b`）。`mr.dirs ∩ landed.deleted_dirs ≠ ∅`、`mr.deleted_dirs ∩ landed.dirs ≠ ∅`。
  同じディレクトリの下の別のファイルで、どちらも何も消していなければ、触れ合わない。
- **不完全（= すべてに触れる）**: 上限（`review.paths_max`、既定 5000、1〜20000）を超えた、キーの合計が 1 MiB を超えた、git の出力が
  切れた、UTF-8 でないパス（`text[]` に入らない）、シンボリックリンク・サブモジュール（どちらかの側の mode が 120000 / 160000）、
  空・`.`・`..` の要素や NUL、**全体に効くパス**。全体に効くパスは (c) 3 の一覧に `.gitmodules` を足した: どこかの `.gitattributes` /
  `.gitmodules`、`.forge/**`、ルートの `.forge-ci.yml` と `CODEOWNERS`。着地の側だけでなく **MR の側** にあっても不完全にする。
  不完全な集合は空の一覧で保存・送信する。読めなかった（git の失敗）は判定の失敗にせず、集合なし（NULL = 確かめ直す）で保存して warn。
- **大文字と小文字**: パスは `/` で分け、文字ごとに大文字小文字をそろえたキーで比べる（`Docs/A.md` と `docs/a.md` は同じ）。git の
  マージは区別するが、区別しないファイルシステムのチェックアウトのために、大文字小文字だけの違いを「触れ合う」に倒した。
  Unicode の正規化（NFC / NFD）は **そろえない**（既知の限界。外れたら列車が見つける）。
- **SQL の対応物**（`forge_db::review::mergeability::paths::interacts_sql`）は、行の列と、束縛した 4 つの引数（着地のファイル、
  ディレクトリ、消したディレクトリ、完全か）の上の式で、NULL にならない。試験 `may_interact_matches_its_sql_twin` が、空の配列、
  NULL、境界でない接頭辞（`a` と `ab`）、末尾の `/`、Unicode、`PathSet::build` を通っていない集合を含めて、Rust と同じ答えを固定する。
- **持ち越しの文**（1 つの UPDATE、購読側のトランザクション）: 対象は、プロジェクトの、target が着地のブランチの、開いた MR の行のうち、
  `status ∈ {can_be_merged, cannot_be_merged}`、`checked_head_sha = MR の head_sha`、`checked_target_sha = 着地の old`、対応物が偽。
  `checked_target_sha := new`、`computed_target_sha := COALESCE(computed_target_sha, checked_target_sha)`、`updated_at := now()`。
  `old` に答えていない行（遅れた・重複したイベント、持ち越されなかった着地の次の着地）は持ち越さず、今までどおり確かめ直す。
  すでに (head, new) に答えている行（同じバッチの先のイベントで付け替え済み、など）は「settled」として何もしない。
  ログ `forge.review.mergeability_carried` に `settled` を足した。
- **`computed_target_sha`** は、結果を実際に計算した target の先端。`record_check` が毎回 `checked_target_sha` と同じ値を書き、
  持ち越しは動かさない。**持ち越した行** ⇔ 集合の印が行自身のもの（下のレビュー対応 1）で、かつ
  `computed_target_sha IS NOT NULL AND computed_target_sha <> checked_target_sha`。
- **キューのバッチ**: エントリーごとに `repository.pushed` が出るが、どれも **同じ** `landed`（`old` = バッチの土台、`new` = 列車の先端、
  バッチ全体のパス。`diff-tree` はバッチに 1 回）を持つ。最初のイベントが持ち越し、残りは settled になる。
- **マージの条件の判定は変えていない**。これまでどおり、保存された結果は `checked_head_sha = head` かつ `checked_target_sha =
  target の生きた先端` のときだけ数える（`stored_mergeable`）。持ち越しは `checked_target_sha` を進めるので、持ち越した結果は
  計算した結果と同じに数えられる。足したのは表示用の `carried_from_target_sha`（状態の API）と `computed_target_sha`
  （mergeability の API）だけ。認可もルートも変えていない。
- **外れた持ち越しを残さない**（(c) に無かった）: 直接のマージの試しのマージか列車が `FG-REVIEW-0006` を出したとき、行が持ち越した
  ものなら計算し直しを積む（`recheck_if_carried`、info `forge.review.mergeability_carried_disproved`）。ページが「マージできる」と
  言い続けない。
- **停止スイッチ `review.path_filter = false`** は 3 か所を切る: 判定は集合を読まず書かない（列は NULL）、着地は差分を読まず
  `landed` を書かない、購読側は持ち越さない。S4 の前とまったく同じ動きになる。列が NULL の古い行、`landed` の無い古いイベント
  （と target への直接の push）も同じ（debug `forge.review.mergeability_paths_unused paths=absent`）。`landed` は 16 KiB まで
  （超える着地は不完全として送る）。
- **(c) 5 の表示**: 「N コミット進んだ」の N は出さない（ページの読み取りごとに git の起動が要る）。文は「`<branch>` の `<sha>` の時点で
  確かめた結果。その後の変更はこの MR のファイルに触れていないので結果をそのまま使っている。何かが入る前にマージキューがもう一度
  マージして確かめる。」（`mr.conditions.carried`、ja / en / id）で、マージの条件の欄に 1 行。マージ結果の ref と MR パイプラインの
  「`<sha>` 時点」の表示は S5（MR パイプラインの段）に残した。
- **マイグレーション** `20261229100200_merge_request_mergeability_paths`: null 可・既定なしの列 8 つ（`head_files`、`head_dirs`、
  `head_deleted_dirs`、`paths_complete`、`computed_target_sha` と、レビュー対応で足した印 `paths_rule`、`paths_head_sha`、
  `paths_seq`）。`ALTER TABLE … ADD COLUMN` は ACCESS EXCLUSIVE を取るが、既定の無い
  null 可の列はカタログの変更だけで表を書き換えないので一瞬。**索引は足さない**: 持ち越しの文は既存の索引で
  (project, target, open) の MR に絞ってから主キーで行を引き、配列の重なりはその行の上だけで評価する。GIN は毎回の判定の
  書き込みを重くするだけ。
- **残る限界**
  - 持ち越した `can_be_merged` には新しい先端のマージ結果の ref が無い。見回り（`sweep_missing_merge_results`、300 秒）は変えて
    いないので、着地が止まって 300 秒たつと 1 回計算し直す（着地が続く間は持ち越しが `updated_at` を進める）。必須の `ci/pipeline` が
    あるリポジトリの直接のマージは FG-CI-0013 で断られるが、断ったその場で計算し直しを積む（下のレビュー対応 3）。キューは
    列車が試すので影響なし。「target が動いただけでは MR パイプラインを積まない」は S5 が扱う。
  - 持ち越した `cannot_be_merged` は、触れ合う着地か新しい head まで残る。
  - git の改名の検出が複数の着地にまたがって初めて成立する形は、消したディレクトリの規則と、マージした木の差分で狭めたが、
    証明はしていない。外れても悪い `main` にはならず、列車が `FG-REVIEW-0006` で見つける。
  - 負荷試験のシナリオ `a`（着地 1 回あたりの判定のジョブ ≈ 0）は、この段の中では走らせていない。
  - merge base が複数ある（交差したマージの）MR は持ち越さない（下のレビュー対応 4）。

## Amendment（2026-11-19、計画 18 S4 のレビュー対応）

S4 の差分を 2 人の評価者（データベース、Rust）が読んだ。「持ち越した行のせいで衝突する変更が入る」経路は見つからなかった
（列車と直接のマージが必ずもう一度マージする）。直したのは、表示が間違いうる経路と、試験の薄いところ。

1. **集合は、どの判定が書いたかの印を持つ**。これまで「保存された集合は `checked_head_sha` のもの」は、`record_check` が同じ
   UPDATE で書くという約束でしかなかった。列を知らない書き手（serve と worker を順に入れ替えている間の古い worker、遅れたジョブ、
   ロールバック）は `checked_head_sha` / `checked_target_sha` だけを書き、集合と `computed_target_sha` は前の判定のまま残る。
   その行を持ち越すと、古い集合で「マージできる」と出る（入るものは変わらない: 列車が断る）。
   - 足した列: `paths_head_sha`（集合を読んだ head）、`paths_seq`（集合を書いた判定の `check_seq`）。`record_check` が結果と同じ
     UPDATE で書く（集合を読めなかったときも印は書く）。**行自身の集合** ⇔ `paths_head_sha = checked_head_sha AND paths_seq =
     check_seq`（`own_set!`）。持ち越しの文、`stored_paths`、`computed_target_sha` を読む側（`carried_from()`、状態の API）は、
     どれもこの条件を通す。印の合わない行は「集合が無い」= 確かめ直す、「持ち越した行ではない」。
   - `paths_seq` はレビューの指示に無かった列。`paths_head_sha` だけでは、古い書き手が **同じ head** を新しい先端で判定し直した
     とき（target が動いただけの再判定）を見分けられない: 集合は同じ head のものなので持ち越しは安全だが、`computed_target_sha`
     が前の先端のまま残り、計算した結果が「持ち越した」と表示される。`check_seq` は判定を積むたびに進むので、これで見分ける。
   - 残る隙間: 新しい書き手が記録した **同じ** `check_seq` を、古い書き手がやり直して上書きする場合（ジョブの再試行が別の
     プロセスに渡ったとき）。集合は同じ head のものなので持ち越しの判断は変わらず、`computed_target_sha` の表示だけが古いことが
     ありうる。順に入れ替えている間だけの話。
   - 判定を待っている行（`unchecked` / `checking`）は、これで `computed_target_sha` が null に読める（`bump_on` が `check_seq` を
     進めるので印が合わない）。API の `computed_target_sha` は「決まった結果を計算した先端」だけを言う。
2. **規則の版も行に書く**（`paths_rule`、いまは 1 = `forge_core::merge_paths::RULE_VERSION`）。`landed` は版を持っていたが、行は
   持っていなかった。持ち越しの文と `stored_paths` は今の版の行だけを使う。規則を変えるときは版を上げれば、古い規則で作った集合は
   全部「確かめ直す」になる（表の書き換えは要らない）。
3. **必須の `ci/pipeline` と持ち越した行**。持ち越した行には今の先端のマージ結果が無いので、`ci/pipeline` は `required_check_stale`
   （前のマージ結果に報告がある）か `required_check_missing` になる。どの判断を止めるかを確かめた:
   - (a) 人がキューに入れる（`POST …/merge-queue`）— **止まらない**。キューに入れる判断は `ci/pipeline` を列車の結果に置き換えて
     評価する（`merges/queue/mod.rs` の `TrainRuns` / `deferred_run()`）。mergeability は `stored_mergeable` で、持ち越した行も数える。
   - (b) 自動マージがキューに入れる — **止まらない**。`merges/auto.rs` の `blockers()` はマージ結果に報告される名前の
     「無い・古い」を落とし、`PipelineState` は赤のときだけ止める。入れる判断は (a) と同じ評価。
   - (c) 直接のマージ — **止まる**（FG-CI-0013、または `required_check_missing` の FG-REVIEW-0007）。これは正しい: 直接のマージには
     列車が無いので、今の先端のマージ結果の上の `ci/pipeline` が要る。問題は、計算し直しが見回り（300 秒、しかも着地のたびに
     時計が戻る）まで来ないことだった。
   - 決めたこと: (c) の条件は緩めない。直接のマージ（要求のときと、ロックの下のジョブ）が、**持ち越した行の上で、マージ結果に
     報告される必須のチェックが無い・古いことを理由に** 断るとき、その場で計算し直しを積む（`merges/carried.rs`、
     `recheck_if_carried`、info `forge.review.mergeability_carried_recompute_requested`）。判定のジョブが今の先端のマージ結果を
     作り、`record_merge_result` が MR パイプラインを積む。断りのコードと利用者への文は変えない（「新しいマージ結果での
     パイプラインが終わるのを待つ」は、積んだので本当になる）。ログの文脈に `mergeability=carried` が付く（ログだけ）。
   - 状態の API: 持ち越した結果では `checks[].waits_for_merge_result` は false（衝突などを解くのを待っているのではない）。画面は
     その行で「取り込み先はこの MR のファイルに触れずに進んだので新しいパイプラインは動かしていない。マージキューに入れると
     今の取り込み先で確かめてからマージする」と出す（`mr.status.hint.checksCarried`、ja / en / id）。
   - S5 との関係: S5 は「target が動いただけでは MR パイプラインを積まない。直接のマージは FG-CI-0013 `hint=merge_queue`、画面の
     『マージ』は『準備ができたらマージ』になる」。ここでやったのはその手前の最小の一歩で、持ち越した行はパイプラインを積まず、
     キューは待たずに入れられ、直接のマージは頼まれたときだけ計算する。`hint=merge_queue` とボタンの付け替えは S5 に残した。
   - 残る限界: 画面の直接の「マージ」ボタンは `mergeable` のときだけ出るので、必須の `ci/pipeline` がある target の持ち越した MR
     では出ない（キューのボタンは出る）。直接のマージを API / CLI で頼むか、見回りが計算し直すと出るようになる。
4. **merge base が複数ある head は持ち越さない**。集合は 1 つの merge base からの差分だが、`merge-tree` は merge base を合成して
   マージするので、差分に無いパスが結果に効きうる。判定はこれまでの `merge-base` の代わりに `merge-base --all` を 1 回呼び
   （git の起動は増えない）、2 つ以上なら集合を不完全として保存する（info `forge.review.mergeability_paths_incomplete`
   `reason=several_merge_bases`）。その MR は着地のたびに確かめ直す。
5. **パスの一覧を読むメモリーの上限**。`Git::changed_path_entries` の出力の上限は `max_paths × 4353` バイト（上限いっぱいで
   約 87 MB）だった。`min(max_paths × 160 + 1 MiB, 4 MiB)`（下限 8 KiB）にした: 既定の 5000 で約 1.8 MiB、20000 でも 4 MiB。
   1 MiB は集合そのものの上限（キーの合計）なので、これを超える一覧はどのみち不完全になる。切れた一覧は不完全
   （`reason=too_large`）で、エラーではない。レコードは 1 件ずつ切り出し、2 つ目の配列は作らない。
6. **規則を本物の git で確かめる試験**（`forge-git/src/mergeability_path_rule_tests.rs`）。小さな語彙（`a`、`a/b`、`ab`、
   大文字小文字、削除、ファイルをディレクトリに置き換え、ディレクトリの削除、改名）から土台の木と 2 つの変更を作り、規則が
   「触れ合わない」と言ったら、`git merge-tree` が衝突なしで、結果の木が 2 つの変更を順に（どちらの順でも）当てた木と同じである
   ことを確かめる。64 件、種は固定、失敗は `proptest-regressions/` に残る。SQL の対応物の試験の生成器にも同じ語彙の変更を足した。
7. **`landed` の予算（16 KiB）は読む側でも確かめる**。超えていれば `oversized` で全件を確かめ直す。持ち越しの文の時間の上限は、
   書き込みプールの接続の `statement_timeout`（`database.statement_timeout_ms`、既定 15 秒）と `lock_timeout`、その外側の
   購読側の `handler` の上限（20 秒）。文がロックするのは mergeability の行だけで、`merge_requests.head_sha` はロックなしで読む:
   版のトランザクションは `head_sha` を進めた **後に** 同じトランザクションで mergeability の行を書く（`downgrade_on`）ので、
   持ち越しが古い head を読んで先にコミットしても、その行は後から来る版のトランザクションが `unchecked` に戻す。
8. **索引は足していない**。持ち越しの文の `merge_requests` の絞り込み（プロジェクト、target、開いている）は既存の
   `merge_requests_one_open`（`(project_id, source_branch, target_branch) WHERE state = 'open'`）か
   `merge_requests_open_project`（`(project_id, id) WHERE state = 'open'`）が受ける。
9. 着地のパスを読む間にマージのロックを失ったときのコードは、これまでどおり `FG-DB-0002 reason=merge_lock_lost`
   （`Merger::bounded` を通すようにした）。ロックの喪失だけの登録済みコードは無い。

### 分かっていて残す限界

- **大文字小文字は 1 文字ずつそろえる**。完全な case folding ではない（`ß` と `SS` は別のキー）。Unicode の正規化（NFC / NFD）も
  しない。NTFS の短い名前（`PROGRA~1`）を「全体に効くパス」として扱うこともしない。どれも、大文字小文字を区別しない
  ファイルシステムへのチェックアウトでだけ意味がある。サーバーの git は区別し、入れる前に列車がマージして確かめる。
- **`recompute_carried` は衝突したときだけ動く**。試しのマージがエラーや時間切れで終わったときは動かさない: そのマージは
  どのみち断られ、次に頼まれたときにまた試しのマージをする。
- **足した列は読み取り専用のロールから読める**。入っているのはパスの名前だけで、リポジトリを読める人が見られるものと同じ。

## 追記（2026-11-19、計画 18 S3 の実装）— (a) の残り（キューと自動マージの起床）: 作ったもの、文との違い、既知の限界、S5/S6 が引き継ぐもの

上の Decision (a) の文は変えない。S3 が作ったのは (a) の 2（`forge_merge_queue` の分）・3・4・5（キューと自動マージの分）・
6（キューの分）・7。新しいエラーコードもマイグレーションも無い（`FG-DB-0024` を S2 の規則のまま使う）。

**作ったもの**

- **送り手**（`crates/forge-db/src/review/merge_queue/wake.rs`）: キューを進められるようにした書き込みは、同じトランザクションで
  2 つのことをする。キューを今すぐ期限にする（`next_run_at = now()`。手番が読む事実）と、`pg_notify('forge_merge_queue', <キューの id>)`
  を送る（暇な手番の待ちを終わらせる合図）。送るのは次のとき:
  - MR を列に入れた（`merges::enqueue_mr_decided`）、外した（`dequeue_mr_decided`）、止めていたキューを再開した
    （`set_paused_decided`。止めるほうと、何も変えなかった呼び出しは送らない）。
  - 列車のパイプラインが終わりの状態になった（`train_pipeline_ended_on`）: ランナーの最後のジョブの報告、回収されたジョブ、
    取り消し、生まれたときから終わっているパイプライン（定義が正しくない、ジョブが 0 件）。列車のものでないパイプライン、
    まだ終わっていないパイプライン、もう試していないバッチは、何も書かず何も送らない。
  - パイプラインを作るジョブが、パイプラインを残さずに終わった（列車に CI の定義が無かった、ジョブが dead になった）:
    `jobs::queue` の `complete` / `fail` の 1 文の中。
- **プロセス内の起床**: `Review` の enqueue / enqueue_auto / dequeue / 再開は、コミットの後に `MergeQueue::wake(queue_id)` と同じ
  `wake_queue`（`WakeHub::nudge`）を呼ぶ。LISTEN の接続の往復を待たない。
- **待つ側（キュー）**: `forge_review::QueueTurn`（`merges/queue/worker/turn.rs`）。`forge serve` は `merge_queue.workers` 個
  （既定 2）の手番タスクを、それぞれ監督つきで動かす。手番は「期限のキューを 1 つ取る → 1 手 → 返す」を期限のキューが無くなるまで
  続け、無くなったら次のうち早いものまで待つ: 通知、次の期限（待っているキューの期限か、返されなかった貸し出しの終わり。
  `merge_queue::next_due`）、安全網（`merge_queue.safety_poll_ms`、既定 30 s）。列車のパイプラインを待っているキューは
  `merge_queue.waiting_safety_poll_ms`（既定 60 s）先に次の期限を置く。
- **待つ側（自動マージ）**: `forge_review::AutoMergeWatch`（`merges/auto/watch.rs`）。`forge_events_sequenced` で起き、カーソルより
  後のイベントが名指しする MR だけを判定する（`auto_merge::named_by_events`: MR 自身のイベントと、その head かマージ結果の
  `check.completed`）。`forge_auto_merge`（下の違い 1）で起き、ペイロードの MR を判定する。同時に判定するのは 4 件まで。全件の
  見回りは `auto_merge.safety_poll_ms`（既定 30 s）ごとと、聞き手がつながり直したとき。
- **取りこぼし**（キューだけ）: 安全網の周期を丸ごと待った後（待ちの前後とも聞き手が生きている）に取ったキューが、その周期以上前から
  取れる状態で（DB の時計。`oldest_ready_ms`）、その手が何かをした（結果が `idle` / `paused` / `waiting` / `retry` 以外）ときだけ
  `FG-DB-0024` を出す（`channel=forge_merge_queue`、`queue_id`、`outcome`、`worker_id`、`safety_poll_ms`）。1 分に 1 回まで。
- **聞き手が無い・切れているとき**: キューの手番は最長 2 s ごと、自動マージは 5 s ごとの `tick`（どちらも計画 18 より前の間隔と動き）に
  戻り、つながり直したら全件を見る。この間は `FG-DB-0024` を出さない。
- **設定**（どれも新しいキー。以前の周期は定数だったので、移す古いキーは無い）: `merge_queue.safety_poll_ms`（30000、100〜300000）、
  `merge_queue.waiting_safety_poll_ms`（60000、100〜600000）、`merge_queue.workers`（2、1〜8）、`auto_merge.safety_poll_ms`
  （30000、100〜300000）。範囲の外は起動時に `FG-SYS-0002`。戻し方: `2000` / `10000` / `1` / `5000` で計画 18 より前の間隔になる
  （通知は送られ続けるので、遅くはならない）。

**1 つのキューを同時に進める手番は 1 つ**（守り方と証明）

S3 より前からの貸し出しをそのまま使う。`merge_queue::claim` が `FOR UPDATE SKIP LOCKED` で期限のキューを 1 行選び、`lease_owner` /
`lease_until`（300 s。1 手の上限 240 s より長い）を書く。貸し出しの間、そのキューは誰の `claim` にも出ない。返せるのは持ち主だけ
（`release` は `lease_owner` を条件にする）。通知は「誰が取るか」に関わらない: どのプロセスのどの手番が起きても、取れるのは 1 つ。
手番ごとに持ち主の名前が違う（`<worker>-<n>`）ので、同じプロセスの 2 つの手番も互いの貸し出しを返せない。
試験: `one_queue_is_never_stepped_by_two_turns_at_once`（2 つの手番が同時に `tick` して取れるのは片方だけ。着地の最中に起床を送り、
別の持ち主で `claim` しても取れない。同時に動いた着地は 1 まで）、`two_queues_progress_concurrently`（1 つのキューの着地を
止めている間に、別のキューのバッチができる）。

**止め方**

`forge serve` の停止は、自動マージ → キューの手番 → …→ hub → `db.close()` の順（S2 までと同じ並び）。待っている手番と自動マージは
停止の合図ですぐ終わる。手の途中（着地を含む）は切らず、終わりまで走らせる（`MERGE_SHUTDOWN_WAIT` まで待つ。過ぎたら
`FG-SYS-0005 operation=merge_queue_task_pending` を出して先へ進み、貸し出しは期限で切れて次の起動が引き継ぐ）。タスクが頼まれずに
終わったら監督がログを出して起動し直す（新しい手番 = 新しい待ち手。自動マージは見回りから始める）。

**(a) の文との違い**（どれも意図したもの）

1. **5 本目のチャネル `forge_auto_merge`**。マージできるかの確認が「マージできる」と答えたとき、その MR の id を同じトランザクションで
   送る（`review/mergeability/run.rs`）。(a) 4 はイベントだけで起きる形だったが、この答えは衝突が消えたとき以外はイベントを出さない。
   イベントを足すと outbox・監査・購読者の全部に波及するので、合図だけを足した。
2. **チェック実行の書き手には通知を足していない。** `check.completed` は outbox のイベントで、順序づけが `forge_events_sequenced` を
   送る（試験 `auto_merge_decides_a_merge_request_right_after_its_check_completes` が、完了したチェックで起床が届くことを確かめる）。
   (a) 4 のかっこ書きの「出さなければ」の枝は要らなかった。
3. **プロセス内の起床は `tokio::sync::Notify` ではなく `WakeHub::nudge`。** 待つ側が 1 つの受け手で、通知とプロセス内の起床の両方を
   待てる。hub の無いプロセス（単発のコマンド）では何もしない。
4. **dequeue はいつも送る**（「取れるようになったときだけ」より広い）。外れた MR がバッチの中にいたか、後ろに誰かいるかを書き込みの
   中で判定するより、空の見回り 1 回のほうが安い。
5. **`release` は、手の間に送り手がキューを期限にしていたら、それを上書きしない。** `claim` が読んだ `next_run_at` を持って帰り
   （`due_us`）、返すときに違っていたら早いほうを残す。以前は次の見回りが短い間隔 1 回先だったので上書きしても害が無かったが、
   長い安全網では、手の最中に届いた enqueue やパイプラインの終わりを上書きすると丸 1 周期遅れる。`claim` の文は、取れるように
   なってからの時間（`ready_for`）も返すように組み替えた。
6. **新しい結果 `retry`（最長 10 s 後にもう一度）。** 今は進めない理由の終わりを誰も知らせないもの — 失敗した手、他の worker が
   マージのロックを持っている着地の回復 — は、待ちの周期（60 s）ではなく最長 10 s 後に見る。以前は `waiting` と同じ扱いだった。
7. **`QueueLimits` に `idle` を足し、`poll` の既定を 10 s から 60 s にした。** 「止めている・することが無い」は `idle`（30 s）、
   「パイプラインを待っている」は `poll`（60 s）。以前はどちらも `poll`。
8. **暇な待ちは、安全網だけでなく「次の期限」でも終わる。** 待っているキューの期限や、死んだ手番の貸し出しの終わりを、30 s の格子に
   合わせずその時刻に見る。期限なのに取れないとき（書き手のトランザクションが行を持っている、他の手番が取った）は、50 ms から
   1 s まで倍々に延ばして待つ。
9. **待つ側は forge-cli ではなく forge-review の型**（`QueueTurn` / `AutoMergeWatch`）。計画は `serve_merge_queue.rs` /
   `serve_auto_merge.rs` に待ちを書く形だったが、待ちの規則を本物の DB と hub で試験するには forge-review の試験
   （`tests/queue_wake/`）から届く必要がある。forge-cli に残るのは監督・停止・失敗の間引きとバックオフ。`spawn` は `Arc` を
   取らなくなった。
10. **自動マージの見回りは `FG-DB-0024` を出さない。** 「準備ができた時刻」が 1 つに決まらない（判定は十数個の事実から）。設定の
    変更は、設計として見回りが拾う。

**既知の限界**

- **送り手の無い変化**: 列にいる MR に新しい版が push された・閉じられた、target が他から動いた。試し中のキューは待ちの期限
  （最長 60 s。以前は 10 s）で気づく。害は遅れだけ（着地の前に必ず読み直すので、古い列車は着地しない）。
- **止めているキューと、緊急停止で保留のエントリー**は 30 s ごとに見る。緊急停止の解除には送り手が無い（キューは最長 30 s、
  自動マージは次の見回り）。プロジェクトの `merge.auto` やインスタンスの許可の変更、`forge/doc-lint` の待ちの時間切れも見回りだけ。
- **安全網の周期より若い取りこぼしは、拾うが報告しない**（S2 の規則）。パイプラインの終わりの通知を取りこぼしたキューは、待ちの
  期限（60 s）で拾う。これは安全網の待ちではないので `FG-DB-0024` は出ない。
- **同じリポジトリの 2 つのキュー**（target が違う）は、着地でリポジトリのマージのロックに並ぶ。並行するのはバッチを作る・試す
  ところまで。
- **起床 1 回で、全プロセスの全手番が `claim` を 1 回ずつ試す**（プロセス数 × `merge_queue.workers`）。取れなかった側は次の期限を
  1 回読んで待ちに戻る。
- **自動マージのイベントの起床には、順序づけの周期（200 ms）が入る。** 1 回の起床で読むイベントは 200 件 × 10 回まで（残りは次の
  起床）。1 回の見回りは 20 回 × `batch` 件まで（残りは次の見回りの先頭）。
- **手番の数は書き込み用のプールと釣り合わせていない。** 手番 1 つは 1 手の間に接続を 1〜2 本使う。`merge_queue.workers` の上限は 8。

**S5 / S6 が引き継ぐもの**

- キューを進められるようにする書き込みを足すときは、同じトランザクションで `next_run_at = now()` と
  `merge_queue::notify_queue_on(conn, queue_id)` を書く。パイプラインを終わりの状態にする文を足すときは、その後で
  `train_pipeline_ended_on(conn, pipeline_id)` を呼ぶ（列車のものでなければ何もしない）。
- 手の結果の文字列を足すときは 2 か所を決める: `tick_timed` の次の期限（`waiting` = 60 s、`idle` / `paused` = 30 s、`retry` = 10 s、
  それ以外 = すぐ）と、`found_by_the_safety_net` の「何もしなかった」の一覧。
- 待ちは `QueueTurn::idle` と `AutoMergeWatch::wait` にだけ書く。forge-cli のループに待ちの規則を足さない。
- 決まりは S2 のまま: 通知は id だけ、起きた側は必ず表を読む、待ちは必ず有限、聞き手が無ければ自分の短い確認に戻る。

## 追記（2026-11-19、計画 18 S3 の最後のコミット）— `forge_refs`: 送り手と聞き手をつないだ。作ったもの、文との違い、既知の限界

S2 の追記の既知の限界「`forge_refs` の送り手と聞き手が無い」を閉じる。どの段の持ち物でもなかったので、S3 の最後に別の
コミットで入れた。

**調べて分かったこと（S8・S2 の追記の訂正）**

- S8 の追記の「他のプロセス（今は worker のマージ）」と、S2 の追記の「`merges/queue/worker.rs` は worker のプロセスで動く」は
  事実と違う。マージ・マージキュー・revert・autofix・version refs は **`forge serve` の中**で、serve の `Browser` の複製を
  持って動く（`forge-cli/src/serve.rs`）。`forge worker` の `Browser` は読むだけで、ref を書くハンドラーは無い。
- だから、今 serve の外で ref が変わるのは次の 2 つだけ: **もう 1 つの `forge serve`**（入れ替えの間の 2 プロセス、将来の
  複数台）と、**1 回きりの管理コマンド**（復元、履歴の取り込みなど）。
- S2 が書き手に数えた `forge-checks/src/risk.rs` と `forge-pipelines/src/spec_publish.rs` は書き手ではなかった。読む前に
  自分のキャッシュを捨てているだけ。この 2 か所は `Browser::forget_refs`（誰にも知らせない）に変えた。

**作ったもの**

1. **`forge_git::browse::Browser`** を 2 つの入口に分けた。
   - `invalidate(repo)`: **このプロセスが ref を書いた**。キャッシュを捨て、`on_refs_changed` で登録された関数に知らせる。
     書き手 15 か所は 1 行も変えていない（今までどおり `invalidate` を呼ぶ）。新しい書き手も `invalidate` を呼べば送られる。
   - `forget_refs(repo)`: キャッシュを捨てるだけ。**他のプロセスの通知を聞いた側**と、読む前に自分のキャッシュを捨てたい側が呼ぶ。
     知らせ直さないので、通知がプロセスの間を回り続けることはない。
   - `on_refs_changed(f) -> bool`: 知らせ先を 1 回だけ登録する（複製は同じ登録を共有する。2 回目は `false`）。forge-git は
     DB を知らないままにする（依存の向き）。`f` は書き手の経路で動くので、待ってはいけない。
2. **`forge_db::wake::refs_changed(db, repos)`**: 何個のリポジトリでも 1 文（`pg_notify('forge_refs', id)`）。書き込み用の
   プールで、`Db::within` の上限つき。
3. **`forge-cli/src/serve_refs.rs`**（`forge serve` だけ）:
   - **知らせるタスク**: 書き手は id を集合に入れるだけで、DB を待たない。タスクがまとめて 1 文で送る。失敗は、その失敗の
     コードと `task=refs_announce` で 1 回（続く間は 1 分に 1 回、回数つき）出し、**送り直さない**。
   - **聞くタスク**: wake hub の `forge_refs` を待ち、聞いた id ごとに `forget_refs` を呼ぶ。id でないペイロードは捨てる
     （debug の 1 行）。再接続・安全網では何もしない（下の「安全網」）。
   - 2 つとも `serve_merges::supervise` の下で動く。頼まれずに終わると `FG-SYS-0005 operation=merge_task_exited`
     （`task=refs_announce` / `task=refs_listen`）を出して、1〜30 s の間隔でやり直す。
   - **止め方**: push・マージなど ref の書き手が全部止まった後、hub と DB のプールを閉じる前に止める。知らせるタスクは残りを
     1 回送ってから終わる。待つのは最長 5 s で、過ぎたら `FG-SYS-0005 operation=refs_task_pending` を出して先へ進む。

**安全網**: 通知で捨てるキャッシュ（ref の一覧、ブランチのスナップショット、到達の答え）は、どれも `refs_ttl`（5 s。設定の
キーではない定数）で自分から古くなる。だから、通知が届かなくても間違いは最長 5 s で直る。正しさは通知に頼らない。新しい
エラーコードも、`FG-DB-0024` の報告も足していない（待っている仕事が無いので、「取りこぼした」と言える時点が無い）。

**文との違い**（どれも意図したもの）

1. **送るのは ref を書いたトランザクションの中ではなく、直後。** git の ref は DB の外にあるので、同じトランザクションに
   できない。書いた後で送れなかった場合は、上の安全網（5 s）が拾う。
2. **書き手ごとに送る文を書かず、`Browser::invalidate` の 1 か所から送る。** 書き手の半分は同期の文脈（`Drop`、クロージャ）で
   `invalidate` を呼ぶので、その場で DB を待てない。1 か所にしたので、書き手が送り忘れることもない。
3. **聞いた側が呼ぶのは `Browser::invalidate` ではなく `forget_refs`。** `invalidate` は知らせ直すので、聞いた側が呼ぶと通知が
   回り続ける。
4. **送り手は `forge serve` だけ。** worker には ref の書き手が無い（上の「調べて分かったこと」）。

**既知の限界**

- **書いたプロセスも自分の通知を聞き、その ref をもう 1 回読む**（`for-each-ref` 1 回。答えは変わらない）。
- **1 回きりの管理コマンドが書いた ref は通知されない**（`Browser` を持たない）。serve からは今までどおり最長 5 s 古く見える。
- **worker の `Browser` は聞かない。** worker のハンドラーは、必要な所で読む前に `forget_refs` を呼んでいる（今までと同じ）。
  それ以外の読み取りは最長 5 s 古い。
- **ref を書くたびに NOTIFY つきの短いトランザクションが 1 つ増える**（NOTIFY は COMMIT を直列にする。
  `ops/runbooks/db-unavailable.md` の「notify queue」）。push 1 回で 1〜2 回。
- 256 を超える別々のリポジトリの通知が、聞くタスクの 1 回の起床の間に溜まると、hub は「全部見ろ」（`Reconnected`）を返す。
  聞くタスクは何もしないので、その分は 5 s の安全網で拾う。

**引き継ぐもの**

- **ref を書く処理を足すときは、書いた後で `Browser::invalidate(repo)` を呼ぶ**（serve の `Browser` の複製で）。それだけで
  `forge_refs` も送られる。読む前に自分のキャッシュを捨てたいだけなら `forget_refs` を呼ぶ。
- `forge serve` の外（worker、管理コマンド）で `refs/heads/` を書く処理を足すときは、書いた後で
  `forge_db::wake::refs_changed` を送る（S8 の追記の決まりの、今の形）。

## 追記（2026-11-19、計画 18 S3 のレビューの後）— 直したもの、そのままにしたもの

S3（`6bd203af..5d9f85f7`）の 2 つの読むだけのレビュー（並行性: WARN、DB: WARN。CRITICAL / HIGH なし）の指摘を、コードで確かめて
から直した。直した順に、先に失敗する試験を書いた（1・2・4・6・9。修正を外した木で 5 件とも失敗、修正を戻して成功）。新しい
エラーコードは無い。マイグレーションを 1 つ足した（下の 7。測った結果による）。

**直したもの**

1. **「すぐ次」で返したキューを誰も起こさなかった。** `merge_queue::release` は、キューが今すぐ取れる状態で返ったとき（手が
   「すぐ次」を求めた、または手の間に送り手が期限にした）、同じ文で `pg_notify('forge_merge_queue', <id>)` を送り、
   `Released::Released { due_now }` を返す。返した手番が別のキューへ行っても、まるごとの待ちにいる手番（貸し出しを見て待った）が
   取る。プロセス内の起床ではなく NOTIFY にしたのは、`forge serve` が 2 つ動く形（入れ替えの間、将来の複数台。上の `forge_refs`
   の追記と `docs/deploy/test-server-guide.md` の「`forge serve` と `forge worker` が複数プロセスなら」）を文書が認めているから。
   同じプロセスの手番も同じ通知を聞く。費用は 1 手あたり NOTIFY 1 回（手は git の仕事で秒の単位）。試験
   `a_queue_released_due_now_wakes_a_turn_in_its_whole_wait`。
2. **`forge_auto_merge` を送りすぎていた。** 「マージできる」の答えごと（着地のたびに target の開いた MR 全部が確かめ直される）に
   送っていた。今は `auto_merge::notify_if_acting_on` が 1 文で、自動マージが扱う MR のときだけ送る。「扱う」は見回りと同じ
   `ACTS`: 開いている、head がある、プロジェクトが消えていない、`merge.auto` が `conditional` か `full`（**行が無ければ
   `full`**。MR ごとの「予約」は無いので、既定のプロジェクトでは条件を満たす開いた MR が全部対象で、取りこぼしは無い）。
   最後の判定の理由でさらに絞ることはしていない（forge-db が forge-review の理由の言葉を知ることになる）。`named` は 1 回に
   `NAMED_MAX`（200）件までで、`decide_named` が切って渡す。S4 の持ち越し（下の「main を取り込んで」）は答えを書き換えないので
   送らない。試験 `can_be_merged_is_announced_only_where_auto_merge_acts`（既定のプロジェクトは 1 回、`off` は 0 回）。
3. **イベントの読み残しを次の起床まで放っていた。** `decide_events` は上限（200 件 × 10 回）で止まったとき「まだある」を返し、
   watch は次の `wait` で待たずに `Events` を返す。1 回の `act` の量は今までどおり有限で、ループの先頭の停止の確認も効く。
4. **最初の見回りが失敗した後、イベントを番号 0 から読んでいた。** カーソルは見回りが成功するまで無い（`Option`）。その間に
   `Events` が来たら、今の最大の番号をカーソルにするだけで何も判定しない（それより古いものは、すぐ後の見回りが見る）。試験
   `auto_merge_reads_no_event_history_before_its_first_sweep`。
5. **つながり直し 1 回で見回りが 2〜3 回走っていた。** 2 つの受け手はそれぞれ 1 回ずつ `Reconnected` を返す。見回りを返すときは
   必ず（安全網、つながり直し、ポールから戻ったとき）両方の受け手に溜まっているものを `wait(0)` で引き取る。見回りはその後で
   読むので、引き取った通知の中身も見る。決定的な試験は書いていない（接続を切る時刻と受け手が気づく時刻の順が決まらない）。
6. **列車のパイプラインを作るジョブが貸し出しの期限切れで dead になっても、キューは 60 s 待っていた。** `jobs::reap` の 1 文に、
   `complete` / `fail` と同じ `poked`（パイプラインを残さずに終わった列車のキューを今すぐ期限にして送る）を足した。試験
   `a_train_create_job_that_dies_by_its_lease_wakes_the_queue`。
7. **`named_by_events` の 2 つの結合に索引が無かった。** 測った（使い捨ての DB。開いた MR 5 000、閉じた MR 15 000、マージ結果の
   ref 60 000、イベント 3 000、1 つのプロジェクト。200 件の 1 回の読み）: 今までの索引では開いた MR のマージ結合が 67 万行を
   捨て、ref は順に全部読む。実行 55.6 ms（20 回の平均 40.8 ms）。1 回の起床で最大 10 回なので、0.4 s 以上。索引 2 つ
   （`merge_requests_open_head (project_id, head_sha) WHERE state = 'open'`、`merge_result_refs_result (project_id,
   result_sha)`）で、どちらもイベントごとの索引の引き当てになり、実行 1.1 ms（平均 4.3 ms、往復込み）。文の書き換えでは
   既存の索引に sha の列が無いので届かない。マイグレーション `20261229100300_auto_merge_named_by_events_indexes.sql`
   （S4 の `20261229100200` より後）。作る間は両方の表に SHARE ロック（書き込みが待つ。読みは続く）。行数は MR の数の桁なので
   短い。
8. **`forge_refs` の数字のペイロードごとに世代の表が増えていた。** 世代は「リポジトリごとの表 + 共通の床」にした。表が 4 096 件に
   なったら空にして、床を表の最大より先へ進める（その前に始まった読みは保存しない。キャッシュの答えは 1 回ずつ読み直し）。
   メモリは有界で、正しさは変わらない。試験は `the_ref_list_is_cached_until_a_push_invalidates_it` に追加（1 万の別の id の後も
   有界、そのリポジトリの変更は今までどおり効く）。
9. **`FG-DB-0024` の誤報。** 他の手番の長い手の間に送り手が期限を押すと、返した後の `next_run_at` は押した時刻のままで、`ready_for`
   （`next_run_at` と `lease_until` の遅いほうから）は貸し出しの間も「取れる状態」と数えていた（返すときに `lease_until` を消す
   ため）。`release` は、手の前より古い押しを返した時刻に上げる（`GREATEST(LEAST(next_run_at, now() + next_in), now())`）。取れる
   ようになったのは返したときからなので、`oldest_ready_ms` もそこから数える。並びの順は、押されたキューの中で同じ（すぐ取れる）。
   試験 `a_queue_stamped_during_a_long_step_is_ready_from_its_release`（直す前は 10.008 s、直した後は 2 s 未満）。

**そのままにしたもの**（意図して）

- **送り手の無い変化は安全網の周期で気づく。** 列にいる MR に新しい版が push された・閉じられた（キューは最長 60 s、以前は
  10 s）、自動マージの緊急停止の解除とプロジェクトの `merge.auto` の変更（キューは最長 30 s、自動マージは次の見回り 30 s）。
  書き手を調べた: push と閉じるは `merge_request.pushed` / `merge_request.closed` のイベントを出すので**自動マージは**それで起きる。
  キューはイベントを聞いていない（`forge_events_sequenced` を聞いて MR からキューを引く仕組みは、手番の起床の数を増やすので
  安くない）。`merge.auto` の変更・プロジェクトとインスタンスの緊急停止（`review/auto_merge.rs` の `set_mode_decided`、
  `set_project_stop_decided`、`set_instance_stop_decided`、`set_full_auto_decided`）は監査だけでイベントを出さない
  （`forge-core/src/audit_details.rs` の `Self::AutoMerge(_) => None`）。害は遅れだけ。
- **自動マージの見回りは `FG-DB-0024` を出さない**（上の S3 の追記の違い 10）。通知を取りこぼしても遅れは最長
  `auto_merge.safety_poll_ms`。
- **N 個の手番が 1 つの締め切りを分け合って止まる。** 締め切りを過ぎても着地中の手番は切らずに手放し、
  `FG-SYS-0005 operation=merge_queue_task_pending` を出す。貸し出しの期限で次の起動が引き継ぐ。
- **書いたプロセスも自分の `forge_refs` を聞く**（上の `forge_refs` の追記の既知の限界）。

**MR を名指しするイベントの書き手**（自動マージが `merge_request` の集約のイベントで起きることの確認）

| 書き込み | 書き手 | イベント（集約 `merge_request`、id = MR） |
|---|---|---|
| 承認 | `crates/forge-db/src/review/approvals/commands.rs:344` `approve_mr_decided`（持ち越し `:215` も） | `merge_request.approved`（`entry`、`:57`、集約 `:82`） |
| 承認の取り消し・版の push で外れた承認 | 同 `:475` `unapprove_mr_decided`、`:122` `ended_records` | `merge_request.unapproved`（同じ `entry`） |
| スレッドの解決・解決の取り消し | `crates/forge-db/src/review/discussions/commands.rs:180` `resolve_discussion_mr_decided` | `merge_request.commented`（`thread_resolved` / `thread_unresolved`、`discussions/steps.rs:238`） |
| 下書きの解除 | `crates/forge-db/src/review/commands/edit.rs:57`（`MergeRequestChange::DraftChanged`） | `merge_request.draft_changed`（`commands/mod.rs:257`） |
| 自動マージの「予約」 | 無い。プロジェクトの `merge.auto`（`review/auto_merge.rs:220` `set_mode_decided`）がそれにあたる | イベントなし（上の「そのままにしたもの」。見回りが拾う） |

足りない通知は、MR の側には無かった。`merge.auto` の変更はプロジェクトの全部の MR に効くので、MR の id で名指しする通知は
足していない（見回りの仕事）。

**main を取り込んで**（S4、変更パスを見たマージ可能性）

S4 の持ち越し（`paths::carry_forward_on`）は、target が動いても重ならない MR の答えを確かめ直さずに残す。答えの状態は
「マージできる」のまま変わらないので、自動マージの判定も変わらない。だから `forge_auto_merge` は送らない（送るのは
`record_check` が「マージできる」を書いたときだけ）。

## 追記（2026-11-19、計画 18 S5 の実装）— (d) のうち作ったもの、文との違い、既知の限界、S6 が引き継ぐもの

### 作ったもの

1. **MR パイプラインは新しい head のときだけ**。マージ結果の書き込み（`forge_db::review::record_merge_result`）が、どの方針で
   パイプラインを積むかを引数 `MergeRequestPipeline` で受ける: `OnNewHead`（既定）は、新しい行がその head の **最初の**
   マージ結果のときだけ積む（同じ head の別の target の行があれば積まない = target の移動）。`OnTargetMove` は新しい行ごと
   （S5 の前）。`Requested` は人の依頼で、行がもうあっても積む（パイプラインのジョブはその素性で重複を除くので 1 本）。
   方針を決めるのは `forge_review::Mergeability`（`with_pipeline_on_target_move`、ジョブの依頼の印）。S4 の持ち越しと、直接の
   マージの拒否が積む計算し直し（`recheck_if_carried`）の新しいマージ結果も、同じ head なのでパイプラインを積まない。
2. **設定キー `ci.mr_pipeline_on_target_move`**（`FORGE_CI_MR_PIPELINE_ON_TARGET_MOVE`、既定 `false`、`true` / `false` 以外は
   起動の時の FG-CFG `invalid`）。`CiConfig` が読み、すべてのプロセスが検証するが、**効くのは `forge worker` だけ**（マージ結果を
   書くのはマージ可能性のジョブだけ。`forge serve` は書かない）。`worker::provision::Storage` → `MergeabilityHandler`。
3. **直接のマージの答え**: `merges::evaluate::blocked` は FG-CI-0013 に文脈 `hint=merge_queue` を付ける（コードは増やしていない）。
   forge-web のエラーの本体は、FG-CI-0013 のときだけ、閉じた一覧（`error::HINTS`）の語を `hint` として足す（`reason` の
   FG-INT-0007 と同じ作り）。OpenAPI の `Error` に省略できる `hint`（enum `merge_queue`）を足した。利用者に見せる文は今までどおり
   `user_message` と `request_id` だけ。
4. **今の target でパイプラインを動かす**: 新しいルート `POST /api/v1/projects/{id}/merge-requests/{iid}/pipeline`
   （`PipelineCreate`、`ProjectCtx`、`202`）。既存の `POST …/pipelines` は使い回さなかった: あれはブランチの先端の手動の実行
   （`source = manual`、MR の紐づけ無し）で、MR の `ci/pipeline`（マージ結果への報告）にならない。処理は
   `forge_pipelines::Pipelines::run_merge_request` → `forge_db::review::run_merge_request_pipeline_decided`（`ChangeTx` で
   `PipelineCreate` をロックした事実で判定し直し、mergeability の行をロックする）。答え: `queued`（今のマージ結果に積んだ）、
   `exists`（もう積んである。何も書かない）、`computing`（今の先端のマージ結果が無い: 依頼の印 `"pipeline": true` のついた判定の
   ジョブを積む。その印のついた判定がもう生きていれば同じ `check_seq` を返して何も書かない）。監査の行は `pipeline.run_merge_request`
   （`queued` と `computing` のときだけ）。権限マトリクスは Action ごとの表なので、`PipelineCreate` の既存の行（`developer_rows!`、
   許可 6・拒否 9）がこのルートを覆う。ルートの許可・拒否はルートの試験で確かめた。
5. **依頼の引き継ぎ**: 判定の依頼（`request_check_on`）は、行を進める同じ文で、生きている（queued / running）依頼つきの
   ジョブがあるかを読み、あれば新しいジョブにも印を付ける。着地・見回り・直接のマージの計算し直しが間に入っても依頼は消えない。
6. **web**: `ci/pipeline` が必須の target では、直接の「マージ…」を出さず、キューのボタンが主の「準備ができたらマージ」になる
   （説明の文を `aria-describedby` で結ぶ）。`ci/pipeline` が新しくないときは「いまの取り込み先でパイプラインを動かす」を出し、
   答えを 1 行の `role=status` で言う。状態の 1 行の `required_check_stale` の次の手は「準備ができたらマージ」か今の target での
   実行。チェックの行に「`<sha>` 時点」（古ければ「古い取り込み先」）を出す（S4 の追記が S5 に残したもの）。
   状態の契約には `checks[].target_sha` と `viewer.can_run_pipeline` を schema_version 1 のまま足した。

### 文との違い

- 計画の「ファイル」は `crates/forge-web/src/` に新しいルートを置くとしていた。置き場はマージリクエストのルートの下の新しい
  ファイル（`routes/merge_requests/pipeline.rs`）にした（`routes/pipelines.rs` は 1100 行を超えている）。
- 計画は「権限マトリクスの行」を先に書く試験に挙げた。マトリクスは Action ごとで、`PipelineCreate` の行がすでにある
  （新しい Action は作っていない）ので、行は足していない。代わりにルートの許可（Developer、サービスアカウントの `api` の
  トークン）と拒否（Reporter・Guest の FG-AUTH-0003、`read_api` のトークンとエージェントの読み取りトークンの FG-AUTH-0002）を
  ルートの試験 `the_merge_request_pipeline_route_is_decided_as_a_pipeline_run` で確かめた。
- 新しいコードもマイグレーションも足していない（計画どおり）。拒否は既存の FG-CI-0003（`operation=run_merge_request`、
  `reason=not_open|no_version|conflict`）と FG-REPO-0005。監査の action の名前に DB の CHECK は無い。
- `replace_merge_result`（`gc` が消したマージ結果を書き直す）は方針によらずパイプラインを積む: 消えたコミットへの報告は
  誰も取り出せないコミットのもので、これは着地ごとではなく `gc` の後にしか起きない。

### auto-merge と、キューの無いリポジトリ

forge では、すべてのプロジェクトの target にマージキューがあり、auto-merge はいつもキューに入れる（`AutoMerger::scan` →
`Review::enqueue_auto`）。だから「キューの無いリポジトリで、`ci/pipeline` が必須の MR の target が動いたら永遠に待つか」には
「待たない」と答える: `merges/auto.rs` の `blockers()` は `ci/pipeline` の missing / stale を落とし、`PipelineState::of` は新しい
実行だけを数え、`decide()` が止まるのは `PipelineState::Red` のときだけ。target が動いた MR は stale のまま（`None`）キューに
入り、列車のパイプラインが着地を決める。挙動の変化は 1 つ: 古いマージ結果で **赤** だった head は、target が動くと stale
（`None`）になり、1 回キューに入って列車が赤になり、`checks_failed` で外れ、その head は `sticky_removal`
（`merge_queue_checks_failed`）で止まる。列車 1 本の無駄で、繰り返さず、main は赤にならない。キューを止めているときは今までどおり。
`merges/auto.rs` は S3 の範囲なので変えていない。

### 既知の限界

- 依頼つきの判定のジョブが dead になった、またはファンアウトの上限で後回しにされた（`defer_checks_on` はジョブを積まずに
  消す）ときは依頼が消える。画面は古いままボタンを出すので、もう一度押せば足りる。dead になったとき worker は warn
  `forge.review.mergeability_ask_lost`（`error_code` はジョブを失敗させたコード、`merge_request_id`）を 1 回出す。ファンアウトの
  後回しと、時間切れ・落ちたジョブ（見回りの `job_stale`）では出ない。
- `queued` は mergeability の行が答える先端（`checked_target_sha`）のマージ結果に積む。着地の後、購読側が行を持ち越す・計算し
  直す前の一瞬（outbox の遅れ）に押すと、一つ前の先端に積むことがある。画面はまた古いと出すので、もう一度押せばよい。
- 依頼は、それで書いた最初のマージ結果が **使い切る**: 同じトランザクションで、その MR の生きた依頼つきのジョブから印を外す
  （レビュー対応）。だから 1 回押すと 1 本。残るのは、使い切るトランザクションのコミットと次の依頼の行の更新が重なる一瞬だけ。

### レビュー対応（2026-11-19）

1. **依頼の重複は、ジョブでなくパイプラインで見る**: 今の先端のマージ結果の `ci_pipelines` の行（`source = merge_request`、
   同じ MR と sha。`ci_pipelines_once` が受ける）があれば `exists` で、ジョブも監査の行も書かない。レビューの指摘の前提は半分だけ
   正しかった: `jobs_dedup` は dead だけを外すので、終わった（succeeded）作成ジョブは重複を止め続け、`ci_pipelines_once` が
   2 本目のパイプラインも止める。抜けていたのは、作成ジョブが dead のとき、または保持期間で消えた後の押下（ジョブと監査の行
   が増え、`queued` と答える）。
2. **「新しい head」は、行ではなくパイプラインで決める**: `OnNewHead` は、その head のマージ結果（新しい順に 100 件まで）に、
   `error` でないパイプラインの行か、dead でない作成ジョブ（`jobs_dedup` を鍵 `create_dedup_key` で引く）があるかを見る。無ければ
   積む。最初のパイプラインの作成が最後まで失敗した head は、次の target の移動で積み直す。force push で戻った head（A → B →
   A）は、前のパイプラインがあればそれで答える（そのパイプラインは古い target のものなので `ci/pipeline` は stale のまま:
   キューか、今の target での実行）。`.forge-ci.yml` が無い（行を書かずに succeeded の作成ジョブ）head は、そのジョブが保持期間で
   消えると、次の target の移動でもう一度作成ジョブを積む（パイプラインは作らない）。同じトランザクション・同じ MR の refs の
   ロックの中で読む。
3. **依頼は使い切る**（上の「既知の限界」）。
4. 既定（`OnNewHead`）の DB の試験、5. 失った依頼の warn — `review/merge_pipeline_tests.rs` と worker のハンドラ。

### S6 が引き継ぐもの

- 合成ハンドラ（`merge_request.prepare`）の「MR パイプラインの判断」は、この方針（`MergeRequestPipeline`）とジョブの印
  （`pipeline_asked`）をそのまま使う。`prepare` のペイロードにも `"pipeline": true` を通すこと（今の判定のジョブと同じ語、
  同じ引き継ぎ: `request_check_asking_on`）。
- `forge.review.merge_result_updated` の `pipeline` / `pipeline_enqueued` が「積まなかった理由」の証拠。証明 `c`（MR パイプライン
  ≤ 45）の数え方は `SELECT count(*) FROM forge.jobs WHERE kind = 'ci.pipeline_create' AND payload->>'source' = 'merge_request'`。

## 追記（2026-11-19、負荷試験の後）— 「準備できたらマージ」の早い断り、承認の変更パスの再利用、人のエントリの除外を自動マージが守る

**何が分かったか（2026-11-19 の負荷試験）。** `POST …/merge-queue` は、必須チェックが走っているだけの 409
（FG-REVIEW-0007）でも p50 3.5 s かかっていた（201 は p50 1.6 s）。`Review::enqueue` が、データベースだけで断れる
場合でも先に git を読んでいた: 2 つの枝の先端（`resolve_ref`）、承認のための `facts()`（`merge-tree`、`diff-tree`、
`rev-list`、CODEOWNERS の `ls-tree`）、その後でやっとロックの中の `evaluate()`。負荷の下では `merge-tree` 1 回で
p50 1.2 s かかった。調べ直して 1 点直した: 枝の先端は毎回の git ではなく、ブラウズ API の ref 一覧のキャッシュ
（`forge_refs` で捨てる）から読まれていた。git が走るのはキャッシュが冷えたときの `for-each-ref` 1 回だけ。

**変えたこと**

1. **早い断り**（`forge-review/src/merges/queue/early.rs`）。経路の `authorize()` と古い画面の確認（FG-REVIEW-0008）の
   後、git の前に、データベースだけの事実をロックなしで読む: MR の状態と下書き、先頭の必須チェック、未解決のスレッド、
   作業項目のリンク。それに `evaluate_merge()` をそのまま掛け、git が決める事実（ソース枝、試しのマージ、承認の
   コミッター・変更パス・CODEOWNERS、マージ結果に載っているかもしれないチェック）には一番通しやすい値を入れる。
   残す理由は git の値で変わらないものだけ（`decided_without_git`、網羅の `match`）。1 つでも残れば、同じ `blocked()`
   の誤り（同じコード、同じ文脈のキー、同じ `user_message`）を返す。何も残らなければ今までどおりの全体の判定に進み、
   ロックの中の `evaluate()` が最後に決める。生きているマージの試みがあれば早い断りはせず、全体の経路が
   FG-REVIEW-0011 を返す。
2. **201 の経路の git を 1 回減らした**（`approvals/load.rs`）。マージ可能性の確認が (head, target の先端) に
   ちょうど書いたマージ結果（`refs/merge-requests/<iid>/merge`）は、同じ組の `merge-tree` の木のコミットなので、
   その先端との差分が「マージで target が変わるパス」そのもの。あればそれを `diff-tree` で読み、`merge-tree` を
   しない。読めなければ（コミットが掃除された）試しのマージに戻る（「分からない」にはしない）。S4 の `head_files` は
   head 自身の差分で、別の先端で計算された持ち越しもあるので、承認には使わない（ちょうどではないから）。
3. **人のエントリの除外を自動マージが守る**（`forge-core/src/auto_merge.rs` の `removal_binds_every_entry`）。
   今までは自動マージのエントリの除外だけが同じ head を止めていた。人が入れた MR !33 を列車の lint が外しても
   （`doc_lint_failed`、FG-CI-0032）、自動マージ（`full`）が 30 s 後に同じ head を入れ直し、もう一度外されてから
   FG-REVIEW-0026 で止まっていた（列車が 1 本むだ）。head と target が証明する除外 — `checks_failed`、
   `doc_lint_failed`、`doc_lint_unavailable`、`doc_lint_base_not_clean`、`doc_lint_repo_config_invalid` — は、
   誰が入れたエントリでも同じ head を止める。同じ head という条件は残す。マージ結果のリスクによる除外は自動マージの
   エントリにしか起きないので、今までどおり。

**`dequeued` の決定。** 人が自分のエントリをキューから出したら、自動マージはその head を入れ直さない
（`held`、`dequeued_by_person`）。人がはっきり外したものを機械が戻すのは、人の判断を上書きすることになるから。
新しい head（push）で、自動マージはまた判定する。人はいつでも自分で入れ直せる。

**git の回数**（テストの世界: CODEOWNERS なし、ref 一覧は冷えている）

| 経路 | 前 | 後 |
|---|---|---|
| 409（データベースが断る: チェック・下書き・閉じている・スレッド・リンク） | 5 | 0 |
| 201 | 5（`for-each-ref`、`merge-tree`、`diff-tree`、`rev-list`、`ls-tree`） | 4（`merge-tree` なし） |

**分かっていて残す限界**

- 早い断りの `reasons` と `details` は、データベースだけで決まる理由しか持たない。承認や試しのマージの理由が
  同時にあれば、全体の判定はそれも並べ、順番で先頭（`reason`）が変わることがある（例: `approvals_missing` が
  先）。コードは同じ FG-REVIEW-0007（`required_check_stale` は列車が決める `ci/pipeline` でしか起きないので、
  早い断りには出ない）。利用者に見えるのは `user_message` と `request_id` だけ。
- 早い読みはロックなしなので、読んだ直後に緑になったチェックで断ることがある。その時点で頼まれたのと同じで、
  もう一度押せば全体の判定に進む。
- 枝の 2 つの先端は並べずに順に読む: 2 つ目は 1 つ目が読んだ ref 一覧のキャッシュから答えるので、並べると冷えた
  キャッシュで `for-each-ref` が 2 回になる。
- 201 の経路の残りの git（`diff-tree`、`rev-list`、`ls-tree`）は順に走る。並べるのは S6（準備のジョブ）の範囲。

**レビューの後（同じ日）**

- 早い 409 の `reason` が全体の判定と違いうること（上の限界の 1 つ目）は、そのままにした。409 の本文は
  `error_code`・`user_message`・`request_id` だけで（`forge-web/src/error.rs`）、画面の「止まっている理由」と
  `queueProves` は `GET …/status` の理由を読む。`gstn` も本文を出すだけ。`reason` を読むのはログだけなので、
  FG-REVIEW-0007 の `diagnosis`（`errors.toml`）に違いを書き、カタログを作り直した。テスト
  `the_early_refusal_names_only_the_reasons_the_database_decides` が、同じ状態の全体の評価（状態の読み取り）と早い断りで
  コードと `user_message` が同じこと、`approvals_missing` が先に来るときだけ `reason` が違うことを確かめる。
- 早い断りは何も書かない: エントリ・キューの行・監査の行は 0。チェックが緑になった後の同じ enqueue は受け付けられる
  （`a_running_required_check_refuses_enqueue_without_git`）。
- `checks_failed` は、人のエントリでも同じ head について止まったままになる（新しい push か、人が自分で入れ直すまで）。
  自動マージ自身のエントリの今までの決まりと同じ。

## 追記（2026-11-20、計画 18 S6 の実装）— (f) のうち作ったもの、文との違い、既知の限界

### 作ったもの

1. **1 つのジョブ `merge_request.prepare`**（`forge_db::review::prepare`、ハンドラーは `forge-cli/src/worker/prepare.rs`）。
   版のジョブを積んでいた 3 か所（`merge-request-refresh` の購読側の push、open、reopen）が、代わりにこれを積む。ペイロードは
   版のジョブと同じ（`merge_request_id`、`pushed_by`。S5 の `"pipeline": true` も読む）。優先度は版のジョブと同じ段 1、
   同時に 3 件まで（`KIND_CAPS`）。ハンドラーは 4 つのジョブが呼んでいた関数を順に呼ぶ: (1) `forge_review::refresh_prepared`
   （版・隠し ref・土台・判定の依頼・衝突の予測の依頼）、(2) `forge_review::check_asked`（判定）、(3) `Risk::run`（`forge/risk`）、
   (4) `SpecChecks::run`（`forge/spec-lint`、`forge/spec-trace`、`forge/doc-lint`、`forge/ci-lint`）。同じ関数なので、同じ行・
   同じチェック名・同じ結論・同じログを書く（不変条件。試験 `prepare_writes_the_same_check_runs_as_the_four_jobs` が、S6 の前の
   4 つのジョブの経路と結果を比べる）。1 回の実行に info を 1 行（`forge.review.prepared`: 各小段の `done` / `skipped` /
   `handed_over` と時間）。
2. **版のイベントの印**。prepare が記録した版の `merge_request.pushed` のペイロードに `"prepared": true` を足した
   （`record_mr_version_prepared`。イベントのペイロードは足すだけ、という Webhook の契約の内側）。`risk` と `spec-index` の購読側は
   印のある版にはジョブを積まない。印のない版（S6 の前に積まれた版のジョブ、他の呼び出し）には今までどおり積む。だから
   古い `merge_request.refresh` のジョブは配備の後も古い経路のまま正しく動き、ハンドラーも残した。
3. **判定の小段は待たせたジョブを後ろに置く**。判定の依頼は今までどおり `check_seq` を進め、判定のジョブを積むが、
   `BACKSTOP_DELAY`（120 s、判定の予算 90 s より長い）の後にしか動けない（`request_check_backstop`、
   `jobs::enqueue_superseding_after_on`）。prepare はその seq を自分で判定し、成功したらそのジョブを消し（`Backstop::Drop`）、
   失敗したら今すぐ動けるようにする（`Backstop::Wake`）。判定の間、そのジョブが依頼の「生きたジョブ」（`LIVE_JOB`）なので、
   見回り（`sweep_unchecked`）と回収（`reap_stale_checks`）は今までどおりに見る。prepare が途中で死んだら、120 s 後にそのジョブが
   自分で判定する。S5 の依頼の印は、prepare の印か、置き換えたジョブの印を受け継ぐ（`request_check_asking_on` と同じ）。
4. **失敗した小段は自分のジョブに渡す**。prepare を失敗させるのは版の小段（`step=refresh`）だけ（版のジョブと同じ再試行の規則）。
   判定・リスク・spec の小段が失敗したら、warn `forge.review.prepare_step_failed`（その小段のコード、`step`、
   `job_kind=merge_request.prepare`）を 1 回出して、その小段を自分のジョブに渡す: 判定は上の `Wake`、リスクと spec は購読側が
   積んだはずのジョブ（同じ重複排除のキー、`checks::risk::hand_over`、`checks::spec_index::hand_over`）。prepare は次の小段に進む。
   だから「失敗した小段だけをやり直す」は、その小段のジョブの再試行と最後の試行の `error` の答えで実現した。
5. **MR パイプラインの判断**。`Mergeability::with_ci_definition_probe`（worker は判定のジョブにも prepare にも付ける）: きれいな
   マージ結果の MR パイプラインを積む前に、マージした木に `.forge-ci.yml` があるかを `ls-tree` 1 回で見る。なければ方針を
   `MergeRequestPipeline::NoDefinition` にして積まない（パイプラインのジョブは同じ木を読んで `no_definition` で何も書かない
   だけだった）。人の依頼（`Requested`）は見ない（そのジョブが自分で答える）。見られなかったら方針はそのまま（ジョブが決める）、
   warn `forge.review.ci_definition_unread`。
6. **衝突の予測の相手を絞った**。`scan_peers(…, filter)`: `review.path_filter` が真なら、相手はパスが影響しうる開いた MR だけ
   （S4 の述語を 2 つの head のパスの集合に掛けた `mergeability::peer_interacts_sql`。どちらかの集合が分からなければ相手に
   入れる）。外した数は `Peers::passed_over`（debug `forge.review.conflict_peers_filtered`）。相手が上限を超えたときは今までどおり
   `incomplete` / `peers_capped`。予測は今までどおり低優先度の別のジョブ。

### 文との違い

- **重複排除のキー**: 計画は `prepare:<mr>:<head>` と書いたが、`mr_prepare:<id>:<trigger>`（`opened`、`reopened:<rev>`、
  `push:<seq>`）にした。head はジョブが読むもので積むときには分からない。また head のキーは成功したジョブが持ち続けるので、
  ソース枝を前の head に戻す push（A → B → A）が版を記録しなくなる。
- **head の一覧 1 回と `cat-file --batch` 1 回の共有**は作っていない。各小段は今までどおり自分で読む（リスク・spec の木の読み）。
  減ったのは claim の往復 3 回と、ジョブの行 3 本（判定の行は作って消す）。共有は S7 の文書の索引（内容アドレスの LRU）と
  一緒に行うのがよい。
- 計画の「ファイル」の `crates/forge-db/src/review/conflicts/` は `conflicts/run.rs`（`scan_peers`）、絞り込みの SQL は
  `mergeability/paths.rs` に置いた（S4 の述語の隣）。
- 新しいコード・キー・マイグレーションはない（計画どおり）。小段のコードは今までのもの。`PREPARE_TIMEOUT` は 360 s
  （60 + 90 + 90 + 120）。

### 既知の限界

- prepare が最後まで失敗した（`dead`）とき、版がもう記録されていれば、その head の `forge/risk` と spec の検査は出ない
  （印のある版なので購読側も積まない）。見回りは `check.risk` / `check.spec_index` の死んだジョブしか見ない。新しい push か
  開き直しで直る（Runbook `job-dead-lettered` に書いた）。prepare が版の後で失敗するのは、小段の後始末の DB の失敗だけ。
- prepare の後の試行（`attempt > 1`）は、版が `already_head` でも検査の小段を走らせる。結果がもうある小段（`forge/risk`、
  `forge/spec-lint` が forge の行である）は飛ばす。引き継いだジョブと後の試行の両方が走ることはありうる（同じ答えを 2 回書く）。
- 判定が 120 s を超えると、待たせたジョブも動き出して同じ seq を 2 回判定することがある（結果は同じ、seq が守る）。
- 衝突の予測の絞り込みは、相手の集合が自分の今の head のものであるときだけ効く（`checked_head_sha = head_sha`）。prepare の後に
  積まれた予測は、判定が先に集合を書くので効きやすい。

### レビュー対応（2026-11-21、rust・database のレビュー）

1. **（HIGH）死んだ prepare の検査に答える**。版を記録した後で prepare が `dead` になると、印のある版なので購読側は積まず、
   見回りは `check.risk` / `check.spec_index` の死んだジョブしか見ないので、`forge/risk` と spec の 4 つの検査（doc-lint、
   ci-lint を含む）が永遠に出なかった。worker の新しい作業 `prepare_sweep`（60 s ごと、`risk_sweep` と同じカーソル）が、
   7 日以内に死んだ prepare のうち、開いた MR の今の head にその検査がないもの（`unanswered_dead_prepares`、その MR に生きた
   prepare があれば除く）に、死んだジョブのコードと要求 id で `error` を書く。答えた head は二度と並ばない。試験
   `a_dead_prepare_job_after_its_version_gets_its_checks_answered`。上の「既知の限界」の 1 つ目はこれで閉じた。
2. **引き継ぎがジョブを積めないとき**（MEDIUM 1、2）。`hand_over` は `HandOver::{Queued, Pending, Held}` を返す。`Held`
   （キーを終わったジョブが持つ。A → B → A などで前にそのキーで評価した head）か、DB の失敗なら、その小段をその場で
   `error` で答える（`Risk::fail`、`SpecChecks::fail`）。error `forge.review.prepare_hand_over_failed`（小段のコード、`reason`
   = `key_held` か DB のコード）。もう prepare を失敗させない（判定をやり直させない）。warn
   `forge.review.prepare_step_failed` は、本当に自分のジョブが再試行するときだけ出す。
3. **待たせたジョブの後始末の失敗は prepare の失敗ではない**（MEDIUM 2）。warn `forge.review.prepare_backstop_unsettled`
   を出して次の小段に進む（そのジョブは遅れの後に動く: 同じ seq の 2 回目の判定か、少し遅い再試行）。
4. **時間**（MEDIUM 3）。`PREPARE_TIMEOUT` は 420 s（小段の予算の和 360 s に余裕）。`BACKSTOP_DELAY` は
   `MERGEABILITY_TIMEOUT + 30 s` から作る。依頼から数えるので、判定がそれより長いと待たせたジョブと並ぶことがある。
   「決して並ばない」という文は直した（並んでも同じ seq の 2 回目の判定で、結果は seq ごと 1 回）。
5. **置き換えで列の場所を失わない**（MEDIUM 4）。遅れつきの置き換えは `LEAST(置き換えたジョブの最小の run_after,
   now + delay)`。すでに動けるジョブを置き換えたら、新しいジョブもすぐ動ける（prepare の判定と並ぶことがある。上と同じ）。
6. **記録済みの head**（LOW）。`already_head` と `restored` でも検査の小段を走らせ、その head に結果がある小段は飛ばす
   （1 回目の試行でも。開き直しや前の head への push で、結果がなければ埋まる）。
7. **衝突の予測の絞り込みと古い予測**（LOW）。`record_scan` は試した組だけを書き、試さなかった組を消さない。それでも
   絞り込みで外れた組に古い「衝突」は残らない: 2 つの MR が衝突するのは同じパスを変えるときだけで、それは S4 の述語では
   必ず「影響しうる」（外れない）。読み手は両方の head が今のものの予測しか信じない（`a_prediction_about_an_old_head_is_not_read`）。
8. 試験を足した: 判定の小段の失敗で待たせたジョブが起きる、spec の小段の失敗の引き継ぎ、キーが持たれた引き継ぎは
   `error` で答える、版の小段の DB の失敗は再試行・壊れたペイロードは即 `dead`。

### 証明について

計画の証明（`a-burst` で作成から enqueue まで p95 ≤ 5 s、MR あたりのジョブ ≤ 2）は、負荷試験（`ops/loadtest/`）で測る。
このコミットでは測っていない。数え方: `SELECT kind, count(*) FROM forge.jobs WHERE created_at > <開始> GROUP BY kind`
（S6 の後、doc の MR あたり `merge_request.prepare` 1、`check.spec_drift` 1、`merge_request.conflict_scan` 1。判定・リスク・
spec のジョブは小段が失敗したときだけ）。

## 追記（2026-11-23、計画 18 S7 の実装）— (b) の 3・5・6・7: 作ったもの、文との違い、既知の限界

上の Decision (b) の文は変えない。S7 が作ったのは (b) の 3（増分・内容アドレスの索引）・5（バッチ 32 / 4）・6（同じ手番での着地）・
7（`assess_train`）。新しいエラーコードは無い。

### 作ったもの

1. **スキーマ**（マイグレーション `20270104100000_merge_queue_batch_limit_64`）: `merge_queues.batch_limit` と `merge_queue_batches.size`
   の CHECK を 1..64 に（ADR-0030 のレビューの型: 新しい CHECK を `NOT VALID` で足し、`VALIDATE` してから古いものを消して名前を戻す）。
   行は書き換えず、`batch_limit` の既定も 4 のまま（下の「レビュー対応」1）。`forge_core::merge_queue::MAX_BATCH` は 64（スキーマの
   上限）。試験 `batch_limit_accepts_64_rejects_65`。
2. **バッチの上限**: `batch_bound(保存した上限, 判定がプロセス内か, doc_batch_max, ci_batch_max)`: 保存した上限が 4（`FULL_MARK`、
   「下げていない」）なら種類の上限、1〜3 ならそれと種類の上限の小さいほう、1..64。
   「プロセス内」は土台に `.forge-ci.yml` が無いこと（`root_entry` を 1 回。読めなければ CI ありとみなす = 小さいほう）で決め、列車の
   先端でもう一度見る。列車の途中で `.forge-ci.yml` が入ったら CI の上限まで切り詰める。切り詰めた後ろで `conflict` などで断られた
   MR は外さない（その接頭辞は試されないので）— 断りは「何段目の列車に対してか」を持って後回しにし、残った列車の中のものだけを外す。
   設定 `merge_queue.doc_batch_max`（32、1〜64）、`merge_queue.ci_batch_max`（4、1〜64）。試験
   `a_doc_queue_builds_batches_of_32_and_a_ci_queue_of_4`、`a_train_that_brings_a_ci_definition_in_takes_the_ci_bound`。
3. **同じ手番での着地**: パイプラインのジョブを積まなかったバッチ（列車に `.forge-ci.yml` が無い）は、`batch_built` の後、同じ手番で
   試しの段（`poll`: エントリーと target の読み直し、列車の doc-lint、マージ結果の判定、着地）に進む。状態は `testing → landing → landed`
   のまま。試しの段のコードは S7 の前と同じ関数で、手番が 1 つ減っただけ。CAS の後で止まったバッチは、次の手番の回復
   （`recover`）が git を見て記録する。試験 `a_doc_batch_lands_in_the_tick_that_built_it`、
   `a_restart_mid_landing_recovers_a_same_tick_batch`（`with_crash_after_cas`）。
4. **増分の lint**（`forge_doc::lint::incremental::LintCache`、`forge_checks::train_lint::DocIndex`）: 文書を blob の id で引く解析の
   キャッシュと、(パス, blob) ごとの検査の結果。検査の結果は、その検査が**索引で引いた id と、そのときの entry**（無いことも含む）と、
   型の定義の世代と一緒に保存し、次の木でそれが全部同じときだけ使う。これが「逆引きの参照」: ある id を足す・消す・改名する・変える
   文書は、その id を引いた文書だけを検査し直させる。索引そのものと重複 id の判定は、保存した解析から毎回作り直す（解析しないので安い）。
   保存した検査は「足した問題の列」で、同じ report に流し直すので、重複の除き方・順番・ファイルごとの上限まで全件の `lint()` と同じに
   なる。試験は性質 `incremental_lint_equals_full_lint`（追加・削除・改名・書き換え・参照・ブロック・型の定義の変更をランダムに、
   十分な大きさのキャッシュと、毎回ほぼ全部を捨てる大きさ 1 のキャッシュの両方で）と、forge-checks の実 git の試験（索引あり／なし、
   冷たい・温かい、大きさ 1 で同じ判定）。`forge serve` の中で、リポジトリごとの `LintCache` を全体で `merge_queue.doc_index_bytes`
   （既定 64 MiB、`0` で索引なし = S1 の読み方）に収め、超えたら最も古く使ったリポジトリから丸ごと捨てる。列車の木は `ls-tree -r`
   1 回で blob の id が分かるので、持っていない文書だけを `cat-file --batch` で読む（型の定義は毎回読む。64 件まで、小さい）。
5. **`assess_train`**（`MergeResultAssessor` の既定のメソッド。既定の実装は段ごとに `assess` を呼ぶ = 定義から等しい）: forge-checks の
   `Risk` は、AI 資産の設定を 1 回だけ読み、マージベースを 1 回だけ計算する（列車の土台が最後の段の祖先なら、どの段のマージベースも
   土台。キューは段を前の段の上に作るので常にそう。違えば段ごとに計算する）。段ごとの評価の予算は変えず、全体は今までどおり
   `MERGE_RESULT_BUDGET`（90 s）。試験 `assess_train_equals_per_step_assess`、`a_32_member_judgement_stays_within_the_budget`
   （32 段で 30 s 未満）。

### 文との違い

- (b) 7 の「各段の diff を前の段から 1 回だけ読んで積み上げ、O(変更パスの合計)」は作っていない。`forge/risk` の規則は土台からの
  累積の diff（改名の検出、大量削除の行数、ファイルの数え方）で決まるので、段ごとの diff を足し合わせても等しくならない。等しさを
  先に守り、段ごとの `risk_diff` は残した（段ごとに git 1 回。マージベースの 1 回を減らした）。32 段の時間は試験で上限を見ている。
- (b) 3 の逆引きの索引は「id → それを参照する文書」の表ではなく、文書ごとに「引いた id とその entry」を持ち、次の木で比べる形にした。
  参照の書き方（`[[ID]]`、`related`、`supersedes`、`docref` / `blockref`、`replaced_by`）を全部列挙しなくても、索引への問い合わせを
  1 か所（`Index::get` / `contains_key`）で記録するので漏れが無い。比べる費用は引いた id の数。
- S1 の既知の限界「予算は待つのをやめるだけ」は残る（1 本の木の lint は途中で止まらない）。ただし温かい索引では読むのも解析するのも
  変わった文書だけになり、1 本の重さが小さくなった。
- 計画の「ファイル」の `crates/forge-db/src/review/merge_queue/` は、`MAX_BATCH` を使う既存の箇所がそのまま新しい上限になったので、
  試験（`limit_tests.rs`）だけを足した。キーは `config/keys.rs` と `config/merge_queue.rs`。

### 既知の限界

- `gstn queue list` の「次のバッチの上限」は保存した上限（ふだん 4 =「下げていない」）で、キューの種類の上限（32 / 4）は含まない。
  表示を変えるのはクライアントと API の変更なので、ここではしていない（Runbook `merge-queue` に読み方を書いた）。
- 同じ手番での着地には切り替えのキーが無い。パイプラインが無いときの答えは次の手番でも同じなので、戻す必要が無いと判断した。
  バッチの大きさは `merge_queue.doc_batch_max = 4` で S7 の前に戻せる。
- 索引は `forge serve` のプロセスのメモリにあり、再起動で空になる（最初の列車は木を全部読む）。2 つのキューが同じリポジトリの木を
  同時に lint すると、それぞれが別のキャッシュで動き、後に戻したほうが残る（答えは変わらない。遅くなるだけ）。
- 証明（計画の `a-burst` ≥ 200/分、`a` の p50 ≤ 15 s など）は負荷試験（`ops/loadtest/`）で測る。このコミットでは測っていない。

### レビュー対応（2026-11-24、rust・database のレビュー）

1. **（db MEDIUM）ロールバックとマイグレーションの順序**。最初の版はマイグレーションで全キューの `batch_limit` を 64 にし、既定も 64 に
   した。S7 の前のビルドは 4 を超える保存値を読めない（`stored_queue_limit_invalid`）ので、ロールバックや「マイグレーションが先、
   バイナリが後」の間に全キューが止まった。直し: 行も既定も変えない。保存値 4 を「下げていない」の印（`FULL_MARK`）とし、新しい
   ビルドはそれをキューの種類の上限として読む。二分・切り詰めは 1〜3 で保存する（`stored_lowered`。文書のバッチで 4 段目以降の切り
   詰めは 3 として保存 — 次のバッチが必要より小さくなるだけで、着地で戻る）。着地は 4 に戻す。だから保存値は常に 1..4 で、S7 の前の
   ビルドも読める（バッチの大きさは読むときに範囲を見ないので、大きさ 5 以上の行も読める）。ロールバックの確かめ方を Runbook
   `merge-queue` の Mitigate に書いた。
2. **（MEDIUM）索引のロックの poison**: 消した後に `clear_poison()` を呼ぶ（呼ばないと、以後の取り出しと戻しのたびに全部を捨てて
   warn を出し続けた）。
3. **（MEDIUM）大きさの見積もり**: 型の定義の bytes を数える（捨てられないので、上限を超えても残る）。解析した文書は bytes の 2 倍に
   加えて解析した項目（front matter の欄、見出し、ブロック、参照、問題）ごとに 96 bytes とし、短い欄の多い front matter を少なく
   見積もらないようにした。試験 `the_estimate_covers_a_dense_front_matter` が、見積もりが解析した値の大きさ（`Debug` の文字数）以上、
   その 6 倍以下であることを見る。上限は**およその**ものとモジュールの文書に書いた。
4. **（LOW）取り消しでキャッシュを失わない**: 取り出したキャッシュはガード（`Taken`）が持ち、落とされたときに戻す。blocking の lint は
   ガードごと持つので、呼び出し側が予算で待つのをやめても、lint が終わったときに戻る。panic で巻き戻るときは戻さない（途中の状態）。
5. **（LOW）同じ手番の試しの失敗**: バッチを作った後の試しの段が失敗しても、手番は `Err` ではなく `retry`（`batch_built` の行は出ている）
   を返し、失敗はその場で `operation=merge_queue_same_tick`、`queue_batch_id` とバッチの `request_id` つきで 1 回ログに出す。次の見回りは
   失敗した手と同じ `RETRY`（10 s）以内。
6. **（LOW）貸し出しと 32 件の同じ手番**: 1 手は `forge serve` の `TICK_TIMEOUT`（240 s）で切られ、貸し出し（300 s）はそれより長い
   （試験 `the_safety_nets_are_the_configured_ones_and_nothing_else_moves` が `lease > TICK_TIMEOUT` を見る）。だから 32 件の組み立て +
   lint（予算 10 s）+ マージ結果（予算 90 s）+ 着地が最悪でも、手の間に他の手番がキューを取ることはない。手が 240 s で切られたら、
   バッチは `testing` か `landing` のまま残り、次の手（同じか別の手番）が試しの段か回復で引き継ぐ。貸し出しの延長は入れていない。
   測った値: 32 段のマージ結果の判定 22 s（Windows、試験環境）、33 件を 2 つのバッチで着地させる試験全体で 32 s。

## 追記（2026-11-28、計画 18 S9 の実装）— (d) 4 の投機の列車: 作ったもの、決めたこと、既知の限界

上の Decision (d) 4 の文は変えない。新しいエラーコードは無い（`predecessor_failed` はバッチの `reason` で、MR は外れず列に戻る）。

### 作ったもの

1. **スキーマ**（マイグレーション `20270115100000_merge_queue_speculation`）: `merge_queue_batches.depth`（1..8、既定 1、CHECK
   `merge_queue_batches_depth`）と `parent_batch_id`（自分の表への外部キー `merge_queue_batches_parent`、索引つき）。
   `depth = 1 OR parent_batch_id IS NOT NULL`（`merge_queue_batches_speculative`）。生きたバッチの一意性を (queue, depth) ごとに
   （`merge_queue_batches_live_depth`）し、それを作ってから同じトランザクションで `merge_queue_batches_one_live` を落とした。新しい
   CHECK と外部キーは `NOT VALID` + `VALIDATE`（ADR-0030 のレビューの型）。試験 `speculation_allows_one_live_batch_per_depth`。
2. **設定** `merge_queue.speculation_depth`（1、1〜8。`forge_core::merge_queue::MAX_SPECULATION` = スキーマの上限）。
   `QueueLimits::speculation_depth`、起動のログに `speculation_depth`。試験 `merge_queue_speculation_depth_has_a_default_and_a_range`。
3. **worker**: 手番は先頭（depth 1）だけを判定する（今までの `poll` / `recover` のまま）。先頭が `waiting`（パイプライン待ち）で、止めて
   おらず、生きたバッチが D 本未満なら（純粋な `builds_behind`）、いちばん深い生きたバッチの列車の上に 1 本作る（`build(.., Some(親))`:
   土台 = 親の `train_sha`、target は読まない、同じ手番の判定はしない）。作る前に、後ろのバッチのエントリーが減っていれば
   （外された等）そのバッチを `entry_left` で捨てる（その後ろも一緒に）。`db::create_batch` は、キューの行をロックしたまま、親がいちばん
   深い生きたバッチであること・その列車が新しい土台であること・生きたバッチが D 本未満であることを確かめてから 1 つ深く書く
   （違えば `raced`）。
4. **着地は順に**: 先頭は今までどおり target が `base_sha` のときだけ着地する。`batch_landed` は同じトランザクションで後ろのバッチを
   前から 1 行ずつ `depth - 1` にする（一意の索引が途中で 2 つを同じ場所に見ないように）。次の手番で、新しい先頭（土台 = 今の target）が
   判定される。
5. **前が着地しなければ後ろも**: `db::end_batch_chain` が、バッチを終える同じトランザクションで、それより深い `testing` のバッチを全部
   `abandoned`、`reason=predecessor_failed` にし、エントリーを `queued` に戻す（数えない・外さない）。先頭の赤（二分は先頭にだけ）・
   捨て（`target_moved` など）・期限切れ・列車の doc-lint の赤や lint できない・マージ結果・着地の途中の回復の作り直し、すべてが同じ
   関数を通る（worker の `end_chain`）。捨てた後ろのバッチは 1 本ずつ `forge.merge_queue.batch_abandoned`（`depth`、
   `predecessor_batch_id`、`predecessor_reason`）。`end_batch` は `end_batch_chain` の薄い包みで、署名は変えていない。
6. **隠した ref**: `refs/merge-queue/<queue_id>/train` は 1 本のまま、いちばん深い生きた列車を指す（浅い列車はその祖先なので gc で
   消えない。パイプラインは自分の `refs/pipelines/<id>/head` を使う）。着地で後ろが残るなら消さない。鎖が切れたら、生きたものが
   無ければいちばん深かった列車の位置で消し、残れば残ったいちばん深い列車へ戻す（`update-ref` の旧値つき、タイムアウトつき）。
   保守の掃除（生きたバッチの無いキューの ref だけを消す）は変えていない。
7. 試験（forge-review、実 git + 開発 DB、`tests/queue_speculation/`）: `a_second_train_lands_only_after_the_first`、
   `a_red_first_train_abandons_the_second`、`a_target_moved_by_a_direct_push_abandons_every_live_train`、
   `recovery_with_two_live_batches_records_only_what_git_shows`（CAS の後で止まった先頭を、再起動が記録し、後ろは残って着地する。
   さらに別の再起動が何も記録し直さない = 冪等）、`a_landing_interrupted_before_the_swap_drops_the_trains_behind_it`、
   `an_entry_leaving_a_speculative_train_drops_it_and_those_behind`、`depth_1_behaves_like_adr_0030`。深さ 1 が今までと同じことは、
   既存のマージキューの試験を**変えずに**全部通すことで示す（既定は 1）。

### 決めたこと（利用者の常設の指示「推奨で進めて」による）

- **深さは「生きた鎖の中の位置」**。着地で前へ詰める。位置を作ったときのまま増やし続ける案は、CHECK の上限が付けられず、
  「深さ ≤ D」が表から読めないので退けた。
- **判定は先頭だけ**。後ろのバッチのパイプラインが先に赤でも、それは前の列車の変更を含んだ結果なので、先頭が決まるまで待つ。
  先頭が着地すれば、それが先頭になったときに今までどおり二分する。決定 4 の「二分は最初の赤のバッチにだけ」をこの形で守った。
- **投機の組み立ては、列車の段で MR を外さない**（衝突、fast-forward できない、身元が正しくない）。まだ赤になりうる列車の上の答え
  だから。バッチはその MR の前で止まり、MR は列に残って深さ 1 と同じ土台で試される。MR 自身の事実（閉じた、新しい head、
  target の変更、自動マージの止まり）はどの深さでも今までどおり外す。
- **文書のキューは深さを使わない**。計画は「doc のキューは深さ 2 を使ってよい」とするが、S7 で文書のバッチは作った手番の中で
  着地まで進むので、後ろに作る隙間が無い（先頭が `waiting` にならない）。組み立てを着地と並べるには、1 つのキューを 2 つの手番が
  同時に動かす必要があり（貸し出しは 1 つ）、それは S10 の「着地の区間」の仕事として残す。
- **既定は 1 のまま**。計画 18 §5 決定 2（テスト用ランナー `docker-test-m4` の並行数）はまだ開いている。ランナーが D 本を並行
  できなければ、投機は CI を無駄にするだけ。上げるのは並行数を測った環境だけ（Runbook `merge-queue` の「投機の列車」）。

### 既知の限界

- 期限（`deadline_at`）はバッチを作ったときから数える。先頭が遅いと、後ろのバッチは待っているだけで期限に近づき、先頭になった
  ときにパイプラインがまだ終わっていなければ期限切れとして数える。`forge doctor` の `review.merge_queue` も、期限を過ぎた後ろの
  バッチを「止まった」と数える。深さを上げるのは CI が期限よりずっと短い環境なので、今は変えない。
- `gstn queue list`（API の `batch`）は先頭のバッチだけを示す。後ろのバッチのエントリーは `testing` として並びに出る。表示を
  変えるのは API とクライアントの変更なので、ここではしていない。
- 証明（深さ 3、ランナーが 3 本を並行できる `c` で ≥ 60/分）は負荷試験で測る。ランナーの並行数が分かるまで測れない。

### レビュー対応（2026-11-28、database・rust のレビュー）

1. **（db HIGH）版の番号の衝突**: 最初の版は `20270114100000` で、同じ日に `main` に入った計画 19 のマイグレーションと同じ番号だった。
   `20270115100000` にした。
2. **（db HIGH）鎖を終える・着地するトランザクションがキューの行をロックしていなかった**: `create_batch` はキューの行の下で鎖を読む
   ので、同時に先頭を終えると、終えた列車の上に子が残りえた（先頭の無い深さ 2）。着地の詰め直しとも競合した。`end_batch_chain` と
   `batch_landed_chain` は、自分のバッチの行を更新した直後（`dequeue_mr_decided` と同じ「バッチ → キュー」の順。逆順だとデッドロック
   する）にキューの行を `FOR UPDATE` で取り、その後に鎖を読む。
3. **（db MEDIUM）着地は先頭だけ、を DB でも**: `begin_landing` は `depth = 1` のときだけ `landing` にする。
4. **（rust MEDIUM）着地の後の失敗しうる読み取り**: ref を消すかどうかを、着地のトランザクションが返す残りの数
   （`batch_landed_chain` の `Some(n)`）で決める。着地の後に別の問い合わせをしない。
5. **（rust MEDIUM）持ち主の無い列車を指す ref**: 鎖が切れて ref を戻すときは、今の値を読んでから（タイムアウトつき）それを旧値に
   compare-and-swap する。
6. **（rust LOW）後ろのバッチの MR の事実**: 後ろのバッチも先頭の `poll` と同じに、閉じた・新しい head・消えた MR を見て、そのバッチ
   （と後ろ）を捨て、その MR は今までどおり外す（`a_new_head_in_a_speculative_train_drops_it_and_leaves_the_queue`）。
7. **（rust LOW）期限**: 前へ進んで先頭になったバッチの期限は、その時から同じ長さで数え直す（上の既知の限界の 1 つ目はこれで
   先頭になるまでの待ちについては解消。`forge doctor` が後ろのバッチを期限で数えることは残る）。
8. 残したもの（rust MEDIUM）: 投機の組み立てで先頭の MR が断られると、先頭が決まるまで、見るたびに列車の 1 段（merge-tree 1 回）を
   繰り返す。見回りの周期（既定 60 s、または起床）ごとに 1 回で上限があるので、覚えておく仕組みは入れていない。

## 追記（2026-11-28、計画 18 S10 の実装）— (d) 5 起動のない着地の区間: 作ったもの、決めたこと、既知の限界

上の Decision の文は変えない。新しいエラーコード・キー・マイグレーションは無い。MR ごとの監査の行とイベントは、中身も順も変えない。

### 作ったもの

1. **source の先端は 1 回で**: マージのロックの下で、バッチの全メンバーの source ブランチを `for-each-ref` 1 回で読む
   （forge-git `Git::read_named_refs`。名前と完全に一致する ref だけを返し、名前の下の ref は除く。名前が無ければ git を起動しない。
   タイムアウトはロックの残りで切る）。今までのメンバーごとの `read_ref` はやめた。
2. **承認の git の事実は組み立てで先に読む**: forge-review の `ReviewGit::facts` を、git の半分 `reads`（変更パス・範囲の
   コミット・CODEOWNERS のファイル。(head, tip) の sha だけの関数）と DB の半分 `facts_from`（範囲の push した人、CODEOWNERS の
   持ち主の解決と照合、セレクタの単位）に分けた。`facts` は今までどおり両方を順に呼ぶ（他の呼び出し元は変わらない）。キューの
   worker は、バッチを作った直後（ロックの外、`REQUEST_BUDGET` の 20 s 以内）に、各メンバーの head と**バッチの土台**の組で
   `reads` を読み、worker のメモリに置く（バッチ → エントリー。最大 64 バッチ、古いものから忘れる）。
3. **ロックの下で sha の一致で確かめる**: 着地は、置いた読みを、head = エントリーの head かつ tip = バッチの土台のときだけ使う。
   target の先端 = 土台はロックの下で先に確かめ、source の先端 = head は 1 の読みで決定が確かめる（`check_head`、違えば
   `FG-REVIEW-0008 reason=source_moved` でそのメンバーが外れる）。DB の半分は毎回ロックの下で読む（承認・持ち主・単位は組み立ての
   後に変わりうるため）。置いた読みが無い（別の worker が作った、再起動した、読みが時間切れか git の失敗だった）ときは、今までどおり
   ロックの下で読み、`forge.merge_queue.approval_facts_read`（info）を出す。
4. **決定は 1 つのトランザクション**: forge-db `queue_merge_mrs_decided`。各メンバーを `queue_merge_mr_decided` と同じ手順
   （自分の事実とゲート、`decide`、`prepared` の試みの書き込み）で、列車の順に、1 つの `ChangeTx` の中で決める。`decide` は
   メンバーごとに今までどおり `decide_queued`（依頼した人の principal を作り直して `authorize()`、自動マージは `prove_auto`）。
   最初の断り・失敗で全部を戻し、そのメンバーの番号を返す（`BatchRefusal`）。
5. **記録は 1 つのトランザクション**: forge-db `finish_merges`。各マージを `finish_merge` と同じ手順（`executing → applied`、
   MR を `merged`、push_commits の行、`merge_request.merged` と `repository.pushed` の監査の行とイベント、マージ id での重複排除）で、
   列車の順に 1 つの `ChangeTx` の中で書き、COMMIT は 1 回。監査の行とイベントは**それぞれの依頼した人の文脈**（actor、credential、
   request id）で書く — `ChangeTx::commit_each`（記録ごとに文脈を持つ。重複が 1 つでもあれば全部を戻す）と `Settle::CommitEach`。
   各記録の git の読み（コミットの一覧、範囲の push した人）はトランザクションの前にメンバーごとに読む（今までどおり）。
6. 試験（forge-review、実 git + 開発 DB、`tests/queue_batched/`）: `a_batched_landing_writes_the_same_audit_rows_and_events_as_per_member`
   （golden: メンバーごとに列車の順で `merge_request.merged` → `repository.pushed` の行、依頼した人の actor・credential・request id、
   `target_sha` / `merged_sha` の鎖、イベントの重複排除のキー。ロックの下の git の起動がメンバー数 × 3 + 4 以下）、
   `a_source_moved_after_build_is_refused_under_the_lock`（組み立ての後に git でだけ source が動く → ロックの下で `source_moved`、
   何も書かれず、残りが次のバッチで着地）、`one_refusal_in_a_batched_decision_rebuilds_without_it`（3 件の真ん中の依頼の
   セッションを失効 → `credential_revoked` で外れ、前のメンバーの試みも残らない、2 件で作り直して着地）。forge-git
   `named_refs_are_read_in_one_call_and_only_by_their_exact_names`。既存のマージキューの試験と S9 の投機の試験は**変えずに**通す。

### 決めたこと（利用者の常設の指示「推奨で進めて」による）

- **先に読むのは git の半分だけ**。DB の半分まで組み立てで読むと、組み立てから着地までの間（CI では分単位）の承認の取り消し・
  持ち主の無効化・規則の変更を見落とす。git の答えは sha が同じなら変わらないので、sha の一致だけで再検証できる。
- **置き場所は worker のメモリ**。新しい表もキーも要らない（計画: マイグレーションなし）。置いた読みが無ければ今までの読み方に
  戻るだけで、正しさは変わらない（遅くなるだけで、ログに出る）。時間切れや git の失敗を含む読みは置かない（もう一度読めば答えが
  変わりうる）。
- **断りは何も残さない**。1 つのトランザクションなので、前のメンバーの `prepared` の試みも書かれない（今までは書いてから
  `superseded`、`queue_rebuild` にしていた）。バッチは今までどおり `entry_refused` で捨て、断られたメンバーを外し、残りを順に戻す。
- **列車の鎖の確かめ（`train_mismatch`）は決定の前に**。決定と同じ材料（エントリーの `train_sha`）だけで分かるので、書く前に止める。
- **記録の失敗**: 1 つのトランザクションなので、失敗すれば全部が記録されない（target はもう動いている）。バッチは `landing` のまま
  残り、今までどおり回復（`recover`）が git を見て全部を記録する。MR が開いていないなど、1 件だけの答えは今までどおりその 1 件の
  エラーで、他のメンバーは記録して `merged` にする（最初のエラーを返し、他は `telemetry::log_failure`）。

### 既知の限界

- ロックの下に残る git の起動: target の先端 1、source の先端 1、compare-and-swap 1、着地したパス 1、メンバーごとに記録のコミットの
  一覧（`cat-file` + `rev-list`）と範囲の push した人（`rev-list`、merge commit / squash のときだけ）。記録の 2〜3 回はコミットの一覧が
  他の ref に依存する（push と同じ「新しいコミット」の意味）ので、組み立てでは先に読めない。計画の「起動のない区間」はこの分だけ
  届いていない。
- 置いた読みは worker ごと。キューの貸し出しが別の worker に移ると、その着地はロックの下で読む（ログ
  `forge.merge_queue.approval_facts_read`）。
- 文書のキューで組み立てを着地と並べる（深さ 2）ことはしていない（S9 の決めたことのとおり、貸し出しが 1 つなので手番が並ばない）。

### レビュー対応（2026-11-28、database・authz-matrix のレビュー。CRITICAL・HIGH は無し）

1. **（db MEDIUM）バッチのトランザクションの期限**: 決定と記録のバッチは `ChangeTx::begin_for_batch` で始める。期限は 30 s +
   メンバーごとに 1 s、最大 90 s（マージのロックの保持 180 s の内側）。
2. **（db MEDIUM）1 件の悪い記録が全員を止める**: `finish_merges` のトランザクションが失敗したら、その失敗を 1 回ログに出し
   （`operation=merge_queue_record_batch`）、各マージを S10 の前と同じに 1 件ずつ `finish_merge` で記録する。悪い 1 件だけが
   自分のエラーになり、他は記録されて `merged` になる（回復も同じ道を通る）。
3. **（authz MEDIUM）事実を読んだ MR と、ロックした MR が同じであること**: 決定の中で、ゲートが読んだ MR の `source_branch` と
   `author` が、source の先端と承認の事実を読んだときの MR と違えば、そのメンバーを `FG-REVIEW-0008 reason=source_moved` で断る。
4. 残したもの: source ブランチが無いとき `check_head` が先端を確かめないこと（S10 の前からの挙動、authz LOW）、重複排除の
   エラーがどのメンバーかを言わないこと（db LOW）、降格・自動マージの停止・置いた読みの sha の不一致の個別の試験（authz LOW）。
