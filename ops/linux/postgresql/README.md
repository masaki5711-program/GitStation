# GitStation の PostgreSQL（PFS と同じサーバーに置くとき）

GitStation は PostgreSQL **16 以上**を使う（16 未満は起動時に拒否）。PFS（IoT 基盤）も PostgreSQL 16 + TimescaleDB を同じサーバーで動かすので、
**どちらの形にするかを PFS チームと決める**。決めるのに要る事実は `ops/runbooks/linux-install.md` の「PFS チームに確認すること」。

## 結論（推奨）

**A. GitStation 専用のインスタンス（推奨）**。PFS のクラスターとは別のポート・別のデータ領域で、もう 1 つ PostgreSQL 16 を動かす。

| 理由 | 内容 |
|---|---|
| バックアップが独立する | `forge backup` の pgBackRest は **クラスター全体**の WAL を保管する（`archive_mode` / `archive_command` はクラスター単位）。PFS のクラスターに相乗りすると、PFS のデータまで GitStation のバックアップに入り、PFS 側の WAL の保管とも衝突する |
| bootstrap に superuser が要る | `ops/db/bootstrap/01〜03` は superuser で流す（ADR-0018）。クラスターのロール `forge_*` 8 つを作る。専用なら PFS の superuser を借りない |
| 設定を GitStation に合わせられる | `max_connections`、`random_page_cost`、ログの設定を PFS に影響させずに決められる |
| 障害の切り分け | 片方の DB の負荷・停止がもう片方を巻き込まない |

代わりに、メモリを約 1 GiB 余分に使う（下の設定）。メモリが足りないときだけ B にする。

**B. PFS のクラスターに相乗り（別 DB・別ロール）**。PFS チームの承認と、PFS の DBA の作業が要る。

## A: 専用インスタンスの作り方

どちらか一方:

- **コンテナ（PFS が Docker / Podman を使っている場合）**: `compose.yml`（このディレクトリ）。PFS と同じイメージ
  `timescale/timescaledb:2.28.1-pg16`、`127.0.0.1:55433`（PLACEHOLDER）、メモリを絞った起動引数。
- **ホストのパッケージ**: ディストリビューションの PostgreSQL 16 で 2 つ目のクラスターを作る
  （Debian / Ubuntu: `pg_createcluster 16 forge --port 55433`、RHEL 系: 別の `PGDATA` と `postgresql-16@forge` のようなユニット）。
  設定は `forge-postgresql.conf` を `conf.d/` に置く。

メモリを絞った設定（`forge-postgresql.conf` と `compose.yml` で同じ値）:

| 設定 | 値 | 理由 |
|---|---|---|
| `shared_buffers` | 256MB | GitStation の DB は小さい（メタデータ・監査・ジョブ。リポジトリの中身はディスク） |
| `effective_cache_size` | 768MB | プランナーへの目安（実際には確保しない） |
| `work_mem` | 4MB | 1 つのソート・ハッシュごと。forge の問い合わせは小さい |
| `maintenance_work_mem` | 64MB | VACUUM・索引の作成 |
| `max_connections` | 40 | 下の「接続数」 |
| `random_page_cost` | 1.1 | SSD。既定の 4 では連携の feed が索引を使わない（2026-11-09 に観測） |
| `max_wal_size` | 1GB | 書き込みは少ない |

### 接続数

forge の各プロセス（`serve`、`worker`）は、書き込み用と読み取り用の 2 つのプールを持つ（`database.write_max_connections` /
`database.read_max_connections`、例の設定では 5 / 5）。`forge doctor` の `db.connections` は
`(write + read) × 2 + 3` が **サーバーの使える接続（`max_connections − superuser_reserved_connections`）の 80 % 以下**であることを確かめ、
超えると `FG-SYS-0002 reason=connection_budget`。

- 例の設定: (5 + 5) × 2 + 3 = 23 → 使える接続が 29 以上 → `max_connections = 40`（予約 3 を引いて 37）。
  残りは `forge backup`、`forgectl`、pgBackRest、`psql` の分。
- プールを大きくするときは `max_connections` も合わせて上げる（1 接続あたり数 MB のメモリを使う）。

## B: PFS のクラスターに相乗りするとき

1. **別の DB**: `02-create-database.sql` を `-v dbname=forge` で流す（DB の所有者は流した superuser）。PFS の DB には何も作らない。
2. **ロール**: `01-cluster-roles.sql` が `forge_owner`・`forge_migrator`・`forge_seq`・`forge_purge`・`forge_app`・`forge_ro`・`forge_maint`・`forge_audit`
   を作る。ロールはクラスター全体のもの。同じ名前が既にあれば止まる（runbook db-bootstrap 手順 1）。PFS 側に同名が無いことを先に確かめる。
3. **接続の上限**: `db.connections` は**クラスター全体の** `max_connections` で判定する。PFS が実際に使っている接続数は数えないので、
   PFS の最大の使用量 + GitStation の 23 + 余裕 が `max_connections` に収まることを PFS チームと確かめる。足りなければプールを
   3 / 3（(3+3)×2+3 = 15）に下げる。
4. **pg_hba.conf**: `forge_app`・`forge_migrator`・`forge_maint` を `forge` DB にだけ、`scram-sha-256` で許す。
   ```
   host  forge  forge_app,forge_migrator,forge_maint  127.0.0.1/32  scram-sha-256
   ```
5. **バックアップ**: pgBackRest の stanza は PFS のクラスターに 1 つしか持てない（`archive_command` は 1 つ）。PFS が既に
   pgBackRest や別の WAL アーカイブを使っているなら、GitStation の `forge backup` の DB の部分は PFS のバックアップに任せる設計の変更になる
   （**エスカレーション**: runbook backup-restore の前提が崩れる）。
6. 設定（`work_mem` など）は PFS チームのもの。GitStation のために変えない。

## どちらでも

- 同じホストのループバックなら `database.tls_mode = "disable"` を使える。別ホストなら `verify-full` と社内 CA（`database.tls_root_cert`）。
- パスワードは 3 つ（`forge_app`・`forge_migrator`・`forge_maint`）とも別の値にし、`\password` で入れる（値を記録に残さない）。
  置き場は `ops/runbooks/linux-install.md` の手順 4。
