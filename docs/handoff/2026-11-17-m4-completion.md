# 引き継ぎ: M4 の完了の条件をこの PC の試験環境で確かめる（2026-11-17）

利用者の決定（2026-11-17）:「テスト段階では　ここのpcを使います」。PFS の Linux サーバーへの最終の配置は後で行う。
対象は試験環境（compose `gs-m3-test`、worktree `D:/APP/GitStation-wt-m3-testenv`、LAN の https の入口は runbook `m3-test-environment`）だけ。本番は無関係。
データはすべて合成。

確かめる条件（計画 14 §8 の 2 と R1）: GitStation の CI が本物の Runner で動き、社内アプリ 1 本がこの PC のターゲットへ自動でデプロイされ、
ヘルスチェックの失敗で自動でロールバックされる。

## 1. 調べた結果（2026-11-17 の時点）

| 項目 | 状態 |
|---|---|
| forge serve / worker | 試験環境の worktree のリリースのビルド（ブランチ `chore/ops-m3-test-environment`）。main より 85 コミット古かった → `deploy-testenv.sh` で main 9ca82c01 に上げる |
| PostgreSQL | `gs-m3-test-gs-test-pg-1`（127.0.0.1:25432） |
| Runner | `docker-test-1`（tags `linux,podman`）が compose `gs-runner-test`（別の worktree `D:/APP/GitStation-wt-runner`、11-11 のイメージ）で動いている。GitStation 自身のパイプライン 4〜7（lint・test・generated）は 11-11 にこの Runner で成功済み |
| 試験用レジストリ | `gs-runner-test-registry-1`（`registry:5000`、`ci/alpine:3.20`・`ci/rust:1.99.1`・`ci/postgres:16`） |
| デプロイ | `[deploy]` の設定が無い（deployments are off）。ターゲットのホストは無い |
| SSH の届き方 | forge の方針で loopback・link-local のターゲットは拒否（FG-DEPLOY-0013 `denied_address`）。Rancher Desktop の Docker は Windows の 127.0.0.1 か 0.0.0.0（LAN に見える）にしか公開できない |

## 2. 選んだ構成

| 項目 | 選択 | 理由 |
|---|---|---|
| デプロイのターゲット | `gs-m3-test` に新しいコンテナ `deploy-target`（`ops/test-env/compose.m4.yml`、alpine + sshd）。host key は初回の起動で作り volume に残す。利用者 `deploy` の鍵は forge の鍵 1 本だけで `restrict,command="/opt/forge-deploy/dispatch"` 付き。sshd は `127.0.0.1:22022` に公開 | 推奨どおり。LAN からは届かない |
| forge から届く経路 | `ops/test-env/deploy-forwarder.mjs`: この PC の Hyper-V 内部のアドレス（WSL の vEthernet、例 `192.0.2.1`）の 22022 → 127.0.0.1:22022。接続元がこの PC 自身のときだけ通す。`deploy.allowed_networks` をその /20 に絞る | loopback は forge が拒否する（仕様、緩めない）。vEthernet は LAN に出ない。アドレスは再起動で変わりうる（そのときはターゲットの `host` を直す） |
| デプロイするアプリ | 合成アプリ `hello-app`（静的なページと `/health`、busybox httpd をコンテナの中の 127.0.0.1:8080 だけで動かす）。試験環境の新しいプロジェクト `M4 hello-app` に置く | 小さく、ヘルスチェックを壊す変更が作りやすい |
| アプリの取得 | ターゲットのスクリプト `hello-app` が `deploy <sha>` で GitStation から `read_repository` のトークンで fetch し、`git archive` でリリースのディレクトリを作り、`current` を切り替える。`rollback <sha>` は前の SHA のリリースへ戻す。`deploy` と `rollback` は同じ `flock` | runbook `deploy` の契約どおり |
| Runner | 新しい Runner `docker-test-m4`（tags `linux,podman,m4`）を `forge admin runner register` で登録し、`gs-m3-test` の新しいコンテナ `runner-m4`（main の `ops/runner/Dockerfile` のビルド、`gitstation/forge-runner:m4`）で動かす。レジストリは `gs-runner-test_runner` のネットワークに参加して使う。古い `docker-test-1` は DB で無効にする（コンテナには触れない） | 古い Runner は 85 コミット前のバイナリ。別のプロジェクトのコンテナを作り直さない |
| CI の定義 | `build` → `test`（`tags: [linux, m4]`、alpine をダイジェストで固定）。デプロイは CI のジョブではなく forge の環境 `stg`（`auto_deploy_ref: refs/heads/main`、`auto_rollback: true`、承認なし） | ジョブはネットワークを持たず、デプロイしない（ADR-0029・ADR-0031）。「deploy の段」は main の push のパイプラインの成功から worker が作る |
| 自動マージ | プロジェクトの `merge.auto = full`（Owner の Web のセッションで設定）、`main` を保護し必須チェック `ci/pipeline` | 計画 14 §8 の 1 |

## 3. 手順

手順の全体は runbook `m4-test-environment`（この branch で追加）。要点:

1. `deploy-testenv.sh` で試験環境を main 9ca82c01 に上げた（migration 9 本を適用、`/`・`/-/projects` など 200）。
2. `forge admin token issue`（dora、7 日）と `forge admin runner register --name docker-test-m4 --tags linux,podman,m4`（runner_id 2、
   request_id 5012318ab24b4245be39754d2a41324a）。トークンは `.local/m4/` のファイルだけに置いた。
3. `deploy-target` と `runner-m4` を `gs-m3-test` に起動。`runner-m4 check` が `ok: configuration, token, rootless podman, job isolation`。
   ターゲットの host key の指紋 `SHA256:YN8pv8tE2Pfg6G9cScfNrVs/N0Mag9w6jhy+5lYdUPE`（コンテナのログ）と、登録の応答の指紋が一致。
   `forge doctor`: `deploy.known_hosts 1 target(s): 7/stg/app-1 … SHA256:YN8pv8tE…`、`ci.runners 2 enabled runner(s)`。
4. プロジェクト `dora/m4-hello-app`（id 7）: `main` を保護し `ci/pipeline` を必須、直接の push を禁止。環境 `stg`
   （request_id 7a0fba9783b549ef995bc58e620b9809）、ターゲット `app-1`（7d5c28d5e5e9464585101042eed4b362）、
   自動マージ `full`（dora の Web のセッション、1b88d46d147b4243b88c2e0b4ac25551、応答 `acts: true`）。

## 4. 証跡

### 4.1 自動マージ → 自動デプロイ（成功）

| 段階 | id | 結果 | request_id |
|---|---|---|---|
| MR !3「hello-app v4」の作成 | MR iid 3 | auto-merge `queued`（18fe3ae2dca74f788c45438e5bdaa1d6） | 4082165542ce451caad9950cb442fe55 |
| MR のパイプライン（build → test、runner `docker-test-m4`） | pipeline 18 | succeeded | 14ab8ff1d57341a3ade546eb5a898195 |
| キューのパイプライン（マージ結果） | pipeline 19 | succeeded → 自動でマージ | 1792a8eaf4374c0096e806bb84c13bf5 |
| `main` の push のパイプライン（`protected = t`） | pipeline 20（6b2838c5） | succeeded | 1792a8eaf4374c0096e806bb84c13bf5 |
| 自動のデプロイ（`requested_by` system、stg） | deployment 6 | **succeeded**（07:23:34 → 07:23:38 UTC） | e811460ea9584efd821ee02e1948a536 |

ターゲットの記録: `deploy 6b2838c5… start / switched`、`health 6b2838c5… healthy`。アプリは `release: v4`、`/health` = `ok`。

### 4.2 ヘルスチェックの失敗 → 自動のロールバック

MR !4「hello-app v5」は `public/health` を `maintenance` にする（CI の構造の検査は通る）。

| 段階 | id | 結果 | request_id |
|---|---|---|---|
| MR / キュー / `main` の push のパイプライン | 22 / 23 / 24（6d27619e） | すべて succeeded、自動でマージ | 7bcf30b62a0b40ee8c942372b7810722 / faa4e930635043dba9a3e87662599290 |
| 自動のデプロイ | deployment 7 | **rolled_back**、`error_code = FG-DEPLOY-0005`、`failed_target = app-1`、`previous_sha = 6b2838c5…` | 8197d734988a4e0dbbc06dd8c7de9c96 |

ターゲットの記録: `health 6d27619e… FAILED: unhealthy: /health answered 'maintenance'` → `rollback 6b2838c5… switched` →
`health 6b2838c5… healthy`。ロールバックの後、アプリは `release: v4`、`/health` = `ok`。

### 4.3 途中で見つかったこと（deployment 1〜5）

| deployment | 結果 | 原因 | 扱い |
|---|---|---|---|
| 1（MR !1 の後）、2（MR !2 の後） | `rollback_failed` FG-DEPLOY-0006（先に FG-DEPLOY-0003 `connection_lost`） | **製品の不具合（Windows だけ）**: 下の D1・D2 | 回避（`sshwrap.exe`）。最初のデプロイなので前の版が無く `no_previous_version`（仕様） |
| 3、4（手動の再要求） | 同上 | 同上（4 で ssh の stderr を取って特定: `command-line line 0: invalid quotes`） | — |
| 5（手動の再要求） | `rollback_failed` | 試験のターゲットの誤り: alpine の busybox に httpd が無い（`busybox-extras` が要る）、`pkill -f` が監視のループも止めた | ターゲットのイメージを直した |

- **D1（不具合）**: `forge-deploy` の `transport.rs` は `-o UserKnownHostsFile="<path>"` を 1 つの引数で渡す。Windows では Rust が
  引数の中の `"` を `\"` にしてコマンド行を作り、ssh（Git の MSYS 版も System32 版も）が `invalid quotes` で 255 を返す。forge は
  stderr を記録しないので、FG-DEPLOY-0003 `reason=connection_lost`（「スクリプトが動いたかもしれない」）に見え、原因が追えない。
  直し方の案: 引用符を付けない（パスに空白が無いことを確かめて拒む）か、`-o` ではなく `UserKnownHostsFile` を一時の設定ファイルに書く。
  Linux（PFS のサーバー）の argv には影響しない。request_id: deployment 1 `8a32fa6de5a248448e98c41d388ba51b`、2 `eb7143fde07c42439ab9ac8c9d81ce7f`。
- **D2（不具合）**: 同じ所で環境変数を `PATH`・`SystemRoot` だけにするため、Windows の OpenSSH（System32）は `ProgramData` が無く、
  何も出さずに 255 で終わる。Windows で動かす前提が無ければ、doctor で「Windows では Git の ssh を使う」と知らせるだけでもよい。
- **D3（運用の穴）**: `test-env.mjs up` は forge.toml を毎回書き直すので `[deploy]` を置けない。今回は環境変数で渡した（runbook の手順 5）。
  `deploy-testenv.sh` で再デプロイすると、デプロイは止まる。testenv の branch の `writeForgeToml` に `.local/forge.extra.toml` を足す仕組みが要る。
- **D4（runbook の誤り）**: `ops/runner/README.md` の手順 1 は `admin runner register … > .local/token` で、出力の表の全体がファイルに入る。
  `grep -oE 'fgr_…'` で 1 行にするよう直した。

### 4.4 GitStation 自身のパイプライン

- 前回（11-11）の Runner `docker-test-1` で、試験環境のプロジェクト `gitstation/gitstation` のパイプライン 4〜7（lint・test・generated）が成功している。
- 今回（main 3300cc30、新しい Runner `docker-test-m4`）: **履歴の取り込みは成功、パイプラインは 1 回動いて failed**（原因は CI の
  イメージが古いこと。Runner とジョブの仕組みは動いた）。時刻は UTC。
  - **入れ先**: `gitstation/gitstation`（project 4）は 11-11 の履歴の無い写し（別の履歴、ref あり）なので、取り込みの許可は使えない。
    空のプロジェクト `gitstation/gitstation-history`（project 14、repository 14）を作った（request_id `fc9498acd3bc4b168a8857d3389824f5`）。
    リポジトリが `ready` になるまで 6 分 6 秒かかった（`repository.provision` のジョブが、同じ時間に走っていた負荷試験の worker の
    キューの後ろに並んだ。普段は 1〜4 秒）。
  - **許可**: `forge admin repo allow-import 14 --ttl-hours 2`（request_id `56fcccbbc90f4b4aa552909a263e6ce8`、監査
    `repository.history_import_allowed`）。
  - **最初の push**（12:26:33〜12:26:40、request_id `7365b2cc64704843b4f6a822112d6709`）: 3300cc30 に「`tags: [linux, m4]`」だけを足した
    試験環境だけのコミット 9b01c587（branch `chore/ci-m4-self-runner`、1252 コミットの履歴ごと）。受け入れられた。履歴にだけある
    秘密の候補は 8 件（すべて `private_key`、合成の鍵のテスト用ファイル: `crates/forge-db/src/ci_outputs/tests.rs` 4、
    `crates/forge-pipelines/src/fingerprint_tests.rs` 2、`crates/forge-oidc/tests/fixtures/synthetic-rs256-key.pem` 1、
    `crates/forge-web/tests/single_file_commit_routes.rs` 1）。利用者には `remote: FG-GIT-0022 warning` の行、監査に
    `repository.history_imported` 1 件と `repository.history_secret_imported` 8 件。先端のツリーの検査は通った（3300cc30 の修正のとおり）。
    許可は使い切られた（`consumed_at` 12:26:38）。
  - **パイプライン 489**（`refs/heads/chore/ci-m4-self-runner`、sha 9b01c587、request_id は push と同じ）: **failed、36 秒**
    （12:26:41〜12:27:16）。3 つのジョブはすべて Runner `docker-test-m4` が取り（Runner のログ `forge.runner.job_started` /
    `job_finished`、job 516〜518）、`generated` 15 秒・`lint` 9 秒・`test` 11 秒で終了コード 101 / 101 / 102。ログの抜き出しは
    どれも `error: no matching package named `russh` found`（cargo はオフライン）。
  - **原因**: `.forge-ci.yml` が指すジョブのイメージ `registry:5000/ci/rust@sha256:e96601af…` は 11-11 のビルドで、その後に
    `Cargo.lock` に入ったクレート（`russh` は 11-14 の 9033307e、M4 C13）を持たない。`.forge-ci.yml` の先頭の注意
    （`Cargo.lock` が変わったら `node ops/ci-image/build-and-push.mjs` で作り直して digest を書き直す）が守られていない。
    これを確かめるゲートは無い。Runner・レジストリ・タグの選び方・サービスのコンテナ（`db` は起動した）は動いている。
  - **`main` の push**（12:27:23、新しいオブジェクトは無い）: 3300cc30 を `main` に置いた。そのパイプライン 490（`tags: [linux]`、
    request_id `ca68c080f9754022a23adf07bafd3fd9`）は重いジョブを 2 つ同時に走らせないため、すぐ取り消した（`canceled`、2 秒。
    取り消しの前に `docker-test-m4` と `docker-test-1` が 1 つずつ取り、どちらも終了コード 101 で 12:27:34 までに終わった。
    ロックを離した 12:27:27 の 7 秒後だが、cargo が依存の解決で止まるのでビルドは走っていない。取り消しの後のログの送信は
    FG-CI-0004 で断られる。仕様）。
  - cargo の枠のロック（`m4done-gitstation-ci`）は 12:26:33〜12:27:27 に持ち、trap で離した。その前の 1 回目（11:06:31〜11:06:33）は
    スクリプトのパスの形の誤り（Git Bash の `/tmp/…` を Windows の git に渡した）で push の前に終わり、何も送っていない。

## 5. 残り

1. GitStation 自身のパイプラインを新しい Runner で**緑に**する（4.4。1 回動いて failed）。要るもの:
   - CI のイメージを今の `Cargo.lock` で作り直し（`node ops/ci-image/build-and-push.mjs`。compose `gs-runner-test` の Runner の
     コンテナの Podman を通してレジストリに入れるので、その compose に触れる承認が要る）、`.forge-ci.yml` の digest を書き直す
     （製品の変更。別の branch）。その後、`gitstation/gitstation-history` に push してもう 1 回動かす（取り込みの許可は要らない。
     2 回目からは新しいオブジェクトだけを検査する）。
   - ゲート: `Cargo.lock`・`rust-toolchain.toml` が変わったのに `.forge-ci.yml` の digest が変わっていない変更を止める
     （今は注意書きだけ。`validate-*` か CI の確かめにする）。
2. 製品の修正: D1（Windows で `UserKnownHostsFile` の引用符）、D2（`ProgramData`）。修正の後は `sshwrap.exe` を外して確かめる。
   ssh の stderr の分類の結果（stderr そのものではなく、例えば `invalid quotes` のような種類）をログに残すと、今回のような
   `connection_lost` の誤った見え方を防げる。
3. D3: testenv の branch の `test-env.mjs` に `.local/forge.extra.toml` の仕組み。今は `deploy-testenv.sh` の後に runbook
   `m4-test-environment` の手順 5 をやり直す必要がある。
4. 計画 14 §8 の 2 の残り: prod の環境（人の承認、要求者・MR の作成者以外の Maintainer の Web のセッション）は今回は作っていない。
5. 最終の配置は PFS の Linux サーバー（利用者の決定）。そこでは D1・D2・forwarder は不要。
