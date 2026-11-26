# エッジの「追加の変更の版」を GitStation で管理する手引き

- 対象: ESP32-S3 のモジュールのエッジの **I/O の意味の辞書**、**ファームウェア**（とその **設定のスキーマ**）を GitStation で版の管理をする人と、
  それを読む **中央の設定管理のツール**（以下「設定のツール」）を作る人。
- 根拠: 計画 21（2026-11-25 の範囲の変更）、ADR-0041 の Amendment 2026-11-25、ADR-0029 の Amendment 2026-11-25（G1 `keep`・G2 `fetch_depth`）。
- 例のリポジトリ: [`docs/examples/edge-dictionary/`](../examples/edge-dictionary/)、[`docs/examples/edge-firmware-s3/`](../examples/edge-firmware-s3/)。
  どちらも **合成の例**（イメージのダイジェスト・グループ名・ID は置き換える）。

GitStation に専用の機能は無い。**普通のリポジトリ・MR・保護ブランチ・必須のチェック・CI・成果物・Webhook・サービスアカウント** の使い方（パターン）で
作る。製品に足したのは 2 つの隙間だけ:

| 隙間 | `.forge-ci.yml` | 効くところ |
|---|---|---|
| G1 成果物を消さない | ジョブの `artifacts: { paths: [...], keep: true }` | **保護ブランチの push のパイプラインだけ**。それ以外（MR・保護されていないブランチ・手動・定期）では 30 日で消え、ジョブのログに 1 行出る |
| G2 取得の深さ | ジョブの `fetch_depth: 2`（1〜50、既定 1） | Runner がその深さでパイプラインの履歴を取る。追記だけの確かめが `HEAD^1` と比べるのに使う |

## 1. 決めた既定（計画 21 §11）

| 問い | 既定 |
|---|---|
| Secure Boot V2 の鍵 | **prod の鍵はオフライン。CI に置かない**。CI は署名の無い `app.bin` を作り、鍵を持つ側（設定のツールの側、または鍵を持つ人の PC）が署名してから配る。モジュールの用意のときに **鍵の要約を 3 つ eFuse に焼く**（使う 1・予備 2。予備の秘密鍵は別々の場所にオフラインで保管）（§8） |
| モジュールの型 | ESP32-S3（Secure Boot V2 は RSA-3072）。ESP-IDF の target は `esp32s3` だけ |
| 辞書の数 | **全社に 1 つ**: `edge-fw/io-dictionary`。ID は全社で一意、工場・設備の文脈は設定のツールのラベル |
| アナログの換算 | **中央**（設定のツール）。モジュールは生の値を送り、係数は設定のツールが持つ。辞書の `unit` は換算の後の単位 |
| リリースの印 | 保護ブランチ **`release`** の push のパイプライン（タグではパイプラインが動かない。タグは人の目印として付けてよい） |
| 置き場 | グループ `edge-fw/` の下に、辞書 1 つと、ハードウェアごと・派生ごとのファームウェアのプロジェクト |
| 追記だけの確かめ | リポジトリの中のスクリプトを CI で動かす。製品の組み込みのチェックにはしない（CODEOWNERS で守る） |

## 2. 全体の形

```text
 MR（承認 1 + ci/pipeline）           release の push のパイプライン           設定のツール
 main ──────────────────▶ main ──MR──▶ release ──▶ 追記だけの確かめ ─▶ ビルド ─▶ 成果物（keep）◀── API で取る（read_api）
                                                                              │                ▲
                                                                 check.completed（Webhook）────┘
```

| 持ち主 | 持つもの |
|---|---|
| GitStation（標準の機能） | 辞書とファームウェアのリポジトリ、MR の承認、保護ブランチと必須のチェック、CI、成果物（sha256 付き）と取得の API、Webhook、サービスアカウント、監査 |
| 利用者のリポジトリ（この手引きのパターン） | `io-dictionary.toml`・`config-schema.toml` と追記だけを確かめるスクリプト、ESP-IDF のビルドのジョブ |
| 設定のツール | モジュールの台帳、どのモジュールがどのファームウェアを使うか、チャンネル → 辞書の ID、配布・承認・突き合わせ、署名（prod の鍵） |
| モジュール | Secure Boot V2 で確かめて起動、OTA（2 つのパーティションとロールバック）。GitStation には接続しない |

## 3. 準備（プロジェクトごとに 1 回）

以下の `<ID>` は数の ID（`gstn project list` など）。操作は Maintainer 以上の **人** が行う。

### 3.1 グループとプロジェクト

1. グループ `edge-fw` を作る。辞書のプロジェクト `edge-fw/io-dictionary` と、ファームウェアのプロジェクト（例 `edge-fw/s3-io8`）を作る。
2. 例のリポジトリの中身を最初のコミットとして push する（辞書は `docs/examples/edge-dictionary/`、ファームウェアは `docs/examples/edge-firmware-s3/`）。
3. `.forge-ci.yml` のイメージを社内のレジストリのダイジェストに置き換える（§6.2）。手元で確かめる:

   ```bash
   gstn ci lint .forge-ci.yml --protected main,release
   gstn ci plan .forge-ci.yml --event push --branch release --protected main,release   # firmware に keeps artifacts が出る
   ```

4. `release` ブランチを `main` から作る。

### 3.2 保護ブランチと必須のチェック

`main` と `release` の両方に:

```bash
gstn protected-ref add --project <ID> --pattern refs/heads/main --push-min-role maintainer
gstn protected-ref add --project <ID> --pattern refs/heads/release --push-min-role maintainer
gstn protected-ref list --project <ID>                       # rule_id を見る
gstn merge-rule set --project <ID> --rule-id <main の rule_id> \
  --approvals 1 --check ci/pipeline --direct-push-forbidden true --resolved-discussions true
gstn merge-rule set --project <ID> --rule-id <release の rule_id> \
  --approvals 1 --check ci/pipeline --direct-push-forbidden true --resolved-discussions true
```

- **直接の push を禁じる**（`--direct-push-forbidden true`）。追記だけの確かめは `HEAD^1` だけと比べるので、複数のコミットをまとめて push されると
  途中の削除と再追加を見逃す。MR のパイプラインはマージの結果（第 1 の親 = 目標の先端）で動くので、MR 全体の差を見る。
  マージの方法は **マージコミットか squash** にする（どちらも第 1 の親が目標の先端なので、マージの後の push のパイプラインでも
  `HEAD^1` は直前の版）。**早送り（fast-forward）は使わない**: 目標が MR の先端へ動くだけなので、`HEAD^1` が MR の中の 1 つ前の
  コミットになり、MR 全体の差を見ない（MR のパイプラインの確かめは効くが、`release` の push の確かめが弱くなる）。
- `ci/pipeline` を必須にすると、追記だけのジョブが赤い MR はマージできない。
- `release` は **保護されている** ことが `keep` の条件（G1）。保護を外すとその後のパイプラインの成果物は 30 日で消える
  （保護はパイプラインを作ったときの値で決まる）。

### 3.3 CODEOWNERS

例の `CODEOWNERS`（リポジトリの直下、または `.forge/CODEOWNERS`）で、辞書・スキーマ・**確かめのスクリプト・`.forge-ci.yml`・CODEOWNERS 自体** の
変更に持ち主の承認を要るようにする。追記だけのジョブを MR で弱める（スクリプトを書き換える、ジョブを消す）ことを防ぐのはこの承認
（計画 21 §9）。所有者は `@username` か `@group/path`（グループの有効なメンバーで Developer 以上）。最後に一致した行が効く。

### 3.4 設定のツールのサービスアカウントとトークン

1. 管理者がブラウザでサービスアカウント（例 `edge-config-tool`）を作る（`gstn admin service-account list` で確かめる）。
2. グループ `edge-fw` の **Reporter** にする: `gstn member add --group <edge-fw の ID> --user-id <サービスアカウントの ID> --role reporter`。
3. ブラウザでそのサービスアカウントのトークンを **`read_api` だけ** で発行し、設定のツールの秘密の置き場に入れる（GitStation の CI には入れない）。
   漏れたら `gstn admin service-account revoke-token`。
- このトークンでエッジのグループのコードも読める（計画 21 §11 問い 5 の既定は「よい」）。成果物だけに狭めたくなったら任意の MR S1。
- **シークレットを受け取ったジョブの成果物は、サービスアカウントには読めない**（ADR-0029 Amendment 2026-11-12）。リリースのジョブに CI の
  シークレットを渡さないこと（§8 で prod の鍵を CI に置かない理由の 1 つ）。

### 3.5 Webhook

```bash
gstn webhook create --project <ID> --url https://config-tool.internal/hooks/gitstation \
  --event check.completed --event repository.pushed
```

- 送り先の `host:port` は運用者の許可リスト（`webhook.allowed_targets`）に入っている必要がある。秘密は作成のときに 1 回だけ出る。
- 受け手は `Forge-Signature` を確かめる（`docs/api/webhooks.md` の「受信先の検証の手順」）。**Webhook は知らせで、正本は API**（§9）。

### 3.6 Runner

- ジョブのイメージのレジストリが Runner の `image_registries` に入っていること（既存の設定）。
- ESP-IDF のイメージは大きい（数 GB）。Runner のホストに一度 pull しておくと最初のビルドが速い。

## 4. 辞書のリポジトリ（`edge-fw/io-dictionary`）

例: [`docs/examples/edge-dictionary/`](../examples/edge-dictionary/)

| ファイル | 中身 |
|---|---|
| `io-dictionary.toml` | 辞書（`dictionary_version` と `[[signal]]`） |
| `scripts/check-append-only.py` | 追記だけの確かめ（標準ライブラリだけ、Python 3.11 以上） |
| `tests/cases/`、`tests/run-cases.py` | 確かめのスクリプト自身の試験（入力の組と、一時の git リポジトリでの浅い取得） |
| `.forge-ci.yml` | `dictionary`（`fetch_depth: 2`）、`dictionary-selftest`、`release`（`keep: true`） |
| `CODEOWNERS` | 辞書の持ち主 |

### 4.1 項目

| 欄 | 必須 | 規則 |
|---|---|---|
| `id` | ○ | 永久の整数。**消さない・変えない・使い回さない**。新しい ID は今の最大 + 1（飛ばさない） |
| `name` | ○ | 汎用の量と状態（`temperature`、`button.pressed`）。小文字・数字・`_`、`.` で区切る。全体で一意（使い終わった名前も再利用しない） |
| `type` | ○ | `digital` / `analog` / `counter` |
| `unit` | analog・counter は ○ | UCUM の記号（`Cel`、`kPa`、`1`、`%`）。換算の後の単位 |
| `description` | ○ | 意味。**誤字も直さない**（直したいときは新しい ID） |
| `since` | ○ | その項目を足した `dictionary_version` |
| `deprecated` | — | `{ since = <版>, replaced_by = <ID> }`。**付けられるだけで、外せない・変えられない** |

設備の文脈（「金型の温度」「2 号機の非常停止」）は辞書に書かない。設定のツールのラベル（チャンネル → 辞書の ID + ラベル）。

### 4.2 許されること・断られること

| 変更 | 結果 |
|---|---|
| `[[signal]]` を末尾に足す（ID = 最大 + 1、`since` = 新しい版）と `dictionary_version` を 1 上げる | 合格 |
| 既存の項目に `deprecated` を付ける（`since` = 新しい版） | 合格 |
| 項目を消す・ID を変える・使い回す・飛ばす | **赤** |
| `name`・`type`・`unit`・`description`・`since` を変える | **赤** |
| `deprecated` を外す・変える | **赤** |
| 中身を変えたのに `dictionary_version` を上げない・2 以上上げる・下げる | **赤** |

### 4.3 意味を直したいとき（ジョブが赤いとき）

辞書は消せないので、**新しい ID を足して古い ID に `deprecated` を付ける**:

```toml
dictionary_version = 4

[[signal]]
id = 3
name = "pressure"
...
since = 1
deprecated = { since = 4, replaced_by = 6 }   # 足すのはこの 1 行だけ

[[signal]]
id = 6
name = "pressure.absolute"
type = "analog"
unit = "kPa"
description = "絶対圧"
since = 4
```

その後、設定のツールでチャンネルの対応を 3 → 6 に移す。古い ID 3 で送られてくる値は、移し終わるまで読み続けられる。

### 4.4 スクリプトの動き

- `python3 scripts/check-append-only.py`: `HEAD^1` の `io-dictionary.toml` と比べる。MR のパイプラインはマージの結果で動くので `HEAD^1` は
  目標のブランチの先端、`main` / `release` の push では直前の版。
- 終了コード: 0 合格 / 1 規則に反する（1 行に 1 つ理由）/ 2 確かめられない。**2 は赤**: `HEAD^1` が取れていない（`fetch_depth: 2` が無い）、
  git の失敗。最初のコミット（親が無い）とファイルを初めて足したコミットは形だけを確かめる。
- 手元で: `python3 scripts/check-append-only.py --previous 前の版.toml --current io-dictionary.toml`、`python3 tests/run-cases.py`。

## 5. ファームウェアのリポジトリ（ハードウェアごと・派生ごとに 1 つ）

例: [`docs/examples/edge-firmware-s3/`](../examples/edge-firmware-s3/)

| ファイル | 中身 |
|---|---|
| `CMakeLists.txt`、`main/`、`sdkconfig.defaults` | ESP-IDF のプロジェクト（target `esp32s3`、`CONFIG_APP_REPRODUCIBLE_BUILD=y`、2 つの OTA パーティション、ロールバック、Secure Boot V2 で署名はビルドの外） |
| `version.txt` | ファームウェアの版（`MAJOR.MINOR.PATCH`。ESP-IDF が `PROJECT_VER` として読む） |
| `config-schema.toml` | 設定のスキーマ（`schema_version` と `[[setting]]`） |
| `scripts/check-config-schema.py` | スキーマの追記だけの確かめと、`--version-bump`（版が上がったか） |
| `scripts/package.py` | 成果物を `out/` に揃える: `app.bin`・`config-schema.json`・`version.txt`・`SHA256SUMS` |
| `tests/` | 確かめのスクリプトの試験 |
| `managed_components/` | ESP-IDF の部品を使うなら、`dependencies.lock` と一緒にここへ入れる（ジョブにネットワークは無い） |
| `.forge-ci.yml` | `config-schema`・`release-version`（`release` の push だけ）・`firmware`（ESP-IDF、`fetch_depth: 2`、`keep: true`） |

**モジュールとファームウェアの対応は GitStation に持たない**（設定のツールがモジュールの登録のときに持つ）。辞書の ID もファームウェアに書かない。

### 5.1 設定のスキーマ

| 欄 | 必須 | 規則 |
|---|---|---|
| `key` | ○ | 永久。消さない・使い回さない。小文字・数字・`_`、`.` で区切る |
| `type` | ○ | `bool` / `int` / `float` / `string` / `ipv4` / `enum`。**変えない** |
| `default` | ○ | **必須**（古い設定のツールが値を送らなくても動く）。**変えない** |
| `min`・`max`（int・float）、`max_len`（string、既定 255）、`values`（enum） | — | **狭めない**（広げるのはよい） |
| `description` | ○ | 直してよい（意味を変えるなら新しい key） |
| `since` | ○ | その項目を足した `schema_version` |
| `deprecated` | — | `{ since = <版> }`。付けられるだけで外せない |
| `lineage`（直下） | 派生だけ | `{ from = "edge-fw/s3-io8", at = "fw-1.4.0" }`。元のプロジェクトと分かれた版の記録。一度書いたら変えない |

中身を変えたら `schema_version` を 1 上げる。アナログの換算の係数はスキーマに入れない（中央）。モジュールで換算したくなったら項目を足せる（追記だけ）。

### 5.2 ESP-IDF のイメージ

1. Espressif の公式のイメージ `espressif/idf`（サポート中の v5 系）を元に、**ツールチェーンを esp32s3（Xtensa）の分だけ** にしたイメージを作り、
   社内のレジストリに入れる（例 `registry.internal/espressif/idf-esp32s3`）。大きさは作るときに測る（全 target の推定 2〜4 GB より小さい見込み）。
2. `.forge-ci.yml` の `image:` を **`@sha256:<ダイジェスト>`** で固定する（タグだけは lint の `image_not_pinned`）。
3. GitStation 自身の CI のイメージ（`ops/ci-image`、`ci/rust`）には混ぜない。
4. ジョブのコンテナはネットワーク無し・ルートは読み取り専用・書けるのは `/workspace` と `/tmp`。例のジョブは `HOME=/tmp/home`、
   `IDF_CCACHE_ENABLE=0`、`IDF_COMPONENT_MANAGER=0` にして、イメージの入口の代わりに `. "$IDF_PATH/export.sh"` を読む。

### 5.3 リリースの手順

1. 機能の変更は `main` への MR（`config-schema` が緑、`firmware` がビルドできる）。
2. 出すときは `version.txt` を上げる MR を `main` に入れ、`main` → `release` の MR を作る。**その MR の承認がリリースの承認**。
3. マージで `release` の push のパイプラインが動く: `config-schema`、`release-version`（版が前の `release` より上がったか）、`firmware`。
   成功すると `firmware` の成果物が **`keep`**（30 日を過ぎても消えない）。ジョブのログに `artifacts kept: they do not expire …`。
4. 人の目印のタグ（`fw/v1.2.3` など）は付けてよい（パイプラインは動かない）。

**`release` で動かしたのに残らない** ときは、ジョブのログの `artifacts keep: true is not honoured here …` を見る: `release` が保護されて
いない（§3.2）、パイプラインが手動の実行・定期の実行（どちらも保護された push ではない）。`release` に push し直す（MR）。

## 6. 派生のファームウェアの作り方

ハードウェアが違えば **別のファームウェア**（例 `edge-fw/s3-io16`、例のリポジトリから作る）。同じモジュールの派生（入力の読み方だけ違う、など）は
**派生のファームウェア**:

1. 新しいプロジェクトを作る（例 `edge-fw/s3-io8-counter`）。**空のまま** にする。
2. 元のプロジェクトの履歴を新しいプロジェクトへ **1 度 push** する（空のリポジトリへの履歴の取り込み、ADR-0008 Amendment 2026-11-12）:

   ```bash
   git clone https://gitstation.internal/edge-fw/s3-io8.git && cd s3-io8
   git push https://gitstation.internal/edge-fw/s3-io8-counter.git main release
   ```

3. 派生の `config-schema.toml` に `lineage = { from = "edge-fw/s3-io8", at = "<分かれた版>" }` を足し、`schema_version` を上げる（最初の MR）。
4. §3 の準備（保護ブランチ・必須のチェック・CODEOWNERS・Webhook）を派生でも行う。サービスアカウントはグループの Reporter なので追加は要らない。
5. **分かれた後に足す設定の項目には派生の接頭辞を付ける**（例 `counter.`）。元と派生で同じ `key` を違う意味に使わない（ジョブでは強制しない。レビューで見る）。
6. 元の修正を派生へ入れるとき: 元を取り込んでマージし、派生への MR にする（普通の git の操作。GitStation にフォークの機能は無く、要らない）。

   ```bash
   git remote add upstream https://gitstation.internal/edge-fw/s3-io8.git
   git fetch upstream && git switch -c sync/upstream-1.5 && git merge upstream/main
   git push origin sync/upstream-1.5      # → 派生の main への MR（config-schema の確かめは派生の HEAD^1 と比べる）
   ```

   元で足された設定の項目は派生にも入る（派生のスキーマの系統の中では追記）。派生の `schema_version` は派生の番号で 1 上げる（マージで衝突したら手で直す）。

設定のスキーマは **系統ごと**: 派生は分かれた時点の元のスキーマから始まり、以後は自分の `HEAD^1` と比べて追記だけ。どのモジュールがどの
ファームウェア（元か派生か）を使うかは設定のツールがモジュールの登録のときに持つ。

## 7. 成果物と完全性

| 区間 | 守るもの |
|---|---|
| GitStation → 設定のツール | TLS + `read_api` のトークン + 成果物の sha256（GitStation が受け取るときに計算、一覧の `sha256`、取得の応答の `X-Forge-Sha256`）+ `SHA256SUMS` |
| 設定のツール → モジュール | ESP32-S3 の Secure Boot V2（モジュールが署名を確かめる） |

Ed25519 の封筒（ADR-0041 の E1）は既定では使わない（守る区間が無い）。設定のツールが取った物を別の経路（USB・別のサーバー）で配るように
なったら任意の MR E4。

## 8. 署名（Secure Boot V2、ESP32-S3）

- **prod の鍵はオフライン、CI に置かない**。理由: (a) CI のジョブの中のコード（MR の作者のコード）が何にでも署名できてしまう、(b) シークレットを
  受け取ったジョブの成果物はサービスアカウントが読めない（設定のツールが取れない）。
- 流れ: 設定のツールが `app.bin` を取り、sha256 を確かめ、鍵を持つ側で `espsecure.py sign_data --version 2 --keyfile <鍵> --output app-signed.bin app.bin`
  で署名してから OTA で配る。ブートローダーはモジュールの用意のときに書く（OTA では変えない）。
- **鍵の要約は用意のときに 3 つ焼く**（ESP32-S3 は 3 つまで、1 つずつ失効できる）: 使う 1・予備 2。予備の秘密鍵は **別々の場所にオフライン** で保管。
- 漏えい・紛失のとき: 使っている鍵の要約を失効し、予備の鍵で署名した版を配る（手順は設定のツールの runbook。GitStation は持たない）。
- 開発・試験の機器の鍵だけは CI のシークレット（保護ブランチだけに渡る）に置いてもよい。そのときその成果物はサービスアカウントには
  読めない（Maintainer の人だけ）。prod の `release` のジョブには渡さない。

## 9. 設定のツールからの読み方

```bash
TOKEN=<read_api のトークン>
API=https://gitstation.internal/api/v1
# 1. release の成功したパイプライン（新しい順）
curl -sS -H "Authorization: Bearer $TOKEN" "$API/projects/<ID>/pipelines?ref=release&limit=20"
# 2. そのパイプラインのジョブ（firmware / release のジョブの id）
curl -sS -H "Authorization: Bearer $TOKEN" "$API/projects/<ID>/pipelines/<pipeline_id>"
# 3. ジョブの成果物の一覧（path・size_bytes・sha256・keep）
curl -sS -H "Authorization: Bearer $TOKEN" "$API/projects/<ID>/pipelines/<pipeline_id>/jobs/<job_id>/artifacts"
# 4. 1 つの成果物（応答の X-Forge-Sha256 と一覧の sha256 を確かめる）
curl -sS -D - -o app.bin -H "Authorization: Bearer $TOKEN" \
  "$API/projects/<ID>/pipelines/<pipeline_id>/jobs/<job_id>/artifacts/<artifact_id>"
```

- **一覧の `keep` が `true` の成果物だけをリリースとして扱う**（`false` は保護された `release` の push ではない、または 30 日で消える）。
- Webhook `check.completed`（`ci/pipeline` のチェックの結論が変わったとき）で知り、上の一覧を読む。取りこぼしは数分ごとの一覧で埋める。
- `SHA256SUMS` の値と、`config-schema.json` の `schema_version`、`version.txt` を記録しておく（どのモジュールにどの版を配ったかは設定のツール）。

## 10. うまくいかないとき

| 症状 | 見るところ | 直し方 |
|---|---|---|
| 追記だけのジョブが赤（終了コード 1） | ジョブのログの `io-dictionary:` / `config-schema:` の行 | §4.3: 新しい ID / key を足して古い方に `deprecated`。版の番号を 1 上げる |
| `cannot verify: HEAD^1 is not in the clone` | ジョブに `fetch_depth: 2` が無い | `.forge-ci.yml` に足す（1〜50） |
| `fetch_depth_out_of_range`（FG-CI-0001） | `fetch_depth` が 1〜50 の整数でない | 直す（`"2"` も `2` もよい） |
| `artifacts_keep_invalid`（FG-CI-0001） | `keep` が `true` / `false` でない | 直す |
| `release` の成果物が `keep: false` | ジョブのログの `is not honoured here` | `release` を保護する（§3.2）。手動・定期の実行ではなく MR のマージ（push）で動かす |
| 成果物の割り当て FG-CI-0022 | `keep` の成果物も数える（既定 5 GiB） | 管理者に相談（runbook `ci-runner` の「残す成果物」） |
| ESP-IDF のビルドがネットワークを取りに行って失敗 | 部品（component manager） | `dependencies.lock` と `managed_components/` をリポジトリに入れる |
| `image_not_pinned` / FG-CI-0011 | イメージのダイジェスト・Runner の `image_registries` | §5.2 |
| 設定のツールが成果物を読めない（FG-AUTH-0022） | そのジョブがシークレットを受け取った | リリースのジョブにシークレットを渡さない（§8） |

## 11. 限界（分かっていること）

- 追記だけの確かめは **リポジトリの中のスクリプト** なので、`.forge-ci.yml` やスクリプトを変える MR で弱められる。守るのは CODEOWNERS の承認と
  レビュー。迂回が起きたら製品の組み込みのチェックにする（計画 21 §11 問い 8）。
- 比べるのは `HEAD^1` だけ。直接の push を禁じ（§3.2）、MR でだけ変える前提。
- `release` の push のパイプラインは保護されたときの値で決まる。保護の前に作られたパイプラインの成果物は残らない。
- `keep` を外す API・CLI は無い。古いリリースを消すのは管理者の手順（runbook `ci-runner`）。
