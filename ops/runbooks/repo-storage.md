# Runbook: repo-storage（FG-REPO-0001 / FG-REPO-0002）

## Trigger

- `FG-REPO-0001`（保存先のパスの拒否）または `FG-REPO-0002`（リポジトリのディレクトリがない）が出た
- 起動時に保存先のルートが使えない

## Impact

- `FG-REPO-0001` でルートが使えない: すべてのリポジトリが使えない（SEV1）
- 特定のリポジトリだけ: そのリポジトリの操作が失敗する（SEV2〜3）

## Diagnose（読み取りのみ）

1. ログの `reason` と `repo_id` を読む。
   - `root_not_dir` / `root_symlink`: `storage.root` の設定、またはマウントの問題
   - `symlink` / `not_dir` / `outside_root`: ルートの下に想定外のシンボリックリンクやファイルがある（**改ざんの可能性も考える**）
   - `create_failed`: シャードのディレクトリを作れない（権限、空き容量）
   - `FG-REPO-0002` の `path`: ディレクトリがない
2. `forge doctor` の保存先の項目（存在、書き込み権限、シンボリックリンクでないこと、空き容量）を確認する。
3. 保存先のボリュームがマウントされているか（`findmnt`、`df`）。
4. `FG-REPO-0002` の場合、削除の猶予期間の後の purge ジョブが動いたか、監査ログで `project.deleted` を確認する。

## Mitigate

1. ボリュームを再マウントする、または `storage.root` を直す。`[MUTATING — needs approval]`
2. 想定外のシンボリックリンクは、**消す前に記録を取り**、セキュリティの事象として扱うか判断する。`[MUTATING — needs approval]`
3. ディレクトリが失われた場合は、バックアップまたはミラーから復元する。`[MUTATING — needs approval]`

## Verify

- `forge doctor` の保存先の項目が ok
- 対象のリポジトリの clone が成功する

## Escalate

- シンボリックリンクによる保存先の外への誘導が見つかった場合は、`/incident` でセキュリティの障害として扱う。
