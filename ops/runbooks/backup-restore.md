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
3. **リポジトリ**: ready のリポジトリを 1 つずつ、**ref を先に、オブジェクトを後に** 作業用のディレクトリへ複写し、`git fsck --connectivity-only`
   で確かめ（保守の gc が途中でパックを入れ替えたら最大 3 回まで複写し直す）、アーカイブに足す。ロック・`gc.pid`・push の隔離は複写しない。
   `refs/merge-requests/*`（版の ref）も含む。
4. `manifest.json` を最後に書き、`.partial` を外し、古い世代を `backup.generations` まで消す。
5. 権限の状態の写しを 1 つ書く（`backup.state_copy_dir` があるとき）。worker も `backup.state_copy_interval_secs`（既定 300 秒）ごとに書く。

**権限の状態の写し**（ADR-0019 Amendment 手順 2）: 利用者の無効化・管理者の印、グループとプロジェクトの所属とロール、保護 ref と
マージの規則、承認の規則、マージの設定。復元は、この写しにある **狭める変更だけ** を当て直す（写しは置き換えられうるファイルなので、
権限を広げる方向には決して使わない）。写しが無い・読めない・バックアップより古いときは **すべての管理者の印を外す**。

## 設定

```toml
[backup]
dir = "/srv/forge-backup"                 # 別のディスク。chmod 0700、chown forge:forge
key_file = "/etc/forge/backup.key"        # chmod 0600。backup.dir の外。鍵の写しは金庫（紙・別の保管）にも
state_copy_dir = "/srv/forge-state"       # chmod 0700。backup.dir の外
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

定期の実行（systemd timer の例、毎日 02:15）:

```
[Service]
User=forge
ExecStart=/usr/local/bin/forge --config /etc/forge/forge.toml backup
```

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

1. **止める**: `systemctl stop forge-serve forge-worker`。
2. **印を付ける**: `forge restore begin`（`<storage.root>/.forge-restore-pending` を書く。これがある間 serve と worker は
   FG-SYS-0009 `restore_pending` で起動しない）。
3. **世代を選ぶ**: `ls <backup.dir>`、`cat <backup.dir>/<g>/manifest.json`（`db.label` が pgBackRest のラベル）。
4. **DB を戻す**（PostgreSQL を止めて）:
   ```
   systemctl stop postgresql
   sudo -u postgres pgbackrest --stanza=forge --set=<db.label> --delta restore
   systemctl start postgresql
   ```
   （時点を指定する PITR は `--type=time --target=<時刻>`。その場合も下の 5〜7 はそのまま。）
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

   **DB が古いとき**（DB だけを古い時点に戻した）: バックアップの後の push はリポジトリにあり DB には無い。MR の版・push の記録は
   DB に戻らない。リポジトリを同じ世代に戻す（手順 5 の `--replace`。後の push は失う）か、新しいリポジトリを残して進める
   （`--accept-db-behind`。一覧の ref は DB が知らないまま残る。開いている MR は次の push で版が足される。残った `refs/merge-requests/<iid>/v<n>`
   が新しい版の ref とぶつかったら、その MR を閉じて開き直す）。どちらにするかは incident-commander が決め、記録に残す。
7. **終える**:
   ```
   forge restore finish --generation <g> [--accept-db-behind]
   ```
   検証をもう一度流し、次をすべて行い、最後に印を消す:
   - **すべての資格情報を失効**: トークン（個人もサービスアカウントも）`revoked_reason=restore`、Web のセッション、Webhook は
     `disabled_reason=restore` で無効（秘密がバックアップの後に入れ替えたものかもしれない）。監査の行 `restore.credentials_revoked` /
     `restore.webhooks_disabled`。
   - **権限の縮小を当て直す**（`backup.state_copy_dir` の写しのうち、世代の `db.completed_at` 以後の最新）: 無効化・管理者の印の削除・
     所属の削除・ロールの引き下げ・保護 ref の追加と `push_min_role` の引き上げ・マージの規則（承認数・必須チェック・未解決の議論・
     直接の push の禁止・作業項目のリンク）・承認の規則・マージの設定（`reset_approvals_on_push`・`committer_approvals_excluded`・
     `codeowners_enforced`）。監査の行 `restore.reapplied`。
   - **写しが使えないとき**: **すべての利用者の管理者の印を外し**、外した利用者を一覧に出す（`restore.admin_flags_removed`）。
   - **イベントの履歴に新しい epoch**（`feed.epoch_rotated`、FG-DB-0014）: すべての連携のカーソルが 410 `epoch_mismatch` になり、
     kanflow の GitStationSource は `/cursor` と開発状況の API から読み直す（`docs/api/integration-feed.md`）。`database.maint_url` が要る。
   - 監査の行 `restore.finished`（世代・件数。秘密なし）。
8. **管理者を付け直す**（印を外したとき）: 一覧の各人を確かめて（本人であること・まだ管理者であるべきこと）、
   ```
   forge restore regrant-admin <username>
   ```
   一覧に無い利用者・無効な利用者・人でない利用者は断る（FG-AUTH-0009 `not_removed_by_restore` / `not_active_human`）。
9. **Keycloak を確かめる**: Keycloak で無効の利用者が GitStation で有効になっていないか（ログインは Keycloak が断るが、
   `forge admin user disable <username>` で揃える）。当て直せなかった利用者は無効のままにする（有効に倒さない）。
10. **起動**: `systemctl start forge-worker forge-serve`、`forge doctor`。
11. **再発行**（下）。

## 再発行（復元の後）

| 資格情報 | 誰が | 手順 | 配る相手 |
|---|---|---|---|
| Web のセッション | 各利用者 | ログインし直す | — |
| 個人のトークン（`gstn` など） | 各利用者 | 画面の「トークン」か `gstn auth login` で新しく発行。運用の経路は `forge admin token issue --user <name>`（runbook service-accounts） | 本人の端末・CI の設定 |
| サービスアカウントのトークン | その所有者 | service-accounts の発行の手順 | **試験サーバーのゲート（`ci/gates`）**、**kanflow**（`GitIntegration` のトークン） |
| Webhook の署名の秘密 | プロジェクトの Maintainer | `gstn webhook rotate-secret <id> --project <p>` → 受け手に配る → `gstn webhook update <id> --project <p> --enable` | 各 Webhook の受け手 |
| 代理実行の HMAC の鍵（ファイル） | 運用者 | act-as の「鍵の入れ替え」（新しい kid を active に） | kanflow は鍵を持たない（GitStation の中だけ） |
| ミラーの資格情報（ファイル） | 運用者 | GitLab でミラー用のトークンを作り直し、`mirror.credentials_dir` のファイルを入れ替える（gitlab-mirror B） | GitLab |
| バックアップの鍵 | 運用者 | 漏れの疑いがあるときだけ: 新しい鍵を作り、`forge backup` を流し、古い鍵で作った世代を消す | — |

## 復元の訓練（四半期に 1 回）

試験環境（m3-test-environment）で、本番と同じ手順を **合成のデータ** で通す。記録（日時・世代・かかった時間・`forge restore verify` の
出力・見つかった問題）を `ops/incidents/` の訓練の記録に残す。

1. 試験環境で `forge backup`。`forge doctor` の `backup.last_success` が ok。
2. バックアップの後に: トークンを 1 つ失効、利用者を 1 人無効化、プロジェクトの所属を 1 つ削除、ロールを 1 つ下げる、管理者の印を 1 つ外す、
   保護ブランチを 1 つ足す。worker が写しを書くのを待つ（`backup.state_copy` の時刻が新しくなる）。
3. 「復元」の手順 1〜10 を通す（DB は pgBackRest で戻す）。
4. 確かめる: 失効したトークンで認証できない、どのトークンでも認証できない、無効化・所属の削除・ロールの引き下げ・印の削除・保護ブランチが
   戻っている、`forge restore verify` が consistent、kanflow の開発タブが読み直して追いつく（410 `epoch_mismatch` から回復）。
5. 写しを置かない回も 1 度: 管理者の印がすべて外れ、一覧が出て、`regrant-admin` で戻せる。

自動のテスト（このリポジトリ、合成のデータ。pgBackRest の代わりに偽の DB の手順）:
`backup_then_delete_then_restore_then_verify`、`a_database_behind_the_repositories_is_detected`、
`repositories_behind_the_database_are_detected`、`restore_does_not_resurrect_revoked_credentials`、
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
