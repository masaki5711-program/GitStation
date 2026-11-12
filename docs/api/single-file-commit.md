# 1 ファイルのコミット（簡易エディター E1）

`POST /api/v1/projects/{id}/repository/commits/single-file`（`RepoPush`、Developer 以上。トークンは `write_repository`）。

Web 画面の簡易エディター（まず Markdown）から、**テキストファイルを 1 つ** 作成または置き換えてコミットする。
コミット先は次の 2 通り。

- **新しいブランチ**（`create_branch`、画面の既定）: 元のブランチの先端から作り、既定でそのブランチへのマージリクエストを開く。
- **今のブランチへ直接**: そのブランチが保護されておらず（または保護の最低ロールを満たし、直接 push 禁止でない）、push してよいときだけ。

このコミットは **その 1 コミットの push と同じに振る舞う**。保護ブランチの規則（`FG-GIT-0008`、直接 push 禁止 `FG-GIT-0017`、
予約された名前空間）、push の秘密情報の検査（`FG-GIT-0022`、`forge:allow-secret` の印は push と同じく例外として記録）、
push の記録（`repository.pushed` の監査行とイベント。パイプライン・MR の版・Webhook・フィードはこれに続く）、閲覧のキャッシュの破棄。
文書の本格的なエディター（計画 13 §7）は後の段階。gstn のコマンドは後で足す（`coverage.rs` で Planned）。

## 本文（JSON、上限 1 MiB）

| 欄 | 型 | 必須 | 意味 |
|---|---|---|---|
| `branch` | 文字列 | ○ | ファイルを読んだブランチ（`main` または `refs/heads/main`）。存在すること |
| `start_sha` | 文字列 | ○ | ファイルを読んだときのコミット（40 / 64 桁の 16 進）。下の「上書きしない」 |
| `path` | 文字列 | ○ | リポジトリの根からのパス（`docs/spec.md`）。無ければ作る（途中のディレクトリも） |
| `content` | 文字列 | ○ | 新しい内容の全体。UTF-8、NUL を含まない、512 KiB 以下（サーバーの push のブロブ上限が小さければそれ） |
| `message` | 文字列 | ○ | コミットメッセージ（空白だけは不可、64 KiB 以下。制御文字は改行とタブだけ） |
| `create_branch` | 文字列 / なし | | このブランチを **新しく作って** そこへコミットする（`branch` の先端から）。既にあれば 409 |
| `open_merge_request` | 真偽 / なし | | `create_branch` のとき `branch` への MR を開くか（既定 true）。`create_branch` 無しで true は 400 |
| `title` | 文字列 / なし | | MR の題（既定はメッセージの 1 行目。1〜255 文字、制御文字なし） |

知らない欄は 400 `FG-WEB-0006`（`unknown_field`）。

## 上書きしない（競合の確認）

1. ブランチの今の先端を読む。`start_sha` と同じなら、その上にコミットする。
2. 違う（その間に誰かが push した）とき、`start_sha` と先端とで **そのファイルが同じ**（同じブロブ・同じモード、どちらにも無いも同じ）なら、
   先端の上にコミットする（ほかのファイルだけの変更は競合にしない）。違えば 409 `FG-GIT-0027 reason=file_changed`。
   `start_sha` がリポジトリのコミットでなければ 409 `reason=start_unknown`、先端の祖先でなければ（強制 push の後など）409 `reason=start_not_ancestor`。
3. ref は `update-ref` の compare-and-swap 1 回で動かす（直接なら「先端がまだ読んだ値なら」、新しいブランチなら「まだ無ければ」）。
   その間に push が割り込めば、先端を読み直して 1 からやり直す（全部で 3 回。だめなら 409 `reason=ref_moved`）。

## 答え（201）

| 欄 | 意味 |
|---|---|
| `commit_sha` | 新しいコミット |
| `parent_sha` | 親（そのときのブランチの先端） |
| `branch` | コミットしたブランチ（`refs/heads/` を除く） |
| `branch_created` | このコミットでブランチを作ったか |
| `path` / `blob_sha` | ファイルとその新しいブロブ |
| `merge_request` | 開いた MR（MR の API と同じ形）。頼まれていない・開けなかったときは null |
| `merge_request_error` | MR を頼まれたのに開けなかったとき `{ "error_code", "message" }`（コミットとブランチはそのまま残る。ログに 1 行）。MR を開くのは `MergeRequestCreate` なので、トークンは `api` も要る（`write_repository` だけなら `FG-AUTH-0002` がここに入る） |

## 断るとき

| 状態 | コード | いつ |
|---|---|---|
| 400 | `FG-WEB-0006` | 欄が不正（`param` と `rule`: `branch` の `not_a_branch` / `invalid_ref` / `hex_like`、`start_sha` の `oid`、`message` の `blank` / `too_long` / `control`、`title` の `control`、`create_branch` の `same_as_branch`、`open_merge_request` の `needs_create_branch`、`title` の `length`） |
| 403 | `FG-AUTH-0003` / `FG-AUTH-0002` | Developer 未満 / トークンに `write_repository` が無い |
| 404 | `FG-REPO-0005` | 見えないプロジェクト |
| 404 | `FG-REPO-0023` | `branch` が無い（`reason=ref_not_found`） |
| 403 | `FG-GIT-0008` / `FG-GIT-0017` | 保護ブランチの最低ロールに足りない / 直接 push 禁止（新しいブランチなら通る） |
| 409 | `FG-GIT-0027` | 上の「上書きしない」（`file_changed`、`start_unknown`、`start_not_ancestor`、`branch_exists`、`branch_name_conflict`（`edit` があるときの `edit/x` など、git が同時に持てない名前）、`ref_moved`） |
| 429 | `FG-WEB-0010` | 呼び出した人ごとに 1 分 30 回まで（`Retry-After`） |
| 500 | `FG-GIT-0002` | git が 1 手 10 秒、全体 20 秒（`operation=single_file_commit`）で終わらなかった。ref の入れ替えは始めたら打ち切らない |
| 413 | `FG-GIT-0029` | 内容が大きすぎる |
| 422 | `FG-GIT-0022` | 秘密情報らしい内容（規則の名前だけをログへ。値は出さない） |
| 422 | `FG-GIT-0028` | パスが不正（`rule`: `empty`、`absolute`、`backslash`、`empty_segment`、`dot_segment`、`dot_git`、`control`、`too_long`、`path_is_directory`、`parent_is_file`、`not_regular_file`、`refused_by_git`）。`dot_git` は Windows / macOS のファイルシステムが `.git` と読む名前すべて（大文字小文字、末尾の点と空白、`:` 以降、ゼロ幅文字、`git~1`） |
| 422 | `FG-GIT-0030` | NUL を含む（テキストでない） |
| 422 | `FG-GIT-0031` | 内容が先端のファイルと同じ（何もコミットしない） |

どの断りでも ref は動かない。書いたブロブ・ツリー・コミットはどの ref からも届かず、`gc` が消す。

## 実装の約束（診断のため）

- git は引数ベクトルで実行する（`forge_git::file_commit`）: `hash-object -w --stdin --no-filters`、一時インデックス
  （`<git home>/file-commits/<uuid>.index`、`GIT_INDEX_FILE`、どの終わり方でも消す）に `read-tree`・`update-index --add --index-info`
  （エントリは標準入力）・`write-tree`、`commit-tree`（フックは空ディレクトリ、メッセージは標準入力、作者とコミッターは
  アカウントの表示名とメール。マージと同じ規則）。どれも 10 秒、全体で 20 秒。インデックスのコマンドは `core.protectNTFS` / `core.protectHFS` 付き。
  サーバーが途中で止まって残った一時インデックス（1 時間より古いもの）は次のコミットで消す（`forge.git.file_commit_index_swept`）。
- 成功の行: `forge.git.single_file_committed`（`request_id`、`branch`、`created`、`commit`、`parent`、`attempts`）と、push と同じ
  `forge.git.push_applied`（`stage=final`、`source=single_file_commit`。記録に失敗したときの再生の入力）、`forge.git.push_recorded`。
  記録が 3 回とも失敗すれば `FG-GIT-0012`（runbook push-record-failed）。
- 断りのログは `route=single_file_commit`、`project_id`、`repo_id` を持つ。
