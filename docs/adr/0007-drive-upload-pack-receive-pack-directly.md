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

## Amendment（2026-11-01、M1 phase 9 MR3: fetch の公開と `want` の到達性の確認）

上の決定 (a)（forge が `want` を確かめてから upload-pack を起動する）を実装し、Smart HTTP の fetch / clone を公開した
（`feat/web-smart-http-fetch`）。決めた点は次のとおり。

- **要求の読み方**（`forge_git::upload_request`）: `POST git-upload-pack` の本文（gzip を展開したもの）を pkt-line として読む。
  - v0 / v1: `want <oid>[ <capability>…]` の行を集める（`have`、`shallow`、`deepen*`、`filter` はそのまま git に渡す）。
  - v2: 最初の行の `command=` が `ls-refs` と `fetch` のときだけ受け付ける。`object-info`、`bundle-uri` などほかのコマンドは、
    `want` なしに ID でオブジェクトの情報を返しうるので拒む（`FG-GIT-0014 reason=command_unsupported`、`command` 付き）。
    1 つの要求に 2 つ目のコマンド（flush の後ろ）があれば同じく拒む（`command=multiple`）。`fetch` の引数（delim の後ろ）の
    `want <oid>` を集める。`want-ref` は git に任せる（`uploadpack.allowRefInWant` は既定の false）。
  - oid は 40 / 64 桁の小文字の 16 進数だけを受け付ける。それ以外は `FG-GIT-0014 reason=want_invalid`。
- **確かめ方**（`Git::check_wants`、どれも captured / stream の git、全体を `git.http.want_check_timeout_ms`（既定 30 秒）で抑え、
  超えたら `FG-GIT-0002 operation=want_check`）。利用者の oid は検証した 16 進数だけを、**標準入力** で渡す（argv には入れない）。
  1. `git cat-file --batch-check='%(objectname) %(objecttype)'` で、重複を除いた `want` の種類を調べる。`missing` は到達できない。
  2. コミット: `git rev-list --max-count=1 --stdin --not --all`。`--stdin` は置いた場所で読まれるので、`want` が正の側、すべての
     ref が負の側になる。出力が空なら、どのコミットも ref から辿れる。普通の fetch（`want` = ref の先端）では ref を読むだけで終わる。
     （git が `--stdin` を `--not` の後で読むように変わると、すべてが辿れることになって穴が開く。テスト
     `unreachable_or_missing_wants_are_refused` がこれを固定している。）
  3. ツリー・blob・注釈付きタグ（partial clone の後からの取得、タグ）: 安くて正確な方法がない。`rev-list --objects --stdin --not --all`
     は、除外する先端のツリーに印を付けないので、辿れる blob も「辿れない」と答える（2.43 で確かめた）。そこで
     `git rev-list --objects --all --no-object-names` をストリームで最後まで読み、`want` の ID を探す（オブジェクト全体の走査）。
     この費用は、コミット以外を `want` する要求だけが払う。ストリームの枠は呼び出し元の主体の分として数える。
  - `--all` は upload-pack が広告する ref と同じ（forge は `transfer.hideRefs` / `uploadpack.hideRefs` を設定しない）。隠す ref を
    作るときは、この確認も同じ ref を除く必要がある（git ≥ 2.42 の `--exclude-hidden=uploadpack` など）。
- **拒否は in-band**: 辿れない `want` が 1 つでもあれば upload-pack を起動せず、HTTP 200（`application/x-git-upload-pack-result`）で
  `ERR <利用者向けの文言> (request_id: …)` の pkt-line を 1 つ返す。git は `fatal: remote error: …` と表示して止まる。ログには
  `FG-GIT-0014 reason=want_unreachable` を 1 回、`wants`、`commits`、`objects`、`unreachable`、`first_unreachable`、`check_ms`、
  `project_id`、`repo_id` 付きで出す。`have` はこれまでどおり git が答える（ID を知っているオブジェクトの有無が `ACK` で分かるのは、
  中身ではないので受け入れる）。
- **確かめた結果**（`crates/forge-cli/tests/smart_http_e2e.rs::unreferenced_object_cannot_be_fetched_by_id`）: 削除したブランチの
  コミットと blob を、git（v2 / v0）でも、手で組み立てた v0 / v2 の要求でも取り出せない。辿れる `want` には pack が返る。
  partial clone（`--filter=blob:none`）の後からの blob の取得と、古いコミットの blob の取得は通る。
- **費用の測定**（`want_check_cost_with_many_refs`、手で実行する ignored テスト。4 コア、git 2.43、debug ビルドの forge から
  git を起動。ブランチごとに 1 コミットと 1 blob を足した合成のリポジトリ）:

  | ref の数 | ref の形 | 先端 1 つ | すべての先端（clone） | 古いコミット | 辿れないコミット | blob（走査） | 辿れない blob（走査） |
  |---|---|---|---|---|---|---|---|
  | 1 000 | loose | 12 ms | 28 ms | 10 ms | 15 ms | 13 ms | 13 ms |
  | 20 000 | loose | 196 ms | 450 ms | 185 ms | 203 ms | 265 ms | 253 ms |
  | 20 000 | packed | 90 ms | 282 ms | 104 ms | 97 ms | 124 ms | 129 ms |

  コミットの確認の費用はほぼ ref を読む時間で、ref の数に比例する（`pack-refs` で約半分）。オブジェクトの走査はオブジェクトの数に
  比例する（この表では 4 万個ほど）。大きなリポジトリで partial clone の後からの取得が多くなれば、bitmap
  （`repack -a -d --write-bitmap-index`、`rev-list --use-bitmap-index`）か、結果のキャッシュで抑える（残り）。
- **起動の前に読む本文の上限**: fetch の要求は生の大きさと展開後の大きさの両方が `git.http.fetch_request_max_bytes`（既定 16 MiB、
  上限 1 GiB）まで（`FG-GIT-0013 limit=request_body` / `gzip_decompressed`）。`Content-Encoding` は `gzip` と `identity` だけ
  （ほかは `FG-GIT-0014 reason=content_encoding`）。`Content-Type` は `application/x-git-upload-pack-request` だけ
  （`reason=content_type`）。本文の読み込みは無通信 60 秒と全体 1 時間（`git.http.idle_timeout_ms` / `timeout_ms`、
  `FG-WEB-0002 operation=git_request_body`）、途中で切れたら `FG-GIT-0015 phase=request_body`（git は起動していない）。
- **レビューを受けて加えた（同日、MR3 の追加のコミット）**:
  - オブジェクトの走査は、コミット以外の `want` をすべて見つけた時点で git を止めて回収する（`walked` に読んだ数）。最後まで
    読むのは、辿れないものがあるときだけである。コミット以外の `want` は 1 つの要求で `git.http.max_object_wants`（既定 10 000）
    まで（超えたら走査の前に `FG-GIT-0014 reason=too_many_wants`。大きなツリーの partial clone の checkout で足りなければ上げる）。
  - `filter` に `sparse` を含むもの（`sparse:oid=<blob>`、`combine:` の中も）は拒む（`reason=filter_unsupported`）。`sparse:oid` は
    ID で blob を読んで sparse の型として使うので、辿れない blob の中身が型の解釈を通じて漏れうる。upload-pack にも
    `-c uploadpack.filter.sparse:oid.allow=false` を付けた（二重の守り）。
  - `have` と `shallow` の ID は確かめない。git の `ACK` と shallow の更新が明かすのは、その ID のオブジェクトがあるかどうかだけで、
    中身ではない（受け入れた残り。強制 push で消した内容があるかどうかは、ID を知っている読み手には分かる）。
  - 要求の本文を読むあいだも枠を持つ（forge-web の `BodySlots`。全体 `git.http.max_streams`、1 人 `max_streams_per_principal`、
    `FG-GIT-0004 reason=body_reads` / `principal_body_reads`）。本文は全体で `git.http.body_timeout_ms`（既定 5 分）以内に読み終える
    （`FG-WEB-0002 operation=git_request_body kind=total`）。遅い要求を何本も開いて本文をため込ませることを防ぐ。
- **残り**: git 2.43 の v2 の広告は `object-info` を含む（forge は要求を拒むので害はないが、広告から外す設定を確かめる）。
  ref の多いリポジトリでの確認の費用の監視（`check_ms` をメトリクスにする）、bitmap / キャッシュ。
