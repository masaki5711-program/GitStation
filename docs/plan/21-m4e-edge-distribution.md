# 21 M4E の計画 — 工場のエッジ機器への署名付き・段階的な配布（pull 型、自動停止、稼働中は更新しない）

- 作成: 2026-11-23（`planner` 相当。GitStation は `main` 1bb6a535 を読み取りだけで確かめた。PFS のリポジトリは読んでいない）
- 状態: **ドラフト（利用者の承認待ち）**。§3 の推奨と §11 の問いの既定で進めてよいかを聞く。§11 の 1〜6 は 01 §8「2026-10-28 追加」の 6 つの問いの答えを兼ねる。
- 参照: 01 §3.4・§3.10・§8（M4E）・§8 の 2026-10-28 の問い、04（承認付きデプロイ、stg / prod）、05（Q5: 機器の行為者の記録は M4E の ADR へ）、07 W13（Kanflow からのデプロイ、保守の時間帯）、09（PFS のポータルと Keycloak）、14（C11 の SSH デプロイ、環境・承認・凍結・ロールバック）、16（A1 の Ed25519）、17 §0.3（AI ノードは C11 を使い M4E は使わない）、18（負荷試験の型）、ADR-0010・0013・0018・0029・0031・0032。

## 0. 前提の点検

### 0.1 目標（1 文）

CI が作った不変のリリースに GitStation が署名し、工場の数千台のエッジ機器が自分から HTTPS で取りに来て、署名を確かめ、**保守の時間帯の中で、かつ設備が止まっているときだけ** 入れ、失敗すれば自分で前の版に戻り、失敗が増えれば展開そのものが自動で止まる — その展開（カナリア → 割合 → 全台）を、prod では人の承認から始める。

### 0.2 決まっていること（議論し直さない）

| 出典 | 決定 |
|---|---|
| 01 §3.10（2026-10-28） | pull 型（エッジは外から接続を受けない）。機器は `authorize()` の主体になり、権限の表に行を持つ。署名付きの不変のリリース。カナリア → 割合 → 全台、しきい値で自動停止。保守の時間帯。前の版を残して自分で戻る。prod は人の承認。工場ごとの中継は Should、オフラインの更新は M8、機器のログ・指標の収集は Won't |
| 01 §3.10 の設計の制約 | 機器の定期の接続（心拍）は `events` / `audit_log` に入れない。台帳の行を上書きし、記録するのは状態の変化だけ。ジョブキューに機器ごとのジョブを積まない。エラーの領域 `EDGE` はエージェントモデルの側で先に足す |
| 05 Q5 | 機器の行為者の記録は M4E の ADR で決める。機器は `UserKind` ではない（試験 `a_device_is_not_a_user_kind`）。`edge`・`fleet`・`devices` は予約した名前（`forge-core/src/namespace_path.rs`） |
| 14 §9-8、ADR-0031 | prod の承認は Maintainer 以上の人の Web のセッションだけ。要求者と、その SHA の背後の人（MR の作成者・push した人・author / committer）は承認できない。デプロイできるのは保護ブランチから到達できるコミットだけ |
| 17 §0.3 | PFS の AI ノード（数台）は C11 の SSH デプロイを使い、M4E は使わない。**この計画は変えない** |
| 契約 7・8 | 本番の変更は人の承認（承認済みの runbook の手順は除く）。オンプレだけ、外部のサービスに頼らない |

### 0.3 コードで確かめたこと（`main` 1bb6a535）

- `forge-deploy`（C11）は forge の worker から SSH で **押す** 形。待ちのデプロイを `FOR UPDATE SKIP LOCKED` で 1 つずつ動かす（`crates/forge-deploy/README.md`）。数千台・NAT の内側・機器の都合の時間帯には合わない（§3）。ただし環境・承認・凍結・「背後の人は承認できない」の判定（`forge-auth/src/deploy_ctx.rs`）は形を借りる。
- `Credential` は `Session`・`Token`・`ActAs`・`Runner`・`Job`・自動マージの主体（`forge-auth/src/model.rs`）。Runner と Job は `job.rs` の自分の腕で判定する。**機器も同じ形で `Credential::Device` を足す**。
- 署名は `forge-agent/src/sign.rs`（純粋、`ring` の Ed25519、`KeyRing`、`verify_signature`、`sign_digest`）。ただし署名の領域の文字列が定数 `SIGNATURE_DOMAIN = "forge-agent-part-signature-v1\0"` に固定されている。エッジのマニフェストには **別の領域** が要る（同じ鍵の署名を流用させない）。
- 秘密の鍵のファイル（ADR-0013）、CI の成果物（C6b、`ci_artifacts`、sha256 を持つ）、保護ブランチからの到達の判定（ADR-0031 Amendment 1）、`ops/loadtest/` の負荷試験の型（計画 18）は既にある。
- `forge-errors/build.rs` の AREAS に **`EDGE` は無い**（14 個。`AGENT`・`AIASSET` まで）。
- コンテナのレジストリは GitStation に無く（01 §3.9 で範囲の外）、社内 GitLab のレジストリは壊れている（PFS の試験サーバーのデプロイはサーバーの Docker でイメージを作っている）。

### 0.4 前提への意見

1. **GitStation が持つもの・持たないもの**を最初に分ける（§1）。GitStation は「リリースの正本・署名・台帳・展開の判断・承認・記録」と **機器の上の汎用のエージェント `forge-edge-agent`** を持つ。PFE のコンテナをどう止めてどう起こすか、設備が今動いているかは **機器の側のフック**（PFS が書く）が答える。GitStation のコード・試験は PFS のリポジトリに依存しない。
2. **01 §3.10 の「機器ごとの証明書による相互 TLS」は、最初は機器ごとのトークンにする**ことを推奨する（§4）。理由: 今の GitStation は TLS を前段の reverse proxy に任せており、相互 TLS は証明書の発行・失効の運用（社内 CA）を GitStation の外に新しく作る。トークンなら PAT（ADR-0019）と同じハッシュの保存・失効・入れ替えの仕組みで、Claude がログと表だけで診断できる。改ざんはマニフェストの署名で防ぎ、トークンの盗聴は TLS で防ぐ。相互 TLS は M8 の選択肢に残す（§11 問い 10）。
3. **「稼働中は更新しない」は時間帯だけでは守れない**。計画どおり止めた日でもラインが動いていることがある。サーバーの時間帯（§5.4）と、入れる直前の機器のフック `can-update`（今止まっているか）の **両方** が許したときだけ入れる。
4. **差分の配布は「ファイル・層の単位で持っていないものだけ取る」で足りる**。内容のハッシュで名前を付けた塊（blob）にすれば、変わらない層は取りに来ない。バイナリの差分（bsdiff など）は作らない（M8 で需要を見て）。
5. 既製品（Eclipse hawkBit、Mender）は §3 の案 D として比べた。考え方（展開のグループ、失敗のしきい値、DDI の pull の API、保守の時間帯）は借りるが、製品は入れない。

## 1. 範囲 — 誰が何を持つか

| 持ち主 | 持つもの | 持たないもの |
|---|---|---|
| **GitStation（サーバー）** | 機器の台帳と登録・トークン、グループ、リリース（CI の成果物 → マニフェスト → 署名、内容のハッシュの blob 置き場）、展開の状態機械・承認・時間帯・停止の判断、望ましい状態の API、報告の受け取り、画面、監査、runbook | 機器のログ・製造データ・指標の収集（01 で Won't）、OS の更新、PLC のプログラムの書き込み |
| **`forge-edge-agent`（GitStation の製品、機器の上の常駐）** | ポーリング、署名の確かめ、blob の取得（途中から再開）、置き場（A/B: 今の版と前の版）、フックの呼び出し、健全性の確認と自分での戻し、報告、トークンの入れ替え、自分自身の更新 | PFE の中身の知識。設備の状態の判断（フックに聞く） |
| **機器のフック（PFS が PFE と一緒に配る）** | `can-update`（今入れてよいか）、`install`（例: `docker load` → `docker compose up -d`）、`health`、`rollback` | GitStation の API を直接呼ばない |
| **PFS** | PFE のリポジトリと `.forge-ci.yml`・`edge-release.toml`、機器の用意（エージェントの初回の導入と登録のトークンの投入）、中継の置き場所 | GitStation の DB・鍵 |

**範囲の外**: AI ノード（17、C11 のまま）、Kanflow からの展開のボタン（07 W13 の形で後から。承認は GitStation の Web のセッションのまま）、USB のオフラインの更新（M8）、設備の仕様書との結び付け（台帳のラベルに仕様書のパスを持てるようにするだけ。画面の結び付けは Could）。

## 2. 依存の順

```
E0 [root] ─▶ E1（forge-edge: マニフェスト・署名・波・停止の判定。ADR-0039）
             ├▶ E2（機器の主体・登録・トークン）─▶ E3（台帳・グループ）
             └▶ E4（リリース・blob・署名の worker）
E3 + E4 ─▶ E5（展開の状態機械・望ましい状態・報告・評価の worker）─▶ E6（forge-edge-agent）─▶ E9（負荷試験・パイロット）═▶ M4E の完了
E5 ─▶ E7（画面）     E6 ─▶ E8（工場の中継、Should）
```

前提: M4 の C6b（成果物）・C11（デプロイ。承認の判定の形）は入っている。計画 17 の P17-7 と E1 はどちらも署名の領域を一般化する。先に入った方が `sign.rs` を変え、後の方はそれを使う。

## 3. 設計の選択肢と推奨

| 軸 | **A: pull の API + 署名したマニフェスト + 自前のエージェント（推奨）** | B: OCI レジストリで配る | C: C11 のデプロイのターゲットを使う | D: 既製品（hawkBit / Mender）を置く |
|---|---|---|---|---|
| 内容 | 機器が `GET /api/v1/edge/desired` を数分ごとに聞く（ETag で多くは 304）。サーバーが展開の波・時間帯から答え、機器はマニフェストの署名を確かめて blob を取る。展開の進み具合は機器の報告と台帳から決める | GitStation の横にレジストリ（distribution など）を置き、リリースを OCI の成果物にし、機器は pull。展開の判断は GitStation の API | 機器ごとに `deploy_targets` を作り、worker が SSH で押す | 配布と展開を既製品に任せ、GitStation は CI のリリースを渡すだけ |
| pull / NAT | ○ | ○ | **×** 押す形。工場の NAT の内側に届かない | ○ |
| 数千台 | ○ 状態は台帳の上書きと集計 1 回。機器ごとのジョブなし | ○（配る部分） | **×** worker が 1 つずつ、SSH の鍵を数千台に | ○ |
| 認可の単一点（契約 4） | ○ すべて `authorize()` | △ レジストリの認可は別の仕組み（トークンの写し） | ○ | **×** 機器・展開の認可が外の製品にある |
| 失敗の見え方（契約 1・2） | ○ すべて FG-EDGE、`request_id`・`device_id`・`rollout_id` | △ レジストリのログは別の形 | ○ | **×** 外の製品のログと DB（Java / MongoDB など）。Claude がコードから追えない |
| 稼働中は更新しない | ○ 時間帯 + 機器のフック | ○（同じ API を作れば） | × 機器の都合を聞けない | ○（hawkBit は時間帯あり） |
| 新しく動かすもの | エージェント 1 つ（Rust、GitStation のリポジトリ） | レジストリ + エージェント | なし | 別のサーバー・DB・運用 |
| 判断 | **採用** | 今は不採用。イメージの層の配布は A の blob で同じ効果。社内のレジストリが直ったら、blob の取り先にレジストリを足すのは A の拡張でできる | 不採用（01 §3.10 が既に除外） | 不採用。展開のグループ・しきい値・時間帯の考え方だけ借りる |

**推奨: 案 A**。配るものの形は次の 3 種類から始める（マニフェストの `payloads[].kind`）。

| kind | 中身 | 機器での入れ方 |
|---|---|---|
| `files` | ファイルの組（設定・スクリプト・実行ファイル）。各ファイルが blob | エージェントが `releases/<digest>/` に置き、フック `install` に渡す |
| `oci-image` | OCI の image layout（index・manifest・config・層）。各層が blob | エージェントが layout を組み立て、フック `install` が `docker load`（OCI の tar）と `docker compose up -d` |
| `agent` | `forge-edge-agent` 自身 | エージェントが新しいバイナリを置き、systemd で入れ替え、起動の確認に失敗したら前のバイナリに戻す |

## 4. 機器の身元と認証

### 4.1 登録

1. 管理者が登録のトークンを作る（`EdgeEnrollmentTokenCreate`、人の Web のセッション）。トークンはサイト（工場）とラベル（ライン・設備・機種）に結び付き、**使える回数**（既定 1、まとめての用意には最大 500）と **期限**（既定 72 時間）を持つ。平文は作成のときに 1 度だけ見せ、DB にはハッシュだけ。
2. 機器の用意（PFS の手順）で、エージェントの設定に登録のトークンとサーバーの URL・CA を入れる。
3. エージェントは最初の起動で `POST /api/v1/edge/enroll`（`machine_id`（`/etc/machine-id`）、ホスト名、CPU の種類、OS、エージェントの版）。サーバーは `device_id` と **機器のトークン** を返す。登録のトークンの回数を 1 減らし、`edge_device.enrolled` の監査の行を書く。
4. 同じ `machine_id` の有効な機器が既にあれば断る（FG-EDGE `DeviceDuplicate`）。入れ替えた機器は管理者が古い行を廃止してから登録し直す。

### 4.2 機器のトークンと入れ替え

- 形: `gse_` + 256 ビットの乱数。DB には SHA-256 だけ（`edge_devices.token_hash`）。期限 90 日。
- **自動の入れ替え**: 期限の 3 分の 2 を過ぎたら、エージェントが `POST /api/v1/edge/token/rotate` で新しいトークンを受け取る。古いトークンは「新しいトークンが 1 度使われる」か「24 時間」の早い方まで有効（入れ替えの途中で切れても機器が締め出されない）。入れ替えは監査の行 `edge_device.token_rotated`。
- 期限が切れた機器は 401（FG-AUTH-0001 `reason=device_token_expired`）。戻すのは管理者の「再発行」（新しい登録のトークン、同じ `device_id`）。
- **失効**: 管理者が機器を無効にすると、次の要求から 401。盗まれた機器は無効にし、`machine_id` も拒否の一覧に入れる。
- トークンが漏れても、できるのは「その機器に割り当てられたリリースを読む・その機器として報告する」だけ（§6）。
- 機器の経路は **TLS が必須**。`edge.require_tls`（既定 `true`）のとき、前段の proxy が付けた `X-Forwarded-Proto: https` の無い機器の要求は断る（試験環境だけ `false`）。doctor `edge.tls` が設定を確かめる。

### 4.3 主体と監査（05 Q5 の答え）

- `Credential::Device(DeviceCredential { device_id, site_id })`。forge-web の認証だけが作り、`authorize()` の自分の腕（`forge-auth/src/device.rs`、`job.rs` と同じ形）で判定する。
- 監査の行為者は `kind = device`、`device_id`。`users` の行は作らない（`a_device_is_not_a_user_kind` を保つ）。
- 機器のポーリング（心拍）は監査にもイベントにも入れない。監査に入るのは登録・入れ替え・無効化・ラベルの変更（人の操作）だけ。機器ごとの版の変化は §5.6 の専用の表に入れる。

## 5. 設計の要点

### 5.1 クレートと依存（契約 9 を変える → root の E0 が先）

| クレート | 層 | 中身 |
|---|---|---|
| `forge-edge`（新規、純粋） | `forge-core → forge-edge → forge-agent → forge-ci → forge-doc → forge-errors` | マニフェストの形・決まった並びの要約・署名の確かめ（`forge-agent::sign` を領域つきに一般化して使う）、`edge-release.toml` の lint、波の割り当て、停止の判定、時間帯の計算。I/O・時計・DB なし（時刻は引数） |
| `forge-fleet`（新規、機能クレート） | `forge-web / forge-cli → forge-fleet → forge-auth / forge-db / forge-git → forge-core` | 台帳・登録・リリースの作成・blob の置き場・展開の状態機械・評価の worker。他の機能クレートには依存しない（成果物とパイプラインの状態は `forge-db` の読み取り。タグの push はドメインイベント `repository.pushed` の購読で受ける。パイプラインの完了のイベントは今は無い） |
| `forge-edge-agent`（新規、バイナリ） | `forge-errors`・`forge-edge`・`forge-client` だけ。サーバーのクレートに依存しない。何もこれに依存しない | 機器の常駐（`forge-runner` と同じ置き方） |

`forge-client`（`gstn`）は `forge-edge` に依存してよい（ADR-0020 の Amendment。`gstn edge release verify` で手元でマニフェストを確かめるため）。

### 5.2 リリースとマニフェスト

- **作り方**: リポジトリの `edge-release.toml`（配るもの: `payloads` の kind と CI の成果物のパス、対象の `arch`（`amd64` / `arm64`）と `os`（`ubuntu-24.04`）、`max_install_seconds`、`health_soak_seconds`）。**保護されたタグ** `edge/v*` の push を購読者（`repository.pushed`、C12 の `spec.publish` と同じ入口）が受けて、リリースの候補の行（`pending`）を書く。worker のタスク `edge_release_builder`（30 秒）が、そのタグのパイプラインが成功した候補だけを取り（失敗・取り消しなら候補を `failed` にして理由を残す）、次を行う。タグのコミットが保護ブランチから到達できることを確かめ（ADR-0031 Amendment 1 と同じ判定）、成果物を blob に移し（sha256 を確かめ直す）、マニフェストを作って worker の鍵で署名する。人が手で作る経路（`EdgeReleaseCreate`、Maintainer 以上の人）も同じ候補の行を書き、同じタスクを通す。
- **マニフェスト**（`schema_version: 1`）: `release_id`、`project`、`version`、`source_sha`、`sequence`（プロジェクトの中で単調に増える）、`created_at`、`arch`・`os`、`payloads[]`（`kind`、`blobs[]` = `sha256`・`size`・`path`）、`max_install_seconds`、`health_soak_seconds`、`ci_pipeline_id`。
- **要約と署名**: ADR-0032 と同じ作り方（可変長の欄に長さ、blob はハッシュの順）で `"forge-edge-manifest-digest-v1\n"` から始まる SHA-256。署名の文は `"forge-edge-manifest-signature-v1\0"` ‖ 要約。鍵は **エッジ専用の鍵ファイル** `edge.signing_keys_file`（ADR-0013 の形、`kid`、入れ替え。§11 問い 7）。公開鍵は `GET /api/v1/edge/keys` で配り、エージェントは導入のときの鍵を手元に固定し、新しい鍵は「今信じている鍵で署名された鍵の一覧」だけで受け入れる。
- **不変**: `edge_releases`・`edge_release_blobs` は追記だけ（ADR-0018 のトリガー）。同じ版の作り直しは FG-EDGE `ReleaseVersionExists`。タグを動かしても既存のリリースは変わらない。
- **blob の置き場**: `<storage.root>/edge-blobs/sha256/<2 文字>/<hex>`。読むときにハッシュを確かめ直す（合わなければ FG-EDGE `BlobIntegrityMismatch`、alert、配らない）。`GET /api/v1/edge/blobs/{sha256}` は `Range` に対応（途中から再開）。同時の送信は `edge.blob_max_concurrent`（既定 64）まで、超えたら 429 と `Retry-After`。
- **シークレットを入れない**: lint が `edge-release.toml` の対象から `*.env`・鍵らしいファイル名を断る。機器ごとの秘密は機器の側（PFS の用意）に置く。
- **保持**: 展開に使ったリリースと、各グループの今の版・前の版の blob は消さない。それ以外は 180 日で消す（worker の `edge_blob_expiry`）。

### 5.3 グループと波

- **グループ**: ラベルの選び方（例 `site=bekasi AND line=L3 AND model=pfe-gw2`）で決める。機器のラベルが変わったときにだけ所属の表 `edge_group_members` を作り直す（ポーリングのたびに計算しない）。1 台は 1 つのプロジェクトについて **1 つのグループにだけ** 入れる（重なれば FG-EDGE `GroupOverlap` で保存を断る）。
- グループはプロジェクトに結び付ける（`EdgeGroupBind`、管理者）。プロジェクトはそのグループにだけ展開できる。
- **グループの環境**: `stg`（試験のライン）か `prod`。prod は承認が要る（§5.5）。
- **波**: 展開ごとに波の列を持つ。既定は `[canary, 5 %, 25 %, 100 %]`。canary はグループの中のラベル `edge.canary=true` の機器（既定は 1 ラインの 5 台以内）。割合の波は `bucket = u32(SHA-256("forge-edge-wave-v1" ‖ rollout_id ‖ device_id)[0..4]) mod 10000` が `割合 × 100` 未満の機器。**単調**（5 % の機器は 25 % にも入る）で、同じ展開の中では再計算しても変わらない。
- 機器の種類が合わない（`arch`・`os`）機器は展開から外し、画面に数を出す（失敗に数えない）。

### 5.4 保守の時間帯と「稼働中は更新しない」

1. **サーバーの時間帯**: グループごとの週の繰り返し（曜日・開始・長さ、IANA のタイムゾーン）と、例外の禁止期間（`tstzmultirange`、C11 の凍結と同じ形）。望ましい状態の答えに `install_window: { not_before, not_after }` を入れる。
2. **時間帯の外**: 取得と確かめ（ダウンロード・署名・置き場への展開）はしてよい。**入れない**。
3. **入れる直前**: 残りの時間帯が `max_install_seconds` より短ければ始めない。フック `can-update` を呼び、終了コード 0（止まっている）のときだけ入れる。0 以外は `deferred`（FG-EDGE `AgentUpdateDeferred`、info）を報告し、時間帯の中で 1 分ごとに聞き直す。フックが無い・時間切れ（30 秒）は **入れない側** に倒す（`can-update` の失敗で入れることはない）。
4. 入れている途中で時間帯が終わっても止めない（止める方が危ない）。`max_install_seconds` を超えたら失敗として戻す。

### 5.5 展開の状態機械

展開（`edge_rollouts`）= リリース × グループ × 波の計画 × 停止の規則。

```
draft ─(提出)→ pending_approval ─(承認)→ scheduled ─(時間帯)→ running(wave k)
  │                 │(期限 7 日)→ expired                    │  ├─(波の条件を満たす)→ running(wave k+1) … → completed
  │                 └(却下)→ rejected                         │  ├─(人の一時停止)→ paused ─(再開: 承認と同じ権限)→ running
  └(取り消し)→ canceled                                        │  └─(停止の条件)→ halted
halted ─(再開: 承認と同じ権限、理由の記入)→ running
halted / paused ─(前の版に戻す)→ reverting ─→ reverted       halted / paused ─(打ち切り)→ aborted
```

- stg のグループは `pending_approval` を飛ばす（承認の規則を持たせることはできる）。
- **prod の承認**: `EdgeRolloutApprove`。Maintainer 以上の人の Web のセッションだけ。展開を作った人と、そのリリースの SHA の背後の人（ADR-0031 Amendment 2 と同じ判定）は承認できない。承認は **波の計画全体** に対するもの（各波の前に毎回は聞かない。§11 問い 8）。
- 1 つのグループで `running`・`paused`・`halted` の展開は 1 つだけ（部分の一意な索引）。
- **前の版に戻す**（`reverting`）: 「そのグループで最後に `completed` になった展開のリリース」を目標にした逆向きの展開を作る。その版は既に人が承認したものなので、prod でも **人の 1 回の操作（Maintainer 以上の Web のセッション）** で始めてよく、2 人目の承認は要らない（17 の決定 2 と同じ考え方。§11 問い 8）。時間帯と `can-update` はそのまま守る。
- 状態の変化は監査の行（`edge_rollout.*`）と、ドメインイベント `edge_rollout.state_changed`（Outbox。1 展開あたり十数件）。機器ごとのイベントは出さない。

**機器ごとの状態**（`edge_rollout_devices`、(rollout, device) ごとに 1 行を上書き）:

```
offered → downloading → staged → installing → verifying ─(健全 health_soak_seconds)→ succeeded
                │            │(時間帯の外 / can-update が拒む)→ deferred → installing
                └────────────┴─(取得・署名・入れる・健全性の失敗)→ failed → rolled_back | rollback_failed
offered … (期限まで報告なし) → unresponsive
```

- `failed` のあと、エージェントは **自分で前の版に戻す**（フック `rollback`、A/B の前の版。人の承認は要らない: 前の版は動いていた版で、機器の中だけの戻し）。戻せなければ `rollback_failed`（機器の状態が分からない）。
- `unresponsive` の期限 = その機器の時間帯の終わり + 2 時間。オフライン（24 時間以上ポーリングが無い）の機器は分母から外し、一覧に出す。

### 5.6 停止の判定と進む条件（`forge-edge` の純粋な関数 `decide(wave_stats, policy, now)`）

| 規則（既定、展開ごとに変えられる） | 結果 |
|---|---|
| `rollback_failed` が 1 台でも | **halt**（`reason=rollback_failed`）。すぐに |
| canary の波で `failed` が 1 台でも | **halt**（`reason=canary_failure`） |
| 割合の波で `failed + unresponsive ≥ 3` かつ 失敗の割合（失敗 ÷（成功 + 失敗））> 2 % | **halt**（`reason=failure_rate`） |
| 波の `unresponsive` が 試みた機器の 10 % を超える | **halt**（`reason=unresponsive_rate`） |
| 波の機器の 90 % 以上が終わりの状態、上の規則に当たらない、見守りの時間（canary 24 時間、他 4 時間）が過ぎた | **advance** |
| 試みた機器が `min_sample`（既定 3）未満 | **wait**（止めもしないし進みもしない） |
| 波の開始から 7 日たっても進めない | `RolloutStalled`（warn）を出し、画面に「何が足りないか」の 1 行 |

- 停止は自動、再開・戻しは人（§5.5）。停止の判断と理由（数・割合・機器の上位の FG コード）は `edge_rollouts.halt_details` に保存し、FG-EDGE `RolloutHalted`（**alert**、runbook `edge-rollout`）を 1 回記録する。
- 評価は worker のタスク `edge_rollout_evaluator`（30 秒ごと、動いている展開ごとに 1 回の `GROUP BY state` の集計）。判定は純粋な関数なので、境目（ちょうど 2 %、`min_sample`、オフライン）を表の試験で固定する。

### 5.7 望ましい状態と報告の API（機器の経路）

| ルート | 中身 |
|---|---|
| `GET /api/v1/edge/desired`（`If-None-Match`） | 機器の今のグループの動いている展開について、機器が今の波に入っていれば `{ release: マニフェスト + 署名, install_window, rollout_id }`、入っていなければ今の版を返す。`ETag` = (グループ、展開、波、時間帯の版、機器の今の版) のハッシュ。多くは 304。`next_poll_seconds` を返す（サーバーが間隔を広げられる） |
| `POST /api/v1/edge/report` | 状態の変化だけ（§5.5 の遷移、FG コード、エージェントの版、今の版・前の版）。冪等キー `(rollout_id, state, attempt)` |
| `GET /api/v1/edge/blobs/{sha256}` | その機器に今・前・望ましいとして割り当てたリリースの blob だけ |
| `POST /api/v1/edge/token/rotate`・`GET /api/v1/edge/keys` | §4.2・§5.2 |

人の経路: `…/projects/{id}/edge/releases`・`…/rollouts`（作成・提出・承認・一時停止・再開・戻し・打ち切り）、`/api/v1/admin/edge/devices`・`groups`・`enrollment-tokens`・`windows`。`gstn edge device|group|release|rollout …`（承認のコマンドは無い。ADR-0031 Amendment 3 と同じ）。

### 5.8 エージェント（`forge-edge-agent`）の動き

- systemd のサービス（root で動かすが、フックは設定した利用者で動かせる）。設定は `/etc/forge-edge/agent.toml`（URL、CA、鍵の固定、フックのパス、置き場）。トークンは `/var/lib/forge-edge/token`（0600）。
- ポーリングは `next_poll_seconds`（既定 300 秒）に ±20 % のゆらぎ。失敗は指数の待ち（最大 30 分）。サーバーの再起動のあとに一斉に来ないよう、最初の 1 回もゆらぎを入れる。
- 取得: blob ごとにハッシュを確かめ、途中なら `Range` で続きから。持っている blob は取らない（層・ファイルの単位の差分）。空きが足りなければ取らずに `AgentDiskSpaceInsufficient`。
- 署名を確かめる前のものは置き場の外（`incoming/`）にだけ置き、確かめてから `releases/<digest>/` に移す。今の版と前の版の 2 つを残し、それより古いものは消す。
- フックは argv で呼ぶ（シェルの文字列を組まない）。引数は `release_digest`・`releases/<digest>` のパスだけ。時間切れ（`can-update` 30 秒、`install` は `max_install_seconds`、`health` 60 秒）。出力は機器のログにだけ残し、サーバーには FG コードと終了コードだけ送る（製造の情報を送らない）。
- 機器のログは JSON（journald）。`device_id`・`rollout_id`・`release_digest`・`error_code` を必ず付ける。報告が届かなければ手元に貯めて、つながったら古い順に送る（失われない）。

### 5.9 データ（すべて追記のマイグレーション。`tables.toml` の台帳と doctor の期待値も更新）

| MR | 表 | 種類 |
|---|---|---|
| E2 | `edge_enrollment_tokens`（ハッシュ、サイト、ラベル、残りの回数、期限、作った人）、`edge_devices`（`machine_id`、名前、サイト、`token_hash`・`prev_token_hash`・`token_expires_at`、状態、`arch`・`os`・`agent_version`、`last_seen_at`、`current_release_id`・`previous_release_id`） | 監査あり（人の操作だけ。`last_seen_at`・版の列は監査しない列） |
| E3 | `edge_device_labels`、`edge_groups`（選び方、環境、タイムゾーン）、`edge_group_members`（派生）、`edge_group_projects`、`edge_windows` | 監査あり（members は派生） |
| E4 | `edge_release_candidates`（タグ、SHA、パイプライン、状態 `pending` / `built` / `failed`、理由。派生）、`edge_blobs`（sha256、大きさ、置き場、最後に使った日）、`edge_releases`（マニフェスト、要約、`kid`、署名、`sequence`、作った人・パイプライン）、`edge_release_blobs` | releases / release_blobs は追記だけ（トリガー） |
| E5 | `edge_rollouts`（リリース、グループ、波の計画、停止の規則、状態、今の波、波の開始、承認者、`halt_details`）、`edge_rollout_devices`（PK (rollout, device)、状態、試行、FG コード、更新時刻）、`edge_device_history`（機器ごとの版の変化だけ、追記、月ごとのパーティション、保持 2 年） | rollouts は監査あり、rollout_devices は派生、history は追記だけ |

- `edge_devices` は `fillfactor = 70`（`last_seen_at` の上書きを HOT にする）。
- 索引: `edge_rollout_devices (rollout_id, state)`、`edge_group_members (device_id)`、`edge_devices (token_hash)`、`edge_rollouts (group_id) WHERE state IN ('running','paused','halted')` の一意。

### 5.10 Action と権限の表（どれも allow と deny の行。`authz-matrix-auditor` が確かめる）

| Action | allow | deny（代表） |
|---|---|---|
| EdgeEnrollmentTokenCreate / Revoke | 管理者の人の Web のセッション | トークン、エージェント、サービスアカウント、ボット、代理、Runner、Job、機器 |
| EdgeDeviceManage（ラベル・無効化・再発行）/ EdgeGroupManage / EdgeGroupBind / EdgeWindowManage | 管理者の人（時間帯はグループを結んだプロジェクトの Maintainer 以上の人も） | Developer、エージェント、サービスアカウント、機器 |
| EdgeDeviceRead / EdgeRolloutRead | 結んだプロジェクトの Reporter 以上、管理者。エージェントも読める（台帳と状態だけ） | 結んでいないプロジェクトの人 → 404、機器（自分以外） |
| EdgeReleaseCreate | system（保護されたタグのパイプラインの購読者）、Maintainer 以上の人 | Developer、エージェント、サービスアカウント、Runner、Job（ジョブはリリースを作れない） |
| EdgeRolloutCreate / Submit / Cancel | Maintainer 以上の人 | Developer、エージェント、サービスアカウント、自動マージの主体 |
| EdgeRolloutApprove（prod） | Maintainer 以上の人の Web のセッション、作った人と SHA の背後の人を除く | トークン（`session_required`）、エージェント、サービスアカウント、ボット、代理、自動マージの主体、Runner、Job、機器、作った人（FG-EDGE `RolloutSelfApproval`） |
| EdgeRolloutPause | Developer 以上（止めるのは安全側。`MergeQueuePause` と同じ） | Reporter、機器 |
| EdgeRolloutResume / Revert / Abort | 承認と同じ（Revert は 2 人目の承認なし、§5.5） | 承認と同じ |
| EdgeDesiredRead / EdgeStatusReport / EdgeTokenRotate | 有効な機器、自分の分だけ | 無効・期限切れの機器、他の機器の分、人・トークン・Runner・Job |
| EdgeBlobRead | 有効な機器、自分に割り当てたリリースの blob だけ。人は `EdgeRolloutRead` と同じ範囲 | 他のグループのリリースの blob（機器）→ 404 |
| 署名の鍵の管理 | HTTP に出さない。`forge admin edge-key` だけ | — |

### 5.11 エラーコード（仮。領域 `EDGE`、番号は登録の順）

| 仮の名前 | 内容 | 重さ | runbook |
|---|---|---|---|
| EDGE-a EnrollmentTokenInvalid | 登録のトークンが無い・期限切れ・回数切れ | info | `edge-enrollment` |
| EDGE-b DeviceDuplicate | 同じ `machine_id` の有効な機器 | info | `edge-enrollment` |
| EDGE-c ReleaseDefinitionInvalid | `edge-release.toml` の形・対象・シークレットらしいファイル | info | — |
| EDGE-d ReleaseNotFromProtectedRef | タグのコミットが保護ブランチから届かない | info | — |
| EDGE-e ReleaseVersionExists | 同じ版の作り直し | info | — |
| EDGE-f EdgeSigningKeyUnavailable | 鍵ファイルが無い・権限が緩い・DB の公開鍵と合わない | **alert** | `edge-signing-key` |
| EDGE-g BlobIntegrityMismatch | 置き場の blob のハッシュが合わない | **alert** | `edge-rollout` |
| EDGE-h BlobDownloadThrottled | 同時の送信の上限（429） | info | — |
| EDGE-i GroupOverlap | 1 台が同じプロジェクトの 2 つのグループに入る | info | — |
| EDGE-j RolloutApprovalRequired / RolloutSelfApproval | 承認待ち / 作った人・背後の人の承認 | info | `edge-rollout` |
| EDGE-k RolloutHalted | 停止の条件に当たった（`reason`、数、割合） | **alert** | `edge-rollout` |
| EDGE-l RolloutStalled | 7 日進めない | warn | `edge-rollout` |
| EDGE-m AgentSignatureInvalid | 機器がマニフェストの署名・blob のハッシュを確かめられない（改ざんの疑い） | **alert** | `edge-agent` |
| EDGE-n AgentInstallFailed / AgentHealthCheckFailed | フック `install` / `health` の失敗（機器は自分で戻す） | warn | `edge-agent` |
| EDGE-o AgentRollbackFailed | 前の版に戻せない（機器の状態が分からない） | **alert** | `edge-agent` |
| EDGE-p AgentUpdateDeferred | 時間帯の外・`can-update` が拒んだ | info | — |
| EDGE-q AgentDiskSpaceInsufficient | 置き場の空きが足りない | warn | `edge-agent` |

認証の失敗は FG-AUTH-0001（`reason=device_token_*`）、拒否は FG-AUTH-0003。機器の側のコード（m〜q）は機器のログに出し、報告で `error_code` としてサーバーに送る。画面に出すのは `user_message` と `request_id`（機器の報告なら `report_id`）だけ。

## 6. MR の分け方

置き場所: **[root]** = エージェントモデル（先に入れる）、印なし = GitStation。各 MR の終わりは計画 14 §5 と同じ（fmt・clippy `-D warnings`・nextest、`gen-catalog`・`gen-authz` の差分なし、`/review` で CRITICAL / HIGH なし）。評価役は 2 つまで。ブランチの area に `edge` は無いので `deploy`・`auth`・`core`・`ui`・`ops` を使う。

| MR | ブランチ | 中身 | 先に書く試験（例） | 評価役 | 前提 |
|---|---|---|---|---|---|
| **E0 [root]** | `chore/harness-m4e-edge-area` | 領域 `EDGE`（`validate-error-registry.js` の AREAS、`forge-error-contract.md`）、契約 9 に `forge-edge`・`forge-fleet`・`forge-edge-agent`、`deploy-safety-reviewer` にエッジの観点（機器のトークンの範囲、署名の領域、時間帯と `can-update` の失敗で入れない、停止の判定、戻しの承認）と eval | eval `edge-install-when-can-update-errors`、`device-token-reads-other-group-blob` | `harness-optimizer` | なし（今すぐ） |
| **E1** | `feat/core-edge-manifest` | `forge-edge`（純粋）: マニフェスト・要約・`edge-release.toml` の lint・波の割り当て・`decide`・時間帯。`forge-agent::sign` を領域つきに一般化（既存の関数は今の領域のまま）。`gstn edge release verify`。ADR-0039 | `digest_is_independent_of_blob_order`、`agent_part_signature_does_not_verify_as_edge_manifest`、`wave_membership_is_monotonic`、`canary_single_failure_halts`、`below_min_sample_waits`、`failure_rate_boundary_2_percent`、`window_across_dst_and_midnight`、`purity` | `rust-reviewer`、`security-reviewer` | E0 |
| **E2** | `feat/auth-edge-device-principal` | `Credential::Device`、`device.rs`、登録・トークン・入れ替え・失効、TLS の必須、監査の行為者 `device`、§5.10 の機器の行と登録の行 | `device_cannot_read_repository`、`revoked_device_denied_next_request`、`old_token_valid_until_new_used_or_24h`、`enrollment_token_single_use_race`、`duplicate_machine_id_refused`、`plain_http_refused_when_require_tls`、`a_device_is_not_a_user_kind`（維持） | `security-reviewer`、`authz-matrix-auditor` | E1 |
| **E3** | `feat/deploy-edge-inventory` | `forge-fleet` の台帳・ラベル・グループ・所属の作り直し・プロジェクトへの結び付け・時間帯、管理の API、`gstn edge device|group`、doctor `edge.devices`（最後の接続の古さの分布） | `label_change_rebuilds_membership_once`、`group_overlap_refused`、`unbound_project_sees_404`、`last_seen_update_is_throttled` | `database-reviewer`、`authz-matrix-auditor` | E2 |
| **E4** | `feat/deploy-edge-releases` | 保護されたタグの push → 候補 → `edge_release_builder`（パイプラインの成功を待つ）→ blob → マニフェスト → 署名（`edge.signing_keys_file`、`forge admin edge-key`）、blob の API（`Range`・429・ハッシュの確かめ直し）、保持の worker、doctor `edge.signing_key`、runbook `edge-signing-key` | `release_only_from_protected_reachable_tag`、`moved_tag_has_no_effect`、`release_rows_are_append_only`、`blob_corruption_is_not_served`、`range_resume_matches_sha256`、`secret_like_file_refused`、`job_token_cannot_create_release` | `deploy-safety-reviewer`、`security-reviewer` | E1、E3、M4 C6b |
| **E5** | `feat/deploy-edge-rollouts` | 展開の状態機械、承認・一時停止・再開・戻し・打ち切り、望ましい状態の API（ETag・304・`next_poll_seconds`）、報告の API（冪等）、`edge_rollout_evaluator`、イベント `edge_rollout.state_changed`、`gstn edge rollout`、doctor `edge.rollouts`、runbook `edge-rollout` | `prod_approve_denied_for_creator_and_people_behind_sha`、`approve_requires_web_session`、`device_outside_wave_gets_current_release`、`outside_window_install_window_is_closed`、`halt_is_recorded_once_with_details`、`revert_targets_last_completed_release`、`one_active_rollout_per_group`、`heartbeat_never_writes_audit_or_outbox`、`no_job_per_device` | `deploy-safety-reviewer`、`database-reviewer` | E3、E4 |
| **E6** | `feat/deploy-edge-agent` | `forge-edge-agent`: ポーリング・ゆらぎ・待ち、取得と再開、署名の確かめ、A/B の置き場、フック（argv・時間切れ・`can-update` の失敗で入れない）、健全性の見守り・自分での戻し、報告の貯めと再送、トークンの入れ替え、自分自身の更新（`agent` の kind）、systemd のユニットと例のフック、runbook `edge-agent`、`edge-enrollment` | `tampered_manifest_never_reaches_releases_dir`、`can_update_timeout_defers`、`no_install_when_window_shorter_than_max_install`、`health_failure_rolls_back_and_reports`、`rollback_failure_reports_alert_code`、`reports_survive_offline_and_send_in_order`、`hook_argv_has_no_shell`、`self_update_reverts_on_failed_start`。本物のフックの試験は Ubuntu 24.04 のコンテナで | `deploy-safety-reviewer`、`rust-reviewer` | E5 |
| E7 | `feat/ui-edge-fleet` | 画面: 機器の台帳（版・最後の接続・状態）、展開の進み具合（波ごとの数）、停止の 1 行（「なぜ止まったか・次に押すもの」、08）、承認待ちに prod の展開を合流 | 読み上げ、色だけに頼らない、権限の無い人にボタンを出さない | `a11y-architect`、`design-reviewer` | E5 |
| E8（Should） | `feat/deploy-edge-relay` | 工場の中継: `forge-edge-agent relay` のモード（blob だけを読み取りキャッシュ。望ましい状態・報告は本体へ通す。中継は信用しない — 署名とハッシュは機器が確かめる）。機器の設定 `blob_sources` | `relay_cannot_forge_blob`、`relay_down_falls_back_to_origin`、`relay_never_caches_desired_or_report` | `deploy-safety-reviewer`、`security-reviewer` | E6 |
| **E9** | `chore/ops-edge-pilot` | `ops/loadtest/edge`（偽の機器 10 000 台）、§8 の数字の測定、パイロット（1 工場 1 ライン）の手順と記録、runbook の仕上げ | 負荷試験の記録（§8 の目標） | `deploy-safety-reviewer`、`silent-failure-hunter` | E6（E7 があれば望ましい） |

## 7. 運用で足すもの

- **doctor**: `edge.signing_key`（鍵の有無・権限・DB との一致）、`edge.tls`、`edge.devices`（15 分 / 1 時間 / 24 時間以上ポーリングの無い台数）、`edge.rollouts`（`halted`・7 日以上進まない展開）、`edge.blob_store`（空き）。
- **メトリクス**: ポーリングの数と 304 の割合、機器の状態ごとの台数、波ごとの成功・失敗・未応答、blob の送信量と 429 の数、`edge_rollout_evaluator` の所要時間。
- **ログ**: すべての失敗に `request_id`・`error_code`、エッジは `device_id`・`rollout_id`・`release_id`。トークン・登録のトークンは出さない。
- **runbook（新規）**: `edge-enrollment`（登録・再発行・盗まれた機器）、`edge-rollout`（停止の調べ方: 上位の FG コード → 機器の一覧 → 再開か戻しか。blob の破損）、`edge-signing-key`（入れ替え、漏えいのとき: 失効 → 新しい鍵 → 全グループの今の版を署名し直したリリースで展開）、`edge-agent`（機器の上: ログの見方、`rollback_failed` の機器の手での復旧、ディスク、時計のずれ）、`edge-relay`（E8）。

## 8. 規模の想定（推定。E9 で測って置き換える）

| 量 | 想定 | 計算 |
|---|---|---|
| 機器の数 | 設計 10 000 台、当面 ≤ 5 000 台、30 工場まで | §11 問い 1 |
| ポーリング | 300 秒 ±20 % | 10 000 / 300 ≈ **33 件/秒**。99 % 以上が 304（展開の無い時間） |
| ポーリング 1 件の DB | トークンのハッシュの索引で 1 行、所属 1 行、動いている展開（メモリに 10 秒保持）。`last_seen_at` の書き込みは前回から 10 分以上たったときだけ | 書き込み ≤ 10 000 / 600 ≈ **17 件/秒**（HOT の更新） |
| 報告 | 状態の変化だけ、1 展開で 1 台 5〜7 件 | 10 000 台の展開でも 1 日 7 万件以下 |
| 評価の worker | 動いている展開ごとに 30 秒に 1 回の集計（≤ 10 000 行、索引あり） | 1 回 ≤ 20 ms の見込み |
| 帯域 | 1 リリース 200 MB、変わる層 20 MB を想定 | 中継なし: 10 000 × 20 MB = 200 GB を波に分けて。中継あり: 30 工場 × 20 MB = 600 MB が本体から。同時の送信は 64 まで |
| Outbox / 監査 | 展開あたり十数件、人の操作だけ | 機器の数に比例しない |

試験: 偽の機器 10 000 台がポーリング・報告・取得をし、serve の p95 ≤ 100 ms、DB の CPU ≤ 30 %、Outbox と `audit_log` の行が機器の数で増えないことを確かめる（計画 18 と同じく Linux のホストで測る）。

## 9. 危険と対策

| 危険 | 対策 |
|---|---|
| 改ざんしたプログラムが機器に入る | マニフェストの Ed25519 の署名（エッジ専用の鍵・領域）、blob のハッシュ、確かめる前は置き場の外。中継は信用しない |
| 稼働中の設備を止める | 時間帯 + `can-update` の両方、失敗は入れない側、残り時間の確認 |
| 悪い版が全台に広がる | canary は 1 台の失敗で停止、割合の波はしきい値で停止、承認は人、波の見守りの時間 |
| 機器のトークンの漏えい | 読めるのは自分に割り当てたリリースだけ、90 日の入れ替え、すぐの失効、TLS の必須 |
| 数千台の一斉のアクセス | ゆらぎ、ETag、`next_poll_seconds`、429 と `Retry-After`、中継 |
| 戻せない機器 | A/B で前の版を残す、`rollback_failed` は即停止の alert、runbook の手での復旧 |
| 自動化が prod の展開を承認する | 権限の表の deny の行（エージェント・自動マージ・Runner・Job・機器）、Web のセッションだけ |
| 署名の鍵の漏えい | 鍵は worker のホストのファイルだけ、`kid` の入れ替えと失効、runbook `edge-signing-key` |

## 10. 完了の条件（01 §8 の M4E）

1. 1 つの工場の 1 ライン（パイロット）で、canary → 割合 → 全台の展開が保守の時間帯の中だけで進み、`can-update` が拒んだ機器は入れずに待つ。
2. 失敗するように作ったリリースで、失敗した機器が自分で前の版に戻り、展開が自動で `halted` になり、alert と runbook で原因が追える。
3. prod の展開はエージェント・サービスアカウント・作った人が承認できない（権限の表の行）。
4. 偽の 10 000 台の負荷試験で §8 の目標を満たす。
5. 新しいルート・Action はすべて権限の表にあり、カタログ・runbook・ADR-0039 がそろい、Windows でも全体の試験が緑。

**確かめのコマンド**: 計画 11 §9 と同じ（fmt・clippy `-D warnings`・nextest・`gen-catalog` / `gen-authz` の差分なし・`node scripts/self-heal.js --check`）。

## 11. 利用者に聞くこと（それぞれ推奨の既定）

| # | 問い | 推奨の既定 |
|---|---|---|
| 1 | エッジは何台、いくつの工場・ラインか | 設計 10 000 台・30 工場、当面 ≤ 5 000 台 |
| 2 | 機器の OS と CPU | Ubuntu 24.04（PFS のエッジサーバーと同じ）、`amd64` と `arm64`。Windows IoT は範囲の外 |
| 3 | 配るものの形 | PFE のコンテナ（`oci-image`）と設定ファイル（`files`）。PLC のプログラムは範囲の外 |
| 4 | 回線: エッジから GitStation へ HTTPS で届くか、工場に中継を置けるか | 直接の HTTPS（社内 CA、前段の proxy で TLS）。機器が 50 台を超える工場には E8 の中継 |
| 5 | 稼働の予定はどこで決まるか、連携するか | GitStation のグループごとの週の時間帯 + 機器のフック `can-update`。生産管理のシステムとの連携は後（フックの中で PFS が見てもよい） |
| 6 | 今の更新の仕組み（置き換えるもの） | 無い（手の SSH と `docker compose`）と仮定。あれば E9 で並行の期間を作る |
| 7 | 署名の鍵をマーケットプレイス（計画 16）と分けるか | **分ける**（`edge.signing_keys_file`。漏えいの範囲を機器の群に閉じる） |
| 8 | prod の承認の単位と戻し | 承認は展開（全部の波）に 1 回。停止は自動、再開は承認と同じ権限。前の完了した版への戻しは人の 1 回の操作で、2 人目の承認なし |
| 9 | 機器のフック（`can-update`・`install`・`health`・`rollback`）を誰が書くか | PFS が PFE と一緒に配る。GitStation は例のフックと契約の試験だけを持つ |
| 10 | 機器の身元: トークンか相互 TLS か | 機器ごとのトークン（90 日、自動の入れ替え）+ TLS 必須。相互 TLS は M8 の選択肢 |
| 11 | エージェント自身の導入と更新 | 初回は PFS の用意の手順で `.deb` を入れる。以後は kind `agent` のリリースとして同じ展開で更新（前のバイナリを残す） |
| 12 | 停止のしきい値の既定（canary 1 台、割合 2 % かつ 3 台、未応答 10 %、見守り 24 時間 / 4 時間） | この値で始め、パイロットの結果で決め直す |
