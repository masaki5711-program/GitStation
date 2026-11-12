# ADR-0030: マージキュー — ターゲットごとの列、まとめて試すマージ結果、失敗の二分

**Date**: 2026-11-12
**Status**: proposed（accepted は C8 `feat/git-merge-queue` の `database-reviewer`・`git-protocol-reviewer` のレビューで CRITICAL / HIGH が無いとき）
**Deciders**: Masaki Fujikawa（ユーザー、2026-11-10「推奨で進めて」: 計画 14 §9 の決定 5「バッチは 4 まで、失敗で二分」）、Claude

## Context

今のマージ（ADR-0024 §4、M3 R4）は MR ごとに 1 回で、必須チェックは **その MR の head を今の target の先端にマージした結果**（`refs/merge-requests/<iid>/merge`、C7 の Amendment 2026-11-11）で判定する。target が進むと結果は古くなり（`required_check_stale`、FG-CI-0013）、新しいマージ結果のパイプラインが終わるまで待つ。MR が多いと、

- 1 本マージするたびに他の MR のチェックが全部古くなり、パイプラインを待ち直す（待ちが MR の数の 2 乗で増える）。
- 2 本が別々には緑でも、両方が入った `main` は赤になりうる（意味の衝突）。C7 は「今の target の先端」に対して 1 本ずつしか見ない。

計画 04 §8（2026-10-26）でマージキューを M4 に前倒しし、計画 14 §9 の決定 5 で「バッチは 4 まで、失敗で二分」と決めた。C9 の自動マージ（`merge.auto = full`）はこのキューに入れることでマージする。今のマージの経路で確かめたこと:

- マージのジョブ（`forge-review` の `Merger::execute`、`forge serve` が動かす）は、リポジトリごとの助言ロック `merge:<repo>` の下で、要求者の主体を DB から作り直して `authorize(MergeRequestMerge)` と `evaluate_merge()` をやり直し（tx1a / tx1b）、`prepared → executing` のフェンス、`update-ref` の CAS、tx2（`finish_merge`: `applied`、MR を `merged`、監査、`merge_request.merged` と `repository.pushed` のイベント）。照合（reconcile）は `executing` の試行を git に聞いて決める（target が `new_sha` かその子孫なら `applied`）。
- 必須チェックは `forge_review::merges::CheckRuns` の trait で読み、`ci/pipeline` だけがマージ結果で報告される名前（`forge_core::merge_results::reported_on_merge_results`）。
- パイプラインの一意キーは (project, source, ref, sha, MR)。`ci/pipeline` は worker（`ci_outputs`）がパイプラインの状態から書く。保護された push の外にはシークレットを出さない（ADR-0029）。
- 隠す ref は `refs/merge-requests/` と `refs/pipelines/`（`forge_git::hidden_refs`）。クライアントの push は `refs/heads/`・`refs/tags/` の外を `ReservedRefNamespace` で断る。

## Decision

1. **ターゲットのブランチごとに 1 つの列**（`forge.merge_queues`、(project, target_branch) で一意、最初に入れたときに作る）。列には止めた印（`paused`）、今のバッチの上限（`batch_limit`、1〜4）、worker の貸し出し（`lease_owner`・`lease_until`）を持つ。
2. **入れる = 「準備ができたらマージ」**（`POST …/merge-requests/{iid}/merge-queue`、`{sha, method?}`）。判定は **マージそのものと同じ**（Action `MergeQueueEnqueue` = `MergeRequestMerge` の規則: 人だけ、target の push の役割、トークンは `merge_execute`）。入れるときに `evaluate_merge()` を、`ci/pipeline` だけを「列が試す」とみなして行う（承認・下書き・衝突・議論・作業項目・外部の必須チェックはその時点で揃っていること。揃っていなければ FG-REVIEW-0007 と理由）。すでに列にある・MR が開いていないは **FG-CI-0020** MergeQueueEntryRefused（`reason=already_queued` など）、マージの試行が生きていれば FG-REVIEW-0011。コミットのメッセージは入れたときに凍結する（ADR-0024 の要求と同じ）。C9 の自動マージの主体はこの経路を自分の Action の行で通る（C9 で足す）。
3. **バッチと列車のコミット**: worker は列の先頭から最大 `batch_limit`（既定 4）件を取り、**今の target の先端の上に順に**マージした列車を作る。各 MR の段は普通のマージと同じ形（マージコミット: 親は前の段と head、作者・コミッターは入れた人。squash: 親は前の段、作者は MR の作者。メッセージは凍結したもの）。git がマージし（`merge-tree --write-tree` → `commit-tree`、契約 5）、forge は木を作らない。衝突する MR は列から外し（FG-REVIEW-0006 `reason=conflict`）、残りで作り直す。列車の先端を隠した ref **`refs/merge-queue/<queue_id>/train`** に向ける（バッチごとに上書き。`hidden_refs` の 3 つ目の接頭辞: 広告・want の確認・`deepen-not` から外し、push は既存の `ReservedRefNamespace` で断る）。
4. **1 本で走るもの**（バッチに入れない、追い越しもしない — 順は保つ）:
   - diff（target との merge-base から head）が **マイグレーション**（`migrations` / `migrate` ディレクトリの SQL、`forge_core::risk::is_migration_path`、forge/risk の規則と同じ）に触れる MR。
   - head の **`forge/risk` が `stop`**（その実行の結論が `failure`）の MR。C9 はこれに頼って自動マージを止める。人が入れたときは 1 本で試し、人の判断としてマージする。
   - 方法が `fast_forward` の MR（列車の上では早送りにならない。head が target の先端の子孫のときだけ、列車 = head）。
   バッチの組み立ては先頭から見て、1 本で走るものに当たったらそこで切る（それが先頭なら 1 本のバッチ）。理由は entry の `isolate`（`migration` / `risk_stop` / `fast_forward`）に残す。
5. **試す**: 列車の先端で **パイプラインを 1 本**（`ci_pipelines.source = 'merge_queue'`、`ref` = target のブランチ、`sha` = 列車の先端、MR なし、保護なし = シークレットなし）。`.forge-ci.yml` のジョブの選び方は MR のパイプラインと同じ（`Event::MergeRequest`）。判定はそのパイプラインの状態: `succeeded` / `skipped` → 緑、`failed` / `invalid` / `error` / `canceled` → 赤、作成のジョブが終わっても行が無い（`.forge-ci.yml` が無い）→ 「パイプラインなし」（緑として着地の判定へ。`ci/pipeline` が必須なら着地で `required_check_missing` になる）。
6. **緑なら着地**（target を試したコミットへ早送り）:
   1. リポジトリのマージのロック（普通のマージと同じ `merge:<repo>`）を取り、target の先端がバッチの土台のままか確かめる。
   2. MR ごとに 1 つのトランザクション（`queue_merge_mr_decided`）で、入れた人の主体を DB から作り直し（トークンは使えること。**Web のセッションは失効していないこと — 時間切れは許す**: 「準備ができたら」は長く待つため。明示のログアウト・取り消し・アカウントの無効化では止まる）、`authorize(MergeRequestMerge)` と `evaluate_merge()` をやり直す。必須チェック `ci/pipeline` は **列車のパイプラインの状態**（`ci_outputs` と同じ写像）で数え、他の名前は head の実行（C7 の規則）。通れば試行を **`prepared`**（`expected_old` = 前の段、`new_sha` = その段）で書き、entry に `merge_id` を書く。ジョブは積まない（試していないコミットを作る普通のマージに拾わせない）。
   3. 1 件でも断られたら、その MR を理由とコードつきで外し、他の試行を `superseded`（`queue_rebuild`）にしてバッチを捨て、残りで作り直す（列車は変わるので試し直す）。
   4. 全件を `executing` にフェンスし、**`update-ref` の CAS 1 回**（target: 土台 → 列車の先端）。負けたら ref を読み直し、列車の先端なら記録へ、別の値なら「target が動いた」で作り直し。
   5. MR ごとに順に `finish_merge`（普通のマージと同じ tx2: `applied`、MR を `merged`、監査、イベント、push の記録は前の段からその段まで）。entry を `merged`、バッチを `landed`、`batch_limit` を 4 に戻す。元のブランチの削除は列では行わない（常に残す。M4 の後で決める）。
7. **赤なら二分**: バッチが 2 件以上なら `batch_limit = ceil(n/2)` にして全件を列に戻す（順はそのまま）→ 次は前半だけで試す。前半が緑なら着地し、後半を次に試す。1 件のバッチが赤なら、その MR が原因: **FG-CI-0018** MergeQueueChecksFailed（info）で外す。これで「4 件中 1 件が悪い」は 3 回（4→2→1、後半はもう 1 回）、「全部悪い」は 4 件それぞれが単独で赤になって外れる。緑の着地で上限を 4 に戻す。
8. **target が他から動いた**（列車を試している間の直接の push、普通のマージ、別の列）: 次に見たとき（またはロックの下の着地）に先端が土台と違えばバッチを捨て（`target_moved`）、全件を列に戻して今の先端で作り直す。試行の数は増やさない。
9. **止める**（`POST …/merge-queue/pause`・`…/resume`、`{target}`）: 止めた列は新しいバッチを作らず、試し中のバッチも **着地しない**（緊急停止。再開した後に、先端が同じならそのまま着地する）。入れることはできる。止めるのは Developer 以上（Action `MergeQueuePause`）、再開は Maintainer 以上（`MergeQueueManage`）。どちらも人だけ、監査の行（`merge_queue.paused` / `.resumed`）。
10. **外す**（`DELETE …/merge-requests/{iid}/merge-queue`）: マージできる人（Action `MergeQueueDequeue`、`MergeQueueEnqueue` と同じ規則）。試し中のバッチの MR を外すと、次に見たときにバッチを捨てて作り直す。監査の行（`merge_queue.dequeued`）。入れる・外すの監査は `merge_queue.enqueued` / `.dequeued`。worker が外すときは監査ではなくログ（`forge.merge_queue.entry_removed`、理由とコード）と entry の `reason` / `error_code`。
11. **読む**（`GET …/merge-queue?target=`、`GET …/merge-requests/{iid}/merge-queue`）: `MergeQueueRead` = `RepoBrowse` と同じ（エージェントの相手も読める: メタデータだけ）。
12. **状態は DB に**: `merge_queue_entries`（`queued` → `testing` → `merged` / `removed`、`testing` → `queued` に戻る）、`merge_queue_batches`（`testing` → `landing` → `landed`、`testing` → `failed` / `abandoned`、`landing` → `abandoned`）。列ごとに生きたバッチは 1 つ（部分一意索引）。MR ごとに生きた entry は 1 つ。
13. **worker**: `forge serve` の中のタスク（マージの ref の書き込みは閲覧のキャッシュと同じプロセス、ADR-0024 R4 と同じ理由）が数秒ごとに `claim`: `next_run_at` の来た、貸し出しの切れた列を `FOR UPDATE SKIP LOCKED` で 1 つ取り、`lease_until` を書いて手放してから git を動かす。2 つの worker は同じ列を同時に持たない。1 回の手番は時間切れで囲む。
14. **時間切れと詰まりの回収**: バッチには期限（既定 2 時間）。過ぎたら **FG-CI-0019** MergeQueueStalled（alert、runbook `merge-queue`）を 1 回ログに出してバッチを捨て、MR を列に戻して試行の数を 1 増やす。3 回目で外す（同じコード）。worker が着地の途中で落ちたら（バッチが `landing` のまま貸し出しが切れる）、次の worker がロックの下で git を見て決める: target が列車の先端なら残りを記録、土台のままなら試行を `superseded` にして作り直し、他の値なら作り直し。ADR-0024 の照合（`executing` を 630 秒後に git で決める）は列車の段も `new_sha` の子孫として `applied` にできる。
15. **エラーコード**（計画 14 §4.3 の CI-next。C6 の CI-g・h に 0016・0017 を残した）: FG-CI-0018 MergeQueueChecksFailed（info）、FG-CI-0019 MergeQueueStalled（alert）、FG-CI-0020 MergeQueueEntryRefused（info）。外すときの他の理由は既存のコード（FG-REVIEW-0006 衝突、FG-REVIEW-0007 マージの条件、FG-REVIEW-0008 head の変化、FG-AUTH-0003 権限）。すべての失敗のログに `request_id`（バッチごとの要求 ID。entry は入れたときの要求 ID も持つ）と `error_code`。

## Alternatives Considered

### 列車ではなく、先頭の 1 件ずつを普通のマージに渡す
- **Pros**: 新しい着地の経路が要らない。
- **Cons**: 試したコミットと入るコミットが違う（普通のマージはジョブの中でコミットを作り直す）。1 件ずつなので待ちは減らない。
- **Why not**: 「試したものを入れる」がキューの目的。

### 列車の段ごとに普通のマージの試行（`queued` とジョブ）を作る
- **Pros**: 既存のジョブ・照合をそのまま使える。
- **Cons**: ジョブが列より先にロックを取ると、試していない新しいコミットでマージしてしまう。
- **Why not**: 試行は列が `prepared` で直接書き、ジョブを積まない。照合は `executing` の後始末にだけ使える。

### 失敗したら 1 件ずつに分ける（二分しない）
- **Pros**: 単純。
- **Cons**: 4 件中 1 件が悪いとき 4 回試す。
- **Why not**: 計画 14 §9 の決定 5。

### パイプラインの source に `merge_request` を流用する
- **Pros**: マイグレーションが要らない。
- **Cons**: MR が 1 つに決まらない（列車は複数の MR）、C7 の一意キー・掃除が MR を前提にする。
- **Why not**: `merge_queue` を足す（`ref` は target、MR なし、保護なし）。

## Consequences

### Positive
- MR が多くても、緑の列車が 1 回の CAS で入る。試したコミットがそのまま target になる。
- 意味の衝突（別々には緑、一緒だと赤）が target に入る前に見つかり、二分で原因の MR だけが外れる。
- C9 の自動マージは、列に入れることと `forge/risk` の `stop` を 1 本で走らせることに頼れる。

### Negative
- 着地の経路（`queue_merge_mr_decided` と列の CAS）が普通のマージの経路と並ぶ。判定は同じ関数（`authorize()`・`evaluate_merge()`・`finish_merge`）を使い、違うのは必須チェック `ci/pipeline` の出所と試行の書き方だけ。
- 元のブランチの削除は列では行わない。
- 列の画面は最小（MR の画面のボタンと位置、ターゲットごとの一覧）。

### Risks
- **worker が着地の途中で落ちる**: §14。ロックと CAS で target は 2 回動かない。
- **パイプラインが終わらない**: 期限と FG-CI-0019（alert）。runbook `merge-queue`。
- **セッションの時間切れで止まる**: 時間切れは許し、失効だけを断る（§6.2）。トークンは従来どおり失効・期限で断る。
- **列車のコミットが gc で消える**: 列ごとの隠した ref が今の列車を指す。過去の列車は target に入ったか、捨てられて不要。
