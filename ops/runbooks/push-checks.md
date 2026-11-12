# Runbook: push-checks（FG-GIT-0021 / 0022 / 0023 / 0024 / 0025 / 0026。関連: FG-GIT-0001、runbook secret-removal、smart-http）

push の検査（M3 X8、ADR-0008 の追記 X8）は、receive-pack の **pre-receive フック**で、push が持ち込む新しいオブジェクト
（新しい先端から辿れて、push の前にどの ref からも辿れなかったもの）だけを、git の隔離（quarantine）の中で調べる。

- 大きいファイル: `git.http.push_max_blob_bytes`（既定 100 MiB）を超える blob → **FG-GIT-0021**（push 全体を拒否）
- 秘密: 秘密鍵（PEM / OpenSSH / PGP）、AWS のアクセスキー ID、GitHub・GitLab・Slack・Google のトークン、GitStation 自身の
  `fgp_` / `gss_` / `whsec_` → **FG-GIT-0022**（push 全体を拒否）。1 MiB を超える blob とバイナリは秘密の検査をしない
- 検査が動かなかった → **FG-GIT-0023**（alert。フックの中なら拒否 = fail closed。`hook_not_run` は ref が検査なしで更新された）
- 秘密の検査が上限（新しいオブジェクト 50 万、256 MiB、60 秒）で止まった、または 1 MiB を超えるテキストを調べなかった → **FG-GIT-0024**（warn。push は通す = fail open）。
  **大きさの検査は止まらない**: 受け取ったオブジェクトをすべて流して読み、読み切れなければ拒否（FG-GIT-0023 `size_check_failed`、120 秒）

仕組み: `forge serve` が push ごとに `<storage root>/push-checks/<nonce>/policy` を書き、receive-pack を
`-c core.hooksPath=<storage root>/push-hooks` と環境変数 `FORGE_PUSH_CHECK_DIR`・`FORGE_PUSH_CHECK_EXE`（serve の実行ファイル）で起動する。
フックのスクリプト `<storage root>/push-hooks/pre-receive` は forge が書いた 3 行（`exec "$FORGE_PUSH_CHECK_EXE" hook pre-receive`）で、
`forge hook pre-receive` が検査し、`result` を書き、利用者向けの行（`remote: FG-GIT-…`。ファイル・コミット・規則だけで、秘密の値は出さない）を出す。
git の終了後、serve が `result` を読んでログに 1 行出し、run のディレクトリを消す。フックは DB に接続せず、秘密も持たない。

## Trigger

- 利用者から「push が `FG-GIT-0021` / `FG-GIT-0022` で拒否された」と問い合わせ
- ログに `error_code=FG-GIT-0023`（alert）。`reason` を見る（errors.toml の FG-GIT-0023 の cause に一覧）
- ログに `error_code=FG-GIT-0024`（warn）が続く

## Impact

- 0021 / 0022: 利用者の push が通らない（ref は変わらず、隔離のオブジェクトは捨てられる）。0022 の秘密は**利用者の手元から出ている**ので、
  本物なら漏れとして扱う（失効が最優先、runbook secret-removal の 1）。
- 0023（フックの中）: そのリポジトリ、またはすべてのリポジトリに push できない（SEV2。全リポジトリなら SEV1 相当で扱う）。
- 0023 `hook_not_run`: ref は**検査なしで**更新された。秘密・大きいファイルが入った可能性がある（SEV3、中身次第で上げる）。
- 0024: push は通った。検査されなかった部分がある。

## Diagnose（読み取りのみ）

1. 利用者の問い合わせ番号（`request_id`）でログを探す: `error_code`、`context` の `event=forge.git.push_checked`、`verdict`、
   `findings`、`rules`、`first_rule`、`first_path`、`first_blob`、`first_commit`、`objects`、`blobs_scanned`、`bytes_scanned`、`allowed`、`applied`。
2. FG-GIT-0021: `first_path` と大きさ。正当なファイルか（設計データ・バイナリの成果物か）を利用者と確かめる。
3. FG-GIT-0022: `first_rule` と `first_path`。テストの合成データなら誤検知（下の Mitigate 2）。本物かどうかは**値をログから探さない**
   （ログには無い）。利用者の手元で確かめてもらう。
4. FG-GIT-0023:
   - `reason=hook_no_result` / `env_missing` / `policy_unreadable`: フックが起動しない・run のディレクトリが読めない。
     - フックのスクリプト: `cat <storage root>/push-hooks/pre-receive`（生成された 3 行と同じか）、`ls -la <storage root>/push-hooks`（0700、他のファイルが無い）
     - forge の実行ファイル: `forge hook pre-receive < /dev/null` を手で実行すると `FG-GIT-0023 … (reason: env_missing)` が出る（出なければ別の実行ファイル）
     - ディスク・権限: `df <storage root>`、`ls -la <storage root>/push-checks`
   - `reason=git_failed`（`cause_code`、`subcommand`）: フックの中の git が失敗した。`cause_code` の runbook（git-process-failed / git-unavailable）
   - `reason=quarantine_invalid`（`detail`）: git が渡した隔離のパスがリポジトリのものでない（git の版の変化、シンボリックリンク）。`git --version`
   - `reason=hooks_dir` は FG-GIT-0001（`detail=push_hook_mismatch` / `unexpected_entry`）で出る: フックのディレクトリが forge のものと違う
   - `reason=hook_not_run`（`applied`）: git がフックを飛ばした。スクリプトの実行権限（Unix）、`core.hooksPath` を上書きする設定が無いか
     （`git config --show-origin --get-all core.hooksPath` を、`HOME=<storage root>` で）
   - `reason=run_dir`（`detail=create` / `remove` / `policy_write`）: `<storage root>/push-checks` の権限・空き
   - `reason=exe_unknown` / `exe_unusable`（serve の起動時）: serve が自分の実行ファイルのパスを得られない・使えない。**serve は起動しない**
   - `reason=size_check_failed`（`detail`、`cause_code`）: 受け取ったオブジェクトの大きさを読み切れなかった（120 秒、git の失敗）。push は拒否された。
     巨大な push なら分けてもらう。続くなら `df`、`git --version`、ディスクの遅さ
   - `reason=hook_unknown`: git の後で ref が読めず、反映が分からないまま result が無い。`hook_not_run` と同じに扱う（unchecked push を手で）
   - 起動時の `FG-GIT-0001 reason=hooks_dir`（`push_hook_missing` / `push_hook_mismatch` / `push_hook_not_executable` / `unexpected_entry`）: serve は起動しない。
     Mitigate 3 で直す
5. FG-GIT-0024: `incomplete`（`objects_cap` / `bytes_cap` / `deadline` / `large_blob_unscanned`）、`objects`、`skipped_large`、`skipped_binary`。初回の大きな取り込みに多い。
6. 免除（`forge:allow-secret`）: 同じ `request_id` の `forge.git.push_check_waived` の行（`rule`、`path`、`blob`）。本物の秘密を印で隠していないか、
   該当のファイルを MR で確かめる（値はログに無い）。
7. git の隔離: forge は git 2.38 以上を求め（`forge doctor` の git の版）、その git はパックを受け取れば必ず隔離する。隔離の変数が無い push は
   パックが無い（削除だけ、既存のコミットへの ref）ので、大きさの検査は測るものが無い。

## Mitigate

1. FG-GIT-0021: 利用者がファイルを履歴から外して push し直す（errors.toml の fix）。正当で、上限を上げると決めたときだけ:
   `[MUTATING — needs approval]` 設定 `git.http.push_max_blob_bytes`（環境変数 `FORGE_GIT_HTTP_PUSH_MAX_BLOB_BYTES`）を上げて serve を再起動する。
   戻し: 元の値に戻して再起動する。`git.http.push_max_bytes`（パック全体の上限）は別の設定。
2. FG-GIT-0022: 本物なら**先に失効**（runbook secret-removal の Mitigate 1）。それから利用者が履歴から取り除いて push し直す。
   合成のテストデータ（誤検知）なら、ファイルのどこか（コメント行）に `forge:allow-secret` を書いてもらう（MR のレビューで見える。push の行の `allowed` に数えられる）。
   管理者が検査を飛ばす操作はない。例外は**空のリポジトリへの 1 回限りの履歴の取り込み**だけ（下の「履歴の取り込み」。先端は検査する）。
3. FG-GIT-0023（フックの中、push できない）:
   - スクリプトが違う・余計なファイルがある: `[MUTATING — needs approval]` `mv <storage root>/push-hooks <storage root>/push-hooks.bad-<date>`。
     次の push で forge が作り直す。戻し: 元の名前に戻す（ただし違うスクリプトでは push は拒否され続ける）。
   - 実行ファイルが動かない: serve を正しいビルドで入れ直す（デプロイの手順。承認が要る）。
   - 直した後、利用者に push し直してもらう。
4. FG-GIT-0023 `hook_not_run` / FG-GIT-0024（検査されなかった push）: **unchecked push** の手順で、手で検査する（読み取りのみ）:

   ```sh
   # ログの request_id の push の範囲（forge.git.push_applied の行の old / new）を使う
   git --git-dir=<repo path> rev-list --objects <new> --not <old> \
     | git --git-dir=<repo path> cat-file --batch-check='%(objecttype) %(objectsize) %(objectname) %(rest)' \
     | awk '$1 == "blob" && $2 > 104857600'
   # 秘密の候補（値は画面に出さない: ファイル名だけ）
   git --git-dir=<repo path> grep -l -E -e '-----BEGIN [A-Z ]*PRIVATE KEY-----' -e '(AKIA|ASIA)[0-9A-Z]{16}' <new> --
   ```

   見つかったら FG-GIT-0021 / 0022 と同じに扱う（すでに ref から届くので、取り除くには runbook secret-removal）。
5. 残った run のディレクトリ（`<storage root>/push-checks/<nonce>`、push が終わっても残ったもの）は、push が動いていないときに
   `[MUTATING — needs approval]` 消してよい（中身は policy と result だけで、秘密は無い。戻し: 不要）。

## 履歴の取り込み（history import、2026-11-12）

既存のプロジェクトを**履歴ごと**持ち込むとき、古いコミットに鍵（合成・本物）が残っていると FG-GIT-0022 で push 全体が拒否される。
インスタンスの管理者は、**空の**リポジトリにだけ、1 回限りの取り込みを許可できる（ADR-0008 の追記「履歴の取り込み」）。

- 許可する: `[MUTATING — needs approval]` `forge admin repo allow-import <repo_id> [--ttl-hours 24]`（1〜168 時間。`repo_id` は
  `forge admin repo-path` と同じ id）。ref が 1 つでもあれば拒否（FG-GIT-0025 `repository_not_empty`、何も書かない）。`provisioning` も拒否
  （`repository_not_ready`）。監査ログに `repository.history_import_allowed`（期限 `expires_at`）。戻し: 期限まで待つ（使われなければ自然に切れる）。
- 許可の間の**最初の push**（リポジトリに ref が無いとき）:
  - push した各 ref の**先端のツリー**は今まで通り検査し、秘密があれば FG-GIT-0022 で拒否する（許可は残る）。
  - **履歴にだけある**秘密（どの先端のツリーにも無い blob）は拒否しない。利用者には `remote: FG-GIT-0022 warning: …` と 1 件ずつの
    ファイル・コミット・規則（値は出さない）。serve のログに 1 件ずつ `forge.git.push_history_finding`（warn、`request_id`、`rule`、`path`、
    `commit`、`blob`）。
  - ref が反映されると許可は使い切られ、監査ログに `repository.history_imported`（`history_findings`、`consumed`）と、見つかったもの 1 件ごとに
    `repository.history_secret_imported`（`severity=warning`、`rule`、`path`、`commit`、`blob`）が残る（最大 500 件。数はすべて数える）。
  - 2 回目以降の push は今まで通り全部を検査する。許可の期限が切れていれば、空のリポジトリでも全部を検査する。
- **取り込んだ側の責任**: 履歴に残った秘密が本物なら、**取り込んだ人（依頼した部署）が失効・交換する**。GitStation は履歴から消さない
  （消すなら runbook secret-removal）。管理者は `repository.history_secret_imported` の一覧を依頼者に渡し、失効の完了を確かめてから
  クローズする。
- 調べる（読み取りのみ）:

  ```sql
  SELECT repository_id, allowed_at, expires_at, consumed_at, consumed_request_id
    FROM forge.repository_history_imports WHERE repository_id = <repo_id>;
  SELECT created_at, request_id, details FROM forge.audit_log
   WHERE action LIKE 'repository.history_%' AND resource_id = <project_id> ORDER BY id;
  ```

- FG-GIT-0026（alert）: 取り込みの push は反映されたが、監査の行と許可の使用済みを書けなかった（DB の障害。`cause_code`）。見つかったものは
  同じ `request_id` の `forge.git.push_history_finding` の行にある。インシデントの記録に写し、依頼者に失効を求める。許可は期限まで残るが、
  リポジトリはもう空ではないので使われない。DB の原因は `cause_code` の runbook で直す。

## Windows の注意

- `<storage root>/push-hooks` と `push-checks` は Unix では 0700 で作るが、Windows ではモードを付けない（ACL は親から継承する）。
  storage root の ACL を、serve を動かすアカウントと管理者だけにしておく（`icacls <storage root>` で確かめる）。他の利用者が書けると
  フックのスクリプトを書き換えられる（forge は内容を毎回確かめるが、確かめた後から git が実行するまでの間は守れない）。
- 実行ファイルのパスは `\\?\` を外し、`/` に直してスクリプトに渡す（git for Windows の sh が実行する）。

## Verify

- 利用者の push が通る。ログに同じ `request_id` の `forge.git.push_checked`（`verdict=accepted`）が出る。
- FG-GIT-0023 が新しく出ていない。`<storage root>/push-checks` が空（push が動いていないとき）。

## Escalate

- FG-GIT-0023 `quarantine_invalid` が続く（git の版の変更が原因なら ADR-0008 の追記 X8 と `push_scan` を見直す）。
- `hook_not_run` が出た（git がフックを飛ばす経路が残っている: 不具合として扱い、feedback action にする）。

## Postmortem feedback

- 誤検知が多い規則は `forge-git` の `secret_scan` の規則とテストを直す（規則を足す・外すのは MR で、評価役 security-reviewer）。
- `hook_not_run` は回帰テスト（e2e）を足す。
