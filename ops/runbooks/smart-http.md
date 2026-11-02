# Runbook: smart-http（Smart HTTP の上限・枠・時間切れ・中断）

対象のコード: FG-GIT-0002（`kind=idle` / `kind=total` のストリーム、`operation=want_check`）、FG-GIT-0004（`reason=streams` /
`reason=principal_streams`）、FG-GIT-0013、FG-GIT-0014（`reason=want_unreachable` ほか）、FG-GIT-0015、FG-GIT-0001（`reason=hooks_dir`）、
FG-WEB-0007、FG-REPO-0016、FG-WEB-0002（`operation=git_request_body` / `protection_load`）、push の拒否（FG-GIT-0005 / 0007 / 0008 /
0009 / 0010 / 0011、M1 phase 9 MR4）。
仕組みは ADR-0007（M1 phase 9 の追記）と ADR-0015（MR3 の追記）、実装は `crates/forge-git`（`stream.rs`、`smart_http.rs`、
`upload_request.rs`、`want_check.rs`、push は `receive_request.rs`、`receive_report.rs`、`push_check.rs`）と
`crates/forge-web`（`routes/git.rs`、`routes/git_push.rs`、`git_stream.rs`）。push の判定の流れは ADR-0008（MR4 の追記、案 B）。

## Trigger

- clone / fetch / push の失敗を利用者が報告した、またはログに上のコードが続けて出ている
- `FG-GIT-0001 reason=hooks_dir`（alert）が出た
- push が失敗したあと、同じブランチへの push が `cannot lock ref` / `Unable to create '….lock': File exists` で失敗し続ける

## Impact

- 時間切れ・中断: その 1 回の clone / fetch / push が失敗する。push は ref が書き換わる前に止まれば何も変わらない。
- 枠の不足（`FG-GIT-0004`）: 混雑の間、新しい clone / fetch / push が 503 になる（SEV3、全員なら SEV2）。
- `hooks_dir`: すべての Smart HTTP が止まる（SEV2）。

## 既定の上限（設定 `git.http.*`、`[git.http]` の表または `FORGE_GIT_HTTP_*`。起動時に検証し、誤りは FG-SYS-0002）

| 項目（設定のキー） | 既定 | 超えたとき |
|---|---|---|
| ストリームの同時数（`git.http.max_streams`） | 16 | 10 秒待って `FG-GIT-0004 reason=streams`（503） |
| 1 人の利用者の同時数（`git.http.max_streams_per_principal`、鍵は `user:<id>`） | 4 | すぐに `FG-GIT-0004 reason=principal_streams` |
| 無通信（どちらの向きにも 1 バイトも流れない）（`git.http.idle_timeout_ms`） | 60 秒 | git を helper ごと強制終了、`FG-GIT-0002 kind=idle`。本文の読み込み中なら `FG-WEB-0002 operation=git_request_body kind=idle` |
| 1 回の全体（`git.http.timeout_ms`。git の POST は要求のタイムアウトの代わりにこれ） | 1 時間 | `FG-GIT-0002 kind=total` / `FG-WEB-0002 kind=total` |
| fetch の要求の本文（生と gzip の展開後の両方、`git.http.fetch_request_max_bytes`） | 16 MiB | `FG-GIT-0013 limit=request_body` / `gzip_decompressed`（413） |
| 本文を読み終えるまで（`git.http.body_timeout_ms`） | 5 分 | `FG-WEB-0002 operation=git_request_body kind=total` |
| 本文を同時に読む数（`max_streams` / `max_streams_per_principal` と同じ） | 16 / 1 人 4 | `FG-GIT-0004 reason=body_reads`（10 秒待つ）/ `principal_body_reads`（すぐ） |
| `want` の到達性の確認（`git.http.want_check_timeout_ms`） | 30 秒 | `FG-GIT-0002 operation=want_check`（504） |
| コミット以外の `want` の数（`git.http.max_object_wants`） | 10 000 | `FG-GIT-0014 reason=too_many_wants` |
| push のパックの大きさ（`git.http.push_max_bytes` → `receive.maxInputSize`） | 2 GiB | git の index-pack が止め、`unpack` の失敗が push の結果に出る |
| push の本文（生と展開後、`push_max_bytes` + 1 MiB） | 2 GiB + 1 MiB | `FG-GIT-0013 limit=push_body`（413、git を止め ref は変わらない） |
| push のコマンド部（ref の更新の一覧） | 1000 件、1 MiB | `FG-GIT-0014 reason=too_many_commands` / `FG-GIT-0013 limit=command_section` |
| 拒否した push の本文の読み捨て | 64 MiB | それを超えると接続を閉じる（拒否のログの `drain_error`） |
| push の後の種類とコミットの読み取り | 各 10 秒 | 数えた分で記録（`total_commits_partial`） |
| push の記録（`record_push`） | 30 秒以内に 3 回 | `FG-GIT-0012`（runbook push-record-failed） |

git の stderr は先頭 64 KiB だけ残す（ログの `stderr` はさらにその先頭の数行、`stderr_truncated` で切ったことが分かる）。
upload-pack と receive-pack には `keepAlive=5` を付けているので、git が数えている最中でも 5 秒ごとに keep-alive が流れ、
無通信の時間切れにはならない。無通信になるのは、相手（クライアントや途中のプロキシ）が止まっているときである。

## Diagnose（読み取りのみ）

1. ログを `error_code` と `request_id` で引く。`subcommand`（`upload-pack` / `receive-pack`）、`bytes_in`、`bytes_out`、
   `duration_ms` を読む。
2. `FG-GIT-0002 kind=idle`:
   - `bytes_in` が少ない（要求の途中で止まった）→ クライアント側の回線、またはリバースプロキシが本文をため込んでいる
     （`proxy_request_buffering` など）。プロキシの設定と時間切れを確かめる。
   - `bytes_out` が途中で止まった → クライアントが受け取りを止めた（回線、`http.lowSpeedTime`）。
3. `FG-GIT-0002 kind=total`: 1 つのリポジトリに集中していればその大きさ（`git count-objects -vH`）、全体なら
   ホストの負荷（`uptime`、`iostat -x 5 3`）を確かめる。
4. `FG-GIT-0004`: `reason=streams` は全体の同時数。同じ時間帯の clone / fetch / push の数を数える。
   `reason=principal_streams` は 1 つの主体が 4 本を超えた（CI の並列 clone、スクリプトの暴走が多い）。
5. `FG-GIT-0015`: `reason=dropped` は要求が途中で取り消された（たいていクライアントの切断）。同じ `duration_ms` で
   多数並んでいれば、プロキシの時間切れ（`proxy_read_timeout` など）と一致するかを見る。
6. `FG-GIT-0013` / `FG-GIT-0014`: 1 つのアドレスから続いていればスキャナや壊れたプロキシ。`reason` と `detail` を見る。
6a. `FG-GIT-0014 reason=want_unreachable`（利用者には `remote error: … (request_id: …)`）: 要求が、どの ref からも辿れない
   オブジェクト（`first_unreachable`）を `want` した。強制 push や削除したブランチの中身を ID で取り出そうとした要求で、拒否が正しい。
   正当な利用者が続けて当たる場合は、`git --git-dir=<path> for-each-ref --contains <first_unreachable>` で ref に含まれていないことを
   確かめ（読み取りのみ）、利用者の clone が古い（削除済みのブランチを追っている）ことを伝える。`command=fetch` の partial clone で
   多いときは `check_ms` を見る（オブジェクト全体の走査。大きいリポジトリでは時間がかかる）。
6b. `FG-GIT-0014 reason=command_unsupported`: v2 の `object-info` などを送るクライアント。forge は `ls-refs` と `fetch` だけを受け付ける。
6c. `FG-GIT-0002 operation=want_check`: 確認が時間内に終わらない。`check_ms` に近い値で続くなら、ref の数
   （`git --git-dir=<path> count-objects -v`、`for-each-ref | wc -l`）とオブジェクトの数を見る。ref が loose なら `pack-refs --all` で
   半分ほどになる（下の Mitigate 5）。
6d. `FG-REPO-0016`（503）: リポジトリが準備中。数秒で直らなければ FG-REPO-0012（runbook `repository-provisioning`）。
6e. `FG-WEB-0007`（400）: `info/refs` に `service` がない・重複・未知。dumb HTTP のクライアントかスキャナ。git ≥ 2.0 を使うよう伝える。
6f. 名前を変えたプロジェクトの古い URL: ログの `namespace.redirected`（`outcome=redirect` は 302、`outcome=served` は古いパスへの
   upload-pack の POST をその場で処理した）。同じ古いパスが続けば、所有者に remote の更新（`git remote set-url origin <今の URL>`）を促す。
6g. push の拒否（利用者には `! [remote rejected] <ref> (<FG コード> <文言>)` と `remote: <FG コード> <文言> (request_id: …)`）:
   ログの 1 行に `refs`（push の ref の数）、`denied_refs`、`denied_codes`、`first_denied_kind` がある。forge が git を起動する前に
   push **全体** を拒否したので、どの ref も変わっていない（拒否されなかった ref には `atomic push failure` と出る）。
   - FG-GIT-0008: 保護 ref の最低ロールに足りない。保護の設定（`protected_refs`）と利用者のロールを確かめる。
   - FG-GIT-0007: 保護 ref の force push、または既存の保護タグの付け替え。`reason=git_denied_non_fast_forward` は git 自身の拒否
     （`receive.denyNonFastForwards`）で、この場合は同じ push の他の ref が反映されていることがある（`forge.git.served` の
     `applied`）。保護ブランチを含む push では、保護されていないブランチの force push も git が拒否する（既知の制限。別の push に分ける）。
   - FG-GIT-0009 / 0010 / 0011 / 0005: 保護 ref の削除 / `refs/heads/`・`refs/tags/` 以外 / 40・64 桁の 16 進の名前 / 不正な名前。
   - `FG-WEB-0002 operation=protection_load`: 規則の読み込みが時間内に終わらない（データベース。runbook db-unavailable）。
6h. push は成功したのに記録がない: 同じ `request_id` の `forge.git.push_recorded`（記録済み、`deduped`）または FG-GIT-0012
   （runbook push-record-failed）を探す。`FG-GIT-0003 reason=report_unreadable` なら forge は git の答えを読めず記録していない:
   `git --git-dir=<path> for-each-ref` の値と push のコマンドを比べ、反映された ref を確かめる（読み取りのみ）。
7. `FG-GIT-0001 reason=hooks_dir`: `ls -lad <git の HOME>/empty-hooks` と `ls -la <git の HOME>/empty-hooks` で権限・中身・
   所有者を見る（消さない）。`detail=insecure_permissions` はグループかほかの利用者が書ける状態。
7a. エラーに `reap_failed` が付いている: 強制終了した git を回収できなかった。`pgrep -af "git.*<path>"` で残っている git と
   helper を探す。残っていればロックを持っているので、下の `.lock` の手順の前に終わるのを待つ（終わらなければ Escalate）。
8. 古い `.lock` ファイル（push が強制終了された、またはサーバーが落ちた）:
   - `forge admin repo-path <repo_id>` でパスを出し、`find <path> -name '*.lock' -mmin +10 -ls` で 10 分より古いものを探す。
   - そのリポジトリで動いている git がないことを確かめる: `pgrep -af "git.*<path>"`（何か出たら終わるまで待つ）。

## Mitigate

1. プロキシが原因: リバースプロキシで git の URL の本文のため込みを止め、時間切れを forge の上限（1 時間）より長くする。
   `[MUTATING — needs approval]` ロールバック: 変更前の設定ファイルを退避し、戻して再読み込みする。
2. 枠が足りない（正当な負荷）: `git.http.max_streams` / `git.http.max_streams_per_principal` を上げて再起動する。CPU と I/O に余裕があることを
   先に確かめる。
   `[MUTATING — needs approval]` ロールバック: 元の値に戻して再起動する。
3. 古い `.lock`（動いている git がないことを確かめたあと）: そのファイルだけを削除する。
   `[MUTATING — needs approval]` ロールバック: 削除の前に `cp -p <file> <退避先>` で退避しておく。ref の値は `.lock` ではなく
   本体の ref に入っているので、`.lock` を消しても ref は変わらない。
4. `hooks_dir`: 中身を退避先へ移し、ディレクトリを空にする（`insecure_permissions` なら `chmod 700` にする）。誰が置いたかが分かるまでインシデントとして扱う。
   `[MUTATING — needs approval]` ロールバック: 退避先から戻す（ただし戻すと Smart HTTP は止まったままになる）。
5. `want_check` の時間切れ（ref が多い）: そのリポジトリの ref をまとめる（`git --git-dir=<path> pack-refs --all`）。
   `[MUTATING — needs approval]` ロールバック: 不要（ref の値は変わらない。`packed-refs` の前の状態は `cp -p` で退避しておく）。
   それでも足りなければ `git.http.want_check_timeout_ms` を上げて再起動する（`git.http.timeout_ms` 以下）。
   `[MUTATING — needs approval]` ロールバック: 元の値に戻して再起動する。

## Verify

- 同じ clone / fetch / push が成功し、上のコードが新たに出ない
- `.lock` を消した場合: そのブランチへの push が通り、`git --git-dir=<path> fsck --no-dangling` がエラーを出さない
- `hooks_dir`: `FG-GIT-0001 reason=hooks_dir` が出なくなる

## Escalate

- 強制終了・失敗のあとの helper は止める作りになっている（無通信・全体の時間切れ・切断に加え、git 自身が 0 以外や
  シグナルで終わったときもグループごと止める）。正常終了（0）のあとに残った helper だけは止めない。

- 強制終了のあと git の helper（`pack-objects`、`index-pack`）が残っていた場合（`pgrep -af pack-objects`）は、プロセスグループの
  強制終了が効いていない。`forge-git` の不具合として起票する（`grandchildren_die_with_the_process_group` の想定外）。
- `empty-hooks` にファイルが置かれていた場合は、保存先と git の HOME に forge 以外が書けるかを調べ、セキュリティの問題として扱う。
