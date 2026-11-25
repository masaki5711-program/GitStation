# ADR-0040: ナレッジ — Wiki は結び付けた普通のプロジェクト、ノートは自分の非公開プロジェクト、検索はアプリ側の 2-gram と `tsvector`

**Date**: 2026-11-23
**Status**: proposed（accepted は K1〜K3 の `/review` で CRITICAL / HIGH なしのとき）
**Deciders**: Masaki Fujikawa（ユーザー、2026-11-23「計画 19・20・21 の未決の問いはすべて推奨の既定で」）、Claude

## Context

M6（計画 20）は Wiki・個人ノート・横断検索（と最小の Issue）を足す。条件は CLAUDE.md §3: 外部の検索サービスを使わない（オンプレ）、PostgreSQL 16 だけ（拡張を足さない — TimescaleDB の像と PFS のサーバーで同じ構成を保つ）、読めない物は 1 件も出さない（判定は `authorize()` の 1 か所）。コードの事実: 全文検索は無い（`tsvector`・`pg_trgm` は未使用）、一覧は行ごとに `authorize()` で決め SQL では決めない（`routes/lists.rs`、案 L1）、1 プロジェクト = 1 リポジトリ（`repositories UNIQUE (project_id)`）、ADR-0015 は `/-/wiki` と `{path}.wiki.git` を M6 用に予約だけしている。

## Decision

1. **検索の方式（S-b）**: 分かち書きは純粋な Rust（`forge-core::search_text`）。NFKC → 小文字 → 和文（漢字・かな・カナ・ハングル）の連続は 2-gram（索引の側は連続の最後の 1 文字も）、それ以外の文字・数字の連続は 1 語（64 文字で切る）。索引は位置と重み（題名 A、パス・ID・見出し B、本文 C）つきの `tsvector` の **文字列表現を Rust で組み**、DB は `$1::tsvector` で受けるだけ。問い合わせも同じ関数で `tsquery` の文字列（全語の `&`、最後の語と 1 文字の和文は前方一致 `:*`）を組み、`@@` + `ts_rank_cd` + 新しさで並べる。DB のロケール・テキスト検索の設定・拡張に依存しない。最短の語は 2 文字（Q7）、最長 200 文字。
2. **索引は派生のデータ**: `forge.search_entries`（`source_kind` = project / merge_request（K4 で document、I-1 で issue）、`UNIQUE (source_kind, project_id, source_key)`、`tokens` に GIN）と `forge.search_index_state`。`tables.toml` で `derived`、監査の行は要らない、いつでも `forge search reindex` で作り直せる。`project_id` に外部キーを張らない（プロジェクトの完全削除の手順を増やさない）: 消えたプロジェクトの行は検索の結果の判定で落ち、購読者と作り直しが消す。1 件の本文は 256 KB、語は 5 万まで。超えたら `truncated` を立て、ログに残す（黙って切らない）。
3. **更新**: outbox の購読者 `search-index`（ordered: 失敗は同じイベントでやり直し、飛ばさない）。プロジェクトのイベント（作成・移動・削除・復元・完全削除）と MR のイベント（作成・更新・閉じる・再開・マージ）で、元の表から読み直して 1 行を書き換える。イベントの無い変更（プロジェクトの説明の編集など）は、そのプロジェクトの次のイベントか作り直しで追い付く。
4. **読める物だけを返す**: `GET /api/v1/search` は新しい Action **`SearchQuery`**（インスタンス、サインインした有効な利用者、トークンは `read_api`、エージェント可、代理・Runner・Job・ポータルは不可）を registry で判定する。候補は順位の順に読み、**1 件ずつ `authorize()`**: プロジェクトは `ProjectRead`、MR は `MergeRequestList`（その MR のプロジェクト、MR の読みと同じ Reporter の条件）。判定の材料はプロジェクトごとに 1 回（`lists::project_records`、1 文）。SQL の条件（種類・プロジェクト）は絞るためだけ。件数は返さない。走査の上限・利用者と問い合わせに封印したカーソル・同時実行の上限は一覧と同じ形。AI の利用者（`is_agent_audience`）には `confidential` の行を返さず数えない。抜粋は保存した本文から作り、強調は文字（コードポイント）の範囲で返す（HTML を返さない）。問い合わせの文字列はログに出さない（長さだけ）。
5. **Wiki（W-C）**: 「Wiki を作る」で同じ名前空間に普通のプロジェクトを作り `project_wikis` で結ぶ。主プロジェクトの役割を同じ強さで継ぐ（片方向・1 段、`forge-auth` の役割の解決の 1 か所、Q2）。保存は直接のコミット（Q3）、ID は `WIKI-0001` からの連番（Q6）。`/-/wiki` は結び付けた Wiki の画面への入口に使い、ADR-0015 の予約を解く。`{path}.wiki.git` は予約のまま。
6. **ノート（N-B）**: 自分の名前空間の非公開プロジェクト `<user>/notes`、`user_notes` で指す。既定の区分は `internal`（Q4）。
7. **Issue（I-1、Q1）**: 最小の Issue を M6 の最後に作る。既定は無効、Kanflow に結んだプロジェクトでは有効にできない。この ADR への Amendment で詳細を決める。
8. **クレートの置き場所**: 分かち書きは `forge-core`、索引の表・購読者・問い合わせは `forge-db`、判定と組み立ては `forge-web` のルート（一覧と同じ形: `forge-core` のポートを `forge-db` が実装し `forge serve` が注入する）。計画 20 §3.5 の機能クレート `forge-search` は、K4 の文書の索引で forge-web と forge-cli が同じ手順を共有する必要が出たときに作る（今は依存の向きの表を変えない）。
9. **エラーの領域 `KNOW`**: 検索の文の時間切れ・DB の失敗は `FG-KNOW-0001`（利用者には「いま検索できません」+ `request_id`）、1 件・1 プロジェクトの索引の失敗は `FG-KNOW-0002`（状態の表に残し、次のイベント・作り直しでやり直す）。問い合わせの形の誤りは既存の `FG-WEB-0006 reason=param_invalid param=q`、同時実行の上限は `FG-WEB-0008`。

## Alternatives Considered

### S-a `pg_trgm`
- **Pros**: PG に同梱、`ILIKE` がそのまま速くなる。
- **Cons**: 日本語の扱いが DB の `LC_CTYPE` に依存し、開発のコンテナと PFS の本番で同じと言い切れない。2 文字の語が 3-gram に足りない。順位付けが弱い。
- **Why not**: 2 文字の語（「設備」「承認」）が主な用途で、ロケールの差は再現しにくい障害の元。

### S-c PGroonga / pg_bigm、S-d Elasticsearch / Meilisearch
- **Pros**: 日本語の精度・機能が多い。
- **Cons**: PG の配布物の外の拡張、または別のサービスの運用。
- **Why not**: 条件（拡張を足さない、外部の検索サービスなし）に反する。

### `to_tsvector('simple', …)` に分かち書きした文字列を渡す
- **Pros**: SQL が短い。
- **Cons**: DB のパーサーが語をもう一度切る（記号・数字の扱いが PG の版と設定に依存）。
- **Why not**: 規則を Rust の単体テストだけで決めたい。文字列表現のキャストなら DB は語を解釈しない。

### Wiki = 2 つめのリポジトリ（W-A）、同じリポジトリの `wiki/`（W-B）
- **Why not**: W-A は `UNIQUE (project_id)` と保存先・バックアップ・ミラー・権限を全部直す。W-B は Wiki の直しにコードの保護ブランチが掛かり、非エンジニアだけに書かせられない（計画 20 §3.2）。

## Consequences

### Positive
- 分かち書きの規則が DB に依存せず、テストで固定できる。開発と本番で同じ結果。
- 結果の判定は既存の `ProjectRead` / MR の読みの規則そのまま。新しい読みの規則を作らない。
- Wiki・ノートは普通のプロジェクトなので、保存・clone・バックアップ・検索（K4）がそのまま効く。

### Negative
- 2-gram は偽陽性がある（「京都」で「東京都」が出る）。題名の重みと新しさで並べて和らげる。
- 索引は購読者の遅れの分だけ古い。イベントの無い変更は次のイベントか作り直しまで古い。
- 候補を順位の順に読み 1 件ずつ判定するので、読めない候補が多いと短いページになる（`has_more: true`）。

### Risks
- **件数・順位・抜粋から読めない物が漏れる** → 件数を返さない、判定の前に何も組み立てない、カーソルを利用者と問い合わせに封印、権限の表の行と K0 の eval。
- **分かち書きの規則を変えると古い索引と合わない** → 変えたら `forge search reindex` で全体を作り直す（runbook `search-index`）。
- **大きな表で遅い** → 文の時間切れ 2 s（`FG-KNOW-0001`）、走査の上限、10 万件で p95 ≤ 300 ms を K5 の前に測って記録する。

## Amendment K3-R（2026-11-23、独立レビューの指摘への対応）

- **読めない行は数えさせない（HIGH）**: 候補を読む **前に**、一覧と同じ判定の材料（`ProjectPageLoader`）でプロジェクトを 1 件ずつ `authorize()` し、`ProjectRead` の通ったプロジェクトと、さらに `MergeRequestList` の通ったプロジェクトの id の集合を作る。検索の文は `project_id = ANY(その集合)` の行だけを順位付け・ページ送りする。だから短いページ・`has_more`・`project=<読めない id>` から隠れた一致の数は分からない（読めないプロジェクトの id は、存在しない id と同じ答え）。1 件ずつの `authorize()` はその後も残す。集合を作るための走査は 2 万プロジェクトまで。超えたら `FG-KNOW-0001 reason=project_scan_limit` で断り、その規模になったら SQL の上位集合（一覧の案 L1 と同じ）に移す。
- **1 つの問い合わせの仕事に上限**: 一致した行のうち **新しい順に 5000 件だけ** を順位付けする（それより古い一致は、語を足した問い合わせで見つける）。前方一致の語は 1 つまで（最後の語が 2 文字以上のとき、なければ最後の 1 文字の和文）。候補が運ぶ本文は先頭 16 384 文字、抜粋はその範囲だけを走査し、探す断片は 8 つまで、組み立ては `spawn_blocking` で行う。
- **書き込みの増幅**: 本文・語・区分・切り詰めが変わらない行は書き直さない（`ON CONFLICT … WHERE … IS DISTINCT FROM`）。作り直しも消してから書かず、上書きと消えた MR の行の削除だけ。GIN は `fastupdate = on`、`gin_pending_list_limit = 512`（kB）。
- **2000 を超えるプロジェクトのパスの変更**: 残りのプロジェクトを `search_index_state` に `failed`（`FG-KNOW-0002`）で残し、古いことが見えるようにする（`forge search reindex` で直る）。
- 作り直しのトランザクションは `statement_timeout` 30 s・`lock_timeout` 5 s。孤児の行は 1000 件ずつ消す。

## Amendment K3-R2（2026-11-24、再レビュー）

- **受け入れた残りの経路（時間）**: GIN の `tokens @@ q` はインスタンス全体の一致を集めてから `project_id = ANY(読める集合)` で絞る。だから文の時間と 2 s の時間切れ（`FG-KNOW-0001`）は、読めない行の一致の数でも伸びる。拡張（`btree_gin` など）を足さない条件（§決定 1）では、索引の中でプロジェクトと語を一緒に引けない。**時間の差だけが残る経路として受け入れる**。抑え: 語は 32 まで（`FG-WEB-0006 rule=too_many_terms`）、前方一致は 1 つまで、順位付けは新しい 5000 件まで、同時実行の上限（一覧と同じ `FG-WEB-0008`）、文の時間切れ 2 s、結果・件数・`has_more` には出ない。問い合わせの語はログに出さない。拡張を足せる日が来たら、`(project_id, tokens)` の GIN で閉じる。
- **読める集合の上限は読める物だけを数える**: 読めないプロジェクトの数では断らない。`project=` が付いたら 1 件だけ読む（走査しない）。2 万件ちょうどは答え、2 万 1 件目から断る。

## Amendment W1/W2（2026-11-24、Wiki の結び付けと作成）

- **継承を決める 1 か所**: `forge-auth` は純粋（DB を読まない）で、`authorize()` は役割を入力として受け取るだけ。役割を計算する唯一の場所は `forge-db` の `roles::chain_role_columns!`（ADR-0038 §2 の組織の結び付けと同じ場所）なので、継承はそこに置いた。Wiki のプロジェクトの段で、主プロジェクトの **自分の** 役割（直接の所属と組織の結び付け）を `forge.role_max` で足す。主プロジェクトの継承した役割は読まない（1 段）。祖先のグループの役割は、Wiki が同じ名前空間にあるので Wiki 自身の鎖に既に入っている（移動した Wiki は主プロジェクト自身の役割だけを継ぐ — 主プロジェクトが与える以上にはならない）。読み込み・一覧・書き込みの中の読み直し（`lock_roles`、結び付けと主プロジェクトの所属も `FOR SHARE`）がすべてこの式を通る。毎回のリクエストで計算するので、外したメンバーは次のリクエストから失う。
- **表**: `forge.project_wikis`（`project_id` PK、`wiki_project_id` UNIQUE、自分自身を禁止する CHECK、連鎖を禁止するトリガ `project_wikis_no_chain` はトランザクションの advisory lock で直列化）。台帳の種類は `derived`（`org_bindings` と同じ）: 結び付けと外しのコマンドが主プロジェクトに監査の行（`project.wiki_linked` / `project.wiki_unlinked`）を書く。どちらかのプロジェクトの完全削除は行を消し、もう片方は残る。主プロジェクトの削除の猶予の間は、Wiki も結び付けも残る（戻せば元どおり）。
- **Action**: `ProjectWikiLink`（主プロジェクト、Maintainer 以上、人だけ、`api`、代理・CI は不可）。作成はさらに名前空間での `ProjectCreate` を呼び出し側の資格情報で `prove` し、作成のトランザクションの中でもう一度決める。保護の既定（`refs/heads/main`、Developer、強制 push と削除は不可）も同じく `ProtectedRefManage` を呼び出し側で決める。
- **作成の手順（W2）**: 作成 → リポジトリをその場で作る（キューの `repository.provision` は準備済みを見て何もしない。同じトークンの同時の作成は採用で吸収）→ 最初のコミット（親なし、`main` の作成だけで上書きしない、push と同じ記録）→ 保護 → **最後に** 結び付け。作成の後の失敗は `FG-KNOW-0004`（`step`、`wiki_project_id`）で、結び付けは書かない。途中のプロジェクトは消さずに残し、利用者が消すか別のパスで作り直す（runbook `knowledge`）。拒否は `FG-KNOW-0003`（`already_linked`・`chain`・`name_taken`・`not_linked` など）。
- **最初のコミットに別の `RepoPush` の判定を置かない**: 最初のコミットは Wiki のプロジェクトの作成の一部（作成者は Owner、`ProjectWikiLink` と `ProjectCreate` で決めた）で、`api` のトークンには `write_repository` が無いことがあるため。途中で降格された Maintainer は結び付けの直前の決め直し（`link_decided` の reconfirm）で止まり、結び付けの無い途中のプロジェクトだけが残る（`FG-KNOW-0004`、runbook `knowledge`）。

## Amendment K4（2026-11-24、文書の索引）

- **きっかけと決める場所**: 購読者 `search-index` は、ブランチを動かした `repository.pushed` ごとにジョブ `search.index_documents`（イベントごとに重複を除く）を入れる。既定ブランチは git の `HEAD` なので、どのブランチかはジョブが決める: 既定ブランチの先端を読み（このプロセスの ref のキャッシュは捨てる）、索引済みの SHA と同じなら何もしない（他のブランチへの push）。`forge search reindex` はプロジェクトごとに全体のジョブを入れる。
- **変わったパスだけ**: 先端の一覧と索引済みの SHA の一覧を、パスとブロブの id で比べる。全体を読み直すのは、索引が無い・作り直し・古い SHA が読めない（強制 push と gc）・一覧が切れた・型の定義が変わった（全文書の区分が変わりうる）・読みの上限に当たったとき。原文が変わったら翻訳も書き直す（区分が原文に従うため）。区分の計算は常に先端の全体の読み（`forge_checks::doc_lint::read_tree` の上限、文書の API と同じ）で行う。
- **区分は先端で**: 文書自身の区分と型の既定、そしてエージェントの読みと同じ `confidential_in`（型が読めない、機密の原文の翻訳、読めない front matter）。
- **閉じる側**: 上限に当たった読みは `FG-KNOW-0002 reason=read_bound`（warn）。読めた分だけ書き、読めなかった文書の行は `confidential` に倒し、完全な一覧に無いパスの行だけ消す。状態は `failed`・SHA なし（次の push で全体を読む）。ジョブが最後の試行でも失敗したら `reason=read_failed` で状態に `failed` を残す。
- **書き込みの順序**: 1 トランザクション（文の時間切れ 30 s、ロック待ち 5 s）。`search_index_state` の行をロックし、ジョブが読み始めた SHA と比べる（間に別のジョブが書いたら読み直す、3 回まで）。
- **検索の結果**: `scope=document`。文書の行は `RepoBrowse`（文書の API と同じ Action）の通ったプロジェクトだけを候補にし、1 件ずつもう一度判定する。結果の `path` は先端のパス。行の `source_updated_at` は索引に入った時刻。
- 新しい機能クレート `forge-search` はまだ作らない: 文書の読みと行の組み立ては `forge-checks::doc_search`（既に `read_tree` と `confidential_in` を持つ機能クレート）、書き込みは `forge-db`、ジョブは `forge-cli`。依存の向きは変わらない。

## Amendment W3（2026-11-25、Wiki への参照）

- **書き方と置き場所**: `[[wiki:<id>]]` / `[[wiki:<id>#<block>]]`。`forge-doc` は純粋なまま（`Reference::wiki`、`wiki` はリポジトリのパスに使えない予約語）。lint は呼び出し側が読んだ Wiki の索引（`WikiRefs::Linked` / `NotLinked` / `Unchecked`）を受け取るだけ。
- **lint は警告だけ**: 壊れた参照は `wiki_ref_broken`、Wiki の無いプロジェクトでは `wiki_not_linked`。どちらも常に警告（`FG-DOC-0008`）で、`forge/doc-lint` を失敗にしない（Wiki は別に編集されるので、主プロジェクトのマージを止めない）。`check.spec_index` ジョブは、文書に `[[wiki:` があるときだけ結び付けた Wiki の既定ブランチの先端を lint の上限で読む。上限に当たった・リポジトリが準備中なら確かめずに、要約にその 1 行を書く（黙って通さない）。`gstn doc lint`・差分の lint・マージキューの列車の lint は確かめない（Wiki を読めない・止める理由にしない）。
- **描画は閲覧者ごと**: 文書の API は、文書に Wiki への参照があるときだけ、結び付け（主プロジェクトの側だけ）→ Wiki のプロジェクトを閲覧者で読み込み → `prove(RepoBrowse)` で決める。拒否・無い・準備中なら `unreadable`（ID だけ、Wiki は何も読まないのでページの有無も分からない）。許されたら先端を 1 回読み（閲覧者の予算に数える）、`found` / `hidden`（この文書より機密）/ `broken`。エージェントには先端で機密のページを `hidden` にする。結び付けが無ければ `no_wiki`。
