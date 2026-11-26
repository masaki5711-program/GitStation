#!/usr/bin/env python3
"""ファームウェアの設定のスキーマ（config-schema.toml）の「追記だけ」を確かめる（計画 21 §2.2）。

CI のジョブ（.forge-ci.yml、fetch_depth: 2）で動かす:

    python3 scripts/check-config-schema.py                 # HEAD^1 と比べる
    python3 scripts/check-config-schema.py --version-bump  # release でだけ: version.txt が上がったか

比べる相手は 1 つ前のコミット HEAD^1（MR のパイプラインではマージの結果の第 1 の親 = 目標のブランチの先端）。
試験の入力では git を使わずに 2 つのファイルを比べる:

    python3 scripts/check-config-schema.py --previous old.toml --current new.toml

断るもの: 項目の削除、type・default の変更、範囲を狭める（min を上げる・max を下げる・max_len を下げる・values を消す）、
default の無い追加、deprecated を外す、lineage の変更、schema_version の上げ忘れ。
終了コード: 0 = 合格、1 = 規則に反する、2 = 確かめられない（HEAD^1 が無い浅い取得など）。標準ライブラリだけ（Python 3.11 以上）。
"""

from __future__ import annotations

import argparse
import ipaddress
import re
import subprocess
import sys
import tomllib

FILE = "config-schema.toml"
VERSION_FILE = "version.txt"
TYPES = ("bool", "int", "float", "string", "ipv4", "enum")
KEY = re.compile(r"^[a-z][a-z0-9_]*(\.[a-z][a-z0-9_]*)*$")
SEMVER = re.compile(r"^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$")
KNOWN = {"key", "type", "default", "min", "max", "max_len", "values", "description", "since", "deprecated"}
GIT_TIMEOUT_S = 60


class Unverifiable(Exception):
    """確かめる材料が無い（終了コード 2）。"""


def git(*args: str) -> subprocess.CompletedProcess[bytes]:
    return subprocess.run(
        ["git", "-c", "safe.directory=*", *args],
        capture_output=True,
        timeout=GIT_TIMEOUT_S,
        check=False,
    )


def previous_from_git(path: str) -> bytes | None:
    """HEAD^1 の path。None: HEAD が最初のコミット、または HEAD^1 にファイルが無い。"""
    head = git("cat-file", "-p", "HEAD")
    if head.returncode != 0:
        raise Unverifiable(f"git cat-file HEAD failed: {head.stderr.decode(errors='replace').strip()}")
    parents = [l for l in head.stdout.decode().splitlines() if l.startswith("parent ")]
    if not parents:
        return None
    parent = parents[0].split()[1]
    if git("cat-file", "-e", f"{parent}^{{commit}}").returncode != 0:
        raise Unverifiable("HEAD^1 is not in the clone (a shallow fetch): set `fetch_depth: 2` on this job")
    if git("cat-file", "-e", f"{parent}:{path}").returncode != 0:
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


def is_num(v: object) -> bool:
    return is_int(v) or isinstance(v, float)


def value_ok(s: dict, v: object) -> bool:
    """v が項目 s の型と範囲に合うか。"""
    t = s.get("type")
    if t == "bool":
        return isinstance(v, bool)
    if t in ("int", "float"):
        ok = is_int(v) if t == "int" else is_num(v)
        if not ok:
            return False
        lo, hi = s.get("min"), s.get("max")
        return (lo is None or v >= lo) and (hi is None or v <= hi)
    if t == "string":
        return isinstance(v, str) and len(v.encode()) <= s.get("max_len", 255)
    if t == "ipv4":
        try:
            ipaddress.IPv4Address(v)  # type: ignore[arg-type]
            return True
        except (ipaddress.AddressValueError, ValueError, TypeError):
            return False
    if t == "enum":
        return isinstance(s.get("values"), list) and v in s["values"]
    return False


def check_shape(doc: dict, problems: list[str]) -> dict[str, dict]:
    version = doc.get("schema_version")
    if not is_int(version) or version < 1:
        problems.append("schema_version: must be an integer >= 1")
        version = None
    for key in doc:
        if key not in ("schema_version", "setting", "lineage"):
            problems.append(f"{key}: unknown top-level key")
    lineage = doc.get("lineage")
    if lineage is not None and (
        not isinstance(lineage, dict)
        or set(lineage) != {"from", "at"}
        or not all(isinstance(v, str) and v for v in lineage.values())
    ):
        problems.append('lineage: must be { from = "<group/project>", at = "<version>" }')
    settings = doc.get("setting", [])
    if not isinstance(settings, list):
        problems.append("setting: must be an array of tables ([[setting]])")
        return {}
    by_key: dict[str, dict] = {}
    for n, s in enumerate(settings, 1):
        if not isinstance(s, dict):
            problems.append(f"setting #{n}: must be a table")
            continue
        key = s.get("key")
        if not isinstance(key, str) or not KEY.match(key):
            problems.append(f"setting #{n}: key must match {KEY.pattern}")
            continue
        at = f"{key}"
        if key in by_key:
            problems.append(f"{at}: key used twice")
            continue
        for k in s:
            if k not in KNOWN:
                problems.append(f"{at}: unknown field {k}")
        t = s.get("type")
        if t not in TYPES:
            problems.append(f"{at}: type must be one of {', '.join(TYPES)}")
        if t in ("int", "float"):
            for b in ("min", "max"):
                if b in s and not (is_int(s[b]) if t == "int" else is_num(s[b])):
                    problems.append(f"{at}: {b} must be a {t}")
            if is_num(s.get("min")) and is_num(s.get("max")) and s["min"] > s["max"]:
                problems.append(f"{at}: min is greater than max")
        elif "min" in s or "max" in s:
            problems.append(f"{at}: min / max are for int and float only")
        if "max_len" in s and (t != "string" or not is_int(s["max_len"]) or not 1 <= s["max_len"] <= 4096):
            problems.append(f"{at}: max_len is for string only, 1 to 4096")
        if t == "enum":
            vals = s.get("values")
            if not isinstance(vals, list) or not vals or not all(isinstance(v, str) and v for v in vals) or len(set(vals)) != len(vals):
                problems.append(f"{at}: values must be a non-empty list of distinct strings")
        elif "values" in s:
            problems.append(f"{at}: values is for enum only")
        if "default" not in s:
            problems.append(f"{at}: default is required (an old configuration must still work)")
        elif t in TYPES and not value_ok(s, s["default"]):
            problems.append(f"{at}: default {s['default']!r} does not fit its type and range")
        if not isinstance(s.get("description"), str) or not s["description"].strip():
            problems.append(f"{at}: description is required")
        since = s.get("since")
        if not is_int(since) or since < 1 or (version is not None and since > version):
            problems.append(f"{at}: since must be an integer from 1 to schema_version")
        dep = s.get("deprecated")
        if dep is not None and (
            not isinstance(dep, dict) or set(dep) != {"since"} or not is_int(dep["since"])
            or (version is not None and dep["since"] > version)
        ):
            problems.append(f"{at}: deprecated must be {{ since = <schema_version> }}")
        by_key[key] = s
    return by_key


def narrowed(o: dict, n: dict) -> str | None:
    """範囲が狭まったなら理由。"""
    if is_num(n.get("min")) and (o.get("min") is None or n["min"] > o["min"]):
        return f"min raised ({o.get('min')} -> {n['min']})"
    if is_num(n.get("max")) and (o.get("max") is None or n["max"] < o["max"]):
        return f"max lowered ({o.get('max')} -> {n['max']})"
    if n.get("max_len", 255) < o.get("max_len", 255):
        return f"max_len lowered ({o.get('max_len', 255)} -> {n.get('max_len', 255)})"
    if isinstance(o.get("values"), list):
        gone = [v for v in o["values"] if v not in (n.get("values") or [])]
        if gone:
            return f"values removed ({', '.join(map(str, gone))})"
    return None


def compare(prev: dict, cur: dict, problems: list[str]) -> None:
    prev_problems: list[str] = []
    old = check_shape(prev, prev_problems)
    if prev_problems:
        # 前の版が壊れている（規則の導入の前など）: 比べられる分だけ比べる。
        print(f"note: the previous version has {len(prev_problems)} shape problem(s); compared what is readable")
    new = check_shape(cur, [])
    pv, cv = prev.get("schema_version"), cur.get("schema_version")
    if prev.get("lineage") is not None and prev.get("lineage") != cur.get("lineage"):
        problems.append("lineage: changed or removed (it records where this firmware forked, once)")
    for key, o in old.items():
        n = new.get(key)
        if n is None:
            problems.append(f"{key}: deleted (keys are permanent: mark it deprecated instead)")
            continue
        for field in ("type", "default", "since"):
            if o.get(field) != n.get(field):
                problems.append(f"{key}: {field} changed ({o.get(field)!r} -> {n.get(field)!r}); add a new key instead")
        why = narrowed(o, n)
        if why:
            problems.append(f"{key}: range narrowed: {why} (an old configuration could stop fitting)")
        od, nd = o.get("deprecated"), n.get("deprecated")
        if od is not None and od != nd:
            problems.append(f"{key}: deprecated was removed or changed (it can never be undone)")
        if od is None and isinstance(nd, dict) and is_int(cv) and nd.get("since") != cv:
            problems.append(f"{key}: newly deprecated: deprecated.since must be schema_version {cv}")
    for key in sorted(set(new) - set(old)):
        if is_int(cv) and new[key].get("since") != cv:
            problems.append(f"{key}: a new key's since must be schema_version {cv}")
    changed = old != new or prev.get("lineage") != cur.get("lineage")
    if is_int(pv) and is_int(cv):
        if cv < pv:
            problems.append(f"schema_version went down ({pv} -> {cv})")
        elif changed and cv != pv + 1:
            problems.append(f"schema_version must be {pv + 1} when the schema changes (found {cv})")
        elif not changed and cv != pv:
            problems.append(f"schema_version changed ({pv} -> {cv}) but no setting did")


def semver(text: str) -> tuple[int, int, int] | None:
    m = SEMVER.match(text.strip())
    return tuple(int(x) for x in m.groups()) if m else None  # type: ignore[return-value]


def version_bump(previous: bytes | None, current: bytes, problems: list[str]) -> None:
    cur = semver(current.decode("utf-8", errors="replace"))
    if cur is None:
        problems.append(f"{VERSION_FILE}: must be MAJOR.MINOR.PATCH")
        return
    if previous is None:
        return
    prev = semver(previous.decode("utf-8", errors="replace"))
    if prev is None:
        problems.append(f"{VERSION_FILE}: the previous version is not MAJOR.MINOR.PATCH, so the bump cannot be checked")
    elif cur <= prev:
        problems.append(f"{VERSION_FILE}: a release must raise the version ({'.'.join(map(str, prev))} -> {'.'.join(map(str, cur))})")


def main(argv: list[str]) -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--file", default=FILE)
    ap.add_argument("--previous", help="the previous version's file (tests; default: HEAD^1 via git)")
    ap.add_argument("--current", help="the current version's file (default: --file)")
    ap.add_argument("--version-bump", action="store_true", help=f"check that {VERSION_FILE} went up against HEAD^1")
    a = ap.parse_args(argv)
    problems: list[str] = []
    try:
        if a.version_bump:
            with open(VERSION_FILE, "rb") as f:
                version_bump(previous_from_git(VERSION_FILE), f.read(), problems)
        with open(a.current or a.file, "rb") as f:
            cur = load(f.read(), "current")
        if a.previous is not None:
            with open(a.previous, "rb") as f:
                prev_bytes: bytes | None = f.read()
        else:
            prev_bytes = previous_from_git(a.file)
        prev = None if prev_bytes is None else load(prev_bytes, "previous")
    except (OSError, subprocess.TimeoutExpired, Unverifiable) as e:
        print(f"cannot verify: {e}", file=sys.stderr)
        return 2
    shape: list[str] = []
    check_shape(cur, shape)
    problems.extend(shape)
    if prev is None:
        print("no previous version (the first commit, or the file is new): shape checked only")
    elif not shape:
        compare(prev, cur, problems)
    for p in problems:
        print(f"config-schema: {p}")
    if problems:
        print(f"{len(problems)} problem(s): the configuration schema is append-only (docs/guides/edge-release-pattern.md)")
        return 1
    print(f"ok: schema_version {cur.get('schema_version')}, {len(cur.get('setting', []))} setting(s)")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
