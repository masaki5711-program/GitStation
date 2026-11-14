# forge-ci

CI のパイプラインの定義 `.forge-ci.yml` の解析・lint・実行計画（計画 14 C1、計画 04 P-1〜P-3）。**純粋**: I/O・時計・乱数を使わない（バイトを受け取り、値を返す）。
同じバイトにはどこで動かしても同じ答えを返す。スクリプトは後で Runner（C4）に渡す文字列で、ここでは解釈も展開も実行もしない。

| 公開の API | 中身 |
|---|---|
| `check(bytes, &LintContext) -> Result<Definition, Vec<Problem>>` | 解析と lint。問題が 1 つも無いときだけ `Definition` になる。問題は行・理由・対象の順に並び、100 件を超えると切って `too_many_problems` を足す |
| `LintContext::with_protected_branches(list)` / `LintContext::local_check()` | 既定値は無い。サーバー（と `gstn --protected`）はリポジトリの保護ブランチの一覧を渡す。手元の検査は「どのブランチも保護されていない」扱い（シークレットを持つジョブは `secret_protection_unknown`） |
| `lint(bytes, &LintContext) -> Vec<Problem>` | 問題だけ（空なら正しい） |
| `plan(&Definition, &Trigger) -> Result<Plan, Vec<Problem>>` | push（ブランチ）または MR のパイプラインで動くジョブと順番。同じ入力には同じ答え。`Trigger.protected` が偽か MR のとき、シークレットを持つジョブが選ばれたら失敗（`secret_on_unprotected_ref`。黙って外さない） |
| `invalid(&[Problem]) -> ForgeError` | `FG-CI-0001 CiConfigInvalid`（422、info）。文脈に最初の問題の `reason`・`line` と `problems`（数） |

`Problem` は `line`（1 から。ファイル全体は 0）、`reason`（安定した名前。下の表）、`subject`（鍵・ジョブ・ステージ・`needs` などの名前。64 文字で切る。
`secret_name_invalid` では値の貼り間違いかもしれないので出さない）。

`Definition` / `Job` / `Rules` のフィールドはクレートの中だけで、外からは読み取りのメソッドで読む（`check` を通ったものしか作れない、という不変条件を守るため）。

## スキーマ（version 1）

```yaml
version: 1
stages: [build, test, deploy]
jobs:
  build:
    stage: build                                  # 必須。stages のどれか
    image: registry.internal/rust@sha256:<64 桁>   # 必須。OCI の参照の文法でダイジェストで固定（タグだけは不可）
    timeout: 30m                                  # 必須。<n>s / <n>m / <n>h、4h まで
    script:                                       # 必須。空でない文字列の並び（200 行まで、1 行 4096 バイトまで、タブ以外の制御文字は不可）
      - cargo build --release
    needs: []                                     # 同じか前のステージのジョブ（16 まで）
    tags: [linux]                                 # Runner の選び方（8 まで）
    artifacts:
      paths: [target/release/app]                 # 作業ツリーの中の相対パス（16 まで）
  deploy:
    stage: deploy
    image: registry.internal/deploy@sha256:<64 桁>
    timeout: 10m
    script: [./deploy.sh]
    secrets: [DEPLOY_KEY]                         # 名前だけ（値はサーバー、C6）。16 まで
    rules:
      branches: [main]                            # push で動くブランチ。完全一致か末尾 `*` の前方一致。既定 ["*"]
      merge_requests: false                       # MR のパイプラインで動くか。既定 true
```

- 知らない鍵は `key_unknown`（どの階層でも）。YAML は forge-doc の厳しい木（アンカー・エイリアス・タグ・2 つ目の文書・重複の鍵を断る。入れ子は 16 まで）。
- イメージ: `[host[:port]/]component(/component)*[:tag]@sha256:<64 桁>`。component は小文字の英数字を `.`・`_`・`__`・`-` でつなぐ。先頭の `-`、引用符、`;`、空白は通らない。
- 成果物のパス: 作業ツリーの中の相対パス。`..`・`.`・`.git` の要素、glob の記号（`* ? [ ] { } !`）、`\`、ドライブは不可。
  **C4 の義務**: Runner はリンクをたどらず（no-follow）、作業ディレクトリの中に閉じて集める。
- 名前: ジョブ・ステージ・タグは `[a-z0-9][a-z0-9._-]*`、シークレットは `[A-Z][A-Z0-9_]*`（どれも 63 バイトまで）。

## 理由（`reason`）

| 分類 | 理由 |
|---|---|
| 読み取り | `too_large`（64 KiB 超）、`not_utf8`、`empty_file`、`not_a_mapping`、`syntax`、`multiple_documents`、`yaml_anchor`、`yaml_alias`、`yaml_tag`、`too_deep`、`too_many_keys`（1 つのマップに 64 超）、`duplicate_key`（同じジョブ名の 2 度書きもこれ）、`key_not_text` |
| スキーマ | `script_line_invalid`、`script_line_too_long`（4096 バイト）、`key_unknown`、`field_missing`、`field_invalid`、`version_unsupported`、`stages_empty`、`stage_name_invalid`、`stage_duplicate`、`too_many_stages`（16）、`stage_unknown`、`jobs_empty`、`too_many_jobs`（48）、`job_name_invalid`、`image_not_pinned`、`script_empty`、`too_many_script_lines`（200）、`timeout_missing`、`timeout_invalid`、`timeout_too_long`（4h）、`tag_invalid`、`too_many_tags`（8）、`secret_name_invalid`、`too_many_secrets`（16）、`branch_pattern_invalid`、`too_many_branches`（16）、`artifact_path_invalid`、`too_many_artifacts`（16）、サービス（C5s）: `too_many_services`（2）、`service_name_invalid`、`service_duplicate`、`service_env_invalid`（値は出さない）、`too_many_service_env`（16）、`service_user_invalid`、`service_tmpfs_invalid`、`too_many_service_tmpfs`（4）。警告（エラーではない、`Definition::warnings`、`gstn ci lint` は exit 0）: `service_user_root` |
| ジョブの間（スキーマが正しいときだけ） | `needs_unknown`、`needs_duplicate`、`too_many_needs`（16）、`needs_later_stage`、`needs_cycle`（循環の上のジョブごと）、`secret_on_unprotected_ref`、`secret_protection_unknown`（保護の一覧を知らない手元の検査） |
| 全体 | `too_many_problems`（100 件を超えた） |
| 計画 | `needs_not_in_pipeline`（選ばれたジョブが、その契機で動かないジョブを `needs` に持つ）、`secret_on_unprotected_ref`（保護されていないブランチか MR で、シークレットを持つジョブが選ばれた）、`eval_report_declared_twice`（計画 17 P17-2: 選ばれたジョブの 2 つ以上が成果物 `eval-report.json` を宣言した。`forge/ai-eval` が読む報告が決まらない） |

**`secret_on_unprotected_ref`**: シークレットを持つジョブは `merge_requests: false` が要り、`branches` のどれもが保護されていること。
`with_protected_branches` の一覧で判定する。パターンが覆われるのは、同じパターンが保護されているか、完全一致の名前が保護のパターンに合うときだけ
（**わざと保守的**: `release/*` が保護でも `release/v1*` は覆われたと見なさない）。`local_check` ではどのブランチも保護されていない扱い。
実行時にも C6 が保護ブランチにしかシークレットを渡さない（二重の守り）。

## 実行計画

ジョブは、その契機で選ばれたジョブのうち **直前の空でないステージのすべて**（ステージの壁。それより前は推移的に満たされる）と **`needs`**（同じステージでもよい）を待つ。
`wave` は前提が無ければ 0、あれば最も遅い前提 + 1。同じ `wave` は並行してよい。並びは (wave, ステージ, 名前)。

## Runner に任せること（C4）

CPU・メモリ・プロセス数などの資源の上限は定義に書かせず、Runner が固定する（C4）。`timeout` だけが定義の側の上限（4 時間まで）。

## 依存の向き

`forge-core → forge-ci → forge-doc → forge-errors`（契約 9。forge-doc は厳しい YAML の木のため）。`gstn`（`forge-client`）も依存してよい（ADR-0020 の Amendment）。依存してよいのはこの 2 つだけで `tests/purity.rs` が確かめる。
`forge-hosting/tests/dependency_direction.rs` の層の表にも載せた。

## 使う側

| 使う側 | 何を読むか | 結果 |
|---|---|---|
| `gstn ci lint [path] [--protected a,b]` / `gstn ci plan [path] --branch <b> [--event push\|mr] [--protected a,b] [--json]`（`forge-client` の `ci.rs`） | 手元の `.forge-ci.yml`（既定は上へたどって見つけたリポジトリの根）。サーバーには何も送らない | 問題があれば終了コード 1（`FG-CI-0001`） |
| `forge/ci-lint`（後の MR） | MR の版の head。保護ブランチの一覧を DB から読んで `with_protected_branches` に渡す | — |
| `forge-pipelines`（C2） | push・MR の版・手動の実行のコミット。保護ブランチの一覧を `with_protected_branches` に、`Trigger.protected` は保護されたブランチへの push だけ真 | 問題があればパイプラインは `invalid`（FG-CI-0001 と問題を行に記録） |

パイプラインの作成・実行（C2 以降、DB・イベント）は `forge-ci` に入れず、機能クレート（仮称 `forge-pipelines`）に置く。
