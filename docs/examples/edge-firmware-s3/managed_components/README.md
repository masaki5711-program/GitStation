# managed_components/

CI のジョブにはネットワークが無い（ADR-0029）。ESP-IDF の部品（component manager）を使うときは、手元で `idf.py update-dependencies`
を動かし、`dependencies.lock` とこのディレクトリの中身をリポジトリに入れる。この例は部品を使わない。
