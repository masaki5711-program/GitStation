# Runbook: repo-storage（FG-REPO-0001 / 0002 / 0003 / 0004）

## Trigger

- `FG-REPO-0001` StoragePathRejected（保存先のパスの拒否）
- `FG-REPO-0002` StorageMissing（リポジトリのディレクトリがない）
- `FG-REPO-0003` StorageIoFailed（保存先の読み書きの失敗）
- `FG-REPO-0004` RepoPathOccupied（作成しようとした場所に既に何かある）
- 起動時に保存先のルートが使えない

## Impact

- ルートが使えない、または多数のリポジトリで発生: すべてのリポジトリが使えない（SEV1）
- 特定のリポジトリだけ: そのリポジトリの操作・作成が失敗する（SEV2〜3）
- `FG-REPO-0004`: プロジェクトの作成が失敗する。**別のプロジェクトのデータが残っている可能性がある**ため、データの露出の観点でも扱う

## Diagnose（読み取りのみ）

1. ログの `reason` / `operation` と `repo_id` を読む。
   - `FG-REPO-0001` の `reason`
     - `root_missing` / `root_not_dir` / `root_symlink`: 保存先の設定、またはマウントの問題
     - `symlink` / `not_dir` / `outside_root`: ルートの下に想定外のシンボリックリンクやファイルがある（**改ざんの可能性も考える**）
   - `FG-REPO-0003` の `operation`: `inspect`、`create_shard`、`rename`。原因（権限、空き容量、I/O エラー）は `source`
   - `FG-REPO-0002` / `0004` の `path`: 対象のディレクトリ
2. `forge admin repo-path <repo_id> --storage-root <保存先>` でパスと状態（`exists` / `missing`）を確認する。拒否された場合はエラーコードが表示される。
3. `ls -la <path の親>` で、シンボリックリンクや想定外のファイルを確認する。
4. ボリュームの状態: `findmnt <保存先>`、`df -h <保存先>`、カーネルログ（`journalctl -k`）の I/O エラー。
5. `FG-REPO-0004`: `git --git-dir=<path> log -1 --format='%H %an %ad'` で中身を確認し、どのプロジェクトのものか DB と突き合わせる。DB を復元して ID の連番が巻き戻っていないか確認する。
6. 作成の失敗で `cleanup_failed` がある場合は、そこに書かれた一時ディレクトリ（`.tmp-<id>-…git`）が残っている。

（`forge doctor` による保存先の検査は M1 の後半で追加する。追加したらこの手順に加える。）

## Mitigate

1. ボリュームを再マウントする、または保存先の設定を直す。
   `[MUTATING — needs approval]` ロールバック: 変更前のマウント設定（`/etc/fstab` の該当行）と設定ファイルを記録しておき、元に戻す。
2. 想定外のシンボリックリンク・ファイル: **消す前に** `ls -la` の出力とリンク先を障害記録に残し、セキュリティの事象として扱うか判断する。そのうえで別の場所へ移す（削除しない）。
   `[MUTATING — needs approval]` ロールバック: 移した場所から元の場所へ戻す。
3. `FG-REPO-0004`: ディレクトリを **削除せず** 別名へ移す（`mv <path> <path>.occupied-<日付>`）。そのうえで DB のリポジトリ ID の連番を、既存の最大の ID より先に進める。
   `[MUTATING — needs approval]` ロールバック: 別名から元の名前へ戻す。連番は進めたままでよい（戻すと再発する）。
4. 権限・空き容量（`FG-REPO-0003`）: 所有者を forge の実行ユーザーに直す、または空きを作る。
   `[MUTATING — needs approval]` ロールバック: 変更前に記録した所有者・権限に戻す。
5. ディレクトリが失われた（`FG-REPO-0002`）: バックアップ（M3 で手順書を作成予定）またはミラーから復元する。
   `[MUTATING — needs approval]` ロールバック: 復元したディレクトリを別名へ移す。
6. 残った一時ディレクトリ（`cleanup_failed`）: 中身を確認してから削除する。
   `[MUTATING — needs approval]` ロールバック: 削除の前に別名へ移しておく。

## Verify

- `forge admin repo-path <repo_id> --storage-root <保存先>` が `exists` を表示する
- 対象のリポジトリの clone が成功する。作成が失敗していた場合は、再作成が成功する

## Escalate

- シンボリックリンクによる保存先の外への誘導、または `FG-REPO-0004` で別のプロジェクトのデータが見つかった場合は、`/incident` でセキュリティの障害として扱う。
