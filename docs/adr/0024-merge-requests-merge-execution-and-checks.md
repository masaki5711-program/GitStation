# ADR-0024: MR の持ち方、マージの実行、マージの条件と Checks API

**Date**: 2026-11-02
**Status**: accepted（計画 11 の承認（2026-11-02、§3 と §10 の推奨をすべて採用）に基づく。ADR-0006・ADR-0007・ADR-0008・ADR-0016・ADR-0017 を補う。代理実行は ADR-0025（MR S1）で決める）
**Deciders**: Masaki Fujikawa（ユーザー、計画 11 の承認）、Claude

## Context

- M3 の R1 の条件は「forge 自身の開発を MR で回せる」こと（計画 01 §8、計画 11 §0.5・§9）。MR・差分・版・承認・マージの条件・マージの実行・
  Checks API はまだコードにない（`forge-review`・`forge-checks` のクレートもない。計画 11 §0.4）。
- 契約 5（git は実行する。引数のベクタ、検証したパス、`--`、時間切れ）と、既存の部品がある:
  `forge-git::command`（唯一の git の起動口。オプションは `&'static str`、revision は `--end-of-options` の後、パスは `--` の後、環境を消して作り直す。
  `command.rs`）、`forge-git::version::MINIMUM` = 2.38（`merge-tree --write-tree` が使える。ADR-0006）、M2 の `browse::diff`（差分の上限と `truncated`）。
- 保護 ref の判定は push の経路（Smart HTTP の事前の検査 + git の `receive.denyNonFastForwards`。ADR-0008 の Amendment 段階 9 MR4）にしかない。
  **マージは forge が自分で `update-ref` するので、push の経路の判定を一切通らない。** マージの経路で保護の判定と push の副作用を出さなければ、
  保護ブランチを迂回する道になる（計画 11 §8）。
- `refs/merge-requests/*` への push は既に `authorize()` が FG-GIT-0010 ReservedRefNamespace で拒む（`forge-auth` の `authorize.rs`、行
  `reserved_ref_namespace_is_rejected_even_for_admins`）。一方 forge は `transfer.hideRefs` / `uploadpack.hideRefs` を **設定していない**ので、
  upload-pack の広告と、`want` の確認（ADR-0007 の Amendment、`rev-list … --not --all`）はすべての ref を見る。
- ADR-0017（accepted）は、マージ可否を `authorize()` と `evaluate_merge()` の 2 つの純粋な関数で決め、承認を head の SHA に結び、エージェントと宣言された
  トークンとサービスアカウントにマージ・承認をさせないと決めている。M3 はその最初の実装になる（自動修正・AI の承認・マージキュー・予約・kanflow の
  オーナーの能力は M4 以降。計画 11 §0.3-4）。
- M3 には CI がない（M4）。必須のチェックの提供者は、試験サーバーのゲートのスクリプト（サービスアカウントで `ci/gates`）と kanflow だけ（計画 11 §0.5、Q10）。
- Claude が新しいセッションで、ログと `request_id` だけから「なぜこの MR はマージできないか」「マージのどの段で落ちたか」「ref は動いたか、DB は追いついたか」を
  辿れなければならない。

## Decision

### 1. MR と版のデータ — SHA だけを持ち、差分は毎回 git で作る（計画 11 §3.1 案 A）

- **`merge_requests`**（R1、追記のマイグレーション）: `project_id`、プロジェクトごとの `iid`（一意 `(project_id, iid)`。採番は原子的で、同時の作成でも重複しない。
  方法 — プロジェクトの行のロックか採番の列か — は R1 で決め、同時の採番のテストで固定する）、`source_branch` / `target_branch`（どちらも `refs/heads/` の
  `RefName`。M3 はフォークがないので同じリポジトリだけ）、`author_id`、`title`、`description`、`state`（`open` / `merged` / `closed`）、`draft`、`head_sha`、
  `base_sha`、`merge_status`（§3）、マージ・クローズの主体と時刻。状態遷移は DB の CHECK と、アプリケーションの状態機械の両方で守る（§6）。
- **ブランチの入力の検査**（作成と target の変更の両方）: `source_branch` と `target_branch` はどちらも `refs/heads/*` だけ（タグ・`refs/merge-requests/*`・
  その他の名前空間は拒む）。`source_branch == target_branch` は入力の検査で拒み、`authorize()` の arm も `reason=source_equals_target` で deny する
  （二重の守り。§12 の `Resource::MergeRequest` が両方の名前を持つ）。
- **開いている MR の重複を許さない**: 同じ `(project_id, source_branch, target_branch)` で `state = 'open'` の行は 1 つまで（部分一意索引）。違反は
  **FG-REVIEW-0001** DuplicateOpenMergeRequest（409）。
- **`merge_request_versions`**: push ごとの `(version_no, head_sha, base_sha（その時の target の先端）, merge_base_sha, pushed_by, created_at)`。
  `pushed_by` は版を作るきっかけの push の actor で、**承認を数えないコミッターの集合は `pushed_by` だけでは決めない**（§8 の 3）。
  版の差分は `merge_base_sha..head_sha`、版どうしの比較は `head_sha(v1)..head_sha(v2)`。差分は M2 の `browse::diff` で作り、上限と `truncated` も M2 と同じ。
  差分の行を DB にも置き場にも保存しない。
- 版は `repository.pushed` の購読者（`forge-review`）が足す。同じトランザクションで隠した ref を進め（§2）、§8 の承認の扱いと
  `merge_request.pushed` を出す（購読者は git を呼べないので、実際には「MR を確かめ直すジョブ」を積み、ジョブが版・ref・イベントを書く。計画 11 §11 の 3）。
- **ブランチが消えても MR とリンクは残る**。ソースブランチの削除は `merge_request.source_branch_deleted` を出し（開いていても終端でも）、開いている MR は
  §7 の止める理由 `source_branch_missing` でマージできなくなる（隠した ref があるので差分と履歴は読める）。

### 2. 隠した ref — 版ごとの `refs/merge-requests/<iid>/v<n>` と `refs/merge-requests/<iid>/head`

- **版ごとに 1 つの ref**: 版 `n` を足すとき、forge は `refs/merge-requests/<iid>/v<n>` を作り（古い値はゼロ。作成だけで、上書きしない）、
  `refs/merge-requests/<iid>/head` を新しい `head_sha` へ進める（比較して交換）。2 つは同じ `update-ref -z --stdin` の 1 回の transaction（`start` …
  `prepare` / `commit`）で書く。`head` だけでは、force push で版 2 が版 1 を置き換えたとき版 1 のコミットがどの ref からも届かなくなり gc で消えるが、
  `v<n>` が残るので **force push でもレビューした版のコミットは失われない**（承認した SHA と版どうしの比較が後から読める）。
- **版の行が先、ref は後**（再レビュー）: ジョブはまず `merge_request_versions` の行（`version_no` の採番を含む）を書いてコミットし、`v<n>` の `n` は
  **コミットした行から導く**（ref を先に作ってから番号を決めない）。同時の 2 つのジョブが同じ `n` を ref に書く競合は、採番が DB の一意制約で 1 つに
  決まることで起きない。
- **ref の更新の再試行**: 版を書くジョブの再試行で、`v<n>` が既に `head_sha` を指す、`head` が既に `head_sha` を指す、のどちらも **成功として扱う**
  （更新の前に ref を読み、目的の値なら何もしない。CAS の負けとして失敗にしない）。`v<n>` が別の値を指していて、**その値を主張する DB の行が無い**
  （行のコミットの前に止まったジョブの残り、ロールバックした行の ref）なら、ジョブはその ref を **取り戻して上書きしてよい**（`update <ref> <new> <old>` の
  CAS で、読んだ古い値を条件に）。これはプログラムの誤りではないので FG-SYS-0001 にしない（info で `forge.review.version_ref_reclaimed` を 1 回ログ）。
  別の値を DB の行が主張しているときだけ、ref を書かずにその MR の確かめ直しをもう一度積む。
- マージ・クローズの後も残す（履歴と差分のため）。消すのはプロジェクトの purge と、§2 の末尾の管理者の操作（秘密の除去）だけ。`refs/merge-requests/<iid>/`
  の下の他の名前（例: M4 のマージ結果の `merge`）は予約する。
- **upload-pack と receive-pack に見せない**: Smart HTTP の固定の `-c` の組（`smart_http.rs`）の **`UPLOAD_PACK_CONFIG` と `RECEIVE_PACK_CONFIG` の
  両方** に `transfer.hideRefs=refs/merge-requests/` を足す（片方だけでは、receive-pack の広告に隠した ref と ID が出る）。receive-pack は隠した ref の
  更新を git 自身も拒む（二重の守り。一次の拒否は今の FG-GIT-0010）。
- **`want` の確認も同じ ref を除く**（ADR-0007 の Amendment の注意書き「隠す ref を作るときは、この確認も同じ ref を除く」を実行する）。ADR-0007 は
  git ≥ 2.42 の `--exclude-hidden=uploadpack` を例に挙げたが、**MINIMUM は 2.38 なので使わない**。代わりに、`command.rs` に crate 内部の
  **`exclude_glob`**（`--exclude=<glob>` を出す。glob は `&'static str` だけ）を足し、`want_check.rs` の **`--all` を使う 2 つの走査の両方**
  （手順 2 の `rev-list --max-count=1 --stdin --not --all` と、手順 3 の `rev-list --objects --all --no-object-names`）で `--all` の直前に置く
  （`--exclude` は次の `--all` / `--branches` などにだけ効くので、位置をテストで固定する）。glob と hideRefs の値は **1 つの定数**
  （例 `forge_git::HIDDEN_REF_PREFIX = "refs/merge-requests/"`、glob はそこから `"refs/merge-requests/*"` を作る）から導き、2 か所が食い違わないことを
  単体テストで確かめる。これで「削除したブランチのコミットを ID で取り出せない」（`unreferenced_object_cannot_be_fetched_by_id`）が、
  隠した ref だけが指すコミットにも保たれる。2.38 の実際の git で、広告（upload-pack と receive-pack）に出ないこと・`want` で取れないこと（コミットと
  blob の両方）・push が拒まれることを R1 のテストで確かめる（`hidden_merge_request_refs_are_not_advertised_or_fetchable`）。
- 閲覧の API の **ref の一覧と名前の解決** は `refs/merge-requests/*` を出さないことを **確かめた**（`browse/refs.rs` の `load_refs` は
  `for-each-ref … refs/heads/ refs/tags/` だけを読み、`resolve_ref` も `refs/heads/` / `refs/tags/` の名前だけを受ける）。R1 にテストで固定する
  （`browse_ref_list_never_shows_merge_request_refs`）。
- **閲覧の API の SHA の指定は到達可能性を確かめていない**（`browse/refs.rs` の `commit_of` は `cat-file --batch-check` で存在と種類だけを見る）。
  そのため、隠した ref だけが指すコミット、mergeability のジョブが作った参照されないコミット・木、削除したブランチのコミットも、ID を知っていれば
  `?ref=<sha>` で読める。**M3-0 の MR `fix/git-browse-reachability`（R1 の前に必須）** で次のとおり直す:
  - SHA の指定は、そのコミットが **ブランチかタグから届く**（`for-each-ref --contains <sha> --count=1 refs/heads/ refs/tags/` が 1 行を返す）ときだけ
    解決する。MR の画面の文脈（版・差分・版どうしの比較）では、加えて **そのプロジェクトのその MR の版の ref**（`refs/merge-requests/<iid>/`）から届くものを
    許す。届かなければ、存在しない SHA と **同じ応答**（FG-REPO-0023 `reason=ref_not_found`）を返す（存在を漏らさない）。
  - **費用の上限**（再レビュー）: 到達の確認は時間切れつきで走らせ、時間切れは FG-GIT-0002（または空きが無ければ FG-GIT-0004 の busy）で、**届いたことに
    しない**。呼び出し元ごとの流量制限を持つ（閲覧の流量の枠に数える）。結果は `(repo, sha, refs の世代)` を鍵に短く（秒の単位）キャッシュしてよい
    （ref が動けば世代が変わり無効になる）。commit-graph があれば使ってよい（`--contains` が速くなる。無くても正しさは変わらない）。
  - テスト: 時間切れが成功にならない、流量制限が効く、キャッシュが ref の更新で無効になる。P6 は **性能のテスト**（多数のブランチ・深い履歴の合成の
    リポジトリで、到達の確認の時間が閲覧の時間切れの内に収まる）も足す。
  - テスト（実際の git）: 削除したブランチだけが持つコミット、隠した ref だけが指すコミット、`merge-tree` / `commit-tree` が作っただけのコミットの
    どれも、ブランチの文脈では存在しない SHA と同じ応答になる（`unreachable_commit_is_not_browsable_by_id`）。
  - **実装で決めたこと（P6 とそのレビュー、2026-11-02）**: 確認は `rev-list --max-count=1 --stdin --not --branches --tags`（ID は標準入力。
    `for-each-ref --contains` より届かない場合が 4〜5 倍速い）。ID で指定した木・blob、届かないコミット、確認が時間切れ・空きなしで決まらなかったものは
    どれも存在しない ID と同じ `ref_not_found`（本当の理由はログだけ）。呼び出し元ごとの上限は、ID で引くたびに数える（存在しない ID も同じ）
    トークンバケツ（既定 60・250 ms ごとに 1）と同時 2 件で、超えたら FG-GIT-0004（`caller_reach_rate` / `caller_reach`）。キャッシュの世代はリポジトリごと。
  - **既知の残り（受け入れた）**: 応答の **時間** の差は残る。存在しない ID は `cat-file` 1 回で答え、存在するが届かないコミットは到達の確認の walk の後に
    答えるので、時間を測れば両者を見分けうる。呼び出し元ごとの上限が探る速さを抑える。完全に隠すには毎回 walk の最悪の時間まで待たせる必要があり、
    閲覧全体が遅くなるので採らない（`browse/reach.rs` のモジュールの説明にも記載）。
- **版の ref の削除（秘密の除去）**: 誤って push した秘密を MR の版から消すため、管理者の操作 `MergeRequestVersionRefsPurge` を足す（人だけ、
  **`SessionOnly`**: 人間の管理者のセッションだけ。`admin` スコープのトークンも、エージェントのトークン・サービスアカウント・代理も deny。監査の行と
  `request_id` つき。再レビュー）。**前提条件**: 開いている MR では、先に MR をクローズするか、ソースブランチを秘密を含まない形に書き換えて
  （新しい版ができる）から行う。開いたままで今の head を消すと次の版の追加で同じ ref が戻るので、開いている MR で今の head を指す版の ref の削除は
  **FG-REVIEW-0002** PurgePreconditionFailed（409、`reason=purge_requires_closed_or_rewritten`）で断る（権限ではなく状態の拒否。手順は runbook `secret-removal`）。`refs/merge-requests/<iid>/`
  の下の ref を CAS で消し、MR に「版の ref を削除した」ことを残す（以後その MR の古い版の差分は読めない、と表示する）。オブジェクトそのものの除去
  （到達できないオブジェクトの即時の prune）は本番を変えるので、runbook `secret-removal` の **人が承認した手順** として行う。ミラー（O2）と
  バックアップに残る写しの扱いも同じ runbook に書く（漏れとして扱い、秘密の失効が先。ref を消すだけでは足りない）。
- この節の確定に合わせ、R1 で ADR-0007 に Amendment（`want` の確認の除外）を書く。

### 3. マージできるかの判定（C-1）— `merge-tree --write-tree` のジョブ

- ジョブ `merge_request.mergeability`（`forge-review`、dedup の鍵 `mr_mergeability:<mr_id>:<head_sha>:<target_sha>`）を、MR の作成・版の追加・
  target の前進（`repository.pushed` の購読者が、その target を向く開いている MR ごとに積む）で実行する。
- **扇の広がりに上限を置く**: target の 1 回の前進で積むジョブは、その target を向く開いている MR のうち最近更新された順に上限まで（設定
  `review.mergeability_fanout_max`、既定は R2 で決める）。上限を超えた MR は `merge_status = unchecked` にするだけで（§7 の `mergeability_unknown` で
  マージは止まる。成功にしない）、MR の画面の表示・`gstn mr status`・マージの要求のときにその MR のジョブを積む。超えた数はログの
  `forge.review.mergeability_deferred`（`repo_id`、`target`、`deferred`）に 1 回出す。ジョブの同時実行もリポジトリごとの上限を持つ。
- 中身は `git merge-tree --write-tree <target_sha> <head_sha>`（作業ツリーなし、ディスクに作業ツリーを作らない）。終了コード 0 は衝突なし（木の ID）、
  1 は衝突（木の ID と衝突したファイルの一覧）、それ以外は失敗。2.40 からの `--merge-base` は使わない（MINIMUM 2.38）。
- **終了コード 1 を失敗にしない口**: 今の `GitCommand::output` は 0 以外をすべて FG-GIT-0003 にして標準出力を捨てる。`forge-git::command` に
  「**受け付ける終了コード**」を指定する出力の形（例 `output_accepting(&[0, 1])`。結果に `exit_code` を持ち、受け付けたコードでは標準出力を残す。
  それ以外のコードは今までどおり FG-GIT-0003）を crate 内部に足す。使うのは `merge-tree --write-tree`（1 = 衝突）と `merge-base --is-ancestor`
  （1 = 祖先でない）だけ。衝突の一覧は **切り詰めの出力**（`output_truncated` と同じ考え方）で読み、上限を超えても FG-GIT-0006 の失敗にせず
  「衝突あり、一覧は `truncated`」とする（衝突の判定は終了コードで決まるので、一覧が切れても判定は正しい）。**切り詰めで git を止めて終了コードが
  得られないとき**は、出力のあふれそのものを衝突とみなす（衝突のない出力は木の ID の 1 行だけで、上限を超えることがない。再レビュー）。2.38 の実際の git で、衝突なし・衝突あり・
  衝突の一覧が上限を超える（あふれ = 衝突、衝突のない出力が 1 行であること）・祖先である / でない、の各場合の終了コードと出力の形をテストで固定する
  （`merge_tree_exit_codes_on_git_2_38`）。
- `merge_status` は **マージできるかの判定だけ** を表す: `unchecked` / `checking` / `can_be_merged` / `cannot_be_merged` / `check_failed`。
  マージの実行中かどうかは別の状態（§4.2 の `merge_request_merges` の行）で表し、`merge_status` に混ぜない。
  **判定できなかったことを成功にしない**: `unchecked` / `checking` / `check_failed` は §7 の止める理由 `mergeability_unknown`。マージの実行中は
  `merge_in_progress`（§4.2）。失敗の理由は FG-GIT の既存のコード（時間切れは FG-GIT-0002、プロセスの失敗は FG-GIT-0003）で 1 回ログに出し、
  `merge_status = check_failed` と `reason` を残す。
- `merge-tree --write-tree` は判定のたびに参照されない木（とオブジェクト）を作る。§4.2 の `commit-tree` も、照合で `superseded` になった分の参照されない
  コミットを残す。O3 の定期の保守は到達できないオブジェクトを **猶予つきで** prune する（`git gc --prune=<猶予>` を明示。猶予は照合の窓（§4.2）と
  ジョブの再試行の全体より長く、既定 24 時間以上。照合の途中の `new_sha` を消さない）。
- `can_be_merged ⇄ cannot_be_merged` の変化のときだけ `merge_request.conflict_detected` / `conflict_resolved` を出す（判定できない状態との行き来では出さない）。
- この判定は表示と早めの拒否のためのもの。**マージの実行は §4 でロックの下でもう一度 `merge-tree` を計算し、それを正とする。**

### 4. マージの実行（計画 11 §3.2 案 A）

#### 4.1 方式

| 方式 | 木 | 親 | 作るコミット |
|---|---|---|---|
| `merge_commit` | `merge-tree --write-tree target head` の木 | `[target, head]` | `commit-tree` で 1 つ |
| `squash` | 同上 | `[target]` | `commit-tree` で 1 つ（作成者は MR の作成者、コミッターはマージした人） |
| `fast_forward` | — | — | 作らない。`target` が `head` の祖先のときだけ（`merge-base --is-ancestor`）。でなければ §7 の `ff_not_possible` |

- 「rebase して早送り」は作らない（M3 後半に一時的な作業ツリー（案 B）か `git replay` かを決める。計画 11 §10 Q3）。
- プロジェクトの `merge_settings`（R4）が許可する方式と既定（推奨: マージコミットと squash を許可、既定はマージコミット）を持つ。許可されない方式の要求は
  **FG-REVIEW-仮** MergeMethodNotAllowed（409）。
- **どの方式も、新しい target は古い target の子孫になる**（マージコミットと squash は第 1 の親が古い target、早送りは祖先の確認）。これを実行の前に不変条件として
  確かめ、破れていればプログラムの誤りとして FG-SYS-0001（`reason=merge_not_descendant`）で止める。マージの経路は force にも削除にもならない。
- コミットのメッセージ（利用者の入力、上限 64 KiB）は **標準入力** で渡す（`commit-tree -F -`）。`command.rs` のオプションは `&'static str` だけなので、
  利用者の文をオプションの位置に置かない。作成者・コミッターの名前とメールは環境変数（`GIT_AUTHOR_*` / `GIT_COMMITTER_*`）で渡す必要があり、今の
  `command.rs` は `GIT_PROTOCOL` しか呼び出し側の環境を許さないので、**crate 内部の「コミットの ID を渡す」口を足す**（値は DB の利用者の表示名とメール。
  改行・NUL と `<` `>` を含む値は拒む）。**前後の空白を除いて空になる名前は、その利用者の `username`（検証済みで空でない）に置き換える**（git は空の名前を
  拒むので、置き換えずに渡すと FG-GIT-0003 の失敗になる）。置き換えたことは `forge.review.merge_started` に `identity_fallback=true` で残す。
- `commit-tree`（`-p <parent>` を親ごとに、木の ID、`-F -`）、`merge-tree --write-tree <target> <head>`、`merge-base --is-ancestor <a> <b>` の
  **正確な引数の並び** は、R4 で 2.38 の実際の git のテストとともに固定する（`merge_plumbing_argv_on_git_2_38`。revision は `--end-of-options` の後に
  置けるかも同じテストで確かめ、置けないコマンドは ID の形（40 / 64 桁の 16 進）を検査してから渡す）。
- `update-ref` と `commit-tree` にも `-c core.hooksPath=<home>/empty-hooks` を付ける（`reference-transaction` のフックを動かさない。receive-pack と同じ扱い）。
  ref の更新は `git update-ref -z --stdin`（`update <ref> <new> <old>`）で、ref 名と ID を標準入力で渡す。

#### 4.2 ジョブ、ロック、ロックの下の再判定

マージは **ジョブ** で実行する（HTTP の要求の中で git を走らせない。`gstn mr merge --wait` はジョブの完了を待つだけ）。

**マージの行の状態**（`merge_request_merges.state`）: `queued`（ジョブを積んだ）→ `prepared`（オブジェクトを作り、`expected_old` と `new_sha` を確定した）→
`executing`（ref を動かす権利を取った）→ `applied`（終端）。途中から `superseded`（ref は動いていない。終端）か `failed`（人の判断。終端）。
**マージの実行中** は「`queued` / `prepared` / `executing` の行がある」ことで、`merge_status`（§3）とは別に持つ。MR ごとに実行中の行は 1 つまでで、
**部分一意索引**（`merge_request_merges (mr_id) WHERE state IN ('queued', 'prepared', 'executing')`）で守る。違反は **FG-REVIEW-仮**
MergeInProgress（409）。git の処理（`merge-tree` / `commit-tree` / `update-ref`）は、MR の行をロックしたトランザクションの **外** で行う
（行のロックを git の時間だけ持ち続けない。advisory lock は持ったまま）。

1. **入口**（HTTP）: 要求の本文は **利用者が見た `sha`**（必須）と方式とメッセージ。ルートが `authorize(MergeRequestMerge)` で判定し、`sha` が MR の今の
   `head_sha` と違えば **FG-REVIEW-仮** HeadChanged（409、`reason=stale_view`。ADR-0017 §4.0 の `MergeHeadMismatch` はこのコードに含める）で断る。
   `evaluate_merge()`（§7）を一度計算し、止める理由があれば **FG-REVIEW-仮** MergeBlocked（409、`reasons`）を返してジョブを作らない。通れば、
   同じトランザクションで `merge_request_merges` の `queued` の行（`merge_id`、方式、`sha`、メッセージ、`request_id`）とジョブを書き、202 とジョブ ID を返す。
   **ジョブの行と `queued` の行に残す主体は「利用者 ID と資格情報の種類と ID（セッション ID かトークン ID）」だけ** で、スコープ・`is_agent`・アカウントの種類・
   ロールは残さない（§5。古い権限の写しで実行しない）。部分一意索引の違反は FG-REVIEW-仮（MergeInProgress）。
2. **ロック**: ジョブはリポジトリごとの advisory lock `merge:<repo_id>`（セッション単位、待ちに時間切れ。時間切れはジョブの再試行、尽きればデッドレター
   FG-DB-0006）を取る。同じリポジトリのマージは直列になる。
3. **準備の判定のトランザクション**（tx1a）: MR の行を `FOR UPDATE` → `Authorized::reconfirm`（§5: 資格情報 ID から主体を作り直し、同じ Action で
   `authorize()` をもう一度）→ 事実を読み直して `evaluate_merge()` をもう一度 → リポジトリから今のソースの先端と target の先端を読む。
   **ソースの先端が `sha` と違えば FG-REVIEW-仮** HeadChanged（`reason=source_moved`）で止める（行は `superseded`）。コミットして行のロックを放す。
4. **オブジェクトを作る**（トランザクションの外、advisory lock の下）: `merge-tree` / `commit-tree`（オブジェクトを作るだけ。ref は動かない）。
   衝突なら **FG-REVIEW-仮** MergeRequestConflict（行は `superseded`）。CODEOWNERS と設定のファイルの判定に使う変更されたパスも、ここで §9 の方法で読む。
5. **準備の記録のトランザクション**（tx1b）: MR の行を `FOR UPDATE` → reconfirm と `evaluate_merge()` をもう一度（DB だけを読む純粋な判定。git は
   呼ばない。4 で読んだ変更されたパスを事実として渡す）→ MR の `head_sha` と 3 で読んだ target の先端が 4 の計算の前提と同じことを確かめる（違えば
   FG-REVIEW-仮（HeadChanged））→ 行を `queued → prepared`（`expected_old` = target の先端、`new_sha`、`head_sha`）。**この遷移も `WHERE state = 'queued'` の条件つき**で、
   1 行でなければ（照合が先に `superseded` にした）先へ進まない。コミット。
6. **フェンス**（ref を動かす直前）: (a) advisory lock を持つ接続が生きていて、その接続のロックがまだあることを確かめる（同じ接続で
   `pg_locks` の自分の `pid` の `merge:<repo_id>` を読む。失敗・不在なら ref を動かさず、ジョブを再試行に回す。行は `prepared` のまま）。
   (b) トランザクションで行を **`prepared → executing` に比較して交換**（`UPDATE … WHERE state = 'prepared'`、1 行でなければ ref を動かさず止める。
   照合のジョブか別の worker が先に取った）。`executing_at` を記録する。
   (c) `update-ref` を起動する **直前** に `now − executing_at` を照合の余裕と比べ、余裕から `update-ref` の時間切れを引いた値を超えていれば起動しない
   （照合が触りうる時刻に ref を動かさない。行は `executing` のまま照合へ）。`update-ref` の時間切れは照合の余裕より十分に短くする（例: 余裕 10 分に
   対して 30 秒。既定は R4 で決め、`余裕 > 時間切れ × 2` を設定の検査で確かめる）。
   **再試行**（ロックのファイル・ロックの喪失・worker の停止の後）は、advisory lock を取り直してから行を読み直し、**まだ `executing` のときだけ** 7 へ進む
   （照合が `superseded` / `applied` / `failed` にしていれば何もしない）。
7. **ref の更新**: `update-ref -z --stdin` で target を `expected_old → new_sha` に比較して交換する（CAS）。**失敗したときは ref を読み直して分ける**:
   - 今の先端が `expected_old` と違う（判定の後に誰かが target に push した）→ 行を `superseded` にして **FG-REVIEW-仮（HeadChanged）**（409、`reason=target_moved`、
     `expected`、`actual`）。alert にしない。
   - git がロックのファイル（`<ref>.lock` が既にある）で失敗した → 再試行できる失敗として行を `executing` のまま残し、ジョブを再試行に回す
     （再試行は照合と同じ手順で ref を読んでから決める）。warn で 1 回ログ（`forge.review.merge_retry`、`reason=ref_locked`。尽きれば FG-DB-0006）。
   - それ以外（先端は `expected_old` のままなのに更新できない、ref が読めない、時間切れ）→ **FG-REVIEW-仮** MergeExecutionFailed
     （alert、`reason=update_ref_failed`）。行は `executing` のまま残し、照合に渡す。
8. ref が動いた直後に、ログの行 `forge.review.merge_applied`（`request_id`、`job_id`、`merge_id`、`mr_id`、`project_id`、`repo_id`、`method`、`target`、
   `expected_old`、`new_sha`、`head_sha`）を **先に** 出す（ADR-0008 の `forge.git.push_applied` と同じ考え方）。
9. **記録のトランザクション**（tx2）: 行を `executing → applied`（比較して交換）、MR を `merged`、監査の行、push の記録（`repository.pushed`、actor は要求者）と
   `merge_request.merged` を **同じトランザクション** で書く。update-ref は pre-receive も post-receive も通らないので、push の副作用はこの経路が自分で出す。
   **`repository.pushed` と `merge_request.merged` の Outbox の dedup の鍵は `merge_id` で作る**（`merge_pushed:<merge_id>`、`merge_request_merged:<merge_id>`）。
   tx2 の再試行や照合のやり直しで 2 回書いても、イベントは 1 回だけになる。
10. ロックを放す。ソースブランチの削除（要求に含まれるとき）は記録の後に別の手順で行い、要求者の `RepoPush`（削除）で判定し、CAS で消す
   （保護されたソースは消さず、MR にその旨を残す）。**削除が成功したら、その削除の push の記録（`repository.pushed`、ref の `old → 0`、actor は要求者）と
   `merge_request.source_branch_deleted` を、マージの tx2 とは別の自分のトランザクションで書く**（ミラー（O2）と連携が削除を取りこぼさない。dedup の鍵は
   `merge_source_deleted:<merge_id>`）。ref が消えた直後、記録を書く **前に** ログの行 `forge.review.merge_source_deleted_applied`（`request_id`、`merge_id`、
   `ref`、`old`）を出す（8 と同じ考え方）。削除の失敗はマージを取り消さない（warn で 1 回、`ref` とエラーコード（FG-GIT-0002 / 0003、記録の失敗は
   FG-DB-0001）つきでログ）。

**照合（ref と DB が食い違ったとき）**: tx2 の失敗は FG-REVIEW-仮（MergeExecutionFailed）（`reason=record_failed`、alert）。同じジョブの再試行と照合のジョブ
`merge_request.reconcile`（`forge doctor` の `jobs.merge` も数える）が、advisory lock の下で行の状態ごとに決定的に片付ける:

- **`queued` / `prepared` のまま**（10 分を超えた行）: 6 の比較して交換を通っていないので、この行の worker は ref を動かしていない。`superseded` にする
  （ジョブがまだ生きていれば、6 (b) の交換に負けて止まる）。
- **`executing` のまま**（`executing_at` から `update-ref` の時間切れ + 余裕（既定 10 分）を超えた行。それより前は、元の worker の `update-ref` がまだ
  動きうるので触らない）: **git に聞く** — target を読み、先端が `new_sha`、または `new_sha` を祖先に持つ → tx2 をやり直して `applied`。先端が
  `expected_old` のまま → ref は動いていないので `superseded`。どちらでもない（`new_sha` が届かない）→ `failed` にして FG-REVIEW-仮（MergeExecutionFailed）
  （`reason=ref_state_unknown`）で人に渡す（runbook `merge-execution`）。
- 祖先の確認は `merge-base --is-ancestor`（§3 の受け付ける終了コード。1 = 祖先でない）。

**マージの実行中**（実行中の行がある間）、MR を変える操作（クローズ、target の変更、ドラフトへの切り替え）は MR の行のロックの下で **FG-REVIEW-仮（MergeInProgress）** で断る。
版の追加は事実の記録なので断らない（マージは判定した `head_sha` を使い、5 の確認でずれを拾う）。

#### 4.3 保護 ref の判定をマージの経路でも行う

- 入口と tx1a / tx1b で、push と同じ `ProtectionLoader`（ADR-0008 の Amendment 段階 9 MR2 のスナップショット）から target の規則を読み、
  `Resource::MergeRequest` の `target_protection: Option<RefProtection>`（§12）として渡す。判定は `authorize()` だけが行う（ハンドラーもジョブもロールを比べない）。
- `MergeRequestMerge` の allow は「target の `push_min_role` 以上（`target_protection = None` なら Developer 以上）」。保護ブランチの「直接の push の禁止」（R6）は
  `RepoPush` を止めるが、**マージの経路は止めない**（MR を通すための設定なので。`authorize()` の Merge の arm は直接の push の禁止を読まない）。
- target が `refs/heads/` 以外の MR は作れず、target を `refs/heads/` 以外へ変えることもできない（§1 の入力の検査）。マージの経路は ref を削除も
  巻き戻しもしない（§4.1 の不変条件）。

#### 4.4 失敗がログにどう見えるか

| 起きたこと | コード | ログで見るもの |
|---|---|---|
| 判定で止まった（承認不足など） | FG-REVIEW-仮（MergeBlocked） | `reasons`（§7 の名前の並び）、`mr_id`、`head_sha` |
| 利用者が見た `sha` が今の head と違う / 判定の後にソースが動いた / target が動いた | FG-REVIEW-仮（HeadChanged） | `reason=stale_view|source_moved|target_moved`、`expected`、`actual` |
| ロックの下の `merge-tree` で衝突 | FG-REVIEW-仮（MergeRequestConflict） | `conflicted_files`（数と最初の 10 件のパス、ログだけ。一覧が切れたら `truncated=true`） |
| git が時間切れ / 異常終了（判定・`merge-tree`） | FG-GIT-0002 / FG-GIT-0003 | `operation=merge_tree|commit_tree|merge_base`、stderr の先頭（ログだけ） |
| ref のロックのファイルで update-ref が失敗した / ref を動かす直前に advisory lock を失っていた（どちらも再試行する） | 再試行のログ `forge.review.merge_retry`（warn）。再試行が尽きれば FG-DB-0006（デッドレター）、行は照合へ | `reason=ref_locked|lock_lost`、`merge_id` |
| update-ref の失敗（CAS の負け・ロックのファイル以外）、記録の失敗、照合できない | FG-REVIEW-仮（MergeExecutionFailed）（alert） | `reason=update_ref_failed|record_failed|ref_state_unknown`、`merge_id`、直前の `forge.review.merge_applied` |
| マージ中の MR を変えようとした / 2 つ目のマージ | FG-REVIEW-仮（MergeInProgress） | `mr_id`、`merge_id`、`job_id` |
| 権限が判定とジョブの間に変わった | FG-AUTH-0003 | `action=MergeRequestMerge`、`reason` |

ジョブの開始と終了は `forge.review.merge_started` / `forge.review.merge_completed`（`outcome`）。どの行も `request_id`（マージを要求した要求の ID）と `job_id` を持つ。

### 5. ジョブの主体

- ジョブは要求者の主体で判定し直す。ジョブの行に残るのは利用者 ID と資格情報の種類と ID だけで（§4.2 の 1）、`reconfirm` は **資格情報 ID から
  `Principal` を作り直す**: 利用者の今の有効・管理者・アカウントの種類（`users.kind`、§12）、トークンなら今の `is_agent`・スコープ・失効・期限、
  セッションなら失効・期限を DB から読む。**主体の事実をジョブの行の写しから作らない**（発行の後に変わった権限で実行しない）。
  トークンが失効・期限切れ、セッションが失効していれば FG-AUTH-0003（`action=MergeRequestMerge`、`reason=credential_revoked`）で拒否し、行は
  `superseded`。テスト: 要求の後・ジョブの前にトークンを失効させるとジョブが拒否され、ref が動かない（`merge_job_with_revoked_token_is_denied`）。
  同じ形で、要求の後の利用者の無効化・ロールの低下・target の保護の引き上げでも拒否されることを確かめる。
  ジョブは「forge が要求者の代わりにマージする」のではなく「要求者のマージを後で実行する」もので、判定は要求者の今の権限で行う。
- 監査とイベントの actor は要求者（`actor_kind = 'user'`）。ジョブ ID は監査の詳細に入れる。

### 6. MR の状態遷移とイベント（計画 02 §5.8 のフル連携）

- 状態: `open`（`draft` の真偽を持つ）・`merged`（終端）・`closed`。遷移と対応するイベントは次の **ちょうど 1 つ**:

| 遷移 / 出来事 | イベント |
|---|---|
| （なし）→ `open` | `merge_request.opened` |
| `open` → `merged` | `merge_request.merged` |
| `open` → `closed` | `merge_request.closed` |
| `closed` → `open` | `merge_request.reopened` |
| `open` の中で `draft` ⇄ ready | `merge_request.draft_changed` |
| 版の追加 | `merge_request.pushed` |
| タイトル・説明・target の変更 | `merge_request.updated` |
| レビュアーの指定 | `merge_request.review_requested` |
| 承認 / 承認の取り消し（本人の取り消し、新しいコミット・target の変更による失効） | `merge_request.approved`（`counted: bool`、`uncounted_reason`）/ `merge_request.unapproved`（`reason`） |
| コメント、スレッドの解決・解決の取り消し | `merge_request.commented`（`data.action = note_added|thread_resolved|thread_unresolved`） |
| ソースブランチの削除 | `merge_request.source_branch_deleted` |
| 衝突の発生 / 解消 | `merge_request.conflict_detected` / `merge_request.conflict_resolved` |

- **`merge_request.approved` は「数えられた」を意味しない**（再レビュー）: 承認の行ができた事実を知らせ、`data.counted`（承認の時点の評価で数えたか）と
  `data.uncounted_reason`（`committer`・`not_in_rule`・`role_insufficient`・`committers_unknown` など。数えたときは無し）を持つ。数えるかは評価の
  時点で変わりうる（§8 の 6）ので、購読者（kanflow・画面・`gstn`）は `approved` を数えた承認として扱ってはならず、マージできるかは `evaluate_merge()` の
  結果（`gstn mr status --json`、`merge_blocked` の `reasons`）で読む。購読者の手引きと計画 02 のイベントの説明にもそう書く。
- `merged` からの遷移はない。`closed` から `merged` へは直接行けない（再オープンが先）。
- **テスト**: 遷移と出来事を列挙した型（`MergeRequestTransition::ALL`）から表のテストを作り、**イベントが出ない、または 2 つ出る遷移があれば落とす**
  （`every_merge_request_transition_emits_exactly_one_event`）。DB の CHECK は終端からの遷移を拒む。
- 種類は `forge-core::EventType` に R1〜R6 で足す（ADR-0016 §5 の分類は `integration`、dedup の鍵はサーバー側の ID とハッシュだけ）。`check.completed` は C1。
- 計画 02 §5.8 の図の `thread_resolved` は、状態の遷移ではなく出来事なので `merge_request.commented` の `action` にまとめた（独立のイベント名にしない）。

### 7. `evaluate_merge()` — 止める理由の一覧を返す純粋な関数

- `forge-review` に置く。入力（`MergeFacts`）はすべて呼び出し側が読み込んで渡す（DB も git も読まない）。出力は「マージしてよい」の証明か、
  **止める理由のすべて**（最初の 1 つで止めない）。
- 止める理由（`BlockReason`、`_` のない全列挙。名前は `reasons` のログと `gstn mr status --json` にそのまま出る）:
  `not_open`、`draft`、`merge_in_progress`、`source_branch_missing`、`mergeability_unknown`、`conflict`、`ff_not_possible`、
  `approvals_missing{rule, required, counted}`、`codeowner_approval_missing{pattern, owners}`、`codeowners_invalid{line, reason}`、
  `required_check_missing{name}`、`required_check_not_successful{name, status, conclusion}`、`unresolved_discussions{count}`、
  `work_item_link_missing`（I1 から）、`config_change_needs_maintainer_approval`（§8 の 7）、`changed_paths_unknown{reason}`（§9。変更されたパスを
  上限の中で読み切れない・UTF-8 でないパスがある。CODEOWNERS と設定のファイルの判定ができないので止める）。
- `merge_in_progress` は `MergeFacts` の「実行中のマージの行がある」（§4.2）から出す。`merge_status` からは出さない。
- 並びは固定（画面の「止まっている理由を 1 行」は最初の 1 つ。計画 08）。
- マージを実行する関数は 1 つだけで、`Authorized<MergeRequestCtx>`（`authorize()` の証明）と `MergeClearance`（`evaluate_merge()` の証明。公開の
  コンストラクタを持たない）の **両方** を引数に取る（ADR-0017 の「両方の証明を取る」）。
- 組み合わせの表のテストは proptest（R6）。ジョブの中の再判定は §4.2 の 3 と 5。

### 8. 承認

1. **SHA と target に結ぶ**: 承認の行は `(mr_id, approver_id, head_sha, target_branch)` を持ち、`evaluate_merge()` は **今の `head_sha` と今の
   `target_branch` に一致する承認だけ** を数える（構造で失効させる。ADR-0017 §2.6）。target を変えると承認は数えられなくなる（`dev` で承認を集めてから
   `main` に向け直す抜け道を塞ぐ。計画 11 は SHA だけを書いていたので、ここで足す）。
   **承認の要求は利用者が見た `sha` を必須で持ち**、MR の行をロックした中で今の `head_sha` と比べる。違えば **FG-REVIEW-仮** HeadChanged
   （409、`reason=stale_view`）で断り、承認の行を作らない（見ていない版を承認させない。マージの要求も同じ。§4.2 の 1）。
2. **新しいコミットで消すのが既定**（計画 11 §10 Q4）: 版を足すとき、古い承認ごとに `merge_request.unapproved`（`reason=new_commits`）を出す。
   プロジェクトの設定 `reset_approvals_on_push = false` のときだけ、版を足す処理が人間の承認を新しい SHA へ写す（`carried_from`）。
   **ただし、次のどれかに当たるときは設定にかかわらず写さない**（消して `reason=new_commits` を出す）:
   (a) target が保護ブランチ（`ProtectionLoader` に規則がある）、(b) 前の版の head から新しい head までの変更（`diff-tree --no-renames -r`、§9 と同じ読み方）が
   §8 の 7 の設定のファイルに触れる、(c) その変更が CODEOWNERS の所有者のいるパス（target の先端の CODEOWNERS で判定）に触れる、
   (d) その変更を読み切れない（§9 の `changed_paths_unknown` に当たる）。写すかどうかの判定は `forge-review` の純粋な関数
   （`may_carry_approvals`）で、各場合をテストで固定する。
3. **自己承認の拒否**: `MergeRequestApprove` は `user == author` を `authorize()` で拒む（**FG-REVIEW-仮** SelfApprovalRefused、403）。
   **管理者にも例外はない**（管理者の作成者の自己承認も deny の行を持つ。`admin_author_self_approval_is_denied`）。
   ADR-0017 §2.6 に従い、コミッターの承認は、プロジェクトの設定「コミッターの承認を数えない」（既定で有効）のとき数えない
   （`evaluate_merge()` が数えないだけで、承認そのものは拒まない）。**コミッターの集合は、MR の範囲（`merge_base..head`）のコミットを ref に入れた
   push の記録の actor 全員**: 版の `pushed_by` だけでなく、MR の作成の前の push、ソース以外のブランチへの push（そのコミットが後でソースに入った）も含む。
   push の記録（`repository.pushed` の元の行）の ref の更新のうち、`new` が範囲のコミットであるもの、または範囲のコミットを祖先に持つもの（push の時点で
   その範囲のコミットを運んだ）の actor を数える。正確な問い合わせは R5 でテストとともに固定する（`committers_include_pushes_before_mr_creation`、
   `committers_include_pushes_to_other_branches`）。**判定できないとき（push の記録の保存期間の外・読み切れない）は、その承認者がコミッターでないと
   示せないので、その承認を数えない**（成功の側に倒さない）。コミットのメールでは決めない（ADR-0021）。
   **この規則の副作用は理由の値で見せる**（再レビュー）: (i) push の記録が保存期間を過ぎた古いコミットを含む MR では、承認が数えられず止まりうる
   （`approvals_missing` の内訳に `uncounted_reason=committers_unknown`）、(ii) レビュアーがそのブランチを別の名前で push したことがあると、その人は
   コミッターになり承認が数えられない（`uncounted_reason=committer`、どの push の記録かを `gstn mr status --json` と画面に出す）。どちらも
   runbook `merge-blocked`（新規。対処は別のレビュアーの承認、または push の記録の確認）で説明する。
4. **エージェントのトークンとサービスアカウントは承認もマージもできない**（M3。ADR-0017 §4.0 の脅威 g）。`authorize()` の入力にするため、
   `forge-auth::Principal` に **アカウントの種類** `AccountKind { Human, ServiceAccount, Bot }` を足す。値は `users.kind`
   （`20261028100100_users_and_namespaces.sql` の CHECK `human` / `service_account` / `bot`）から **`Principal` のコンストラクタだけ** が設定する
   （`authenticated(…)` の引数にし、既定値を持たない。後から変える `as_*` の口を作らない。匿名は `Human` でも他でもなく、`user` が無いことで deny）。
   ADR-0017 §4.0 の `Kind`（`Bot(BotKind)`・`System`）の最初の形で、ボットの細分は M4 で足す。
   `MergeRequestApprove` / `MergeRequestUnapprove` / `MergeRequestMerge` / `DiscussionResolve` / `MergeRequestVersionRefsPurge` /
   `CodeownersEnforcementToggle`（§9）/ **`MergeSettingsManage` / `ApprovalRuleManage`**（マージの条件そのものを変えるので。再レビュー）は
   **人だけの arm**（`people_only`）を通る: `is_agent` のトークンは FG-AUTH-0003（`reason=agent_token`。既存の
   値をそのまま使い、ログの検索を 1 つに保つ）、`AccountKind::ServiceAccount` は `reason=service_account`、`AccountKind::Bot` は `reason=bot`。
   **各 Action に、エージェントのトークンの deny の行と、サービスアカウントのトークンの deny の行を両方** 置く（`agent_token_cannot_<action>`、
   `service_account_cannot_<action>`）。
5. **トークンのスコープ**: ADR-0017 §4.0 のとおり `api` スコープは承認・マージを満たさない。`forge-core::Scope` に **`MergeApprove`**（`merge_approve`、
   承認・取り消し）と **`MergeExecute`**（`merge_execute`、マージ）を足す。**`Scope::satisfies` の規則**: 「マージのスコープは自分自身だけが満たす」
   — `MergeApprove` を満たすのは `MergeApprove` だけ、`MergeExecute` を満たすのは `MergeExecute` だけ（`Api` も `Admin` も満たさない。
   `MergeExecute` は `MergeApprove` を満たさず、その逆も満たさない）。マージのスコープは他のどのスコープも満たさない（`Api` / `ReadApi` の代わりにならない）。
   単体テスト `merge_scopes_are_satisfied_only_by_themselves` で `Scope::ALL × Scope::ALL` の表を固定する。
   権限の表の否定の行: `api` だけのトークンで承認・マージ（deny、FG-AUTH-0002）、`merge_approve` だけのトークンでマージ（deny）、`merge_execute` だけの
   トークンで承認（deny）。
   `tokens_scopes_known` の CHECK（`20261030100000_tokens.sql`）は今 5 つの値だけを受けるので、**スコープの変種を足す MR が、同じ MR で追記の
   マイグレーションで CHECK を広げる**: **R4 が `MergeExecute` と `merge_execute` の CHECK、R5 が `MergeApprove` と `merge_approve` の CHECK**
   （`Scope::ALL` と CHECK の値の一致はテストで確かめる。変種だけ・CHECK だけの MR を作らない）。M3 は `Action::required_scope()` の 1 つの値のまま。
   ADR-0017 の `ScopeRequirement`（All / Any）は、2 つのスコープを要る Action（M4 の予約など）と一緒に入れる。
6. **承認のルール**（`approval_rules`、R5）: 名前、対象の target のパターン（任意、`RefPattern`）、必要な数、承認できる人（利用者・グループの一覧。
   空なら Developer 以上の誰でも）。保護ブランチの `protected_ref_merge_rules.required_approvals`（R6）は最低の数。ルールの変更は `evaluate_merge()` の
   入力を変えるだけで、既にある承認は消さない。
   **承認者の有効性は評価の時点で読み直す**: ローダーは承認ごとに、承認者の今の有効（無効化・ロックされていない）、プロジェクトでの今のロール
   （Developer 以上）、アカウントの種類（人間）、ルールの承認できる人に今も入っているか（グループなら今のメンバー）を読み、`evaluate_merge()` は
   満たさない承認を数えない（承認した時点の事実を使わない）。ルールの承認できる人かどうかは `authorize()` ではなくこの評価で決める（承認の後に
   変わりうるので、評価の時点の判定が要る。`authorize()` は Developer 以上の人間で作成者でないことだけを見る）。
7. **設定のファイルを変える MR**: 設定のファイルの集合は **ADR-0017 §2.7 の一覧そのもの**（`.forge/**`、`.forge-ci.yml`、`CODEOWNERS`（直下と
   `.forge/CODEOWNERS`）、対象ブランチの `.forge-ci.yml` が読み込む・実行するファイル、保護パス）。判定は 1 つの関数（`is_config_path`）で、ADR-0017 の
   §2.1 / §4.1.3 と共有する。M3 の時点で入力が無い要素（保護パスの設定 `protected_paths` と、CI の読み込むファイル。どちらも M4）は、入力が入った MR で
   同じ関数に足す（足すまでの間は、その要素は空の集合として扱われる、と ADR とテストの名前で明示する）。これらを変える MR は、ADR-0017 §2.7 の止める理由
   `config_change_needs_maintainer_approval` を出す（評価の時点でロールが Maintainer 以上の人間の承認だけが満たす。作成者とコミッターの承認は数えない）。
   kanflow の出どころの能力（ADR-0017 §4.4）は M4 なので、M3 では所属のロールだけで判定する。変更されたパスの読み方は §9。

### 9. CODEOWNERS

- **置き場所**: target のブランチの `.forge/CODEOWNERS`、無ければ `CODEOWNERS`（リポジトリの直下）。**target の先端（ジョブでは `expected_old`）から読む**。
  MR の中のファイルは自分の MR に効かない。
- **文法**（GitLab の形の一部。計画 11 §10 Q5）: 1 行に「パスの glob + 所有者 1 つ以上」。`#` から行末は注釈、空行は無視。glob は `*`・`**`・`?`、
  先頭の `/` はリポジトリの直下に固定、末尾の `/` はディレクトリの下のすべて、`/` のないパターンはどの深さでも一致。所有者は `@username` と
  `@group/path`（グループの全パス。そのグループの有効なメンバーで、プロジェクトでのロールが Developer 以上の人）。
  **同じパスに複数の行が一致するときは最後の行が効く**。
- **入れないもの**: セクション（`[Section]`）、任意の承認（`^[...]`）、メールでの所有者、否定。これらは「無視」ではなく **解析の誤り** にする。
- **解析できない・知らない所有者・上限超え（大きさと行数の上限は R5 で決める）は黙って無視しない**: `evaluate_merge()` の止める理由 `codeowners_invalid`
  （行番号と `reason`）でマージを止め、ログには **FG-REVIEW-仮** CodeownersInvalid（warn、runbook `codeowners`）を 1 回出す。所有者の要件は
  変更されたパスごとに「一致した最後の行の所有者のうち 1 人以上の承認」（所有者の承認の有効性は §8 の 6 と同じく評価の時点で読み直す）。
- **変更されたパスの読み方**（CODEOWNERS と §8 の 7 の設定のファイルの判定に共通）: 表示用の差分（`browse::diff`。上限・`truncated`・名前の変更の検出
  あり）は **使わない**。専用の `git diff-tree --no-renames -r -z --name-only <merge_base> <head>`（名前の変更の検出なし。名前の変更は「古いパスの削除と
  新しいパスの追加」の両方として数え、どちらの所有者も要る）を、出力の上限で切らずに流して読む（`spawn_stream`。件数の上限と時間切れは設定で持つ）。
  `merge_base` は判定のときに `git merge-base <target> <head>` で求める（ジョブでは `expected_old` と `head_sha` から）。
  **上限の中で読み切れない（件数・時間切れ・git の失敗）とき、または UTF-8 でないパスがあるとき** は、「所有者なし」「設定のファイルなし」として扱わず、
  §7 の止める理由 `changed_paths_unknown{reason=too_many_paths|timeout|git_failed|non_utf8_path}` でマージを止める（UTF-8 でないパスは CODEOWNERS の
  glob と照合できないので、閉じる側に倒す）。git の失敗は FG-GIT-0002 / 0003 で 1 回ログに出す。
- **行き詰まりの逃げ道**: CODEOWNERS が壊れると、それを直す MR も止まる。プロジェクトの設定 `codeowners_enforced`（既定 true、監査とイベントつき）を
  一時的に切り、直して戻す。手順は runbook `codeowners`。切り替えは専用の Action **`CodeownersEnforcementToggle`**（`MergeSettingsManage` に含めない）で、
  - **人だけ**（§8 の 4 の `people_only`。エージェントのトークン・サービスアカウントは deny）、
  - **`above_own_role` の検査**: 切る人のロールは、そのプロジェクトの保護ブランチの `push_min_role` の最大値以上でなければならない（Maintainer は
    Owner の水準で保護されたブランチがあるプロジェクトの CODEOWNERS の強制を切れない。deny `reason=above_own_role`。`Resource` にその最大値を渡す）、
  - **自動で期限が切れる**: 切るときは期限（`codeowners_enforced_off_until`、最長 24 時間。既定は R5 で決める）を必ず付け、`evaluate_merge()` の入力は
    「`codeowners_enforced = true` または 期限を過ぎた」なら強制とみなす（戻し忘れても期限で構造的に戻る。定期のジョブに頼らない）。
    期限が過ぎたことは定期のジョブが監査の行とイベント `merge_settings.codeowners_enforcement_restored` で 1 回知らせる（知らせるためだけ。判定は
    このジョブにもイベントにも頼らない）。

### 10. Checks API（計画 11 §3.3 案 A）

- check run は **コミットの SHA に結ぶ**: `check_runs`（`project_id`、`head_sha`、`name`、`status` = `queued` / `in_progress` / `completed`、
  `conclusion` = `success` / `neutral` / `failure` / `error` / `action_required`（`completed` のときだけ）、`summary`、`details_url`、書いた主体、時刻）と
  `check_annotations`（上限つき）。同じ `(project_id, head_sha, name)` は 1 行で、同じ名前の再登録はその行を更新する。
- **名前空間**: 名前は `<namespace>/<name>`。`ci/`（CI のサービスアカウント）、`forge/`（forge 自身の `system` の worker だけ）、`kanflow/`（kanflow の
  サービスアカウント）、`ext/`（`api` スコープの Developer 以上の人間。外部の道具を人が使う場合）。
- **`CheckWrite` の判定**（`authorize()` だけが行う）: `Resource::CheckRun { project: ProjectView, namespace: CheckNamespace, sha_stakeholders: Vec<UserId> }`
  （`sha_stakeholders` は、その `head_sha` を今の head に持つ開いている MR の作成者と §8 の 3 のコミッター。ローダーが読む）。
  - `forge/` は **HTTP からは誰にも書けない**（管理者も deny）。forge 自身のチェックは `system` の worker が HTTP を通らず DB の経路で書く
    （ADR-0017 の `InternalCheckWrite`、`app::INTERNAL`）。
  - `ci/` / `kanflow/`: その名前空間を割り当てられたサービスアカウント（割り当ては S0）だけが、**自分に結びつけられたプロジェクトにだけ** 書ける
    （`ProjectView.via_bound = true`。ADR-0017 §4.0）。結びついていないプロジェクトへの書き込み、他の名前空間への書き込み、人間のトークンは deny。
  - `ext/`: `api` スコープの Developer 以上の人間。ただし **`sha_stakeholders`（作成者・コミッター）とエージェントのトークンは deny**
    （自分の MR のチェックを自分で緑にさせない）。サービスアカウントも `ext/` には書けない。
  - 拒否は **FG-REVIEW-仮** CheckNamespaceDenied（403）、注釈の上限超え・形の誤りは **FG-REVIEW-仮** CheckPayloadInvalid（422）。
- **必須のチェック**は「MR の今の `head_sha` に、その名前の `completed` かつ `success` がある」。**チェックが無いことは成功と区別する**
  （`required_check_missing`。計画 02 §8.3 O-6）。`neutral` も必須のチェックを満たさない。
- **必須にできる名前はサービスアカウントか forge が持つ名前空間（`ci/`・`kanflow/`・`forge/`）だけ**。`ext/` の名前を必須のチェックに設定する要求は
  設定の API が **FG-REVIEW-仮** RequiredCheckNamespaceNotAllowed（422、この ADR で追加）で拒む。人が書ける名前でマージの条件を満たさせない。
- `check.completed` は結論が変わったときだけ出す（C1）。
- target が進んだときにチェックを「古い」とする扱い（ADR-0006 のマージ結果のパイプライン）は M4。M3 のチェックは head の SHA にだけ結ぶ。

### 11. クレートと依存の向き（契約 9）

```
forge-web / forge-cli（serve・worker が組み合わせる）
   ├─▶ forge-review  （MR・版・承認・CODEOWNERS・evaluate_merge・マージのジョブ・mergeability のジョブ）
   └─▶ forge-checks  （check run の検証・名前空間の判定・注釈の上限・check.completed の規則）
           │ どちらも ▼（互いには依存しない）
   forge-auth / forge-git / forge-db ─▶ forge-core（CheckResultsSource など）─▶ forge-errors
```

- `forge-review` は check の結果を要るが、`forge-checks` に依存しない。`forge-core` の trait `CheckResultsSource` を読み、実装は `forge-db` の `Db` が持つ
  （SQL は forge-db だけが持つ。`ProtectionLoader` と同じ形）。`forge serve` / `forge worker` が注入する。
- 作業項目のリンクの条件（`work_item_link_missing`）は、I1 で `forge-core` に trait を足して入れる（`forge-review` は `forge-integ` に依存しない）。
  R6 の時点ではこの条件は設定できない（設定の API が拒む）。
- `crates/forge-hosting/tests/dependency_direction.rs` の表に 2 つのクレートの行を足し、両者の間の辺がないことを検査する。
- **review の `*_decided` の命令の呼び出し元**: `crates/forge-hosting/tests/decided_callers.rs` の一律の例外（`forge-db` と `forge-hosting` だけが
  `*_decided` を呼べる）に **`forge-review` を足さない**（足すと、forge-review が hosting の命令まで呼べることを許してしまう）。代わりに別のテスト
  `only_forge_review_calls_the_review_commands`（同じ形の走査）を足し、`forge-db/src/review/` の `*_decided` の命令とマージ・承認の命令は
  **forge-review（と forge-db 自身のテスト）だけ** が呼ぶことを確かめる。加えて、forge-review の公開の関数は型で証明を要求する:
  MR を変える命令は `Authorized<MergeRequestCtx>` を、承認は `Authorized<MergeRequestCtx>`（Action が `MergeRequestApprove`）を、
  マージの実行は `Authorized<MergeRequestCtx>` と `MergeClearance` の両方を引数に取る（§7）。forge-db の命令は forge-auth の型を取れない
  （依存の向き）ので、この 2 段（呼び出し元の走査 + forge-review の型）で守る。計画 11 §11 の 2 の「例外に足す」はこの決定で置き換える。
- **命令の名前は既存の走査の needle と重ならないように付ける**（再レビュー）: 既存の `decided_callers.rs` は `create_decided(`・`update_decided(`・
  `change_decided(`・`delete_decided(`・`restore_decided(` などの語尾で探すので、review の命令を `create_decided` などと名付けると forge-review からの呼び出しで
  古いテストが落ちる（あるいは例外が要る）。そこで review の命令は **`open_mr_decided`・`edit_mr_decided`・`close_mr_decided`・`reopen_mr_decided`・
  `add_mr_version_decided`・`approve_mr_decided`・`unapprove_mr_decided`・`resolve_discussion_decided`・`merge_mr_decided`** のように `_mr_` / 対象の名前を
  含む、既存の needle のどれにも一致しない名前にする（`<needle>_decided(` の部分文字列にならないこと）。新しいテストはこの一覧を needle にする。
  古いテストは変えず、例外も足さない。命令を足すときは、名前が既存の needle に一致しないことを新しいテストが確かめる。

### 12. 権限（新しい Action。どれも allow と deny の行を持つ）

**MR の `Resource`**（`forge-auth`。ローダーが読み、`authorize()` だけが判定に使う）:

```rust
Resource::MergeRequest {
    project: ProjectView,                       // 見えるか・ロール・via_bound
    author: UserId,
    state: MergeRequestState,                   // open / merged / closed
    draft: bool,
    source_branch: RefName,                     // refs/heads/* だけ
    target_branch: RefName,                     // refs/heads/* だけ
    target_protection: Option<RefProtection>,   // ProtectionLoader の target の規則（push_min_role など）
}
Resource::Discussion { mr: /* 上の MergeRequest と同じ形 */, started_by: UserId }
Resource::CheckRun { project: ProjectView, namespace: CheckNamespace, sha_stakeholders: Vec<UserId> }   // §10
```

- `state` と `draft` は Update / Close / Reopen の arm（ドラフトの切り替えの主体、`merged` の再オープンの拒否）だけが読む。Approve / Merge の arm は
  読まない（条件未達・ドラフト・状態は `evaluate_merge()` が止める。権限と状態を混ぜない）。
- `source_branch == target_branch` はどの MR の arm でも deny（`reason=source_equals_target`。§1）。
- **見えないプロジェクトの MR は、iid が存在してもしなくても FG-REPO-0005 の 404 で、応答の本文も同じ**。見えるプロジェクトで iid が存在しないときも
  FG-REPO-0005（MR 専用の not-found のコードは作らない）。行: 見えない × 存在する iid、見えない × 存在しない iid、見える × 存在しない iid が同じ応答になる
  （`invisible_merge_request_is_indistinguishable_from_missing`）。
- **代理（act-as）は ADR-0025 が受理されるまで、この節のすべての Action で deny**。今の `Credential` は `Session` と `Token` だけで、代理の資格情報の
  種類が無いので、代理はそもそも到達できない。**代理の deny の行（`act_as_denied_for_<action>`）は、代理の資格情報を足す ADR-0025 の MR と一緒に
  この節のすべての Action について入れる**（再レビューで明記。資格情報の種類が無いうちは行を書けない）。下の表の「代理」の列は ADR-0025 の受理の後の
  目標で、今の挙動ではない。ADR-0025 で決めること（計画 02 / ADR-0025 の方向。ここでは記録だけ）: 社員番号は Keycloak の
  claim から取り、一意にする。主張（assertion）は `sub`（本人）と `via`（サービスアカウント）の両方に結びつける。代理の読み取りの規則
  （どの読み取りを主張なしで許すか）も ADR-0025 で決める。
- 管理者も作成者なら自分の MR を承認できない（`admin_author_self_approval_is_denied`）。

| Action | 必要なスコープ | allow | deny（行に入れる） | 代理（ADR-0025 の後の目標。それまで deny） |
|---|---|---|---|---|
| `MergeRequestCreate` | `api` | ソースのブランチに push できる Developer 以上 | Reporter、private の Guest、匿名、削除待ちのプロジェクト、source == target、`refs/heads/` 以外の target | 不可 |
| `MergeRequestRead` / `MergeRequestList` | `read_api` | `RepoBrowse` と同じ | 見えない利用者（FG-REPO-0005 の 404。存在しない iid と同じ応答） | 読み取りは可（ADR-0025 の規則） |
| `MergeRequestUpdate` / `MergeRequestClose` / `MergeRequestReopen` | `api` | 作成者、Developer 以上（他人の MR のドラフトの切り替えは作成者と Maintainer 以上） | Reporter、他人のドラフトを切り替える Developer、target を `refs/heads/` 以外・source と同じ名前に変える | 不可 |
| `MergeRequestComment` | `api` | Reporter 以上。**エージェントのトークンも可**（コメントはマージの条件にならない。スレッドの解決は別の Action）。サービスアカウントも自分のロールの範囲で可 | private の Guest、匿名、Reporter 未満 | 不可 |
| `DiscussionResolve` | `api` | **人だけ**。自分が始めたスレッド（Reporter 以上）。**他人が始めたスレッドは、MR の作成者でない Developer 以上** | エージェントのトークン（`agent_token`）、サービスアカウント（`service_account`）、Reporter が他人のスレッド、**MR の作成者が他人のスレッド**（作成者が `unresolved_discussions` を自分で消さない） | 不可 |
| `MergeRequestApprove` / `MergeRequestUnapprove` | `merge_approve` | 人間の Developer 以上で作成者でない（取り消しは自分の承認だけ）。ルールで数えるかは評価の時点（§8 の 6） | **作成者自身**（FG-REVIEW-仮（SelfApprovalRefused）。管理者でも）、エージェントのトークン、サービスアカウント、`api` だけのトークン、`merge_execute` だけのトークン、Reporter | **可（主張が必須。ADR-0025）** |
| `MergeRequestMerge` | `merge_execute` | target の `push_min_role` 以上（保護なしは Developer 以上）。直接の push の禁止（R6）は効かない | 保護が Maintainer の target × Developer、エージェントのトークン、サービスアカウント、`api` だけのトークン、`merge_approve` だけのトークン | 不可（M4） |
| `MergeSettingsManage` / `ApprovalRuleManage` | `api` | **人だけ**。Maintainer 以上 | Developer、エージェントのトークン、サービスアカウント、`ext/` を必須のチェックにする設定（FG-REVIEW-仮（RequiredCheckNamespaceNotAllowed）） | 不可 |
| `CodeownersEnforcementToggle` | `api` | 人だけ。Maintainer 以上で、ロールがプロジェクトの保護ブランチの `push_min_role` の最大値以上。期限つき（§9） | エージェントのトークン、サービスアカウント、Developer、`above_own_role`（Owner の保護があるプロジェクトの Maintainer）、期限なし・期限が上限を超える要求 | 不可 |
| `MergeRequestVersionRefsPurge` | `SessionOnly` | 人間の管理者のセッションだけ。監査つき。開いている MR はクローズか書き換えの後（§2） | 管理者でない、**`admin` スコープのトークンを含むすべてのトークン**、エージェントのトークン、サービスアカウント | 不可 |
| `CheckWrite` | `api` | §10: `ci/`・`kanflow/` は割り当てられたサービスアカウント × 結びついたプロジェクト（`via_bound`）、`ext/` は作成者・コミッターでない人間の Developer 以上 | `forge/`（HTTP からは管理者も）、他の名前空間、結びついていないプロジェクト、`ext/` × 作成者・コミッター・エージェントのトークン・サービスアカウント | 不可 |
| `CheckRead` | `read_api` | `RepoBrowse` と同じ | 見えない利用者 | 不可 |

- 人だけの Action（Approve / Unapprove / Merge / DiscussionResolve / CodeownersEnforcementToggle / VersionRefsPurge / MergeSettingsManage / ApprovalRuleManage）は、**エージェントのトークンの deny の行と
  サービスアカウントのトークンの deny の行を Action ごとに両方** 持つ（§8 の 4）。
- 承認とマージの否定の行（スコープ）: `api` だけのトークン × 承認・マージ、`merge_approve` だけ × マージ、`merge_execute` だけ × 承認（§8 の 5）。
- `MergeRequestMerge` の保護の行: target が Maintainer で保護 × Developer の呼び出し（deny）、保護なし × Developer（allow）、管理者（allow。管理者の権限が
  効く資格情報のとき）、エージェントのトークン（deny）、サービスアカウント（deny）。
- 保護ブランチのマージの条件（`protected_ref_merge_rules`）の管理は既存の `ProtectedRefManage` に含める（行を足す）。
- 条件未達・ドラフトは `authorize()` ではなく `evaluate_merge()` が止める（権限と状態を混ぜない）。
- 「他人のドラフトを切り替える Developer」の deny は計画 11 §4.3 の「他人のドラフトの変更」の解釈。R1 で行を確定する。
- どのルートも C-1 の表（`forge-client/src/coverage.rs`）に行が要る。`gstn mr create/list/view/diff/comment/merge --wait/status --json`、`gstn checks` を
  ルートと同じ MR で足す。

## Alternatives Considered

### MR の差分を DB に保存する（計画 11 §3.1 案 B）
- **Pros**: 差分の表示で git を起動しない。
- **Cons**: DB が膨らむ。git と DB の二重の真実になり、復元のときにずれうる。
- **Why not**: 差分は git から決定的に作れる。ずれは診断の対象を増やすだけ。

### MR の差分をファイルの置き場に保存する（計画 11 §3.1 案 C）
- **Pros**: DB を膨らませない。
- **Cons**: 新しい保存先と、そのバックアップ・復元の整合（O1）が要る。
- **Why not**: 案 A で足り、置き場を増やす理由がない。

### 隠した ref を持たない（オブジェクトだけを残す）
- **Pros**: ref が増えない。
- **Cons**: force push とブランチの削除で、レビューした版のコミットが gc で消える。
- **Why not**: 版の差分と承認した SHA が後から読めなくなる。

### 一時的な作業ツリーでマージする（計画 11 §3.2 案 B）
- **Pros**: rebase を含む全方式を作れる。
- **Cons**: 一時ディレクトリの後始末、時間切れ、ディスク、作業ツリーのフックや設定の影響。
- **Why not**: R1 の 3 方式は plumbing で足りる。rebase だけのために R1 の危険を増やさない（M3 後半で再検討）。

### libgit2 などでマージする（計画 11 §3.2 案 C）
- **Why not**: 契約 5（git は実行する）に反する。

### マージを 1 つのトランザクションで行う（ロックも MR の行のロックも tx の中だけ）
- **Pros**: 状態 `prepared` がいらない。
- **Cons**: update-ref の後にコミットが失敗すると、ref が動いたことが DB のどこにも残らない。再試行は「target に head が入っているか」で推測するしかなく、
  squash では推測できない。
- **Why not**: 照合の材料（`expected_old` と `new_sha`）を ref を動かす前に確定させる方が、ログと DB だけで決定的に片付く。

### 要求の中で同期してマージする
- **Why not**: git の時間が HTTP の時間切れと要求の取り消しに縛られ、ref が動いた後に切断されると記録の責任が曖昧になる。

### commit status（名前ごとに状態 1 つ）（計画 11 §3.3 案 B）
- **Why not**: 注釈と助言を表せない。M4 の CI と M5 の LLM が提供者になれない。

### MR に結ぶ check（計画 11 §3.3 案 C）
- **Why not**: push で無効になり、同じコミットの結果を再利用できない。承認と同じく SHA に結ぶ方が構造で正しい。

### CODEOWNERS の読めない行を飛ばす（GitLab の挙動）
- **Why not**: 所有者の要件が黙って消える（沈黙の失敗、契約 2）。止めて理由を見せ、逃げ道は監査つきの設定にする。

## Consequences

### Positive
- 差分の真実は git だけ。MR の失敗は FG-GIT の既存のコードと FG-REVIEW のコードに分かれ、どの段で落ちたかがコードと `reason` で分かる。
- マージの経路でも保護の判定と push の副作用（`repository.pushed`）が出るので、ミラー（O2）と連携（I1）がマージを取りこぼさない。
- 「ref が動いたか、DB が追いついたか」は `forge.review.merge_applied` と `merge_request_merges.state`（`executing` なら git に聞く）で決定的に分かる。
- `evaluate_merge()` が理由をすべて返すので、画面・`gstn`・kanflow が同じ理由を表示できる。

### Negative
- `queued` / `prepared` / `executing` の状態と照合のジョブを持つ分、マージの状態機械が大きくなる。
- 版ごとの ref を持つので、force push の多い MR は ref が増える（ref の数はリポジトリの大きさと同じく doctor で見る）。
- 隠した ref はクローズした MR の分も残り、リポジトリの大きさが増える。
- スコープ `merge_approve` / `merge_execute` が増え、利用者はマージ用のトークンを別に発行する必要がある（`api` では足りない）。

### Risks
- **マージの経路が保護を迂回する** → 判定は `authorize()` だけ、ロックの下で再判定、不変条件（子孫であること）、R4 のテスト「保護 ref の判定がマージの経路でも効く」。
- **隠した ref の漏れ**（upload-pack と receive-pack の広告・`want` の 2 つの走査・閲覧の API）→ §2 の各か所をテストで固定。閲覧の API の ref の一覧は
  確かめた。SHA の指定の到達可能性は M3-0 の `fix/git-browse-reachability` で直す。
- **ミラーとバックアップ**: O2 の GitLab へのミラーは **明示の refspec**（`refs/heads/*`・`refs/tags/*` だけ。`--mirror` や `refs/*` を使わない）で送り、
  `refs/merge-requests/*` を外へ出さない（テスト: ミラー先に `refs/merge-requests/` が無い）。O1 のバックアップはリポジトリのファイルの複写なので
  隠した ref（版の ref）を **含む**。`forge restore verify` は版の ref と `merge_request_versions` の突き合わせも行う。
- **記録の失敗が放置される** → FG-REVIEW-仮（MergeExecutionFailed）（alert）、doctor の `jobs.merge`、照合のジョブ、runbook `merge-execution`。
- **CODEOWNERS で全員が止まる** → `codeowners_enforced` の逃げ道（監査つき）と runbook `codeowners`。
- **advisory lock の保持中に DB の接続を 1 つ使い続ける** → マージのジョブの同時実行の上限を設定に持つ（R4 で既定を決める）。

## Error codes and runbooks affected

**エラーの領域**: `crates/forge-errors/build.rs` の `AREAS` は今 `AUTH GIT REPO DB CI DEPLOY DOC WEB SYS CLI` の 10 個で、`INT`・`LLM`・`REVIEW` が無い
（ADR-0017 決定 8 で承認済み、エージェントモデルのエラーの規則には既にある）。**R1 が `REVIEW` を、最初に `INT` のコードを登録する MR（I1 か O2）が `INT` を
足す**。`LLM` は最初の LLM のコードと一緒に足す。

予約するコード。**番号は登録の順に決める**（計画 11 §11.4）。R1a で登録した DuplicateOpenMergeRequest が **FG-REVIEW-0001**、R1b で登録した PurgePreconditionFailed が **FG-REVIEW-0002**、R2 のレビューで登録した MergeabilityNotChecked が **FG-REVIEW-0003**、R3 で登録した DiscussionPositionInvalid が **FG-REVIEW-0004**、R3 のレビューで登録した DiscussionLimitReached が **FG-REVIEW-0005** で、ほかはすべて仮（下の表と本文の「FG-REVIEW-仮」）。番号は各 MR が errors.toml に登録したときに確定し、そのときにこの表を直す（R1a のレビューで改訂）:

| コード | 名前 | HTTP | 重さ | runbook | 入れる MR |
|---|---|---|---|---|---|
| 仮（登録の順に決める） | MergeRequestConflict | 409 | info | — | R2 / R4 |
| 仮（登録の順に決める） | MergeBlocked（`reasons`） | 409 | info | — | R6（入口は R4） |
| 仮（登録の順に決める） | HeadChanged（`stale_view` / `source_moved` / `target_moved`） | 409 | info | — | R4 |
| 仮（登録の順に決める） | MergeExecutionFailed（`update_ref_failed` / `record_failed` / `ref_state_unknown`） | 500 | **alert** | `merge-execution`（新規） | R4 |
| FG-REVIEW-0001（R1a で登録済み） | DuplicateOpenMergeRequest | 409 | info | — | R1 |
| 仮（登録の順に決める） | SelfApprovalRefused | 403 | info | — | R5 |
| 仮（登録の順に決める） | CodeownersInvalid | 422 | warn | `codeowners`（新規） | R5 |
| FG-REVIEW-0004（R3 で登録） | DiscussionPositionInvalid | 422 | info | — | R3 |
| FG-REVIEW-0005（R3 のレビューで登録） | DiscussionLimitReached（スレッド・コメントの上限） | 409 | info | — | R3 |
| 仮（登録の順に決める） | MergeMethodNotAllowed | 409 | info | — | R4 |
| 仮（登録の順に決める） | CheckNamespaceDenied | 403 | info | — | C1 |
| 仮（登録の順に決める） | CheckPayloadInvalid | 422 | info | — | C1 |
| 仮（登録の順に決める） | MergeInProgress（この ADR で追加） | 409 | info | — | R4 |
| 仮（登録の順に決める） | RequiredCheckNamespaceNotAllowed（設計レビューで追加。`ext/` などを必須のチェックにする設定） | 422 | info | — | R6 |
| FG-REVIEW-0003（R2 のレビューで登録） | MergeabilityNotChecked（`job_failed` / `job_stale` / `fanout_deferred` / doctor の `undecided`。HTTP では返さない） | 500 | warn | `merge-request-mergeability`（新規） | R2 |
| FG-REVIEW-0002（R1b で登録済み） | PurgePreconditionFailed（再レビューで追加。開いている MR の今の head の版の ref の削除、Purge 済みの head での再オープン） | 409 | info | `secret-removal` | R1b |

- 再利用: 権限の拒否は FG-AUTH-0003（エージェントは `reason=agent_token`、サービスアカウントは `service_account`、ボットは `bot`、ジョブの時点で失効した
  資格情報は `credential_revoked`、`source_equals_target`、`above_own_role`）、スコープ不足は FG-AUTH-0002、見えない MR と存在しない iid は FG-REPO-0005 の 404、
  閲覧の API で届かない SHA は存在しない SHA と同じ FG-REPO-0023（`reason=ref_not_found`）、git の失敗は
  FG-GIT-0002 / 0003、`refs/merge-requests/*` への push は FG-GIT-0010、DB は FG-DB-0001、ジョブのデッドレターは FG-DB-0006、不変条件の破れは FG-SYS-0001。
- runbook: `merge-execution`（新規。`forge.review.merge_applied` と `merge_request_merges` の突き合わせ、照合のジョブ、`ref_state_unknown` の人の判断）、
  `codeowners`（新規。行番号と `reason` の読み方、`codeowners_enforced` の切り替えは承認された手順）、`secret-removal`（新規。§2 の版の ref の削除、
  到達できないオブジェクトの prune、ミラーとバックアップの写し。開いている MR は先にクローズか書き換え。どれも人が承認した手順で、秘密の失効が先）、
  `merge-blocked`（新規。コミッターの規則で承認が数えられない理由 `committer` / `committers_unknown` の読み方と対処）。`merge-execution` には `queued` /
  `prepared` / `executing` の各状態の読み方と、`executing` の照合（git に聞く）を書く。
- 他の ADR への追記: ADR-0007（`want` の確認の除外、R1）。ADR-0008 の Amendment 段階 9 MR4 が「M3（段階 10）」に送った pre-receive / post-receive の
  フックは、この ADR の範囲ではない（マージの経路はフックを通らない設計にした。push の経路のフックをいつ入れるかは計画 11 にない。下の「未決」）。

## 未決

- push の経路の pre-receive / post-receive のフック（ADR-0008 の Amendment 段階 9 MR4 が M3 に送ったもの）を M3 のどの MR で入れるか。計画 11 では
  「push 時の検査（X8、M3 後半）」だけがフックに触れる。
- マージのジョブの同時実行の上限、CODEOWNERS の大きさの上限の既定値（R4・R5 で決める）。`merge-tree` の時間切れは R2 で 30 秒に決めた（末尾の R2 の節）。
- 変更されたパスの件数の上限と時間切れ、`codeowners_enforced_off_until` の既定と上限（24 時間以内）、保守の prune の猶予
  （24 時間以上）の既定値（R5・O3 で決める）。`review.mergeability_fanout_max` は R2 で既定 50・上限 500 に決めた。
- **エージェントモデルへの申し送り**（再レビュー、ここでは記録だけ。製品の MR ではない）: ゲート `authz-coverage` が、運用の例外の一覧（CLAUDE.md の
  契約 4 と ADR-0019 §8 の `forge admin …` / `forge backup` / `forge restore`）と実際の運用のコマンドの一致も確かめるようにする（[root] の MR）。

## 設計レビューで決めたこと（2026-11-02）

P1 の設計レビュー（`authz-matrix-auditor`・`git-protocol-reviewer`・`security-reviewer`）の指摘への決定。**この表を正とする**（計画 11 §11）。
本文の該当の節は書き換え済み。指摘の修正案をそのまま採らなかったものは「決定」に理由を書いた。

| # | 指摘の要旨 | 決定 | 節 |
|---|---|---|---|
| 1 | HIGH: 「サービスアカウントは拒否」を `authorize()` が判定する入力が無い | `Principal` に `AccountKind { Human, ServiceAccount, Bot }` を足し、`users.kind` からコンストラクタだけが設定する。Approve / Unapprove / Merge / DiscussionResolve（と CodeownersEnforcementToggle・VersionRefsPurge）は人だけの arm。理由は `agent_token`（既存の値を保つ。指摘の `agent` と同じ意味）/ `service_account` / `bot`。Action ごとにエージェントとサービスアカウントの deny の行 | §8 の 4、§12 |
| 2 | HIGH: エージェント・サービスアカウントが `DiscussionResolve` で `unresolved_discussions` を消せる。他人のスレッドの解決の主体 | `DiscussionResolve` は人だけ。他人が始めたスレッドは MR の作成者でない Developer 以上だけ（作成者は自分の MR の他人のスレッドを解決できない）。`MergeRequestComment` はエージェントのトークンにも許す（コメントはマージの条件にならない）と明記し、行を置く | §12 |
| 3 | HIGH: スコープの満たし方が未定義 | `Scope::MergeApprove` / `MergeExecute` を足す。規則「マージのスコープは自分自身だけが満たす」（`Api`・`Admin` は満たさず、互いにも満たさない）。否定の行（`api` だけ、approve だけでマージ、execute だけで承認）。CHECK を広げるマイグレーションは変種を足す MR と同じ: **R4 が `merge_execute`、R5 が `merge_approve`**（R4 のマージに `merge_execute` が要るので「両方 R5」は採らない） | §8 の 5、§12 |
| 4 | HIGH: MR の `Resource` と Merge の allow が未定義 | `Resource::MergeRequest { project, author, state, draft, source_branch, target_branch, target_protection: Option<RefProtection> }`。Merge の allow は target の `push_min_role` 以上（保護なしは Developer）、直接の push の禁止（R6）はマージを止めない。保護 Maintainer × Developer（deny）、保護なし（allow）、管理者、エージェント、サービスアカウントの行 | §4.3、§12 |
| 5 | MEDIUM: ジョブの再判定が古い主体の写しを使いうる | ジョブの行には利用者 ID と資格情報の種類・ID だけを残し、`reconfirm` が資格情報 ID から `is_agent`・種類・スコープ・失効を読み直す。失効したトークン → ジョブは拒否（`merge_job_with_revoked_token_is_denied`） | §4.2 の 1、§5 |
| 6 | MEDIUM: 作成者の拒否が管理者にも効くか | 効く。管理者の自己承認の deny の行 | §8 の 3、§12 |
| 7 | MEDIUM: 見えない MR の応答で存在が漏れる | 見えないプロジェクトは存在する iid でも存在しない iid でも FG-REPO-0005 で同じ応答。MR 専用の not-found のコードは作らない | §12 |
| 8 | MEDIUM: `CheckWrite` の `Resource` と `forge/`・サービスアカウントの範囲 | `Resource::CheckRun { project, namespace, sha_stakeholders }`。`forge/` は HTTP から常に deny（system の worker は DB の経路）。サービスアカウントは自分の名前空間 × 結びついたプロジェクト（`via_bound`）だけ | §10、§12 |
| 9 | MEDIUM: `decided_callers.rs` の一律の例外に forge-review を足すと広すぎる | 足さない。別のテスト `only_forge_review_calls_the_review_commands` と、forge-review の関数が `Authorized<MergeRequestCtx>`（マージは + `MergeClearance`）を型で取ることで守る。計画 11 §11 の 2 を置き換える | §11 |
| 10 | LOW: 代理の扱い | ADR-0025 の受理まで、新しい Action のすべてで代理は deny（行は ADR-0025 の MR で入れる。#38）。表の「代理」の列は受理の後の目標 | §12 |
| 11 | HIGH: `head` の ref 1 つでは force push で版のコミットが消える | 版ごとの `refs/merge-requests/<iid>/v<n>` と `head` を 1 つの transaction で書く。再試行で「既に `head_sha`」は成功 | §2 |
| 12 | HIGH: `merge-tree` / `merge-base --is-ancestor` の終了コード 1 を今の `command` が失敗にする | 「受け付ける終了コード」の出力の形を足し、そのコードでは標準出力を残す。衝突の一覧は切り詰めの出力で、上限超えを FG-GIT-0006 にしない。2.38 のテストで固定 | §3、§4.2 |
| 13 | HIGH: ref の更新のフェンスが無い（ロックを失った worker と照合が競う） | ref の直前に行を `prepared → executing` に比較して交換、ロックの接続が生きていてロックがあることを確かめる。照合は `executing` を「git に聞く」（時間切れ + 余裕の後だけ）。`repository.pushed` と Outbox の dedup の鍵は `merge_id` | §4.2 の 6・9、照合 |
| 14 | MEDIUM: CAS の負けの見分け方 | 失敗したら ref を読み直す: 先端 ≠ `expected_old` → `target_moved`（409、FG-REVIEW-仮（HeadChanged））、ロックのファイル → 再試行、それ以外だけ FG-REVIEW-仮（MergeExecutionFailed）（alert） | §4.2 の 7、§4.4 |
| 15 | MEDIUM: 実行中を `merge_status` に混ぜている | `merge_status` はマージできるかだけ。実行中は `merge_request_merges` の `queued` / `prepared` / `executing` の行で表し、部分一意索引で 1 つに限る。git は行をロックしたトランザクションの外（tx1a / tx1b に分けた） | §3、§4.2 |
| 16 | MEDIUM: ソースブランチの削除が push の副作用を出さない | 削除の `repository.pushed` を自分のトランザクションで出す（dedup `merge_source_deleted:<merge_id>`） | §4.2 の 10 |
| 17 | MEDIUM: hideRefs が upload-pack だけ、ミラー・バックアップの扱い | `UPLOAD_PACK_CONFIG` と `RECEIVE_PACK_CONFIG` の両方。O2 は明示の refspec（`refs/merge-requests` を送らない）。バックアップは版の ref を含む | §2、Risks |
| 18 | MEDIUM: mergeability の扇の広がりと、参照されないオブジェクト | 1 回の前進で積むジョブに上限（超えた分は `unchecked` で止まり、表示・要求のときに積む）。O3 の prune は照合の窓より長い猶予つき | §3 |
| 19 | MEDIUM/LOW: 引数の並び、`want` の 2 つの走査、空の名前 | `commit-tree` / `merge-tree --write-tree` / `merge-base` の引数は R4 で 2.38 のテスト。`exclude_glob` を足し、`want_check.rs` の手順 2 と 3 の両方の `--all` に付ける。glob と hideRefs は 1 つの定数から。空の名前は `username` に置き換える | §2、§4.1 |
| 20 | HIGH: CODEOWNERS・設定の判定に表示用の差分（上限・名前の変更の検出）を使うと所有者が消える | 専用の `diff-tree --no-renames -r -z --name-only <merge_base> <head>` を上限で切らずに流す。読み切れなければ `changed_paths_unknown` で止める（「所有者なし」にしない） | §9、§7 |
| 21 | HIGH: 承認を新しい SHA へ写す設定が保護ブランチ・設定の変更にも効く | 保護された target、設定のファイル・所有者のいるパスに触れる新しい版、読み切れない変更では、設定にかかわらず写さない（`may_carry_approvals`） | §8 の 2 |
| 22 | HIGH: コミッターを版の `pushed_by` だけで決めている | MR の範囲のコミットを運んだすべての push の記録の actor（MR の作成の前、他のブランチへの push を含む）。判定できなければその承認を数えない | §1、§8 の 3 |
| 23 | HIGH: `ext/` のチェックを必須にすると人がマージの条件を満たせる | 必須にできるのは `ci/`・`kanflow/`・`forge/` だけ（設定は FG-REVIEW-仮（RequiredCheckNamespaceNotAllowed） で拒む）。`ext/` は作成者・コミッター・エージェントのトークンに deny | §10、§12 |
| 24 | HIGH: 承認・マージが利用者の見た版に結びついていない | 承認とマージの要求は `sha` を必須で持ち、今の head と違えば FG-REVIEW-仮 HeadChanged（409、`reason=stale_view`） | §4.2 の 1、§8 の 1 |
| 25 | HIGH: 閲覧の API が到達できないオブジェクトを ID で返す（`browse/refs.rs` の `commit_of`） | SHA の指定はブランチ・タグ（MR の文脈では、その MR の版の ref）から届くときだけ解決し、届かなければ存在しない SHA と同じ応答。人だけ・監査つきの管理者の操作 `MergeRequestVersionRefsPurge`（秘密の除去）。M3-0 に `fix/git-browse-reachability`（R1 の前に必須）を足す | §2、§12、計画 11 §5.1 |
| 26 | HIGH: 復元で失効した資格情報が生き返る | `forge restore` は復元の前に発行されたすべてのセッションと個人のトークンを失効させ（利用者は再発行）、利用者の無効化の状態を Keycloak と監査の記録から当て直す。バックアップは暗号化し、ファイルの権限を絞る | ADR-0019 の Amendment（計画 11 P1）、計画 11 O1 |
| 27 | MEDIUM: 承認者の有効性を承認した時点で決めている | 評価の時点で、有効・ロール・種類・ルールの承認できる人を読み直す。ルールの承認できる人かは `authorize()` でなく評価で決める（承認の後に変わるので） | §8 の 6 |
| 28 | MEDIUM: `codeowners_enforced` の切り替えが広すぎる | 専用の Action `CodeownersEnforcementToggle`: 人だけ、`above_own_role`、最長 24 時間で自動で戻る（判定の入力で構造的に） | §9、§12 |
| 29 | MEDIUM: 設定のファイルの集合が ADR-0017 とずれている | ADR-0017 §2.7 の一覧そのもの（`is_config_path` 1 つ）。M3 で入力の無い要素（保護パス、CI の読み込むファイル）は入力が入る MR で足す | §8 の 7 |
| 30 | MEDIUM: 代理の本人の結びつけ（計画 02 / ADR-0025） | 社員番号は Keycloak の claim から・一意、主張は `sub` と `via` に結ぶ、代理の読み取りの規則は ADR-0025 で決める、と記録 | §12 |
| 31 | LOW: 入力の検査 | source / target は作成でも変更でも `refs/heads/*` だけ。source == target は入力の検査と `authorize()` の arm の両方で拒む。UTF-8 でないパスは CODEOWNERS の判定で閉じる側（`changed_paths_unknown`） | §1、§9、§12 |
| 32 | 再レビュー HIGH（security）: 復元が権限の縮小と、個人のトークン以外の資格情報を戻してしまう | 復元はバックアップの外の監査の写しから、無効化・所属の削除・ロールの引き下げ・管理者の印の削除・保護 ref の強化を当て直す。写しが無ければ管理者の印をすべて外して一覧を出す。失効はセッション・個人のトークン・サービスアカウントのトークン（`ci/gates`・kanflow）・連携の秘密（Webhook の署名、代理の HMAC の鍵、ミラーの資格情報）の全種類。再発行の手順は runbook `backup-restore`。テスト `restore_does_not_resurrect_revoked_credentials`（名前は保つ）と `restore_reapplies_post_backup_role_changes` | ADR-0019 の Amendment（計画 11 P1）、計画 11 O1 |
| 33 | 再レビュー MEDIUM（authz NEW-1）: review の命令の名前が `decided_callers.rs` の needle に一致すると古いテストが落ちる | `open_mr_decided`・`edit_mr_decided`・`close_mr_decided`・`approve_mr_decided`・`merge_mr_decided` などの既存の needle に一致しない名前にする。古いテストは変えず、例外も足さない | §11 |
| 34 | 再レビュー MEDIUM（authz NEW-2）: マージの条件を変える設定がエージェント・サービスアカウントに開いている | `MergeSettingsManage`・`ApprovalRuleManage`・`CodeownersEnforcementToggle` は人だけ。エージェントとサービスアカウントの deny の行 | §8 の 4、§12 |
| 35 | 再レビュー LOW（authz）: `merge_request.approved` を数えた承認と誤読しうる | `counted: bool` と `uncounted_reason` を持たせ、購読者は `approved` を数えたものとして扱わない（マージできるかは `evaluate_merge()` の結果で読む） | §6 |
| 36 | 再レビュー MEDIUM（git）: 版の ref の競合 | 版の行を先に書いてコミットし、`v<n>` はコミットした行から導く。DB の行が主張しない `v<n>` はジョブが CAS で取り戻してよく、FG-SYS-0001 にしない | §2 |
| 37 | 再レビュー MEDIUM（git+security）: 到達の確認の費用 | 時間切れつき（時間切れ・busy は FG-GIT-0002 / 0004 で、届いたことにしない）、呼び出し元ごとの流量制限、`(repo, sha, refs の世代)` の短いキャッシュ、commit-graph を使ってよい。P6 に性能のテスト | §2、計画 11 §5.1 P6 |
| 38 | 再レビュー LOW（authz）: 代理の deny の行がいつ入るか、運用の例外の一覧の検査 | 代理の資格情報の種類がまだ無いので、代理の deny の行は ADR-0025 の MR でこの ADR のすべての Action に入れる。ゲート `authz-coverage` が運用の例外の一覧を確かめることはエージェントモデルへの申し送り | §12、未決 |
| 39 | 再レビュー LOW（git）: フェンスと再試行、遷移の条件、ソース削除のログ、衝突の一覧のあふれ、保守 | `update-ref` の直前に `now − executing_at` を照合の余裕と比べ、`update-ref` の時間切れは余裕より十分に短く。再試行はロックの下で行を読み直し `executing` のときだけ進む。tx1b の `queued → prepared` は `state='queued'` の条件つき。ソース削除は記録の前に `forge.review.merge_source_deleted_applied`、失敗は ref とエラーコードつきでログ。`merge-tree` の出力のあふれは衝突（衝突のない出力は 1 行）で 2.38 のテストに固定。計画の O3 に prune の猶予と「版の ref が gc の後も残る」テスト | §3、§4.2 の 5・6・10、計画 11 §5.5 O3 |
| 40 | 再レビュー LOW（security）: 版の ref の削除の前提、資格情報、コミッターの規則の副作用 | 開いている MR は先にクローズかソースの書き換え（前提を満たさなければ FG-REVIEW-仮（PurgePreconditionFailed）、runbook `secret-removal`）。`MergeRequestVersionRefsPurge` は `SessionOnly`（`admin` スコープのトークンも deny）。コミッターの規則の副作用（push の記録が保存期間を過ぎると止まる、別名でブランチを push したレビュアーはコミッターになる）は `uncounted_reason`（`committers_unknown` / `committer`）と runbook `merge-blocked` で見せる | §2、§8 の 3、§12 |
| 41 | 最終の再レビュー MEDIUM（security）: 復元でマージの条件の強化が戻る | 復元の当て直し（ADR-0019 Amendment の手順 2）は、監査の写しにある**権限や条件を狭める設定の変更をすべて**当て直す。少なくとも `protected_ref_merge_rules`・`approval_rules`・`merge_settings`・`codeowners_enforced`・必須チェック。`restore_reapplies_post_backup_role_changes` に含める（O1） | ADR-0019 Amendment |
| 42 | 最終の再レビュー MEDIUM（security）: Purge の後に版の ref が作り直される | `merge_request_versions` に削除済みの印 `refs_purged_at` を持つ。確かめ直すジョブは印のある版の ref を作らない。テスト `purged_version_refs_are_never_recreated`（R1b） | §2 |
| 43 | 最終の再レビュー LOW（security）: Purge 済みの MR の再オープン | Purge 済みの MR の再オープンは、ソースの head が Purge した版と違うときだけ許す（同じなら FG-REVIEW-仮（PurgePreconditionFailed））。runbook `secret-removal` にも書く（R1b） | §2、§12 |
| 44 | 最終の再レビュー LOW（security）: 管理者の印を外した後の戻し方 | runbook `backup-restore` に、`system` で印を付け直す `forge admin` のコマンドを名指しで書く（無ければ O1 で足す） | ADR-0019 Amendment |

## 実装で決めたこと（R1b `feat/core-merge-request-versions`、2026-11-02）

- **版を作るきっかけ**: `merge-request-refresh` の購読者（`forge-db/src/review/refresh.rs`、`independent`）が `repository.pushed` で
  ジョブ `merge_request.refresh` を **積むだけ**（dedup `mr_refresh:<mr_id>:push:<seq>`）。積む相手は、作成・fast-forward・force の
  `refs/heads/*` を source に持つ開いている MR。`refs_truncated` ならそのプロジェクトの開いている MR **すべて**。MR の作成
  （`open_mr_decided`、`mr_refresh:<id>:opened`）と再オープン（`reopened:<revision>`）も同じジョブを同じトランザクションで積む
  （最初の版もジョブが作る。git をトランザクションに入れない、が一貫する）。購読者は R1b で初めて本番の worker に登録される
  （`worker::registry()`、`Start::Head`）。
- **ジョブ**（`forge_review::versions::refresh`、worker の `RefreshHandler`）: source の先端を `for-each-ref` で読む → 版の行を先に
  コミット（`record_mr_version`、MR の行のロックの下で `max + 1`、監査 `merge_request.pushed` とイベント、actor `system`、
  `pushed_by` は push の actor）→ ref を同期。同じ head の再実行は何も書かない（2 つ目のイベントも出ない）。
- **同じ SHA に戻った**（A → B → A）: 一意 `(merge_request_id, head_sha)` があるので版は足さず、その版を head に戻す。
  監査だけの `merge_request.head_restored`（イベントは出さない。`merge_request_pushed:<id>:<sha>` は既にある）。
- **ref の同期**（`VersionRefs::sync_locked`）: ref を読んで 1 回の `update-ref` の transaction（`start` / `prepare` / `commit`、
  `core.hooksPath` は空のディレクトリ）。**MR ごとのロック**（`lock_version_refs`: `pg_advisory_xact_lock` を
  `merge_request_refs:<mr_id>` に。待ちは 10 秒まで（超えたら FG-DB-0002、ジョブは再試行）、持つのは 45 秒まで）の下で行う。
  版のジョブは先端の読み取り・版の行・同期の **全体** でロックを持つ（独立のレビューで MEDIUM: 2 つのジョブが交差すると、
  A が古い先端を読み、B が v3 を書いて同期した後で、A が `head` を戻しうる）。Purge は印のコミットの後の同期でロックを取る。
  Purge の印が同期の最中にコミットされても、その同期が書いた ref は Purge 自身の同期（ロックを待つ）が消し、以後の同期は印を読む。
  MR の行も版の行もロックしない（編集を git で待たせない）。git の呼び出しはどれも「ロックを持てる残りの時間」（最大 20 秒）で
  抑えるので、遅い git は FG-GIT-0002 になる。ref を書く直前にロックの接続が生きていることを確かめる（`RefsLock::alive`）。
  ロックの間は書き込みのプールの接続を 1 本（トランザクションの中で待機）、加えて短い文に 1 本を使う。worker はジョブを 1 つずつ
  実行し、serve は Purge の要求ごとに 1 回なので、同時に走る数 × 2 本をプールの大きさに見込む。拒まれた transaction（手で ref が
  動いた、ロックを失った）は warn で 1 回ログに出し（`forge.review.version_refs_retry`、`request_id`・`error_code`）、読み直して
  1 回だけ計画し直す。既に目的の値なら何もしない。
  行が主張しない値の `v<n>` は CAS で取り戻し `forge.review.version_ref_reclaimed`（info）。行を先に書く設計では `v<n>` を主張できる
  行は `n` の行だけなので、§2 の「別の行が主張していれば積み直す」は起こらない。Purge 済みの版の ref は消し、書かない。
  `refs/merge-requests/<iid>/` の下の他の名前（`merge` など）には触れない。
- **閲覧のキャッシュ**: 同期は `Browser` を持つとき（`forge serve` の Purge）`invalidate` を呼ぶ。worker のプロセスには閲覧の
  キャッシュが無く、隠した ref は閲覧の ref 一覧（`refs/heads/` `refs/tags/`）にも到達の確認（`--branches --tags`）にも入らないので、
  他のプロセスのキャッシュはこの ref に依存しない。`ReachRoots` に MR の ref は **足していない**（MR の画面は R2 以降）。
- **Purge**: `POST /api/v1/projects/{id}/merge-requests/{iid}/version-refs/purge`、本文 `{"versions": [n, …]}`（省略 = すべて）。
  開いている MR で今の head の版を含めば FG-REVIEW-0002 `purge_requires_closed_or_rewritten`。印（`refs_purged_at`、一度だけ。
  ガード `merge_request_versions_guard`）を先にコミットし、その後に ref を消す（消すのに失敗しても、もう一度頼めば消える）。
  Purge 済みの head に source が force push で戻っても head にしない（`purged_head`）。その事実は監査だけの行
  `merge_request.purged_head_pushed` と warn のログ `forge.review.purged_head_pushed` に残す（独立のレビュー）。このとき
  ブランチの先端と MR の記録した head は食い違うので、**R4 のマージは、ブランチの先端が記録した head と違えば拒む**
  （HeadChanged `source_moved`）。再オープンは source の先端が Purge 済みの
  版なら FG-REVIEW-0002 `reopen_at_purged_head`。gstn は `BrowserOnly`（`SessionOnly` でトークンを持つ gstn は届かない）。
- **途中の先端**: 2 回のジョブの間にだけ source が指したコミット（push の直後に force push で消えた、ジョブが先端を読む前）は
  版にならず、版の ref も持たない（ジョブは実行の時点の先端だけを読む）。レビューの対象は版になった head だけ。
- **扇の広がり**: 1 回の push で積むジョブは最大 500（最近更新された MR の順）。超えたら warn
  `forge.review.refresh_fanout_capped` を 1 回出し、残りはそれぞれの次の push・作成・再オープンで確かめ直す。
- **`deepen-not`**: upload-pack の要求の `deepen-not` が `merge-requests/` を含む名前（git の DWIM で隠した ref に解決される）なら
  FG-GIT-0014 `deepen_not_hidden` で拒む（v0 / v2）。
- **AccountKind**: `Principal` への `AccountKind` の追加は R5（承認）に送った。`MergeRequestVersionRefsPurge` は `SessionOnly` で、
  サービスアカウントとボットはセッションを持てない（サインインが `not_human` で拒む）ので、トークンの deny の行（`admin` スコープ、
  エージェントの `admin` トークンを含む）がそのまま彼らの deny になる。人だけの arm（`people_only`）も通す。

## 実装で決めたこと（R2 `feat/git-merge-request-diff-mergeability`、2026-11-02）

本文（§1・§3）と違えたところは **逸脱** と書いた。

- **状態の置き場（逸脱: §1 の `merge_requests.merge_status`）**: マージできるかは新しい **派生の表**
  `merge_request_mergeability`（`tables.toml` の `kind = "derived"`、MR ごとに 1 行、マイグレーション 20261108100000）に置き、
  `merge_requests` の列にしない。`merge_requests` の行は版の番号の守り（20261106100100: 変更のたびに `revision + 1`）と
  監査のトリガー（監査の行が要る）を持つので、ジョブと購読者が 1 つの MR に何度も書く状態には使えない。列は `status`
  （`unchecked` / `checking` / `can_be_merged` / `cannot_be_merged` / `check_failed`）、`reason`、`check_seq`、確かめた組
  `checked_head_sha` / `checked_target_sha` / `checked_at`、衝突のパス `conflicts`（`text[]`、最大 100 件・16 KiB、超えたら
  `conflicts_truncated`）、`last_decided`（最後の決定 can / cannot）。プロジェクトの purge で消す。監査の行が無い状態の変更なので、
  `ChangeTx` に `Settle::CommitDerived`（監査の行なしの COMMIT。監査つきの表を同じ tx で書けば遅延トリガーが COMMIT を拒むので、
  監査つきの変更を隠せない）を足した。
- **古い判定の守り（決定 2）**: 確かめ直しの要求ごとに `check_seq` を 1 つ進めて `unchecked` にし、ジョブ
  `merge_request.mergeability` の dedup の鍵を **`mr_mergeability:<mr_id>:<check_seq>`** にした（逸脱: §3 の
  `…:<head_sha>:<target_sha>`）。jobs の dedup の索引（20261027120000）は成功したジョブの鍵を残し続けるので、SHA の組を鍵にすると
  target が A → B → A と戻ったときの 3 回目が積めない。ジョブは開始時（`begin_check`）と書き込み時（`record_check`）の両方で、
  自分の seq が最新でなければ何も書かない（`superseded`。新しいジョブが決める）。版のジョブは、保存された状態が決まっていて
  （can / cannot / check_failed）確かめた組が今の (head, target の先端) と同じなら要求しない。
- **判定の中身**: 判定するのは **記録した head**（最新の版。ソースブランチが消えても隠した ref が持つ）と target の今の先端。
  版がない → `check_failed reason=no_version`、target が無い → `target_missing`、共通の履歴がない（`merge-base` が終了コード 1）→
  `unrelated_histories`（`merge-tree` は `--allow-unrelated-histories` なしで 128 を返す。どちらも「マージできる」にしない）、
  `merge-tree --write-tree --name-only -z` の 0 → `can_be_merged`、1（または出力のあふれ）→ `cannot_be_merged reason=conflict`。
  git の失敗は `check_failed reason=git_timeout|git_busy|git_failed` を保存してからジョブの失敗として返す（worker が 1 回ログに出し、
  再試行する）。成功は `forge.review.mergeability_checked`（info）にパスの一覧つきで 1 回出す。`merge-tree` の時間切れは **30 秒**、
  出力の上限は 256 KiB、一覧は 100 件（`forge_git::mergeability::MergeTreeLimits`）。
- **受け付ける終了コード（決定 3）**: `forge-git::command` の `capture` が受け付ける終了コードの組を取り、終了コードを返す
  （`output_accepting(cap, &[0, 1])`。`output()` / `output_truncated()` は `[0]` の包み）。シグナルの終了は受け付けない。
  上限で git を止めたときは終了コードが無く、`merge-tree` ではそれを衝突（`files_truncated`）とし、FG-GIT-0006 にしない。
  `merge_tree_exit_codes_on_git_2_38` が argv と終了コードを固定する。**ローカルの git は 2.53 で、2.38 そのものでは動かしていない**
  （使うオプションは 2.38 にある）。
- **版の base / merge-base（決定 4）**: 版のジョブが版を記録した直後に、その時の target の先端と merge-base を
  `record_version_bases` で **一度だけ**、2 つ一緒に書く（監査だけの `merge_request.version_bases_recorded`）。古い版は target が
  動いても書き換わらない（set-once の守りはそのまま）。target が無いときは書かず、次の版のジョブ（同じ head の再実行を含む）が埋める。
- **確かめ直しのきっかけ（決定 5・6）**: 作成・再オープン・ソースへの push は版のジョブが（上の組の比較のあとで）要求する。
  target の変更は編集のトランザクションの中で要求する（`unchecked` に戻る）。target への push と **削除** は
  `merge-request-refresh` 購読者が要求する（2 つ目の購読者は足していない）。削除は購読者がジョブを積み、ジョブが先端なしを見て
  `target_missing` を保存する。購読者は最近更新された順に `review.mergeability_fanout_max`（**既定 50**、1〜500）件まで積み、
  残りは `unchecked` にして seq を進めるだけ（マージは `mergeability_unknown` で止まる）、warn の
  `forge.review.mergeability_deferred` を 1 回出す。`refs_truncated` の push は target を特定できないので、版のジョブ（開いている MR
  すべて、500 件まで）の組の比較に任せる。**読み取りでは積まない**（逸脱: §3 の「MR の画面の表示・`gstn mr status` のときに積む」。
  上限を超えて `unchecked` になった MR は、次の push・target の変更・再オープンで確かめ直す。マージの要求のときに積むのは R4）。
  ジョブの同時実行は worker がジョブを 1 つずつ実行することで抑える（リポジトリごとの上限は worker の数を超えない）。
- **衝突のイベント（決定 8）**: `merge_request.conflict_detected` / `conflict_resolved`（integration）は、新しい決定が
  `last_decided`（前の決定）と can ⇄ cannot で違うときだけ出す。最初の決定、決まらない状態への・からの変化では出さない
  （can → check_failed → cannot は 1 回の変化として出す）。ジョブが `system` として監査の行と同じトランザクションで書き、
  dedup は `<prefix>:<mr_id>:<check_seq>`。ペイロードは ID・SHA・`conflict_files`（件数）・`files_truncated` で、パスはログと状態だけ。
  `MergeRequestTransition` に `ConflictDetect` / `ConflictResolve` を足し、`every_merge_request_transition_emits_exactly_one_event`
  が 1 件ずつを確かめる。
- **到達可能性（決定 9）**: `ReachRoots::merge_request(iid)`（`refs/merge-requests/<iid>/*`、その MR の ref だけ）と、公開の
  `Browser::merge_request_commit` / `merge_request_compare`（両端を呼び出し元ごとの上限つきで確かめてから `compare`）を足した。
  Purge 済みの版と存在しない版は、git を動かす前に同じ FG-REPO-0023 `ref_not_found` で答える。`merge-tree` / `commit-tree`
  だけが作ったオブジェクトは MR の文脈でも読めない（`merge_request_versions_are_browsable_only_in_their_merge_request`）。
  `forge serve` の到達の答えのキャッシュは worker が書いた新しい版の ref を最大 `refs_ttl`（5 秒）知らないことがある
  （「見つからない」側にだけずれる。Purge は serve が自分のキャッシュを捨てる）。
- **ルート（決定 10）**: どれも `GET`・`MergeRequestRead`（新しい Action なし）。`…/{iid}/versions`、`…/versions/{version}/diff`
  （`merge_base_sha..head_sha`。共通の履歴がない版は `base_sha..head_sha`、base が未記録なら FG-REPO-0023
  `version_base=pending`）、`…/compare?from=<版>&to=<版>`（`head..head` の二点）、`…/mergeability`。マージできるかは
  **別の GET** にした（MR の本文に入れると一覧と書き込みの応答が毎回別の表を読む。答えが MR と別に変わる）。差分とコミットの本文は
  M2 のもの（`routes/history.rs`）を使う。gstn は `mr versions` / `mr diff`（`--version N` か `--from N --to M`）/
  `mr mergeability`。
- **新しいエラーコードなし（決定 7）**: R2 は 409 を返さない（MergeRequestConflict は R4）。

R4 へ: マージの要求で確かめ直しを積む・ロックの下での `merge-tree` の再計算（この表の状態は表示と早めの拒否だけ）、
上限を超えて `unchecked` のままの MR の扱い。O3 へ: 判定のたびにできる参照されない木の、猶予つきの prune。

### R2 の独立レビューで決めたこと（2026-11-02）

独立レビュー（git-protocol・database・security・rust・silent-failure・pr-test）の HIGH 1 件と MEDIUM / LOW への対応。上の R2 の節と
違えたところは **改訂** と書いた。

- **`checking` のまま残らない（HIGH）**: `begin_check` の後にジョブが時間切れ・worker の停止・恒久的な失敗・デッドレターで終わると、
  行が `checking` のまま残りえた。(a) ジョブが最終的に失敗する経路（恒久的な失敗、または最後の試行の失敗）で、その seq がまだ最新なら
  `check_failed reason=job_failed` を書く（`forge_review::mergeability::fail`、`forge_db::review::fail_check`）。(b) worker の新しい
  タスク `mergeability_sweep`（30 秒ごと）が、ジョブの時間切れ + リースの猶予（`STALE_AFTER`）より長く `checking` で、生きている
  （queued / running の）ジョブが無い行を `check_failed reason=job_stale` にする（`reap_stale_checks`）。どちらも **新しいコード
  FG-REVIEW-0003** MergeabilityNotChecked（warn、runbook `merge-request-mergeability`）でログに出す（**改訂**: 上の「新しいエラー
  コードなし」。R2 は 409 を返さないが、決まらない判定をログと doctor で見分けるコードが要った。HTTP では返さない）。`forge doctor` に
  `review.mergeability`（古い `checking`・ジョブの無い `unchecked` の件数、FG-REVIEW-0003 `reason=undecided`）を足した。
- **古い決定を見せない（MEDIUM）**: 状態は `checked_head_sha` / `checked_target_sha` の組についての答えでしかない。新しい版を記録する
  トランザクションで、決まっていた状態を `unchecked` に下げる（`downgrade_on`。`checking` の行は seq を残し、その結果は head が
  違うので `record_check` が `Superseded` にする）。`GET …/mergeability` は、組が今の head と target の生きた先端（名前で ref の一覧から
  読む。到達の確認も呼び出し元の上限も使わない）と違えば `unchecked` と `stale: true` を返し、古い理由と衝突の一覧を出さない。
  **R4 のマージは、この表の状態を信じず、ロックの下で `checked_head` / `checked_target` を生きた先端と照らし直す**（さらに
  `merge-tree` を計算し直す。§4）。`record_check` は、MR が閉じた・head が `begin_check` の時と違う結果も書かない。
- **扇の上限の飢え（MEDIUM）**: 上限を超えて `unchecked` にした MR は、`mergeability_sweep` が生きているジョブの無い `unchecked` の
  開いている MR を、最後の確認が古い順（`checked_at` が無いものが先）に 1 回 50 件ずつ確かめ直す（**改訂**: 上の「次の push・target
  の変更・再オープンで確かめ直す」だけではない）。`defer_checks_on` は 1 回 5 000 件まで（`ORDER BY id` でロックの順を固定）。それも
  超えた分は状態を変えず（読む側の古さの規則が守る）、error で FG-REVIEW-0003 `reason=fanout_deferred` を 1 回出す。
- **答えになる `check_failed`（MEDIUM）**: 版のジョブが「保存済みの状態が今の組に答えている」とみなすのは、決定（can / cannot）と、
  同じ組でやり直しても同じになる理由（`target_missing`・`unrelated_histories`・`no_version`。`CheckReason::is_deterministic`）の
  `check_failed` だけ。`git_timeout` / `git_busy` / `git_failed` / `job_failed` / `job_stale` は確かめ直す。理由は `CheckReason` の enum。
- **参照されないオブジェクト（MEDIUM）**: 判定の木・blob を消す仕組みはまだ無い（O3 で入れる予定）。runbook
  `merge-request-mergeability` にディスクの増加、承認つきの手での `git prune --expire=<猶予>`、秘密の除去の後も判定が書いた blob が残る
  ことを書き、計画 11 §9 に「O3 がマージされるまで M3 を完了にしない」を足した。
- **小さな修正（LOW）**: 確かめ直しの要求で衝突の一覧を空にする（`checked_*` は最後に終わった確認として残す）、確認した head の版を
  Purge したら状態を `unchecked` に戻し一覧を空にする、ログには保存した（上限つきの）パスと全体の件数だけを出す、`merge-tree` の
  出力が一覧の終わりの後で切れたときは一覧を切れたとしない、`enqueue_on` が重複で入れなかったら warn、決まらない結果は warn で出す。
- **再レビュー（2026-11-02）**: 決まっている状態（can / cannot、決まった理由の `check_failed`）の組が生きた組と違うまま（push の
  イベントのデッドレター、繰り延べの上限 5 000 件の超過）だと、`stale` と表示され続けて誰も確かめ直さなかった。`mergeability_sweep` が
  1 回 50 件ずつ、最後に確かめた時刻（新しい列 `confirmed_at`、追記のマイグレーション 20261108100100。無ければ `checked_at`）の古い順に、
  target の生きた先端を git で読んで組を比べ、動いていれば確かめ直しを要求し、生きていれば `confirmed_at` を進める（git の失敗は
  その行を飛ばし warn。生きているとはみなさない）。doctor の `review.mergeability` は、head が違う決定の件数と、1 時間確かめられていない
  決定の件数も数える。`record_check` が「確認の途中で MR が閉じた」ために書かないときは、行を `unchecked`（ジョブなし）に戻し、
  見回りの `job_stale`（と偽の FG-REVIEW-0003）を出さない。見回りは開いている MR の行だけを終わらせる。

## 実装で決めたこと（R3 `feat/core-merge-request-discussions`、2026-11-02）

本文（§6・§8 の 4・§12）と違えたところは **逸脱** と書いた。

- **表**（追記のマイグレーション 20261109100000）: `merge_request_discussions`（`kind` = `general` / `inline`、inline は
  `version_no`・`path`・`side`（`old` = 版の merge base、`new` = 版の head）・`line` をすべて持ち、general はすべて持たない
  （CHECK）。`(merge_request_id, version_no)` は版の表への複合の外部キー。`started_by`、`resolved` / `resolved_by` /
  `resolved_at`、`revision`（作成で 1、解決・解決の取り消しごとに + 1））と `merge_request_notes`（`author_id`、本文 1 文字〜
  64 KiB、`created_at`）。どちらもプロジェクトに監査される状態（`tables.toml` の `audited`、resource `project`）。ガード
  `merge_request_discussions_guard`（スレッドの同一性と位置は変わらない、変更ごとに `revision + 1`）と
  `merge_request_notes_guard`（コメントの UPDATE をすべて拒む）を guarded object と doctor の期待値に足した。プロジェクトの
  purge はコメント → スレッド → 版の順に消す。
- **コメントの編集は R3 に入れない**: 編集の権限（`NoteEdit`）・履歴・監査の形を決める前に入れると、表と Action を後で
  作り直すことになる。コメントは不変（ガードで固定）。編集の列は入れる MR で追記のマイグレーションで足す。
- **イベント**: §6 の表のとおり 1 つの型 `merge_request.commented` で、`data.action` は `note_added` / `thread_resolved` /
  `thread_unresolved`（§6 の値をそのまま使う。新しいスレッドか返信かは `discussion_started: bool` で見分ける）。ペイロードは
  ID・`kind`・フラグだけで、本文とパスは入れない（衝突のイベントと同じ）。dedup の鍵はサーバーの ID だけ:
  `merge_request_note_added:<note_id>`、`merge_request_thread_resolved|unresolved:<discussion_id>:<revision>`（解決 →
  取り消し → 解決が別のイベントになる）。`MergeRequestTransition::Comment` を 1 つ足し、
  `every_merge_request_transition_emits_exactly_one_event` に含めた。既に同じ状態への解決・取り消しは何も書かない
  （`ResolveOutcome::Unchanged`、イベントなし）。返信はスレッドの解決の状態を変えない。
- **位置の検査**: inline の位置は、トランザクションの前に forge-review が **その版の差分**（`merge_base..head`。共通の履歴が
  なければ `base..head`。R2 の `merge_request_compare` を `DiffProbe` 越しに使い、新しい git の呼び出しはない）に対して確かめる。
  行は、その側にそのパスを持つファイルの **hunk の中**（変更の行と文脈の行。差分に見える行）でなければならない。理由は
  `version_unknown` / `version_purged` / `version_base_pending` / `path_not_in_diff` / `diff_truncated`（上限で切れた差分に
  無い: 有ると示せないので拒む）/ `no_patch`（バイナリ・上限で省かれたパッチ）/ `line_out_of_range`。**FG-REVIEW-0004**
  DiscussionPositionInvalid（422、info）。ログにはパスと本文を出さない。外部キーが、版の行が無いときの最後の守り（同じ
  FG-REVIEW-0004 `version_unknown`）。
- **`outdated` は読むときに計算する**（記録しない）: 版を記録するジョブで書くと、push のたびにすべてのスレッドの行を
  書き換え、ジョブに git の差分を足すことになる（版のジョブは MR ごとのロックを持つので重くしたくない）。読むときは、
  スレッドの側のコミット（`new` = その版の head、`old` = その版の merge base）から、今の版（MR の `head_sha` の版）の同じ側の
  コミットへの差分を 1 回だけ取り（同じ組はキャッシュ）、その行が `-` の行か、ファイルが削除・名前の変更・種類の変更か、
  行ごとに読めない（バイナリ・省かれたパッチ・切れた差分に隠れうる）なら `outdated: true`。Purge 済みの版・base の未記録も
  `true`（見直しを促す側に倒す）。差分の取得の失敗は読み取りの失敗にする（「古くない」にしない）。今の版のスレッドと
  general のスレッドは常に `false`。
- **権限**（§12）: `MergeRequestComment` は `Resource::MergeRequest` で判定し、**所属のロールで Reporter 以上**（逸脱ではないが
  解釈: internal のプロジェクトの所属していない利用者は読めるがコメントできない。書き込みはどれも所属が要る、に揃えた）。
  エージェントのトークンとサービスアカウントは可。`DiscussionResolve` は `Resource::Discussion { project, mr_author,
  started_by }`（**逸脱**: §12 の「MR と同じ形」ではなく、判定に使う 3 つだけ。MR の状態は入力にしない — どの状態でも
  解決できる）。人だけ → 自分が始めたスレッドは Reporter 以上 → 他人のスレッドは MR の作成者なら `author_foreign_discussion`
  （管理者でも）→ Developer 以上。返信は MR で判定し、スレッドがその MR に無ければ判定の後で FG-REPO-0005
  `resource=discussion`（判定の前に存在を漏らさない）。
- **AccountKind**（§8 の 4。R1b で R5 に送ったものを R3 で入れた）: `forge_auth::AccountKind` は forge-core の `UserKind` の
  再輸出。`Principal::authenticated` の 5 つ目の引数（既定値なし）。トークンとセッションの認証が `users.kind` を読み
  （`TokenIdentity.kind` / `SessionIdentity.kind`、読めない値は FG-SYS-0001 `unknown_user_kind`）、`reconfirm` は種類を
  そのまま保つ（変わらない値）。**人だけの arm は既存の人だけの Action すべてで** サービスアカウント（`service_account`）と
  ボット（`bot`）も拒む（閉じる側。それまで種類は判定の入力でなかった）。行列の行に `ServiceAccountTokenApi` /
  `BotTokenApi` を足した。行は `matrix_rows_discussion.rs`（`ROWS` は基本の行とこの行を合わせた一覧）。
- **ルート**: `GET|POST …/{iid}/discussions`、`POST …/discussions/{discussion_id}/notes`、`POST …/resolve` / `…/unresolve`
  （閉じる・再オープンと同じく POST）。本文の上限はレジストリの JSON の上限 64 KiB（MR の説明と同じく、上限ちょうどの
  コメントが JSON のエスケープで超えると FG-WEB-0009 RequestTooLarge）。一覧は 500 スレッド・5 000 コメントまで（`truncated`）。本文は
  書かれたまま保存して返し、サーバーは描画しない（画面が安全に描画する、U1）。gstn は `mr discussions` / `mr comment`
  （`--discussion` で返信、`--version --path --line [--side]` で行）/ `mr resolve [--unresolve]`。

### R3 の独立レビュー（1 回目）で決めたこと（2026-11-02）

上の R3 の節と違えたところは **改訂** と書いた。

- **人だけの Action の行（HIGH）**: 人だけの arm が既存の人だけの Action でもサービスアカウントとボットを拒むようになったのに、
  行は `DiscussionResolve` にしか無かった（arm を消しても行列が赤くならない）。`Action::is_people_only` に一覧を置き、各 Action に
  サービスアカウントとボットの deny の行を置いた（`matrix_rows_people.rs`）。どの行も、同じロール・スコープ・管理者の印の **人**
  （`Actor::human_twin`）なら allow であることをテストで確かめ（拒否の理由が種類だけであること）、逆に人だけでない Action の
  allow の行は種類を変えても allow であることを確かめる。ボットは自分のロールの範囲でコメントできる（allow の行）。
- **書き込みの応答で失敗しない（HIGH、改訂）**: 書き込みのルートはコミットの後に `outdated` の差分を取っていたため、git の失敗
  （busy・時間切れ・届かないコミット）で保存済みのコメントに 4xx / 5xx を返し、再試行で重複を招いた。`outdated` を
  `Outdated::{No, Yes, Unknown}` にし、計算できないもの（今の版が無い、Purge 済みの版、base の未記録、1 回の読み取りで
  **20 組**（`MAX_DIFFS_PER_READ`）を超える差分、差分の失敗、書き込みの後の版の読み取りの失敗）は `Unknown`。API では
  `outdated: true` と `outdated_unknown: true`。読み取りごとに warn `forge.review.discussion_outdated_unknown` を 1 回
  （件数・理由・最初の差分の失敗の `error_code`）。**一覧の読み取りも差分の失敗で失敗させない**（上の「差分の取得の失敗は
  読み取りの失敗にする」を改訂。DB の読み取りの失敗だけが失敗）。今の版は MR の `head_sha` の版で、無ければ `Unknown`
  （「古くない」にしない）。
- **上限（FG-REVIEW-0005 DiscussionLimitReached、409）**: MR ごとに 500 スレッド・5 000 コメント、スレッドごとに 200 コメント。
  MR の行のロックの下で数える（書き込みはどれもこのロックを持つ）。一覧の上限と同じなので、一覧が切れることは無い。
  1 スレッドのコメントの読み取りにも LIMIT。
- **コメントの上限を 60 KiB に（改訂）**: リクエストの JSON の上限が 64 KiB なので、64 KiB のコメントは送れなかった。規則が
  受け付けるコメントは、包みと 4 KiB のパスを足しても上限に収まる（表は 64 KiB のまま、最後の守り）。
- **合成の外部キー**（追記のマイグレーション 20261109100100）: スレッドの `(merge_request_id, project_id)` →
  `merge_requests (id, project_id)`、コメントの `(discussion_id, merge_request_id, project_id)` → スレッドの同じ組。
- **クローズ・マージ済みの MR**: コメントも解決もできる（解決は記録の整理で、マージを止めるのは `evaluate_merge()`）。
  `Resource::Discussion` は MR の状態を入力にしないので行列の行では表せず、HTTP のテストで固定した。
- **コピー**: old 側の位置はコピーの項目に解決しない（コピーの元は変わらずに残っている）。
- テスト: `check_inline` の理由（`version_base_pending`・`version_purged`・`no_patch`（バイナリ）・`diff_truncated`（ファイル数の
  上限））を実リポジトリで、本文の規則・別の MR / 別のプロジェクトのスレッドの ID（同じ 404、何も書かない）・サービス
  アカウントの解決の拒否・同じ解決の繰り返し（イベントは増えない）・hunk の境界・gstn の終了コードを HTTP / CLI で。
