# ADR-0007: Smart HTTP は upload-pack / receive-pack を直接起動する（http-backend を使わない）

**Date**: 2026-10-26
**Status**: accepted
**Deciders**: Masaki Fujikawa（ユーザー）、Claude

## Context

Git のプロトコルは自前で実装せず `git` を実行する（CLAUDE.md §3-5）。実行の方法として、
`git http-backend`（CGI）を挟むか、`git upload-pack` / `git receive-pack --stateless-rpc` を直接起動するかを決める。
認可は起動の前に行い、失敗はサービスごとのエラーコードで記録したい。

## Decision

`forge-git` が `upload-pack` / `receive-pack` を `--stateless-rpc`（と `--advertise-refs`）で直接起動する。
広告の接頭辞（`# service=…`）、gzip の展開（上限付き）、ストリーミング、アイドルと全体のタイムアウト、
同時実行数の上限、終了コードからエラーコードへの対応を `forge-git` が持つ。
fetch / clone は `Git-Protocol` ヘッダを `GIT_PROTOCOL` として渡し protocol v2 に対応する。push は v0 / v1。

## Alternatives Considered

### git http-backend（CGI）
- **Pros**: プロトコルの細部を git に任せられる
- **Cons**: CGI ヘッダの解析、環境変数での設定が要り、失敗が CGI のステータス行に隠れる。上限や早期の拒否を制御しにくい
- **Why not**: 失敗をエラーコードで記録するという契約に合わない

## Consequences

### Positive
- 認可を起動前に行い、拒否のときは git を起動しない
- 失敗がサービスごとのエラーコード（`FG-GIT-*`）で記録される

### Negative
- 約 300 行のプロトコルの周辺処理を自前で持つ

### Risks
- 広告の形式の誤り → 実際の git クライアントでの E2E テスト（clone / fetch / shallow / partial / push）で検出する

## Amendment（2026-11-01、M1 phase 9 MR1: ストリーミング、v2、上限）

- **ストリーミングの実行**: `GitCommand::spawn_stream(principal)` が git を起動し、標準入力（書き込み口）と標準出力
  （読み出し口）を呼び出し側に渡す。子プロセスは監視のタスクが持ち、次をすべて受け持つ。
  - 標準エラーは先頭 64 KiB だけ残し（残りは読み捨てて git が詰まらないようにする）、ログにだけ出す。
  - **無通信の時間切れ**（どちらの向きにも 1 バイトも流れない時間。既定 60 秒）と**全体の時間切れ**（既定 1 時間）で
    git を強制終了する（`FG-GIT-0002`、`kind=idle` / `kind=total`）。upload-pack / receive-pack は `keepAlive=5` で
    数えている最中も keep-alive を送るので、無通信になるのは相手が止まったときだけである。
  - **helper ごと止める**: Unix（サーバー）では子を新しいプロセスグループで起動し、グループ全体に `SIGKILL` を送る。
    親を回収する前に送るので、グループの番号が他のプロセスに再利用されていることはない。Windows（開発機）は Job Object
    （閉じたら中のプロセスをすべて終わらせる設定）に入れる。ワークスペースは `unsafe` を禁じているので、安全な包み
    （`win32job`）を使う。それ以外の OS ではコンパイルを拒む。
  - ストリームを捨てる（HTTP の要求が取り消される＝たいていクライアントの切断）と git を helper ごと止め、
    `FG-GIT-0015`（`reason=dropped`）を記録する。呼び出し側が切断に気づいたときは `abort_client_disconnected()` で
    同じことをして、エラーを呼び出し側が記録する。
- **枠**: ストリーム専用の枠（既定 16）を、短い captured の git の枠（32）とは別に持つ。長い clone が短い git の呼び出しを
  止めないためである。10 秒待って空かなければ `FG-GIT-0004 reason=streams`。さらに 1 つの主体（呼び出し側が渡す鍵。
  利用者、匿名ならアドレス）につき 4 本までで、超えたら待たずに `FG-GIT-0004 reason=principal_streams`。
- **固定の `-c`**（要求の値は argv に入らない。入るのはリポジトリ ID から作ったパスと forge の設定だけ）:
  - upload-pack: `uploadpack.allowFilter=true`、`uploadpack.keepAlive=5`、`uploadpack.allowAnySHA1InWant=false`、
    `--strict`
  - receive-pack: `receive.fsckObjects=true`、`receive.maxInputSize=<push の上限>`、`receive.keepAlive=5`、
    `receive.advertisePushOptions=false`
  - 両方: `core.hooksPath=<git の HOME>/empty-hooks`（空のディレクトリ。ADR-0009 の追記）
  - 要求から git に届くのは `Git-Protocol` ヘッダだけで、`[A-Za-z0-9=:._-]` の 256 バイトまでなら `GIT_PROTOCOL` として渡し、
    それ以外は捨てる（git は v0 で答える）。forge が判断に使うプロトコルの版も、git が受け取った値だけから決める。
- **広告**: `info/refs` では v0 / v1 のとき git の出力の前に `# service=git-<service>` と flush を付ける。upload-pack の v2 では
  付けない（git の v2 の広告は `version 2` から始まる）。receive-pack には v2 がなく、v2 の要求にも v0 で答えるので常に付ける。
- **gzip**: 要求の本文の gzip は展開した大きさに上限を持って少しずつ展開し、上限に達したらそこで止める
  （`FG-GIT-0013 limit=gzip_decompressed`）。壊れた・途中で切れた・後ろに余計なものがある gzip は `FG-GIT-0014 reason=gzip`。
- **pkt-line**: 長さ 0001〜0003（v2 では 0003 だけ）と 65520 を超える長さ、16 進でない長さは `FG-GIT-0014 reason=pkt_line`。
  side-band（`side-band-64k`）の組み立てと分解も `forge-git` が持つ。
- **v2 の参照されていないオブジェクト（確認した事実）**: git 2.43 の protocol v2 の `fetch` は、ref から辿れないコミットや
  blob でも、オブジェクト ID を知っていれば送る。`uploadpack.allowAnySHA1InWant=false` はこれを止めない（v2 の仕様で
  「want は広告したものに限らない」）。v0 は参照されていないコミットを `not our ref` で拒む（ただし参照されていない blob は
  送る）。テスト `v2_fetch_serves_unreferenced_objects_despite_allow_any_sha1_false` がこの振る舞いを固定している。
  強制 push で消したはずの内容（誤って push した秘密など）を、ID を知っている読み取り権限の利用者が取り出せることになる。
  候補は (a) forge が v2 / v0 の要求の `want` を読み、ref から辿れることを git で確かめてから git に渡す、
  (b) upload-pack を v0 に限る（blob の穴は残る）、(c) 読み取り権限があれば見えてよいとして受け入れ、強制 push のあとは
  `git gc --prune` と秘密の失効で対処する、の三つだった。
  **決定（2026-11-01、ユーザーの「推奨で進める」による）: (a) を採る。** forge は upload-pack の要求（v0 / v2）の `want` を
  すべて読み、どれも ref から辿れることを git で確かめてから upload-pack を起動する。辿れないものが一つでもあれば git を
  起動せずに拒む。実装と確認の費用の測定は MR3（`feat/web-smart-http-fetch`）で行い、それまで Smart HTTP の fetch は公開しない。
- **レビューを受けて加えた（同日、MR1 の追加のコミット）**:
  - 親（git）が自分で失敗して終わったとき（0 以外の終了、またはシグナル）も、helper（`pack-objects`、`index-pack`）を残さない。
    Unix では親の終了を `waitid(WNOWAIT)` で見る（回収しないのでゾンビのまま番号が取られている）。失敗ならその時点で
    グループ全体に `SIGKILL` を送り、それから回収する。正常終了（0）のときは git が helper を待ち終えているので何もしない。
  - 強制終了した git を回収できたか確かめる。回収できなかったとき（kill の失敗、待機の失敗、5 秒以内に回収できない）は、
    返すエラーに `reap_failed` と `reap_error` を付け、境界で `request_id` とともに一度だけ記録される。git がまだ動いて
    ロックを持っているかもしれないことを示す。
  - 監視のタスクが途中で捨てられたとき（ランタイムの停止など）も、プロセスの木を止める（`ProcessTree` の `Drop`）。
  - `receive.maxInputSize` は 0 を「無制限」と読むので、上限は型で 0 を許さない（`NonZeroU64`）。
