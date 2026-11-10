# ADR-0023: リポジトリの種類（spec / system）と文書ポータルをやめる

**Date**: 2026-11-01
**Status**: accepted（ADR-0004 の「種類」の部分を置き換える。文書だけの変更で、コードの削除は後続の MR）
**Deciders**: Masaki Fujikawa（ユーザー、2026-11-01 のチャットでの決定）、Claude

## Context

- ADR-0004 は、リポジトリに種類 `spec`（仕様）/ `system`（システム開発）を持たせ、`spec` は Git を知らない人にも開くため
  「文書ポータル」（Git の言葉を出さない別の画面系統。計画 01 §3.7、08、10 §4.7）で扱う前提にした。
  M2 の MR12 `feat/ui-document-portal` で、`kind=spec` のプロジェクトの読み取り専用の画面まで実装済み（統合済み、`f95e2e8`）。
- 実際に使う段階で、次の問題が見えた。
  - 画面の系統が 2 つあると、言葉（Git の語と言い換えの語）、翻訳、アクセシビリティ、テストが二重になる。保守は Claude が 1 人で行うので、
    二重の維持は診断の対象を増やす。
  - 種類は作成時に決まり変わらない（ADR-0004 の Amendment）。しかし「仕様を持つか」は、リポジトリの中身（`spec/` があるか）で決まる事実であり、
    ラベルと食い違う。コードと同じ MR で変える部品レベルの仕様（案 A）が既定になるなら、ラベルは判断の材料にならない。
  - 利用者（2026-11-01）は、Git の基本は利用者が学んでから GitStation を使う前提でよいと決めた。
- 複数のシステムで共有する仕様（計画 03 §3.1 の案 B）の需要は残る。ただし、これは「種類」ではなく「別のリポジトリを版を固定して参照する」
  ことで足りる（計画 03 §4.4）。

## Decision

1. **文書ポータルをやめる。** 画面の系統は 1 つ（リポジトリの画面）だけにする。Git を知らない人にも、リポジトリの画面と Git の基本の学習で開く。
   M2 の MR12 の画面は後続のコードの MR で取り除く（この ADR と同じ MR ではコードを変えない）。M7「文書ポータル」の別の画面の系統も作らない。
2. **リポジトリの種類 `spec` / `system` の区別をやめる。** すべてのリポジトリは同じ種類のリポジトリである。
   - リポジトリの仕様は、そのリポジトリの `spec/` に置く（計画 03 の案 A を既定にする）。
   - 複数のシステムで共有する仕様は、**ふつうのリポジトリ**（特別な種類なし）に置き、他のリポジトリが `.forge/repo.yml` で版を固定して参照する。
     ```yaml
     # .forge/repo.yml
     specs:
       - repo: <リポジトリのパス>
         version: <タグ>        # 準拠する版を固定する（"track: main" も可）
         scope: [<要件 ID のパターン>]
       - path: spec/            # このリポジトリ自身の仕様
     ```
   - 「仕様を持つ」は中身（`spec/` の有無、`specs:` の宣言）から導く。ラベルでは決めない。`.forge/repo.yml` の `kind:` の項目はなくす。
3. **リポジトリをまたぐ連携は、まだ作らない。M3 の、はっきり分けた項目にする。** 仕様の参照、仕様のずれ（spec-drift）、
   要件 → PBI → MR → テストの、リポジトリをまたぐ追跡、複数のリポジトリにまたがる AI の文脈、Kanflow のプロジェクトへの複数のリポジトリの登録。
   MR と一緒に設計する。最初の部分は次の順:
   - (a) `.forge/repo.yml` の解析と、仕様の参照
   - (b) 仕様のずれの通知（`forge/spec-drift`）
   - (c) Kanflow のプロジェクト ↔ 複数のリポジトリの登録

   計画 03 の考え（要件 ID、`@req`、spec-lint / spec-trace / spec-conformance / spec-drift）は残す。外すのは、spec と system の区別と、ポータルだけ。

## Alternatives Considered

### ポータルを残して隠す（機能フラグで無効にする）
- **Pros**: 実装した MR12 の成果を捨てない。後で戻しやすい。
- **Cons**: 使われない画面にも、翻訳、a11y、E2E、依存の更新、セキュリティの審査が掛かる。死んだコードは診断のときの紛らわしさになる。
- **Why not**: Claude が保守する前提で、使わない系統を持ち続ける費用が見合わない。必要になったら Git の履歴（`f95e2e8`）から戻せる。

### ポータルをリポジトリの画面に統合する（やさしい表示のモード）
- **Pros**: 画面は 1 つのまま、Git を知らない人にも言葉をやさしくできる。
- **Cons**: 画面の中に 2 つの語彙（Git の語と言い換えの語）が残る。モードごとのテストと翻訳が要る。
- **Why not**: 利用者が「基本の Git を学んでから使う」と決めた。語彙を 1 つにする方が単純。

### 種類をラベルだけとして残す
- **Pros**: データと API をそのまま保てる。一覧の絞り込みなどが使える。
- **Cons**: ラベルと中身が食い違える。作成時に決めて変えられないので、間違えたときの直し方がない。「仕様がある」かどうかは中身で分かる。
- **Why not**: 判断の材料にならないラベルを持つ費用の方が大きい。「仕様を持つか」は中身から導く（決定 2）。

## Consequences

### Positive
- 画面の系統、言葉、テスト、翻訳が 1 つになる。
- 共有する仕様は「ふつうのリポジトリ + 版の固定の参照」で足り、リポジトリの種類ごとの規則（権限の既定値、マージの条件）を持たなくてよい。
- 領域 D（Git を知らない人への開放）は、リポジトリの画面（Markdown の表示、タグ = 版、比較）と Git の基本の学習で満たす。

### Negative
- Git を知らない人は、そのままでは使えない。学習の資料と支援が要る。
- M2 の MR12 の実装は、導入した直後に取り除く（作業の無駄）。
- Office / PDF の仕様の取り込みと比較、変更の要約、Git を知らない承認者向けの画面は、いま計画に居場所がない（未決定。下の Risks）。

### 具体的な影響
- **DB**: `forge.repositories.kind`（CHECK `spec` / `system`、`forge_app` に UPDATE の権限なし）の列は残る。マイグレーションは追記のみ（CLAUDE.md の契約 6）で、
  適用済みのものは書き換えない。列は使われなくなる。後続の MR が、作成時に固定の値を入れるか、新しいマイグレーションで既定値を足すかを決める。
  列の値に意味を持たせない。
- **API**: `POST /api/v1/projects` の `kind` と、`GET /api/v1/projects?kind=` の絞り込みは取り除く（応答の `kind` の欄も）。
  まだ外に出していない API なので、互換のための無視の期間は置かない（後続の MR で確認し、要るならそこで決める）。
- **`gstn`**: `project create --kind` を取り除く。
- **web**: 作成フォームの種類の選択、MR12 の文書ポータル、言い換えの辞書の `plain.*` の仕組みと禁止語の検査、ホームの「仕様書（`kind=spec`）」を取り除く。
- **Kanflow の連携（計画 02、03 §7）**: 結びつけの `role: spec|system` をなくす。結びつけるのは、ふつうのリポジトリ。
  計画 03 §7.1 の「1 つの system リポジトリを複数の Kanflow のプロジェクトに結びつけない」は、共有する仕様のリポジトリだけを複数のプロジェクトで共有してよい、
  という形に直す（要件 ID が全体で一意であること）。
- **ADR-0017 の「仕様のプロジェクト」**: 「`.forge/repo.yml` の `specs:` で固定されたリポジトリ」のことになる。読み取りの権限の規則は変わらない
  （種類ではなく、参照されているかどうかで決まる）。
- **テストサーバーの試験（2026-11-02）**: 使うコミット `f95e2e8` には文書ポータルと `kind` が **含まれる**。この ADR で取り除くと決めたものが
  入ったビルドである。試験では文書ポータルの確認は不要。

### Risks
- 非エンジニアが GitStation を使えず、領域 D が進まない → 学習の資料を用意する。R3（全社展開）の前に、実際の利用状況で再検討する。
  必要なら、リポジトリの画面の中の機能（Markdown の版の比較、承認）として扱い、別の画面の系統は作らない。
- 連携が M3 まで入らないので、それまで共有の仕様の参照は手作業 → M3 の項目として順序（a）〜（c）を明示した（計画 01 §3.1・§8、03 §8）。

## 後続のコードの MR の範囲

別の MR（`feat` / `refactor`）で行う。この ADR の MR は文書だけ。

- `forge-core` の `RepositoryKind` と、作成の入力、`forge-db` の作成の処理（`kind` の値の扱い。固定値か、追記のマイグレーション）。
- API: 作成の `kind`、一覧の `?kind=`、応答の `kind` を除く。OpenAPI（utoipa）、`docs/authz/routes.generated.json`、`matrix.generated.json` を再生成する。
  権限の表のテストの行と、カーソルの purpose（`projects.list:…:kind=…`）を直す。
- `gstn`: `--kind` を除く。`crates/forge-client` の `registry.rs`、カバレッジの表（`coverage.rs`）、`tests/snapshots/help.json`、`tests/hosting.rs`。
- web: 作成フォームの種類の選択、MR12 の文書ポータル（画面、ルート、辞書、e2e）、`plain.*` と禁止語の検査、ホームの仕様書の一覧を除く。
- テスト: 上の各所のテスト（`crates/forge-web/tests/lists.rs`、`routes/lists_tests.rs` など）と web の vitest / Playwright。
- ゲート: `cargo fmt`、`clippy -D warnings`、`cargo nextest run --workspace -j 4`、web のテスト、`gen-catalog --check`、`authz-coverage`。

## Amendment

この ADR に対応する、既存の ADR への追記は次のとおり（それぞれの末尾の Amendment）。

- ADR-0004: 種類（spec / system）の部分を置き換え。要件 ID、`@req`、`.forge/repo.yml` の `specs:` は残る。
- ADR-0010: 「リポジトリは種類を持つ」を置き換え。
- ADR-0015: 「リポジトリの種類」の節、`project_view` の `kind`、一覧の `?kind=` を置き換え。
- ADR-0017: 「仕様のプロジェクト」の読み替え。
- ADR-0022: 文書ポータルと `plain.*` の部分を置き換え。

## Amendment 2026-11-01 — エンジニアでない人の承認（利用者の決定）

- 仕様の変更の承認は、**Git の基本を覚えてもらったうえで、リポジトリの画面と MR の承認で行う**。別の承認用の画面は作らない。
  上の Consequences と Risks で「未決定」とした「Git を知らない承認者向けの画面」はこれで決着する。
- **MR の承認は kanflow からも行えるようにする**（計画 02 F-36）。kanflow は代理実行（計画 02 §5.3、`Forge-Act-As`）で forge を呼び、
  forge の `authorize()` が押した本人の権限と、サービスアカウントの範囲の共通部分で判定する（新しい Action `MergeRequestApprove`、
  権限の表に allow と deny の行）。承認の規則（必要な人数、承認者の条件、保護ブランチ）は forge にだけ置き、kanflow は表示と操作だけを持つ。
- 時期: MR は M3 なので、forge の MR と承認が先（M3）、kanflow からの承認はその後（計画 02 の第 1 段）。代理実行は ADR と
  `security-reviewer` のレビューが必須（計画 02 §5.3）。

## Amendment 2026-11-05 — X1: `.forge/repo.yml` の解析と仕様の参照の解決（計画 11 §5.7 の X1、計画 03 §8.1 の (a)）

計画が開けていた選択を、次のとおり決めた（計画の推奨に沿う。推奨のない点は、Claude が診断のしやすさと、閉じた既定を優先して決めた）。

1. **読み取りの API**: `GET /api/v1/projects/{id}/repository/spec-refs[?ref=]`。新しい Action `SpecRefRead`（`read_api`）。
   判定の規則は `RepoBrowse` と同じ（同じ arm。Reporter、内部のプロジェクトはサインインした人全員、サービスアカウントは所属だけ）。
   権限の表の行は `RepoBrowse` の全行の双子（`the_spec_refs_mirror_every_browse_row` が保証する）。`ref` を省くと既定のブランチを読む。
   `gstn browse spec-refs --project <id> [--ref <rev>]` で読める（C-1: 予定のままのルートを残さない）。X4 の `gstn spec` は要件の索引の側を足す。
2. **参照先の権限は、読む人の権限で判定する**（計画 11 の X1）。`repo:` のパスは読む人を viewer として 1 回だけ引き、
   参照先のプロジェクトをもう一度 `authorize()`（forge-auth の `prove_spec_ref`。ルートの証明の principal を使い、呼び出し側が渡す principal は使わない）で判定する。
   参照する側のプロジェクトを読めることは、参照先を読む理由にならない。
3. **存在を漏らさない**: 参照先が無い、プロジェクトではない（グループ・ユーザー）、移動・削除の墓標、`authorize()` の拒否、リポジトリの準備中は、
   すべて同じ形の `unavailable`（`FG-REPO-0026`、`project_id` と `commit` は出さない）。理由はログ（`forge.spec_refs.unresolved` の `reason`）にだけ出す。
   **移動の墓標は追わない**: 版を固定する参照なので、パスの書き換えを求める（暗黙のリダイレクトで別のリポジトリを指さない）。
4. **版の解決**: `version: <tag>` は `refs/tags/<tag>`、`track: <branch>` は `refs/heads/<branch>`（完全な名前だけを引く。ブランチとタグの取り違えをしない）。
   注釈付きのタグはコミットまで剥がす。読める参照先に無い版は `version_not_found`（`FG-REPO-0025`）。git の失敗・時間切れは要求全体の失敗（項目を黙って落とさない）。
5. **ファイルの形式と、解析の閉じた既定**: 置き場所は `.forge/repo.yml` だけ（通常のファイル。シンボリックリンク・ディレクトリ・サブモジュールは `not_a_file`）、64 KiB まで。
   最上位のキーは `specs`（と、ADR-0017 §2.7 が定める `review`。X1 では読まずに受け入れる）だけ。項目のキーは `repo` / `path` / `version` / `track` / `scope`。
   知らないキー（ADR-0023 でなくした `kind:` を含む）、重複したキー、2 つ目の文書、YAML のアンカー・エイリアス・タグは **拒否する**（無視しない）。
   スカラーは書かれたとおりの文字列（`1.10` は数値 1.1 にしない）。`specs:` は 32 項目まで、`scope:` は 64 個まで（要件 ID の型 `[A-Z0-9-]`、末尾に `*` を 1 つまで）。
   拒否は `config: "invalid"` と `problem {error_code: FG-REPO-0024, line, reason}` で返し、**参照は 1 つも解決しない**（部分的な読み取りをしない）。
   CODEOWNERS（ADR-0024 §9）と同じく、内容の問題は 200 の応答のデータで返す（読む人が直す場所を知るため）。
6. **YAML の部品**: `yaml-rust2`（0.13、MIT OR Apache-2.0、純 Rust、既定の機能 `encoding` を外す）の **イベントのパーサーだけ** を使う。
   木は forge-core が明示のスタックで組み立てる（深さ 16 まで）。エイリアスを展開する読み込み（billion laughs）は使わない。
   新しい依存は arraydeque・hashlink 0.12・foldhash 0.2（いずれも許可済みのライセンス）。
7. **置き場所（クレート）**: 解析は `forge-core::repo_config`（純粋、どのクレートからも使える）、ファイルの読み取りは `forge-git` の `Browser::small_file`、
   判定は `forge-auth::prove_spec_ref`、組み立ては `forge-web` のルート。X2（`forge/spec-drift`）が同じ解決をジョブから使うときは、
   組み立てを機能クレートへ移す（そのとき、ボットの主体の読める範囲は ADR-0017 §4.0 のローダーの規則で決める）。
8. **エラーコード**: `FG-REPO-0024 RepoConfigInvalid`（422 / info）、`FG-REPO-0025 SpecVersionNotFound`（404 / info）、
   `FG-REPO-0026 SpecRepositoryUnavailable`（404 / info）。どれも内容の問題で、アラートにしない（Runbook は不要）。
9. **X1 の独立レビュー（2026-11-06、security・rust。CRITICAL / HIGH なし）で直したこと**:
   - 要求全体に 1 つの予算（20 秒、`AppState::spec_refs_budget`）。超えたら `FG-WEB-0002 operation=spec_refs`（項目ごとに worker を占有しない）。
     たどった参照は 1 つずつ、読む人ごとの上限（60 秒に 600、`FG-WEB-0010`）に数える。
   - 参照先のリポジトリでの git の失敗（busy・時間切れ・保管場所）は、その項目の `unavailable`（`FG-REPO-0026`）にする。
     応答の状態コードが存在の手がかりにならないように。本当の失敗は warn のログ（`reason=git_failed`、`git_error_code`）にだけ出す。
   - 版の解決は型の結果（forge-git の `Browser::find_ref` → `RefLookup::{Commit, Missing, NotACommit}`）。理由の文字列を照合しない。
   - 空・null（`~` / `null`）の値はそのフィールドの `*_invalid`。1 つの mapping は 64 キーまで（`too_many_keys`）、重複は読まない部分（`review:`）も含めて 1 回の走査で拒否。
   - `path:` は browse の規則（`RepoPath`）に加え、先頭の `:`（pathspec の magic）、ドライブ文字（`C:`）、バックスラッシュを拒否する（`SpecPath`）。

## Amendment 2026-11-06 — X2: `forge/spec-drift`（計画 11 §5.7 の X2、計画 03 §5.4・§8.1 の (b)）

計画の推奨（`neutral`、決定的）に沿い、開いていた点を次のとおり決めた（推奨のない点は、Claude が存在を漏らさないことと診断のしやすさを優先して決めた）。

1. **いつ評価するか**: MR の版ごと（`merge_request.pushed`。開いたときの最初の版も、再オープンで head が変わったときもこのイベント）。
   購読者 `spec-drift`（`independent`、forge-db `checks::spec_drift`）がジョブ `check.spec_drift`（payload は `project_id` と `head_sha`、
   重複の鍵 `spec_drift:<project>:<head>`、60 秒、5 回）を積み、`forge worker` が forge-checks の `SpecDrift` で評価する。
   同じ head は 1 回だけ（成功したジョブが鍵を持つ）。**参照先に新しい版が出ても、既にある run は評価し直さない**（run は head の時点の記録。
   次の版で評価される。仕様の公開からの逆向きの再評価は M4 の影響の分析（計画 03 §5.2）で扱う）。
2. **読む `.forge/repo.yml`**: MR の **head** のもの（その MR が版を上げていれば、上げた後を評価する）。ADR-0017 §4.1.1 の spec-trace が
   対象ブランチの版を読むのとは目的が違う（spec-drift は「この変更が古い仕様に準拠したままか」を知らせる）。
3. **参照先を辿る権限 — サインインした全員**: run はプロジェクトを読める全員に見える（`CheckRead` = `RepoBrowse`）。そこで参照先は
   **ロールを持たないサインインした人** の権限で辿る: forge-auth の `prove_spec_ref_for_every_person`（`authorize()` の `SpecRefRead` を、
   ロールを落とした `LoadedProject` と、id 0 の有効な人のセッションで判定。テストで「ロールのない人の `SpecRefRead`」と一致を確かめる）。
   実際には内部公開で削除待ちでないプロジェクトだけを辿る。非公開の参照先は、MR の作成者が読めても `unavailable`（`FG-REPO-0026`）で、
   版もタグも出さない（X1 の「読む人の権限で判定」を、読む人が不特定の run に広げた形）。検索は viewer なし（`NamespaceResolver` の同じ文）。
   サービスアカウントは参照する側のプロジェクトの所属で run を読むが、内部公開の参照先は会社の全員が読めるものなので、範囲に入れない。
   ADR-0017 §4.0 の spec-lane のボットの「固定した仕様のプロジェクトを読める」ローダーの規則は、ここでは **使わない**（結果が全員に見えるため）。
4. **最新の版**: 版はタグ（計画 03 §5.1）。forge-core `spec_version`: 接頭辞（英字・`-`・`_`、32 バイトまで）+ 1〜4 個の数（各 9 桁まで）だけを
   版とし、**同じ接頭辞の系列の中で** 数として比べる（足りない部分は 0、`v1.5` = `v1.5.0`）。`v2.0-rc1` などの付き物は版にしない。
   同じ版の 2 つの綴りは文字列の大きい方を最新として名指す（決定的）。固定が版の形でなければ `unversioned`（比べない）。`track:` は `tracking`。
   ref の一覧が上限（`BrowseLimits::max_refs`）で切れたら `undecided`（最新を言わない）。参照先の git の失敗は X1 と同じくその項目の答えにする
   （タグの一覧は `undecided` と warn のログ `forge.spec_drift.tags_unread`）。
5. **結論**: `success` — `.forge/repo.yml` が無い、またはすべての項目が `current` / `tracking` / `unversioned` / `local`。
   `neutral` — `behind`（新しいコード **`FG-REPO-0027` SpecVersionBehind**、info、アラートなし、Runbook 不要）、`version_not_found`（FG-REPO-0025）、
   `unavailable`（FG-REPO-0026）、`undecided`、またはファイルを使えない（FG-REPO-0024、参照は 1 つも辿らない）。`failure` にはしない（知らせるだけ）。
   要約は行ごとの状態（行番号・書かれたとおりのパスと固定・状態・コード）、注釈は `.forge/repo.yml` の該当行（`behind` と `version_not_found` は warning、
   他は notice）。要約は 64 KiB、注釈の文は 4 KiB で切り、末尾に `…` を付ける（拒否しない）。
6. **失敗を黙らせない**: 評価の失敗（head を読めない git、DB、プロジェクトの検索の時間切れ）は何も書かずにジョブへ返し、git / DB / 準備中は
   キューの間隔でやり直す。最後の試み、またはやり直しても無駄な失敗では、`error` の run（要約に FG コードと request id）を書いてからジョブを
   死なせる（`FG-DB-0006` は従来どおりジョブの runner が出す）。`error` も書けなければ `forge.spec_drift.error_unrecorded`（error）を出す。
   プロジェクトが消えていれば何も書かずに終える（`forge.spec_drift.project_gone`、info）。
7. **forge 自身の run の書き込み**: forge の worker はアカウントではないので、`forge/` の run は **報告した主体なし** で保存する
   （追記のマイグレーション 20261124100000: `check_runs.reported_by` を NULL 可にし、CHECK `check_runs_reporter` で
   「`forge/` ⇔ `reported_by` が NULL（`credential_id` も NULL）」を固定）。forge-db `record_forge_check_run` は
   `forge_core::checks::FORGE_REPORTED_CHECKS`（今は `forge/spec-drift` だけ）の名前しか受けず、報告と同じロックの順・上限・
   `check.completed`（結論が変わったときだけ、actor は system）で書く。呼べるのは forge-checks だけ（`tests/decided_callers.rs` に足した）。
   ADR-0017 の `InternalCheckWrite` と `Principal::system` は **作らない**（判定する主体がいない書き込みで、HTTP の `CheckWrite` は従来どおり
   `forge/` を全員に拒否する。Action を増やすと INTERNAL のゲートの整備が要る — 必要になるのは主体を持つボット（spec-lane）が書くとき）。
   API の `reported_by` は `forge/` の run で `null`（docs/api/checks.md、OpenAPI、`gstn checks list` は `forge` と表示）。
8. **報告できない必須のチェック**: `forge/spec-drift` は forge が報告するので、マージの規則の `unreportable_checks` と doctor の
   `checks.reporters` に入れない。他の `forge/` の名前（`forge/risk` など）は従来どおり入る。必須にすると `neutral` は満たさない（ADR-0024 §10）。
9. **置き場所（クレート）**: X1 の組み立て（`.forge/repo.yml` の読み取りと、参照の辿り方）を forge-web のルートから
   **forge-checks の `spec_refs`** へ移し、ルートと `SpecDrift` が共有する（`Reader::Proof` = 読む人、`Reader::EveryPerson` = 全員）。
   新しいクレートは作らない（run を書くのが forge-checks で、機能クレート同士の辺を増やさない）。ログの `forge.spec_refs.unresolved` は
   target が `forge_checks` になり、`reader`（`caller` / `every_person`）が増えた。ルートの振る舞いは変わらない（X1 のルートのテストがそのまま通る）。
10. **X2 の独立レビュー（2026-11-06、security・rust。HIGH 2 件）で直したこと**:
    - **答えのない head を残さない（rust HIGH）**: 評価全体に内部の締め切り（`EVALUATION_BUDGET` 30 秒。超えたら `FG-WEB-0002 operation=spec_drift`、
      やり直せる）と、書き込みの締め切り（`WRITE_TIMEOUT` 10 秒、`error` の書き込みにも）を置いた。30 + 10 + 10 < ジョブの 60 秒なので、
      最後の試みはジョブの runner の時間切れより前に handler 自身が `error` を書ける。それでも答えられずに死んだジョブ（worker の落ち、
      リースの期限切れでの reaper）は、`forge worker` の新しいタスク `spec_drift_sweep`（60 秒ごと、50 件まで）が、7 日以内に死んで
      head に run が無い `check.spec_drift` を探し（forge-db `unanswered_dead_jobs`、部分索引 `jobs_dead_kind`、マイグレーション 20261124100100）、
      ジョブの最後のコード（reaper なら FG-DB-0007）と request id で `error` を書く。
    - **他の報告者が forge の run を締め出せない（security HIGH）**: 1 コミット 100 名の上限（FG-REVIEW-0021）は報告者（`ci/`・`kanflow/`・`ext/`）の
      名前だけを数え、forge が書く名前（`FORGE_REPORTED_CHECKS`）は数えず、上限にも当たらない。読み取りの上限は
      `MAX_STORED_CHECK_RUNS_PER_COMMIT`（100 + forge の名前の数 = 101）。CI が 100 件書いても `forge/spec-drift` とその `error` は書ける。
    - **参照先のタグを保存しない（security MEDIUM、簡単な方を選んだ）**: 保存する要約と注釈には、参照先の **タグの名前と新しい版の数を入れない**
      （`behind: a later version is published` とパスと固定だけ）。run はサービスアカウント（内部公開のプロジェクトを所属なしでは読めない）にも
      Checks API で見えるため。最新の版は、読める人が `gstn browse refs --project <参照先> --type tag` / `gstn browse spec-refs` で自分の権限で見る。
      読み取り時に描き直す案は、run の保存の形と API を変えるので採らなかった。そのため「後から非公開にされた参照先のタグが古い run に残る」問題（LOW）も起きない
      （残るのは「behind だった」という 1 ビットとパスだけで、パスは参照する側のファイル自身の文字）。
    - **LOW**: 参照先のタグの一覧の git の失敗は、その項目の `undecided` にせず、run の失敗として返す（busy・時間切れはやり直し、最後は `error`）。
      `undecided` は一覧が上限で切れたときだけ。版の形でない固定（`stable`、`v1.2.3.4.5`、`v2.0-rc1`）は `success` のまま、notice の注釈で知らせる。

## Amendment 2026-11-06 — X7: `forge/risk`（計画 11 §5.7 の X7、計画 04 M-6 の層 1）

計画の推奨（決定的な規則を MR の差分に当て、forge 自身の check run として残す。`gstn risk check` は任意）に沿い、開いていた点を次のとおり決めた。

1. **範囲**: M-6 の **層 1（決定的な規則）** と、その結果の `forge/risk` run だけ。含めないもの: ローカル AI の層（層 2。LLM ゲートウェイと並行）、`.forge/risk.yml`
   （プロジェクトが規則を足す・既定の規則を Owner の承認で外す）、解除（Maintainer の承認と警告の ID。M4 の自動マージと同時）、個人情報の列名の登録、
   検出率・誤検知率の画面と評価セット。いずれも規則表（下の 5）に足す形で後から入れられる。**`conditional` / `full` は層 2 が動くまで実効にならない**（M-5）ので、
   この MR は自動マージを有効にしない。
2. **いつ評価するか**: MR の版ごと（`merge_request.pushed`。開いたときの最初の版も同じ）。購読者 `risk`（`independent`、forge-db `checks::risk`）が
   ジョブ `check.risk`（payload は `project_id` と `head_sha`、重複の鍵 `risk:<project>:<head>`、90 秒、5 回）を積み、`forge worker` が forge-checks の `Risk` で評価する。
   同じ head は 1 回だけ（成功したジョブが鍵を持つ）。run は head の時点の記録で、target が進んでも評価し直さない（M4 の自動マージは、マージの直前に
   target の先頭に対する判定を使う。ここでは作らない）。
3. **何と比べるか**: head を版に持つ **開いている MR の target ブランチごと**（ジョブの実行時に forge-db `risk_targets` が読む。重複なし、名前順、`MAX_TARGETS` = 4。
   超えたら `targets_not_assessed`）。各 target の先頭と head の **merge base から head まで** の差分を `git diff-tree -M -r -z --raw --numstat` で取る
   （forge-git `risk_inputs`。引数ベクトル、リビジョンは object id だけ、20 秒、出力の上限）。**改名は 1 件**にまとめ（削除に数えない。計画 04 M-6）、編集を伴う改名も
   類似度（git 既定の 50 %）で対にする。MR が開いていない head、target が無い、履歴が共通でない、パスが UTF-8 でない、は **`success` にしない**
   （`neutral`、`FG-REVIEW-0025`、規則 `no_open_merge_request` / `target_missing` / `no_common_history` / `non_utf8_path`）。結果は複数の target のうち **最悪**。
4. **結論**: `ok` → `success`（要約に「何と比べたか・何ファイル見たか」）、`warn` → `neutral`（`FG-REVIEW-0024`）、`stop` → **`failure`**（`FG-REVIEW-0023`）。
   `failure` は、保護ブランチが `forge/risk` を必須にしたとき条件を満たさない（`neutral` も満たさない。ADR-0024 §10）。必須にしない限り手動のマージは止めない
   （計画 04 M-5: 止めるのは自動マージ）。M4 の自動マージは `failure` の head を **どの方式でも** マージしない（このコミットでは契約として記録するだけで、
   自動マージ自体は M4）。run の要約には、規則の id・パス・行・固定の理由・コードを並べ、**一致した文字列は決して書かない**（秘密や表名を、プロジェクトの読み手全員が見る
   run に写さない）。注釈は最悪のものから 50 件まで（`failure` / `warning`）、要約は 64 KiB で切る（拒否しない）。注釈に載せられないパス（バックスラッシュ・制御文字など）は要約だけに出す。
5. **規則（forge-core `risk::RULES`。純粋関数、AI で下げられない）**:
   - `stop`: `applied_migration_changed`（`migrations` / `migrate` の下の `.sql` の変更・削除・型の変更・改名）、`sql_drop`（`DROP TABLE|SCHEMA|DATABASE`、`ALTER TABLE … DROP [COLUMN]`）、
     `sql_truncate`、`sql_unbounded_write`（`WHERE` のない `DELETE FROM` / `UPDATE`、`WHERE TRUE` / `WHERE 1 = 1` も同じ）、`sql_column_change`（列の型の変更、`RENAME`）、
     `sql_drop_constraint`、`sql_disable_guard`（`DISABLE TRIGGER` / `ROW LEVEL SECURITY`）、`deploy_rollback`（CI・デプロイのファイルの `migrate revert` / `db:rollback` / `dropdb` / `DROP DATABASE`）、
     `secret_file`（`.env`（`.example` 等を除く）、`id_rsa` など、`.htpasswd`、`.netrc`、`.p12` / `.pfx` / `.jks`。削除は除く）、`secret_in_content`（X8 の `forge_git::secret_scan` と同じ規則。
     `forge:allow-secret` のあるファイルは除く）、`data_lifecycle_job`（ファイル名に `backup` / `restore` / `retention`。スクリプト・ジョブ・設定のファイル、または運用のディレクトリの非ソースのファイルだけ。アプリケーションのソース（`events_retention.rs` など）・文書・テストは除く。X7 レビューで狭めた）、
     `mass_deletion_lines`（削除行 500 以上かつ target の text 行の 30 % 以上）、`mass_deletion_files`（削除ファイル 20 以上かつ 30 % 以上）、`too_many_changes`（5000 ファイル超）、`critical_content_unread`（新しい migration か CI・デプロイのファイルを読めなかった。X7 レビュー）。
   - `warn`: `new_migration`、`sql_drop_object`、`sql_privilege`、`auth_change`（パスの語が auth / permission / acl / rbac / oidc / ldap / codeowners など。文書・テストは除く）、
     `ci_deploy_config`、`dependency_change`、`special_entry`（シンボリックリンク・サブモジュール）、`content_not_scanned`（読めなかったファイルがある）。
   - 大量削除は計画の仮置きの数字のとおり（行は numstat の削除行の合計で、編集内の削除も数える。バイナリは行でなくファイル数。生成物・ロックファイルも例外にしない）。
     割合の分母は **merge base の tree**（`diff-tree --numstat` を空の木に当てて数える。20 万ファイルで打ち切り、`truncated` のときは分母が小さい側に誤る = `stop` に倒れる）。
     分母は絶対値の閾値（500 行 / 20 ファイル）に届いたときだけ数える。
6. **読む量の上限**: 本文を読むのは新しい migration と CI・デプロイのファイル（SQL・ロールバックの検査）と、行が増えたテキストのファイル（秘密の検査）。
   優先はこの順で **60 ファイルまで、1 ファイル 256 KiB まで**。読めなかった数は `content_not_scanned`（`neutral`）に出す（黙って通さない。push の時点では X8 が同じ規則で拒否している）。
   SQL は注釈・文字列を除いて文に分け（`;` と `$$` で区切る）、文ごとの先頭の語と並びで見る（`WHERE` の有無は文のどこかの `WHERE` で判定。副問い合わせの `WHERE` で通る限界がある）。
   評価全体は 45 秒（超えたら `FG-WEB-0002 operation=risk`、やり直す）、書き込みは 10 秒。
7. **失敗を黙らせない**: X2 と同じ構造。評価の失敗は何も書かずジョブへ返し（git / DB / 準備中はやり直し）、最後の試みで `error` の run（要約に FG コードと request id）を書いてからジョブを死なせる。
   handler が答えられずに死んだジョブ（worker の落ち、reaper）は、worker の新しいタスク `risk_sweep`（60 秒ごと、50 件まで。forge-db `checks::risk::unanswered_dead_jobs`、
   既存の部分索引 `jobs_dead_kind` は kind を問わないので **マイグレーションは足さない**）が `error` で答える。プロジェクトが消えていれば何も書かない（`forge.risk.project_gone`）。
8. **forge 自身の run の扱い（X2 の 7・8 を改める）**: `FORGE_REPORTED_CHECKS` は `forge/spec-drift` と `forge/risk` の 2 つ。
   1 コミットの読み取りの上限 `MAX_STORED_CHECK_RUNS_PER_COMMIT` は 100 + 2 = 102。報告者の 100 件の上限には、forge の名前は数えない・当たらない。
   `forge/risk` は forge が報告するので、マージの規則の `unreportable_checks` と doctor の `checks.reporters` に **入れない**（X2 の 8 が「入る」とした例を `forge/risk` から別の `forge/` の名前に改める）。
   HTTP の `CheckWrite` は従来どおり `forge/` を全員に拒否する。`record_forge_check_run` の呼び出しは forge-checks だけ（`tests/decided_callers.rs`）。
9. **エラーコード**: `FG-REVIEW-0023` RiskStop、`FG-REVIEW-0024` RiskWarn、`FG-REVIEW-0025` RiskNotAssessed（いずれも info、HTTP には出ない通知。run の要約・注釈が名指す。
   アラートなしなので Runbook は不要）。run が書けない・git が失敗する、は既存のコード（`FG-GIT-*`、`FG-DB-*`、`FG-REPO-0016`、`FG-WEB-0002`）。
10. **置き場所**: 規則は forge-core `risk`（純粋。`gstn` と同じ実装を共有できる形）、git の入力は forge-git `risk_inputs`、job・購読者・target の問い合わせ・sweep の問い合わせは
    forge-db `checks::risk`、評価と run の組み立ては forge-checks `risk`、worker は forge-cli の `risk` と `risk_sweep`。新しいクレートは作らない。
    **新しい HTTP のルートは作らない**（読み取りは既存の `GET /api/v1/projects/{id}/commits/{sha}/checks`、認可は `CheckRead` = `RepoBrowse` のまま。したがって認可の matrix の行は増えない）。
    **新しい EventType は作らない**（`check.completed` が結論の変化で出る）。
11. **`gstn risk check` は作らない（任意の項目）**: `gstn`（forge-client）は forge-core に依存しない（forge-errors だけ）ので、同じ規則を共有するには依存の向きを変える必要がある。
    push の前の手元の確認は、X8 の pre-receive（秘密・大きなファイル）と、MR を開いた直後の `gstn checks list --project <id> --sha <head>`（`forge/risk` の run）で足りる。
    手元の差分を検査するコマンドが要る証拠が出たら、規則の crate を `gstn` から使える形に切り出して足す（forge-core の `risk` は I/O を持たないので移せる）。
12. **既知の限界（評価のケースとして足す）**: 個人情報の列をログ・API・読み取り用ロールに出す変更、新しい外部の送り先、ルートの認証・認可の検査の削除、公開範囲を広げる変更は、
    文字列の規則では確実に拾えない。層 2（ローカル AI。`warn` / `stop` に上げることだけができる）と `.forge/risk.yml`（列名の登録）に残す。
    本文を読まない `auth_change` 等はパスの語だけで判定するので誤検知する（`warn` なので進む）。`data_lifecycle_job` は `stop` だが、アプリケーションのソース（GitStation 自身の `events_retention.rs` など）には当てない（X7 レビュー）。

### X7 の独立レビュー（2026-11-06、security・rust。BLOCK）で直したこと

- **バイナリ判定で素通りしない（security HIGH）**: git が `-`（NUL バイトが 1 つあるだけで付く）と出したファイルも本文の候補にし、SQL・デプロイ・秘密の検査はバイト列に対して走る
  （forge-git `secret_scan::scan_bytes`。push の検査 X8 の `scan` は従来どおりバイナリを読まない）。numstat の行が raw の行と対応しない場合は
  `GitProcessFailed`（`reason=numstat_unmatched`）の失敗にする（黙って落とさない）。読めなかった（大きい・上限の外・存在しない・通常のファイルでない）新しい migration と
  CI・デプロイのファイルは `stop`（`critical_content_unread`）。ほかのファイルは数えて `content_not_scanned`（`neutral`）。migration とデプロイのファイルを先に読む。
- **同じ head の再評価（rust HIGH）**: 重複の鍵は `risk:<project>:<head>:mr<MR の id>`（版）と `risk:<project>:<head>:mr<id>:e<event seq>`（付け替え・再オープン）。
  購読者 `risk` は `merge_request.pushed` に加え、target を変えた `merge_request.updated` と `merge_request.reopened` でも、その MR の今の head のジョブを積む
  （head がまだ無ければ最初の版が積む）。別の MR が同じ head を別の target に持っても、先のジョブに潰されない。run は head ごとに 1 つで、評価のたびに
  その時点の開いている MR のすべての target で描き直す。
- **SQL（MEDIUM）**: 文頭の制御構文（`BEGIN` / `IF … THEN` / `ELSE` / `LOOP`）を読み飛ばし、CTE の中の `DELETE FROM` / `UPDATE`（括弧の中の始まり）も `WHERE` を見る
  （`FOR UPDATE`、`ON UPDATE CASCADE`、`DO UPDATE SET` は命令ではない）。
- **掃除（MEDIUM）**: `risk_sweep` は死んだジョブを id の順に、カーソルで 50 件ずつ歩き、終わったら先頭に戻る（答えを書けないジョブが後ろを止めない）。
  論理削除されたプロジェクトのジョブは一覧に出さない（run を書けない）。request id が読めないときは新しい id で答え、warn `forge.worker.risk_sweep_request_id_replaced` を出す。
- **既知の限界（security LOW）**: 評価は head の版・付け替え・再オープンの時点の target の先頭に対して行い、**target が進んだだけでは評価し直さない**。
  merge base が変われば差分も変わるので、`failure` / `success` は「その時点」の判定である。**M4 の自動マージは、マージの直前にその target の先頭に対して
  もう一度評価する（または評価の時点の target の先頭が今と同じことを確かめる）こと**。この MR では自動マージを作らない。

## Amendment 2026-11-06 — X4: 要件の索引・spec-lint・`@req` の索引・spec-trace・仕様の API（計画 11 §5.7 の X4、計画 03 §4・§5.4・§7.2）

計画の推奨（spec-trace は助言から、`PBI-123/AC-2` も対象）に沿い、開いていた点を次のとおり決めた（推奨のない点は、Claude が決定的であることと、
読む人に読めないものを出さないこと、診断のしやすさを優先して決めた）。

1. **要件ブロックの形式**（forge-core `spec_doc`）: 先頭の `---` で始まる YAML の前付け（`spec`・`title`・`status` が必須、`owners` は任意。ほかのキーは拒否。
   X1 の `repo_config` と同じ木の組み立てで、アンカー・エイリアス・タグ・重複したキーは拒否）と、`::: req <ID> [priority=…] [status=…]` 〜 `:::` のブロック。
   要件 ID は `-` で区切った 2 つ以上の大文字英数字の部分（先頭は英字、64 バイトまで）。**`PBI` で始まる系列は要件 ID にしない**（kanflow の作業項目と取り違えないため）。
   `priority` は `must` / `should` / `could` / `wont`、`status` は `active` / `deprecated`（ID は消さず廃止にする、計画 03 §4.2）。ほかの `::: note` などの入れ物は入れ子にでき、
   要件ではない。**フェンスのコードブロックの中は読まない**（仕様の例に書式を載せられるように）。解析は止まらない: 壊れたブロックがあっても次のブロックを読み、
   規則の違反を行と規則の名前で残す（1 文書 200 件まで、要件は 2000 件まで、本文は 16 KiB まで）。前付けも要件ブロックも無い Markdown（README）は仕様の文書ではない。
2. **自分の仕様の置き場所**: `.forge/repo.yml` の `path:` の項目。無ければ（ファイルが無い・拒否された・`repo:` だけ）既定の `spec/`（計画 03 §4.4）。
   その下の `*.md` / `*.markdown`（通常のファイルだけ）を読む。
3. **`@req` の形式**（forge-core `req_tags`）: 語 `@req`（直前が英数字・`_`・`.`・`@`・`/` でなく、直後が空白）のあとに ID を 1 つ、`,` で区切って続けてよい。
   ID は要件 ID か、kanflow の受入基準 `PBI-<番号>/AC-<番号>`（番号は先頭に 0 を付けない、AC は 1〜9999）。文末の `.`・`)`・`` ` `` などは ID に含めない。
   それ以外が続くタグは「形の崩れたタグ」として行を残す。言語を問わず、コメントでも名前でもよい（計画 03 §4.3）。
4. **索引は保存しない**（追記のマイグレーションは無し）: 索引はコミットごとに git から作る（forge-checks `spec_index::index_commit`: `ls-tree -r`、
   `cat-file --batch`、`git grep -F @req`）。決定的で、古くならず、後から埋める処理も要らない。**上限**: Markdown 1000 ファイル、合計 8 MiB、1 ファイル 256 KiB
   （超えたファイルは読まずに `file_too_large` の違反）、`git grep` の出力 8 MiB と 20 秒。上限に当たれば `incomplete`（`spec_files` / `spec_bytes` / `tag_lines`、
   新しいコード **`FG-REPO-0031` SpecIndexIncomplete**）として必ず答えに出し、黙って通さない。リポジトリをまたぐ一意性の検出と、全リポジトリの要件の検索
   （MCP の `search_requirements`、計画 03 §6）には保存した索引が要るが、それは文脈 API の X5（または M4 の影響の分析）で、使う側と一緒に決める。
5. **`forge/spec-lint`**（決定的）: 文書の規則、同じ ID の二重の定義（`duplicate_id`、最初の場所を付ける）、同じ `spec:` の名前（`duplicate_spec`）、
   **壊れた参照** `broken_reference` = このリポジトリ自身の系列（ID の最後の部分を除いたもの、`ZEVA-CFG`）の ID を指すのに、定義が無く、`repo:` の `scope:` にも当たらないタグ。
   ほかの系列のタグはこのリポジトリが判断するものではない（spec-trace が `unknown` と言う）。結論: 違反なし `success`、違反あり **`failure`**
   （新しいコード **`FG-REPO-0028` SpecLintProblem**、要約と failure の注釈に `パス:行 規則`）、違反は無いが索引が上限 `neutral`。必須にするかは保護ブランチの規則で決める。
6. **`forge/spec-trace`**（決定的、**助言**、計画 03 §9-4 と 06 の D-7）: MR が触れる対象 =（a）head と対象ブランチの先端の **マージベース** から、自分の仕様で追加・変更された要件
   （廃止したものは除く。マージベースが無ければ head のすべてを新しいものとする）∪（b）追加された `@req`（(パス, ID) の組で比べる。行が動いただけでは追加ではない）
   ∪（c）MR に結びついた作業項目 `PBI-<番号>`。それぞれ head の木に `@req` があれば `tested`（件数）、無ければ `untested`（新しいコード **`FG-REPO-0029` RequirementUntested**）、
   (b) の ID が自分の仕様にも `repo:` の `scope:` にも無ければ `unknown`（**`FG-REPO-0030` RequirementTagUnknown**、形の崩れたタグも同じコード）。
   作業項目は、その受入基準 `PBI-<番号>/AC-*` のどれかのテストがあれば `tested`。結論は `success` か `neutral` で、**`failure` にはしない**。
   **M3 はテストの有無だけ** を見る。ADR-0017 §4.1.1 の「マージ結果の tree と JUnit の成功」は、パイプラインがある M4 で足す（そのとき必須化も計測を見て決める）。
7. **run に出すもの**: このリポジトリ自身の文字だけ（自分の仕様の ID とパス、自分のタグ、MR 自身の作業項目）。run はプロジェクトを読める全員とサービスアカウントに見えるので、
   参照先の仕様は **読まない**（X2 の「保存した run に参照先の中身を残さない」と同じ考え）。参照先の要件まで含めた表は、仕様の API の trace で読む人の権限で見る。
8. **いつ評価するか**: 購読者 `spec-index`（`independent`、forge-db `checks::spec_index`）が、`merge_request.pushed` ごとにジョブ `check.spec_index`
   （payload は `project_id`・`iid`・`head_sha`、重複の鍵 `spec_index:<project>:<iid>:<head>`、120 秒、5 回）を積む。spec-trace は作業項目の結びつきを見るので、
   `work_item.linked` / `unlinked` でもその MR の **今の head** をもう一度評価する（鍵に `:seq:<seq>` を足し、イベントごとに 1 回）。版がまだ無い MR は飛ばす
   （最初の版が、その時点の結びつきで評価される）。1 つのジョブが 2 つの run を書く。同じ head を持つ 2 つの MR は、後から書いた方の run が残る（run はコミットのもの）。
9. **失敗を黙らせない**（X2 と同じ形）: 評価の締め切り 70 秒、書き込みは 1 件 10 秒（70 + 10 + 10 に `error` の 2 件を足しても 110 < ジョブの 120 秒。下の 15 で 80 秒から改めた）。git / DB / 準備中 / 時間切れはやり直し、
   最後の試みでは両方の run を `error`（要約に FG コードと request id）にしてからジョブを死なせる（`SpecChecks::fail`）。答えずに死んだジョブ（worker の落ち、reaper）は、
   `forge worker` の新しいタスク `spec_index_sweep`（60 秒ごと、50 件まで）が、7 日以内に死んで head に 2 つの run が揃っていない `check.spec_index` を探して `error` を書く
   （X2 の部分索引 `jobs_dead_kind` をそのまま使う）。`error` も書けなければ `forge.spec_index.error_unrecorded`（error）を出す。プロジェクトか MR が消えていれば何も書かない
   （`forge.spec_index.gone`、info）。
10. **forge 自身の run**: `FORGE_REPORTED_CHECKS` に `forge/spec-lint` と `forge/spec-trace` を足した。X2 のマイグレーションの CHECK（`forge/` ⇔ 報告者なし）と、
    報告者の上限に数えない規則はそのまま効く（1 コミットの読み取りの上限は、先に入った X7 の `forge/risk` と合わせて 100 + 4 = 104 件）。マージの規則の `unreportable_checks` と doctor の `checks.reporters` にも入らない。
    書けるのは forge-checks だけ（`tests/decided_callers.rs` のまま）。
11. **仕様の API**（計画 03 §7.2）: パスは計画の `/api/v1/specs/{repo}/…` ではなく、ほかの API と同じプロジェクトの id の形 `GET /api/v1/projects/{id}/specs/{requirements,versions,diff,trace}`
    （ADR-0015。リポジトリのパスはプロジェクトの id で引く）。版は計画の `version=` ではなく browse API と同じ `ref`（タグの名前を渡せば版）。新しい Action **`SpecRead`**
    （`read_api`、`RepoBrowse` と同じ arm。権限の表の行は `RepoBrowse` の全行の双子で、`the_spec_api_mirrors_every_browse_row` が保証する）。
    - `requirements?ref=&q=&limit=`: 自分の仕様の文書・要件（ID と本文の大文字小文字を区別しない検索、既定 100、最大 500）・文書の違反（200 件まで。壊れた参照はタグが要るので spec-lint と trace で）。
    - `versions`: `forge_core::spec_version` で版の形のタグだけ、系列ごとに新しい順（200 件まで）。公開日と変更の要約は M4 の影響の分析で。
    - `diff?from=&to=`: 要件ごとの追加・変更（本文か priority）・削除・廃止。別のファイルへ動いただけでは変更ではない。
    - `trace?ref=`: 自分の要件と、`repo:` の項目ごとに **読む人の権限で辿った** 参照先（`prove_spec_ref` は `SpecRead` の証明も受け付け、参照先を `SpecRead` で判定する）の
      `scope:` の中の要件、それぞれを指すこのリポジトリの `@req` のテスト（件数と、最初の 20 か所）。読めない参照先は `unavailable`（`FG-REPO-0026`）で要件を出さない。
      要件の状態（計画 03 §5.5 の 5 段階）は M4。
    - 1 人 1 分に 120 回（`FG-WEB-0010`、辿った参照は X1 の上限にも数える）、1 回 30 秒（`FG-WEB-0002 operation=spec_index`）。
12. **`gstn spec`**: `requirements`・`versions`・`diff`・`trace`（C-1: 予定のままのルートを残さない）。
13. **置き場所（クレート）**: 解析・lint・差分・trace は純粋な forge-core（`spec_doc`・`req_tags`・`spec_index`）、git の読み取りは forge-git の browse API
    （`blobs_under`・`read_blobs`・`grep_fixed`・`merge_base`）、索引の組み立てと 2 つのチェックは forge-checks（`spec_index`・`spec_checks`）、ジョブと購読者は forge-db
    （`checks::spec_index`）、ルートは forge-web（`routes/specs.rs`）。新しいクレートは作らない。
14. **見つけた既存の問題（この MR では直さない）**: `gstn` の URL の組み立ては空白を `+` にするが、サーバーの query の復号は `+` を空白に戻さない
    （`gstn browse commits --path "a b"` も同じ）。`gstn spec requirements --search` に空白を含めると一致しない。別の MR で直す。
15. **X4 の独立レビュー（2026-11-06。HIGH 2 件）で直したこと**:
    - **対象ブランチの変更で評価し直す（HIGH）**: `merge_request.updated` の `target_branch` があれば（マージベースが動く）、今の head をもう一度評価する。
    - **仕様の文書の中の `@req` はテストではない（HIGH）**: 自分の仕様のディレクトリの下の Markdown の `@req`（説明の文やコードの例）は索引に入れない。
      偽の `tested` と偽の `broken_reference` が出ないように。
    - **重複の鍵は状態ごと（MEDIUM）**: 鍵を `spec_index:<project>:<iid>:<head>:<state>` にした（`state` = 対象ブランチと開いている結びつきの作業項目の id の md5）。
      同じ head でも結びつきや対象が変われば評価し直し、同じ状態なら 1 回。**まとめる**: 同じ (プロジェクト, MR, head) のジョブが待っている（`queued`）間は積まない
      （ジョブは動くときに結びつきと対象を読む）。結びつきを大量に変えても評価は 1 回。8 の `:seq:<seq>` の鍵はこれに置き換えた。
    - **締め切り（MEDIUM）**: 9 のとおり 70 秒に。最後の試みが `error` を書き終えるまでジョブの時間内に収まる（テストで固定）。
    - **フェンス（MEDIUM）**: バッククォートのフェンスの情報文字列にバッククォートがあればフェンスではない（CommonMark）。閉じないフェンスは `unclosed_fence` の違反にし、
      その中に要件ブロックがあれば前付けの無いファイルも仕様の文書として扱う（後ろのブロックを黙って落とさない）。前付けの前の UTF-8 の BOM は読み飛ばす。
    - **仕様の API の上限は仕事の量で数える（MEDIUM, security）**: 1 人 1 分に索引 120 回分（`requirements`・`versions` は 1、`diff` は 2、`trace` は 1 + 辿る参照の数）、
      同時に 2 本まで（超えると `FG-WEB-0010 reason=in_flight`）。
    - **LOW**: 索引が上限に達していて違反が `broken_reference` だけなら spec-lint は `neutral`（定義を読み落としたかもしれないため）。形の崩れたタグは 80 バイトで切る。
      `cat-file --batch` の大きさの足し算はあふれを確かめる。
    - **採らなかったもの**: 「`follow` の resolver の失敗・時間切れを `unavailable` にしてログ」は採らない。X1 の決定（引けない失敗は要求の失敗で、項目を黙って落とさない）と、
      X2 の spec-drift が DB の一時的な失敗を `neutral` の run として残してしまう（やり直しが効かない）ことに反するため。git の失敗は従来どおり `unavailable`。

## Amendment 2026-11-06 — X5: 文脈 API・`gstn context`・MCP サーバー（読み取り）・D-5 の機密の境界（計画 11 §5.7 の X5、計画 03 §6・§10、計画 06 D-5、計画 02 §12）

計画が決めていない点を次のとおり決めた（X4 と同じく、計画のパスよりほかのルートの決まりに合わせ、読む人に読めないものを出さないことと、診断のしやすさを優先した）。

1. **ルート**（計画 03 §6 の `/api/v1/context/…` ではなく、プロジェクトの id の形。ADR-0015、X4 の 11 と同じ）:
   - `GET /api/v1/projects/{id}/context/work-item?ref=PBI-<n>` — 新しい Action **`ContextRead`**（`read_api`、`RepoBrowse` と同じ arm。権限の表の行は `RepoBrowse` の全行の双子で、
     `the_context_api_mirrors_every_browse_row` が保証する）。作業項目はそのプロジェクトの結びつき（1 プロジェクトに 1 つ）の `(integration, kanflow のプロジェクト, PBI-<n>)`。
     計画の `provider=kanflow&project_id=…` は結びつきから決まるので取らない。結びついていないプロジェクトは `FG-INT-0018`（`reason=project_not_bound`）。
   - `GET /api/v1/projects/{id}/context/requirement?id=<要件 ID>[&ref=]` — `ContextRead`。版は計画の `version=` ではなく X4 と同じ `ref`。自分の仕様に無い ID は
     新しいコード **`FG-REPO-0032` RequirementNotFound**（404）。
   - `GET /api/v1/projects/{id}/merge-requests/{iid}/context` — 既存の **`MergeRequestRead`**（MR の loader と枝の規則をそのまま使う。X6 の `…/conflicts` と同じ扱い）。
2. **答えの中身**（forge が持っているものだけ。kanflow には触れない）:
   - 作業項目: 結びつき・forge が初めて見た時刻、**そのプロジェクトの** MR のうち開いた結びつきのあるもの（新しい順に 50 件、`truncated`）、既定のブランチの
     `@req PBI-<n>/AC-<m>`（50 件と件数）。ユーザーストーリーと受入基準の本文は kanflow にあり forge は持たないので返さない（kanflow 側の連携で足す）。
   - 要件: 本文・priority・廃止・文書（spec・title・status・`confidential`）、それを指す `@req`（50 件と件数）、**新しい版のタグ 5 つ** での状態
     （`same` / `changed` / `absent`、それぞれの場所。指定の版と同じコミットのタグは索引し直さない）。結びつく MR・デプロイ・要件の 5 段階の状態は M4（X4 の 11 と同じ）。
   - MR: 題・説明（4 KiB まで）・状態・枝・最新の版のマージベース、`マージベース..head` の変更ファイル（300 件）、自分の仕様の要件の差分（100 件、追加・変更は head の本文つき）、
     開いた結びつきの作業項目、head のチェックの run（要約は 4 KiB まで）。差分の本文（パッチ）は返さない（`gstn mr diff` で読む）。
   - **すべての項目に `source`**（`project_id`・`project` の今のパス・`version`（読んだ枝やタグ、コミットを指定したときはその id）・`commit`・`path`・`line`、無いものは `null`）。
3. **ほかのリポジトリを読まない**: 3 つのルートは、ルートのプロジェクトだけを読む（作業項目の MR も、そのプロジェクトのものだけ）。ほかのプロジェクトの MR の件数も出さないので、
   読めないリポジトリがあるかどうかは答えから分からない（存在の手がかりを出さない）。ほかのプロジェクトは、そのプロジェクトの id で別に聞く。
4. **上限**: X4 の仕様の API と同じ予算（30 秒、`FG-WEB-0002 operation=spec_index`）、1 人 1 分に索引 120 回分の上限（同じカウンタ）と同時 2 本（`FG-WEB-0010`）。
   数え方は仕事の量: 作業項目は 1、要件は 1 + 索引し直す版の数（最大 5）、MR は 2（マージベースと head）。DB の読み取りは `lookup_timeout` と read pool の `statement_timeout` の内側。
5. **D-5（AI に渡さないもの）**: **エージェントの相手**（`Principal::is_agent_audience`）= `is_agent` で発行したトークン（ADR-0017 §4.0）、または人でないアカウント
   （サービスアカウント・bot。`users.kind`、ADR-0024 §8.4）。人のセッションと、人が `is_agent` なしで発行したトークンはエージェントではない。加えて、リクエストのヘッダー
   **`Forge-Context-Audience: agent`** はどの呼び出しでも答えをエージェント向けにする（狭めるだけで広げない。`agent` 以外の値は `FG-WEB-0006`）。`gstn mcp` は必ず送る。
   - **機密の印**: 仕様は文書の前付けの **`confidential: true`**（forge-core `spec_doc`。`true` / `false` 以外は `confidential_invalid` の違反。前付けが読めない文書と UTF-8 でない文書は
     **機密として扱う**（印がそこにあるかもしれない。fail closed）。前付けの無い文書は印なし）。作業項目は **`forge.work_items.confidential`**（新しいマイグレーション
     `20261128100000_work_item_confidential`、既定 `false`）。この MR では forge にこの列を書く経路は無い（UPDATE の権限も付けない）。kanflow の印を送る経路
     （連携のサービスアカウントからの書き込みと監査の行）は kanflow 側の連携と一緒に足す（計画 02 §12）。
   - **振る舞い**: 1 件を聞かれたとき（作業項目・要件）は **`FG-AUTH-0021` ContextWithheld**（403）。一覧（MR の文脈、X4 の `specs/requirements`・`diff`・`trace`）からは除き、
     `withheld {error_code, documents, requirements, work_items}` に数える（人には `null`）。除いたときは `forge.context.withheld`（info）を 1 回ログに出す。
     MCP の `search_requirements` と `get_trace_matrix` は X4 のルートを使うので、X4 の答えにも同じ除外を入れた（`SpecDocumentBody.confidential` も足した）。
   - **これは認可ではない**: 認可は `authorize()` のまま（エージェントもプロジェクトを読める）。境界は「文脈 API がまとめて AI に渡さない」ことで、git の clone・fetch・
     archive は印を見ない（browse の読み取りは 9 で揃えた）。機密の高い仕様は、エージェントのアカウントが読めないリポジトリに置く（D-5 の「Qwen だけ」）。印は二重の守り。
6. **`gstn context`**: `work-item`・`requirement`・`merge-request`（C-1: 予定のままのルートを残さない）。人の道具なのでヘッダーは送らない（トークンが `is_agent` なら除外される）。
7. **MCP サーバー `gstn mcp`**（読み取りのみ。forge-client の中、依存は forge-errors だけのまま）: 標準入出力の JSON-RPC 2.0（1 行 1 メッセージ、MCP の stdio）。
   `initialize`（`2025-06-18` / `2025-03-26` / `2024-11-05`）・`ping`・`tools/list`・`tools/call`。道具は計画の 4 つ（`get_work_item_context`・`get_requirement`・
   `search_requirements`（**1 プロジェクトの中**の検索。X4 の `specs/requirements?q=`）・`get_trace_matrix`（X4 の `specs/trace`））に `get_merge_request_context` を足した 5 つ。
   どれも `readOnlyHint`、`additionalProperties: false`（知らない引数・道具は `-32602`）。呼び出しは保存したトークンで REST API を 1 回 `GET` するだけで、権限はサーバーの
   `authorize()` が決める。失敗は `isError` の道具の結果（`error_code`・`user_message`・`request_id`）と stderr の 1 行。1 メッセージ 1 MiB まで。
8. **採らなかったもの・後に回すもの**: リポジトリをまたぐ要件の検索（保存した索引が要る。X4 の 4 の宿題のまま。M4 の影響の分析で、使う側と一緒に決める）。
   作業項目の文脈にほかのプロジェクトの MR を含めること（3 のとおり、存在の手がかりになるため。要るなら読む人の権限で辿る形で別に決める）。要件と PBI の結びつき
   （kanflow が持つ）。kanflow から機密の印を受け取る経路（5）。
9. **X5 の独立レビュー（2026-11-06、security・rust。BLOCK）で直したこと**:
   - **コードの番号（衝突）**: `FG-AUTH-0020` は先に main に入る S2（ADR-0025 の ActAsAssertionInvalid）が使うので、ContextWithheld は **`FG-AUTH-0021`** にした。
   - **読み取りのルートも同じ境界（security HIGH）**: エージェントの相手には、印の付いた仕様の文書の中身を HTTP の読み取りの API でも渡さない。
     `GET …/repository/blob`・`{path}/-/raw/…`・`…/repository/blame` は **`FG-AUTH-0021`**（`reason=spec_confidential`、`path`）。`…/repository/compare`・
     `…/repository/commits/{oid}`・`…/merge-requests/{iid}/versions/{n}/diff`・`…/merge-requests/{iid}/compare` は、その文書のファイルを差分から除き、
     `diff.withheld {files}` に数える（人には出さない）。**git の clone / fetch と archive は印を見ない**（対象外。
     2026-11-10 に改めた: 既定のブランチの先端に機密のファイルがあれば、エージェントの相手には全体を断る — ADR-0026 の末尾、`FG-AUTH-0022`）。**機密の高い仕様の本当の境界はリポジトリの権限**で、
     エージェントのアカウント（`is_agent` のトークンの持ち主・サービスアカウント・bot）に、そのリポジトリの読み取りを与えないこと。印は HTTP の読み取りの API の **二重の守り** にすぎない。
   - **版を選んで印を避けること（MEDIUM a）**: エージェントの相手には、読む版で印が付いているか、**既定のブランチの先端で印が付いている** パスを機密とする
     （`marked_paths`、先端の索引は 1 回分として上限に数える）。残る穴: どの版にも印の無い古い履歴（印を付ける前の版で、先端でもパスが変わっている・消えている文書）は読める。
     本当に機密なものは上のとおりリポジトリの権限で守る。
   - **MR の文脈の `files`（MEDIUM b）**: 印の付いた仕様のファイル（名前を変えた前のパスも）をエージェントには出さず、`withheld.files` に数える。
   - **`forge.work_items.confidential` を書く経路が無いこと（MEDIUM c）**: 既定の `false` のまま。kanflow の印を受け取る経路は、X3 / kanflow の同期の宿題として後に回す
     （連携のサービスアカウントからの書き込み、監査の行、`UPDATE (confidential)` の権限）。それまでは作業項目の印は効かない。
   - **エージェントには `is_agent` のトークンを使う（MEDIUM d）**: 人が `is_agent` なしで発行したトークンをエージェントが使えば、サーバーにとっては **人** である。
     `gstn mcp` のヘッダーで文脈・仕様・読み取りの API の答えは狭まるが、ほかのルートと監査の上でエージェントとして扱われるのは `is_agent` のトークンだけ。
     `gstn mcp --help` にも書いた。
   - **MCP の入力（rust HIGH、MEDIUM e）**: 1 MiB を超える行は、読み手のバッファを通して捨て、ためない（何 GB の行でもメモリは上限 + 1 回の読み取り分）。
     `-32600` を返して続ける。配列（バッチ。対応しない）・オブジェクトでない値は `-32600`（id は `null`）、id は文字列か数値だけ（ほかは `-32600`）。
     標準入力の終わり・閉じたパイプはきれいに終わり（終了コード 0）、それ以外の読み取りの失敗は新しいコード **`FG-CLI-0009` CliInputFailed**。
   - **LOW**: MR の文脈は compare も上限に数える（2 + エージェントは先端の 1）。要件の版の履歴は、エージェントには文書に印のあった版を出さない。
     `source` の `project` が読めないときは `FG-SYS-0001`（空の文字列にしない）。
