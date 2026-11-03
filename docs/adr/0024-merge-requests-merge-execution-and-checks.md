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

予約するコード。**番号は登録の順に決める**（計画 11 §11.4）。R1a で登録した DuplicateOpenMergeRequest が **FG-REVIEW-0001**、R1b で登録した PurgePreconditionFailed が **FG-REVIEW-0002**、R2 のレビューで登録した MergeabilityNotChecked が **FG-REVIEW-0003**、R3 で登録した DiscussionPositionInvalid が **FG-REVIEW-0004**、R3 のレビューで登録した DiscussionLimitReached が **FG-REVIEW-0005**、R4 で登録した MergeRequestConflict / MergeBlocked / HeadChanged / MergeExecutionFailed / MergeMethodNotAllowed / MergeInProgress が **FG-REVIEW-0006〜0011**、R4 のレビューの MergeIdentityInvalid が **FG-REVIEW-0012**、R5 で登録した SelfApprovalRefused / CodeownersInvalid / ApprovalLimitReached が **FG-REVIEW-0013〜0015**、R6 で登録した RequiredCheckNamespaceNotAllowed が **FG-REVIEW-0016**（と FG-GIT-0017 ProtectedRefDirectPushForbidden）で、ほかはすべて仮（下の表と本文の「FG-REVIEW-仮」）。番号は各 MR が errors.toml に登録したときに確定し、そのときにこの表を直す（R1a のレビューで改訂）:

| コード | 名前 | HTTP | 重さ | runbook | 入れる MR |
|---|---|---|---|---|---|
| FG-REVIEW-0006（R4 で登録） | MergeRequestConflict | 409 | info | — | R4 |
| FG-REVIEW-0007（R4 で登録） | MergeBlocked（`reasons`） | 409 | info | — | R4（理由は R5 / R6 で増える） |
| FG-REVIEW-0008（R4 で登録） | HeadChanged（`stale_view` / `source_moved` / `target_moved`） | 409 | info | — | R4 |
| FG-REVIEW-0009（R4 で登録） | MergeExecutionFailed（`update_ref_failed` / `record_failed` / `ref_state_unknown`） | 500 | **alert** | `merge-execution`（R4 で作成） | R4 |
| FG-REVIEW-0001（R1a で登録済み） | DuplicateOpenMergeRequest | 409 | info | — | R1 |
| FG-REVIEW-0013（R5 で登録） | SelfApprovalRefused | 403 | info | — | R5 |
| FG-REVIEW-0014（R5 で登録） | CodeownersInvalid | 422 | warn | `codeowners`（R5 で作成） | R5 |
| FG-REVIEW-0015（R5 で登録） | ApprovalLimitReached（MR ごとの生きた承認 50 件。版の記録の取り消しが 1 つの変更のイベント 100 件に収まるように） | 409 | info | — | R5 |
| FG-REVIEW-0004（R3 で登録） | DiscussionPositionInvalid | 422 | info | — | R3 |
| FG-REVIEW-0005（R3 のレビューで登録） | DiscussionLimitReached（スレッド・コメントの上限） | 409 | info | — | R3 |
| FG-REVIEW-0010（R4 で登録） | MergeMethodNotAllowed | 409 | info | — | R4 |
| 仮（登録の順に決める） | CheckNamespaceDenied | 403 | info | — | C1 |
| 仮（登録の順に決める） | CheckPayloadInvalid | 422 | info | — | C1 |
| FG-REVIEW-0011（R4 で登録） | MergeInProgress（この ADR で追加） | 409 | info | — | R4 |
| FG-REVIEW-0016（R6 で登録） | RequiredCheckNamespaceNotAllowed（設計レビューで追加。`ext/` などを必須のチェックにする設定） | 422 | info | — | R6 |
| FG-GIT-0017（R6 で登録） | ProtectedRefDirectPushForbidden（保護ブランチのマージの条件の直接の push の禁止。管理者も） | 403 | info | — | R6 |
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

- ~~push の経路の pre-receive / post-receive のフックを M3 のどの MR で入れるか~~ → **R4 で決めた**（末尾の「実装で決めたこと（R4）」）:
  マージの経路はフックに頼らず、ロックの下で保護 ref の判定と生きた先端を照らし直す。push の経路のフックは X8（M3 後半）に残す。
- ~~CODEOWNERS の大きさの上限の既定値（R5 で決める）~~ → **R5 で決めた**（64 KiB・1 000 行・1 行 4 KiB・1 行の所有者 50。末尾の R5 の節）。マージのジョブの同時実行は R4 で「`forge serve` の 1 つのランナーで 1 件ずつ、
  リポジトリごとのロックで直列」に決めた（末尾の R4 の節）。`merge-tree` の時間切れは R2 で 30 秒に決めた（末尾の R2 の節）。
- 変更されたパスの件数の上限と時間切れは **R5 で決めた**（10 000 件・git の呼び出しごとに 10 秒）。`codeowners_enforced_off_until` の既定と上限
  は **R6 で決めた**（上限 24 時間・1〜1440 分、既定は置かず期限の指定を必須にした。末尾の R6 の節）。保守の prune の猶予（24 時間以上）は O3。`review.mergeability_fanout_max` は R2 で既定 50・上限 500 に決めた。
- **リポジトリの ref の数の上限**（R5 の再レビュー 6 回目からの申し送り、O シリーズ）: push のたびに全 ref の先端を読む
  （`ref_snapshot`、上限 1 000 000 ref で失敗 = その push の一覧は不完全）。ref が N 本を超えるリポジトリへの push を拒む上限と、
  N に近づいたら知らせる doctor の warn を O シリーズで入れる（N は運用の実測で決める）。R5 では実装しない。
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

### R3 の再レビューで決めたこと（2026-11-02）

- **`outdated` の時間の予算**: 1 回の読み取り（と書き込みの応答の表示）が `outdated` の差分に使う時間は合計 **8 秒**
  （`OUTDATED_BUDGET`、要求の時間切れ 30 秒より十分短く、書き込みの経路の余裕を残す）。期限を過ぎたら残りの組は git を
  動かさずに `Unknown`（`reason=deadline`）、実行中の差分は期限で打ち切る。古い版への行のコメントの書き込みも、コミットの
  後に要求の時間切れを超えない。
- **ログの重さ**: `version_purged` / `version_base_pending` は日常の状態なので debug（`error_code` なし）。warn は診断の
  失敗だけで、`diff_failed` / `versions_unreadable` は `error_code` と `error_reason`（失敗の context の `reason`）を持つ。
  `deadline` / `diff_cap` / `no_current_version` / `version_unknown` は warn で `error_code` なし。
- **既にデータのある表への UNIQUE の追加**: 20261109100100 の `merge_requests_id_project` は `ALTER TABLE … ADD CONSTRAINT …
  UNIQUE` で入れた（R3 の時点では行が少ない）。今後、行の多い表に UNIQUE を足すときは、書き込みを止めないように
  `CREATE UNIQUE INDEX CONCURRENTLY`（トランザクションの外の別のマイグレーション）で索引を作り、`ALTER TABLE … ADD
  CONSTRAINT … UNIQUE USING INDEX` で制約にする。

## 実装で決めたこと（R4 `feat/git-merge-execution`、2026-11-02）

本文（§4・§7・§8 の 5・§12）と違えたところは **逸脱** と書いた。

- **フックに頼らない（計画 11 の決定 12 を R4 で決めた）**: マージの経路は pre-receive / post-receive のフックを通らず、それに頼らない。
  ロックの下で、push の経路と同じ保護 ref の判定（`authorize()` が target の `RefProtection` で `MergeRequestMerge` を判定。規則は
  push と同じ `protected_refs` の行をトランザクションの中で読む）と、記録した head・ソースの先端・target の先端を生きた値と照らし直す。
  `update-ref` と `commit-tree` は `core.hooksPath` を空のディレクトリに固定する。push の経路の pre-receive / post-receive のフックは
  M3 後半の X8 に残す（上の「未決」から外した）。
- **表**（追記のマイグレーション 20261110100000）: `merge_request_merges`（1 試行 1 行、`id` が `merge_id`）。**派生の表**
  （`tables.toml` の `kind = "derived"`）にした: ジョブの運用の記録で、人の判断はマージが着地したとき（tx2）の
  `merge_request.merged`（プロジェクトの監査の行）とソース削除の `merge_request.source_branch_deleted` に残る。状態機械は
  ガード `merge_request_merges_guard`（`MergeState::may_become` と同じ遷移。終端は変わらない、`expected_old` / `new_sha` /
  `executing_at` / `source_outcome` は一度だけ、試行の同一性は変わらない）。実行中の試行は MR ごとに 1 つ（部分一意索引
  `merge_request_merges_one_live`）。プロジェクトの purge で消す。§4.2 の状態 `applied` の名前はそのまま。
- **スコープ**: `Scope::MergeExecute`（`merge_execute`）と CHECK `tokens_scopes_known` の拡張を同じ MR で入れた。`satisfies` は
  既存の規則（同じスコープか、`write_repository`→`read_repository` / `api`→`read_api` の含意）のままで「マージのスコープは自分だけが
  満たす」が成り立つ（`merge_scopes_are_satisfied_only_by_themselves`）。`merge_approve` は R5。
- **権限**: `MergeRequestMerge`（人だけ。`Action::is_people_only` に足したので、サービスアカウントとボットの deny の行は
  `matrix_rows_people.rs`、`merge_execute` のトークンの人の双子で allow を確かめる）。必要なロールは target の
  `push_min_role` と Developer の大きい方（保護なしは Developer）。状態・ドラフトは見ない。行は `matrix_rows_merge.rs`
  （Developer × Maintainer の保護は deny、Maintainer × Owner の保護は deny、管理者 allow、エージェントの `merge_execute` は
  `agent_token`、`api` だけ・`admin` だけ・`write_repository` だけのトークンは FG-AUTH-0002）。`merge_approve` だけのトークンの
  行は R5（スコープが無い）。`Resource::MergeRequest` に `target_protection` を足し、`MergeRequestCtx::merging` だけが入れる。
- **入口**（`POST …/{iid}/merge` → 202、`GET …/{iid}/merge` → 最新の試行）: 本文は `sha`（必須）・`method`・
  `delete_source_branch`・`message`。順に、方式（FG-REVIEW-0010）→ メッセージ（FG-WEB-0006）→ `sha` と MR の head（FG-REVIEW-0008
  `stale_view`）→ ソースと target の生きた先端（閲覧の API の ref 一覧。新しい git の呼び出しはない）→ レビューのロックの下で
  再判定（ルートのローダーが読んだ保護ではなく、トランザクションの中で読み直した規則で `reconfirm`）→ 実行中の試行（FG-REVIEW-0011）
  → `evaluate_merge()`（FG-REVIEW-0007、`reasons` はすべて）。`mergeability_unknown` のとき、確認が走っていなければ確かめ直しを
  積む（R2 の「マージの要求のときに積むのは R4」）。資格情報は種類と ID だけを残す（セッションは `web_sessions.id`、トークンは
  `tokens.id`）。本文の上限は登録の JSON の上限 64 KiB（64 KiB ちょうどのメッセージはエスケープで超えうる。説明と同じ）。
- **`evaluate_merge()`**（§7）: R4 の入力にある理由だけを持つ（`not_open`・`draft`・`merge_in_progress`・
  `source_branch_missing`・`mergeability_unknown`・`conflict`・`ff_not_possible`）。承認・CODEOWNERS（R5）、必須チェック・未解決の
  スレッド（R6）は入力と一緒に変種を足す。保存された判定は、確かめた組が生きた (head, target の先端) と同じときだけ数える
  （`stored_mergeable`）。ジョブの tx1a は判定を `None`（ロックの下で自分の試しのマージで決める）、tx1b は `Clean`。
- **ジョブの置き場（逸脱: §4.2 は「ジョブ」とだけ書き、ADR-0016 §7 のジョブは worker が走らせる）**: `merge_request.merge` は
  **`forge serve` が走らせる**（`forge-cli/src/serve_merges.rs`。`JobRunner` に 1 種類だけ登録し、1 秒ごと・ジョブの後はすぐ）。
  マージの ref の書き込みが、次の要求が読む閲覧のキャッシュ（`Browser`、ref 一覧と到達の答え）と同じプロセスで `invalidate` を
  呼べるようにするため（worker には閲覧のキャッシュが無く、別のプロセスのキャッシュは捨てられない）。worker はこの種類を登録しない
  ので取らない。リースの切れたジョブは worker の reaper が戻す。停止のときは実行中のマージに 10 秒、その後ジョブを返す。
- **主体の作り直し**（§5）: ジョブは試行の行の利用者 ID と資格情報（種類と ID）から、ロックの下で利用者の有効・管理者・種類と、
  トークンのスコープ・`is_agent`・失効・期限（セッションは失効・期限）を読み、`Principal` を作り直して `prove` する。無い・失効・
  期限切れは FG-AUTH-0003 `credential_revoked`（試行は `superseded`）。
- **フェンスと窓**: リポジトリごとのロック `merge:<repo_id>`（`pg_advisory_xact_lock`、待ち 30 秒（超えたら FG-DB-0002 で
  ジョブの再試行）、保持は最大 180 秒）。ref を動かす直前に (a) ロックの接続で `pg_locks` の自分の pid のロックを確かめる、
  (b) `prepared → executing`（条件つき UPDATE）、(c) `now − executing_at`（DB の時計）が「照合の余裕 − `update-ref` の時間切れ」
  （既定 600 − 30 = 570 秒）を超えていれば `update-ref` を起動しない（照合に任せる）。既定値: `update-ref` 30 秒、`commit-tree`
  30 秒、ref の読み取り・`merge-base` 10 秒、`merge-tree` 30 秒（R2 と同じ、ロックの残りで抑える）、照合の余裕 10 分、
  `queued` / `prepared` の放置 10 分。`余裕 > update-ref の時間切れ × 2` を起動時に確かめる（FG-SYS-0002
  `review.merge_grace below_update_ref`）。
- **CAS の負けの分類**（§4.2 の 7）: `update-ref` の失敗の後に ref を読み直す。`new_sha` なら記録へ（前の試行か git が動かした）、
  `expected_old` 以外なら `superseded`（FG-REVIEW-0008 `target_moved`、alert にしない）、`expected_old` のままで `<ref>.lock` が
  あれば再試行（`forge.review.merge_retry reason=ref_locked`）、それ以外は FG-REVIEW-0009 `update_ref_failed`（行は
  `executing` のまま、ジョブの再試行は git に先に聞いてから、窓の中なら決め直して動かす）。`update-ref` の後は成否にかかわらず
  `invalidate` を呼ぶ。
- **照合**: `forge serve` が 60 秒ごとに `Merger::reconcile`（1 回 50 件まで）。ジョブと同じロックの下で行を読み直し、`queued` /
  `prepared` で 10 分更新の無いものは `superseded`（`reason=stale`）、`executing` で `executing_at` から 10 分 30 秒を過ぎたものは
  git に聞く（`new_sha` か、`new_sha` を祖先に持つ → tx2 で `applied`。`expected_old` → `superseded` `not_applied`。どちらでもない
  → `failed`、FG-REVIEW-0009 `ref_state_unknown`）。§4.2 の照合のジョブ `merge_request.reconcile` は **作らなかった**（逸脱:
  サーブの定期の処理にした。ロックの待ちが短く、空振りのジョブの行を増やさないため）。最後の試行で失敗したジョブは、フェンスを
  通っていない試行を `superseded reason=job_failed`（原因のコード）にする（`Merger::give_up`）。
- **コミット**: マージコミットの作成者とコミッターはマージした人、squash の作成者は MR の作成者・コミッターはマージした人（§4.1 の
  表のとおり。forge のシステムの ID は使わない）。どれも DB の表示名とメール（クライアントの入力ではない）。メールの無い
  アカウントは `<username>@users.noreply.invalid`。`commit-tree` の引数は `-p <id>… -F - --end-of-options <tree>`（2.38 で
  parse-options のコマンドなので区切りを受け付ける。手元の git は 2.53）。既定のメッセージはマージコミットが
  `Merge branch '<source>' into '<target>'` + タイトル + `Merge request !<iid>` + 説明、squash はタイトル + 説明（64 KiB で切る）。
- **記録**（tx2）: `merge_request.merged`（監査の行が先）と target の `repository.pushed`（影響の行）、イベント 2 つ
  （`merge_request_merged:<merge_id>`、`merge_pushed:<merge_id>`）。actor は要求者（資格情報の ID と、トークンならスコープ）。
  push の記録のコミットの一覧は push と同じ `new_commits`（失敗は一覧なし・`commits_truncated`、warn）。ソースの削除は
  記録の後に自分のトランザクションで `merge_request.source_branch_deleted` と削除の `repository.pushed`
  （`merge_source_deleted:<merge_id>`、`merge_source_pushed:<merge_id>`。§4.2 の 10 の鍵は 2 つのイベントに分けた）。
- **ソースブランチの削除**: 要求者の主体で `RepoPush`（`authorize_ref_updates`、`RefUpdate::Delete`、push と同じ規則）を判定し、
  ソースの先端がマージした head のときだけ CAS で消す。保護・ロール・スコープ（`merge_execute` だけのトークンは
  `write_repository` が無いので消さない）で断られたら `kept_refused`、先端が動いていたら `kept_moved`、失敗は `kept_failed`
  （warn）。マージは取り消さない。
  tx2 とソースの削除の間で止まったジョブは、再試行（試行が既に `applied` でも `source_outcome` が空なら）と照合（`applied` を記録した直後）が削除を続ける。
- **マージの実行中の拒否**: クローズ、target の変更、ドラフトの切り替えは MR の行のロックの下で生きた試行を見て
  FG-REVIEW-0011。タイトル・説明の変更は断らない（試行の入力ではない）。
- **プロジェクトの `merge_settings`（逸脱: 計画 11 §4.1 は R4 に表を置く）**: R4 では表を作らず、すべてのプロジェクトで ADR の
  既定（マージコミットと squash を許可、既定はマージコミット、ソースは残す）を使う（`forge_review::MergeSettings`）。設定を書く
  `MergeSettingsManage` と表は、最初の書き手である R6（マージの条件の設定）で一緒に入れる（書き手の無い表を作らない）。
  それまで `fast_forward` は FG-REVIEW-0010。
- **新しいコード**（登録の順）: FG-REVIEW-0006 MergeRequestConflict、0007 MergeBlocked、0008 HeadChanged、0009
  MergeExecutionFailed（alert、runbook `merge-execution`）、0010 MergeMethodNotAllowed、0011 MergeInProgress。`gstn` の
  `--wait` の時間切れに FG-CLI-0008 CliWaitTimedOut（再試行できる、終了コード 6）。
- **gstn**: `mr merge --sha <head> [--method] [--delete-source-branch] [--message] [--wait [--wait-timeout]]` と
  `mr merge-status [--wait]`。`--wait` は 1 秒ごとに `GET …/merge` を読み、`superseded` / `failed` はその試行のコードと
  マージの要求の `request_id` で終わる。`--sha` は必須（見ていない版をマージさせない。§4.2 の 1）。
- **テスト**: 実際の git（`forge-review/tests/merges_git.rs`）で 3 方式の結果の木、衝突、同時の push での CAS の負け、同時の 2 つの
  マージ、保護の引き上げ・トークンの失効のジョブでの拒否、ソースの削除（保護ありは残す）、時間切れの後始末、照合の 3 つの場合、
  フェンスの後の再試行。閲覧が古い答えを返さないこと（マージとソース削除の直後の ref 一覧）。forge-db（`merge_tests.rs`）で
  同時の要求、ガード、フェンス、ロック、資格情報の読み直し。forge-git（`commit_tests.rs`）で `merge_plumbing_argv_on_git_2_38`。

### R4 の独立レビュー（1 回目）で決めたこと（2026-11-03）

評価役（git-protocol・security・database・rust・silent-failure）の HIGH 4 件と MEDIUM / LOW への対応。上の R4 の節と違えたところは
**改訂** と書いた。

- **照合はロックの取り合いだけを飛ばす（HIGH）**: マージのロックの待ちが尽きたこと（`55P03`、または自前の上限）は FG-DB-0002
  `reason=merge_lock_busy`（`is_lock_busy`）として他の失敗と分けた。照合はそれだけを次の回へ回し、接続の喪失などほかの失敗は
  エラーとして返して `error_code` つきで 1 回ログに出す。どちらの場合も試行は次の走査の後ろへ回す（追記のマイグレーション
  20261110100100 の `reconcile_seen_at`。走査は `reconcile_seen_at NULLS FIRST, id` の順で、片付けられない数件が残りを止めない）。
  1 回の走査は 45 秒まで。照合が `end_merge` に負けた（ほかが先に片付けた）ときは alert を出さない。`applied` を記録した後に試行を
  読み直せなければ、マージは `applied` のまま warn を出す。
- **serve のマージのタスクを見張る（HIGH、改訂）**: ジョブのランナーと照合を **別のタスク** にし（遅い照合がマージを待たせない）、
  それぞれを監督するタスクが、停止の要求なしに終わった・panic したことを FG-SYS-0005 ServeFailed（`operation=merge_task_exited`、
  `task`、`panicked`）で 1 回ログに出し、1 秒から倍々で 30 秒までの間隔で起動し直す。停止のときは両方に止まれと伝え、panic した・
  時間内に終わらなかった監督のタスクもログに出す。照合の 1 回は 50 秒で打ち切り、停止の要求ですぐ止まる。毎回ハートビート（2 回目で info に改訂） `forge.serve.merge_reconcile_pass`（件数つき）を出す。ジョブを取れない失敗は 1 秒から 30 秒まで間隔を延ばし、
  続く失敗は最初の 1 回とその後 1 分に 1 回だけ（件数つき）ログに出す。`retryable` は `ErrorCode` の全列挙の match にした
  （新しいコードは分類するまでコンパイルできない）。
- **`forge doctor` の `review.merges`（HIGH の一部）**: serve のプロセスの中の最終実行時刻は doctor（別のプロセス）から読めないので、
  照合が止まっていることを **結果で** 見る: 照合の期限（放置 10 分、`executing` は 10 分 30 秒）をさらに 3 分過ぎた生きた試行の数
  （照合が動いていないか、毎回失敗している）と、直近 7 日（2 回目で 24 時間に改訂）の `failed`（人の判断待ち）。どちらも FG-REVIEW-0009
  （`reconcile_overdue` / `ref_state_unknown`）。
- **既定のコミットメッセージは要求の時点で固める（HIGH、改訂）**: `message` が無い要求は、要求のトランザクションでロックした MR の
  タイトルと説明から既定のメッセージを作り、試行の行の `message` に保存する。キューにある間のタイトル・説明の変更（許している）は
  コミットに入らない。ジョブは保存されたメッセージを使う。
- **gstn の `--wait`（HIGH）**: `merge: null` や別の試行の答えは終わりにしない。到達できない・5xx の答えは 1 秒から倍々で 8 秒までの
  間隔で期限まで待ち直す。期限は FG-CLI-0008。
- **ロックの下の git の時間（MEDIUM）**: ロックの下の git の呼び出し（ref の読み取り・`merge-base`・`commit-tree`・`update-ref`）は
  どれも「その呼び出しの時間切れ」と「ロックの保持の残り」の短い方で抑え、残りが 0 なら起動しない（FG-DB-0002
  `merge_lock_lost hold_exceeded`、再試行）。ソースブランチの削除は ADR §4.2 の 10 のとおりロックを放した後に行うので、自分の
  時間切れ（ref の読み取り 10 秒・`update-ref` 30 秒）だけで抑える。
- **`invalidate` の漏れ（MEDIUM）**: ジョブの再開で先端が既に `new_sha` だったとき、照合が「着地した」と判断したときも閲覧の
  キャッシュを捨てる。
- **ロックの取り方（MEDIUM）**: ロックのトランザクションは `statement_timeout` を待ちより 5 秒長く上げ（既定の 15 秒の文の
  時間切れで 30 秒の待ちが切れないように）、取得の全体を自前の時間切れ（待ち + 10 秒）で包む。生きていることの確認は
  `pg_locks` の `objsubid = 1` とデータベースも見る。
- **MR が開いていない tx2（MEDIUM、改訂）**: ref が動いた後に MR が開いていない（不変条件の破れ。開いている MR を変える操作は
  どれも生きた試行を断る）ときは、試行を `executing` のまま残さず、同じトランザクションで `failed`（FG-REVIEW-0009
  `merge_request_not_open`、alert、runbook `merge-execution`）にする。版の ref の Purge も生きた試行があれば FG-REVIEW-0011 で
  断る（マージが読む head の ref を消さない）。
- **作成者の名前（MEDIUM、改訂）**: git は名前とメールの両端の「crud」（空白・`.` `,` `:` `;` `"` `\` `'`）を落とすので、crud だけの
  表示名は「empty ident」で恒久的に失敗していた。名前は作るときに正規化する（制御文字は空白、`<` `>` は除く、両端の crud を落とす、
  何も残らなければ username）。メールは直さずに断る: 制御文字・`<` `>`・crud だけのメールは **FG-REVIEW-0012**
  MergeIdentityInvalid（422、新規）で、試行は `superseded`（FG-SYS-0001 にしない）。
- **管理者のトークン（MEDIUM）**: `admin` と `merge_execute` を持つ管理者のトークンは、Owner で保護された target にもマージできる
  （push の経路と同じく、管理者の権限は保護を越える）。`admin` の無い管理者のトークンは普通の利用者（行で固定）。ジョブの時点の
  再判定（所属の削除・Reporter への引き下げ・無効化・種類が bot に・セッションの失効）をそれぞれ実際の git のテストで固定した。
- **両方の証明（MEDIUM）**: `update_ref` は `Decided`（`Authorized` と `MergeClearance`）を引数に取る。再開の経路も同じ。
- **ガード（MEDIUM、追記のマイグレーション 20261110100100）**: 試行は `queued` で始まる（INSERT もガードする。トリガーは
  `BEFORE INSERT OR UPDATE`）。`applied` の試行は `source_outcome`（と `updated_at`）のほかは変わらない。
- **tx1b と `update-ref` の間の残り（MEDIUM、§5 の補足）**: tx1b の再判定から `update-ref` までの間に権限が下がる・保護が上がる
  ことは、push の経路（判定の後に git が ref を書く）と同じく残る窓として受け入れる。窓はロックの保持（最大 180 秒）とフェンスの
  時刻の窓で抑えられ、照合の時刻の窓（570 秒）を超えては動かさない。
- **小さな修正（LOW）**: `ref_locked` は `packed-refs.lock` も見る。push の記録で読めなかったコミットの ID は warn で数を出し、
  一覧を `commits_truncated` にする。target が既に head の早送りは `MergeBlocked`（`nothing_to_merge`、新しい止める理由）。再開の
  経路で ref を動かしたときの監査にトークンのスコープを残す（先端が既に動いていた再開と照合は資格情報を読み直さないので空）。
- **今後のマイグレーションの注意**: 行の多い表の CHECK を広げるときは `ADD CONSTRAINT … NOT VALID` と `VALIDATE CONSTRAINT`
  に分ける（書き込みを止めない）。R4 の `tokens_scopes_known` はトークンの行が少ないので一度に置き換えた。

### R4 の再レビュー（2 回目）で決めたこと（2026-11-03）

- **コードの誤記（HIGH）**: マージのタスクが止まったときのコードは **FG-SYS-0005** ServeFailed（FG-SYS-0003 は
  UnknownErrorCode）。runbook・この ADR・FG-REVIEW-0009 の説明・serve_merges のコメント・forge-cli の README を直し、
  FG-SYS-0005 の診断にマージの operation（`merge_task_exited`・`merge_supervisor_exited`・`merge_task_pending`・
  `reconcile_pass_timeout`・`reconcile_attempt_timeout`）と runbook `merge-execution` を足した。
- **照合の 1 件ごとの時間切れ**: 1 件の片付けは 20 秒まで（`ATTEMPT_TIMEOUT`）。超えたら FG-SYS-0005
  `reconcile_attempt_timeout` を `merge_id` つきで 1 回ログに出し、その試行を次の走査の後ろへ回す（1 件が 45 秒の走査を
  使い切らない）。
- **doctor の `failed` は直近 24 時間（改訂: 上の 1 回目の節の「7 日」）**: 対処の済んだ失敗が doctor を長く赤くしないように、
  新しい表は足さずに数える範囲を 24 時間にした。それより古い `failed` は runbook の SQL で見る。doctor の期限は serve と同じ
  `MergeTimeouts::default()` から作る（R4 では設定の鍵が無い。鍵を足すときは serve とこの検査の両方がそれを読む）。
- **ロックの下の git の読み取り**: `merge-tree`、CAS の負けの読み直し、照合の ref の読み取りと祖先の確認も、ロックの保持の残りで
  抑え、残りが 0 なら起動しない（再試行）。ref が動いた後の記録（tx2）だけは保持の残りで断らない: push の記録のコミットの一覧は
  残りの時間で取り、残りが無ければ一覧なし（`commits_truncated`、warn）で記録する。
- **ハートビート**: `forge.serve.merge_reconcile_pass` を info（1 分に 1 回）にし、forge-review の同じ意味の debug のイベントは
  消した。生きていることの信号はこのハートビートと doctor の `review.merges`。照合が何かを片付けた回の
  `forge.review.merge_reconciled`（結果の件数）は別の意味なので残す。
- **配布の注意**: 既定のメッセージを要求の時点で固める変更（1 回目）より前に積まれた試行は `message` が空で、ジョブの時点の
  タイトルと説明を使う。配布の前にマージのキューを空にする（`queued` / `prepared` / `executing` の試行が無いことを確かめる）。
  手順は runbook `merge-execution` の「配布の前に」。

## 実装で決めたこと（R5 `feat/core-approval-rules-codeowners`、2026-11-03）

本文（§6・§7・§8・§9・§12）と違えたところは **逸脱** と書いた。

- **表**（追記のマイグレーション 20261111100000）: `tokens_scopes_known` に `merge_approve` を足した（変種と CHECK は同じ MR、§8 の 5）。
  `merge_request_approvals`（`approver_id`・`head_sha`・`target_branch`・`created_at`・終わりの `revoked_at` / `revoked_reason` =
  `unapproved` / `new_commits` / `target_changed`）。承認は 1 行 1 回で、終わった行は消さずに残す（履歴）。生きた承認は承認者と MR ごとに 1 つ
  （部分一意索引 `merge_request_approvals_one_live`）。ガード `merge_request_approvals_end_once`（同一性は変わらない、終わりは一度だけ）。
  `approval_rules`（名前 `[a-z0-9][a-z0-9_.-]{0,63}`、プロジェクトで一意、`target_pattern` は `RefPattern`、`required_approvals` 1〜20）と
  `approval_rule_approvers`（利用者かグループのどちらか 1 つ、外部キーつき）。3 つともプロジェクトに監査される表で、プロジェクトの purge で消す。
- **承認と取り消し**: `approve_mr_decided` は MR の行のロックの下で、`decide`（`reconfirm` と、要求の `sha` が今の head、target が git の事実を
  読んだときと同じこと。違えば FG-REVIEW-0008 `stale_view`）の後に行を書く。同じ組の生きた承認があれば何も書かない（`unchanged`）。別の組の
  生きた承認（通常は起きない）は終わらせて書き直す。生きた承認が 50 件で **FG-REVIEW-0015** ApprovalLimitReached（**新規**。版の記録が承認ごとに
  `merge_request.unapproved` を出すので、1 つの変更のイベントの上限 100 件に収める）。`merge_request.approved` の `counted` /
  `uncounted_reason` は、書いた直後に同じトランザクションで読んだ承認の事実と、トランザクションの前に読んだ git の事実で評価する（§6）。
  dedup は `merge_request_approved:<approval_id>` / `merge_request_unapproved:<approval_id>`（行は一度だけ終わる）。
- **取り消し**: 版の記録（新しい版、**A → B → A の戻りを含む**）が `new_commits`、target の変更が `target_changed` で、生きた承認をすべて
  終わらせ、承認ごとに監査の `affected` の行とイベントを同じトランザクションで書く。**逸脱**: `reset_approvals_on_push = false` で承認を
  写す設定と `may_carry_approvals` は R5 に入れていない（設定を持つ `merge_settings` は R6。それまですべてのプロジェクトで既定の「消す」）。
- **だれが承認できるか**（§8 の 4、§12）: `MergeRequestApprove` / `MergeRequestUnapprove` は人だけ（`is_people_only` に足した。サービス
  アカウント・ボットの行は `matrix_rows_people.rs`）、所属のロールで Developer 以上、作成者は **ロールより先に** FG-REVIEW-0013
  （`reason=self_approval`、管理者も。`admin_author_self_approval_is_denied`）。スコープ `merge_approve`（`api`・`admin`・`merge_execute` は
  満たさない）。R4 の宿題だった「`merge_approve` だけのトークンでマージ」の deny の行も入れた（`matrix_rows_approval.rs`）。取り消しも同じ arm
  （作成者には取り消す承認が無い）。
- **数えるか**（§8 の 3・6、評価の時点）: 今の head と target の承認（`stale`）、承認者が有効（`inactive`）、人（`not_human`）、
  所属のロールが Developer 以上（`role_insufficient`。**管理者の印は数えない**: 所属の無い管理者の承認は数えられない）、作成者でない
  （`author`）、コミッターでない（`committer`、示せなければ `committers_unknown`）。有効でも、適用されるルール・CODEOWNERS の行・設定の
  ファイルの条件のどれにも入らなければ `not_in_rule`（要件がまったく無いときは数える）。「コミッターの承認を数えない」設定は R6 まで常に有効。
- **コミッターの問い合わせ**（§8 の 3 の「正確な問い合わせは R5 で固定する」）: 範囲 `merge_base..head` を git で読み（最大 1 000 コミット）、
  そのリポジトリの `repository.pushed` の監査の行のうち、ref の `new` か一覧のコミットが範囲にあるものの actor（再生の行は `replayed_from`）を
  コミッターにする（MR の作成の前の push・他のブランチへの push・別名の push を含む。テスト `committers_include_pushes_before_mr_creation`、
  `committers_include_pushes_to_other_branches`）。どの行にも名前の出ない範囲のコミットは、一覧が切れた（21 件以上の）ref の更新のうち `new` から
  そのコミットに届くもの（git、最大 16 件）の actor を全員コミッターにする。届く更新が無い・ref の一覧が切れた記録がある・git が失敗した・
  上限を超えた → `committers_unknown`。**逸脱**: 本文の「範囲のコミットを祖先に持つ push の actor 全員」は、一覧が切れた更新についてだけ
  git で確かめる（すべての push の `new` の祖先を確かめると、リポジトリの push の数だけ git を動かす）。範囲のコミットを新しく運ばず、
  その上に自分のコミットを積んで別名で push しただけの人はコミッターにならない。push の記録の無いコミット（取り込み・手の `update-ref`・
  保存期間の外）があると、ルールのある MR は止まる（`committers_unknown`、runbook `merge-blocked`）。
- **CODEOWNERS**（§9、計画 11 Q5）: target の先端（ジョブではロックの下で読んだ target の先端）の `.forge/CODEOWNERS`、無ければ `CODEOWNERS` を
  `ls-tree` と `cat-file` で読む（シンボリックリンク・ディレクトリは `not_a_file`）。文法は本文のとおりで、`\#` と `\ ` のエスケープ、
  `[` `]` `{` `}`・`***`・要素の一部の `**`・`.` / `..` は `pattern_invalid`、セクション・任意の承認・メール・否定は `unsupported`。
  **先頭の `/` の無いパターンはどの深さでも一致**（`/` を含んでも。GitLab と同じく `**/` を前に付けた扱い）、パターンはそのパス自身と
  その下のすべてに一致、末尾の `/` は下だけ。所有者は 1 要素の `@name` が利用者、無ければ最上位のグループ。`@a/b` はグループの全パス。
  グループの所有者は **直接のメンバー**（下位グループのメンバーは含めない）。知らない所有者は最初にその名前を書いた行で `unknown_owner`。
  **上限（未決を決めた）**: 64 KiB・1 000 行・1 行 4 KiB・1 行の所有者 50。解析できないファイルは `codeowners_invalid{line, reason}` で止め、
  評価のたびに warn `forge.review.codeowners_invalid`（FG-REVIEW-0014）を 1 回出す。
- **変更されたパス**: `diff-tree --no-renames -r -z --name-only <merge_base> <head>`（**上限 10 000 件**、出力の上限は件数 × 4 KiB、
  **git の呼び出しごとに 10 秒**。ジョブではロックの保持の残りで抑える）。超過は `too_many_paths`、UTF-8 でないパスは `non_utf8_path`、
  共通の履歴が無いと `no_merge_base`、target が無いと `target_missing`、git の失敗は `timeout` / `git_failed`（warn
  `forge.review.approval_input_unread`、git のコード）。**逸脱**: 本文の `spawn_stream` ではなく、上限つきの取り込み（超えたら止めて
  `too_many_paths`）にした。閉じる側に倒すことは同じ。
- **設定のファイル**（§8 の 7）: `is_config_path` は `.forge/**`・`.forge-ci.yml`・直下の `CODEOWNERS`。保護パスと CI の読み込むファイルは
  M4 まで空の集合（テスト `is_config_path_holds_no_protected_paths_or_ci_includes_until_m4`）。Maintainer 以上の数えられる承認が 1 つ要る。
- **`evaluate_merge()`**: `BlockReason` にデータを持たせた（`Copy` ではなくなった）。並びは §7 のとおり `approvals_missing`（ルールの名前の順）→
  `codeowner_approval_missing`（行の順）→ `codeowners_invalid` → `config_change_needs_maintainer_approval` → `changed_paths_unknown`。
  FG-REVIEW-0007 の context に `details`（`approvals_missing:rule=… required=… counted=…;…`）を足した。入力の git の事実（変更されたパス・
  範囲・target の CODEOWNERS）は、マージの要求ではトランザクションの前に、ジョブではロックの下で ref を読んだ直後に 1 回読み、tx1a・
  tx1b・再開のすべての判定が使う。承認・ルール・承認者の事実はゲートのトランザクションの中で読む（`MergeGate.approvals`）。
  git の失敗は判定の失敗ではなく「分からない」事実として止める（ジョブでは `superseded` の MergeBlocked。再試行はしない）。
- **ルールの管理**: `ApprovalRuleManage`（人だけ、Maintainer 以上、`api`）。`PUT /api/v1/projects/{id}/approval-rules/{name}`（作成か置き換え。
  承認できる人は利用者名とグループの全パスで受け取り、ID で保存）、`DELETE …/{name}`、一覧は `GET …/approval-rules`（`ProjectRead`）。
  プロジェクトごとに 20 ルール、1 ルール 50 の利用者・グループまで（FG-WEB-0006）。監査だけ（`approval_rule.put` / `.removed`、イベントなし）。
  ルールの変更は承認を消さない（テスト `a_rule_change_changes_evaluate_merge`）。保護ブランチの `protected_ref_merge_rules`（最低の数）は R6。
- **ルート**: `POST …/{iid}/approve`（本文 `sha`）、`POST …/{iid}/unapprove`、`GET …/{iid}/approvals`（`MergeRequestRead`: 承認ごとの
  `counted` / `uncounted_reason` / `committer_push_audit_id`、ルール、必要な CODEOWNERS の行、`config_change_approved`、`blocked_by`）。
  gstn は `mr approve --sha` / `mr unapprove` / `mr approvals` と `approval-rule list|set|remove`。
- **R6 へ送ったもの**: `codeowners_enforced` と期限つきの切り替え（`CodeownersEnforcementToggle`、`codeowners_enforced_off_until`、期限が
  過ぎたことの通知）と `merge_settings`（`reset_approvals_on_push`・コミッターの規則の切り替え）。それまで CODEOWNERS の行き詰まりの逃げ道は
  target に push できる人の直接の push（runbook `codeowners`。R6 で保護ブランチへの直接の push の禁止を入れる前に切り替えを入れる）。
  代理の deny の行は ADR-0025。

### R5 の独立レビュー（1 回目）で決めたこと（2026-11-03）

評価役（security・authz・database・rust）の HIGH 3 件と MEDIUM / LOW への対応。上の R5 の節と違えたところは **改訂** と書いた。

- **一覧が切れた push のコミッター（HIGH、改訂）**: 一覧が切れた（21 件以上の）ref の更新は、その `new` が範囲に届けば（範囲にある、または範囲の
  底（範囲に親を持たないコミット）を祖先に持つ。`merge-base --is-ancestor`）、**範囲のコミットが別の記録に載っていても** その actor をコミッターに
  する（Bob が B1..B21 を 1 回で push して B1 が一覧から落ち、Alice が B1 で `feature` を作ったとき、Bob の承認を数えない。テスト
  `a_cut_push_reaching_a_named_commit_still_makes_a_committer`・`a_cut_push_reaching_the_range_makes_its_pusher_a_committer`）。リポジトリ全体の
  一覧の切れた push を読み（最大 10 000 件）、届かないものは数えない（無関係な push で `committers_unknown` にしない）。届くものが 16 件を超える・
  届くかが分からないと `committers_unknown`。push の記録の読み取りに部分索引 `audit_log_pushes`（追記のマイグレーション 20261111100100。
  **CONCURRENTLY ではない**: 守られた監査の表への DDL は db.migrate の行と同じトランザクションでしか認められない。`-- no-transaction` は 1 文で
  その行を書けない。作る間は監査の書き込みが待つ）。
- **範囲と変更されたパス（MEDIUM、改訂）**: 範囲は `head ^<target の先端>`（マージが持ち込むコミット）、変更されたパスは **target の先端と
  試しのマージ（`merge-tree --write-tree`）の木の差分**（共通の祖先が複数ある criss-cross の履歴でも、target に着地する変更だけ。テスト
  `criss_cross_histories_count_only_what_the_merge_changes`）。`merge-base` は呼ばない（その失敗を `no_merge_base` と読む誤りも消えた）。
  衝突なら承認の側の止める理由は足さない（`ChangedPathsFact::Conflict`。衝突は mergeability とジョブの試しのマージ（FG-REVIEW-0006）が止める。`changed_paths_unknown` にするとジョブの最初の判定が FG-REVIEW-0007 で先に止め、衝突のコードが失われた。全体のテストで見つけた）。
- **読み取りの予算（MEDIUM）**: 1 回の評価のすべての git の呼び出しと push の記録の読み取りを 1 つの期限と回数で抑える（`Budget`）。要求
  （マージ・承認）は 20 秒・64 回、`GET …/approvals` は 10 秒・32 回、マージのジョブはロックの保持の残り・64 回。各呼び出しは最大 10 秒。
  尽きたら「分からない」事実（`timeout`）で止める。
- **CODEOWNERS の照合（HIGH、改訂）**: パターンは解析の時に要素へ分け、連続する `**` と `*` をまとめ、ワイルドカードは 1 パターン 16 個まで
  （超えたら `pattern_invalid`、行番号つき）。照合は（要素 × パスの位置）の表と要素ごとの 2 点の glob で、指数的な後戻りをしない。照合は
  ブロッキングのタスクで 5 秒まで（超えたら `codeowners_invalid reason=match_budget`）。結果（行 → パスの数）を評価の入力に持つ。
- **見えない文字（HIGH）**: 先頭の UTF-8 の BOM は捨てる。注釈の外の Unicode の Cf（ゼロ幅・方向制御など）と ASCII でない空白（U+3000 など）は
  `pattern_invalid`（行番号つき）。注釈の中は文字として扱う。
- **大文字と小文字（MEDIUM）**: CODEOWNERS の照合と `is_config_path` は ASCII の大文字と小文字を区別しない（`.Forge/ci.yml`、`SRC/auth.rs`）。
- **ログ（MEDIUM）**: 読めない CODEOWNERS は `forge.review.codeowners_invalid`（FG-REVIEW-0014）を評価ごとに 1 回だけ出す（git の失敗は同じ行の
  `git_error_code`）。
- **承認者の事実（MEDIUM）**: 承認者の数によらず 3 文（アカウント、利用者の一覧を渡す 1 つの chain の問い合わせによるロール、直接のグループ）。
  `GET …/approvals` の読み取りは 1 つのスナップショットではない（READ COMMITTED の 1 トランザクション）: 表示だけで判定に使わず、マージは
  ゲートのトランザクションでロックの下で読み直す。分離レベルの指定は `ChangeTx` だけに許す（db-write-boundary ゲート）ので、そう注記した。
- **存在の問い合わせ（MEDIUM）**: ルールの承認できる人と CODEOWNERS の所有者は **有効な** 利用者と生きたグループだけを引く。存在しない名前と
  無効化された利用者は同じ答え（ルールは FG-WEB-0006 `unknown_user`、CODEOWNERS は `unknown_owner`）。グループは内部の公開で、見えないグループは無い。
- **LOW**: ルールの利用者・グループは重複を除いてから比べて書く。権限の表に `merge_execute` だけ・`api` の無い Maintainer のトークンでの
  `ApprovalRuleManage` の deny と、`merge_approve` + `admin` の管理者のトークンでの承認の allow を足した。
- **記録だけ（LOW）**: (1) 引き下げられた元の承認者（Reporter など）は `MergeRequestUnapprove` を拒まれる。その承認は評価の時点で
  `role_insufficient` で数えられないので、取り消せなくてもマージの条件を満たさない（必要なら新しいコミットか target の変更で消える）。
  (2) 所属の無い管理者の承認は **受け付けるが数えない**（`role_insufficient`）。`authorize()` は管理者の権限で承認を許す（管理者の印は
  評価に使わない）。拒むより、表示で「数えない理由」を見せる方を選んだ。(3) `forge_app` の `merge_request_approvals`・`approval_rules`・
  `approval_rule_approvers` への DELETE はプロジェクトの purge だけが使う（承認の終わりは `revoked_at` の UPDATE、ルールの置き換えは承認できる人の
  行の DELETE と INSERT で、どれも監査の行と同じトランザクション）。

### R5 の再レビュー（2 回目）で決めたこと（2026-11-03）

security の HIGH 3 件と MEDIUM / LOW への対応。上の R5 の節と 1 回目の節と違えたところは **改訂** と書いた。

- **一覧に無い ref（HIGH、改訂）**: `repository.pushed` の監査の行は ref を 100 件（とバイトの予算）までしか載せない。101 件目に範囲のコミットを
  置いた push は見えず、範囲のコミットが別の記録に載っていると早い return で `Known` になっていた。push の記録のトランザクションで、一覧に
  載らなかった ref の更新（削除を除く）を新しい表 `push_unlisted_refs`（追記のマイグレーション 20261111100200。プロジェクトに監査される表、
  更新しない（ガード `push_unlisted_refs_immutable`）、purge で消す）に残し、コミッターの規則はそれを「コミットの一覧が分からない push」として同じ
  到達の判定に入れる。表ができる前の、ref の一覧が切れた記録が 1 つでもあれば、範囲のコミットがすべて名指されていても `committers_unknown`
  （早い return の前に確かめる。テスト `a_record_with_cut_refs_is_unknown_even_when_every_commit_is_named`、
  `a_ref_beyond_the_listed_hundred_still_makes_a_committer`）。ref の一覧が空の記録も読む（`LEFT JOIN`）。
- **届くかの確認は git 1 回（HIGH、改訂）**: 一覧の切れた push の先端すべてと `^<target の先端>` を `rev-list --parents --stdin` 1 回に渡し
  （最大 50 000 コミット。超えたら `push_graph_too_large`）、届くか・どのコミットに届くかはメモリの上で決める。大きな push が何件あっても予算の
  回数を使い切らない（テスト `many_large_pushes_do_not_exhaust_the_budget`: 41 件で `GET …/approvals` の 32 回の予算が足りる）。先端の
  コミットがリポジトリに無いと git の失敗（`git_failed`）で、「届かない」とはしない（O3 の prune が入ったら見直す）。
- **パスの長さと照合の時間（HIGH）**: 4 KiB を超える変更されたパスは `changed_paths_unknown reason=path_too_long`（warn を 1 回、
  FG-GIT-0006 `path_too_long`）。照合はパスの間に加えてルール 64 行ごとにも期限を確かめ、`spawn_blocking` の待ちも `tokio::time::timeout`
  で抑える。タスクの失敗（panic）は予算の超過と分けて `codeowners_invalid reason=match_failed`（`join_error` つき）。`matches()` は 1 行の
  表を回して使い、後ろが合わないときは glob を呼ばない。
- **見えない文字の追加（MEDIUM）**: 注釈の外の、タブ以外の制御文字（VT・DEL など）、結合文字（U+0300–036F・0483–0489・1AB0–1AFF・
  1DC0–1DFF・20D0–20FF・3099–309A・FE20–FE2F）、ハングルの埋め字（U+115F・U+1160・U+3164・U+FFA0）、点字の空白（U+2800）、異体字
  セレクタ（U+FE00–FE0F・U+E0100–E01EF）も `pattern_invalid`。
- **target が無いとき（MEDIUM）**: 評価できないことを warn `forge.review.approval_input_unread`（FG-REPO-0023 `ref_not_found`）で 1 回出す。
- **状態の表示（MEDIUM）**: `GET …/approvals` は `conflict`（今の head が target の先端にきれいにマージできない）を返し、gstn も 1 行出す。
  承認の側の止める理由が無くても、衝突している MR は「承認済み」に見えない。
- **パターンの位置（LOW、改訂）**: §9 の「`/` のないパターンはどの深さでも一致」に合わせ、gitignore と同じく **先頭か途中に `/` がある
  パターンは直下に固定** にした（上の R5 の節の「`/` を含んでもどの深さでも一致」を改める）。末尾だけの `/`（`docs/`）はどの深さでも一致。
  両端の `//` は `pattern_invalid`。

### R5 の再レビュー（3 回目）で決めたこと（2026-11-03）

security の HIGH 2 件（どれも安い操作で 1 つのリポジトリのすべての承認を永久に止められる、可用性の問題）と MEDIUM / LOW への対応。

- **削除だけの一覧に無い ref（HIGH、改訂）**: 一覧に載らない ref がすべて削除の push は `push_unlisted_refs` に行が無く、「表ができる前の
  切れた記録」と読まれて `committers_unknown` になっていた（101 本の `git push --delete` で永久に止まる）。新しい push の記録はすべて、監査の
  行の `details` に印 `unlisted_kept: true` を持つ（イベントのペイロードには入れない）。古い記録かどうかは行の有無ではなくこの印で決める
  （削除はどの範囲にも届かないので行は要らない）。テスト `deleting_many_branches_does_not_block_approvals`。
- **リポジトリ全体の行の上限（HIGH、改訂）**: 一覧に無い ref の行をリポジトリ全体で数えて 10 000 件の上限に当てていたため、1 000 個のタグの
  push 12 回で永久に止まった。**選んだ方法**: 一覧に無い ref は (先端, push した人) ごとに 1 項目にまとめ、記録に載った ref も、範囲の
  コミットだけをコミットの一覧に残して小さくし、上限は各 200 000 項目（実際のリポジトリよりはるかに大きい正気の上限。超えたら
  `push_records_over_cap`）。候補の先端はすべて 1 回の `commit_graph` に渡し、届くかはメモリの上で決め、**16 件の上限は範囲に届く候補の
  数に当てる**。target の歴史の上のタグや無関係なブランチは `^<target の先端>` で外れるか届かないので、上限にも予算にも効かない
  （テスト `many_tags_do_not_block_approvals`: 12 000 個のタグ）。新しい順に読む・時刻の窓で絞る方法は採らなかった（古い push を
  上限の外へ押し出せばすり抜けられ、コミットの時刻は利用者が決められる）。
- **消えた先端（MEDIUM）**: `commit_graph` の前に候補の先端を `cat-file --batch-check` で確かめ、リポジトリに無い先端は、どの ref も
  向いていない（`for-each-ref`）ときだけ外す（届かない）。消えた先端に ref が向いていれば破損として `committers_unknown`
  （`push_tip_missing`）。prune（O3、runbook `merge-request-mergeability`）の後の扱いを runbook に書いた。テスト
  `a_pruned_cut_tip_no_ref_points_at_is_dropped`。
- **見えない文字（LOW、改訂）**: 結合文字は選んだブロックではなく、Unicode の一般カテゴリ Mn / Me（と Cf）で判定する。表はすでに依存の木に
  ある `unicode-properties`（sqlx の stringprep が使う。MIT / Apache-2.0）を `general-category` だけで直接の依存にした。

### R5 の再レビュー（4 回目）で決めたこと（2026-11-03）— コミッターの規則の作り直し

**改訂（R5 の節と 1〜3 回目の到達可能性の設計を置き換える）**: 監査の行（ref 100 件・ref ごとのコミット 20 件までの一覧）から push の
中身を組み立て直し、一覧が切れたところを git の到達の判定で補う設計は、上限・予算・prune のたびに「安い操作で 1 つのリポジトリの承認を
すべて止める」新しい穴を生んだ（タグの作成・削除の繰り返しで行数の上限、無関係な 51 000 コミットの孤立したブランチでグラフの上限、
切れた先端の prune で B1 のすり抜けの再発）。コーディネーターの判断で、§8 の 3 の規則の実装を次のとおり作り直した。

- **push を受けた時点で記録する**: 新しい表 `push_commits`（追記のマイグレーション 20261111100300。`repository_id`・`commit_sha`・
  `pusher_id`・`project_id`・`request_id`（その push の監査の行と結ぶ。監査の行の id はコミットの時に決まるので）・`first_pushed_at`。
  主キー (repository_id, commit_sha, pusher_id)、`ON CONFLICT DO NOTHING`、プロジェクトに監査される表、更新はガードで禁止、削除は purge だけ）。
  push の経路（forge-web）は ref が動いた後に、push がそれまでどの ref からも届かなかったコミットを持ち込んだ集合を 1 回の
  `rev-list --stdin <new>… --not --exclude=<更新した ref> --glob=refs/*`（と各 `^<old>`）で全部読み（forge-git `introduced_commits`。
  切り詰めない。1 回の push に上限 1 000 000 コミット・30 秒）、push の記録と同じトランザクションで (コミット, push した人) ごとに書く。
  既にあるコミットにタグ・ブランチを作るだけの更新、削除、木や blob のタグは行を書かない。上限・時間切れ・git の失敗は監査の行に
  `push_commits_incomplete: true` を付け、一覧に入らなかったコミットは行を持たない。マージは自分のマージコミット（または squash の
  コミット）を要求者の行として記録する（早送りは書かない）。git の一覧を持たない記録（runbook の再生、テスト）は、監査の行の一覧が
  完全なとき（どの ref も切れていない）だけその一覧を、そうでなければ不完全として扱う。
- **MR のコミッター** = `SELECT pusher_id FROM push_commits WHERE repository_id = $1 AND commit_sha = ANY(<範囲>)`（1 文、索引つき）。
  範囲は従来どおり `head ^<target の先端>`（最大 1 000、超えたら `committers_unknown`）。**範囲のコミットに 1 つでも行が無ければ
  `committers_unknown`**（表の前の履歴、`update-ref` の取り込み、不完全な push。閉じる側）。監査の行を読まず、履歴をたどらず、無関係な
  履歴の量に上限が効かず、オブジェクトが残っているかにも依らない（prune に左右されない）。
- **消したもの**: 監査の行からの組み立て（`push_coverage`）、一覧の切れた push の到達の判定（`commit_graph`・`reach_in`・16 件の上限・
  50 000 の上限）、消えた先端の確かめ（`missing_objects` / `referenced_objects`）、印 `unlisted_kept` と表 `push_unlisted_refs` への書き込み。
  **表 `push_unlisted_refs` は残す**（追記だけの履歴。読み手も書き手も無い。後のマイグレーションで消してよい、と tables.toml にも書いた）。
  **索引 `audit_log_pushes` は消した**（20261111100300 で `DROP INDEX`。ほかに読み手が無い）。
- **テスト**（実際の git と DB）: Bob が B1..B21 を push し Alice が B1 で `feature` を作る → Bob はコミッター（prune の後も同じ）、
  ref 101 本の最後に head → コミッター、ブランチ 101 本の削除・タグ 12 000 個・タグの作成と削除の繰り返し・無関係な 51 000 コミットの
  push → 止めない、`update-ref` で入れたコミット → `committers_unknown`、上限で不完全な push → その push の一覧に無いコミットを範囲に
  持つ MR だけが止まる。forge-git で「既にあるコミットへのタグ・ブランチ・早送り・削除は何も持ち込まない」「上限で不完全」。
- **残る設計上の制約**: 3 回目までに挙がった「子を 17 個タグ付けすると 1 つの MR を止められる」は、一覧の切れた push の扱いが無くなった
  ので **もう無い**。代わりに、表の前の履歴を範囲に持つ MR は（ルールがあれば）止まる。**解き方は 5 回目で訂正した**（下の節）:
  同じコミットの push し直しでは解けない（もう届くコミットは「新しくない」ので行を書かない）。コミットの id を変えて push する。

### R5 の再レビュー（5 回目）で決めたこと（2026-11-03）— push_commits の作り直しの見直し

- **HIGH 1（「push の前に届いていた」の TOCTOU）**: 4 回目の一覧は push の **後** に生きた ref を `--glob=refs/*` で読み、更新した ref
  だけを除いていた。receive-pack と一覧の間に別の ref が同じコミットを指すと（同じコミットの別の push、MR の版の固定 ref
  `refs/merge-requests/<iid>/…` の書き込み）、そのコミットは「前から届いていた」に見え、どちらの push も行を書かなかった
  （コミッターの見落とし = 承認のすり抜け）。**改訂**: forge-web は receive-pack を始める **前** に全 ref の先端を読む
  （forge-git `ref_snapshot`: `for-each-ref`、隠し ref も含む、タグはコミットまで剥がす、シンボリック ref は飛ばす、各コミット 1 回、
  上限 1 000 000 ref、10 秒）。一覧は `rev-list --stdin --end-of-options` に、新しい先端・`^<スナップショットの先端>`・`^<old>` を
  **すべて標準入力で** 渡す。生きた ref の glob は使わない。スナップショットの失敗は push を止めず、その push の一覧を不完全にする
  （下の FG-GIT-0016）。
  - **過剰な帰属は安全と決めた**: スナップショットの後に別の push が持ち込んだコミット、同時に同じ新しいコミットを push した 2 人は、
    どちらも行を持つ。コミッターが増えるのは「承認を数えにくくする」方向だけで、数えすぎにはならない。危ないのは見落としの方。
  - テスト（forge-git、実際の git）: 受けた後・一覧の前に同じコミットへ 2 本目の ref → 両方の push が行を持つ、push の後の版の固定
    （隠し ref）→ 一覧は変わらない、シンボリック ref は何も隠さない（スナップショットに入らない）、隠し ref はスナップショットに入る。
- **HIGH 2（行の上限と記録の予算）**: 上限 1 000 000 行は、記録の 30 秒の予算（3 回の試行の共通）の中で書けず、push の監査の行ごと
  失う（FG-GIT-0012）恐れがあった。**測った**: 50 000 行を監査のトリガーつきで書くのに約 3.5 秒（開発機、debug ビルド）。上限を
  **50 000**（6 回目で **20 000** に下げた。下の節）（`forge_core::MAX_PUSHED_COMMITS`）に下げ、予算は `forge_core::PUSH_RECORD_BUDGET` として共有し、テスト
  `the_largest_listing_is_recorded_within_a_third_of_the_budget` が「上限の行が予算の 1/3 未満で書ける」ことを確かめる。上限を超えた
  一覧は上限で止め（それ以上の行は書かない。記録の側でも上限で切って `push_commits_incomplete`）、保持するのは高々 50 000 個の id。
  - **監査のトリガーは行ごとのまま**にした: PostgreSQL の制約トリガー（`CREATE CONSTRAINT TRIGGER … DEFERRABLE INITIALLY DEFERRED`）は
    `FOR EACH ROW` しか作れない。文ごとの遅延しないトリガーにすると、監査の行が同じトランザクションの後で書かれる ChangeTx の順と、
    doctor の検査（`audited.rs`: `audited_change_present` は遅延の制約トリガーで ALWAYS）の両方を崩す。第 2 のトランザクションに分ける
    案は、その行のための監査の行がもう 1 つ要り、失敗したときの「不完全」の印を不変の監査の行に後から付けられない。測った値が予算に
    十分収まるので、どちらも採らなかった。
- **MEDIUM 3（不完全な一覧の warn）**: 新しいコード **FG-GIT-0016 `PushCommitsIncomplete`**（warn、runbook merge-blocked）。不完全な
  一覧ごとに 1 行、`reason`（`cap` / `deadline` / `snapshot_failed` / `git_failed`）・`listed`・`refs`・`listing_ms`・`request_id`、
  失敗には `cause_code`。
- **MEDIUM 4（解き方の訂正と補填の手順）**: runbook と ADR の「push し直せば解ける」は誤り。**コミットの id を変えて**（`git commit
  --amend`、`git rebase --force-rebase <target>`）push する。作り直せない履歴には、承認の要る管理者の補填 SQL を runbook merge-blocked
  の Mitigate 2b に置いた（6 回目で「数えすぎにはならない」の誤りを直した。下の節）（監査の行と行を 1 トランザクションで。テスト
  `the_runbook_backfill_of_push_commits_writes_rows_with_their_audit_row` が runbook の SQL をそのまま実行し、監査の行の無い挿入が拒まれる
  ことも確かめる）。CLI は今は作らない。
- **MEDIUM 5（squash がコミッターを洗い流す）**: マージ・squash のコミットは要求者だけでなく、**マージした範囲（`head ^<元の target>`）の
  コミットを push した全員** の行を持つ（forge-review `merges/attribution.rs` が tx2 の前にロックの残り時間で範囲と push した人を読み、
  forge-db `finish_merge` の `range_pushers` に渡す）。範囲の push した人が分からない（行の無いコミット、範囲が 1 000 超、git・DB の
  失敗、時間切れ）ときは、マージのコミットに **行を書かない**（後でそれを含む範囲は `committers_unknown`。要求者だけを書くと見落としに
  なる）。FG-GIT-0016 `reason=merge_pushers_unknown:<理由>` の warn を 1 行。再開・照合の経路も同じ関数を通る。早送りは新しいコミットが
  無いので何も書かない。テスト: olga が dave の MR を squash した後の main → release の MR で dave はコミッター（承認は数えない）、
  マージコミットも同じ、行の無い範囲の squash は行を持たない。
- **MEDIUM 6（push の遅延）**: 毎回の一覧で `forge.git.push_commits_listed`（info）に `refs`（スナップショットの ref 数）・`listed`・
  `complete`・`snapshot_ms`・`listing_ms` を出す。ref の多いリポジトリでの遅れはここで見える。

### R5 の再レビュー（6 回目、マージ前の小さな直し）で決めたこと（2026-11-03）

- **記録の最後の試行は行を持たない**: push の記録の 3 回の試行のうち、最後の 1 回は `PushedCommits::unknown()` を送る
  （`push_commits` の行を書かず、その push を `push_commits_incomplete` にする）。DB が行の多さで遅いときにも、監査の行と
  イベントは残る（FG-GIT-0012 で記録ごと失うより、コミッターが分からない方が安全で、閉じる側に倒れる）。最後の試行の時間を
  確保するため、それより前の試行は予算の残りから 10 秒（`RECORD_FINAL_RESERVE`）早く打ち切り、最後の試行の前の待ちもその
  10 秒を削らない。行を落として記録できたときは FG-GIT-0016 `reason=record_fallback`（`dropped`・`attempts`）を 1 行出す
  （前の試行が実はコミットしていて最後が Deduped なら、落としていないので出さない）。テスト: 試行の計画の単体テストと、
  最初の試行が 120 秒止まる記録先で「最後の試行が行なしで記録し、FG-GIT-0012 は出ない」。
- **行の上限を 20 000 に下げた**: 5 回目の上限 50 000 の書き込みは全体テストの負荷の下で 10.1 秒（2 回分と DB の準備を含む）
  かかり、予算の 1/3 の余裕が小さかった。20 000 行は通常時 1.35 秒。runbook も合わせた。
- **スナップショットの記憶**: 先端は文字列とハッシュ集合でなく、32 バイトの生の id（SHA-1 は先頭 20 バイト）を並べて整列・重複除去して
  持つ（100 万 ref でも 32 MB まで）。失敗したスナップショットの誤りにも読めた ref の数（`refs`）を載せ、FG-GIT-0016 の warn の
  `refs` に出す。ref の数の上限は上の「未決」に O シリーズの申し送りとして書いた（今は実装しない）。
- **runbook の補填の手順の訂正**: 「承認を数えにくくするだけで、数えすぎにはならない」は誤り。範囲のコミットに行が揃うと
  `committers_unknown` が消え、補填した P 以外の承認が数えられるようになる（P のほかにも push した人がいれば、その人の承認まで
  数えられる）。だから手順の承認者が証拠を自分で確かめ、push した人を全員確かめられないコミットは補填しない、と書いた。
  値は psql の変数（`:'var'` / `:var`）で渡し、実行した人を `actor_label`（`merge-blocked 2b by <operator>`）と details の `operator` に残す。
  テストは psql と同じ規則で変数を展開して runbook の SQL を実行する。
- **マージのコミットの帰属の warn**: `forge.review.merge_commit_unattributed` に `repo_id` と `merge_request_id` を足した。

## 実装で決めたこと（R6 `feat/core-merge-conditions`、2026-11-03）

本文（§4・§7・§8・§9・§10・§12）と違えたところは **逸脱** と書いた。R5 から送られたもの（`codeowners_enforced` と期限つきの切り替え、
`merge_settings`、`reset_approvals_on_push` と `may_carry_approvals`、コミッターの規則の切り替え）はすべてここで入れた。

- **表**（追記のマイグレーション 20261112100000）:
  - `merge_settings`（プロジェクトごとに 1 行。**行が無ければ既定**: マージコミットと squash を許可、既定はマージコミット、ソースは残す、
    新しいコミットで承認を消す、コミッターの承認を数えない、CODEOWNERS を強制）。許可する方式は 1〜3 個、既定は許可の中（CHECK）。
    CODEOWNERS の一時停止は `codeowners_enforced = false` と `codeowners_suspended_at`・`codeowners_enforced_off_until` の組で、
    **期限は開始から 24 時間以内**（CHECK。`authorize()` の範囲の後ろ盾）。監査される表（resource `project`）。
  - `protected_ref_merge_rules`（保護のルール 1 つに高々 1 つ、`protected_refs (id, project_id)` への外部キー）: 最低の承認数 0〜20、
    必須のチェックの名前（20 個まで、`ci/`・`forge/`・`kanflow/` だけ。CHECK でも `ext/` を拒む）、スレッドの解決、直接の push の禁止。
    **作業項目のリンクの列は作らない**（§11: I1 で列と条件を一緒に足す。設定の API は `require_work_item_link: true` を FG-WEB-0006
    `rule=unavailable_until_i1` で拒む）。保護のルールの削除は同じ変更でマージの条件も消す。purge でも消す。
  - `merge_request_approvals.carried_from` と `revoked_reason = 'carried'`、ガードと監査のトリガーの列に `carried_from` を足した。
- **マージの条件の合わせ方**: target に一致するすべての保護のルールのマージの条件を **いちばん厳しく** 合わせる（`MergeRule::strictest`:
  最低の数は最大、必須のチェックは和、スレッドと直接の push の禁止はどれかが on なら on）。`push_min_role` の「いちばん厳しい」と同じで、
  ルールを足して緩むことはない。push とマージは同じスナップショット（`protected_refs::rules_for` / `rules_on` が結合して読む。
  読めない行は FG-DB-0020 で、飛ばさない）を使う。
- **マージの条件の管理**: `PUT|DELETE /api/v1/projects/{id}/protected-refs/{rule_id}/merge-rule`（§12 のとおり `ProtectedRefManage` に含めた。
  ルールの今の水準を `from` と `to` の両方にして判定するので、Maintainer は Owner の水準のルールのマージの条件を変えられない
  `above_own_role`）。タグのパターンには付けられない（FG-REPO-0013 `merge_rule_on_tag`）。命令は forge-hosting（`put_merge_rule` /
  `remove_merge_rule`、forge-db の `*_decided` を呼ぶのは forge-hosting だけ。走査の needle に足した）。監査だけ
  （`protected_ref.merge_rule_put` / `.merge_rule_removed`、イベントなし。復元の当て直し（設計レビュー 41）は監査の写しで行う）。
  一覧（`GET …/protected-refs`）の各ルールに `merge_rule` を足した。
- **直接の push の禁止**: `RefProtection::of(rules, name)` がルールの禁止を持ち、`RepoPush` は禁止のあるブランチの作成・早送りを
  **管理者も含めて** 新しいコード **FG-GIT-0017** ProtectedRefDirectPushForbidden（403、info）で拒む（force と削除はこれまでどおり
  FG-GIT-0007 / 0009）。`MergeRequestMerge` は読まない（§4.3）。実際の push のテスト（`smart_http_e2e`）で Owner と管理者の拒否、
  別のブランチの push を確かめた。本文はコードを決めていなかった（FG-GIT-0008 の「ロール不足」と原因が違うので分けた）。
- **CODEOWNERS の強制の一時停止**（§9）: `PUT …/merge-settings/codeowners-enforcement`（`CodeownersEnforcementToggle`、
  `Resource::CodeownersEnforcement { project, highest_protection, change }`、ローダーがプロジェクトの保護のルールの最大の水準を読む）。
  人だけ・Maintainer 以上。**一時停止** は加えて `above_own_role`（管理者は越える）と **期限 1〜1440 分の指定が必須**（未決だった
  「既定」は **置かない** と決めた: 期限なしは FG-AUTH-0003 `expiry_missing`、範囲外は `expiry_out_of_range`。権限の表の行）。
  **強制に戻す** のは締める方向なので Maintainer なら誰でも。期限は DB の時計で計算し、判定は `codeowners_enforced OR 期限 <= now()`
  を強制とみなす（`CodeownersEnforcement::Expired` は強制。worker に頼らない）。変更は監査の行とイベント
  `merge_settings.codeowners_enforcement_changed`（maintainer の分類）と info のログ `forge.review.codeowners_enforcement_changed`。
  期限が過ぎたことは worker のタスク `codeowners_restore`（60 秒ごと、1 回 100 件）が監査の行とイベント
  `merge_settings.codeowners_enforcement_restored`（actor `system`、dedup `codeowners_enforcement_restored:<project_id>:<期限の epoch>`）と
  ログ `forge.worker.codeowners_enforcement_restored` で 1 回知らせる。
  一時停止の間に外れるのは **CODEOWNERS の所有者の承認と読めないファイル（`codeowners_invalid`）だけ**。設定のファイルの条件
  （`config_change_needs_maintainer_approval`。CODEOWNERS を直す MR はこれに当たる）、承認のルール、保護ブランチのマージの条件は残す。
  順序: 切り替え（`d8ffe5d`）を直接の push の禁止（`b195afa`）より先のコミットに入れた。
- **マージの設定**（§4.1、§8.2、§8.3）: `GET|PUT /api/v1/projects/{id}/merge-settings`（読むのは `ProjectRead`、書くのは
  `MergeSettingsManage`: 人だけ・Maintainer）。PUT は値をすべて置き換える（CODEOWNERS の切り替えは触らない。`gstn merge-settings set` は
  読んでから指定した値だけを変えて送る）。`fast_forward` は許可すれば使える（R4 で実装済み）。コミッターの規則の切り替えは
  `committer_approvals_excluded`（既定 true。切るとコミッターの承認も数え、`committers_unknown` でも止まらない）。
  **マージの要求とジョブのすべての判定がプロジェクトの保存された設定を読む**（R4 の固定の既定 `MergeSettings::default` と
  `with_merge_settings` / `with_settings` をやめた）。要求の後に方式が許されなくなれば、ジョブが FG-REVIEW-0010 で `superseded` にする
  （§4.2 の 3・5。テスト `the_merge_job_decides_the_allowed_method_again`）。`delete_source_branch` を省いた要求はプロジェクトの既定。
- **`evaluate_merge()`**（§7）: `required_check_missing{name}`・`required_check_not_successful{name, status, conclusion}`・
  `unresolved_discussions{count}` を足した。保護ブランチの最低の承認数は承認の要件の 1 つにし、`approvals_missing` のルール名
  **`@protected_branch`**（`@` は承認のルールの名前に使えないので衝突しない）で出す。数えるのは「数えない理由の無い」承認なら誰のものでも
  （そのような承認は `not_in_rule` にならない）。**並びは `BlockReason::rank`**（§7 の列挙の順: … `codeowners_invalid` →
  必須のチェック 2 つ → `unresolved_discussions` → `config_change_needs_maintainer_approval` → `changed_paths_unknown`）で、見つけた順に
  依らない（R5 の並びの `config_change…` → `changed_paths_unknown` は §7 どおりに直った）。組み合わせの表は proptest
  （`every_combination_yields_exactly_its_reasons_in_order`、512 件）、ドラフト・「チェックが無いことは成功と区別」（`neutral` と実行中も
  満たさない）は個別のテスト。スレッドの件数はゲートのトランザクションで MR の行のロックの下で数える（解決・新しいスレッドも同じロックを取る）。
  保存されたマージ可否の判定は、確かめた組が生きた (head, target の先端) と同じときだけ数える（`stored_mergeable`、R4 のまま。状態の表示も同じ）。
- **必須のチェックの結果（逸脱）**: 本文 §11 の `forge-core::CheckResultsSource` は C1 で入れる。R6 は forge-review に
  `merges::conditions::CheckRuns` の口を置き、実装は `NoCheckRuns`（どの名前にも報告が無い）だけ。**C1 までは必須のチェックを
  設定した保護ブランチにマージできない**（報告が無いことは成功でない）。runbook `merge-blocked` にそう書いた。C1 はこの口を
  `CheckResultsSource` で満たす。
- **マージの状態**（`GET …/merge-requests/{iid}/status`、`MergeRequestRead`）: `evaluate_merge()` を今の事実で評価し、止める理由の
  すべてと入力（承認・必須のチェック・スレッド・CODEOWNERS の強制）を返す。読み取りだけ（確認を積まない、ロックしない）。評価する方式は
  プロジェクトの既定で、早送りの可否は出さない（ジョブがロックの下で決める）。版が無ければ `mergeability_unknown`。JSON は安定した契約
  （`schema_version: 1`、欄は足すだけ。`docs/api/merge-status.md`）。`gstn mr status --json` はそれをそのまま出す。
  **既存の `gstn mr merge-status`（最新の試行）と名前が近いので、こちらは `mr status`** にした。
- **承認の引き継ぎ**（§8.2）: `may_carry_approvals`（forge-review の純粋な関数。各場合をテストで固定）は、設定が「消す」、target が保護
  ブランチ、変更（前の head → 新しい head の `diff-tree --no-renames -r`）が設定のファイル・target の先端の CODEOWNERS の行が覆うパスに
  触れる、変更を読み切れない・CODEOWNERS を読めない、のどれでも写さない。版のジョブは設定が「消さない」ときだけ git を読み、決定を
  `forge.review.approvals_carry_decided`（`carry`、`reason`）で 1 行、読めなかった入力を warn `forge.review.approvals_carry_unread` で出す
  （git の失敗は「写さない」）。forge-db の `record_mr_version_carrying` は MR の行のロックの下で、設定・target の保護・「決めたときの
  head が今の head」をもう一度確かめてから写す。**逸脱**: 写した承認ごとのイベントは **`merge_request.approved` 1 つ**（`carried_from`、
  `counted: false`、`uncounted_reason: "carried"` — ジョブは評価しない）で、古い行の `merge_request.unapproved` は出さない（古い行は
  `revoked_reason = carried`）。取り消しと承認の 2 つにすると、生きた承認 50 件で 1 つの変更のイベントが 100 件を超えるため。
  `UnapproveReason::Carried` と `UncountedReason::Carried` を足した。
- **権限の表**: `matrix_rows_conditions.rs`（`MergeSettingsManage`、`CodeownersEnforcementToggle` の allow / deny と期限・`above_own_role`、
  直接の push の禁止の `RepoPush`（Maintainer・Owner・管理者の拒否、force と削除は元のコード）と、同じブランチへの `MergeRequestMerge` の
  allow）、サービスアカウントとボットの行は `matrix_rows_people.rs`。代理の行は ADR-0025。
- **新しいコード**（登録の順）: **FG-REVIEW-0016** RequiredCheckNamespaceNotAllowed（422、計画 11 の「REVIEW-0002」は仮の番号）、
  **FG-GIT-0017** ProtectedRefDirectPushForbidden（403）。再利用: 方式は FG-REVIEW-0010、ほかの入力の誤りは FG-WEB-0006、
  権限は FG-AUTH-0003（`above_own_role`・`expiry_missing`・`expiry_out_of_range`）。
- **gstn**: `mr status`、`merge-settings show|set`、`codeowners-enforcement suspend --for <分>|enforce`、`merge-rule set|remove`
  （C-1 の表、登録簿、help のスナップショット）。
- **残したこと**: 必須のチェックの報告（C1）、作業項目のリンクの条件（I1）、画面（U2）。保存されたマージ可否の判定は版の記録と
  target の push で確かめ直されるが、マージの条件の変更そのものは確かめ直しを積まない（判定はいつも評価の時点。表示も要求も読み直す）。

### R6 の独立レビュー（1 回目）で決めたこと（2026-11-03）

評価役（authz・git / security・rust / db）の HIGH 1 件と MEDIUM / LOW への対応。上の R6 の節と違えたところは **改訂** と書いた。
マイグレーションの `E'\n'` は利用者の承認を得てコーディネーターが直した（`dc34b6e`）。

- **承認の引き継ぎを小さな普通の変更に限る（HIGH、改訂）**: 引き継ぎは見ていないコードに承認を付けることなので、§8.2 の opt-in の
  設計は保ったまま、`may_carry_approvals` に次を足した。前の head から新しい head への変更（forge-git `changed_entries`:
  `diff-tree --no-renames -r -z --raw --numstat` 1 回）が、`.gitmodules` か `.gitattributes`（どの階層でも）に触れる（`special_path`）、
  サブモジュール（gitlink 160000）の項目（`submodule`）、シンボリックリンク（120000）の追加・変更・置き換え（`symlink`）、残るファイルの
  モードの変更と実行可能な新しいファイル（`mode_change`）、バイナリ（`binary_file`）、**51 ファイル以上**（`too_many_files`）、
  **追加と削除の合計 1 001 行以上**（`too_many_lines`）、読み切れない（`changes_unknown`）のどれでも写さない（`CARRY_MAX_FILES = 50`、
  `CARRY_MAX_LINES = 1000`）。各場合を単体テストで、件数の上限を実際の git のテスト（`a_large_push_never_carries_the_approvals`）で固定した。
  **残る危険は受け入れる**: Maintainer 以上が選ぶ opt-in（下の「設定を緩める変更」で保護の水準以上のロールが要る）で、保護ブランチでない
  target だけ。上限の中の小さな普通の変更なら、見ていないコードにも承認が残る。
- **一時停止は延ばせない（MEDIUM）**: 一時停止中にもう一度止める要求は `Unchanged`（何も書かず、答えは今の期限）。期限の過ぎた一時停止を
  別の切り替え（止める・戻す）で置き換えるときは、同じ変更に **期限の記録**（`merge_settings.codeowners_enforcement_restored` の監査の行と
  dedup つきのイベント）を先に入れる（worker が後で記録するものが残らないので、記録が失われない）。テスト
  `a_suspension_cannot_be_extended`・`a_new_suspension_after_an_expired_one_records_the_restore_first`。
- **期限の記録の失敗が他を止めない（MEDIUM）**: `restore_expired_codeowners` は 1 件の失敗で止まらず、失敗をプロジェクトと誤りの組で返す。
  worker が 1 件ずつ `forge.worker.codeowners_restore_failed`（warn、`error_code`）で出す。失敗した行は列
  `codeowners_restore_seen_at`（追記のマイグレーション 20261112100100。監査しない帳簿の列、`tables.toml` の `unaudited_columns`）を
  付けて次の走査の後ろへ回す（`codeowners_restore_seen_at NULLS FIRST`）。テスト `a_failing_restore_does_not_stop_the_batch`。
- **設定を緩める変更（MEDIUM、改訂）**: `MergeSettingsManage` を `Resource::MergeSettingsChange { project, highest_protection, loosens }`
  （`MergeSettingsCtx`）で判定する。`committer_approvals_excluded` か `reset_approvals_on_push` を on から off にする変更（承認の規則を
  緩める）は、CODEOWNERS の一時停止と同じく **プロジェクトの保護ブランチの最大の水準以上のロール** が要る（`above_own_role`、管理者は越える）。
  締める変更は Maintainer で足りる。ローダーは管理できる人にだけ今の設定と保護を読み、トランザクションの中でロックした設定に対して
  もう一度判定する（古い証明でも緩める変更は通らない。テスト `loosening_the_approvals_needs_a_role_at_or_above_every_protection`、
  権限の表の行）。方式の並びは設定ではない（集合として比べ、並べ替えだけは `Unchanged`）。
- **マージの状態の早送り（MEDIUM、改訂）**: 方式が `fast_forward` のとき、状態の表示も target の先端が head の祖先かを確かめて
  `ff_not_possible` を出す（ジョブはロックの下でもう一度決める）。`nothing_to_merge` はジョブだけが出す。
- **C1 までの必須のチェック（MEDIUM）**: 必須のチェックを設定したマージの条件の答え（`PUT …/merge-rule`）に `warnings:
  ["required_checks_unreportable"]` を付け、`gstn merge-rule set` は警告の 1 行を出す（報告が無いのでどのマージも止まり、直接の push の
  禁止と合わせるとブランチが動かなくなる）。逃げ道は runbook `merge-blocked`（チェックの名前を外す `--clear-checks`、またはマージの条件を消す）。
  拒否ではなく警告にしたのは、C1 の前に条件を用意しておけるようにするため。
- **LOW**: FG-GIT-0017 と FG-REVIEW-0016 に runbook `merge-blocked`。表の CHECK を `CheckName::parse` に合わせた（20261112100100:
  末尾の `/`・`//`・空白・101 バイト以上を拒む。テスト `the_table_refuses_check_names_check_name_refuses`）。理由の並びを文字列の表で固定し、
  この ADR の §7 の並びと一致することをテストで確かめる（`the_reason_order_is_fixed`・`the_reason_order_agrees_with_the_adr`）。
  一致する保護のルールが複数あるときの合わせ方を実際の git で確かめた（`several_matching_rules_combine_to_the_strictest`）。
  CODEOWNERS の切り替えのローダーは Maintainer 未満には保護を読まない。期限の誤り（なし・0・1441 以上）は判定の deny（403
  FG-AUTH-0003 `expiry_missing` / `expiry_out_of_range`）のままにした（権限の表の行にあるため。400 にしない）。`gstn merge-rule set` は
  今のマージの条件を読んでから指定した値だけを変える（`--clear-checks`、`--resolved-discussions <BOOL>`、`--direct-push-forbidden <BOOL>`）。
  権限の表に CODEOWNERS の切り替えの NonMember（internal ではない）と、直接の push の禁止の `write_repository` トークン（人・エージェント）の
  行を足した（サービスアカウント・ボットの `write_repository` の actor は表に無いので足していない。`RepoPush` は人だけの Action ではない）。
- **既知の窓**: ジョブの最後の判定（tx1b）の後・`update-ref` の前に変わった設定とマージの条件は、そのマージに反映されない（権限の窓と同じ、
  §4.2・R4 の節）。**直接の push の禁止は forge-web の push の経路（receive-pack の前の判定）でだけ効く**。サーバーのフック
  （pre-receive）で守るのは X8。
