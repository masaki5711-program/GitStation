# Runbook: cli-distribution（FG-WEB-0012 / FG-WEB-0013）

GitStation の画面（設定 → アクセストークン →「CLI（gstn）をダウンロード」）と
`GET /api/v1/clients`・`GET /api/v1/clients/gstn/{target}/binary` で配布する `gstn` の置き場所と、
配布できないときの調べ方。

## 置き場所

| 項目 | 値 |
|------|----|
| ディレクトリ | 設定 `clients.dir`（環境変数 `FORGE_CLIENTS_DIR`、絶対パス）。未設定なら `<forge の実行ファイルのディレクトリ>/../share/forge/clients` |
| 試験サーバーのイメージ | `/usr/local/share/forge/clients`（`ops/gitlab-trial/Dockerfile` のステージ `gstn` が作る。`forge.toml` には書かない） |
| ファイル | `gstn-linux-x86_64`、`gstn-windows-x86_64.exe`、`manifest.json` |
| 対象（target） | `linux-x86_64`、`windows-x86_64` の 2 つだけ。URL の `{target}` はこの名前と照合するだけで、パスとしては使わない |

`manifest.json` はイメージのビルドが書く（サーバーは何もビルドしない）。

```json
{"clients":[{"name":"gstn","target":"linux-x86_64","version":"0.1.0+<commit 12 桁>","sha256":"<64 桁の小文字 16 進>","size":<バイト数>}]}
```

- マニフェストにない対象、ファイルがない対象は一覧に出ない（ダウンロードは 404 `FG-WEB-0012`）。
- ディレクトリかマニフェストがなければ「配布なし」（一覧は空。警告のログ `forge.clients.no_manifest`、`FG-WEB-0012`）。
- マニフェストに対象があるのに 1 つも配れない（すべて欠けている・大きさが違う）ときは、一覧が `500 FG-WEB-0013 reason=nothing_servable`。
- ダウンロードの途中で失敗したとき（読み取りの失敗、1 回の読み取りが 10 秒・全体が 10 分を超えた、ファイルが途中で短くなった）は、
  `forge.clients.download_incomplete`（`reason`、`sent`、`expected`）が 1 回だけ出る。利用者には途中で切れたファイルが届くので、SHA-256 の照合で気づける。
- 起動時のログ `forge.serve.clients_dir` に、使うディレクトリが出る。

## Trigger

- `error_code=FG-WEB-0012`（info）: 配布していない対象を求められた。利用者の操作か、イメージに対象がない。
- `error_code=FG-WEB-0013`（warn）: マニフェストが読めない・形式が違う・ファイルの大きさがマニフェストと違う・読み取りが時間切れ。
- ログ `forge.clients.entry_skipped`: 一覧から外した対象（`reason=file_missing` か大きさの不一致）。

## Impact

`gstn` の新規ダウンロードができない。すでに入っている `gstn`、git、画面のほかの機能には影響しない。

## Diagnose（読み取りのみ）

1. ログの `reason`・`target`・`dir` を読む（`rule` はマニフェストのどの規則に反したか: `json`、`name`、`target`、`duplicate_target`、`sha256`、`version`、`size`）。
2. 置き場所を見る（試験サーバー）:

   ```
   docker exec gitstation-trial-serve-1 ls -l /usr/local/share/forge/clients
   docker exec gitstation-trial-serve-1 cat /usr/local/share/forge/clients/manifest.json
   docker exec gitstation-trial-serve-1 sha256sum /usr/local/share/forge/clients/gstn-linux-x86_64
   ```

   コンテナ名は `docker ps` で確かめる。大きさ（`ls -l`）と SHA-256 をマニフェストと比べる。
3. `clients.dir` を設定している環境では、その値と、forge のユーザーがそこを読めるかを確かめる。

## Mitigate（承認が必要）

- パイプラインがビルドしたイメージをデプロイし直す（`deploy:pfs-trial`、Runbook gitlab-trial-deploy）。
  バイナリとマニフェストは同じビルドで一緒に作られる。
- **サーバー上でマニフェストやバイナリを手で書き換えない。** 中身の確かめようがなくなる。
- Windows の対象がイメージにない場合は、ビルドのログ（ステージ `gstn`、`x86_64-pc-windows-gnu`）を見る。

## Verify

```
curl -fsS -H "Authorization: Bearer <read_api のトークン>" <base_url>/api/v1/clients
```

で対象が並び、ダウンロードしたファイルの SHA-256 が `X-Checksum-Sha256` と一覧の `sha256` に一致する。

## 未対応（open）

- マニフェストの署名（Ed25519）と、Windows 版の Authenticode 署名はまだない。今は HTTPS とトークンでの配布と、一覧・ヘッダーの SHA-256 の照合だけで守っている。
- サーバーは配るときにバイトを読み直して SHA-256 を計算しない（大きさだけ照らし合わせる）。マニフェストとファイルの一致はイメージのビルド（`entry()`）と `build:image` の `ops/gitlab-trial/check-clients.sh` で確かめている。
