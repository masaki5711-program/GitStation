# forge-ci

CI のパイプラインの定義 `.forge-ci.yml` の解析・lint・実行計画（計画 14 C1、計画 04 P-1〜P-3）。**純粋**: I/O・時計・乱数を使わない（バイトを受け取り、値を返す）。
同じバイトにはどこで動かしても同じ答えを返す。スクリプトは後で Runner（C4）に渡す文字列で、ここでは解釈も展開も実行もしない。

| 公開の API | 中身 |
|---|---|
| `check(bytes, &LintContext) -> Result<Definition, Vec<Problem>>` | 解析と lint。問題が 1 つも無いときだけ `Definition` になる。問題は行・理由・対象の順に並ぶ |
| `lint(bytes, &LintContext) -> Vec<Problem>` | 問題だけ（空なら正しい） |
| `plan(&Definition, &Trigger) -> Result<Plan, Vec<Problem>>` | push（ブランチ）または MR のパイプラインで動くジョブと順番。同じ入力には同じ答え |
| `invalid(&[Problem]) -> ForgeError` | `FG-CI-0001 CiConfigInvalid`（422、info）。文脈に最初の問題の `reason`・`line` と `problems`（数） |

`Problem` は `line`（1 から。ファイル全体は 0）、`reason`（安定した名前。下の表）、`subject`（鍵・ジョブ・ステージ・`needs`・シークレットの名前）。

## スキーマ（version 1）

```yaml
version: 1
stages: [build, test, deploy]
jobs:
  build:
    stage: build                                  # 必須。stages のどれか
    image: registry.internal/rust@sha256:<64 桁>   # 必須。ダイジェストで固定（タグだけは不可）
    timeout: 30m                                  # 必須。<n>s / <n>m / <n>h、4h まで
    script:                                       # 必須。空でない文字列の並び（200 行まで）
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
- 名前: ジョブ・ステージ・タグは `[a-z0-9][a-z0-9._-]*`、シークレットは `[A-Z][A-Z0-9_]*`（どれも 63 バイトまで）。

## 理由（`reason`）

| 分類 | 理由 |
|---|---|
| 読み取り | `too_large`（64 KiB 超）、`not_utf8`、`empty_file`、`not_a_mapping`、`syntax`、`multiple_documents`、`yaml_anchor`、`yaml_alias`、`yaml_tag`、`too_deep`、`too_many_keys`（1 つのマップに 64 超）、`duplicate_key`（同じジョブ名の 2 度書きもこれ）、`key_not_text` |
| スキーマ | `key_unknown`、`field_missing`、`field_invalid`、`version_unsupported`、`stages_empty`、`stage_name_invalid`、`stage_duplicate`、`too_many_stages`（16）、`stage_unknown`、`jobs_empty`、`too_many_jobs`（48）、`job_name_invalid`、`image_not_pinned`、`script_empty`、`too_many_script_lines`（200）、`timeout_missing`、`timeout_invalid`、`timeout_too_long`（4h）、`tag_invalid`、`too_many_tags`（8）、`secret_name_invalid`、`too_many_secrets`（16）、`branch_pattern_invalid`、`too_many_branches`（16）、`artifact_path_invalid`、`too_many_artifacts`（16） |
| ジョブの間（スキーマが正しいときだけ） | `needs_unknown`、`needs_duplicate`、`too_many_needs`（16）、`needs_later_stage`、`needs_cycle`（循環の上のジョブごと）、`secret_on_unprotected_ref` |
| 計画 | `needs_not_in_pipeline`（選ばれたジョブが、その契機で動かないジョブを `needs` に持つ） |

**`secret_on_unprotected_ref`**: シークレットを持つジョブは `merge_requests: false` が要り、`branches` のどれもが保護されていること。
`LintContext.protected_branches` にリポジトリの保護ブランチを渡すとそれで判定し、渡さない（手元の検査）ときはワイルドカードを断る（完全一致の名前だけ通す）。
実行時にも C6 が保護ブランチにしかシークレットを渡さない（二重の守り）。

## 実行計画

ジョブは、その契機で選ばれたジョブのうち **直前の空でないステージのすべて**（ステージの壁。それより前は推移的に満たされる）と **`needs`**（同じステージでもよい）を待つ。
`wave` は前提が無ければ 0、あれば最も遅い前提 + 1。同じ `wave` は並行してよい。並びは (wave, ステージ, 名前)。

## 依存の向き

`forge-ci → forge-doc → forge-errors`（forge-doc は厳しい YAML の木のため）。依存してよいのはこの 2 つだけで `tests/purity.rs` が確かめる。
`forge-hosting/tests/dependency_direction.rs` の層の表にも載せた。

## C1 で残したもの

契約 9（CLAUDE.md）は `forge-client`（`gstn`）の依存を `forge-errors` と `forge-doc` に限り、機能クレートどうしの依存（`forge-checks → forge-ci`）も禁じる。
そのため `gstn ci lint` / `gstn ci plan`（P-3）とサーバーのチェック `forge/ci-lint`（P-1）は、契約 9 と ADR-0020 の Amendment（`forge-ci` を `forge-doc` と同じ純粋な層に置き、
`gstn` と `forge-checks` からの依存を許す）を先にエージェントモデル側で決めてから足す。
