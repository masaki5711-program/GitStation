# Runbook: secret-removal（FG-REVIEW-0002。関連: FG-AUTH-0002、FG-AUTH-0003、FG-GIT-0002 / 0003）

誤って push した秘密（パスワード、鍵、トークン、個人情報）を、マージリクエスト（MR）の版の隠した ref
（`refs/merge-requests/<iid>/v<n>` と `…/head`、ADR-0024 §2）から取り除く手順。版の ref は force push でも消えないように
レビューしたコミットを残す仕組みなので、**ブランチを書き換えただけでは秘密は残る**。

**秘密の漏れとして扱う。最初にすることは秘密の失効（ローテーション）で、ref を消すことではない。** ref を消しても、
clone 済みの手元、ミラー（O2）、バックアップには写しが残る。

## Trigger

- 利用者・レビュアーから「MR に秘密を push した」と報告があった
- `POST /api/v1/projects/{id}/merge-requests/{iid}/version-refs/purge` が `error_code=FG-REVIEW-0002` を返した
  - `reason=purge_requires_closed_or_rewritten`: 開いている MR の **今の head** の版を消そうとした（`version` に版の番号）
  - `reason=reopen_at_purged_head`: Purge 済みの MR を再オープンしようとしたが、source ブランチがまだ Purge した
    コミットを指している（`version`）

## Impact

- FG-REVIEW-0002 そのものは状態の拒否で、何も書かれておらず ref も変わっていない（SEV なし）。
- 秘密が残っている間は、そのリポジトリを読める人に見える（SEV2 以上。秘密の種類で上げる）。

## Diagnose（読み取りのみ）

1. どの MR のどの版に秘密が入ったか: `gstn mr view <iid> --project <id>` で `state` と `head_sha`。
   版の一覧は DB の読み取りで: `SELECT version_no, head_sha, pushed_by, refs_purged_at FROM forge.merge_request_versions
   WHERE merge_request_id = <id> ORDER BY version_no;`
2. 版の ref の今の値（読み取り）: `git --git-dir <repo-path> for-each-ref refs/merge-requests/<iid>`
   （`forge admin repo-path <repo_id>` でパスを出す）。
3. 同じコミットがブランチ・タグから届くか（届くなら版の ref を消しても読める）:
   `git --git-dir <repo-path> for-each-ref --contains <sha> refs/heads/ refs/tags/`

## Mitigate（どれも人が承認した手順。本番を変える）

1. `[MUTATING — needs approval]` **秘密を失効させる**（パスワードの変更、鍵の再発行、トークンの失効）。これが最優先。
2. `[MUTATING — needs approval]` **ブランチから取り除く**（戻し: 元のコミットへ force push し直す。版の ref が残っている間だけ可能）: 作成者に、秘密を含まない形で source ブランチを書き換えて force push してもらう
   （保護されたブランチなら、保護の手順に従う）。新しい版（`v<n+1>`）ができ、それが head になる。
   書き換えられないなら、MR をクローズする（`gstn mr close <iid> --project <id>`）。
3. `[MUTATING — needs approval]` **版の ref を消す**（戻し: 印 `refs_purged_at` は一度だけで戻せない。ref は `git update-ref refs/merge-requests/<iid>/v<n> <sha>` で手で作り直せるが、次の同期が消す。消す前に Diagnose 1 の一覧を記録に残す。管理者のブラウザのセッションだけ。トークンは `admin` スコープでも FG-AUTH-0002
   `session_required`、管理者でなければ FG-AUTH-0003）:
   - 開いている MR: 秘密を含む版だけを指定する（今の head は指定できない: FG-REVIEW-0002）。
     本文 `{"versions": [1, 2]}`
   - クローズ・マージ済みの MR: 本文 `{}`（すべての版）も可
   - ブラウザのセッションで（CSRF トークン付き）`POST /api/v1/projects/{id}/merge-requests/{iid}/version-refs/purge`。
     答え `{"iid", "purged", "already_purged"}`。監査の行 `merge_request.version_refs_purged`（`versions`）と
     ログ `event=forge.review.version_refs_purged` が残る。
   - DB の印（`refs_purged_at`）が先にコミットされ、その後に ref を消す。ref を消すのに失敗したら（FG-GIT-0002 / 0003）
     **同じ要求をもう一度送る**（`already_purged` の版の ref も毎回消す）。印のある版の ref は、版を作るジョブが二度と書かない。
4. `[MUTATING — needs approval]` **到達できないオブジェクトを消す**（戻せない。実行の前にバックアップがあることを確かめる。承認された保守の手順。猶予なしの prune は他の進行中の処理のオブジェクトも消しうるので、
   push とマージを止めた時間帯に行う）:
   `git --git-dir <repo-path> reflog expire --expire=now --all && git --git-dir <repo-path> gc --prune=now`
   その後、Diagnose 2 / 3 と `git cat-file -e <sha>`（失敗すれば消えた）で確かめる。
5. `[MUTATING — needs approval]` **写しを扱う**（戻し: 宛先ごとの手順に従う）: ミラー（O2）の宛先は `refs/merge-requests` を送らない設定だが、ブランチの履歴に秘密が入っていた期間の写しは
   宛先側でも同じ手順で消す。バックアップは版の ref を含む: 秘密を含むバックアップの扱い（保存期間・隔離）を決める。

6. **再オープン**（Purge した MR）: Purge した MR は、source ブランチが Purge した版のコミット **以外** を指すときだけ再オープンできる
（`reopen_at_purged_head`）。ブランチを書き換えてから再オープンすると、新しい版ができる。source を Purge した
コミットに force push で戻しても、そのコミットは head にならない（ジョブのログ `outcome=purged_head`）。

## Verify

- `git --git-dir <repo-path> for-each-ref refs/merge-requests/<iid>` に Purge した版の ref が無い
- `git cat-file -e <sha>` が失敗する（Mitigate 4 の後）
- `forge doctor` が緑（`subscribers.lag`、`jobs.dead`）

## Escalate

- Purge の要求が FG-SYS-0001 `version_refs_unconfigured` → `forge serve` の設定の不具合（Runbook `internal-error`）
- 版を作るジョブが dead になる（`jobs.dead`、kind `merge_request.refresh`）→ Runbook `job-dead-lettered`

ポストモーテムの feedback の候補:

- 秘密の push を防ぐ push 時の検査（X8、M3 後半）の優先度を見直す
- インシデントの記録（`ops/incidents/`）を残す
