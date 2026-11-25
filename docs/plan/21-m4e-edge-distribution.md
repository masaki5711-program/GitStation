# 21 M4E の計画 — エッジ向けの署名付きリリース（作成・署名・読み取りの API・公開の通知。機器の管理は別のアプリ）

- 作成: 2026-11-23（`planner` 相当）。**範囲の変更: 2026-11-25（§0）**。2026-11-23 版（機器の台帳・展開・機器のエージェントまで持つ形）は
  コミット `50e86273` の本文にある。
- 状態: **範囲の変更は利用者の決定（2026-11-25）**。E1 は `main` に入った（`bc58f07e`）。E4 の形と §11 の問いの既定で進めてよいかを聞く。
- ADR: **0041**（E1。2026-11-25 の Amendment で範囲を変えた）。
- 参照: 01 §3.10・§8（M4E）、04（承認付きデプロイ）、14（C6b の成果物、C11 の承認の形）、16（A1 の Ed25519）、ADR-0010・0013・0016・0018・
  0019・0031・0032・0041。

## 0. 範囲の変更（2026-11-25）

### 0.1 何が変わったか

| | 2026-11-23 版 | 2026-11-25 から |
|---|---|---|
| GitStation の役割 | リリース + 機器の台帳・登録・グループ・波・停止・承認・時間帯・機器のエージェント | **リリースの作成と署名だけ**（利用者の選択「リリースの作成と署名だけ」） |
| 機器の管理 | GitStation（`forge-fleet`・`forge-edge-agent`） | **別のエッジ管理・設定のアプリ**（利用者が作る） |
| 機器の資格情報 | `Credential::Device`（機器のトークン） | **作らない**。機器は GitStation に接続しない |
| GitStation とエッジのアプリの境目 | — | 署名したリリースの **読み取りの API** と、公開の **イベント / Webhook**（§3） |

### 0.2 なぜ

- 利用者はエッジ管理・設定のアプリを別に作っている。そのアプリが機器の台帳・登録・グループ・機器ごとの設定・展開の波・停止・承認・
  機器の上のエージェントを持つ。GitStation が同じものを持つと二重になる（入口は 1 つ、08 §1）。
- GitStation の強みは「保護されたタグ → パイプラインの成功 → 不変の成果物 → 署名 → 誰が出したかの記録」で、ここは既に CI（M4）と
  署名（ADR-0032・0041）がある。機器の都合（時間帯・稼働中か・戻し）は機器の近くのアプリが判断する方が正しい。

### 0.3 エッジのアプリへ移るもの（GitStation は作らない）

機器の台帳と登録、機器の資格情報、グループと機器ごとの設定、展開の波・停止の判定・再開・戻し、prod の展開の承認、保守の時間帯と
`can-update`、機器の上のエージェント（取得・確かめ・A/B・フック・自分での戻し・報告）、工場の中継、負荷試験とパイロットの機器の側。

### 0.4 GitStation に残るもの

1. **リリースの作成**: 保護されたタグ `edge/v*` → そのタグのパイプラインが成功 → 成果物を内容のハッシュの blob に → マニフェスト → **エッジ専用の鍵**で
   Ed25519 の署名（ADR-0041 の領域）。
2. **読み取りの API**: マニフェスト（署名の封筒のまま）・blob（`Range`、読むたびに sha256 を確かめ直す）・公開鍵。エッジのアプリはサービスアカウントの
   トークンで読む。
3. **公開の通知**: ドメインイベント `edge_release.published`（Outbox）と、既存の Webhook（M3 I2）での配信。
4. **監査**: 誰が（タグを push した人・パイプライン・手で作った人）いつ何を出したか・取り下げたか。

### 0.5 変わらない前提

| 出典 | 決定 |
|---|---|
| 契約 7 | 本番の変更は人の承認。**リリースの公開は展開ではない**（GitStation は機器に何も入れない）。prod の展開の承認はエッジのアプリが持つ |
| 契約 8 | オンプレだけ。エッジのアプリも社内 |
| ADR-0041 | マニフェストの形（`schema_version: 1`）、要約、署名の領域 `forge-edge-manifest-signature-v1\0`、`edge-release.toml` の lint、エッジ専用の鍵の一覧 |
| 17 §0.3 | PFS の AI ノードは C11 の SSH デプロイのまま |

## 1. 範囲 — 誰が何を持つか

| 持ち主 | 持つもの | 持たないもの |
|---|---|---|
| **GitStation** | `edge-release.toml` の lint、リリースの候補と作成、blob の置き場、マニフェストと署名、署名の鍵（ファイル、`forge admin edge-key`）、読み取りの API、`edge_release.published` / `.withdrawn`、監査、取り下げ、runbook | 機器・グループ・設定・展開・承認・時間帯・機器のエージェント・中継。機器のログ・指標 |
| **エッジ管理・設定のアプリ**（利用者が作る、別のリポジトリ） | §0.3 のすべて。GitStation の API をサービスアカウントで読む。マニフェストを **そのまま** 機器へ渡す（作り直さない） | GitStation の DB・鍵 |
| **機器** | 署名と blob のハッシュを確かめる（GitStation の公開鍵を固定）。確かめは ADR-0041 §5 の順 | GitStation への接続（資格情報を持たない） |
| **PFS** | PFE のリポジトリと `.forge-ci.yml`・`edge-release.toml` | — |

## 2. 依存の順

```
E0 [root] ✓ ─▶ E1 ✓（forge-edge: マニフェスト・要約・署名・lint。ADR-0041）
                └▶ E4a（リリースの作成・blob・署名）─▶ E4b（読み取りの API・イベント / Webhook）═▶ M4E の完了（GitStation の側）
E1 ─▶ E1b（任意、後片付け: 波・停止・時間帯を退役。§5.6）
E2・E3・E5・E6・E7・E8・E9: 取りやめ（エッジのアプリへ。§6）
```

前提: M4 の C6b（成果物、sha256）、M3 I2（Webhook）、ADR-0031 Amendment 1（保護ブランチからの到達）は入っている。E4b の後、エッジのアプリとの
**結合の確かめ**（§10-3）は向こうのリポジトリの試験で行う。

## 3. エッジのアプリとの境目（連携の契約）

| 方向 | 形 | 中身 |
|---|---|---|
| GitStation → アプリ（知らせ） | Webhook `edge_release.published` / `edge_release.withdrawn`（既存の Webhook の署名・再送・無効化） | `release_id`、`project`、`version`、`sequence`、`digest`、`key_id`、`source_sha`、`ci_pipeline_id`、`created_by`、封筒の URL。**マニフェストの本文は入れない**（知らせはヒント、正本は API） |
| アプリ → GitStation（取る） | `GET …/edge/releases?after_sequence=` で取りこぼしを埋める（Webhook が落ちても追い付く） | 一覧は `sequence` の順、カーソル |
| アプリ → GitStation（取る） | 封筒・blob・公開鍵の取得 | §5.3 |
| アプリ → 機器 | アプリの都合（波・時間帯・承認） | 封筒を **そのまま** 渡す。機器が GitStation の鍵で確かめる（アプリが改ざんしても通らない） |

**信頼の起点は GitStation のエッジの鍵**。アプリは配る経路で、信用しない（中継と同じ扱い）。アプリが blob を自分の置き場へ写すのは自由
（推奨。§8）。

## 4. 設計の選択肢と推奨（2026-11-25）

| 軸 | **A: 読み取りの API + Webhook（推奨）** | B: Webhook に全部を入れて押す | C: アプリに GitStation の DB / 置き場を読ませる |
|---|---|---|---|
| 内容 | 知らせは Webhook、取得は pull、取りこぼしは `sequence` で埋める | 封筒と blob の URL を押し、アプリが受けたら終わり | 直接読む |
| 取りこぼし | ○ 一覧で埋まる | × 再送が尽きると欠ける | ○ |
| 認可の単一点（契約 4） | ○ `authorize()` と権限の表 | ○（配信先の登録だけ） | **×** |
| 失敗の見え方 | ○ FG-EDGE と `request_id`、Webhook の配信の記録 | △ | × |
| 判断 | **採用** | 不採用（Webhook は知らせだけに使う） | 不採用 |

## 5. 設計の要点

### 5.1 クレートと依存（契約 9）

| クレート | 層 | 中身 |
|---|---|---|
| `forge-edge`（E1、純粋） | `forge-core → forge-edge → forge-agent → …` | マニフェスト・要約・署名・lint（使う）。波・停止・時間帯（退役の候補、§5.6） |
| `forge-edge-release`（E4a、新しい機能クレート。旧名 `forge-fleet`） | `forge-web / forge-cli → forge-edge-release → forge-auth / forge-db / forge-git → forge-core` | 候補・リリースの作成の worker・blob の置き場・署名の鍵の読み込み・取り下げ。他の機能クレートに依存しない（パイプラインと成果物の状態は `forge-db` の読み取り、タグは `repository.pushed` の購読） |
| ~~`forge-fleet`~~・~~`forge-edge-agent`~~ | — | **作らない**（2026-11-25） |

`clippy.toml` の `forge_edge::sign_manifest` の呼び出し元は `forge-edge-release` のリリースの作成だけ（E4a で名前を直す）。

### 5.2 リリースの作り方とマニフェスト（2026-11-23 版から変えない部分）

- **作り方**: `edge-release.toml`（E1 の lint）。保護されたタグ `edge/v*` の push を購読者（`repository.pushed`）が受け、候補（`pending`）を書く。
  worker のタスク `edge_release_builder`（30 秒）がタグのパイプラインの成功を待ち（失敗・取り消しなら `failed` と理由）、タグのコミットが保護ブランチから
  到達できることを確かめ、成果物を blob に移し（sha256 を確かめ直し、展開した各ファイルに `is_secret_like_path`）、マニフェスト → `manifest_digest` →
  `sign_manifest`。人が手で作る経路（`EdgeReleaseCreate`、Maintainer 以上の人の Web のセッション）も同じ候補の行を書き、同じタスクを通す。
- **マニフェスト・要約・署名・封筒**: ADR-0041 §2〜5 のまま（`schema_version: 1`）。`max_install_seconds`・`health_soak_seconds` は **エッジのアプリへの
  ヒント** として残す（署名に入っている。使うかはアプリが決める）。
- **鍵**: `edge.signing_keys_file`（ADR-0013 の形、`kid`、入れ替え）。部品（計画 16）の鍵と分ける。公開鍵は `GET /api/v1/edge/keys`。新しい鍵は
  「今信じている鍵で署名した鍵の一覧」で配る（機器が固定した鍵から辿れる）。
- **不変**: `edge_releases`・`edge_release_blobs` は追記だけ（ADR-0018 のトリガー）。同じ版の作り直しは FG-EDGE `ReleaseVersionExists`。
  タグを動かしても既存のリリースは変わらない。
- **取り下げ**（`EdgeReleaseWithdraw`）: 別の追記だけの表 `edge_release_withdrawals`（理由・人）。リリースは消さず、一覧と封筒の答えに
  `withdrawn` を付け、`edge_release.withdrawn` を出す。機器から消すかはアプリが決める。
- **blob の置き場**: `<storage.root>/edge-blobs/sha256/<2 文字>/<hex>`。読むたびにハッシュを確かめ直す（合わなければ FG-EDGE `BlobIntegrityMismatch`、
  alert、配らない）。同時の送信は `edge.blob_max_concurrent`（既定 16。読み手はアプリだけ）、超えたら 429 と `Retry-After`。
- **保持**: 各プロジェクトの新しい 10 リリースと、180 日以内のリリースの blob は消さない。それ以外は worker `edge_blob_expiry` が消す（マニフェスト・
  監査は残す。消した blob は 410 `blob_expired`）。§11 問い 7。

### 5.3 読み取りの API とイベント

| ルート | Action | 中身 |
|---|---|---|
| `GET /api/v1/projects/{id}/edge/releases?after_sequence=&limit=` | `EdgeReleaseRead` | 一覧（`sequence` の順、`withdrawn`、`digest`、`key_id`） |
| `GET /api/v1/projects/{id}/edge/releases/{release_id}` | `EdgeReleaseRead` | 封筒 `{manifest, digest, key_id, signature}`（`gstn edge release verify` が読む形と同じバイト列）。`ETag` = 要約 |
| `GET /api/v1/projects/{id}/edge/blobs/{sha256}` | `EdgeReleaseRead` | そのプロジェクトのリリースに含まれる blob だけ（他は 404）。`Range`、読みながら sha256 を確かめ、合わなければ途中で切って alert |
| `GET /api/v1/edge/keys` | `EdgeKeysRead` | 公開鍵の一覧（鍵で署名した一覧）。インスタンスの資源 |
| `POST /api/v1/projects/{id}/edge/releases` | `EdgeReleaseCreate` | 手で候補を作る（人の Web のセッション） |
| `POST …/edge/releases/{release_id}/withdraw` | `EdgeReleaseWithdraw` | 取り下げ（人、理由は必須） |

- **イベント**: `edge_release.published`・`edge_release.withdrawn`（`VisibilityClass::Integration` = プロジェクトの Reporter。Webhook の配信は既存の
  `WebhookDeliver` の判定）。1 リリースに 1〜2 件。
- `gstn edge release list|get|withdraw`（取得したファイルを `gstn edge release verify` でそのまま確かめられる）。

### 5.4 トークンと Action（ADR-0019 の形）

今のトークン（ADR-0019）はスコープで役割を絞る。`read_api` を持つ Reporter のトークンはプロジェクトのコードも読める。エッジのアプリのトークンが
漏れてもソースが読まれないよう、**新しいスコープ `edge_release_read`** を足す（`MergeExecute` と同じく **自分自身だけで満たされる**: `read_api`・
`api`・`admin` は満たさず、ほかのスコープも満たさない）。エッジのアプリは **サービスアカウント**（ADR-0019 の Amendment、`admin` は付かない）を
エッジのプロジェクト（またはそのグループ）の **Reporter** にし、`edge_release_read` だけのトークン（期限 365 日以内、入れ替えは runbook）で読む。

| Action | スコープ | allow | deny（代表。どれも権限の表に行） |
|---|---|---|---|
| **EdgeReleaseRead**（新規） | `edge_release_read` | プロジェクトの Reporter 以上の人の Web のセッション、`edge_release_read` のトークン（人・サービスアカウント、Reporter 以上） | `read_api` / `api` / `admin` だけのトークン（FG-AUTH-0002 `scope`）、メンバーでない・読めないプロジェクト → 404、エージェントのトークン（`is_agent`）、代理（act-as）、ポータルの資格情報、Runner、Job、ボット、無効の利用者・失効したトークン（401） |
| **EdgeKeysRead**（新規） | `edge_release_read` | サインインした有効な人、`edge_release_read` のトークン | 匿名、Runner、Job、ポータル、エージェント |
| **EdgeReleaseCreate** | —（セッションだけ） | system（保護されたタグの購読者）、Maintainer 以上の人の Web のセッション | トークン（`session_required`）、Developer、サービスアカウント、ボット、エージェント、代理、自動マージの主体、Runner、Job（ジョブはリリースを作れない） |
| **EdgeReleaseWithdraw** | —（セッションだけ） | Maintainer 以上の人の Web のセッション | トークン、Developer、サービスアカウント、ボット、エージェント、代理、Runner、Job |
| 署名の鍵の管理 | — | HTTP に出さない。`forge admin edge-key` だけ | — |

`EdgeReleaseCreate`・`EdgeReleaseWithdraw` は `is_session_only` と `is_people_only` に入れる。`EdgeReleaseRead` は `required_scope = Some(EdgeReleaseRead)`。
§11 問い 1 で `read_api` でも読めるようにする案を聞く。

### 5.5 データ（すべて追記のマイグレーション。`tables.toml` と doctor の期待値も更新）

| 表 | 種類 |
|---|---|
| `edge_release_candidates`（タグ、SHA、タグを push した人、パイプライン、状態 `pending` / `built` / `failed`、理由） | 派生 |
| `edge_blobs`（sha256、大きさ、置き場、最後に参照したリリース） | 派生 |
| `edge_releases`（プロジェクト、版、`sequence`、マニフェスト、要約、`kid`、署名、作った主体、パイプライン） | 追記だけ（トリガー）、監査あり |
| `edge_release_blobs`（リリース × blob） | 追記だけ |
| `edge_release_withdrawals`（リリース、人、理由、時刻） | 追記だけ、監査あり |

監査: `edge_release.created`（行為者 = system とタグを push した人・パイプライン、または人）、`edge_release.withdrawn`、`edge_signing_key.*`（CLI）。

### 5.6 `forge-edge`（E1）の各部の扱い

| 部分 | 推奨 | 理由 |
|---|---|---|
| マニフェスト・`parse_release`・`manifest_digest`・`verify_release`・`parse_edge_keys` | **残す** | E4 の作成、`gstn edge release verify`、仕様と試験の値としてエッジのアプリ・機器の確かめの正本 |
| `sign_manifest`・`EDGE_MANIFEST_DOMAIN`・`forge-agent` の領域の一般化 | **残す** | E4a が使う |
| `lint_release_definition`・`is_secret_like_path` | **残す** | E4a と `gstn` |
| payload の kind `agent` | **残す（意味を変える）** | 「エッジのアプリの機器のエージェント（単独のバイナリ）」として使える。形は変えない（署名した形を変えない）。§11 問い 9 |
| 欄 `max_install_seconds`・`health_soak_seconds` | **残す** | `schema_version: 1` の署名した欄。アプリへのヒント |
| 波（`WavePlan`・`wave_bucket`・`first_wave`・`in_wave`） | **退役** | 展開はアプリが持つ。GitStation に呼び出し元が無くなる |
| 停止の判定（`decide`・`HaltPolicy`・`WaveStats`） | **退役** | 同上 |
| 保守の時間帯（`Schedule`・`install_window`・`may_start_install`）と `jiff` | **退役** | 同上。退役で `gstn` とワークスペースから `jiff`（tz database 入り）が抜ける |
| FG-EDGE-0005 EdgeRolloutPolicyInvalid | **退役**（コードは再利用しない。errors.toml の退役の手順） | 波・停止・時間帯だけが出す |

**今はコードを消さない**。E1b（任意、`refactor/core-edge-retire-rollout`）で、エッジのアプリが Rust の `forge-edge` を
ライブラリとして使わないこと（§11 問い 3）を確かめてから消す。それまでは README と ADR-0041 の Amendment に「退役の候補、新しい呼び出し元を作らない」と書く。

### 5.7 エラーコード（仮。領域 `EDGE`、番号は登録の順で 0006 から）

| 仮の名前 | 内容 | 重さ | runbook |
|---|---|---|---|
| EdgeReleaseNotFromProtectedRef | タグのコミットが保護ブランチから届かない | info | — |
| EdgeReleaseVersionExists | 同じ版の作り直し | info | — |
| EdgeReleaseBuildFailed | パイプラインの失敗・成果物が無い・成果物のハッシュが合わない・シークレットらしいファイル（`reason`） | warn | `edge-release` |
| EdgeServerSigningKeyUnavailable | サーバーの鍵ファイルが無い・権限が緩い・DB の公開鍵と合わない | **alert** | `edge-signing-key` |
| EdgeBlobIntegrityMismatch | 置き場の blob のハッシュが合わない（配らない） | **alert** | `edge-release` |
| EdgeBlobThrottled | 同時の送信の上限（429） | info | — |
| EdgeBlobExpired | 保持の期限で消した blob（410） | info | `edge-release` |
| EdgeReleaseAlreadyWithdrawn | 取り下げの重複 | info | — |

認証の失敗は FG-AUTH-0001、拒否は FG-AUTH-0002（スコープ・セッション）/ 0003。`EdgeReleaseBuildFailed` は候補の行の理由にも残し、画面に
「なぜ作れなかったか・次に何をするか」の 1 行を出す。

## 6. MR の分け方

置き場所: **[root]** = エージェントモデル（先に入れる）、印なし = GitStation。各 MR の終わりは計画 14 §5 と同じ。評価役は 2 つまで。

| MR | 状態 | ブランチ | 中身 | 先に書く試験（例） | 評価役 | 前提 |
|---|---|---|---|---|---|---|
| **E0 [root]** | 済 | `chore/harness-m4e-edge-area` | 領域 `EDGE`、契約 9、`deploy-safety-reviewer` のエッジの観点 | — | — | — |
| **E1** | 済（`bc58f07e`） | `feat/core-edge-manifest` | `forge-edge`、署名の領域の一般化、`gstn edge release verify`、ADR-0041 | — | — | — |
| **E0b [root]** | 新規 | `chore/harness-edge-rescope` | 契約 9（`forge-fleet`・`forge-edge-agent` を消し `forge-edge-release`）、`deploy-safety-reviewer` の機器・波・時間帯の観点を「リリースの作成・読み取りの範囲」に置き換え、機器の eval 2 つを退役し `edge-release-read-token-reads-source`・`release-from-unprotected-tag` を足す | eval 2 つ | `harness-optimizer` | なし |
| **E4a** | 新規 | `feat/deploy-edge-releases` | `forge-edge-release`: 候補（`repository.pushed`）→ `edge_release_builder` → blob → 署名（`edge.signing_keys_file`、`forge admin edge-key`）、取り下げ、保持の worker、監査、doctor `edge.signing_key`・`edge.blob_store`、runbook `edge-release`・`edge-signing-key` の更新 | `release_only_from_protected_reachable_tag`、`release_waits_for_green_pipeline`、`failed_pipeline_marks_candidate_failed_with_reason`、`moved_tag_has_no_effect`、`release_rows_are_append_only`、`secret_like_file_in_artifact_refused`、`job_token_cannot_create_release`、`signed_release_verifies_with_gstn` | `deploy-safety-reviewer`、`security-reviewer` | E0b、E1、M4 C6b |
| **E4b** | 新規 | `feat/web-edge-release-read-api` | スコープ `edge_release_read`、Action `EdgeReleaseRead`・`EdgeKeysRead`・`EdgeReleaseCreate`・`EdgeReleaseWithdraw` と権限の表、§5.3 のルート（`Range`・429・410・読みながらの確かめ）、イベント `edge_release.published` / `.withdrawn` と Webhook、`gstn edge release list|get|withdraw`、`docs/api/agent-tools.toml` | `read_api_token_cannot_read_edge_release`、`edge_release_read_token_cannot_clone_or_browse`、`blob_of_other_project_is_404`、`blob_corruption_is_not_served`、`range_resume_matches_sha256`、`after_sequence_fills_missed_webhook`、`withdrawn_release_is_marked_not_deleted`、`publish_event_once_per_release` | `security-reviewer`、`authz-matrix-auditor` | E4a |
| E1b（任意） | 新規 | `refactor/core-edge-retire-rollout` | §5.6 の退役（波・停止・時間帯・`jiff`・FG-EDGE-0005 の退役） | `purity` の依存の一覧の更新 | `rust-reviewer` | §11 問い 3 の答え |
| ~~E2~~ | **取りやめ** | — | 機器の主体・登録・トークン → **エッジのアプリ** が機器の登録と資格情報を持つ | — | — | — |
| ~~E3~~ | **取りやめ** | — | 台帳・ラベル・グループ・時間帯 → **エッジのアプリ** が台帳・グループ・機器ごとの設定・時間帯を持つ | — | — | — |
| ~~E5~~ | **取りやめ** | — | 展開の状態機械・承認・望ましい状態・報告 → **エッジのアプリ** が波・停止・承認・戻しを持ち、GitStation のリリースを使う | — | — | — |
| ~~E6~~ | **取りやめ** | — | `forge-edge-agent` → **エッジのアプリ** の機器のエージェントが取得・署名の確かめ（GitStation の鍵）・入れる・戻すをする | — | — | — |
| ~~E7~~ | **取りやめ** | — | 台帳と展開の画面 → **エッジのアプリ** の画面。GitStation はリリースの一覧と取り下げだけ（E4b の範囲で既存の画面に足すかは §11 問い 8） | — | — | — |
| ~~E8~~ | **取りやめ** | — | 工場の中継 → **エッジのアプリ** が blob を写して配る（中継・キャッシュ） | — | — | — |
| ~~E9~~ | **取りやめ** | — | 偽の 10 000 台の負荷試験とパイロット → **エッジのアプリ** が機器の側で行う。GitStation の読み取りの量は §8 の小さな試験で足りる | — | — | — |

## 7. 運用で足すもの

- **doctor**: `edge.signing_key`（鍵の有無・権限・DB との一致）、`edge.blob_store`（空き・保持の worker の最後の成功）、`edge.release_candidates`
  （24 時間以上 `pending` の候補）。
- **メトリクス**: リリースの作成の数と失敗、blob の送信量・429・410、Webhook の配信の失敗（既存）。
- **ログ**: すべての失敗に `request_id`・`error_code`、エッジは `release_id`・`project_id`・`digest`。鍵・トークンは出さない。
- **runbook**: `edge-release`（新規: 候補が作られない・失敗した理由、blob の破損、保持）、`edge-signing-key`（入れ替え、漏えいのとき: 失効 → 新しい鍵 →
  エッジのアプリに知らせ、使っているリリースを作り直す）、`edge-agent`（FG-EDGE-0004。機器の手順は書かず「エッジのアプリの runbook へ」に直す）。

## 8. 規模の想定（推定）

| 量 | 想定 | 計算 |
|---|---|---|
| 読み手 | エッジのアプリ 1〜2 台（冗長）。**機器は GitStation に来ない** | — |
| リリース | 1 日に数件、1 リリース 200 MB（変わる層 20 MB） | 作成の worker は 1 件 ≤ 数分 |
| blob の読み | アプリが blob を写すなら 1 リリースにつき 1 回 | ≤ 1 GB / 日。同時 16 で足りる |
| 一覧の読み | アプリが 1〜5 分ごと（取りこぼしの埋め） | ≤ 1 件 / 秒 |
| Outbox / 監査 | 1 リリースに数件 | 機器の数に比例しない |

試験: E4b の結合の試験で 200 MB の blob の `Range` の再開と、同時 16 + 429 を確かめる。アプリが blob を写さず機器の代わりに毎回取りに来る形は
想定しない（§11 問い 5）。

## 9. 危険と対策

| 危険 | 対策 |
|---|---|
| 改ざんしたプログラムが機器に入る | エッジ専用の鍵と領域の署名、blob のハッシュ。アプリは信用しない経路（機器が GitStation の鍵で確かめる） |
| アプリのトークンの漏えいでソースが読まれる | スコープ `edge_release_read` は自分だけで満たされ、コード・API を読めない。サービスアカウントは Reporter、`admin` なし、すぐ失効できる |
| 保護されていないコミットがリリースになる | 保護されたタグ + 保護ブランチからの到達 + パイプラインの成功。ジョブはリリースを作れない |
| 悪い版を出してしまった | 取り下げ（`edge_release.withdrawn`）。消さずに印を付け、機器から消すかはアプリ |
| Webhook の取りこぼし | `after_sequence` の一覧で埋める |
| 署名の鍵の漏えい | 鍵は worker のホストのファイルだけ、`kid` の入れ替えと失効、runbook `edge-signing-key` |

## 10. 完了の条件（01 §8 の M4E。GitStation の側）

1. 保護されたタグの push から、パイプラインの成功のあとに署名したリリースができ、`gstn edge release get` で取った封筒が `gstn edge release verify --blobs`
   で通る。保護されていないタグ・失敗したパイプライン・ジョブのトークンからはできない。
2. `edge_release_read` のトークンでリリース・blob・鍵を読め、同じトークンでコード・他の API は読めない（権限の表の行）。
3. エッジのアプリ（または同じ API を使う試験の偽のアプリ）が Webhook を受け、取りこぼしを一覧で埋め、封筒を機器の確かめに渡せる。
4. 新しいルート・Action・スコープはすべて権限の表にあり、カタログ・runbook・ADR-0041 の Amendment がそろい、Windows でも全体の試験が緑。

**確かめのコマンド**: 計画 11 §9 と同じ（fmt・clippy `-D warnings`・nextest・`gen-catalog` / `gen-authz` の差分なし・`node scripts/self-heal.js --check`）。

## 11. 利用者に聞くこと（それぞれ推奨の既定）

2026-11-23 版の問い 1〜12 のうち、台数・回線・稼働の予定・承認の単位・フック・機器の身元・しきい値（1・4・5・6・8・9・10・12）は
**エッジのアプリの問い** になった。署名の鍵を分ける（7）は「分ける」のまま。OS・CPU（2）と配るものの形（3）は manifest の `arch`・`os`・
`payloads[].kind` に残る（Ubuntu 24.04、`amd64` / `arm64`、`oci-image` と `files`）。

| # | 問い | 推奨の既定 |
|---|---|---|
| 1 | **エッジのアプリはどう認証するか** | サービスアカウント（エッジのプロジェクトかそのグループの Reporter）+ スコープ `edge_release_read` だけのトークン（365 日、runbook で入れ替え）。`read_api` で読める形にはしない（漏えいでソースが読まれる） |
| 2 | **pull か Webhook か** | 両方。Webhook `edge_release.published` は知らせだけ、正本は API。アプリは 5 分ごとに `after_sequence` の一覧で取りこぼしを埋める |
| 3 | **アプリが読むマニフェストの形** | ADR-0041 の封筒 `{manifest, digest, key_id, signature}`（`schema_version: 1`、JSON）をそのまま。GitStation は形の説明と試験の値（`docs/edge/release-format.md`、署名の通る例・通らない例）を出す。アプリが Rust なら `forge-edge` をライブラリとして使ってもよいが、既定は「形の説明と試験の値で各自が実装」（GitStation のクレートに外の依存を作らない） |
| 4 | **署名を確かめるのは誰か** | **機器**（GitStation の鍵を導入のときに固定）。アプリも受け取るときに確かめる。アプリは署名し直さない（信頼の起点を 1 つにする） |
| 5 | **blob を機器へどう配るか** | アプリが自分の置き場へ写し、機器はアプリから取る。機器は GitStation に接続しない |
| 6 | 取り下げを作るか | 作る（E4a・E4b。小さい）。消さずに印、イベント `edge_release.withdrawn` |
| 7 | blob の保持 | プロジェクトごとに新しい 10 リリース + 180 日。それより古い blob はアプリの置き場に頼る |
| 8 | GitStation の画面にリリースの一覧を出すか | 出す（プロジェクトの「リリース」の 1 画面: 版・作った人・パイプライン・取り下げ）。展開の様子はアプリの画面へのリンク（設定で URL） |
| 9 | payload の kind `agent` | 残し、「エッジのアプリの機器のエージェント」に使ってよい。使わないなら次の `schema_version` で外す |
| 10 | 波・停止・時間帯の退役（E1b） | 問い 3 の答えの後に消す。それまで呼び出し元を作らない |
| 11 | prod 向けのリリースに GitStation での人の承認を足すか | 足さない。リリースは展開ではなく、prod の展開の承認はアプリが持つ（契約 7 はアプリの側で守る）。保護されたタグを push できる人を絞るのが GitStation の側の統制 |
