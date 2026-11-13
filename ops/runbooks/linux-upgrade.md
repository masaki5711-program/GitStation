# Runbook: linux-upgrade（Linux サーバーの更新と、前の版への戻し）

`linux-install` で入れた GitStation を新しいリリースに替える手順と、問題があったときに前の版へ戻す手順。
配置（`/opt/gitstation/releases/<版>/` と `/opt/gitstation/current`）は `ops/linux/README.md`。

- 本番を変える手順はすべて `[MUTATING — needs approval]`（CLAUDE.md §3.7）。コマンドは root（`sudo -i`）で実行する。
- **停止を伴う**（serve と worker を止めてマイグレーションを流す）。利用者に時間を知らせる。目安は数分（マイグレーションの量による）。

## Trigger

- 新しいリリース（`gitstation-<版>-<target>.tar.gz`）を入れるとき。
- `forge doctor` / ログが、直した不具合の入ったリリースへの更新を求めるとき（障害の対処として）。

## Impact

- 手順 4〜7 の間、GitStation の Web・API・git は使えない（SEV にはしない計画停止）。実行中の push は止める前に終わるのを待つ（最大 180 秒）。
- **マイグレーションを含む更新は、バイナリだけでは戻せない**（下の「戻し」B）。

## 準備（読み取りのみ・停止なし）

1. リリースのファイルを確かめて、`current` に触れずに展開する。`[MUTATING — needs approval]`（新しいディレクトリを足すだけ。ロールバック: そのディレクトリを消す）

   ```sh
   cd /tmp && sha256sum -c gitstation-<新版>-<target>.tar.gz.sha256
   tar -xzf gitstation-<新版>-<target>.tar.gz -C /opt/gitstation/releases
   mv /opt/gitstation/releases/gitstation-<新版>-<target> /opt/gitstation/releases/<新版>
   NEW=/opt/gitstation/releases/<新版>
   readlink -f /opt/gitstation/current        # 今の版を記録する（戻し先）
   $NEW/bin/forge --version && cat $NEW/BUILD-INFO
   ```

2. 新しい版がどのマイグレーションを持つか（読み取りのみ。DB は変えない）:

   ```sh
   forgectl --release <新版> db status
   ```

   - `pending -` なら**マイグレーションなし**: 戻しはリンクの付け替えだけ（戻し A）。
   - `pending <版の一覧>` なら、runbook `db-migration` の「版ごとの注意」で該当の版を読む（worker を止めてから流すものがある）。

3. 設定・ユニットの差分を確かめる:

   ```sh
   for f in forge-serve.service forge-worker.service forge-backup.service forge-backup.timer; do
     diff -u /etc/systemd/system/$f $NEW/share/systemd/$f
   done
   diff -u /opt/gitstation/current/share/etc/forge.toml.example $NEW/share/etc/forge.toml.example
   diff -u /usr/local/sbin/forgectl $NEW/share/bin/forgectl
   ```

   新しいキーが要るなら、下の手順 1（止める）の後・手順 2 の前に `/etc/forge/forge.toml` に足す（古い版は未知のキーで起動しないので、
   止める前には足さない。戻し A では足したキーを消す）。

4. 直前のバックアップを取る（戻し B の戻り先）:

   ```sh
   systemctl start forge-backup.service && journalctl -u forge-backup -o cat -n 20
   forgectl doctor                            # backup.last_success が ok、時刻が今
   ```

   失敗したら更新しない（runbook `backup-restore`）。

## 手順

1. **止める** `[MUTATING — needs approval]`（ロールバック: `systemctl start forge-worker forge-serve`）

   ```sh
   systemctl stop forge-serve forge-worker     # serve は実行中の push を待つ（最大 180 秒）
   systemctl is-active forge-serve forge-worker forge-backup.service   # すべて inactive
   pgrep -u forge || echo none                   # none（forgectl の db migrate は forge のプロセスがあると拒否する）
   ```

2. **マイグレーション**（`pending` があるときだけ）`[MUTATING — needs approval]`（ロールバック: 下の「戻し」B）

   ```sh
   forgectl --release <新版> db migrate
   forgectl --release <新版> db status --check     # 終了コード 0、serve compatible
   ```

   失敗（FG-DB-0003）: 各マイグレーションは 1 つのトランザクションなので何も変わっていない。runbook `db-migration` で原因を読み、
   直せなければ `systemctl start forge-worker forge-serve` で古い版のまま再開する。

3. **切り替える** `[MUTATING — needs approval]`（ロールバック: 戻し A）

   ```sh
   ln -sfn $NEW /opt/gitstation/current.new && mv -T /opt/gitstation/current.new /opt/gitstation/current   # 一度に入れ替わる
   install -m 0755 $NEW/share/bin/forgectl /usr/local/sbin/forgectl
   # 手順 3 でユニットに差分があったときだけ（古いものは .bak に残す）:
   #   cp /etc/systemd/system/forge-serve.service /etc/systemd/system/forge-serve.service.bak
   #   install -m 0644 $NEW/share/systemd/forge-serve.service /etc/systemd/system/
   systemctl daemon-reload
   ```

4. **起動する** `[MUTATING — needs approval]`（ロールバック: 戻し A / B）

   ```sh
   systemctl start forge-worker forge-serve
   journalctl -u forge-serve -o cat -n 20      # forge.serve.listening の version が新しい版
   ```

## Verify

- `forgectl doctor` が FAIL 0、`web.ui` が `embedded`。
- `curl -fsS https://<ホスト名>/healthz` が `{"status":"ok"}`、ブラウザで開いてログインできる。
- 試しのリポジトリで `git fetch` / `git push` が通る。
- 10 分後に `journalctl -u forge-serve -u forge-worker --since -10min -o cat | grep -c '"level":"ERROR"'` が増えていない。
- 前の版のディレクトリは**消さない**。直近 3 つを残し、それより古いものは 1 週間問題がなければ消す
  （動いている serve が使うバイナリは `/proc/<pid>/exe` の先。その版は消さない）。

## 戻し（前の版へ）

どちらも `[MUTATING — needs approval]`。`OLD=<準備 1 で記録した今の版のパス>`。

### A. マイグレーションを流していない（`pending -` だった、または手順 2 の前）

```sh
systemctl stop forge-serve forge-worker
ln -sfn $OLD /opt/gitstation/current.new && mv -T /opt/gitstation/current.new /opt/gitstation/current
install -m 0755 $OLD/share/bin/forgectl /usr/local/sbin/forgectl
# ユニットを替えていたら .bak を戻して daemon-reload
systemctl daemon-reload && systemctl start forge-worker forge-serve
forgectl doctor
```

### B. マイグレーションを流した後

古いバイナリは、自分の知らない版を持つ DB では起動しない（`FG-DB-0004 reason=db_ahead`。runbook `db-migration`）。down マイグレーションは無い（ADR-0014）。
次のどちらかを、障害の担当と開発者（Claude）で決めて承認を取る:

1. **前へ直す（第一の選択）**: 不具合を直したリリースを作り、この runbook で入れる。新しい版のまま、問題の機能だけを避けて運用できるなら待つ。
2. **データごと戻す**: 準備 4 のバックアップから DB とリポジトリを復元し（runbook `backup-restore`）、戻し A でバイナリを戻す。
   **準備 4 の後の変更（push、MR、コメント）は失われ、すべての利用者のトークンとセッションが失効する**。最後の手段。

## Escalate

- `db migrate` が `sql_failed` を繰り返す、`checksum_mismatch` / `dirty`: runbook `db-migration` の Escalate。
- 新しい版で `forge serve` が起動しない（`FG-WEB-0004` など、リリースの不具合）: 戻し A（流していなければ）または B、
  そして評価（`/review`）とテストの抜けとして還元策を起票する。
- glibc の誤り（`GLIBC_2.xx not found`）: ビルド機が本番より新しい。本番と同じ OS の版でビルドし直す（`ops/linux/README.md` の「ビルド」）。
