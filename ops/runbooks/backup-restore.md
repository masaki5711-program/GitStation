# Runbook: backup-restore（FG-SYS-0008 / FG-SYS-0009）

GitStation のバックアップ（`forge backup`）と復元（`forge restore …`）、**復元の訓練**の手順。M3 O1（計画 11 §5.5 O1・§10 Q11、
ADR-0019 Amendment「計画 11 P1」、ADR-0024 末尾の表 #26・#41・#44）。

- **保管先はローカルのディレクトリ**（`backup.dir`。別のディスクを推奨）。世代は既定 7（`backup.generations`）。NAS・別のサーバーへの転送は後の作業。
- **DB は pgBackRest**、**リポジトリはファイルの複写**（暗号化した 1 つのアーカイブ）。DB が先、その後でリポジトリ。
- バックアップはトークンのハッシュ・利用者の情報・全ソースを含む。**暗号化して保存し、権限を forge の運用のアカウントだけに絞る**。
- 次の操作のうち `[MUTATING — needs approval]` の付いたものは本番を変える。本番では **人が承認した手順としてだけ** 実行する（CLAUDE.md §3.7）。
  `forge backup` は製品の状態を変えないが、保管先と定期実行の設定はこの runbook で決める。

## Trigger

- ログに **FG-SYS-0008** BackupFailed（alert）: `forge backup` が完全な世代を書けなかった。`reason` で原因が分かる（下の表）。
- `forge doctor` の **`backup.last_success`** が FG-SYS-0008 `reason=never`（世代が 1 つも無い）/ `overdue`（最新が `backup.max_age_hours`、
  既定 26 時間より古い）/ `insecure_permissions`。**`backup.state_copy`** が warn（権限の状態の写しが無い・古い）。
- ログに **FG-SYS-0009** RestoreVerificationFailed（alert）: `forge restore verify` / `finish` が DB とリポジトリの食い違いを見つけた、
  または復元の手順が動けなかった。`forge serve` / `forge worker` が `reason=restore_pending` で起動しない（復元が終わっていない）。
- 障害（DB・ディスクの喪失）で復元が要るとき、と **定期の訓練**（四半期に 1 回を推奨）。

## Impact

- バックアップの失敗そのものはサービスを止めない。**次の障害で戻せる点が古くなる**（最後の成功の時点まで失う）。SEV3。2 日続けば SEV2。
- 復元中は GitStation 全体が止まる（serve と worker は印のファイルがある間は起動しない）。復元の後、**すべての利用者がログインし直し、
  トークン・サービスアカウントのトークン・Webhook の秘密を発行し直す**（下の「再発行」）。

## 仕組み（読むだけ）

```
<backup.dir>/                       0700、forge の運用のアカウントの持ち物
  .lock                              実行中の印（create-new。残っていたら下の A）
  20261109T021500Z/                  完全な世代（名前は DB の時計の UTC）
    manifest.json                    中身の一覧（秘密なし。pgBackRest のラベル、DB の手順が終わった DB の時刻、リポジトリ、アーカイブの sha256）
    repositories.fgbk                全リポジトリ（AES-256-GCM、1 MiB ごとの塊、改ざん・欠けを検出）
  20261110T021500Z.partial/          失敗した回の残り（次の回が消す）
<backup.state_copy_dir>/             0700、backup.dir の外（別のディスク・別のマウント）
  state-<時刻>-<マイクロ秒>.fgbk     権限の状態の写し（暗号化、48 個まで）
<backup.key_file>                    0600、backup.dir の外。64 桁の 16 進 1 行（32 バイト）
```

1. 保存先・写しの置き場・鍵のファイルの権限を確かめる（グループ・他人に権限がある、持ち主が違う、シンボリックリンク → FG-SYS-0008 `insecure_permissions`。
   鍵や写しが `backup.dir` の中 → `key_inside_backup`）。
2. **DB**: `pgbackrest --stanza=<stanza> --type=full --log-level-console=warn backup`、続けて `pgbackrest --stanza=<stanza> --output=json info`
   から新しいラベルを読む（時間切れ `backup.db_timeout_secs`、超えたら殺す）。
3. **リポジトリ**: ready のリポジトリを 1 つずつ、そのリポジトリの保守（O3）と同じロック `repo_maint:<repo_id>` を取ってから（保守中なら
   待つ。保守の側は複写中なら `skipped backup_running`）、**ref を先に、オブジェクトを後に** 作業用のディレクトリへ複写し、`git fsck --connectivity-only`
   で確かめ（保守の gc が途中でパックを入れ替えたら最大 3 回まで複写し直す）、アーカイブに足す。ロック・`gc.pid`・push の隔離は複写しない。
   `refs/merge-requests/*`（版の ref）も含む。
4. `manifest.json` を最後に書き、`.partial` を外し、古い世代を `backup.generations` まで消す。
5. 権限の状態の写しを 1 つ書く（`backup.state_copy_dir` があるとき）。worker も `backup.state_copy_interval_secs`（既定 300 秒、
   **最小 60 秒**・最大 3600 秒。範囲の外は FG-SYS-0002）ごとに書く。

**権限の状態の写し**（ADR-0019 Amendment 手順 2）: 利用者の無効化・管理者の印、グループとプロジェクトの所属とロール、保護 ref と
マージの規則、承認の規則、マージの設定。復元は、この写しにある **狭める変更だけ** を当て直す（写しは置き換えられうるファイルなので、
権限を広げる方向には決して使わない）。写しが無い・読めない・バックアップより古いときは **すべての管理者の印を外す**。

## 設定

```toml
[backup]
dir = "/srv/forge-backup"                 # 別のディスク。chmod 0700、chown forge:forge
key_file = "/etc/forge/backup.key"        # chmod 0600。backup.dir の外。鍵の写しは金庫（紙・別の保管）にも
state_copy_dir = "/srv/forge-state"       # chmod 0700。backup.dir の外
state_copy_interval_secs = 300            # 既定 300。60〜3600（60 未満は FG-SYS-0002）
generations = 7
pgbackrest_bin = "/usr/bin/pgbackrest"
pgbackrest_stanza = "forge"
db_timeout_secs = 3600
repo_timeout_secs = 3600
max_age_hours = 26
```

鍵を作る: `umask 077; openssl rand -hex 32 > /etc/forge/backup.key`。**鍵を失うとバックアップは読めない**。鍵はバックアップと別の場所
（金庫・別のサーバー）にも保管する。

pgBackRest（`/etc/pgbackrest/pgbackrest.conf`、forge の外。**この環境では未確認**、試験環境で確かめる）:

```ini
[global]
repo1-path=/srv/forge-backup/pgbackrest   # backup.dir の中でよい（同じディスク・同じ権限）
repo1-cipher-type=aes-256-cbc
repo1-cipher-pass=<別の秘密。backup.key とは別に保管>
repo1-retention-full=7                     # backup.generations と同じに
start-fast=y
compress-type=zst

[forge]
pg1-path=/var/lib/postgresql/16/main
pg1-port=5432
```

PostgreSQL 側: `archive_mode = on`、`archive_command = 'pgbackrest --stanza=forge archive-push %p'`、初回だけ
`pgbackrest --stanza=forge stanza-create` と `pgbackrest --stanza=forge check`。`forge backup` を動かす OS のアカウントが pgBackRest の
設定とリポジトリを読めること（forge と postgres が別のアカウントなら `pgbackrest` は `sudo -u postgres` の包みを `backup.pgbackrest_bin` に置く）。

**PostgreSQL がコンテナのとき**（試験環境の `gs-test-pg` など。**この環境では未確認**）: pgBackRest は PG のコンテナの中に入れ
（`pg1-path` はコンテナの中のデータのパス、`repo1-path` はホストの `backup.dir` の下をマウントした先）、`backup.pgbackrest_bin` には
`docker exec` の包みのスクリプトを置く。forge は引数をそのまま渡す（`--stanza=… backup` と `--stanza=… --output=json info`）。

```sh
#!/bin/sh
# /usr/local/bin/forge-pgbackrest（chmod 0755）。backup.pgbackrest_bin = "/usr/local/bin/forge-pgbackrest"
exec docker exec -u postgres <PG のコンテナ> pgbackrest "$@"
```

`docker` を使えるアカウントは実質 root と同じ権限を持つ。forge の運用のアカウントに与えるかは承認して決める。

定期の実行（systemd timer の例、毎日 02:15）:

```
[Service]
User=forge
ExecStart=/usr/local/bin/forge --config /etc/forge/forge.toml backup
```

**復元のコマンドには別の設定ファイル**: `forge restore finish` は epoch を進めるため `database.maint_url` と
`database.maint_password_file`（`forge_maint`）が要る。一方 `forge serve` / `forge worker` は設定に `database.maint_*` /
`database.migrate_*` の秘密のファイルがあると起動しない（FG-SYS-0002 `config_key=database.maint_password_file`
`reason=not_for_this_command`）。そこで `/etc/forge/forge.toml` を写した `/etc/forge/forge-restore.toml`（0600）に `[database]` の
`maint_url` と `maint_password_file` を足し、**復元のコマンドだけ** `forge --config /etc/forge/forge-restore.toml restore …` で流す
（試験環境の `forge-migrate.toml` と同じ考え方）。serve と worker の設定には足さない。

## Diagnose（読み取りのみ）

1. ログを `error_code=FG-SYS-0008` / `FG-SYS-0009` と `request_id` で探す。鍵・トークン・パスワードはログに出ない
   （pgBackRest の stderr は伏せ字の先頭だけ）。
2. `forge doctor` の `backup.last_success` / `backup.state_copy`。
3. `ls -l <backup.dir>`（完全な世代は `manifest.json` を持つ）、`cat <世代>/manifest.json`、`pgbackrest --stanza=forge info`、
   pgBackRest のログ（`/var/log/pgbackrest`）。
4. 復元の状態: `forge restore verify`（読むだけ。いつでも流せる）。

| FG-SYS-0008 の reason | 意味 | 直し方 |
|---|---|---|
| `not_configured` | `backup.dir` か `backup.key_file` が無い | 「設定」のとおりに置く |
| `insecure_permissions` | 置き場がグループ・他人に開いている／持ち主が違う／シンボリックリンク（`path`・`mode`・`detail`） | `chmod 0700`、`chown forge:forge`。権限を広げて通さない |
| `key_inside_backup` | 鍵か写しの置き場が `backup.dir` の中 | 外へ移す |
| `backup_running` | `<backup.dir>/.lock` がある | A |
| `db_tool_missing` / `db_backup_failed` / `db_backup_timeout` / `db_backup_unexpected` | pgBackRest が無い・失敗・時間切れ・ラベルが読めない | pgBackRest のログを読む。`pgbackrest --stanza=forge check` |
| `repository_copy_failed` / `repository_copy_timeout` | リポジトリの読み・アーカイブの書きの失敗／時間切れ（`repo_id`・`operation`） | ディスクの空き・I/O のエラー。`repo_timeout_secs` |
| `repository_copy_inconsistent` | 複写したリポジトリの fsck が 3 回とも失敗 | 保守（repo-maintenance）が動いていないときに流し直す。元のリポジトリの `git fsck` |
| `storage_io` | 世代の作成・名前の変更・回転の失敗（`operation`） | ディスク |
| `never` / `overdue` | 世代が無い／古い | 定期の実行（timer）が動いているか、直前の FG-SYS-0008 |

FG-SYS-0002 `config_key=backup.key_file`（`insecure_permissions` / `invalid`）は鍵のファイルの権限・中身の問題。

## Mitigation

### A. 残ったロック `[MUTATING — needs approval]`

`ps -ef | grep 'forge.*backup'` で動いていないことを確かめてから `rm <backup.dir>/.lock`。`.partial` は次の回が消す。

### B. 手でバックアップを取る

```
forge backup
forge doctor        # backup.last_success が ok
```

## 復元（障害のとき・訓練のとき）`[MUTATING — needs approval]`

**順序を変えない。** 途中で止まったら同じ手順から流し直してよい（どの手順も繰り返せる）。incident-commander が指揮する（SEV1〜2）。
`forge restore …` はすべて復元の設定ファイル（上の「復元のコマンドには別の設定ファイル」）で流す:
`forge --config /etc/forge/forge-restore.toml restore …`（下では `--config` を省く）。

**前提**: 手順 12 の連携の有効化とサービスアカウントの作成・トークンの発行は **管理者の Web セッションだけ**（トークンは
FG-AUTH-0002 `reason=session_required`）。復元はすべてのセッションを失効させるので、手順 11 で管理者が **Keycloak 経由でサインインし直す**。
Keycloak（とその DB）が動いていること、サインインできる管理者が 1 人以上残ること（手順 8）を先に確かめる。

1. **止める**: `systemctl stop forge-serve forge-worker`。
2. **印を付ける**: `forge restore begin`（`<storage.root>/.forge-restore-pending` を書く。これがある間 serve と worker は
   FG-SYS-0009 `restore_pending` で起動しない）。
3. **世代を選ぶ**: `ls <backup.dir>`、`cat <backup.dir>/<g>/manifest.json`（`db.label` が pgBackRest のラベル）。
4. **DB を戻す**（PostgreSQL を止めて）。**戻す点はその世代のバックアップの点**（`--type=immediate`: バックアップが一貫した
   ところで WAL の再生を止める）。`--set` だけでは pgBackRest はアーカイブにある WAL を **最後まで再生する**（既定 `--type=default`）。
   すると DB が世代のリポジトリより新しくなり（`version_ref_missing`）、権限の写しの当て直しの起点（世代の `db.completed_at`、
   ADR-0019 Addendum O1）とも合わない。`--target-action=promote` も付ける（pgBackRest の既定 `pause` では読み取り専用のまま止まる）。
   ```
   systemctl stop postgresql
   sudo -u postgres pgbackrest --stanza=forge --set=<db.label> --type=immediate --target-action=promote --delta restore
   systemctl start postgresql
   ```
   **PG がコンテナのとき**（2026-11-11 の訓練で確認。リポジトリは `<backup.dir>/pgbackrest` をマウントした形）: PG のコンテナを止め、
   同じイメージ・同じデータのボリューム・pgBackRest の設定とリポジトリのマウントで 1 回だけのコンテナを走らせてから、PG のコンテナを起動する。
   ```
   docker stop <PG のコンテナ>
   docker run --rm -u postgres -v <データのボリューム>:<pg1-path の親> -v <backup.dir>/pgbackrest:<repo1-path> \
     -v /etc/pgbackrest:/etc/pgbackrest:ro <PG のイメージ> \
     pgbackrest --stanza=forge --set=<db.label> --type=immediate --target-action=promote --delta restore
   docker start <PG のコンテナ>
   ```
   `--delta` は空のディレクトリに対して警告を出すだけで、そのまま進む（データのボリュームを新しく作った場合も同じ）。
   **データのボリュームを失ったとき**（既存のボリュームは使えない）: 次の順で行う。`docker start` で元のコンテナを起こしてはいけない（失ったボリュームを指したまま）。
   1. 新しいデータのボリュームを作る（`docker volume create <新しいボリューム>`）。
   2. そのボリュームに 1 回だけのコンテナで pgbackrest の restore を流す（上の `docker run --rm …` の `-v` の `<データのボリューム>` を `<新しいボリューム>` に替える）。
   3. 元の PG のコンテナと同じイメージ・環境変数・ポート・ネットワークで、**新しい PG のコンテナを `docker run` で作り**、`<新しいボリューム>` をマウントして起動する。
      元のコンテナは止めたままにしておく（記録のために残してよい）。
   4. 以降の手順 5〜7 は同じ。
   （バックアップの点より後まで戻すのは、**リポジトリを失っていない**（手順 5 を飛ばす）ときだけ: 最後まで再生（`--type=immediate`
   を付けない）か、時点を指定する PITR（`--type=time --target=<時刻> --target-action=promote`）。どちらにしたかを記録に残す。
   その場合も下の 6〜7 はそのまま。）
5. **リポジトリを戻す**（リポジトリも失ったとき。DB だけを戻すときは飛ばす）:
   ```
   forge restore repositories --generation <g>            # 既にあるリポジトリは断る
   forge restore repositories --generation <g> --replace  # 今のものを <storage.root>/pre-restore-<g>/ へ退避して入れ替える
   ```
   先に sha256 を確かめ、変わっていれば何も書かずに FG-SYS-0009 `archive_corrupt`。
6. **検証**（読むだけ）: `forge restore verify`。

   | 見つかったもの | 意味 | すること |
   |---|---|---|
   | `repository_missing` | ready の行があるのにディレクトリが無い | 手順 5（`--replace`） |
   | `repository_corrupt` | `git fsck --connectivity-only` の失敗（FG-GIT-0019 も出る） | 古い世代から戻す、repo-maintenance |
   | `version_ref_missing` / `version_ref_mismatch` | DB の版（`merge_request_versions`）の ref がリポジトリに無い・違う = **リポジトリが DB より古い** | 同じ世代のリポジトリを戻す（手順 5） |
   | `purged_ref_present` | 秘密の削除で消したはずの版の ref が戻った | **secret-removal** の手順で消し直す（その後で 6 から） |
   | `version_ref_unknown` / `repository_unknown` | DB が知らない版の ref・リポジトリ = **DB がリポジトリより古い**（DB だけを戻した） | 下の「DB が古いとき」 |

   DB が知っている MR の `refs/merge-requests/<iid>/head`（最新の版）と `refs/merge-requests/<iid>/merge`（マージ可否の検査が作る
   マージの結果。検査のたびに作り直し、閉じた・マージした MR のものは worker が消す）は比べない。DB が知らない MR のものと、
   それ以外の名前（`v<n>`・`head`・`merge` でない）は `version_ref_unknown` に出る。

   **DB が古いとき**（DB だけを古い時点に戻した）: バックアップの後の push はリポジトリにあり DB には無い。MR の版・push の記録は
   DB に戻らない。リポジトリを同じ世代に戻す（手順 5 の `--replace`。後の push は失う）か、新しいリポジトリを残して進める
   （`--accept-db-behind`。一覧の ref は DB が知らないまま残る。開いている MR は次の push で版が足される。残った `refs/merge-requests/<iid>/v<n>`
   が新しい版の ref とぶつかったら、その MR を閉じて開き直す）。どちらにするかは incident-commander が決め、記録に残す。
7. **終える**:
   ```
   forge --config /etc/forge/forge-restore.toml restore finish --generation <g> [--accept-db-behind] [--allow-older-copy]
   ```
   `database.maint_url` の無い設定では epoch の手順で失敗する（上の「復元のコマンドには別の設定ファイル」）。
   検証をもう一度流し、次をすべて行い、最後に印を消す。**手順ごとに繰り返せる**: 途中で止まったら同じコマンドをもう一度流す
   （印のファイルの `begin=` で同じ復元と分かり、`restore.finished` に `rerun: true`、管理者の一覧は前の実行の行から出る）。
   - **すべての資格情報を失効**: トークン（個人もサービスアカウントも）`revoked_reason=restore`、Web のセッション、Webhook は
     `disabled_reason=restore` で無効（秘密がバックアップの後に入れ替えたものかもしれない）、**push ミラーと連携（kanflow）も無効**
     （バックアップの後に外した・止めたものを戻さないため）、保留中の代理実行の主張は `reject_reason=restore`。監査の行
     `restore.credentials_revoked` / `restore.webhooks_disabled` / `restore.push_mirrors_disabled` / `restore.integration_disabled` /
     `restore.assertion_rejected`。
   - **権限の縮小を当て直す**（`backup.state_copy_dir` の写しのうち、世代の `db.completed_at` 以後の最新）: 無効化・管理者の印の削除・
     所属の削除・ロールの引き下げ・保護 ref の追加と `push_min_role` の引き上げ・マージの規則（承認数・必須チェック・未解決の議論・
     直接の push の禁止・作業項目のリンク）・承認の規則・マージの設定（`reset_approvals_on_push`・`committer_approvals_excluded`・
     `codeowners_enforced`）・承認できる人（写しと共通の人に切る）・プロジェクトの公開範囲（`internal` → `private`）・チェックの
     名前空間の許可（写しに無いものを消す）。監査の行 `restore.reapplied`（プロジェクト・グループと、所属は各利用者にも）。
   - **写しが使えないとき**（無い・古い・**最新の写しが読めない**）: **すべての利用者の管理者の印を外し**、外した利用者を一覧に出す
     （`restore.admin_flags_removed`）。最新の写しが読めない理由を確かめた後でだけ、`--allow-older-copy` で読める古い写しを使える
     （最新の写しにあった縮小は失われるので、手で確かめる）。
   - 当て直しで有効な管理者が 0 人になったら報告に出る: 手順 8 か `forge admin bootstrap-admin --user <username>`。
   - **イベントの履歴に新しい epoch**（`feed.epoch_rotated`、FG-DB-0014）: すべての連携のカーソルが 410 `epoch_mismatch` になり、
     kanflow の GitStationSource は `/cursor` と開発状況の API から読み直す（`docs/api/integration-feed.md`）。`database.maint_url` が要る。
   - 監査の行 `restore.finished`（世代・件数。秘密なし）。
8. **管理者を付け直す**（印を外したとき）: 一覧の各人を確かめて（本人であること・まだ管理者であるべきこと）、
   ```
   forge restore regrant-admin <username>
   ```
   最新の `restore.finished` の一覧に無い利用者、一覧の後に変更（無効化・名前の変更・付け直し済みなど）のあった利用者、無効な利用者・
   人でない利用者は断る（FG-AUTH-0009 `not_removed_by_restore` / `not_active_human`）。断られたら確かめ直して `bootstrap-admin`（管理者が
   いないとき）か管理者の画面で付ける。**`regrant-admin` は手順 10（起動）の前に流す**: 起動の後は利用者のログインなどの
   ふだんの監査の行も「後の変更」に数えられ、断られる。写しから当て直して外した印（`restore.reapplied` だけを書き、
   `restore.admin_flags_removed` の一覧には入らない）は `regrant-admin` で戻せない: 本当に要るなら `bootstrap-admin`
   （管理者が 1 人もいないとき）か、残った管理者が付け直す。
9. **Keycloak を確かめる**: Keycloak で無効の利用者が GitStation で有効になっていないか（ログインは Keycloak が断るが、
   `forge admin user disable <username>` で揃える）。当て直せなかった利用者は無効のままにする（有効に倒さない）。
10. **起動**: `systemctl start forge-worker forge-serve`（コンテナなら `docker start`。設定は **ふだんの** `forge.toml`、
    復元の設定ファイルではない）。**1〜2 分待ってから** `forge doctor`。
    復元の間（worker が止まっていた間）の監査の行は封印されていない。5 分より古い未封印の行があると `audit.seal_backlog` が
    FG-DB-0015 で失敗する。worker の封印のタスクは 10 秒ごとに見て、前の封印から 60 秒たつ（または 1000 行を超える）と封印し、
    溜まった行は 5000 行ずつ続けて封印する。`context` の `unsealed_numbered` が 0 になれば ok。`unsequenced > 0` かつ
    `audit_sequencing=off` は待っても消えない（番号付けが止まっている）: runbook audit-seal-backlog の「想定どおりの場合」と Diagnose 2。
    `audit.seal_backlog` の判定（`forge-db` の `seal/backlog.rs`）は `audit_sequencing` を見ない。**最も古い未封印の行**（最後の封印より後に
    番号を振られた行、または番号のない行）が 5 分を超えると、`audit_sequencing=off` のままでも FG-DB-0015 で失敗する。
    起動の直後（1〜2 分）は番号のない行だけでも ok（詳細 `nothing sealed yet … unnumbered rows waiting, audit sequencing off`）になりうるが、
    これは判定の窓の中だからで、番号付けが動いている証拠ではない。`audit_sequencing=off` の間は、5 分を過ぎた後の FG-DB-0015 も想定どおり
    （待って ok にはならない。直すのは番号付けの有効化＝C9 の比較の後）。
    `feed.sink_compare` の警告（「database restore の後に `forge admin events new-epoch`」）は古い文言: `restore finish` が epoch を既に進めている
    （`feed.epoch_rotated`）。**`new-epoch` は流さない**（流すと、すべての連携のカーソルがもう一度 410 `epoch_mismatch` になる）。
11. **管理者がサインインし直す**: Keycloak が動いていることを確かめ（keycloak-unreachable）、手順 8 で残った・付け直した管理者が
    ブラウザで Keycloak 経由でサインインする（復元の前のセッションはすべて失効している）。サインインできる管理者がいなければ
    手順 12 の連携とサービスアカウントは進められない: `forge admin bootstrap-admin --user <username>` で付けてから。
12. **再発行**（下。連携の有効化とサービスアカウントは手順 11 のセッションで）。

## 再発行（復元の後）

**管理者の Web セッションが要る行**（トークンでは FG-AUTH-0002 `reason=session_required`）: サービスアカウントの作成・トークンの発行と、
連携の有効化。管理者が Keycloak 経由でサインインし直してから（復元の手順 11）、ブラウザのコンソールで流す（service-accounts の A・B、
integrations）。順序: ①管理者のサインイン → ②サービスアカウントのトークンの発行 → ③連携の有効化 → ④kanflow に新しいトークンを設定。

| 資格情報 | 誰が | 手順 | 配る相手 |
|---|---|---|---|
| Web のセッション | 各利用者 | ログインし直す | — |
| 個人のトークン（`gstn` など） | 各利用者 | 画面の「トークン」か `gstn auth login` で新しく発行。運用の経路は `forge admin token issue --user <name>`（runbook service-accounts） | 本人の端末・CI の設定 |
| サービスアカウントのトークン | その所有者（**管理者の Web セッション**） | service-accounts の発行の手順（ブラウザのコンソール。運用の経路は `forge admin token issue --user <name>`） | **試験サーバーのゲート（`ci/gates`）**、**kanflow**（`GitIntegration` のトークン） |
| Webhook の署名の秘密 | プロジェクトの Maintainer | `gstn webhook rotate-secret <id> --project <p>` → 受け手に配る → `gstn webhook update <id> --project <p> --enable` | 各 Webhook の受け手 |
| 代理実行の HMAC の鍵（ファイル） | 運用者 | act-as の「鍵の入れ替え」（新しい kid を active に） | kanflow は鍵を持たない（GitStation の中だけ） |
| ミラーの資格情報（ファイル） | 運用者 | GitLab でミラー用のトークンを作り直し、`mirror.credentials_dir` のファイルを入れ替える（gitlab-mirror B） | GitLab |
| push ミラー（`finish` が無効にした） | 管理者 | まだ要るものだけ `gstn mirror set --project <p> …` で設定し直す（有効に戻り、全部の ref を 1 回送る） | GitLab |
| 連携（kanflow、`finish` が無効にした） | 管理者（**Web セッションだけ**） | まだ要る連携だけ、管理の API（`PATCH /api/v1/admin/integrations/{id}`、`{"enabled": true}`、ヘッダー `X-GitStation-CSRF`）をサインインしたブラウザのコンソールから流して有効に戻す。サービスアカウントのトークンは上の行で先に再発行 | kanflow |
| バックアップの鍵 | 運用者 | 漏れの疑いがあるときだけ: 新しい鍵を作り、`forge backup` を流し、古い鍵で作った世代を消す | — |

## 復元の訓練（四半期に 1 回）

試験環境（m3-test-environment）で、本番と同じ手順を **合成のデータ** で通す。記録（日時・世代・かかった時間・`forge restore verify` の
出力・見つかった問題）を `ops/incidents/` の訓練の記録に残す。直近の記録: `ops/incidents/2026-11-11-drill-backup-restore.md`（使い捨ての `gs-drill`、2 回）。

1. 試験環境で `forge backup`。`forge doctor` の `backup.last_success` が ok。
2. バックアップの後に: トークンを 1 つ失効、利用者を 1 人無効化、プロジェクトの所属を 1 つ削除、ロールを 1 つ下げる、管理者の印を 1 つ外す、
   保護ブランチを 1 つ足す。worker が写しを書くのを待つ（`backup.state_copy` の時刻が新しくなる。最短 60 秒、既定 300 秒）。
   **管理者の印を外す製品の操作（API・CLI）はまだ無い**（付けるのは `bootstrap-admin` / `regrant-admin` だけ）。訓練では
   **試験環境の DB でだけ** 次の SQL で外す（本番では流さない。監査の行と同じトランザクションで変える）:
   ```sql
   -- 試験環境だけ。<username> は合成の利用者
   BEGIN;
   INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, resource_id, request_id)
     SELECT 'system', 'drill.admin_flag_removed', 'succeeded', 'user', id, 'drill-<日付>'
       FROM forge.users WHERE username = '<username>';
   UPDATE forge.users SET is_admin = false, updated_at = now() WHERE username = '<username>';
   COMMIT;
   ```
3. 「復元」の手順 1〜12 を通す（DB は pgBackRest で `--type=immediate` で戻す）。
4. 確かめる: 失効したトークンで認証できない、どのトークンでも認証できない、無効化・所属の削除・ロールの引き下げ・印の削除・保護ブランチが
   戻っている、`forge restore verify` が consistent、kanflow の開発タブが読み直して追いつく（410 `epoch_mismatch` から回復）。
5. 写しを置かない回も 1 度: 管理者の印がすべて外れ、一覧が出て、`regrant-admin` で戻せる。

自動のテスト（このリポジトリ、合成のデータ。pgBackRest の代わりに偽の DB の手順）:
`backup_then_delete_then_restore_then_verify`、`a_database_behind_the_repositories_is_detected`、
`repositories_behind_the_database_are_detected`、`the_merge_result_ref_of_a_known_merge_request_is_no_finding`、
`restore_does_not_resurrect_revoked_credentials`、
`restore_reapplies_post_backup_role_changes`、`without_a_state_copy_every_admin_flag_is_removed_and_listed`、
`wide_permission_storage_is_refused`、`rotation_keeps_n_generations`、`no_secrets_in_logs_or_in_the_backup_files`。

## Verify

- `forge doctor`: `backup.last_success` ok（最新の世代と経過時間）、`backup.state_copy` ok。
- 復元の後: `forge restore verify` が consistent、`forge doctor` 全体、監査に `restore.finished` が 1 行。

## Escalate

- 鍵を失った・どの世代も `archive_corrupt`: SEV1。incident-commander。古い世代・pgBackRest だけで戻せる範囲を決める。
- `purged_ref_present`（秘密の削除が戻った）: security の担当者へ。secret-removal の手順。

## Feedback

障害・訓練のたびに postmortem-writer。見つかった穴は、テスト・doctor の項目・この runbook・エラーの目録の修正のどれかで返す。
