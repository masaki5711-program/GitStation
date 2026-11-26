#!/usr/bin/env python3
"""リリースの成果物を out/ に揃える: app.bin、config-schema.json、version.txt、SHA256SUMS（計画 21 §2.2）。

ESP-IDF のビルドの後に動かす（idf.py build の出力は build/<project>.bin）。標準ライブラリだけ。
"""

import hashlib
import json
import pathlib
import shutil
import sys
import tomllib

ROOT = pathlib.Path.cwd()
OUT = ROOT / "out"


def main() -> int:
    bins = sorted((ROOT / "build").glob("*.bin"))
    app = [b for b in bins if b.stem not in ("bootloader", "partition-table", "ota_data_initial")]
    if len(app) != 1:
        print(f"expected one application binary in build/, found {[b.name for b in app]}", file=sys.stderr)
        return 1
    OUT.mkdir(exist_ok=True)
    shutil.copyfile(app[0], OUT / "app.bin")
    with open(ROOT / "config-schema.toml", "rb") as f:
        schema = tomllib.load(f)
    (OUT / "config-schema.json").write_text(
        json.dumps(schema, ensure_ascii=False, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    shutil.copyfile(ROOT / "version.txt", OUT / "version.txt")
    names = ["app.bin", "config-schema.json", "version.txt"]
    lines = [f"{hashlib.sha256((OUT / n).read_bytes()).hexdigest()}  {n}\n" for n in names]
    (OUT / "SHA256SUMS").write_text("".join(lines), encoding="utf-8")
    print("".join(lines), end="")
    return 0


if __name__ == "__main__":
    sys.exit(main())
