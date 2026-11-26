#!/usr/bin/env python3
"""I/O の意味の辞書（io-dictionary.toml）の「追記だけ」を確かめる（計画 21 §2.1）。

CI のジョブ（.forge-ci.yml の dictionary、fetch_depth: 2）で動かす:

    python3 scripts/check-append-only.py

比べる相手は 1 つ前のコミット HEAD^1。MR のパイプラインはマージの結果で動くので HEAD^1 は
目標のブランチの先端、マージの後の main / release のパイプラインでは直前の版になる。

試験の入力では git を使わずに 2 つのファイルを比べる:

    python3 scripts/check-append-only.py --previous old.toml --current new.toml

終了コード: 0 = 合格、1 = 規則に反する（1 行に 1 つ、理由を出す）、2 = 確かめられない
（HEAD^1 が無い浅い取得、git の失敗、読めないファイル）。標準ライブラリだけ（Python 3.11 以上）。
"""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
import tomllib

FILE = "io-dictionary.toml"
TYPES = ("digital", "analog", "counter")
# 汎用の量と状態: 小文字・数字・_、点で区切る（例 temperature、button.pressed）。
NAME = re.compile(r"^[a-z][a-z0-9_]*(\.[a-z][a-z0-9_]*)*$")
# UCUM の記号の範囲の文字だけ（意味の正しさは人のレビュー）。
UNIT = re.compile(r"^[A-Za-z0-9%\[\]/.*'^\-]{1,32}$")
IMMUTABLE = ("name", "type", "unit", "description", "since")
KNOWN = set(IMMUTABLE) | {"id", "deprecated"}
GIT_TIMEOUT_S = 60


class Unverifiable(Exception):
    """確かめる材料が無い（終了コード 2）。"""


def git(*args: str) -> subprocess.CompletedProcess[bytes]:
    # safe.directory: チェックアウトの持ち主とコンテナの利用者が違っても読む（コマンドラインの設定は保護された設定）。
    return subprocess.run(
        ["git", "-c", "safe.directory=*", *args],
        capture_output=True,
        timeout=GIT_TIMEOUT_S,
        check=False,
    )


def previous_from_git(path: str) -> bytes | None:
    """HEAD^1 の path。None: HEAD が最初のコミット、または HEAD^1 にファイルが無い（最初の版）。"""
    head = git("cat-file", "-p", "HEAD")
    if head.returncode != 0:
        raise Unverifiable(f"git cat-file HEAD failed: {head.stderr.decode(errors='replace').strip()}")
    parents = [l for l in head.stdout.decode().splitlines() if l.startswith("parent ")]
    if not parents:
        return None
    parent = parents[0].split()[1]
    if git("cat-file", "-e", f"{parent}^{{commit}}").returncode != 0:
        raise Unverifiable(
            "HEAD^1 is not in the clone (a shallow fetch): set `fetch_depth: 2` on this job"
        )
    exists = git("cat-file", "-e", f"{parent}:{path}")
    if exists.returncode != 0:
        return None
    blob = git("cat-file", "blob", f"{parent}:{path}")
    if blob.returncode != 0:
        raise Unverifiable(f"git cat-file {parent}:{path} failed")
    return blob.stdout


def load(data: bytes, where: str) -> dict:
    try:
        return tomllib.loads(data.decode("utf-8"))
    except (UnicodeDecodeError, tomllib.TOMLDecodeError) as e:
        raise Unverifiable(f"{where}: not valid UTF-8 TOML: {e}") from e


def is_int(v: object) -> bool:
    return isinstance(v, int) and not isinstance(v, bool)


def check_shape(doc: dict, problems: list[str]) -> dict[int, dict]:
    """今の版の形。id → 項目（形の壊れた項目は比べない）。"""
    version = doc.get("dictionary_version")
    if not is_int(version) or version < 1:
        problems.append("dictionary_version: must be an integer >= 1")
        version = None
    for key in doc:
        if key not in ("dictionary_version", "signal"):
            problems.append(f"{key}: unknown top-level key")
    signals = doc.get("signal", [])
    if not isinstance(signals, list):
        problems.append("signal: must be an array of tables ([[signal]])")
        return {}
    by_id: dict[int, dict] = {}
    names: dict[str, int] = {}
    for n, s in enumerate(signals, 1):
        at = f"signal #{n}"
        if not isinstance(s, dict):
            problems.append(f"{at}: must be a table")
            continue
        sid = s.get("id")
        if not is_int(sid) or sid < 1:
            problems.append(f"{at}: id must be an integer >= 1")
            continue
        at = f"id {sid}"
        for key in s:
            if key not in KNOWN:
                problems.append(f"{at}: unknown key {key}")
        if sid in by_id:
            problems.append(f"{at}: id used twice")
            continue
        name = s.get("name")
        if not isinstance(name, str) or not NAME.match(name):
            problems.append(f"{at}: name must match {NAME.pattern}")
        elif name in names:
            problems.append(f"{at}: name {name} is already id {names[name]}")
        else:
            names[name] = sid
        kind = s.get("type")
        if kind not in TYPES:
            problems.append(f"{at}: type must be one of {', '.join(TYPES)}")
        unit = s.get("unit")
        if kind in ("analog", "counter") and unit is None:
            problems.append(f"{at}: unit is required for {kind} (a UCUM symbol)")
        if unit is not None and (not isinstance(unit, str) or not UNIT.match(unit)):
            problems.append(f"{at}: unit must be a UCUM symbol")
        if not isinstance(s.get("description"), str) or not s["description"].strip():
            problems.append(f"{at}: description is required")
        since = s.get("since")
        if not is_int(since) or since < 1 or (version is not None and since > version):
            problems.append(f"{at}: since must be an integer from 1 to dictionary_version")
        dep = s.get("deprecated")
        if dep is not None:
            if not isinstance(dep, dict) or set(dep) - {"since", "replaced_by"}:
                problems.append(f"{at}: deprecated must be {{ since, replaced_by? }}")
            else:
                ds = dep.get("since")
                if not is_int(ds) or (version is not None and ds > version) or (is_int(since) and ds < since):
                    problems.append(f"{at}: deprecated.since must be between since and dictionary_version")
                rb = dep.get("replaced_by")
                if rb is not None and (not is_int(rb) or rb == sid):
                    problems.append(f"{at}: deprecated.replaced_by must be another id")
        by_id[sid] = s
    for sid, s in by_id.items():
        rb = (s.get("deprecated") or {}).get("replaced_by") if isinstance(s.get("deprecated"), dict) else None
        if is_int(rb) and rb not in by_id:
            problems.append(f"id {sid}: deprecated.replaced_by {rb} is not in the dictionary")
    return by_id


def compare(prev: dict, cur: dict, problems: list[str]) -> None:
    """前の版から今の版へ: 追記と deprecated を付けることだけ。"""
    prev_problems: list[str] = []
    old = check_shape(prev, prev_problems)
    if prev_problems:
        # 前の版が壊れている（規則の導入の前など）: 比べられる分だけ比べる。
        print(f"note: the previous version has {len(prev_problems)} shape problem(s); compared what is readable")
    new = check_shape(cur, [])  # 今の版の形の問題は呼び出し側で出した
    pv, cv = prev.get("dictionary_version"), cur.get("dictionary_version")
    for sid, o in old.items():
        n = new.get(sid)
        if n is None:
            problems.append(f"id {sid}: deleted (ids are permanent: mark it deprecated instead)")
            continue
        for key in IMMUTABLE:
            if o.get(key) != n.get(key):
                problems.append(
                    f"id {sid}: {key} changed ({o.get(key)!r} -> {n.get(key)!r}); add a new id and deprecate this one"
                )
        od, nd = o.get("deprecated"), n.get("deprecated")
        if od is not None and od != nd:
            problems.append(f"id {sid}: deprecated was removed or changed (it can never be undone)")
        if od is None and nd is not None and is_int(cv) and isinstance(nd, dict) and nd.get("since") != cv:
            problems.append(f"id {sid}: newly deprecated: deprecated.since must be dictionary_version {cv}")
    top = max(old, default=0)
    added = sorted(set(new) - set(old))
    expected = top + 1
    for sid in added:
        if sid != expected:
            problems.append(f"id {sid}: a new id must be the next one ({expected}): ids are never skipped or reused")
        else:
            expected += 1
        if is_int(cv) and new[sid].get("since") != cv:
            problems.append(f"id {sid}: a new id's since must be dictionary_version {cv}")
    changed = old != new
    if is_int(pv) and is_int(cv):
        if cv < pv:
            problems.append(f"dictionary_version went down ({pv} -> {cv})")
        elif changed and cv != pv + 1:
            problems.append(f"dictionary_version must be {pv + 1} when the dictionary changes (found {cv})")
        elif not changed and cv != pv:
            problems.append(f"dictionary_version changed ({pv} -> {cv}) but no signal did")


def main(argv: list[str]) -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--file", default=FILE)
    ap.add_argument("--previous", help="the previous version's file (tests; default: HEAD^1 via git)")
    ap.add_argument("--current", help="the current version's file (default: --file)")
    a = ap.parse_args(argv)
    try:
        with open(a.current or a.file, "rb") as f:
            cur_bytes = f.read()
        if a.previous is not None:
            with open(a.previous, "rb") as f:
                prev_bytes: bytes | None = f.read()
        else:
            prev_bytes = previous_from_git(a.file)
        cur = load(cur_bytes, "current")
        prev = None if prev_bytes is None else load(prev_bytes, "previous")
    except (OSError, subprocess.TimeoutExpired, Unverifiable) as e:
        print(f"cannot verify: {e}", file=sys.stderr)
        return 2
    problems: list[str] = []
    check_shape(cur, problems)
    if prev is None:
        print("no previous version (the first commit, or the file is new): shape checked only")
    elif not problems:
        compare(prev, cur, problems)
    for p in problems:
        print(f"io-dictionary: {p}")
    if problems:
        print(f"{len(problems)} problem(s): the dictionary is append-only (docs/guides/edge-release-pattern.md)")
        return 1
    print(f"ok: dictionary_version {cur.get('dictionary_version')}, {len(cur.get('signal', []))} signal(s)")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
