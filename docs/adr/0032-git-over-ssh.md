# ADR-0032: SSH の git 転送 — `forge serve` の中の SSH サーバー、Smart HTTP の経路への橋渡し

**Date**: 2026-11-13
**Status**: proposed（accepted は C13 `feat/git-ssh-transport` の `git-protocol-reviewer`・`security-reviewer` のレビューで CRITICAL / HIGH が無いとき）
**Deciders**: Masaki Fujikawa（ユーザー、2026-11-10「推奨で進めて」: 計画 14 の C13）、Claude

## Context

計画 01 §3.1 の「SSH での clone / push、デプロイキー」（Should、M4）と計画 14 の C13（M4 の終盤、R1 の外）。いまの git の入口は Smart HTTP だけで、その経路にはすでに次がある（`crates/forge-web/src/routes/git.rs`・`git_push.rs`）:

- ディスパッチャーがパスを解き、`authorize()`（`RepoRead` / `RepoPush`）で決める。ref ごとの判定（保護された ref、予約の名前空間、直接の push の禁止）は `authorize_ref_updates`。
- push: コマンドの厳しい解析、保護の写し、`receive.denyNonFastForwards`、pre-receive のチェック（秘密・大きなファイル・履歴の取り込み、`forge hook pre-receive`）、push の記録（`repository.pushed`）、閲覧のキャッシュの破棄、切れた push の照合。
- fetch: want が ref から届くかの確認、AI のエージェントへの機密の複製の拒否（`RepoCopy`）、ストリームの上限と時間切れ（`git.http.*`）。

SSH を足すときに、これを **二つ目の実装として持たない**（契約 4: 判定点は 1 つ、計画: 「smart-http 相当」）ことが要点。

## Decision

1. **`forge serve` の中の SSH サーバー（Rust の russh）にする**。`git.ssh.enabled`（既定 `false`）で起動し、`git.ssh.bind`（既定 `127.0.0.1:2224`）で待つ。OpenSSH の sshd は使わない（§Alternatives）。
   - russh は `>= 0.65.1`、`default-features = false, features = ["ring"]`（aws-lc-rs と C のツールチェーンを入れない。ring はすでに依存にある）、`rsa` の機能は入れない（RUSTSEC-2023-0071 の `rsa` を木に入れない）。
   - 調べたこと（2026-11-13）: ライセンスはすべて `deny.toml` の許可リストの中（`cargo deny check licenses bans sources` が ok。cargo-deny 0.20.2）。追加される 92 個のクレートの版に OSV（RustSec を含む）の既知の脆弱性は無い。`cargo deny check advisories` が赤なのは main にもある rustix 1.1.5 の RUSTSEC-2026-0486（unsound、本件と無関係）だけ。
   - russh には 2026 年に事前認証の DoS・panic の修正が多い（GHSA が 20 件以上、すべて 0.65.1 までに修正）。この履歴を受け入れる代わりに: 既定は無効、`git.ssh.bind` は社内のネットワークだけ、認証の前の接続数・時間・回数を絞る（§4）、CI の `cargo audit` / `cargo deny` で新しい勧告を検出したら上げる。
2. **鍵と主体**:
   - 人の公開鍵は `forge.user_ssh_keys`（監査対象、所有者に `ssh_key.added` / `ssh_key.deleted`）。受け付けるのは Ed25519 と ECDSA（P-256 / P-384 / P-521）だけ。RSA・DSA・セキュリティキー・証明書は FG-AUTH-0023 で断る。鍵は SHA-256 の指紋（`SHA256:…`）で一意（同じ鍵を 2 人は使えない。誰のものかは言わない）。1 人 50 個まで。
   - 追加はブラウザのセッションだけ（Action `SshKeyAdd`、セッション専用。トークンは鍵を足せない: トークンを取り消しても残る資格情報を作らせない）。一覧（`SshKeyList`、`read_api`）と削除（`SshKeyDelete`、`api`）は自分のものだけ（他人のものは管理者でも 404）。Web の画面はアカウントの設定の「SSH 鍵」タブ。`gstn` のコマンドは作らない（追加がセッション専用のため、今は価値が小さい）。
   - SSH で認証した人の主体は **`Principal::ssh_key`**: スコープ `read_repository` + `write_repository` のトークンと同じに判定する（新しい `Credential` は作らない）。管理者の権利は無い（`admin` スコープが無い）。API の操作はすべて FG-AUTH-0002。エージェントの資格情報ではない（サービスアカウント・ボットは `users.kind` でエージェントの相手として扱われる。ただし鍵を足せるのはセッションを持つ人だけ）。作るのは forge-web の SSH の層だけ（テスト `only_forge_web_builds_ssh_principals`）。権限の表には SSH の鍵の行（許可と拒否）を `matrix_rows_ssh.rs` に置く。
   - 所有者が無効のとき、鍵は残るが認証しない（FG-AUTH-0004）。再び有効にすると使える（鍵は利用者が持ち込んだ公開情報で、トークンのようにサーバーが発行した秘密ではない）。
   - ホスト鍵は Ed25519 を 1 つ（`git.ssh.host_key_file`）。**`forge admin ssh-host-key generate` で 1 回だけ作り**（既存のファイルは上書きしない、Unix では 0600）、`forge serve` は読むだけ（無い・壊れている・他人も読める → FG-SYS-0002 で起動しない）。指紋と `known_hosts` の行は `forge admin ssh-host-key show` と SSH 鍵の画面に出す。利用者は初回の接続でこの指紋と照らす。
3. **git のコマンドは Smart HTTP の経路で処理する（橋渡し）**。
   - 許すのは `git-upload-pack '<path>'` と `git-receive-pack '<path>'`（`git upload-pack` の綴りも）だけ。パスは `A-Z a-z 0-9 . _ - /` の区切り、空・`.` で始まる区切り（`..` を含む）を拒否、512 バイトまで。それ以外は FG-GIT-0033（`not_git` / `malformed` / `path_invalid`）。シェルは渡さない（何も実行しない）。`shell` の要求は「誰として認証されたか」を表示して終わる（`ssh -T git@host` で鍵を確かめられる）。pty・subsystem（sftp）・X11・エージェント・ポート転送は断る。環境変数は `GIT_PROTOCOL` だけ受ける。
   - コマンドは **製品のルーター（HTTP と同じ `Router`）にプロセスの中で HTTP の要求として渡す**。要求の拡張に SSH の鍵の主体（ネットワークの入力からは設定できない）と接続元のアドレスを入れる。だから判定点（ディスパッチャー → `prove` → `authorize()`）、保護された ref、pre-receive、push の記録、want の確認、機密の複製の拒否、ストリームの上限・時間切れ（`git.http.*`）は Smart HTTP と **同じコード** が動く。
     - `git-upload-pack`: **プロトコル v2 だけ**。`GET {path}.git/info/refs?service=git-upload-pack`（v2 の能力の広告）、その後は v2 の要求（flush まで）ごとに `POST {path}.git/git-upload-pack`。v0 / v1 は FG-GIT-0034（v0 の状態を持つ交渉は Smart HTTP の 1 往復ごとの要求に写せない。git 2.26 以降の既定は v2 で、OpenSSH は `GIT_PROTOCOL` を送る）。
     - `git-receive-pack`: `GET …?service=git-receive-pack` の広告から HTTP の `# service=` の行を除いて返し、コマンドの並びと pack（クライアントが EOF を送るまで。すべて削除のときは pack が無く、git は閉じないので flush で止める）を 1 つの `POST {path}.git/git-receive-pack` の本文にする。
   - HTTP のエラーの答え（401 / 403 / 404 / 503）は `ERR <FG コード> <利用者向けの文> (request_id: <HTTP の要求の ID>)` の 1 行にして返す（git は `fatal: remote error: …` と出す）。ログはルートがすでに出している。答えを読めないときだけ SSH の層が FG-GIT-0036 `reason=bridge` を出す。
   - push の記録（`Pusher.credential_id`）はトークンの ID の欄なので SSH では空。どの鍵かは SSH の層のログ `forge.ssh.command`（`request_id`、`ssh_key_id`、`user_id`、`http_request_ids`）で、HTTP の要求の ID から辿る。User-Agent は `gitstation-ssh`。
4. **上限**（FG-GIT-0035、ログは 10 秒に 1 行と抑えた件数）: 接続の総数 `git.ssh.max_connections`（64）、接続元ごと `git.ssh.max_connections_per_peer`（8、IPv6 は /64 ごと）、接続元ごとの新しい接続 `git.ssh.connections_per_minute_per_peer`（60/分）、認証できずに終わった接続が `git.ssh.auth_failures_per_minute_per_peer`（20/分）に達した接続元はその分の残りを断る。SSH の識別子の交換と認証は `git.ssh.auth_timeout_ms`（30 秒）以内、鍵の試行は `git.ssh.max_auth_attempts`（6）まで、無通信は `git.ssh.inactivity_timeout_ms`（10 分、`git.http.idle_timeout_ms` 以上）。1 接続のセッションのチャネルは 4 つまで。git のストリームの上限（`git.http.max_streams` など）はそのまま効く。
5. **エラーコード**（計画 14 の GIT-next）: FG-GIT-0033 SshCommandRefused（info）、FG-GIT-0034 SshProtocolV2Required（info）、FG-GIT-0035 SshConnectionRefused（warn、runbook `ssh-transport`）、FG-GIT-0036 SshTransportFailed（alert、runbook `ssh-transport`）、FG-AUTH-0023 SshKeyInvalid（info）。設定の問題は FG-SYS-0002（`git.ssh.*`）。
6. **止め方**: `forge serve` は HTTP と SSH を同じシグナルで止める。SSH は新しい接続を受けず、接続に切断を送り、10 秒まで待つ。受け取り済みの push は Smart HTTP と同じく自分のタスクで最後まで動く（`push_tasks`）。HTTP が動いているのに SSH の待ち受けが終わったら FG-GIT-0036 `reason=server_stopped`。

## Alternatives Considered

### OpenSSH の sshd + `AuthorizedKeysCommand` + 強制コマンド `forge git-shell`
- **Pros**: 認証の前の攻撃面が最も枯れた実装（OpenSSH）。新しいクレートが要らない。GitLab と同じ形。
- **Cons**: 運用が増える（専用の sshd かポートの分離、root 所有の `AuthorizedKeysCommand`、sshd の設定・ホスト鍵・上限は forge の設定の外）。`forge git-shell` は別のプロセスなので、Smart HTTP と同じ経路を通すには forge serve にループバックの HTTP で「信頼された内部の主体」を渡す新しい仕組みが要り、それ自体が攻撃面になる（さもなければ push の経路を二つ持つことになり契約 4 に反する）。実際の `git clone` / `push` の E2E テストに sshd が要り、Windows の開発機と CI の両方で動かしにくい（`AuthorizedKeysCommand` は root 所有の実行ファイルを求める）。上限・時間切れ・ログが forge のエラーコードの外に出る。
- **Why not**: 「Claude が成果物だけで診断できる」こと（ログとコードが一か所）と、判定点が 1 つであることを優先した。russh の勧告の履歴は §1 の対策で受ける。

### russh で SSH の層から git のサブプロセスを直接動かす
- **Pros**: 橋渡しが要らず、v0 の upload-pack もそのまま動く。
- **Cons**: pre-receive・保護・記録・want の確認・機密の複製の拒否を Smart HTTP の外にもう一つ持つことになる。
- **Why not**: 契約 4 と計画の「smart-http 相当」に反する。

### 鍵の主体に新しい `Credential::SshKey` を作る
- **Pros**: 監査の行・ログで「SSH から」と型で分かる。
- **Cons**: `authorize()` とその周りのすべての網羅的な match に新しい腕が要り、判定はトークン（`read_repository` + `write_repository`）と同じになる。
- **Why not**: 判定が同じものを二つ書かない。SSH の区別はログ（`forge.ssh.*`、HTTP の要求の User-Agent）で足りる。必要になったら Amendment で足す。

## Consequences

### Positive
- SSH の git の操作は HTTPS と同じ判定・同じ push の経路・同じ記録になる。保護の規則やチェックを足すと SSH にもそのまま効く。
- 設定・ホスト鍵・上限・ログ・エラーコードがすべて forge の中にあり、runbook `ssh-transport` で診断できる。
- 実際の `git clone` / `push` を OpenSSH のクライアントで通す E2E テストが、sshd なしで Windows と Linux で動く（`crates/forge-cli/tests/ssh_e2e.rs`）。

### Negative
- russh と RustCrypto の新しい世代（rc 版を含む）の約 90 個のクレートが依存に入る（ビルドの時間とメモリ）。
- russh の認証の前の不具合は forge serve のプロセスに及ぶ（panic はセッションのタスクの中で止まるが、メモリの枯渇は全体に効く）。§4 の上限と既定の無効で小さくする。
- v0 / v1 の fetch は SSH では使えない（HTTPS は使える）。
- RSA の鍵は使えない（利用者は Ed25519 を作る必要がある）。

### Risks
- russh の新しい勧告: CI の `cargo audit` / `cargo deny` で検出し、上げる。上げられないときは `git.ssh.enabled = false` にする（HTTPS は影響を受けない）。
- SSH の層の不具合で、ルーターに渡す要求の拡張（主体）を誤る: `Principal::ssh_key` 以外を作らないことはテストで固定し、ディスパッチャーは HTTP と同じ判定をする。

## Follow-ups
- デプロイキー（リポジトリごとの読み取り専用の鍵、計画 01 §3.1）は別の MR。
- 管理者による他人の鍵の一覧・削除と、無効化のときの鍵の扱いは必要になったら Amendment。
- `gstn` からの鍵の一覧・削除は、要望があれば足す。
