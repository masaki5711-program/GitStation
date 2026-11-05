# Runbook: push-checks（FG-GIT-0021 / 0022 / 0023 / 0024。関連: FG-GIT-0001、runbook secret-removal、smart-http）

push の検査（M3 X8、ADR-0008 の追記 X8）は、receive-pack の **pre-receive フック**で、push が持ち込む新しいオブジェクト
（新しい先端から辿れて、push の前にどの ref からも辿れなかったもの）だけを、git の隔離（quarantine）の中で調べる。

- 大きいファイル: `git.http.push_max_blob_bytes`（既定 100 MiB）を超える blob → **FG-GIT-0021**（push 全体を拒否）
- 秘密: 秘密鍵（PEM / OpenSSH / PGP）、AWS のアクセスキー ID、GitHub・GitLab・Slack・Google のトークン、GitStation 自身の
  `fgp_` / `gss_` / `whsec_` → **FG-GIT-0022**（push 全体を拒否）。1 MiB を超える blob とバイナリは秘密の検査をしない
- 検査が動かなかった → **FG-GIT-0023**（alert。フックの中なら拒否 = fail closed。`hook_not_run` は ref が検査なしで更新された）
- 上限（新しいオブジェクト 50 万、秘密の検査 256 MiB、全体 60 秒）で止まった → **FG-GIT-0024**（warn。push は通す = fail open）

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
   - `reason=exe_unknown`（serve の起動時）: serve が自分の実行ファイルのパスを得られなかった。この serve では検査が動いていない
5. FG-GIT-0024: `incomplete`（`objects_cap` / `bytes_cap` / `deadline`）と `objects`。初回の大きな取り込みに多い。

## Mitigate

1. FG-GIT-0021: 利用者がファイルを履歴から外して push し直す（errors.toml の fix）。正当で、上限を上げると決めたときだけ:
   `[MUTATING — needs approval]` 設定 `git.http.push_max_blob_bytes`（環境変数 `FORGE_GIT_HTTP_PUSH_MAX_BLOB_BYTES`）を上げて serve を再起動する。
   戻し: 元の値に戻して再起動する。`git.http.push_max_bytes`（パック全体の上限）は別の設定。
2. FG-GIT-0022: 本物なら**先に失効**（runbook secret-removal の Mitigate 1）。それから利用者が履歴から取り除いて push し直す。
   合成のテストデータ（誤検知）なら、ファイルのどこか（コメント行）に `forge:allow-secret` を書いてもらう（MR のレビューで見える。push の行の `allowed` に数えられる）。
   管理者が検査を飛ばす操作はない（計画にない。必要になったら ADR で決める）。
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

## Verify

- 利用者の push が通る。ログに同じ `request_id` の `forge.git.push_checked`（`verdict=accepted`）が出る。
- FG-GIT-0023 が新しく出ていない。`<storage root>/push-checks` が空（push が動いていないとき）。

## Escalate

- FG-GIT-0023 `quarantine_invalid` が続く（git の版の変更が原因なら ADR-0008 の追記 X8 と `push_scan` を見直す）。
- `hook_not_run` が出た（git がフックを飛ばす経路が残っている: 不具合として扱い、feedback action にする）。

## Postmortem feedback

- 誤検知が多い規則は `forge-git` の `secret_scan` の規則とテストを直す（規則を足す・外すのは MR で、評価役 security-reviewer）。
- `hook_not_run` は回帰テスト（e2e）を足す。
