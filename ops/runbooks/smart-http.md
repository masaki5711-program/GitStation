# Runbook: smart-http（Smart HTTP の上限・枠・時間切れ・中断）

対象のコード: FG-GIT-0002（`kind=idle` / `kind=total` のストリーム）、FG-GIT-0004（`reason=streams` /
`reason=principal_streams`）、FG-GIT-0013、FG-GIT-0014、FG-GIT-0015、FG-GIT-0001（`reason=hooks_dir`）。
仕組みは ADR-0007（M1 phase 9 の追記）、実装は `crates/forge-git`（`stream.rs`、`smart_http.rs`）。

## Trigger

- clone / fetch / push の失敗を利用者が報告した、またはログに上のコードが続けて出ている
- `FG-GIT-0001 reason=hooks_dir`（alert）が出た
- push が失敗したあと、同じブランチへの push が `cannot lock ref` / `Unable to create '….lock': File exists` で失敗し続ける

## Impact

- 時間切れ・中断: その 1 回の clone / fetch / push が失敗する。push は ref が書き換わる前に止まれば何も変わらない。
- 枠の不足（`FG-GIT-0004`）: 混雑の間、新しい clone / fetch / push が 503 になる（SEV3、全員なら SEV2）。
- `hooks_dir`: すべての Smart HTTP が止まる（SEV2）。

## 既定の上限（`GitConfig`、M1 phase 9 MR3 / MR4 で設定 `git.http.*` から渡す）

| 項目 | 既定 | 超えたとき |
|---|---|---|
| ストリームの同時数（`max_streams`） | 16 | 10 秒待って `FG-GIT-0004 reason=streams` |
| 1 つの主体（利用者またはアドレス）の同時数（`max_streams_per_principal`） | 4 | すぐに `FG-GIT-0004 reason=principal_streams` |
| 無通信（どちらの向きにも 1 バイトも流れない）（`stream_idle_timeout`） | 60 秒 | git を helper ごと強制終了、`FG-GIT-0002 kind=idle` |
| 1 回の全体（`stream_timeout_total`） | 1 時間 | 同じく `FG-GIT-0002 kind=total` |
| gzip の展開後の大きさ | 呼び出し側が指定（fetch 16 MiB 予定） | 展開を止めて `FG-GIT-0013 limit=gzip_decompressed` |
| push の大きさ（`receive.maxInputSize`） | 呼び出し側が指定（2 GiB 予定） | git の index-pack が止め、`unpack` の失敗が push の結果に出る |

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
7. `FG-GIT-0001 reason=hooks_dir`: `ls -la <git の HOME>/empty-hooks` で中身と所有者を見る（消さない）。
8. 古い `.lock` ファイル（push が強制終了された、またはサーバーが落ちた）:
   - `forge admin repo-path <repo_id>` でパスを出し、`find <path> -name '*.lock' -mmin +10 -ls` で 10 分より古いものを探す。
   - そのリポジトリで動いている git がないことを確かめる: `pgrep -af "git.*<path>"`（何か出たら終わるまで待つ）。

## Mitigate

1. プロキシが原因: リバースプロキシで git の URL の本文のため込みを止め、時間切れを forge の上限（1 時間）より長くする。
   `[MUTATING — needs approval]` ロールバック: 変更前の設定ファイルを退避し、戻して再読み込みする。
2. 枠が足りない（正当な負荷）: `max_streams` / `max_streams_per_principal` を上げて再起動する。CPU と I/O に余裕があることを
   先に確かめる。
   `[MUTATING — needs approval]` ロールバック: 元の値に戻して再起動する。
3. 古い `.lock`（動いている git がないことを確かめたあと）: そのファイルだけを削除する。
   `[MUTATING — needs approval]` ロールバック: 削除の前に `cp -p <file> <退避先>` で退避しておく。ref の値は `.lock` ではなく
   本体の ref に入っているので、`.lock` を消しても ref は変わらない。
4. `hooks_dir`: 中身を退避先へ移し、ディレクトリを空にする。誰が置いたかが分かるまでインシデントとして扱う。
   `[MUTATING — needs approval]` ロールバック: 退避先から戻す（ただし戻すと Smart HTTP は止まったままになる）。

## Verify

- 同じ clone / fetch / push が成功し、上のコードが新たに出ない
- `.lock` を消した場合: そのブランチへの push が通り、`git --git-dir=<path> fsck --no-dangling` がエラーを出さない
- `hooks_dir`: `FG-GIT-0001 reason=hooks_dir` が出なくなる

## Escalate

- 強制終了のあと git の helper（`pack-objects`、`index-pack`）が残っていた場合（`pgrep -af pack-objects`）は、プロセスグループの
  強制終了が効いていない。`forge-git` の不具合として起票する（`grandchildren_die_with_the_process_group` の想定外）。
- `empty-hooks` にファイルが置かれていた場合は、保存先と git の HOME に forge 以外が書けるかを調べ、セキュリティの問題として扱う。
