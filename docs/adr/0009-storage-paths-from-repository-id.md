# ADR-0009: リポジトリの保存先はリポジトリ ID だけから決める

**Date**: 2026-10-26
**Status**: accepted
**Deciders**: Masaki Fujikawa（ユーザー）、Claude

## Context

URL のパス（グループ / プロジェクト名）から保存先を作ると、名前の変更や移管でファイルの移動が必要になり、
パストラバーサルの危険もある。Windows の開発機では大文字小文字を区別しないファイルシステムと長いパスの問題もある。

## Decision

保存先は `<root>/repos/<sha256(repo_id) の先頭 2 桁>/<repo_id>.git`。
URL は DB で `RepoId` に解決してからパスを作り、URL の文字列をパスに使わない。
ルートは起動時に一度だけ正規化する（Windows では `\\?\` 接頭辞を外して比較）。
各パスがルートの下にあることと、シンボリックリンクでないこと（`symlink_metadata`）を確認する。

## Alternatives Considered

### 名前ベース（`group/sub/proj.git`）
- **Why not**: 名前の変更・移管でファイルが動く。トラバーサルの危険

### `<owner_id>/<repo_id>.git`
- **Why not**: 移管でファイルが動く

## Consequences

### Positive
- 名前の変更と移管が DB の更新だけで済む。パスに利用者の入力が入らない

### Negative
- ファイルシステムだけを見てもどのリポジトリか分からない → `forge admin repo-path <path>` で逆引きできるようにする

### Risks
- ルートの設定ミス → `forge doctor` がルートの存在、書き込み権限、シンボリックリンクでないこと、空き容量を検査する

## Amendment（2026-10-30、手順 D4 MR3a: 作成のやり直しと目印）

- リポジトリの作成は D4 からワーカーのジョブ（`repository.provision`）で行い、失敗や中断のあとにやり直されることがある。
  やり直しで同じ場所に二つ目を作ろうとして `FG-REPO-0004` で止まり続けないよう、**作ったリポジトリに目印を付け、自分の目印の
  ときだけ引き取る**。
- `init_bare` は一時ディレクトリで初期化と設定を終えたあと、最後に `forge.provisionToken=<repositories.provision_token>`
  （UUID）をそのリポジトリの設定に書き、それから名前を変えて置く。目印があるリポジトリは設定まで終わったものだけになる。
- `init_bare_or_adopt` は、置き場所に何もなければ作る（`Created`）。何かあれば、次をすべて満たすときだけ引き取る（`Adopted`）:
  シンボリックリンクではない本物のディレクトリでルートの下にある、その中の `config` が普通のファイル（ルートの外を指す
  リンクではない）、`forge.provisionToken` がジョブのトークンと同じ、ベアリポジトリである。引き取るときは設定とフックの場所を
  もう一度書く（何度書いても同じ結果）。
- それ以外（目印なし、別のトークン、ファイル、リンク、git が読めない設定）は今までどおり `FG-REPO-0004`。理由は
  `adopt_refused` に残し、トークンの値はログに出さない。こうして、**復元したデータベースが自分のものではないディレクトリを
  引き取ることはない**。git の時間切れ・混雑・未導入はそれぞれのコードのまま返し、ジョブのやり直しに任せる。
- 名前を変える前に止まった試みの一時ディレクトリ（`.tmp-<id>-<uuid>.git`）は試みごとに名前が違うので、やり直しを妨げない。
  自動では消さない（片付けは `forge doctor` などで後から扱う）。
- レビューを受けて次を加えた（同日）。
  - 目印は `git config --file <repo>/config --no-includes` で読む。`--local` でも `include.path` は辿られるので、
    設定に `include.*` / `includeIf.*` があれば引き取らない（`adopt_refused=includes`）。目印が二つ以上あるときも引き取らない
    （`duplicate_marker`）。`core.bare=false` は `not_bare`、git が読めない設定は `unreadable`。
  - 引き取りで設定を書く直前に、`config` が普通のファイルであることをもう一度確かめ、`--file <repo>/config` に書く。
    確かめてから書くまでのわずかな隙は残る。これは **保存先のルートには forge だけが書ける**（OS の利用者と権限で守る）
    という前提で受け入れる。この前提が崩れると、リンクへのすり替えで root の外のファイルに書かれうる。
  - 同時に二つの試みが走って名前の変更で負けた側は、そのまま一度だけ引き取りを試す（同じトークンなら `Adopted`、
    違えば `token_mismatch`）。
  - やり直してよい失敗の決まり: git の時間切れ・混雑・未導入、保存先の I/O の失敗、新しく作る途中の git の失敗は
    やり直す。既にあるディレクトリについての失敗（`adopt_refused` の付いた `FG-REPO-0004`。`config` を見る権限がない、
    git が読めない・書けない場合を含む）、パスの拒否、フックの場所の設定の誤りはやり直さず、運用者が見る。
    判定は `forge_git::storage::provision_error_is_retryable` に一つにまとめた。

## Amendment（2026-10-30、手順 D4 MR3b: 作成のジョブ、worker の git、doctor）

- `forge worker` は `repository.provision` のジョブ（60 秒）を処理する。手順: 行のトークンを書き込み用の接続で読む（`forge_ro` は読めない）→
  `init_bare_or_adopt` → `mark_ready`。行が既に `ready` ならディレクトリに触らない（`outcome=already_ready`）。
- **worker は `storage.root` が無いと起動しない**（`FG-SYS-0002 config_key=storage.root`、ルートが開けなければ `FG-REPO-0001`）。
  起動してからジョブが全部失敗するより、起動の時点で止める方が原因が分かりやすい（封印の鍵と同じ扱い）。
- worker の git は `PATH` の `git`、**HOME は保存先のルート**（`<root>/.gitconfig` が大域の設定になる。ルートには forge だけが書ける
  という MR3a の前提に乗る。共有の `/tmp` などを HOME にすると他人の置いた設定が効きうる）。1 回の git の呼び出しは 20 秒まで
  （ジョブの 60 秒より十分短く、止まった git は中断ではなくやり直せる `FG-GIT-0002` になる）。
- フックの場所（`core.hooksPath`）はまだ渡さない（設定 `git.hooks_path` は M1 phase 10 で足す）。引き取りは設定を書き直すので、
  そのときに既存のリポジトリにも行き渡る。
- やり直しの判定: 保存先の失敗は `provision_error_is_retryable` のとおり。DB の失敗は `FG-DB-0001` / `FG-DB-0002` ならやり直し、
  それ以外はすぐ `dead`。`mark_ready` が失敗してもディレクトリは目印付きで残るので、次の試行は引き取る。
- `forge doctor` の `repositories.storage` は `ready` のリポジトリのディレクトリがあるかを ID の順に最大 10 000 件確かめる
  （無ければ `FG-REPO-0002`、ほかの拒否はそのコード）。それを超えた分は確かめず、`truncated` で知らせる（黙って省かない）。
  `repositories.provisioning` は `provisioning` のまま 10 分を超えたもの、または生きたジョブの無いものを `FG-REPO-0012`（alert）で知らせる。
- 作りかけの一時ディレクトリ `.tmp-*` は今回も自動では消さない。探し方と消してよい条件は Runbook `repository-provisioning.md`。
- レビューを受けて次を加えた（同日）。
  - **1 回の実行に 1 つの期限**: 1 回 20 秒の git を何度か呼び、ファイル操作と `mark_ready`（最大 30 秒）も足すと、ジョブの
    60 秒を超えうる。そのときは実行器の一般的な時間切れになり、どの段で止まったかが分からなかった。作成のジョブはジョブの時間切れから
    期限を決める: 保存先の作業は前半（60 秒なら 30 秒）で `init_bare_or_adopt_until(…, deadline)` を使う。各 git の呼び出しは
    残り時間まで（`FG-GIT-0002`）、ファイル操作は残り時間の中で行い（`std::fs` のものは blocking のスレッドで）、超えたら
    `FG-REPO-0003`（`operation`、`reason=deadline`）。どちらもやり直す。`mark_ready_within` には残りから COMMIT の分（12 秒）を
    引いた時間（最低 1 秒）を渡す（超えたら `FG-DB-0002 reason=change_timeout`、やり直す）。
  - doctor の `repositories.storage` は `stat` を blocking のスレッドで行い、全体を 60 秒で打ち切る（`FG-REPO-0003
    operation=doctor_storage reason=check_timeout`）。`missing` と並べて `other`（リンク、ファイル、I/O）の件数を出す。
    `ready` のリポジトリのディレクトリが消えたことを見つけるのはこの検査だけである。

## Amendment（2026-11-01、M1 phase 9 MR1: 空のフックのディレクトリ）

- Smart HTTP の upload-pack / receive-pack には、毎回 `-c core.hooksPath=<git の HOME>/empty-hooks` を付ける。
  phase 9 ではフックを使わない（保護された ref の検査は forge が git を起動する前に行う。ADR-0008 の追記、pre-receive は
  phase 10 = M3）。リポジトリの中の `hooks/` や大域の設定のフックが、誰も確かめていない方針として動くことを防ぐ。
- このディレクトリは forge のもので、起動のたびに「本物のディレクトリで（リンクではない）、空である」ことを確かめる。
  無ければ権限 0700 で作る。グループやほかの利用者が書ける（`mode & 022`）なら、フックを置かれうるので
  `FG-GIT-0001 reason=hooks_dir detail=insecure_permissions` にする。中に何かあれば、どこの何かが分からないフックを動かさないよう git を起動せず `FG-GIT-0001 reason=hooks_dir`
  （alert、Runbook `smart-http`）にする。
- 保存先のリポジトリの `core.hooksPath`（`init_bare` の引数）は今までどおりで、phase 10 で forge のフックのディレクトリに
  切り替える。コマンド行の `-c` はリポジトリの設定より優先される。
