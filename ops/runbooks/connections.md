# Runbook: connections（FG-WEB-0008 ServerBusy、FG-WEB-0002 `operation=response_write`）

接続の公平さ（M1 段階 9 MR0）。`forge serve` の接続ループ（`crates/forge-web/src/server.rs`）は、リクエストを読む前に
接続の数を数え、上限を超えた接続には `503`（`Retry-After: 5`、本文は `error_code` / `user_message` / `request_id` だけ）を
返して閉じる。待たせない（OS のバックログに溜めない）。

## Trigger

- ログに `FG-WEB-0008` が出た（利用者には「サーバーが混み合っています」と `request_id`）
- ログに `FG-WEB-0002` で `operation=response_write kind=idle` が出た（応答を読まないクライアントを切った）
- 特定のクライアントやスクリプトが「接続できない」「503 になる」と報告した

## Impact

- 上限を超えた新しい接続だけが 503 になる。すでに開いている接続の処理は続く。
- `peer_limit` は 1 つの接続元だけが締め出される（SEV3）。`connection_limit` は全員が影響を受ける（SEV2）。
- `response_write` は、その 1 回の応答が途中で切れる（クライアントが読まなかったため）。

## 既定の上限（設定 `server.*`、`[server]` の表または `FORGE_SERVER_*`。起動時に検証し、誤りは FG-SYS-0002）

| 項目（設定のキー） | 既定 | 超えたとき |
|---|---|---|
| 接続の総数（`server.max_connections`） | 1024 | `FG-WEB-0008 reason=connection_limit`（503） |
| 1 つの接続元の接続数（`server.max_connections_per_peer`、総数以下。IPv6 は /64 単位、IPv4 は 1 アドレス） | 64 | `FG-WEB-0008 reason=peer_limit`（503） |
| ヘッダを読み終えるまで・keep-alive の無通信（`server.header_read_timeout_ms`、`keep_alive_max_ms` 以下） | 30 秒 | 接続を閉じる（デバッグログのみ） |
| 1 つの接続の寿命（`server.keep_alive_max_ms`） | 10 分 | 処理中のリクエストが終わってから閉じる（長い git の応答は切らない） |
| 1 つの接続のリクエスト数（`server.keep_alive_max_requests`） | 1000 | 最後の応答に `Connection: close` |
| 応答の書き込みが進まない時間（`server.write_idle_timeout_ms`） | 60 秒 | 接続を閉じる、`FG-WEB-0002 operation=response_write kind=idle` |
| リクエスト本文が届かない時間（`server.read_idle_timeout_ms`、`git.http.idle_timeout_ms` 以上） | 60 秒 | 本文の読み込みが失敗、`FG-WEB-0002 operation=request_read kind=idle` |
| API の JSON リクエスト本文が全部届くまでの時間（`server.body_read_timeout_ms`、`keep_alive_max_ms` 以下） | 10 秒 | 本文の読み込みが失敗、`FG-WEB-0002 operation=request_read kind=total`（少しずつ送り続ける相手も切る） |

- 本文の読み込みで相手が切断した・HTTP の形式が壊れた場合は `FG-WEB-0002 operation=request_read kind=aborted`（`source` に原因）。何も読み込まれていない。
- 書き込みの無通信は「ソケットの送信が詰まっている（相手が読まない）時間」で、1 バイトでも進めば数え直す。流れ続けている
  git のストリームは対象にならない（git 側の無通信は別で、`git.http.idle_timeout_ms`、runbook smart-http）。
- 本文の無通信は、ハンドラが本文を待っている間だけ数える（git fetch が応答を作っている間などは対象外）。短くしすぎると遅い push を切るので、`git.http.idle_timeout_ms` を下回る値は起動時に拒否される（FG-SYS-0002 `below_git_idle_timeout`）。
- 接続元は **ソケットの相手のアドレス**で、IPv6 は上位 64 ビット（/64）にまとめて数える（1 契約者が /64 を丸ごと持つため）。IPv4 射影アドレスは IPv4 として数える。信頼するプロキシの設定（`X-Forwarded-For`）はない。リバースプロキシの後ろでは全員が
  プロキシのアドレスに見えるので、`max_connections_per_peer` はプロキシ全体の上限になる。プロキシの想定同時接続数に
  合わせて上げること（プロキシが接続を使い回すなら、その数より十分大きく）。
- `FG-WEB-0008` のログは 1 秒に 1 行まで。その間の拒否の数は次の行の `suppressed` に入り、拒否が止まって 1 秒たつと（またはシャットダウン時に）`reason=summary` の 1 行で残りの数が出る。

## Diagnose（読み取りのみ）

1. ログを `error_code=FG-WEB-0008` で引く。`reason`（`peer_limit` / `connection_limit`）、`peer`、`limit`、`open`、`suppressed`
   を読む。応答本文の `request_id` はこの行の `request_id` と同じ。
2. `peer_limit` で `peer` が 1 つに偏る:
   - `peer` がリバースプロキシのアドレス → 上限が低すぎる（上の注意）。
   - それ以外 → そのクライアントが接続を開きすぎ・閉じていない。`ss -tn state established '( sport = :<port> )' | awk '{print $4}' | sort | uniq -c | sort -rn | head`
     で接続元ごとの数を確かめる。
3. `connection_limit` で偏りがない → 実際の負荷、または遅いクライアントの滞留。`ss -tn` の Send-Q が大きい接続が多ければ、
   読まないクライアント（`FG-WEB-0002 operation=response_write` が同じ時間帯に出ているはず）。
3a. `FG-WEB-0002 operation=request_read`: `peer` と `idle_ms`。本文を途中で止めるクライアント（遅い回線、スローロリス）。プロキシ経由ならプロキシのバッファリングを見る。
4. `FG-WEB-0002 operation=response_write`: `peer` と `idle_ms`。同じ `peer` で続くなら、遅い回線かクライアントの不具合。
   リバースプロキシ経由なら、プロキシがクライアントの遅さを吸収せずに forge へ伝えている（バッファリングの設定を見る）。

## Mitigate

1. 暴走したクライアント: そのクライアントを止める（持ち主に連絡）。必要なら前段のプロキシ / ファイアウォールで接続元を制限する。
   `[MUTATING — needs approval]` ロールバック: 設定を戻して再読み込みする。
2. プロキシの後ろで `peer_limit`: `server.max_connections_per_peer`（と必要なら `server.max_connections`）を上げて再起動する。
   総数以下に保つ。ファイルディスクリプタの上限（`LimitNOFILE`）が総数より大きいことを先に確かめる。
   `[MUTATING — needs approval]` ロールバック: 元の値に戻して再起動する。
3. 正当な負荷の `connection_limit`: `server.max_connections` を上げて再起動する（CPU・メモリ・DB のプールに余裕があるか確認）。
   `[MUTATING — needs approval]` ロールバック: 元の値に戻して再起動する。

## Verify

- 同じクライアントが接続でき、`FG-WEB-0008` が新たに出ない
- `ss -tn` の接続数が上限より十分低い

## Escalate

- 正常な負荷でも `FG-WEB-0008` が続き、上限を上げるとメモリや DB が逼迫する場合は、容量の問題として扱う（インスタンスの追加、M2 以降）。
- 接続が閉じられず `open` が上限に張り付いたままなのに `ss` の接続が少ない場合は、forge の不具合（接続の数え漏れ）。ログと `ss` の出力を添えて起票する。
