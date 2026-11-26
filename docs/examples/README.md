# 例のリポジトリ（合成）

GitStation の標準の機能で作るパターンの例。そのまま新しいプロジェクトの最初のコミットにして使う
（イメージのダイジェスト・グループ名は置き換える）。GitStation 自身のビルドには使われない。

| 例 | 手引き | 中身 |
|---|---|---|
| [`edge-dictionary/`](edge-dictionary/) | [`docs/guides/edge-release-pattern.md`](../guides/edge-release-pattern.md) §4 | I/O の意味の辞書（追記だけ）と、その CI の確かめ |
| [`edge-firmware-s3/`](edge-firmware-s3/) | 同 §5・§6 | ESP32-S3 のファームウェア（ESP-IDF）、設定のスキーマ（追記だけ）、`release` の成果物（`keep`） |

確かめのスクリプトの試験は手元でも動く（Python 3.11 以上と git）:

```bash
python3 docs/examples/edge-dictionary/tests/run-cases.py
python3 docs/examples/edge-firmware-s3/tests/run-cases.py
```
