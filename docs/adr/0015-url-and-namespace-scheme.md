# ADR-0015: URL と名前空間の設計 — GitHub の語彙を `/-/` の後ろに置き、入れ子のグループと両立させる

**Date**: 2026-10-27
**Status**: accepted
**Deciders**: Masaki Fujikawa（ユーザー）、Claude

## Context

ユーザーは、画面・URL・使い勝手を GitHub に合わせたい（`/{owner}/{repo}`、`/pull/{n}`、`/blob/{ref}/{path}`、
`/tree/...`、`/commits/...`、`/settings`、clone は `https://<host>/{owner}/{repo}.git`）。一方、権限のモデルは
GitLab 型で、入れ子のグループ → プロジェクト、5 段階のロールに決まっている（計画 01 §3.2、§11-3、ADR-0010）。
入れ子があるので、owner の部分が複数のセグメントになる（`/acme/platform/infra/forge-web`）。そのため
`/acme/platform/blob/...` の `blob` が、ルートの語なのかサブグループの名前なのかを、文字列だけでは決められない。

ほかの制約は次のとおり。

- 名前の変更、移管、削除は Must（計画 01 §3.1）。古い URL（kanflow のリンク、手元の `remote.origin.url`、CI やデプロイの設定）が、黙って別のリポジトリを指すようになってはいけない。
- Smart HTTP の URL の形は git クライアントが決める（`<url>/info/refs?service=…`、`<url>/git-upload-pack`）。
- ルートの登録（ADR-0010、`crates/forge-web/src/registry.rs`）は、ルーター・OpenAPI・`routes.generated.json` の唯一の元である。登録は 1 セグメントの `{param}` しか表せない。axum 0.8 のワイルドカード `{*rest}` は末尾に 1 つしか置けない。
- 保存先はリポジトリ ID だけから決まる（ADR-0009）。作業項目は kanflow から来る（計画 01 §3.5、計画 02）。文書ポータル（§3.7、M7）は別の画面の系統である。
- ログの 1 行から「どのルートの型で、どのリソースの ID か」が分かる必要がある（CLAUDE.md §2）。

## Decision

**URL の文字列の形は 3 つだけにする。どの形かは文字列だけで決まる（DB を見ない）。**

| 形 | 文字列の規則 | 例 |
|---|---|---|
| 名前空間のトップ | `-` のセグメントがなく、`.git` で終わるセグメントもない | `/acme/platform/infra/forge-web` |
| 名前空間 + 画面の語 | 最初の `-` のセグメントで区切る。後ろは GitHub の語彙。`-` より前に `.git` で終わるセグメントはない | `/acme/platform/forge-web/-/blob/main/README.md` |
| Smart HTTP | 名前空間の **最後の** セグメントだけが `.git` で終わる。その直後が固定の 3 種類のどれか。**`.git` は必須** | `/acme/platform/forge-web.git/info/refs?service=git-upload-pack` |

`/api/v1/…` は API である。トップレベルの予約語（下記）は名前空間ではない。**`.git` で終わるセグメントが許されるのは、Smart HTTP の形の、名前空間の最後のセグメントだけである。**
それ以外の場所（最後以外のセグメント、`/acme/x.git` のように git の固定の後ろがないもの、`/acme/x.git/-/blob/…`）は、どれも FG-WEB-0001（`reason=path_rejected`）にする。

### ルートの系統と認可（1 ルート = 1 Action）

| 系統（URL） | 処理する場所 | Action | Resource | スコープ |
|---|---|---|---|---|
| 画面の殻: 名前空間のトップ、`/-/tree` `blob` `blame` `commits` `commit` `compare` `branches` `tags` `settings` `members`、固定リンク `/-/projects/{id}/…` | サーバー（SPA の `index.html`） | `WebShell`（公開） | `Instance` | なし |
| 上の画面のデータ（ファイル、コミット、ブランチ、blame、比較） | API（`/api/v1/projects/{id}/repository/…?ref=&path=`） | `RepoRead` | `Project` | `read_repository` |
| プロジェクト `GET /api/v1/projects/{id}`（固定リンクの SPA が呼ぶ） | API | `ProjectRead` | `Project` | `read_api` |
| `/-/raw/{ref}/{file}`、`/-/archive/{ref}.{zip\|tar.gz}` | サーバー（中身をそのまま返す） | `RepoRead` | `Project` | `read_repository` |
| 設定、メンバー（プロジェクトとグループ） | API | 既存の `ProjectRead` / `ProjectUpdate` / `ProtectedRefManage` / `MemberManage` / `GroupRead` / `GroupUpdate` | 既存 | 既存 |
| パスの解決 `GET /api/v1/namespaces/resolve?path=` | API | `PathResolve`（新規） | `Unresolved` / `Resolved` | `read_api` |
| `GET {path}.git/info/refs?service=git-upload-pack`、`POST {path}.git/git-upload-pack` | サーバー | `RepoRead` | `Project` | `read_repository` |
| `GET {path}.git/info/refs?service=git-receive-pack`、`POST {path}.git/git-receive-pack` | サーバー | 既存の `RepoPush`（ref なしのプロジェクト単位の判定。Developer 以上） | `Project` | `write_repository` |
| push の各 ref の更新 | pre-receive（ADR-0008） | 既存の `RepoPush` | `Ref` | `write_repository` |
| パスの変更・移管 | API | `ProjectTransfer`（既存。Resource を拡張）/ `GroupTransfer`（新規）/ `UserRename`（新規） | `Transfer` / `GroupTransfer` / `UserRename` | `api` |
| 墓標の解放 | API | `NamespaceTombstoneRelease`（新規） | `Tombstone` | `api` |
| プレビュー用のオリジン（`web.content_origin`） | M2 | **M2 で決めるまでルートを作らない**（前提は下記） | — | — |
| `/-/pulls` `/-/pull/{iid}` `/-/merge_requests/{iid}`（別名）、`/-/work-items`、`/-/work-items/{provider}/…` | M3 | **その段階の ADR が Action を決めるまでルートを作らない** | — | — |
| `/-/pipelines` `/-/jobs` `/-/environments` `/-/deployments` | M4 | 同上 | — | — |
| `/-/issues`、`/-/wiki`、`{path}.wiki.git` | M6 | 同上（今は予約だけ） | — | — |
| `/docs/…` | M7 | 同上（文書ポータルの ADR で決める） | — | — |

- receive-pack の advertise と POST は、既存の `(RepoPush, Resource::Project)` の arm（ref なし、Developer 以上）でそれぞれ判定する。
  この arm の matrix の行は、すでにある。新しい Action は作らない。
- `HEAD` は、同じルートの `GET` の Action で明示的に登録する（`Verb::Head` を足す）。`is_git_operation()` は「git のプロトコルでも行う」
  という意味に変える。`RepoRead` は HTTP のルート（raw / archive）にもなるので、`authz-coverage` は両方を受け付ける。
- ルートのない新しい Action（`GroupTransfer` など、API が後の段階で入るもの）は、`app::PLANNED` に入れる。
- 作業項目のルート（M3）の前提は今決めておく。対象は呼び出し元が読めるプロジェクトだけにする。`provider` は enum の許可リストにする。外部への URL は、
  連携の設定に登録したベース URL と、検証した ID から組み立てる。文字列の連結では作らない（SSRF 対策）。

### 判定の順序（固定。dispatcher は 1 か所）

forge-web の dispatcher は、`Registry::scoped()` で作った表だけを引く。表にないものは、ハンドラを呼ばずに 404 にする（fail closed）。
パスは `req.uri().path()` の生の値を使う。axum の `Path` 抽出はデコードを済ませてしまうので使わない。

1. **文字列で分ける**: 生のパスを `/` で区切ってから、各セグメントを 1 回だけデコードする。そのうえで 3 つの形のどれかに分ける。`parse_web_path` が返すのは
   **語の enum（`RouteWord`）だけ** で、スコープ（グループかプロジェクトか）は決めない。規則に反するものは FG-WEB-0001（404）にする。
2. **語彙を引く**: 語が、グループの語彙とプロジェクトの語彙の和集合（RouteDef の表から作る）にあるかを確かめる。ない語、`/api/…`、dumb HTTP（`.git/objects/…`、`.git/HEAD`）は FG-WEB-0001 にし、殻は返さない。
3. **`service` を検査する（git だけ）**: `service` がちょうど 1 つあり、値が `git-upload-pack` か `git-receive-pack`（大文字小文字も完全一致）であることを確かめる。違えば 400 `SmartHttpServiceUnsupported` にする。起動する git のサブコマンドは、一致した RouteDef から決める。`service` を解析し直さない。
4. **認証する**: 解決の前なので、スコープはまだ決まっていない。そこで、認証のガードは **語（と verb）で選ぶ**。ある語が公開になるのは、その語を持つ
   RouteDef が **すべて** `WebShell` のときだけである。1 つでも公開でない RouteDef がある語（`raw`、`archive`、git のルート）は、認証を求める。
   公開の語は、ここで殻を返して終わる。殻の応答は、どのパスでも同じ内容にする（DB を見ない）。認証がないときは、パスの存在に関係なく、同じ 401
   （git には `WWW-Authenticate: Basic`）を返す。登録の仕組みのテストで、「公開と非公開の RouteDef が 1 つの語に混ざっていれば、その語は非公開として扱われる」こと、
   および「公開と判定された語の RouteDef は、すべて `WebShell` である」ことを確かめる。
5. **解決する**: パスを解決し、**解決した種類 + 語** で RouteDef を 1 つ選ぶ（`settings` と `members` は、グループとプロジェクトの両方にある）。
   その組の RouteDef がなければ（グループに `raw` など）、「ない」と同じに扱う。
6. **`authorize()`**: RouteDef の Action で判定する。ないもの、見えないもの、種類が違うもの、墓標の先が見えないもの、削除の墓標は、どれも同じ FG-REPO-0005 にする。本文、ステータス、ヘッダはそろえる。照会の回数もそろえて、時間の差を小さくする。違いはログの `reason`（`absent` / `not_visible` / `kind_mismatch` / `tombstone_not_visible` / `tombstone_deleted`）にだけ出す。
7. **ハンドラ**: 証明（下記の `Authorized<R>`）を受け取る。登録の仕組みは `proof.action() == def.action` を確かめてからハンドラを呼ぶ。ref とパスの境目は、ここで初めて決める（下記）。

### 認可の証明（`Authorized<R>`）

- 純粋な判定 `forge_auth::authorize(&Principal, Action, &Resource) -> Result<(), ForgeError>` は、名前も型もそのまま残す（判定の点は 1 つ。
  matrix のテストは、引き続きこの関数を直接呼ぶ）。レビューでは `decide()` への改名も提案されたが、CLAUDE.md §3-4、規則、評価のエージェントが
  `authorize()` の名前を参照しているので、改名しない。
- その上に、薄いラッパー `forge_auth::prove::<R>(&Principal, Action, R) -> Result<Authorized<R>, ForgeError>` を足す。中で `authorize()` を呼ぶだけで、
  判定の規則は持たない。`R` は型付きのリソースの文脈（`ProjectCtx`、`GroupCtx` など）で、`Resource` への変換を持つ。
- `Authorized<R>` は、Action と対象の ID（`ProjectId` など）を持つ。`Clone`、`Default`、公開のコンストラクタは持たない。作れるのは `prove()` だけである。
  ハンドラは、対象の ID を証明から取る。パスから解析し直すことはしない。
- 既存の `route()` も、同じ phase 8 でこの形にそろえる（forge-web README の MUST）。

### パスの規則とデコード

- **作成のとき**（`PathSegment::parse`）は、`^[a-z0-9](?:[a-z0-9._-]{0,62}[a-z0-9])?$` の 1〜64 文字だけを受け付ける。
  大文字、`..`、`.git` `.atom` `.wiki` で終わる名前は拒否する。Windows のデバイス名（`con` `prn` `aux` `nul` `com1`〜`com9` `lpt1`〜`lpt9`）は、
  そのものでも、`.` の前の部分（stem）がそれであっても拒否する。表示名 `name` は自由な文字列だが、bidi 制御や不可視の文字は取り除き、NFKC で
  紛らわしい表示名は警告する。DB は `full_path text COLLATE "C"` にし、セグメントにも同じ正規表現の CHECK を付ける。
- **要求のとき**（`LookupPath::fold`）は、名前空間のセグメントに `%` があれば拒否する（正しい名前は符号化する必要がない）。非 ASCII、空のセグメント
  （`//`、末尾の `/`。補正しない）、制御文字も拒否する。そのうえで `to_ascii_lowercase()` で小文字にする（`to_lowercase()` は使わない。
  U+212A のケルビン記号や `İ` が ASCII に化けるため）。小文字にするのは名前空間だけで、画面の語、ref、ファイルのパスはそのまま扱う
  （`/-/Blob` は 404）。Smart HTTP の形では、最後のセグメントの `.git` を 1 回だけ取り除く（`x.git.git` → `x.git` は名前として不正なので 404）。
- **`/-/` の後ろ**は、各セグメントを 1 回だけデコードし、正しい UTF-8 であることを求める（ファイル名に日本語を使えるようにするため）。
  `%2F` `%5C`、NUL や制御文字、`.` `..` のセグメントは拒否する。
- 拒否は 404 か 400 にする。**リダイレクトにはしない。** テストベクタ（`Git/tests/fixtures/url-vectors.json`）は、ブラウザの `new URL()` で
  正規化した形と、生のバイト列（`curl --path-as-is`）の両方を含める。`.git` が許されない場所の例もすべて含める。Rust と TS の両方の解析を、このベクタで確かめる。
  不正なパスはログに生のまま出さない。256 文字で切り、制御文字はエスケープする。
- **ref の大文字小文字**: URL の ref は完全一致で扱う（git の ref は大文字小文字を区別する）。ADR-0010 の修正にある「保護のパターンは大文字小文字を
  区別しない」は、防御を強める側の判定なので、これと矛盾しない。
- 同じ親の下では、ユーザー・グループ・プロジェクトの名前は重複できない。墓標（下記）とも重複できない。深さは 20 まで、全体の長さは 255 文字まで。
- **トップレベルの予約語**（`forge-core` の 1 つの定数）: `-` `api` `admin` `assets` `login` `logout` `signup` `settings` `dashboard` `explore` `new`
  `search` `notifications` `docs` `help` `healthz` `readyz` `metrics` `users` `groups` `projects` `oauth`。これに Web の成果物のルートにある
  ファイル名（`favicon.ico` `robots.txt` …）を加える。テストで、トップレベルの全ルートの先頭セグメントと、成果物のルートにある全ファイル名が
  リストに含まれることを確かめる。静的なファイルは `/assets/…` か `/-/static/…` にだけ置く。トップレベルの `.` は禁止しない（`john.doe` のようなユーザー名のため）。
- **LDAP で自動作成するユーザー名**が、予約語や既存の名前、墓標とぶつかったときは、ログインを失敗させる（`UsernameUnavailable`）。
  管理者が `UserRename`（下記）で、ログイン名とは別の名前空間のパスを割り当てて解決する。黙って既存の名前空間と結びつけることはしない。

### 名前の変更、移管、削除と墓標（tombstone）

- `namespaces`（`parent_id`、`kind`、`segment`、`full_path`）でパスを `(kind, id)` に解決する。保存先は `RepoId` から作る（ADR-0009）。
- **墓標**（`namespace_tombstones(old_path, parent_namespace_id, namespace_id, kind: moved | deleted, owner_at_write, created_at)`）は、次のときに書く。
  - **名前の変更と移管**: 動いた節と、**その子孫のすべて** について、古い `full_path` ごとに 1 行書く。行き先は、その節の ID である。
  - **削除**（猶予期間を過ぎて消すときも含む）: 消した節とその子孫の `full_path` ごとに 1 行書く。行き先は消した ID で、リダイレクトはしない（`kind = deleted`）。
- **解決は完全一致だけで行う。** (1) 今のパスへの完全一致を探す。(2) なければ、墓標への完全一致を探す。墓標の接頭辞による一致は使わない。
  墓標は、書いた時点の部分木の節ごとにあるので、墓標を書いた後に新しいパスの下に作ったリソースへ、古いパスから到達することはない。
  `moved` の墓標なら、ID から今のパスを組み立てる。組み立てたパスは、今のパスへの完全一致だけで確かめ、墓標をもう一度たどることはしない（連鎖しない）。
- **行き先がなくなった `moved` の墓標**: 行き先が後で削除された場合や、今のパスを組み立てられない場合は、`deleted` と同じに扱う。FG-REPO-0005
  （`reason=tombstone_deleted`）を返し、古いパスは引き続き使えない。解放は、下の「削除の墓標」の規則に従う。行き先の削除で書かれる `deleted` の墓標とは
  別の行のまま残る（どちらも解放されるまでパスを塞ぐ）。
- **作成と移動**: 墓標のパスや、墓標の下にリソースを作ろうとすると、`NamespacePathTaken`（409、`reason=tombstoned`）になる。そのパスへ移動したり、
  名前を変えたりする場合も同じである。削除の墓標も同じである。
  - **自分の古いパスにも戻れない。** A → B と名前を変えた後で B → A に戻すには、先に A の墓標を解放する必要がある。解放できるのは、墓標の先（そのリソース自身）の
    今の Owner である。つまり今の Owner は、自分の墓標を取り戻せる。自動では取り戻さない。「墓標は、解放されるまで必ずパスを塞ぐ」という規則を 1 つにしておくためである。
  - 409 の `reason` が `tombstoned` か `sibling` かで、そのパスが過去に使われたことが分かる。これは **受け入れる露出** とする。作成や移動には、もともと親の名前空間で
    作成の権限（Maintainer 以上）が要り、その人は親の下の名前の一覧を見られるからである。
  - これらの行は、ルート単位の結合テスト `namespace_tombstone_routes`（`Git/crates/forge-web/tests/namespace_tombstone_routes.rs`）に載せる。名前が空いているかどうかは、
    `authorize()`（誰が作れるか）ではなく、名前空間のサービスが同じトランザクションの中で判定する。このため、pure な matrix の外に置く。
- **解放**（`NamespaceTombstoneRelease`）の権限は次のとおり。解放は監査ログに残す。
  - **`moved` の墓標**（行き先がある）: 墓標の先のリソースの、**今の** Owner か、管理者。
  - **削除の墓標**（行き先がなくなった `moved` を含む）: 記録した Owner（`owner_at_write`）が解放できるのは、アカウントが有効で、**かつ** 墓標の親の名前空間で
    今も Owner であるときだけである。それ以外は管理者だけが解放できる。親がない（トップレベルのグループやユーザーの）墓標は、管理者だけが解放できる。
- **画面**: SPA が resolve API の `canonical_path` を受け取り、同じオリジンの中だけで `history.replaceState` する。`next` などの戻り先は相対パスだけを受け付け、解析してから使う。
- **git**:
  - `moved` の墓標への `GET info/refs?service=git-upload-pack` は、`authorize(RepoRead)` で許可されたときだけ `302`（`Cache-Control: no-store`）で
    新しいパスへ送る。git は最初の要求のリダイレクトに従い（`http.followRedirects=initial` が既定）、`warning: redirecting to …` を表示する。
  - `Location` は、設定の `web.base_url` と検証済みのセグメントから組み立てる（または相対パス）。`Host` や `X-Forwarded-*` は使わない。
    クエリは、検証済みの `service` 1 つだけを付け直し、ほかは捨てる。
  - **receive-pack はリダイレクトしない。** 墓標への `info/refs?service=git-receive-pack` は FG-REPO-0005（`reason=tombstone_push`）にする。リダイレクトすると、
    git が新しいパスへ POST して push が通ってしまうからである。古い URL への push は、利用者が remote を直すまで失敗させる。
  - 墓標のパスへの POST（upload-pack も receive-pack も）は処理しない（FG-REPO-0005、`reason=tombstone_post`）。
  - 削除の墓標（行き先がなくなった `moved` を含む）は、どの要求でも FG-REPO-0005（`reason=tombstone_deleted`）にする。
- 墓標を経由した要求は、`warn` のイベント `namespace.redirected`（`namespace_id`、`tombstone_id`、`route`）として記録する。runbook
  `namespace-redirects` に、件数の監視と、所有者に URL の更新を促す手順を書く。
- **移管は両側で判定する。** `Resource::Transfer` と `Resource::GroupTransfer` は、移動先を共通の型 `TransferTarget` で持つ。
  - `TransferTarget::Group { role, visible }`、`TransferTarget::PersonalNamespace { owner }`、`TransferTarget::TopLevel`。移動先が見えなければ FG-REPO-0005 にする。
  - プロジェクト: 移動元で Owner、移動先のグループで Maintainer 以上が要る。個人の名前空間へ移せるのは、その本人（か管理者）だけ。`TopLevel` は常に拒否する（プロジェクトは名前空間の下にしか置けない）。
  - グループ: 移動元で Owner、移動先で Owner が要る。移管すると、移動先の全員に継承のロールが付くからである。`TopLevel` へ移せるのは管理者だけ（`GroupCreateTopLevel` の迂回を防ぐ）。`PersonalNamespace` は常に拒否する。
  - 名前だけの変更は、移動先 = 今の親として扱う（`TransferTarget::Group { role: 自分の実効ロール, visible: true }`。トップレベルのグループなら `TopLevel`）。
  - `UserRename`（`Resource::UserRename { target_user }`）は管理者だけにする。LDAP の衝突のときに名前空間のパスを割り当てるのも、この操作で行う。墓標を量産されないよう、名前空間ごとに変更の回数を制限する（`RenameRateLimited`）。
  - 同じトランザクションで、深さ（部分木の高さ + 移動先の深さ ≤ 20）、配下の全パスの長さ（≤ 255）、循環（自分の子孫の下には移せない）、
    名前の衝突（墓標を含む）を検査する。実効ロールが変わる利用者の一覧を、監査ログに残す。
  - `ProjectUpdate` と `GroupUpdate` の要求の型には `path` を持たせない。パスはここに挙げた操作でだけ変わる。

### ref とパスの境目（認可の後で決める）

- 境目は `authorize(RepoRead, Project)` の後で、ハンドラか API（`?ref=&path=`）が決める。dispatcher は `/-/` の後ろを、検証済みのセグメントの
  並びとして渡すだけで、git を起動しない。`RefNotResolved` も、認可の後でしか返さない。
- 対象にできるのは `refs/heads/` と `refs/tags/` だけ（内部の ref は対象にしない）。優先の順序は次のとおりで、長さより先に種類で決める。
  (1) 40 / 64 桁の完全なコミット ID。(2) ブランチの最長一致。(3) タグの最長一致。git は `feature` と `feature/x` の共存を許さない
  （D/F 衝突）ので、同じ種類の中では一致は 1 つに決まる。
- 40 / 64 桁の 16 進数と同じ名前のブランチやタグ、`-` で始まる ref は、push のときに拒否する。archive の拡張子は 1 回だけ取り除く。
- git に渡すのは、解決し終えた完全な ref 名（`refs/heads/…`）か oid だけにする。ファイルのパスは `<oid>:<path>` の形か、`--` の後ろで渡す。
  引数はベクタだけで、`--end-of-options` を付ける（forge-git の 1 つのラッパー経由、タイムアウト付き）。
- `for-each-ref` の結果は、リポジトリごとに数秒だけ保持する（push で消す）。リポジトリごとの同時実行数にも上限を設ける。

### 応答の安全（raw、archive、プレビュー、キャッシュ、CSRF）

- **raw** は、正しい UTF-8 で NUL を含まないものを `text/plain; charset=utf-8` で返す。それ以外は `application/octet-stream` で返す。
  `Content-Disposition` は RFC 6266 に従い、安全にしたファイル名を `filename*=UTF-8''…` で付ける。
- **画像** は許可リスト（png / jpeg / gif / webp）だけを表示する。中身の先頭のバイト（マジックナンバー）で形式を確かめ、それに合う `Content-Type`
  （`image/png` / `image/jpeg` / `image/gif` / `image/webp`）を付ける。拡張子と中身が合わなければ `application/octet-stream` にする。
  画像にも、下の `nosniff` と CSP の sandbox を付ける。SVG・HTML・PDF は、アプリのオリジンでは表示しない。
- **archive** は `application/zip` / `application/gzip` と `Content-Disposition: attachment` で返す。
- raw、画像、archive のどれにも、`X-Content-Type-Options: nosniff`、`Content-Security-Policy: sandbox; default-src 'none'`、`Cross-Origin-Resource-Policy: same-origin` を付ける。
- **プレビュー用のオリジン（M2 で決める。それまでルートを作らない）** の前提は次のとおり。
  - `scoped()` で `RepoRead` として登録する。
  - アプリのオリジンが `authorize()` の後で、短い期限（60 秒以内）の署名付き URL を発行する。署名は `(project_id, oid, path, user_id)` に結びつける。
  - プレビュー用のオリジンは署名を検証し、`user_id` の利用者として `authorize(RepoRead)` をもう一度行う（期限内でも、所属を外されていれば拒否する）。
    `Authorization` と Cookie は受け付けない。応答には CSP の sandbox、`nosniff`、`Cache-Control: private, no-store` を付ける。
  - matrix の行: 許可 = Reporter 以上。拒否 = 期限切れ、他の利用者の署名、別のパスに使い回した署名。
- 認証付きの応答には `Cache-Control: private, no-store` と `Vary: Authorization, Cookie` を付ける。公開の殻は、セッションに依存するものを含めない（CSRF トークンは API で取る）。
  キャッシュのキーは正規の形にする。Smart HTTP は `Authorization` だけを受け付け、Cookie は使わない。状態を変える API は、CSRF トークンと `SameSite` の Cookie を求める。副作用のある GET は作らない。
- サーバーが返すリダイレクトは `302`（GET）か `307` / `308`（メソッドを保つ）にし、`301` は使わない。行き先は `web.base_url` か相対パスと検証済みのセグメントから組み立て、
  `//host`、`/\host`、CR / LF を含められないようにする。

### パスの解決（PathResolve）と、見え方

- `authorize(PathResolve, Resource::Unresolved)` の判定は次の順で行う。認証がなければ FG-AUTH-0001、無効なアカウントなら FG-AUTH-0004、それ以外は FG-REPO-0005（`reason=absent`）。
  判定は `authorize()` の中にあり、2 つ目の判定点はできない。`Resource::Resolved { kind, view }` は、「見えるか」（`can_see`。private の Guest でも解決できる）
  で判定する。Reporter 以上という読み取りの条件ではない。git のパスの解決は `PathResolve` ではなく、`RepoRead` / `RepoPush` で判定する。
  `Resource::Namespace { owner }`（`ProjectCreate` 用）は使い回さない。
- **固定リンク**: `/-/projects/{id}/…` は殻（`WebShell`）だけを返す。SPA が `GET /api/v1/projects/{id}`（`ProjectRead`）を呼ぶ。ID は連番なので、
  この API では、ないものと見えないものの応答を完全にそろえる（本文、ステータス、ヘッダ、時間）。
- resolve API は利用者ごとに流量を制限し、ログに残す。子孫のプロジェクトが見える人には、祖先のグループの名前を含む正規のパスを見せる
  （clone の URL に含まれるので、隠しても意味がない）。祖先のグループのページは、そのグループの見え方で判定する。
- **GitHub の形の URL の救済（M2）**: `/a/b/pull/3` は文字列の上では名前空間のトップなので、殻が返る（どのパスでも同じ応答）。SPA が、認証付きの
  resolve API（`prefix=true`）を呼び、`/-/` の付いた形へ置き換える。公開のルートは DB を見ない。`prefix=true` の規則は次のとおり。
  - **今のパスへの完全一致だけ** を接頭辞ごとに調べる。墓標は一切見ない（墓標の先へ案内するのは、完全なパスの解決だけである）。
  - 返すのは、呼び出し元に **見えるプロジェクト** の最長一致と、残りのセグメントだけ。見えるプロジェクトがなければ、途中に何があっても FG-REPO-0005（`reason=absent`）
    にする。どの接頭辞まで一致したか、途中のグループが存在するかは返さない。
  - 見えるプロジェクトへの経路の途中に、呼び出し元に見えないグループがあっても、そのグループを単独の結果として返すことはない（祖先の名前は、そのプロジェクトの
    正規のパスの一部としてだけ出る。上の規則と同じ）。

### リポジトリの種類（ADR-0004）

`spec` / `system` は URL に入れない。種類は属性であり、種類を変えてもリンクは変わらない。画面ではバッジで示し、一覧はクエリ（`?kind=spec`）で絞る。

### ルートの登録（ADR-0010）に足すもの

- `Registry::scoped(scope, verb, word, query_key, action, summary, handler)` を足す。重複の判定のキーは `(scope, verb, word, query_key)` にする。
  `Router::route` を呼ぶのは、引き続き登録の中の 1 か所だけである（`scoped()` の表は、1 つの dispatcher として 1 回だけ載せる）。
- 認証のガードは語ごとに決める（判定の順序の 4）。登録の仕組みが、語ごとの「公開か」を表から計算して持つ。
- `routes.generated.json` に `scope`、`word`、`query_key` の欄を足す。今のゲート `authz-coverage`（`scripts/validate-authz-coverage.js`）は
  `method path` で重複を判定するので、クエリだけが違う `info/refs` の 2 行がぶつかる。キーを `(scope, method, path, query_key)` に変え、
  `RepoRead` をルートと git の操作の両方として受け付けるようにする。`gen-authz --check` も合わせる。このゲートの変更は、エージェントモデルの側で先に入れる。
- `authz-coverage` は、ルート単位の結合テスト `namespace_tombstone_routes` が存在し、墓標の 3 つの行（削除の後の作成 → 409、墓標への receive-pack → 404、
  墓標への upload-pack → 302）と、行き先がなくなった `moved` の墓標の行を持つことも確かめる（テストの一覧をゲートの入力にする）。
- 複数のセグメントにまたがるスコープ付きのルートは、OpenAPI には **出さない**（OpenAPI は `/api/v1` だけを表す）。

## Alternatives Considered

### A. GitHub と同じく owner を 1 階層にする
- **Cons**: 権限の継承の単位（祖先のグループ）が URL から消え、移管のたびに名前が変わる
- **Why not**: GitLab 型の権限モデル（§11-3）と食い違う。拒否（FG-REPO-0005）の原因を URL から推測できない

### B. 区切りを使わず、すべての階層の予約語で判別する
- **Cons**: 画面の語を足すたびに既存の名前とぶつかり、名前の変更を強いる（GitLab が `/-/` に移った理由）
- **Why not**: 衝突したプロジェクトに到達できなくなっても、ログ（FG-WEB-0001 / FG-REPO-0005）からは原因が分からない

### C. 区切りを使わず、DB の最長一致で判別する
- **Cons**: グループの下の画面がサブグループとぶつかる。解析が DB の状態に依存し、公開のルートで使うと存在を確かめる手段になる
- **Why not**: 障害を URL だけで再現できない。認証付きの resolve API の救済としてだけ使う

### D. owner を 1 セグメントに詰める（`/acme~platform/forge-web`）
- **Why not**: GitHub にも GitLab にも似ない

### Smart HTTP で `.git` を省略できるようにする
- **Why not**: `/x/info/refs` が「`x` の git の要求」なのか「グループ `x/info` のプロジェクト `refs`」なのかが、DB の状態で決まってしまう。
  画面が示す clone の URL には必ず `.git` を付けるので、GitHub との違いは小さい

### 墓標を接頭辞 1 行だけにし、子孫は接頭辞の一致で解決する
- **Why not**: 墓標を書いた後に新しいパスの下に作ったリソースへ、古いパスから到達できてしまう。節ごとに書けば、完全一致だけで済む（行の数は部分木の大きさまで）

### 自分の古いパスへは、解放なしで戻れるようにする
- **Why not**: 「墓標は解放されるまで必ず塞ぐ」という規則に例外ができる。今の Owner は自分で解放できるので、手順が 1 つ増えるだけで済む

## Consequences

### Positive
- 形の判定が文字列だけで決まる。障害は、URL とテストベクタで再現できる
- 画面の語を足しても（`/-/wiki` など）既存の名前とぶつからない。予約語はトップレベルだけで済む
- 古い URL が、黙って別のリポジトリを指すことはない（移動でも削除でも墓標が残る）。名前の変更は DB の更新だけで済み、保存先は動かない
- 全ルートが 1 つの Action に対応する。ハンドラは証明なしでは呼べず、対象の ID は証明から取る

### Negative
- URL に `/-/` が入る。`.git` を省いた clone はできない
- 古い URL への push は、remote を直すまで失敗する（fetch はリダイレクトで続けられる）
- 解析が Rust と TS の 2 か所にある（共通のテストベクタで食い違いを検出する）
- 墓標によって、名前を再利用するには解放が要る。元の名前に戻す場合も同じである。大きなグループの移管では、墓標が部分木の節の数だけ書かれる
- 計画 02 §5.2 の例 `…/-/merge_requests/87` は、画面では `…/-/pull/87` になる（`merge_requests` の別名は M3 で `pull` と一緒に決める）。API のリソース名は計画 02 のまま

### Risks
- 墓標の量産 → 変更の回数の制限、`namespace-redirects` の監視
- 削除の墓標を解放できる人がいなくなる（元の Owner が異動した）→ 管理者が解放する。runbook `namespace-redirects` に手順を書く
- 予約語の追加が既存の名前とぶつかる → `forge doctor` の検査（`ReservedNameOccupied`）。予約語を足す MR では、本番の名前と墓標の一覧で照合する
- ref の解決の負荷 → 短い保持と、リポジトリごとの同時実行数の上限
- プレビュー用のオリジンを置かない環境では、プレビューが制限される → 運用の手順書に `web.content_origin` の設定を書く（M2）

## エラーコード（実装時に登録する。今は登録しない）

| 候補 | 名前 | HTTP | severity | 場面 |
|---|---|---|---|---|
| FG-REPO-next | `NamespacePathInvalid` | 400 | info | 作成・変更で規則に反する（`reason`: `charset` / `length` / `suffix` / `device_name` / `depth`） |
| FG-REPO-next | `NamespacePathReserved` | 400 | info | トップレベルの予約語 |
| FG-REPO-next | `NamespacePathTaken` | 409 | info | 同名がある（`reason`: `sibling` / `tombstoned`。この区別は受け入れる露出） |
| FG-REPO-next | `TransferInvalid` | 409 | info | 移管で、深さ・長さ・循環の制約に反する |
| FG-REPO-next | `RenameRateLimited` | 429 | info | 名前の変更の回数の上限 |
| FG-REPO-next | `RefNotResolved` | 404 | info | 認可の後で、ref にもコミット ID にも一致しない |
| FG-REPO-next | `ReservedNameOccupied` | — | alert | `forge doctor` が、予約語と同名の名前空間を検出した（runbook `namespace-reserved-collision`） |
| FG-WEB-next | `SmartHttpServiceUnsupported` | 400 | info | `service` がない、重複している、または未知の値 |
| FG-GIT-next | `AmbiguousRefName` | — | info | 16 進数の ID と同じ形の名前、`-` で始まる ref の push |
| FG-AUTH-next | `UsernameUnavailable` | 409 | warn | LDAP のユーザー名が予約語、既存の名前、墓標とぶつかる |

既存のコードの使い方は次のとおり。文字列の規則に反する（`.git` の場所を含む）、または未知の語 → FG-WEB-0001（`reason=path_rejected` / `unknown_route`）。
ない・見えない・種類が違う・墓標（push、POST、削除、行き先がなくなった移動）→ FG-REPO-0005。登録の誤り → FG-WEB-0004。削除の猶予中 → FG-REPO-0006。
新しい runbook は `namespace-redirects` と `namespace-reserved-collision` の 2 つ。

## インターフェースの概略（実装は含めない）

```rust
// forge-core
pub struct PathSegment(String);      // creation rules; rejects uppercase
pub struct NamespacePath(Vec<PathSegment>);
pub struct LookupPath(NamespacePath); // request side: ASCII-only, to_ascii_lowercase
pub struct TailSegments(Vec<String>); // decoded once, validated, not folded
pub enum NamespaceKind { User, Group, Project }
pub const RESERVED_TOP_LEVEL: &[&str];
pub enum RouteWord { Tree, Blob, Raw, Blame, Commits, Commit, Compare, Branches, Tags, Archive, Settings, Members }
pub enum InstanceRoute { ProjectPermalink }
pub enum GitRoute { InfoRefsUploadPack, InfoRefsReceivePack, UploadPack, ReceivePack }
pub enum ParsedPath {
    NamespaceHome(LookupPath),
    Scoped { path: LookupPath, word: RouteWord, rest: TailSegments },   // scope decided after resolution
    Git { path: LookupPath, route: GitRoute },
    Instance { route: InstanceRoute, rest: TailSegments },
}
impl PathSegment { pub fn parse(s: &str) -> Result<Self, ForgeError>; }
impl LookupPath { pub fn fold(raw_segments: &[&str]) -> Result<Self, ForgeError>; }
pub fn parse_web_path(raw_path: &str, query: Option<&str>) -> Result<ParsedPath, ForgeError>;
pub trait NamespaceResolver: Send + Sync {
    async fn resolve(&self, path: &LookupPath) -> Result<Resolution, ForgeError>;          // exact matches only
    async fn resolve_prefix(&self, path: &LookupPath) -> Result<Vec<Resolved>, ForgeError>; // live paths only, never tombstones
}
pub enum Resolution { Live(Resolved), Moved { resolved: Resolved, tombstone: TombstoneId },
                      Deleted { tombstone: TombstoneId },   // also a moved tombstone whose target is gone
                      Absent }

// forge-auth
pub fn authorize(principal: &Principal, action: Action, resource: &Resource) -> Result<(), ForgeError>; // unchanged, pure
pub fn prove<R: ResourceCtx>(principal: &Principal, action: Action, ctx: R) -> Result<Authorized<R>, ForgeError>;
pub trait ResourceCtx { type Id; fn resource(&self) -> Resource; fn id(&self) -> Self::Id; }
pub struct Authorized<R: ResourceCtx> { /* private: action, id, ctx; no Clone / Default / public ctor */ }
impl<R: ResourceCtx> Authorized<R> { pub fn action(&self) -> Action; pub fn id(&self) -> &R::Id; pub fn ctx(&self) -> &R; }
pub enum TransferTarget { Group { role: Option<Role>, visible: bool }, PersonalNamespace { owner: UserId }, TopLevel }
pub enum TombstoneRelease {
    Moved { target_owner_role: Option<Role> },                                  // current role on the live target
    Deleted { recorded_owner: Option<UserId>, caller_parent_role: Option<Role>, has_parent: bool },
}
// Action (new): PathResolve, WebShell, GroupTransfer, UserRename, NamespaceTombstoneRelease
// Resource (new / changed): Unresolved, Resolved { kind, view },
//   Transfer { source: ProjectView, target: TransferTarget },
//   GroupTransfer { source_role: Option<Role>, target: TransferTarget },
//   UserRename { target_user: UserId }, Tombstone(TombstoneRelease)

// forge-web
pub enum Verb { Get, Head, Post, Put, Patch, Delete }
pub enum RouteScope { Namespace, Group, Project, ProjectGit, Instance }
pub enum ScopedRoute { Home, Word(RouteWord), Git(GitRoute), Instance(InstanceRoute) }
pub struct RouteDef { pub scope: RouteScope, pub verb: Verb, pub path: &'static str,
                      pub route: Option<ScopedRoute>, pub query_key: Option<(&'static str, &'static str)>,
                      pub action: Action, pub summary: &'static str }
impl<S> Registry<S> {
    pub fn scoped(self, scope: RouteScope, verb: Verb, route: ScopedRoute,
                  query_key: Option<(&'static str, &'static str)>, action: Action,
                  summary: &'static str, h: H) -> Self;
    pub fn is_public_word(&self, verb: Verb, route: ScopedRoute) -> bool; // true only if every RouteDef with it is WebShell
}
```

## 新しい Action と変わる Resource の最小の matrix の行（実装時に `matrix_rows.rs` に足す）

- `PathResolve`: 許可 = Developer / private、NonMember / internal、Guest / private。拒否 = Anonymous（FG-AUTH-0001）、Disabled（FG-AUTH-0004）、
  NonMember / private（FG-REPO-0005）、ないパス（FG-REPO-0005）、墓標の先が見えない（FG-REPO-0005）、`read_repository` だけのトークン（FG-AUTH-0002）、
  削除の猶予中（FG-REPO-0006）
- `PathResolve`（`prefix=true`）: 許可 = 見えるプロジェクトへの接頭辞（Developer / private、NonMember / internal）。拒否（どれも FG-REPO-0005 `absent`）=
  接頭辞が見えないグループにしか一致しない、接頭辞が private のプロジェクト（NonMember）にしか一致しない、接頭辞が墓標にしか一致しない
- `WebShell`: Anonymous でも許可（`is_public()` が真、`required_scope()` は `None`）
- `RepoPush` + `Project`（advertise と POST）: 既存の行（Developer の許可、Reporter の拒否など）をそのまま使う
- `ProjectTransfer`（Resource を変える）: 許可 = 移動元 Owner + 移動先グループ Maintainer、本人の個人の名前空間への移動。拒否 = 移動元 Maintainer、
  移動先 Developer、移動先が見えない（FG-REPO-0005）、他人の個人の名前空間（Forbidden）、`TopLevel`（管理者も拒否）
- `GroupTransfer`: 許可 = 両側で Owner、管理者によるトップレベルへの移動。拒否 = 移動元で Maintainer、移動先で Owner 未満、移動先が見えない（FG-REPO-0005）、
  Owner によるトップレベルへの移動、`PersonalNamespace`
- `UserRename`: 許可 = 管理者（セッション、または `admin` スコープのトークン）。拒否 = 本人、他のユーザー、`admin` スコープのない管理者のトークン
- `NamespaceTombstoneRelease`:
  - 許可 = `moved` の墓標の先の今の Owner（自分の古いパスを取り戻す場合を含む）。削除の墓標の記録した Owner で、有効かつ親の名前空間で今も Owner の人。管理者
  - 拒否 = Maintainer、NonMember。**元の Owner で、もうメンバーでないか Owner でない人**。無効にされたアカウントの記録した Owner（FG-AUTH-0004）。
    親のない削除の墓標に対する、管理者でない人
- ルート単位の結合テスト `namespace_tombstone_routes`（pure な matrix の外。`authz-coverage` がこのテストと行を確かめる）:
  削除した後に同じ名前で作る → 409、墓標への receive-pack → 404、墓標への upload-pack → 302、行き先が削除された `moved` の墓標 → 404（`tombstone_deleted`）で、
  そのパスへの作成も 409、A → B → A への名前の変更 → 409（今の Owner が解放した後は成功）

## 実装時の手順

1. **スキーマ B（M1、forge-db）**: `namespaces`（`full_path COLLATE "C"` に一意の索引、`UNIQUE NULLS NOT DISTINCT (parent_id, segment)`、セグメントの CHECK）と
   `namespace_tombstones`（`old_path COLLATE "C"` に一意の索引、`parent_namespace_id`、`owner_at_write`）を、新しいマイグレーションで追加する（ADR-0014）。
   移管と削除は、配下の `full_path`、節ごとの墓標、制約の検査、監査を 1 つのトランザクションで行う。
2. **forge-core**: パスの型、`parse_web_path`、予約語、テストベクタ（ブラウザの形、生のバイト列、`.git` が許されない場所）を作る。proptest には拒否のベクタも含める。
3. **M1 phase 8（forge-auth + forge-web）**: 先に、エージェントモデルの側で `authz-coverage`（キー、`namespace_tombstone_routes` の確認）と `forge-authz.md` の
   規則 1 と 5 を更新する。そのうえで、新しい Action と Resource、matrix の行（ルートのないものは `app::PLANNED`）、`prove()` と `Authorized<R>` を作り、
   既存の `route()` もそろえる。`Verb::Head`、`scoped()`、語ごとのガード、dispatcher（上の 7 段階、`req.uri().path()` の生の値）、manifest の新しい欄、
   `gen-authz --check` を作る。テストは次の 4 つ:
   到達できる `(scope, verb, word)` のすべてに RouteDef がある。種類の違うパスは 404 になる。`/api/…` と未知の `/-/…` には殻が返らない。
   公開と非公開の RouteDef が混ざった語は、認証を求める。
4. **M1 phase 9（forge-web + forge-git）**: `ProjectGit` の 4 つのルート、`service` の検査、401 の統一、upload-pack だけの `302`（`Host` の偽装と `service` の
   保持のテスト付き）、receive-pack と POST の墓標の拒否、dumb HTTP の 404、`namespace_tombstone_routes` を作る。実際の git クライアントで E2E テストを作る。
   名前の変更の後の clone ではリダイレクトの警告が出ること、古いパスへの push は拒否されること、削除した後の古いパスは 404 になることを確かめる。
5. **M2（web）**: SPA の解析（共通のベクタ）、`replaceState`、救済、raw / 画像 / archive のヘッダ、プレビュー用のオリジンの ADR か修正、`forge doctor` の予約語の検査を作る。
   エラーコードは、使い始める段階で登録する。
6. **M3 / M4 / M6 / M7**: 表の「ルートを作らない」系統は、それぞれの ADR で Action を決めてから足す。

## レビューで変えた点（security-reviewer + authz-matrix-auditor）

### 1 回目（BLOCK）

- **1（CRITICAL）**: dispatcher は `scoped()` の表だけを引き、表になければ 404 にする（fail closed）。`String` の tail をやめ、網羅的な enum にした。判定の順序を固定し、ガードを RouteDef ごとにかける。証明を手順 3 に入れ、`route()` もそろえる。HEAD を明示した。テストを 3 つ足した。殻は `/api` と未知の `/-/` には返さない。
- **2（CRITICAL）**: 「系統 → Action → Resource → スコープ」の表を足した。raw / archive はサーバーが返す `RepoRead` とし、`is_git_operation` の意味を変えた。作業項目の制約を書いた。M3 / M4 / M6 / M7 の系統は「Action が決まるまでルートを作らない」とした。新しい Action の行を列挙した。
- **3（CRITICAL）**: ref とパスの境目は `authorize()` の後で決める。dispatcher は git を起動しない。
- **4（CRITICAL）**: 「新しいリソースが優先される」をやめ、永続の墓標にした。解放は Owner か管理者が行い、監査に残す。`info/refs` のリダイレクトと、POST の拒否に決めた。`warn` のイベントと runbook を足した。
- **5（HIGH）**: advertise と POST をそれぞれ判定する。`service` の検査を 400 にして解決の前に置き、サブコマンドは RouteDef から決める。401 を統一し、dumb HTTP は 404 にした。
- **6（HIGH）**: Smart HTTP では `.git` を必須にし、「`-` ですべてを区切る」という記述を 3 つの形の規則に直した。
- **7（HIGH）**: デコード、拒否、`to_ascii_lowercase`、作成と要求の型の分離、`COLLATE "C"` と CHECK、`.git` を 1 回だけ取り除くこと、ref の大文字小文字を書いた。
- **8（HIGH）**: raw / archive の Content-Type、`nosniff`、CSP の sandbox、RFC 6266、別のオリジンを決めた。
- **9（HIGH）**: 移管を両側で判定する規則、管理者だけの操作、同じトランザクションでの検査、監査、`UserRename` と回数の制限を足した。
- **10（HIGH）**: `Resource::Unresolved` / `Resolved` を足し、判定を `authorize()` に寄せた。`can_see` で判定する。git は `PathResolve` を使わない。
- **11（HIGH）**: 救済は SPA が認証付きの resolve API で行う。公開の殻は、どのパスにも同じ応答を返す。
- **12（HIGH）**: 判定の順序を 7 段階に固定した。種類の違いは「ない」と同じ 404 にした。
- **MEDIUM / LOW**: リダイレクトのステータス、存在の漏れ、ref の規則、キャッシュと CSRF、manifest と OpenAPI、予約語、LDAP、Windows のデバイス名、ログの書き方を反映した。

### 2 回目（BLOCK）

- **R1（HIGH）**: 削除（猶予期間を過ぎて消すときも含む）でも、消した節と子孫ごとに `deleted` の墓標を書く。リダイレクトはせず FG-REPO-0005。作成は 409。解放は削除の時点の Owner か管理者。「削除した後に同じ名前で作る → 409」の行を、ルート単位の結合テストに足した（名前が空いているかは pure な `authorize()` の入力ではないため、matrix の外に置いた）。
- **R2（HIGH）**: 墓標を、移動した部分木の節ごとに書くようにした。解決は完全一致だけで、接頭辞による一致をやめた。組み立てたパスは、今のパスへの完全一致だけで確かめる。比較した案に「接頭辞 1 行」を足した。
- **R3（HIGH）**: 純粋な判定は `authorize()` のまま残し、薄いラッパー `prove()` → `Authorized<R>` を足した。証明は Action と ID を持ち、Clone / Default / 公開のコンストラクタを持たない。登録の仕組みは `proof.action() == def.action` を確かめ、ハンドラは ID を証明から取る。**名前**: レビューの提案（`decide()` + `authorize()` のラッパー）とは逆に、純粋な関数の名前 `authorize()` を残した。CLAUDE.md §3-4、規則、評価のエージェントへの影響を避けるためである。`forge-authz.md` の規則 1 は、ラッパーを足す形で更新する。
- **R4（HIGH）**: 固定リンクは `WebShell` だけにした。応答をそろえる要件は `GET /api/v1/projects/{id}`（`ProjectRead`）に移した。
- **R5（HIGH）**: `TransferTarget { Group { role, visible }, PersonalNamespace { owner }, TopLevel }` を足し、`Transfer` と `GroupTransfer` で共有する。拒否の行（他人の個人の名前空間、移動先が見えない）を書けるようにした。
- **R6**: upload-pack だけをリダイレクトする。墓標への receive-pack の advertise は FG-REPO-0005（`tombstone_push`）。
- **R7**: `Location` は `web.base_url` か相対パスから作る。付け直すクエリは `service` だけ。`Host` の偽装と `service` の保持のテストを足した。
- **R8**: プレビュー用のオリジンは、M2 で決めるまでルートを作らない。前提（`scoped()` + `RepoRead`、60 秒以内の署名付き URL、もう一度の `authorize()`、Cookie を受け付けない、ヘッダ、matrix の行）を書いた。
- **R9**: archive の Content-Type と `attachment`、raw と archive の `Cross-Origin-Resource-Policy: same-origin` を足した。
- **R10**: `RepoPushAdvertise` をやめた。既存の `(RepoPush, Resource::Project)` を使う。「RepoPush は ref 1 つだけ」という誤った記述は消した。
- **R11**: `parse_web_path` は語の enum（`RouteWord`）だけを返し、スコープは解決した種類 + 語で RouteDef の表から選ぶ。`members` をプロジェクトの語彙にも入れ、表とそろえた。
- **R12**: LDAP の衝突のときの割り当ては `UserRename` に含めた（管理者だけ。行は上のとおり）。
- **R13**: 手順 3 で、`authz-coverage` の重複のキーを `(scope, method, path, query_key)` に変え、`gen-authz --check` も合わせる。エージェントモデルの側の変更を先に入れる。
- **R14**: 最後以外のセグメントの `.git` は FG-WEB-0001 にし、ベクタに足した。
- **R15**: git には、解決し終えた ref 名か oid、`<oid>:<path>` か `--` の後ろのパスだけを渡す。`--end-of-options` を付ける。
- **R16**: ルートのない Action は `app::PLANNED` に入れる。`Verb::Head` を足す。dispatcher は `req.uri().path()` の生の値を使う。
- **R17**: 墓標の解放は「墓標の先のリソースの今の Owner か管理者」とした（削除の墓標は、削除の時点の Owner か管理者）。

### 3 回目（CRITICAL / HIGH なし。security PASS、authz WARN → 文面の修正。採用）

- **3-1**: 削除の墓標を記録した Owner が解放できるのは、アカウントが有効で、かつ親の名前空間で今も Owner のときだけにした。それ以外は管理者だけ。親のない墓標は管理者だけ。拒否の行「元の Owner で、もうメンバーでないか Owner でない人」を足した。`Resource::Tombstone(TombstoneRelease)` の形を決めた。
- **3-2**: 行き先が削除された（または今のパスがない）`moved` の墓標は `deleted` と同じに扱う（FG-REPO-0005 `tombstone_deleted`、パスは塞いだまま、解放は 3-1 の規則）。ルート単位のテストの行を足した。
- **3-3**: 認証のガードは語で選ぶ。語が公開になるのは、その語の RouteDef がすべて `WebShell` のときだけ。登録の仕組みのテストと `is_public_word()` を足した。
- **3-4**: `prefix=true` は、今のパスへの完全一致だけを調べ、見えるプロジェクトだけを返し、墓標を見ず、見えない途中のグループを結果として出さない。matrix の行を足した。
- **3-5**: `.git` で終わるセグメントは、Smart HTTP の形の、名前空間の最後のセグメントだけに許す。それ以外はどこでも `path_rejected`。
- **3-6**: 自分の古いパスも塞いだままにする（A → B → A には、今の Owner による解放が先に要る）と決め、比較した案に足した。
- **3-7**: 許可リストの画像は、中身で確かめた形式に合う `Content-Type` と、`nosniff`、CSP の sandbox で返す。
- **3-8**: ルート単位の結合テストの名前を `namespace_tombstone_routes`（`Git/crates/forge-web/tests/namespace_tombstone_routes.rs`）とし、`authz-coverage` がこのテストと行を確かめるとした。
- **3-9（L1）**: 409 の `reason` が `tombstoned` か `sibling` かの違いは、受け入れる露出と明記した（作成する人は、親の名前空間で作成の権限をもともと持っている）。

## Amendment（2026-10-28、手順 D1: ユーザーと名前空間の表）

- **`namespaces` に `kind` と `owner_id` を足す**（migration `20261028100100_users_and_namespaces.sql`）: `kind` は `user` / `group` / `project`、
  `owner_id` はその名前空間が指すユーザー・グループ・プロジェクトの ID である。監査の行のリソースは `(kind, owner_id)` なので、持ち主の 1 つの監査の行が、
  その名前空間の変更（`segment`、`full_path`、`depth`、`parent_id` の変更を含む）を覆う。`full_path` と `depth` は部分木の書き換えで変わるため、
  `tables.toml` では txn-audited（同じ種類の監査の行が同じトランザクションにあること）にする。
  - `owner_id` は種類によって指す表が変わる（多態）ため、**外部キーを持たない**。持ち主との整合は、同じ監査の行が両方を覆うこと（作成は 1 トランザクション）と、
    D5 の削除・パージのコマンドで保つ。
  - **D5 への注意**: txn-audited の判定は「同じ **種類** の監査の行」なので、グループの名前を変えると、子孫のプロジェクト（`kind = project`）の `full_path` の書き換えは、
    グループの監査の行では覆われない。D5 では、部分木に含まれる種類ごとに監査の行を書くか、トリガーの判定を「部分木の根の監査の行」に広げるかを決める（未決）。
- **トップレベルの予約語に `edge`・`fleet`・`devices` と、Web のルートのファイル `favicon.ico`・`robots.txt` を足す**（計画 01 §3.10、エッジ端末の経路や静的ファイルと衝突しないため）。
  上の一覧は、これらを含めて `forge-core` の `RESERVED_TOP_LEVEL` の 1 つの定数が正である。
  既存の名前がこれらと衝突する場合の扱いは、上の「予約語の追加」と同じく `ReservedNameOccupied` の検査で見つける（検査そのものは未実装）。
- **墓標の表**（`namespace_tombstones`）は、上の「名前の変更、移管、削除と墓標」の列（`old_path`、`parent_namespace_id`、`namespace_id`、
  `kind`（`moved` / `deleted`）、`owner_at_write`、`created_at`）に、監査の行のリソースを示す `cause_type`（`user` / `group` / `project`）と `cause_id` を足したものである。
  - `old_path` は一意で、アプリが挿入と削除だけを行う（更新はしない。`forge_app` に UPDATE の権限はない）。
  - `namespace_id` と `parent_namespace_id` は `forge.namespaces` への外部キーで、`ON DELETE SET NULL` にする。名前空間をパージすると NULL になり、
    行き先のない `moved` の墓標は `deleted` と同じに扱う（上の規則どおり）。この 2 列は、外部キーの動作でしか変わらないため、監査の対象外の列（`unaudited_columns`）にする。
  - `owner_at_write` は `forge.users` への外部キーである（ユーザーは削除せず、無効にするだけなので、参照は残る）。
- **パスのロック**: パスに名前空間を作るコマンドと、墓標を書く・解放するコマンド（D5 の名前の変更、移動、削除、解放）は、最初に
  `pg_advisory_xact_lock(hashtextextended('namespace:' || full_path, 0))`（`forge-db` の `namespaces::lock_path`）を取る。
  これで、作成が、同時に書かれた墓標を見落とすことはない（2 接続のテスト `a_tombstone_written_concurrently_blocks_the_create`）。
  作成は、念のため INSERT の後にも墓標をもう一度確かめる。
- **users の表**: `username` には `namespaces.segment` と同じ CHECK を付ける。トップレベルの名前空間では `full_path = segment` を CHECK で保証する。
  `external_id` は `(auth_source, external_id)` で一意にする。入力の誤りは FG-AUTH-0006 `UserRequestInvalid`、社員番号と外部 ID の重複は
  FG-AUTH-0007 `UserIdentifierTaken` にする（`DbQueryFailed` にしない）。
- **メールアドレスの列挙**: FG-AUTH-0005（メールアドレスの重複）は、そのアドレスが登録済みであることを相手に知らせる。このため、ユーザーを作る操作（と、
  メールアドレスでの検索）は **管理者だけ** に許す（D3 以降の route で `authorize()` の行にする）。アドレスは比較の時だけ小文字にし、保存は入力のままにする。
- 新しい表の上限は、深さ 20（`MAX_DEPTH`）、全体の長さ 255（`MAX_FULL_PATH_LEN`）、セグメントの長さ 64 である。
  DB の CHECK は `forge-core` の `PathSegment::parse` と同じ規則を持つ。

## Amendment（2026-10-29、手順 D2: グループ）

- **グループの名前空間は、トップレベルか、グループの下にだけ置ける**。ユーザーの名前空間の下にグループは作れない（FG-REPO-0011 `GroupRequestInvalid`、`reason=parent_not_group`）。
  プロジェクト（D4）の置き場所は D4 で決める。
- グループの作成は、ユーザーの作成と同じく、パスのロック（`namespaces::lock_path`）を取り、墓標を INSERT の前と後に確かめる。親の名前空間は `FOR SHARE` で読み、作成の確定まで動かないようにする。
- 入れ子の上限（深さ 20、全体の長さ 255）を超える作成は、FG-REPO-0007 の `reason=too_deep` / `path_too_long` にする。
- グループの名前空間の `owner_id` は `groups.id` で、`group` の監査の行 1 つが、グループ、名前空間、最初の Owner をまとめて覆う。

## Amendment（2026-10-31、手順 D4 MR1: `projects` の表）

- **`projects` に `namespace_id` の列を置かない**。D4 の計画は `projects.namespace_id UNIQUE` を挙げていたが、ユーザー・グループと同じく、
  プロジェクトのパスは `namespaces` の行（`kind = 'project'`、`owner_id = projects.id`）が持つ。理由:
  - 1 対 1 は、すでに一意の索引 `namespaces_owner (kind, owner_id)` が守る（D1 の Amendment）。
  - `namespace_id NOT NULL` の外部キーを足すと、プロジェクトと名前空間がお互いの ID を先に必要とする。1 つのトランザクションで作るには、
    ID の順序（sequence）を `forge_app` に直接使わせる権限が要り、その権限は doctor の ACL の比較の外にある。
  - 1 つの `project` の監査の行が、プロジェクトと名前空間をまとめて覆う（グループと同じ形）。
- `projects.deleted_at` と `purge_after` は、両方あるか両方ないか（CHECK `projects_deletion_pair`。`purge_after >= deleted_at`）。書き手は D5。
  プロジェクトの DELETE の権限も D5 まで与えない。
- `project_members` には、`group_members` と同じく、bot を入れないトリガー（`project_members_no_bot`）を DB の裏付けとして付ける。
- **プロジェクト・名前空間・リポジトリの 1 対 1 対 1 は、DB だけでは守られない**（`namespaces.owner_id` は多態で外部キーがなく、
  プロジェクトがリポジトリや名前空間を持つことを強制する制約もない）。このため:
  - D4 MR2 の作成（`projects::create`）は、プロジェクト、名前空間（`kind = 'project'`）、リポジトリ（`provisioning`）を **1 つのトランザクション** で書く。
  - MR2 は、作成の後に 3 つがそろっていること（どれかが欠けた作成は確定しないこと）を確かめるテストを持つ。
  - 欠けた組（名前空間のないプロジェクト、プロジェクトのない `kind = 'project'` の名前空間、リポジトリのないプロジェクト）を doctor で検査するかは、
    削除とパージを入れる D5 で決める。

## Amendment（2026-10-31、手順 D4 MR2: プロジェクトの置き場所と作成）

- **プロジェクトは、個人の名前空間（`kind = 'user'`）かグループ（`kind = 'group'`）の下にだけ置ける**（計画 D4 の回答 1）。
  プロジェクトの下にプロジェクトは作れない（FG-REPO-0014 `ProjectRequestInvalid`、`reason=parent_not_owner_namespace`）。
  トップレベルのプロジェクトはない（`NewProject.parent` は必須）ので、予約語の検査は親の下のセグメントとして `PathSegment::parse` だけを使う。
  グループの下ではサブグループとプロジェクトが同じ兄弟の空間を使う（`namespaces_sibling_key`、FG-REPO-0009 `sibling`）。
- `projects::create` は、グループと同じく、親の名前空間を `FOR SHARE` で読み、パスのロック（`namespaces::lock_path`）を取り、墓標を INSERT の前と後に確かめる。
  深さと長さの上限（20、255）を超える作成は FG-REPO-0007 `too_deep` / `path_too_long`。
- **1 対 1 対 1 は 1 つのトランザクションで書く**（MR1 の Amendment の約束）: プロジェクト、名前空間（`kind = 'project'`、`owner_id = projects.id`）、
  リポジトリ（`provisioning`、種類は既定 `system`）、作成者の直接の Owner、`repository.provision` のジョブ、`project.created` の監査の行とイベントを、
  1 つの `ChangeTx` で書く。テスト `project_create_writes_namespace_project_repository_owner_job_and_one_audit_row` が、3 つがそろって存在すること
  （1 つの結合の問い合わせ）と、全体が 1 つのトランザクション（同じ `txid`）であることを確かめる。拒否された作成は何も残さない（各テストが件数を比べる）。
- 作成者（`NewProject.creator`）は呼び出し側が渡すプリンシパルで、有効な人間かサービスアカウントに限る。bot はグループの最初の Owner と同じく
  FG-REPO-0010 `bot_member`、無効なユーザーは FG-REPO-0014 `creator_inactive`。誰がその名前空間に作ってよいかは `authorize()` が決め、このコマンドは決めない
  （D4 には経路がなく、権限のマトリクスは変わらない）。
- 可視性の既定は `private`、リポジトリの種類の既定は `system`（回答 5）。
- **レビュー（security / database）後の追加**:
  - 親が個人の名前空間のとき、その持ち主はプロジェクトの Owner を継承するので、持ち主のユーザーの行も `FOR SHARE` で読み、bot なら
    FG-REPO-0010 `bot_member`、有効でなければ FG-REPO-0014 `parent_owner_inactive` で拒否する（すべての種類のユーザーが個人の名前空間を持つため、
    これがないと管理者が `ci-bot/foo` を作ると bot が Owner になる）。
  - 作成者は変更の行為者でなければならない（`Actor::User` / `ServiceAccount` はその本人、`UserViaServiceAccount` はその人）。違えば FG-REPO-0014
    `creator_not_actor`。`Actor::System`（操作者の CLI とワーカー、ADR-0019 §8 と同じ信頼の起点）だけが他のユーザーを作成者に指定できる。
    経路は認証済みのプリンシパルを渡し、その前に `authorize()` で作成の可否を決める。
  - **既知の TOCTOU**: `authorize()` の判定（呼び出し側）と、このコマンドが親の名前空間を `FOR SHARE` で読むまでの間に、たとえばグループの Owner が
    外されても、その作成は通る（判定に使った役割は読み直さない）。影響は「外される直前に始めた 1 回の作成」に限られ、作成者は監査の行に残る。
    経路を入れる手順で、役割の読み込みと作成を同じトランザクションにするかを決める。
  - 2 接続の競合のテスト（`projects/tests/race.rs`）: 作成と作成者の無効化（両方の順序を固定）、作成と同じパスの墓標の書き込み。
    作成者の `FOR SHARE` やパスのロックを外すと赤くなることを、一度ずつ変異で確かめた。
  - **無効化とプロジェクトの Owner**: `users::disable` は、直接の Owner を失うプロジェクトを数えない（この MR では変えない）。プロジェクトの
    最後の Owner の方針（回答 2: 継承された Owner がいる間は拒まない）と、それが無効化のガードと doctor（ADR-0019 §10〜12）に与える影響は、
    D4 MR4（プロジェクトのメンバー）で決めて実装する。

## Amendment（2026-10-31、手順 D4 MR4: プロジェクトのメンバー）

- プロジェクトの直接のメンバーは `projects::{add_member, change_member, remove_member}` で変える。監査の行はプロジェクト（`project` / id、
  `project_id` あり）に付け、`member.*` のイベント（分類 `maintainer`、ADR-0016 §5）を同じ変更で出す。グループのメンバーの変更は監査だけのまま。
- 最後の Owner の方針（継承された有効な Owner がいる間は最後の直接の Owner を外せる）と、それが無効化のガードと doctor に与える影響は
  ADR-0019 の Amendment（D4 MR4）に書く。

## Amendment（2026-10-30、M1 phase 8 MR1: forge-core のパスの解析）

上の決定が開けたままにしていた点を、次のとおり決めた（`forge_core::web_path`、`namespace_path`、`namespace_resolver`）。

- **`.git` の規則は名前空間のセグメントだけに掛かる。** `/-/` の後ろ（と `/-/projects/{id}/…` の後ろ）は ref やファイルの名前なので、
  `lib.git` のような `.git` で終わるセグメントを受け付ける。形を決めるのは `/-/` より前だけで、後ろの名前が形を変えることはないからである。
  `.git` の検出は ASCII の大文字小文字を区別しない（`X.GIT` は `x`。名前空間の小文字化と合わせる）。
- **拒否の `reason`**: FG-WEB-0001 の `reason` は `path_rejected`（文字列の規則）、`unknown_route`（未知の語、トップレベルの予約語、
  `/-/` の未知の語、`/-` だけ、`-` の後ろに語がない）、`service_unsupported`（`info/refs` の `service` がない、重複、未知）の 3 つ。
  どれにも `part`（`path` / `namespace` / `tail` / `git` / `query` / `instance` / `word` / `reserved_top_level`）と、`path_rejected` では
  `rule`（破った最初の規則）を付ける。入力そのものは入れない（ログには `log_safe_path` で出す）。
  - `service_unsupported` は、phase 9 が `SmartHttpServiceUnsupported`（400）を登録するまでの仮の扱いで、それまでは 404 になる。
    `service` は、キーも値もデコードせず完全一致で比べる（`git%2Dupload-pack` は未知）。ほかのキーは無視する。POST と他の形はクエリを見ない。
- **生のパスの上限は 8192 バイト**（`MAX_RAW_PATH_LEN`）。超えたものは区切る前に `path_rejected`（`rule=too_long`）にする。`/` で始まらないもの、
  `/` だけのものも `path_rejected`。クエリは、どの形でも 2048 バイト（`MAX_QUERY_LEN`）までにし、超えたものは区切る前に
  `path_rejected`（`part=query`、`rule=too_long`）にする。
- **HTTP のメソッドは解析では見ない。** 形とメソッドの組（`info/refs` は GET / HEAD、`git-upload-pack` は POST など）は、MR4 のルートの表が決める。
- **デコードは厳密にする。** `%` の後に 16 進数 2 桁がなければ拒否する（文字として通さない）。制御文字は C0、DEL に加えて C1（U+0080〜U+009F）も拒否する。
  `/-/` の後ろの `-` だけのセグメントは名前として受け付ける。`%252F` は 1 回だけデコードして `%2F` という名前になる。
- **見えない書式文字**: `/-/` の後ろのセグメントは、ゼロ幅・bidi の制御・行と段落の区切り・語の結合子・BOM・タグ文字
  （U+200B〜U+200F、U+2028〜U+202E、U+2060〜U+2064、U+2066〜U+2069、U+FEFF、U+E0000〜U+E007F）を含めば
  `path_rejected`（`part=tail`、`rule=invisible`）にする。ファイル名や ref の見た目を偽装できないようにするためである。
- **`-` で始まるセグメント**: `/-/` の後ろのセグメントは `-` で始まってもよい（`--output=x` は正しいファイル名・ref 名である）。このため、
  **MR4 と phase 9 の要件として**、`TailSegments` から git の引数に届く値は、必ず `--` / `--end-of-options` の後ろに置くか、
  解決し終えた完全な ref 名か oid にしてから渡す（上の「ref とパスの境目」と同じ規則）。
- **固定リンクの ID** は、先頭が 0 でない 10 進数で、`i64` の正の値に収まるものだけを受け付ける（それ以外は `path_rejected`、`rule=invalid_id`）。
  殻は DB を見ないので、存在は確かめない。
- **解決と認可の入力は 1 回の呼び出しで得る。** `NamespaceResolver::resolve(&LookupPath, viewer: Option<UserId>) -> Resolution` が、
  パスの解決と、閲覧者の認可の入力の読み込みを同時に行う。解決の後で別の照会をすると、ないパスと見えないパスで照会の回数が変わり、
  時間の差から存在が分かってしまうからである（判定の順序の 6）。実装は、結果（今のパス・移動・削除・なし、ユーザー・グループ・プロジェクト）にも、
  閲覧者に見えるかどうかにも関係なく、**同じ照会を同じ回数だけ** 行わなければならない（MR3 は 1 つの文で実装する）。見えるかどうかは `authorize()` が決め、ここでは決めない。
  `viewer` は匿名なら `None`（公開のプロジェクトや clone も同じ経路を通る）。
- **解決の型**: `Resolved` は `{ namespace_id, target: ResolvedTarget }` とし、`ResolvedTarget` は `User(UserId)`、
  `Group { id, viewer_role }`、`Project { id, access: ProjectAccess { visibility, role, pending_deletion } }` にした
  （種類と ID と認可の入力が食い違えないため。`owner()` で型付きの `NamespaceOwner` を返す）。`Deleted` と `Absent` は何も持たない。
  `Resolution::Moved` は、ID から組み立てて今のパスへの完全一致で確かめた `current: NamespacePath` も持つ（upload-pack の `302` と
  SPA の `canonical_path` に使う。見せるのは認可の後）。`resolve_prefix` は、`prefix=true` と一緒に M2 で足す。
- **ID で指すプロジェクト**（`GET /api/v1/projects/{id}`）には `ProjectAccessLoader::project_access(ProjectId, viewer: Option<UserId>)
  -> Option<ProjectAccess>` を使う（`None` はない。同じ照会の規則）。forge-core は forge-auth より下にあるので、forge-auth の型は使わず、
  forge-web（MR4）が forge-auth の文脈の型に変換する。
- **ログ**: `log_safe_path` は最初の 256 文字だけを残し（切ったときは `…(+残りの文字数)`）、`\` を二重にし、制御文字と、bidi の制御・
  ゼロ幅などの見えない書式文字を `\u{…}` にする。
- **テストベクタ**は `tests/fixtures/url-vectors.json`（リポジトリのルート）。`form` は `both`（ブラウザの `new URL()` でも変わらない）、
  `browser`（`browser_input` を `new URL()` で正規化した形）、`raw`（`curl --path-as-is` でしか送れない）の 3 つで、ラベルは WHATWG の URL 解析で確かめた。
  `dot_git` が `.git` の許されない場所の印である。
