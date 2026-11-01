# ADR-0019: 個人用アクセストークンと認証

**Date**: 2026-10-29
**Status**: 承認済み（accepted。手順 D3 の計画とともにユーザーが承認 2026-10-29。ADR-0010・ADR-0016・ADR-0018 を改訂する）
**Deciders**: Masaki Fujikawa（ユーザー）、Claude

## 背景

M1 では `git clone` / `git push` と API を、人のユーザーが自分の資格情報で使えるようにする必要がある。手順 D1・D2 でユーザー・名前空間・グループと
役割の読み込み（ADR-0010 の Amendment 2026-10-29）までが入ったが、**誰がリクエストを送ったか**を決める認証がまだない。

前提として確かめたこと（作業ツリーで確認、2026-10-29）:

- `forge admin` にはまだ読み取りのコマンドしかない。トークンの発行・失効・一覧、ユーザーの無効化、`bootstrap-admin` は **最初の
  書き込みをする `forge admin` のコマンド**になる。
- `Scope` は `forge-auth` にある。`forge-db` は `forge-auth` を見られない（依存の向き、CLAUDE.md §3.9）ので、トークンの表を
  扱う `forge-db` から `Scope` を参照できない。
- `forge-web` は `forge-db` に依存しない（readyz と同じく、`forge-cli serve` がトレイトの実装を注入する形）。
- ADR-0018 の `forge_ro` には資格情報（トークンのハッシュ）を見せない。
- Smart HTTP はまだない（フェーズ 9）。実際の `git` を使う e2e テストはここでは書けない。
- `forge-auth` README の引き継ぎ事項 7（無効化するユーザーが最後の Owner の場合）と 9（Basic 認証のパスワードはトークンだけ）が未決。

**ユーザーの方針（2026-10-30）**: 製品の根本の目的は、kanflow と密に連携してバイブコーディングの開発速度を上げること。連携は必須で、
「連携は簡略に」は **設定（つなぐ手順と設定項目）を簡略にする** という意味であり、連携の機能やイベントを減らす意味ではない。
AI のエージェントもトークンで git と API を使うため、トークンには最初から `is_agent` の印を持たせる。

## 決定

### 1. トークンの形式と生成

- 形式: `fgp_<public_id>_<secret>`。`public_id` は 16 桁の小文字の 16 進（64 bit）、`secret` は 64 桁の小文字の 16 進（256 bit）。
  接頭辞 `fgp_` は秘密情報の走査（ログ・リポジトリ）で検出しやすくするため。
- 乱数は OS の CSPRNG（`getrandom`）から 256 bit。`public_id` も同じ源から取る。
- プリミティブ（解析、生成、ハッシュ、定数時間の比較）は **`forge-core`** に置く（`token.rs`）。`Scope` も `forge-core` の
  `scope.rs` へ移し、`forge-auth` は再エクスポートする。
- 秘密の型 `TokenSecret` は `Serialize` も `Display` も実装しない。`Debug` は伏せ字にする。

### 2. ハッシュ: SHA-256（パスワード用のハッシュではない）

- DB には `secret_sha256`（`bytea`、32 バイト）だけを保存し、平文は保存しない。
- argon2 / bcrypt を使わない理由: 秘密は 256 bit の一様な乱数なので総当たりは成り立たず、遅いハッシュは防御を増やさない。
  一方で git の操作は 1 回の clone で何度も認証が走るので、遅いハッシュはそのまま遅延と DoS の面になる。

### 3. 照合: `public_id` で引き、定数時間で比べる

- `public_id`（UNIQUE）で 1 行を引き、`secret` の SHA-256 と `secret_sha256` を `subtle` で定数時間に比較する。
- **`public_id` が見つからないときも、ダミーのハッシュと定数時間の比較を実行する**（存在の有無を時間で漏らさない）。
- トークンの表の読み取りは **書き込みのプール（`forge_app`）で行う**。`forge_ro` は `secret_sha256` を読めない（ADR-0018 の Amendment）。
- 認証中に DB が使えないときは **`FG-DB-0001`（503）** にする。401 にはしない（クライアントが資格情報を捨てたり、
  正しいトークンを「誤り」と記録したりしないため）。
- 判定の順序は `malformed` → `unknown` → `mismatch` → `revoked` → `expired`（いずれも **`FG-AUTH-0001`（401）**、どれだったかは
  `reason` としてログにだけ出す）。トークンの状態は秘密を持つ者にだけ分かる。所有者が無効化されているときは、
  **`tokens::authenticate` 自身が `FG-AUTH-0004`（`reason=owner_disabled`）を返す**（呼び出し側の確認漏れで通らないように。
  2026-10-30 の /review の指摘による）。このとき `last_used_at` は更新しない。
- 拒否はログのイベント `forge.auth.token_rejected`（`request_id`、`error_code`、`reason`、`public_id`）に出す。秘密は出さない。

### 4. 有効期限と上限

- `expires_at` は NOT NULL。`created_at` より後で、**365 日以内**（DB の CHECK と `tokens::issue` の両方で強制）。
  CLI の既定は `--expires-in 90d`。
- 1 ユーザーあたりの有効なトークンの数に上限を設ける（超えたら `FG-AUTH-0008 reason=active_limit`）。
- 名前は 1〜100 文字、NUL 文字とその他の制御文字（タブ・改行・エスケープ）は不可（`token list` の表示を偽造させない）。スコープは 1 つ以上で、すべて既知のもの。

### 5. 監査とイベント: 監査は所有者のユーザーに付け、`token.*` は `admin` のイベントにする

- `tokens` の表は監査の対象。表の台帳（`tables.toml`）で `audit_resource = "user"`、`audit_id_column = "user_id"` とし、
  トークンの監査の行は **所有者のユーザー**に付ける（ユーザーの履歴で発行・失効がまとめて見える）。
- `AuditDetails::{TokenCreated, TokenRevoked}` を追加する。秘密もハッシュも載せない。
- **`token.created` / `token.revoked` はドメインイベントとして出す**（ADR-0016 §10 の M1 のイベント、分類は `admin`）。
  payload はトークンの ID、`public_id`、所有者、スコープの名前、`expires_at`、`is_agent` だけ。
- トークンが最初のイベントの書き手になるので、D3 で `forge-core` に **`EventType` の列挙**（`ALL`、`as_str`、`visibility_class()`）と
  **`DedupKey` の規則**（サーバー側の ID とハッシュだけから作る。`^[a-z][a-z_]*(:[0-9a-z_-]{1,64}){1,4}$`、200 文字まで）を入れ、
  ADR-0016 §11 の未決を閉じる（同 ADR の Amendment）。
- 無効化による一括の失効も、失効した本数だけ `token.revoked` を出す（有効は最大 50 本なので、1 回の変更のイベントの上限 100 に収まる）。
- `tokens.is_agent`（`boolean NOT NULL DEFAULT false`、監査の対象）を最初から持つ。発行時に宣言し、ADR-0017 §4.0 の AI のエージェントの
  識別に使う（`Credential` 型への反映は W1）。

### 6. `last_used_at` の更新

- 更新は **最大 10 分に 1 回**: `UPDATE … SET last_used_at = now() WHERE id = $1 AND (last_used_at IS NULL OR last_used_at < now() - interval '10 minutes')`。
- `last_used_at` の変更は監査しない（監査のトリガーで除外する列）。
- 更新の失敗は `FG-DB-0002` を warn で 1 回出し、**認証はそのまま続ける**（使用時刻は補助情報であり、
  これで認証を落とすと DB の一時的な競合が利用者の失敗になる）。失敗はログに残るので沈黙の失敗ではない。

### 7. HTTP での受け取り方（`forge-web`）

- `Authorization: Bearer <token>`、または `Authorization: Basic`（**パスワードがトークンであるもの**）。Basic のユーザー名は
  無視し、ログにも出さない。**Basic のパスワードとしてユーザーのパスワードは受け付けない**（ADR-0010 の引き継ぎ事項 9 を閉じる）。
- 認証のヘッダーが 2 つあるときは 401（`reason=ambiguous`）。
- `forge-web` は `forge-db` に依存しない: `Authenticator` トレイト（`forge-core`）を `AppState.authenticator` に持ち、
  `forge-cli serve` が `forge-db` による実装を注入する。
- 認可は従来どおり `authorize()` だけで決める。認証はプリンシパルとスコープを作るだけ。権限の表（matrix）は変わらない。

### 8. CLI が信頼の起点、`tokens::issue` が不変条件を守る

- トークンの発行は `forge admin token issue --user <u> --scopes <s,..> [--expires-in 90d] [--name <n>]`。ホストの操作者が信頼の
  起点であり、**この経路は `authorize()` を通らない**（HTTP の経路ではないため。HTTP で発行する経路を作るときは別に認可の行を足す）。
- そのため `tokens::issue` 自身が不変条件を強制する（この順で判定）: ボットには発行しない（`owner_bot`）、無効化されたユーザーには
  発行しない（`owner_disabled`）、`admin` スコープは管理者のユーザーだけ（`admin_scope_requires_admin`）、上限（`active_limit`）。
  `tokens::revoke` は所有者を確かめない（操作者専用）。HTTP の経路は `authorize()` で判定し、他人のトークンは見つからない扱いにしてから呼ぶ。
- トークンは **標準出力に 1 回だけ**表示し、ログには出さない。`token revoke`、`token list`（ハッシュは出さない）も追加する。
- **権限の単一判定点（CLAUDE.md §3.4）の明示的な例外**（2026-10-30、/review の指摘を受けユーザーが決定）: `admin_scope_requires_admin`
  は `users.is_admin` を見る判定だが、`forge-auth` ではなく `forge_db::tokens::issue` に置く。理由: 依存の向き（§3.9）で `forge-db` は
  `forge-auth` を使えず、この判定は HTTP/git の操作の認可ではなく「保存されるトークン」の不変条件であるため。CLI の発行・失効も
  `authorize()` を通らない（ホストの操作者が信頼の起点）。**この例外は CLI の経路だけ**: HTTP でトークンを発行・失効する経路を作るときは
  `Action` と権限マトリクスの行（allow と deny）を足し、判定は `authorize()` で行う（`tokens::issue` の不変条件は二重の守りとして残す）。

### 9. 最初の書き込みをする `forge admin` のコマンドは、人が承認する Runbook の手順

- `token issue` / `token revoke` / `user disable` / `bootstrap-admin` は本番を変える操作なので、本番では
  **人が承認した Runbook の手順**としてだけ実行する（CLAUDE.md §3.7）。Runbook に承認の手順として明記する。

### 10. 無効化のガード

- 既定では拒否する: 無効化によって次のいずれかになるとき **`FG-AUTH-0009 AdministrationConflict`（409）**。
  `reason` は `last_admin`（最後の有効な管理者）、`last_owner`（最上位のグループの最後の直接の Owner）、`admin_exists`。
- **CLI の `forge admin user disable --force` だけが通す**。監査の行に `forced`、`ownerless_groups`、`left_without_admin` を記録する。
  HTTP からは `--force` 相当を出さない。
- その結果できた「管理者のいない状態」「Owner のいない最上位のグループ」は `forge doctor`（`users.administration`）が
  **`FG-AUTH-0010 AdministrationOrphaned`**（warn、Runbook `user-offboarding`）として報告する。復旧は `bootstrap-admin`。
- **数え方**: 「最後の管理者」は **有効な人間の管理者だけ**を数える（ボットと無効化済みのユーザーは数えない）。
- **最上位のグループは有効な直接の Owner が必要**。サブグループは親から Owner を継承してよいので対象外。
- 拒否された無効化は何も失効させない。

### 11. ロックの順序と、グループの行をロックしない理由

- 無効化・`bootstrap-admin`（と、将来の `is_admin` の変更）は次の順で取る:
  1. トランザクション単位の advisory lock `users.admin_set`
  2. 対象のユーザーの行を `FOR UPDATE`
  3. 数え上げ（ロックを取らない）
- **グループの行はロックしない**。メンバーの変更は「グループ → Owner の行（`FOR SHARE`）」の順でロックするので、無効化が
  「ユーザー → グループ」の順で取ると逆順になりデッドロックする。数え上げで最後の Owner を見落とす競合は、メンバーの変更が
  Owner の行を `FOR SHARE` で取ることで、対象のユーザーの `FOR UPDATE` と直列化される。

### 12. 一括失効と、発行と無効化の直列化

- 無効化は同じ `ChangeTx` で、そのユーザーの有効なトークンをすべて失効させる（`revoked_reason = 'user_disabled'`）。
  監査は `user.disabled` の 1 行に `revoked_tokens` をまとめる（トークンごとの監査の行は出さない）。イベントは失効したトークンごとに
  `token.revoked` を出す。有効期限の切れたトークンは対象にしない（もう使えないため）。D3 の MR1 で実装（2026-10-30、計画の MR2 から前倒し）。
- `tokens::issue` は所有者の行を `FOR UPDATE` でロックする。これで「無効化の途中に発行され、失効を免れるトークン」は生じない。

### 13. スキーマ（`tokens`）と権限

- 列: `id`、`user_id`（FK）、`public_id`（UNIQUE、16 進 16 桁の CHECK）、`secret_sha256`（`bytea`、32 バイト）、`name`（1〜100）、
  `scopes`（`text[]`、1 つ以上、既知のもの）、`created_at`、`expires_at`（NOT NULL、`> created_at`、365 日以内）、`revoked_at`、
  `revoked_reason`（CHECK `revoked` | `user_disabled`）、`last_used_at`。部分索引 `(user_id) WHERE revoked_at IS NULL`。
- `forge_app`: SELECT・INSERT と、UPDATE(`revoked_at`, `revoked_reason`, `last_used_at`) だけ。DELETE なし。
- `forge_ro`: `secret_sha256` を除く列だけ。`guarded_objects` とスキーマの期待値に登録する。

### 14. エラーコード

| コード | 名前 | HTTP | 内容 |
|---|---|---|---|
| `FG-AUTH-0008` | TokenRequestInvalid | 400 | 発行の要求が不正。`reason`: `expires_out_of_range`、`scopes_empty`、`scope_unknown`、`name_length`、`nul_character`、`control_character`、`admin_scope_requires_admin`、`owner_bot`、`owner_disabled`、`active_limit` |
| `FG-AUTH-0009` | AdministrationConflict | 409 | 無効化のガード。`reason`: `last_admin`、`last_owner`、`admin_exists` |
| `FG-AUTH-0010` | AdministrationOrphaned | —（doctor、warn） | 管理者または最上位のグループの Owner がいない。Runbook `user-offboarding` |
| `FG-DB-0019` | CredentialDataInvalid | 500（alert） | DB のトークンの行が不正（ハッシュの長さ、形式など）。Runbook `credential-data` |

既存のコードの再利用: `FG-AUTH-0001`（期限切れ・失効・不一致、401）、`FG-AUTH-0004`（所有者が無効）、`FG-DB-0001`（認証中の DB 障害、503）、
`FG-DB-0002`（`last_used_at` の更新の失敗、warn）。

### 15. 実装の分け方（MR）

1. `feat/auth-personal-access-tokens`: `forge-core` の `scope.rs`・`token.rs`、マイグレーション `tokens`、`AuditDetails`、
   `forge admin token issue|revoke|list`、`FG-AUTH-0008`・`FG-DB-0019`。
2. `feat/auth-disable-guard`: ガード、一括失効、`bootstrap-admin`、doctor の `users.administration`、`FG-AUTH-0009`・`FG-AUTH-0010`、
   Runbook `user-offboarding.md`。D2 の LOW の修正（`groups/members.rs` の `unknown_role` → `FG-DB-0018`）。
3. `feat/web-token-authentication`: `forge-web` の `auth.rs`、`AppState.authenticator`、`serve` の配線、
   `forge-web/tests/token_principal.rs`。

## 検討した案

### トークンの処理をどこに置くか

| 案 | 内容 | 失敗の現れ方 | 判断 |
|---|---|---|---|
| **A（採用）** | プリミティブを `forge-core`、保存と照合を `forge_db::tokens`、HTTP は `forge-web` の middleware が `Authenticator` トレイト経由で呼ぶ | 解析の失敗は `FG-AUTH-0001`、DB は `FG-DB-0001`、行の破損は `FG-DB-0019`。どの層で落ちたかがコードで分かる | 依存の向きを守り、`forge-web` は `forge-db` を見ない |
| B | すべてを `forge-auth` に置き、`forge-auth` が DB を引く | — | `forge-auth` は判定だけの純粋な層（ADR-0010）。DB への依存を入れると判定の単体テストが DB を要する |
| C | `forge-web` が `forge-db` を直接呼ぶ | — | ADR-0001 の依存の向きと readyz の前例に反する。HTTP の層と DB の層の失敗が混ざる |

### 無効化のガード

| 案 | 内容 | 判断 |
|---|---|---|
| **1（採用）** | 既定で拒否（`FG-AUTH-0009`）、CLI の `--force` だけが通す。結果の孤立は doctor の `FG-AUTH-0010` で見える化、`bootstrap-admin` で復旧 | 誤操作を防ぎつつ、退職者の即時の無効化（セキュリティ）を止めない |
| 2 | 常に拒否（先に Owner を移す必要がある） | 緊急の無効化（資格情報の漏えい）が Owner の移譲待ちで遅れる |
| 3 | 常に許可し、警告だけ | 管理者のいない状態が黙って生じる。沈黙の失敗に近い |

## 結果

- 依存の向きは変わらない（`forge-core` に `Scope`・`token`・`Authenticator`、`forge-db` は `forge-core` だけを見る）。
- 認証の失敗は `FG-AUTH-0001`／`FG-AUTH-0004`、DB の障害は `FG-DB-0001`、データの破損は `FG-DB-0019` と分かれ、ログの
  `forge.auth.token_rejected` の `reason` で原因まで追える。

**受け入れたリスク**

- **無効化の時点で処理中のリクエスト**は、そのリクエストの間は完了しうる（認証はリクエストの開始時に 1 回だけ行う）。
  次のリクエストからは拒否される。
- **トークンは端末に 1 回表示される**。端末の履歴やスクロールバックに残りうる。ログには出さない。
- **メモリ上の秘密を `zeroize` しない**。プロセスのメモリを読める攻撃者はすでにより多くを持っている。

## 見送り

- `is_agent` を `Credential` 型と認可に反映すること: W1（ADR-0017 §4.0）。列は D3 で入れる。
- 拒否されたリクエストの監査の集約（ADR-0016 §6）。
- 認証のレート制限。
- 実際の `git` による e2e テスト: フェーズ 9（Smart HTTP）へ渡す。名前だけ決める:
  `git_clone_with_a_token_in_basic_auth_succeeds`、`git_push_with_a_read_repository_token_is_refused`、
  `git_without_credentials_gets_www_authenticate_basic`。

## 影響するエラーコードと Runbook

- 追加: `FG-AUTH-0008`、`FG-AUTH-0009`、`FG-AUTH-0010`、`FG-DB-0019`（`errors.toml` に登録し、カタログは生成する）。
- Runbook 追加: `user-offboarding.md`（`FG-AUTH-0010`、`--force`、`bootstrap-admin`）、`credential-data.md`（`FG-DB-0019`）。
- Runbook 更新: `forge admin` の書き込みのコマンドは、人が承認する手順として記載する。

## Amendment（2026-10-30、手順 D3 MR3: HTTP でのトークン認証）

§7 を実装したときに決めた細部。権限の判定（`authorize()`）と権限のマトリクスは変わらない。新しいエラーコードはない。

- **`Authenticator` トレイトと `TokenIdentity`**: どちらも `forge-core`（`authenticator.rs`）に置き、`forge_db::tokens` は
  `TokenIdentity` を再エクスポートする。`forge-db` の `Db` が `Authenticator` を実装し（中身は `tokens::authenticate` そのもの）、
  `forge serve` が readyz のプローブと同じく `AppState.authenticator` に注入する。`AppState::new` は authenticator を
  **必須の引数**にした（既定の実装を持たない。配線を忘れたサーバーはコンパイルできない）。
- **認証を走らせる場所**: 登録簿（`Registry::route`）が、公開でない経路ごとに「トークン認証 → 既存のガード」の順で
  付ける。**公開の経路（`/healthz`、`/readyz`）と未登録のパス（404）は資格情報を見ない**。生存確認がトークンの表や DB に
  依存せず、公開の経路に不正なヘッダーを付けても 401 にならない。
- **`ambiguous`（401）**: `Authorization` のヘッダーが 2 つ以上のとき、または別の仕組み（将来のセッション）がすでに
  プリンシパルを置いたリクエストにヘッダーがあるとき。どちらの資格情報も試さない。
- **ヘッダー自体の拒否**: 読めないヘッダー・base64 でない・`:` がない Basic・空の値は `FG-AUTH-0001 reason=malformed_header`、
  Bearer と Basic 以外の方式は `reason=unsupported_scheme`。方式の名前もログに出さない（ヘッダーに秘密だけを書くクライアントが
  あり、その場合は「方式」が秘密そのものになるため）。Basic のパスワードが何であっても認証器に渡すので、ユーザーの
  パスワードは `malformed` で必ず拒否される（ADR-0010 の引き継ぎ事項 9）。
- **ログ**: `FG-AUTH-0001` / `FG-AUTH-0004` の拒否は `forge-web` の middleware がイベント `forge.auth.token_rejected`
  （`request_id`、`error_code`、`reason`、`scheme`、分かれば `public_id` と所有者の `user`。`user` は内部の数値の ID で、
  FG-AUTH-0004 のカタログの診断と `forge_db::tokens` のエラーがすでに記録している慣習に合わせた。文脈をまとめた文字列は
  出さず、フィールドだけにする）として **1 回だけ**出し、エラーの描画では
  もう一度出さない。それ以外のコード（`FG-DB-0001` など）は通常どおりエラーの描画が 1 回出す（文脈に `scheme`）。
  ヘッダーの値、トークン、秘密、Basic のユーザー名はどこにも出さない（テストでログを捕まえて確かめる）。
  認証に成功したら `Authorization` ヘッダーをリクエストから外してからハンドラーに渡す（ハンドラーが資格情報を
  ログに出したり返したりできないように）。
- **`WWW-Authenticate`**: エラーの描画（`error.rs`）の 1 か所で、**すべての 401** に `Basic realm="GitStation", charset="UTF-8"`
  と `Bearer realm="GitStation"` を付ける（git は Basic のチャレンジで資格情報ヘルパーに聞く）。403 と 503 には付けない
  （障害のときにクライアントが正しい資格情報を捨てないように）。
- **方式と資格情報の区切り**: 最初の ASCII の空白（スペースまたはタブ）。
- **リクエスト ID がないとき**: 新しい ID を作って続けず、`FG-SYS-0001 reason=request_id_missing`（500、1 回ログ）にする。
- **同時に走る認証の上限**（2026-10-30 のレビューの指摘）: 照合は書き込みのプールを使うので、形の正しいでたらめなトークンを
  大量に送られると書き込みが詰まる。`forge-web` の汎用のラッパー `BoundedAuthenticator`（セマフォ）で同時の照合の数を抑え、
  `forge serve` が許可の数を `max(1, database.write_max_connections / 2)` にする。許可を待つ時間はリクエストのタイムアウト
  （`FG-WEB-0002`）で区切る。新しいエラーコードはない。
- **テストのための口**: 製品の登録簿には公開でない経路がまだないので、`forge-web` の feature `test-support` で
  `app::with_registry`（製品と同じ middleware でテスト用の登録簿を組む。`#[doc(hidden)]`）を出す。`forge-web` が自分自身を
  dev-dependency にして有効にするため `cargo build --workspace --all-targets` では有効になるが、`forge` のバイナリには入らない。
  CI が `cargo tree -p forge-cli -e normal,features` で `test-support` が通常の依存のグラフにないことを確かめる。

**見送り（追加）**: リクエストに `token_id` と `is_agent` を載せること（監査と ADR-0017 §4.0 の W1 で、使う側と一緒に入れる）。

**未決（追加）**: クライアント（IP）ごとの認証のレート制限。同時の照合の上限は全体で 1 つなので、1 つのクライアントが
許可を使い切ると他のクライアントの認証が待つ（リクエストのタイムアウトで 503 になりうる）。クライアントを区別する制限は
プロキシの構成（送信元のアドレスをどこから取るか）と一緒に決める。

## Amendment（2026-10-30、D3 MR2: 無効化のガードと `bootstrap-admin`）

実装（`feat/auth-disable-guard`）で決めたこと。§10〜§12 の決定は変えていない。

1. **`bootstrap-admin` の形**（リードの決定）: `forge admin bootstrap-admin --user <username>` は既存の有効な人間を管理者にする（`Promote`）。
   `--user <username> --email <email> [--display-name <name>] --create` は、`users::create` と同じ経路（パスのロック、墓標の確認、個人の名前空間）で
   新しい人間の管理者を作る（`Create`。`--display-name` の既定はユーザー名）。clap で `--create` には `--email` を、`--email`・`--display-name` には
   `--create` を必須にした。有効な人間の管理者がいるときは `FG-AUTH-0009 reason=admin_exists` で拒否し、**`--force` はない**。
   ロックは §11 と同じ順（`users.admin_set` → 対象の行 `FOR UPDATE`（`Promote` のみ）→ 数え上げ）。
2. **`bootstrap-admin` で拒否する対象のコード**: ボット・サービスアカウント（`reason=not_human`）と無効化された利用者（`reason=user_disabled`）も
   `FG-AUTH-0009` にした（新しいコードを作らない）。「管理の状態に合わない管理の操作」を 1 つのコードにまとめ、Runbook も 1 つにするため。
   `--create` は常に人間を作るので、`Create` に人間以外が来るのはプログラムの誤りだが、同じく `not_human` で拒否する。
3. **`bootstrap-admin` の監査**: 新しい `AuditDetails::AdminBootstrapped { created, namespace_id }`、action `user.admin_bootstrapped`、
   リソースは `user`/`users.id`、イベントなし（`user.*` は監査だけ）。1 つの変更に監査の行は 1 つなので、`Create` でも `user.created` の行は
   別に書かない（`created: true` と `namespace_id` がその代わり）。名前・メールは入れない。
4. **`user.disabled` の詳細**: 常に `{"kind","revoked_tokens","forced","ownerless_groups","left_without_admin"}` の 5 つを書く（強制しなかった
   無効化でも `false`/`0`）。`forced` は「操作者が `--force` を指定した」ことで、ガードが実際に拒否したかどうかではない（それは
   `ownerless_groups` と `left_without_admin` で分かる）。`ownerless_groups` は **数** にした（グループの id は `forge doctor` の
   `users.administration` が `group_ids` で示す。監査の行の大きさを利用者の持つグループの数に比例させないため）。
5. **API**: `users::disable` は既定でガードを守る（`Guard::Enforce`。HTTP の経路はこれだけを使う）。`users::disable_with(…, Guard::Force)` は
   CLI の `--force` だけが使う。ガードが拒否したはずの無効化を強制したときは `DisableOutcome::DisabledPastGuard { ownerless_groups,
   left_without_admin }` を返し、CLI は孤立した状態と復旧の手順を表示する。
6. **ガードの数え方の細部**: `last_admin` は対象が有効な人間の管理者のときだけ判定する（管理者がもともといない状態で一般の利用者を
   無効化するのは拒否しない — 最後の管理者を失わせる操作ではないため。その状態は doctor が報告する）。両方に当たるときは `last_admin` を先に報告する。
   `last_owner` の誤りには `user`、最初のグループの `group_id`、`ownerless_groups`（数）を付ける。advisory lock は管理者でない利用者の
   無効化でも取る（2 人の Owner が互いを同時に無効化する競合を、行のロックだけでは防げないため）。
7. **doctor の `users.administration`**: 利用者が 1 人もいない（新しいインストール）は FG-AUTH-0010 にせず、コードなしの `warn`
   （「`forge admin bootstrap-admin` で最初の管理者を作る」）にした。失われたものはなく、インストール直後の `forge doctor` を毎回失敗させないため。
   利用者がいて有効な人間の管理者がいない（`reason=no_admin`）、または直接の有効な Owner のいない最上位のグループがある
   （`reason=ownerless_group`）ときに FG-AUTH-0010（warn、終了コード 1）。出力は数と最初の 10 個のグループの id だけ。読み取りの接続で読む。
8. **FG-AUTH-0010 の HTTP の状態**: doctor だけのコードだが、カタログの形（400〜599）に合わせて 500 にした（利用者には返さない）。
9. **D2 の LOW の修正**: `groups::members` の `unknown_role` を `FG-SYS-0001` から `FG-DB-0018 MembershipDataInvalid` にし、ロールの読み込み
   （`roles`）と同じ文脈（`resource=group`、`id`、`user`、`raw`、32 文字まで）を付けた。
10. **まだできないこと（Runbook に明記）**: 管理者を付与するコマンドと、グループのメンバーを変える操作の手段（HTTP の経路）はまだない。
    そのため「先に別の管理者・Owner を用意する」手順は、今は `--force` と `bootstrap-admin` での復旧になり、Owner のいない最上位のグループは
    メンバーの変更の手段ができるまで doctor が warn を出し続ける。
11. **グループの状態**（レビューの指摘、2026-10-30）: グループには今、アーカイブ・削除待ちの状態がない（`forge.groups` は状態の列を持たない。
    M1 ではグループを削除しない、計画の Q11）。D5 でそのような状態を足すときは、ガードの数え上げ（`users::admin::what_disable_leaves`）と
    doctor の `users::administration` の両方で、その状態のグループを除くこと（除かないと、消えかけのグループのために無効化が拒否される、
    または doctor が報告し続ける）。
12. **`actor_label` は自己申告**（レビューの指摘、変更なし）: CLI の監査の行の `actor_label = forge-cli:<OS のアカウント>` は環境変数
    `USERNAME` / `USER` から取り、実行する人が書き換えられる。ホストの操作者が信頼の起点（§8）なので、新しい依存（OS の API での照会など）は
    足さない。**「誰が実行したか」の正式な記録は Runbook の承認の記録**（`request_id` を残して監査の行と突き合わせる）。Runbook
    `user-offboarding` に明記した。
13. **HTTP から強制の経路を使えないことの検査**: `Guard::Force`・`disable_with`・`bootstrap_admin` が HTTP に出ないことを、文書だけでなく
    forge-db のテスト `forge_web_never_names_the_operator_only_user_commands`（`crates/forge-web/src` のソースを走査）で守る。
14. **`is_admin`・管理者の状態を変えるすべての経路は `lock_admin_set` を先に取る**: 今は `disable` と `bootstrap_admin`。管理者の付与・剥奪の
    コマンド、LDAP の同期、HTTP の経路を足すときも、§11 の順（`users.admin_set` → 対象の行）に従う（`users/admin.rs` のモジュールの説明）。
    `create` で `is_admin = true` の利用者を作る経路は管理者を増やすだけなので、最後の管理者の数え上げを壊さない。
15. **doctor の読み取りは 1 つの文**: `users.administration` は、利用者の数と Owner のいない最上位のグループを 1 つの SQL 文（CTE）で読み、
    同じスナップショットにした。REPEATABLE READ のトランザクションは `SET TRANSACTION` を要し、エージェントモデルのゲート
    db-write-boundary が `SET TRANSACTION` を `ChangeTx` の中に限っているため使わない。
16. **ガードを越えた無効化のログ**: `forge.admin.user.disabled` は、ガードが拒否したはずの強制（`outcome = disabled_past_guard`）のとき
    warn（`ownerless_groups`、`left_without_admin` 付き）、それ以外は info。
17. **最後の管理者の通常の退職**: 管理者を付与するコマンドがない間は、`--force` → 直後の `bootstrap-admin` になり、管理者のいない時間ができる。
    承認の時点で後任を決め、無効化の直後に復旧する（Runbook）。管理者の付与のコマンドは追跡中の後続の作業。

## Amendment（2026-10-31、手順 D4 MR2: プロジェクトの Owner と無効化）

- D4 MR2 でプロジェクトの直接の Owner（作成者）ができたが、`users::disable` の無効化のガード（§10〜12）と `forge doctor` の
  `FG-AUTH-0010` は、まだプロジェクトを数えない（MR2 では変えない）。
- プロジェクトの最後の Owner の方針（計画 D4 の回答 2: 継承された Owner — 祖先のグループの Owner、個人の名前空間の持ち主 — がいる間は拒まない）と、
  それが無効化と doctor に与える影響は、**D4 MR4（プロジェクトのメンバー）で決めて実装する**。詳細は ADR-0015 の D4 MR2 の Amendment。

## Amendment（2026-10-31、手順 D4 MR4: プロジェクトの最後の Owner と無効化）

D4 MR2 の Amendment が D4 MR4 に送った決定（計画 D4 の回答 2）。

- **プロジェクトの Owner** は `roles::load_project_roles` と同じ: 直接の Owner（`project_members`）、祖先のグループの直接の Owner、
  プロジェクトが直下にある個人の名前空間の持ち主（ボットを除く）。どれも有効な利用者だけを数える。
- **メンバーの変更**（`projects::{add_member, change_member, remove_member}`）: 最後の直接の Owner を外す・降格するのは、継承された
  有効な Owner がいる間は拒まない（本人が継承しているなら本人でもよい）。いなければ `FG-REPO-0010 last_owner`（`project_id`）。
  ロックの順は グループと同じ: プロジェクトの行 `FOR UPDATE` → 残る Owner の利用者の行 `FOR SHARE`（id の順）。
- **無効化のガード**（§10 の拡張）: `users::disable` は、その利用者がいなくなると有効な Owner（直接・継承）のいなくなるプロジェクトがあれば
  `FG-AUTH-0009 last_owner` で拒む（グループがなければ `project_id`。件数は `ownerless_groups` と `ownerless_projects` の両方）。
  `--force` は通し、`DisableOutcome::DisabledPastGuard.ownerless_projects` と `user.disabled` の `details.ownerless_projects` に記録する。
  数え方は §11 の順のまま: `users.admin_set` → 利用者の行 `FOR UPDATE` → ロックなしの数え上げ。**プロジェクトの行は取らない**
  （メンバーの変更は プロジェクト → 利用者 の順なので、逆順にするとデッドロックしうる）。利用者の行が、無効化と「その利用者を残る Owner と
  頼るメンバーの変更」を直列にする（ピン留めのテスト `a_disable_waits_for_a_project_owner_removal_and_then_refuses`・
  `a_project_owner_removal_waits_for_a_disable_and_then_refuses`）。
- **削除待ちのプロジェクト**（`deleted_at` あり、D5）はガードと doctor の両方で数えない（グループの将来の削除待ちの扱い、D3 MR2 の
  Amendment と同じ考え方）。
- **祖先のグループの Owner を外す変更**（`groups::remove_member` など）は、プロジェクトを数えない（変えない）。最上位のグループには常に
  直接の有効な Owner がいて、その Owner がグループの下のすべてのプロジェクトに継承されるので、グループのメンバーの変更だけで
  グループの下のプロジェクトの Owner がいなくなることはない（強制した無効化の後を除く。それは doctor が出す）。
- **doctor**: `forge_db::projects::ownerless(&Db) -> Option<Ownerless>`（読み取り用のプール、1 文）が、有効な Owner のいないプロジェクトの数と
  最初の 10 件の id を返し、`Ownerless::check()` が `FG-AUTH-0010 reason=ownerless_project`（`ownerless_projects`、`project_ids`）を返す。
  `forge doctor` の `users.administration` への配線（と `doctor/render.rs` の `SAFE_CONTEXT` への `ownerless_projects`・`project_ids` の追加）は、
  doctor を同時に変える D4 MR3b と衝突しないよう **D4 の統合のときに行う**。
- （D4 のレビュー）無効化のガードのプロジェクトの読み込みは、数と最初の 11 件の id だけを返す 1 行の問い合わせにした。どちらの問い合わせ（ガード、doctor）も
  集合で求める（祖先を 1 度たどり、Owner のいるプロジェクトを結合で集め、反結合で残りを出す。プロジェクトごとの相関副問い合わせはしない。テスト
  `ownerless_reads_hundreds_of_projects_set_based` が 300 件で数と計画を確かめる）。ボットの無効化ではプロジェクトを数えない（ボットは Owner にならない）。
- （D4 のレビュー）グループとプロジェクトの `change_member` / `remove_member` は、呼び出し側が `authorize()` で見た直接のロールを `from` として受け取り、
  ロックの下で読み直した値と違えば FG-REPO-0010 `role_changed`（`expected`、`actual`）で何も書かない。認可と変更の間の昇格（例: Owner への昇格の後に、
  Maintainer が認可済みの降格を書く）を防ぐ。

## Amendment（2026-11-01、M1 phase 9 MR3: 実際の git による確認）

「見送り」の e2e のうち 2 つを `crates/forge-cli/tests/smart_http_e2e.rs` に書いた: `git_clone_with_a_token_in_basic_auth_succeeds`
（credential helper がトークンを Basic のパスワードとして渡す）と `git_without_credentials_gets_www_authenticate_basic`（401 に
`WWW-Authenticate: Basic realm="GitStation"`、ないパスでも同じ応答）。`api` スコープだけのトークンは `read_repository` がないので
clone できない（403 `FG-AUTH-0002`、`an_api_scope_token_is_refused`）。`git_push_with_a_read_repository_token_is_refused` は
receive-pack（MR4）と一緒に書く。

## Amendment（2026-11-01、M1 phase 9 MR4: push の e2e）

`git_push_with_a_read_repository_token_is_refused`（`read_repository` と `api` のトークンでは push できない。receive-pack の
広告が 403 `FG-AUTH-0002`、本文は読まない）を `crates/forge-cli/tests/smart_http_e2e.rs` に書き、「見送り」の e2e はすべて
済んだ。`write_repository` のトークンでの push（作成・fast-forward・保護 ref の拒否）も同じファイルにある。管理者の権限は
`admin` スコープ付きのトークンでだけ効き、保護 ref の force push は管理者でも FG-GIT-0007（`force_push_to_protected_is_fg_git_0007_even_for_an_admin`）。

## Amendment（2026-11-01、M1 段階 8b MR7a: トークンの `is_agent` が認可に効く）

- 認証でトークンから作る `Principal` は `is_agent` を持つ（`Principal::authenticated(..).as_agent(identity.is_agent)`）。
  これまで `is_agent` は `GET /api/v1/user` で見せるだけだった。拒否される操作は ADR-0017 の Amendment（MR7a）を参照。
- 書き込みのトランザクションの中の再判定（`Authorized::reconfirm`）は、ユーザー・資格情報・スコープ・`is_agent` を引き継ぎ、
  有効か・管理者かだけを users 行（`FOR SHARE`）から読み直す。

## Amendment（2026-11-01、M1 段階 8b MR8: `is_agent` で拒否される操作が増える）

- `is_agent` のトークンは `ProjectVisibilityChange` に加えて `MemberManage` と `ProtectedRefManage` も拒否される（ADR-0017 の
  Amendment MR8）。トランザクションの中の再判定（`Authorized::reconfirm`）も `is_agent` を引き継ぐので、レジストリの判定の後に
  権限が変わっても同じ。
- メンバーと保護 ref の一覧・変更のルートは `read_api` / `api` のスコープ（`write_repository` だけのトークンは FG-AUTH-0002）。
- 最後の Owner の計算（`removes_last_owner`）はプロジェクトと最上位のグループだけ。サブグループは最上位の Owner を必ず継承する
  （無効化のガード FG-AUTH-0009 が最上位の唯一の有効な Owner を守る）ので対象外。ADR-0015 の Amendment（MR8）を参照。

## Amendment（2026-11-01、手順 D5 MR5: §8 の例外を運用の移動・名前の変更のコマンドへ広げる）

§8 の「権限の単一判定点（CLAUDE.md §3.4）の明示的な例外」は、トークンの CLI に加えて、次の運用のコマンドにも適用する
（D5 MR5 のレビューの指摘。設計は ADR-0015 の「D5 MR5」）。

- 対象: `forge admin project move`、`forge admin group move`、`forge admin user rename`（`forge-cli/src/admin_namespace.rs`）。
  先例として `forge admin user disable`、`bootstrap-admin`、`repo reprovision`、`project purge --now` も同じ扱い（どれも
  `authorize()` を通らず、actor `system`、監査の `actor_label = forge-cli:<OS のアカウント>`）。
- **`authorize()` を通らない理由**: 呼び出し元はホストの操作者で、HTTP / git の主体（`Principal`）がない。判定するロールを持つ
  利用者がいない。本番では承認済みの Runbook の手順（`namespace-redirects`）としてだけ実行する（CLAUDE.md §3.7）。
- **HTTP から到達できない理由**: 入口は `forge-hosting` の `Hosting::operator_move_project` / `operator_move_group` /
  `operator_rename_user` だけで、どれも `ChangeContext` の actor が `system` でなければ `FG-SYS-0001 reason=operator_not_system`
  で断る。forge-web の書き込みは必ず `Actor::User`（forge-hosting `change_context`）で、`system` の文脈を作る経路を持たない。
  forge-db の側でも、`RateLimit::Exempt` は actor が `system` でなければ `rate_limit_exempt_not_system` で断る（二重の守り）。
  forge-db の `*_decided` を判定なしの閉包で呼べるのは forge-hosting だけ（テスト `decided_callers`）。
- **回数の制限の外**: これらのコマンドは名前空間ごと 24 時間 10 回の制限（FG-REPO-0018）の対象外（手順 D5 の計画の答え:
  運用の CLI だけが除外）。それ以外の規則（部分木の上限、深さ、パスの長さ、循環、移動先の種類、衝突、Owner がいなくならないこと）、
  墓標、監査の行、移動のイベントは API と同じ。
- **この例外は CLI の経路だけ**（§8 と同じ）: HTTP の移動・名前の変更は `ProjectTransfer` / `GroupTransfer` / `UserRename` の
  `authorize()` と権限の表の行で判定する。
