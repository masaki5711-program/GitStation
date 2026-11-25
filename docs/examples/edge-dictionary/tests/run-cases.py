#!/usr/bin/env python3
"""scripts/check-append-only.py の試験（CI のジョブが動かす。標準ライブラリと git だけ）。

1. 入力の組: tests/cases/<名前>/ の previous.toml → current.toml を確かめ、expect の 1 行目（"ok" か、
   出力に含まれるはずの理由）と比べる。
2. git の形: 一時のリポジトリに 2 つの版をコミットし、HEAD^1 と比べること、浅い取得（--depth=1）では
   確かめられない（終了コード 2）こと、--depth=2 なら比べられることを確かめる。
"""

import pathlib
import subprocess
import sys
import tempfile

ROOT = pathlib.Path(__file__).resolve().parent.parent
SCRIPT = ROOT / "scripts" / "check-append-only.py"
FILE = "io-dictionary.toml"
# git の形で使う組: 1 つ目は合格、2 つ目（同じ previous から）は不合格。
NEEDLE = "id 5: deleted"
GIT_OK, GIT_BAD = "ok-add-and-deprecate", "fail-delete"


def run(args, cwd=None):
    r = subprocess.run(
        [sys.executable, str(SCRIPT), *args], cwd=cwd, capture_output=True,
        text=True, encoding="utf-8", timeout=60, check=False,
    )
    return r.returncode, r.stdout + r.stderr


def pairs() -> list[str]:
    failed = []
    for case in sorted(p for p in (ROOT / "tests" / "cases").iterdir() if p.is_dir()):
        expect = (case / "expect").read_text(encoding="utf-8").splitlines()[0]
        code, out = run(["--previous", str(case / "previous.toml"), "--current", str(case / "current.toml")])
        if expect == "ok":
            ok = code == 0
        elif expect.startswith("exit2:"):
            ok = code == 2 and expect[len("exit2:"):] in out
        else:
            ok = code == 1 and expect in out
        print(f"{'PASS' if ok else 'FAIL'} {case.name}")
        if not ok:
            failed.append(f"{case.name}: expected {expect!r}, exit {code}:\n{out}")
    return failed


def git(cwd, *args):
    subprocess.run(
        ["git", "-c", "user.name=selftest", "-c", "user.email=selftest@example.invalid",
         "-c", "safe.directory=*", *args],
        cwd=cwd, check=True, capture_output=True, timeout=60,
    )


def git_out(cwd, *args) -> str:
    r = subprocess.run(
        ["git", "-c", "user.name=selftest", "-c", "user.email=selftest@example.invalid",
         "-c", "safe.directory=*", *args],
        cwd=cwd, check=True, capture_output=True, timeout=60, text=True,
    )
    return r.stdout.strip()


def git_mode() -> list[str]:
    failed = []
    cases = ROOT / "tests" / "cases"
    with tempfile.TemporaryDirectory() as tmp:
        src = pathlib.Path(tmp) / "src"
        src.mkdir()
        git(src, "init", "-q")
        for name, source in (("v1", GIT_OK + "/previous.toml"), ("v2", GIT_OK + "/current.toml"),
                             ("v3", GIT_BAD + "/previous.toml")):
            (src / FILE).write_bytes((cases / source).read_bytes())
            git(src, "add", FILE)
            git(src, "commit", "-q", "-m", name)
        # v3 goes back to v1's content after v2: id 5 is deleted -> 1.
        # A merge commit (as GitStation writes a merge result: target tip
        # first) whose first parent is v2 and whose tree is v3's: compared
        # with the FIRST parent it is a deletion (1); compared with the
        # second parent (v3, the same tree) it would wrongly pass.
        v2 = git_out(src, "rev-parse", "HEAD~1")
        v3 = git_out(src, "rev-parse", "HEAD")
        tree3 = git_out(src, "rev-parse", "HEAD^{tree}")
        merge = git_out(src, "commit-tree", tree3, "-p", v2, "-p", v3, "-m", "merge")
        git(src, "update-ref", "refs/heads/merged", merge)
        mdir = pathlib.Path(tmp) / "m"
        git(tmp, "clone", "-q", "--depth=2", "--branch", "merged", src.as_uri(), str(mdir))
        checks = [("root-and-parent", src, 1, NEEDLE)]
        for depth, code, needle in ((1, 2, "fetch_depth: 2"), (2, 1, NEEDLE)):
            clone = pathlib.Path(tmp) / f"d{depth}"
            git(tmp, "clone", "-q", f"--depth={depth}", src.as_uri(), str(clone))
            checks.append((f"shallow-depth-{depth}", clone, code, needle))
        checks.append(("merge-commit-first-parent", mdir, 1, NEEDLE))
        for name, where, code, needle in checks:
            got, out = run([], cwd=where)
            ok = got == code and needle in out
            print(f"{'PASS' if ok else 'FAIL'} git:{name}")
            if not ok:
                failed.append(f"git:{name}: expected exit {code} with {needle!r}, exit {got}:\n{out}")
    return failed


def main() -> int:
    failed = pairs() + git_mode()
    for f in failed:
        print(f)
    print("all passed" if not failed else f"{len(failed)} failed")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
