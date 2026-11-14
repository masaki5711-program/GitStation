#!/usr/bin/env python3
"""Clone speed and git memory of a synthetic repository (ops/runbooks/memory-sizing.md).

Builds a reproducible repository (fixed seed: a few thousand commits, tens of
MB), then measures, for each repository state and server-side git setting:

- the wall time of a full `git clone --bare --no-local` over git's own
  upload-pack (the same `upload-pack` -> `pack-objects` forge streams for
  Smart HTTP; the server side gets its settings through `-u`);
- the peak memory of the server's `pack-objects` alone (what upload-pack runs
  for that clone: `pack-objects --revs --all --stdout --thin
  --delta-base-offset`);
- the wall time and peak memory of the maintenance gc (the argv of
  `forge_git::maintenance::gc_command`).

Peak memory: Windows - the job object's PeakProcessMemoryUsed (the largest
private commit of any one process in the tree); Linux - ru_maxrss of the
child tree (resident set, mapped pack pages included). The two are different
measures; compare numbers from the same OS only.

Standard library only. Usage:

    python ops/bench/git-pack-memory.py [--commits 3000] [--runs 3] [--work DIR]
"""

import argparse
import os
import random
import shutil
import statistics
import subprocess
import sys
import tempfile
import time

# The caps forge passes to every pack-heavy git (forge_git::PackLimits
# defaults). Keep in sync with crates/forge-git/src/config.rs.
FORGE_CAPS = [
    ("pack.windowMemory", "256m"),
    ("pack.threads", "2"),
    ("core.deltaBaseCacheLimit", "64m"),
    ("core.packedGitLimit", "1024m"),
    ("core.packedGitWindowSize", "64m"),
]

# main before this change: gc writes one pack with bitmaps.
GC_MAIN = [
    ("gc.packRefs", "false"),
    ("gc.writeCommitGraph", "false"),
    ("repack.writeBitmaps", "true"),
    ("pack.writeBitmapHashCache", "true"),
]
# This change: plus the bitmap lookup table (git >= 2.38).
GC_LEAN = GC_MAIN + [("pack.writeBitmapLookupTable", "true")]


def cflags(pairs):
    out = []
    for k, v in pairs:
        out += ["-c", f"{k}={v}"]
    return out


# ---------------------------------------------------------------- peak memory

if os.name == "nt":
    import ctypes
    from ctypes import wintypes

    class _IoCounters(ctypes.Structure):
        _fields_ = [(n, ctypes.c_ulonglong) for n in (
            "ReadOperationCount", "WriteOperationCount", "OtherOperationCount",
            "ReadTransferCount", "WriteTransferCount", "OtherTransferCount")]

    class _Basic(ctypes.Structure):
        _fields_ = [
            ("PerProcessUserTimeLimit", ctypes.c_longlong),
            ("PerJobUserTimeLimit", ctypes.c_longlong),
            ("LimitFlags", wintypes.DWORD),
            ("MinimumWorkingSetSize", ctypes.c_size_t),
            ("MaximumWorkingSetSize", ctypes.c_size_t),
            ("ActiveProcessLimit", wintypes.DWORD),
            ("Affinity", ctypes.c_size_t),
            ("PriorityClass", wintypes.DWORD),
            ("SchedulingClass", wintypes.DWORD),
        ]

    class _Extended(ctypes.Structure):
        _fields_ = [
            ("BasicLimitInformation", _Basic),
            ("IoInfo", _IoCounters),
            ("ProcessMemoryLimit", ctypes.c_size_t),
            ("JobMemoryLimit", ctypes.c_size_t),
            ("PeakProcessMemoryUsed", ctypes.c_size_t),
            ("PeakJobMemoryUsed", ctypes.c_size_t),
        ]

    _k32 = ctypes.WinDLL("kernel32", use_last_error=True)
    _k32.CreateJobObjectW.restype = wintypes.HANDLE
    _k32.GetCurrentProcess.restype = wintypes.HANDLE

    def run_measured(argv, **kw):
        """Run argv in a fresh job object (this process joins it: nested jobs,
        Windows 8+); returns (seconds, peak bytes of the largest process)."""
        # A child Python joins the job, so the job holds only it and git.
        code = (
            "import subprocess,sys;sys.exit(subprocess.call(sys.argv[1:]))"
        )
        job = _k32.CreateJobObjectW(None, None)
        t0 = time.perf_counter()
        p = subprocess.Popen([sys.executable, "-c", code] + argv,
                             creationflags=0x4, **kw)  # CREATE_SUSPENDED
        if not _k32.AssignProcessToJobObject(job, wintypes.HANDLE(int(p._handle))):
            raise OSError(ctypes.get_last_error(), "AssignProcessToJobObject")
        _resume(p.pid)
        rc = p.wait()
        dt = time.perf_counter() - t0
        info = _Extended()
        _k32.QueryInformationJobObject(job, 9, ctypes.byref(info),
                                       ctypes.sizeof(info), None)
        _k32.CloseHandle(job)
        if rc != 0:
            raise SystemExit(f"failed ({rc}): {argv}")
        return dt, info.PeakProcessMemoryUsed

    def _resume(pid):
        ntdll = ctypes.WinDLL("ntdll")
        h = _k32.OpenProcess(0x0800, False, pid)  # PROCESS_SUSPEND_RESUME
        ntdll.NtResumeProcess(wintypes.HANDLE(h))
        _k32.CloseHandle(wintypes.HANDLE(h))

else:
    def run_measured(argv, **kw):
        t0 = time.perf_counter()
        p = subprocess.Popen(argv, **kw)
        _, status, ru = os.wait4(p.pid, 0)
        p.returncode = os.waitstatus_to_exitcode(status)
        dt = time.perf_counter() - t0
        if p.returncode != 0:
            raise SystemExit(f"failed ({p.returncode}): {argv}")
        # ru_maxrss: KiB on Linux.
        return dt, ru.ru_maxrss * 1024


# ---------------------------------------------------------- synthetic repo

WORDS = [f"w{i:04x}" for i in range(4096)]


def text(rng, lines):
    return "".join(" ".join(rng.choices(WORDS, k=12)) + "\n" for _ in range(lines))


def build_repo(path, commits, seed=20261113):
    """fast-import: `commits` commits over 400 text files (each edit rewrites
    a few lines: good deltas), 8 binary 4 MiB "documents" (a spec repository's
    PDFs / spreadsheets: one is edited in place every 25th commit, so their
    versions delta against each other) and a 512 KiB incompressible blob
    every 100th commit. Deterministic for a seed."""
    rng = random.Random(seed)
    subprocess.run(["git", "init", "--bare", "-q", path], check=True)
    files = {f"src/f{i:03}.txt": text(rng, rng.randint(200, 800)).splitlines(True)
             for i in range(400)}
    docs = {f"docs/d{i}.bin": bytearray(rng.randbytes(4 * 2**20)) for i in range(8)}
    p = subprocess.Popen(["git", "-C", path, "fast-import", "--quiet"],
                         stdin=subprocess.PIPE)
    w = p.stdin.write

    def blob(data):
        w(b"data %d\n" % len(data))
        w(data)
        w(b"\n")

    for n in range(commits):
        changed = list(files) if n == 0 else rng.sample(sorted(files), 3)
        w(b"commit refs/heads/main\n")
        w(b"committer Bench <bench@example.invalid> %d +0900\n" % (1_700_000_000 + n * 60))
        blob(f"commit {n}".encode())
        for name in changed:
            lines = files[name]
            if n:
                for _ in range(rng.randint(1, 6)):
                    lines[rng.randrange(len(lines))] = text(rng, 1)
                lines.append(text(rng, 1))
            w(f"M 100644 inline {name}\n".encode())
            blob("".join(lines).encode())
        if n == 0 or n % 25 == 0:
            for name in (sorted(docs) if n == 0 else [rng.choice(sorted(docs))]):
                if n:
                    at = rng.randrange(len(docs[name]) - 16384)
                    docs[name][at:at + 16384] = rng.randbytes(16384)
                w(f"M 100644 inline {name}\n".encode())
                blob(bytes(docs[name]))
        if n and n % 100 == 0:
            w(f"M 100644 inline bin/b{n:05}.dat\n".encode())
            blob(rng.randbytes(512 * 1024))
        if n % 500 == 499:
            w(f"reset refs/tags/v{n // 500}\nfrom refs/heads/main\n\n".encode())
    p.stdin.close()
    if p.wait() != 0:
        raise SystemExit("fast-import failed")


def rmtree(path):
    """shutil.rmtree that also removes git's read-only pack files (Windows)."""
    def force(func, p, _exc):
        os.chmod(p, 0o700)
        func(p)
    if os.path.exists(path):
        shutil.rmtree(path, onerror=force)


def pack_size_mib(path):
    total = 0
    for root, _, names in os.walk(os.path.join(path, "objects")):
        total += sum(os.path.getsize(os.path.join(root, n)) for n in names)
    return total / 2**20


# ----------------------------------------------------------------- measuring

def gc(path, gc_cfg, caps):
    argv = ["git"] + cflags(caps) + cflags(gc_cfg) + [
        "gc", "--cruft", "--prune=172800.seconds.ago", "--quiet"]
    dt, peak = run_measured(argv, env=dict(os.environ, GIT_DIR=path))
    subprocess.run(["git", "--git-dir", path, "commit-graph", "write",
                    "--reachable", "--no-progress"], check=True)
    return dt, peak


def clone(path, caps, work):
    dest = os.path.join(work, "clone.git")
    rmtree(dest)
    up = " ".join(["git"] + [f"-c {k}={v}" for k, v in caps] + ["upload-pack"])
    url = "file:///" + os.path.abspath(path).replace("\\", "/").lstrip("/")
    dt, _ = run_measured(["git", "clone", "--bare", "--no-local", "-q",
                          "-u", up, url, dest])
    rmtree(dest)
    return dt


def pack_objects(path, caps):
    argv = ["git"] + cflags(caps) + ["--git-dir", path, "pack-objects", "--revs",
                                     "--all", "--stdout", "--thin",
                                     "--delta-base-offset", "-q"]
    with open(os.devnull, "wb") as null:
        return run_measured(argv, stdin=subprocess.DEVNULL, stdout=null)


def median(xs):
    return statistics.median(xs)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--commits", type=int, default=3000)
    ap.add_argument("--runs", type=int, default=3)
    ap.add_argument("--work")
    a = ap.parse_args()
    work = a.work or tempfile.mkdtemp(prefix="git-pack-bench-")
    os.makedirs(work, exist_ok=True)
    print(subprocess.run(["git", "--version"], capture_output=True,
                         text=True).stdout.strip(), f"cpus={os.cpu_count()}")
    seed_repo = os.path.join(work, "seed.git")
    if not os.path.isdir(seed_repo):
        t0 = time.perf_counter()
        build_repo(seed_repo, a.commits)
        print(f"built {a.commits} commits in {time.perf_counter() - t0:.1f}s, "
              f"{pack_size_mib(seed_repo):.1f} MiB as pushed (fast-import packs)")

    rows = []

    def state(name, gc_cfg, gc_caps):
        repo = os.path.join(work, name + ".git")
        rmtree(repo)
        shutil.copytree(seed_repo, repo)
        if gc_cfg is not None:
            dt, peak = gc(repo, gc_cfg, gc_caps)
            rows.append((f"gc -> {name}", dt, peak))
        print(f"{name}: {pack_size_mib(repo):.1f} MiB on disk")
        return repo

    # "loose": every object a loose file, as many small pushes leave them
    # (receive.unpackLimit: a push of < 100 objects is unpacked loose);
    # pack-objects must search deltas for each one on a clone and on gc
    # (git never retries deltas between objects of one existing pack, so a
    # no-delta *pack* would not show this).
    loose = os.path.join(work, "loose.git")
    if not os.path.isdir(loose):
        shutil.copytree(seed_repo, loose)
        packdir = os.path.join(loose, "objects", "pack")
        held = os.path.join(work, "seed-packs")
        os.makedirs(held, exist_ok=True)
        for name in os.listdir(packdir):
            os.replace(os.path.join(packdir, name), os.path.join(held, name))
        for name in sorted(os.listdir(held)):
            if name.endswith(".pack"):
                with open(os.path.join(held, name), "rb") as pack:
                    subprocess.run(["git", "--git-dir", loose, "unpack-objects",
                                    "-q"], stdin=pack, check=True)
        rmtree(held)
    print(f"loose: {pack_size_mib(loose):.1f} MiB on disk (loose objects)")
    dt, peak = run_measured(["git", "version"], stdout=subprocess.DEVNULL)
    rows.append(("baseline (wrapper + git version)", dt, peak))
    seed_repo = loose
    states = {
        "loose": loose,
        "gc-main": state("gc-main", GC_MAIN, []),
        "gc-lean": state("gc-lean", GC_LEAN, FORGE_CAPS),
    }
    for sname, repo in states.items():
        for cname, caps in (("git-defaults", []), ("forge-caps", FORGE_CAPS)):
            clones = [clone(repo, caps, work) for _ in range(a.runs)]
            packs = [pack_objects(repo, caps) for _ in range(a.runs)]
            rows.append((f"clone  {sname:8} {cname}", median(clones), None))
            rows.append((f"pack-objects {sname:8} {cname}",
                         median(p[0] for p in packs), max(p[1] for p in packs)))

    print(f"\n{'case':44} {'wall s':>8} {'peak MiB':>9}")
    for name, dt, peak in rows:
        pk = f"{peak / 2**20:9.1f}" if peak is not None else f"{'-':>9}"
        print(f"{name:44} {dt:8.2f} {pk}")
    if not a.work:
        rmtree(work)


if __name__ == "__main__":
    main()
