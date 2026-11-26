#!/usr/bin/env python3
"""Differential fuzzing of `cmake -E` tools.

Each case runs a short random sequence of `cmake -E` commands (cat,
compare_files, copy*, echo, echo_append, env, make_directory, *sum, remove,
remove_directory, rename, rm, tar create/list/extract, touch,
touch_nocreate, create_symlink, create_hardlink, true, false, chdir,
cmake_echo_color) with random, often wrong, arguments inside a scratch
tree, recording every command's exit code, stdout and stderr, and then the
resulting tree (names, types, sizes, link targets, permission bits).  The
oracle and candidate runs must match byte for byte after the scratch path
is normalized.

Usage: fuzz_cli.py --candidate <staged bin/cmake> [--seed N] [-n COUNT] [-j N]
"""
import argparse
import concurrent.futures
import json
import os
import random
import shutil
import stat
import subprocess
import tempfile
from pathlib import Path

ORACLE = "/tmp/cmake-rust-validation.hSLgEZHR/kitware-reference/bin/cmake"
NAMES = ["a.txt", "b.txt", "d", "d/x.txt", "d/e", "missing", "l", "d/l2", "out.tar", "out.tgz",
         "sp ace.txt", "-dash", "e", "ar.zip", "ar.7z"]


def pick(rng, k=None):
    return rng.sample(NAMES, k or rng.randint(1, 3))


def gen_cmd(rng):
    c = rng.randrange(26)
    if c == 0:
        # Not the archives: their compressed bytes depend on the linked
        # liblzma/zlib version (compared by content in the tree snapshot).
        plain = [n for n in NAMES if not n.endswith((".tar", ".tgz", ".zip", ".7z"))]
        return ["cat", *rng.sample(plain, rng.randint(1, 3))]
    if c == 1:
        return ["compare_files", *(["--ignore-eol"] if rng.random() < 0.3 else []), *pick(rng, 2)]
    if c == 2:
        return [rng.choice(["copy", "copy_if_different"]), *(["-t", rng.choice(NAMES)] if rng.random() < 0.2 else []), *pick(rng, rng.randint(1, 3))]
    if c == 3:
        return [rng.choice(["copy_directory", "copy_directory_if_different"]), *pick(rng, 2)]
    if c == 4:
        return ["echo", *rng.sample(["a", "", "b c", "-n", "--", "x;y"], rng.randint(0, 3))]
    if c == 5:
        return ["echo_append", *rng.sample(["a", "", "b c", "-n"], rng.randint(0, 2))]
    if c == 6:
        return ["env", *rng.sample(["--unset=HOME", "A=1", "B=", "--", "--modify", "X=y"], rng.randint(0, 3)),
                *rng.choice([[], ["true"], ["false"], ["/no/such/prog"]])]
    if c == 7:
        return ["make_directory", *pick(rng)]
    if c == 8:
        return [rng.choice(["md5sum", "sha1sum", "sha256sum", "sha512sum"]), *pick(rng)]
    if c == 9:
        return ["remove", *(["-f"] if rng.random() < 0.5 else []), *pick(rng)]
    if c == 10:
        return ["remove_directory", *pick(rng)]
    if c == 11:
        return ["rename", *pick(rng, 2)]
    if c == 12:
        return ["rm", *rng.sample(["-r", "-f", "-rf", "-R", "--"], rng.randint(0, 2)), *pick(rng)]
    if c == 13:
        mode = rng.choice(["cf", "czf", "cjf", "cJf", "tf", "tvf", "xf", "xvf", "cvf", "c", "x"])
        extra = rng.choice([[], ["--format=zip"], ["--format=7zip"], ["--format=pax"], ["--mtime=2020-01-01"], ["--files-from=a.txt"]])
        return ["tar", mode, *extra, rng.choice(["out.tar", "out.tgz", "ar.zip", "missing"]), *pick(rng, rng.randint(0, 2))]
    if c == 14:
        return [rng.choice(["touch", "touch_nocreate"]), *pick(rng)]
    if c == 15:
        return ["create_symlink", *pick(rng, 2)]
    if c == 16:
        return ["create_hardlink", *pick(rng, 2)]
    if c == 17:
        return [rng.choice(["true", "false"])]
    if c == 18:
        return ["chdir", rng.choice(NAMES), *rng.choice([["true"], ["false"], ["/bin/echo", "hi"], []])]
    if c == 19:
        return ["cmake_echo_color", *rng.sample(["--red", "--bold", "--green", "--normal", "--no-newline",
                                                 "--switch=", "--switch=ON", "text", "", "--white"], rng.randint(0, 4))]
    if c == 20:
        return ["capabilities"]
    if c == 21:
        return [rng.choice(["copy", "rename", "create_symlink", "compare_files", "cat"])]
    if c == 22:
        return ["md5sum"]
    if c == 23:
        return ["no_such_command", "x"]
    if c == 24:
        return ["copy", "-t"]
    return ["make_directory"]


def make_case(seed, index):
    rng = random.Random(f"{seed}:{index}")
    return [gen_cmd(rng) for _ in range(rng.randint(2, 8))]


def hash_bytes(data):
    import hashlib
    return hashlib.sha256(data).hexdigest()[:16]


def archive_summary(data):
    """Members of a tar or zip archive without timestamps (directories
    touched during the run carry the current time)."""
    import io
    import tarfile
    import zipfile
    try:
        with tarfile.open(fileobj=io.BytesIO(data)) as tar:
            return "tar[" + "; ".join(
                f"{m.name} {m.type!r} {m.mode:o} {m.size} {m.linkname} "
                + (hash_bytes(tar.extractfile(m).read()) if m.isfile() else "")
                for m in tar.getmembers()) + "]"
    except Exception:
        pass
    try:
        with zipfile.ZipFile(io.BytesIO(data)) as z:
            return "zip[" + "; ".join(f"{i.filename} {i.file_size} {hash_bytes(z.read(i))}" for i in z.infolist()) + "]"
    except Exception:
        return None


def snapshot(root):
    out = []
    for path in sorted(root.rglob("*")):
        rel = path.relative_to(root).as_posix()
        st = path.lstat()
        if stat.S_ISLNK(st.st_mode):
            out.append(f"{rel} link -> {os.readlink(path)}")
        elif stat.S_ISDIR(st.st_mode):
            out.append(f"{rel} dir {oct(st.st_mode & 0o777)}")
        else:
            data = path.read_bytes()
            # Compressed archives are compared by content: the oracle's
            # vendored liblzma and gzip's header timestamp make the bytes
            # library- and time-dependent.
            for magic, module in ((b"\xfd7zXZ", "lzma"), (b"\x1f\x8b", "gzip"), (b"BZh", "bz2")):
                if data.startswith(magic):
                    try:
                        data = __import__(module).decompress(data)
                    except Exception:
                        pass
                    break
            summary = archive_summary(data)
            out.append(f"{rel} file {oct(st.st_mode & 0o777)} " + (summary or f"{len(data)} {hash_bytes(data)}"))
    return "\n".join(out)


def run_side(cmake, cmds, work):
    if work.exists():
        subprocess.run(["chmod", "-R", "u+rwx", str(work)], capture_output=True)
        shutil.rmtree(work)
    (work / "d" / "e").mkdir(parents=True)
    (work / "a.txt").write_text("alpha\r\nbeta\n")
    (work / "b.txt").write_text("alpha\nbeta\n")
    (work / "d" / "x.txt").write_text("x")
    os.symlink("a.txt", work / "l")
    # Fixed timestamps: archives record them.
    for path in (work / "a.txt", work / "b.txt", work / "d" / "x.txt", work / "d" / "e", work / "d", work):
        os.utime(path, (1700000000, 1700000000))
    results = []
    env = {k: v for k, v in os.environ.items() if k not in ("CLICOLOR_FORCE", "CLICOLOR", "COLOR")}
    for cmd in cmds:
        proc = subprocess.run([cmake, "-E", *cmd], cwd=work, capture_output=True, timeout=30, env=env)
        norm = lambda b: b.replace(str(work).encode(), b"<W>").replace(str(cmake).encode(), b"<CMAKE>")
        out = norm(proc.stdout)
        if cmd and cmd[0] == "capabilities":
            out = b"<capabilities>"  # version strings and the generator list differ by build
        results.append((cmd, proc.returncode, out, norm(proc.stderr)))
    return results, snapshot(work)


def check(args, index):
    cmds = make_case(args.seed, index)
    root = Path(tempfile.mkdtemp(prefix="fc-", dir=args.work))
    try:
        oracle = run_side(args.oracle, cmds, root / "w")
        candidate = run_side(args.candidate, cmds, root / "w")
        if oracle == candidate:
            return None
        keep = Path(args.out) / f"case-{args.seed}-{index}"
        keep.mkdir(parents=True, exist_ok=True)
        lines = []
        for (cmd, orc, oout, oerr), (_, crc, cout, cerr) in zip(oracle[0], candidate[0]):
            mark = "" if (orc, oout, oerr) == (crc, cout, cerr) else "  <<< DIFF"
            lines.append(f"$ cmake -E {' '.join(cmd)}{mark}\n  oracle rc={orc} out={oout!r} err={oerr!r}\n  cand   rc={crc} out={cout!r} err={cerr!r}")
        if oracle[1] != candidate[1]:
            lines.append("TREE DIFF\n--- oracle\n" + oracle[1] + "\n--- candidate\n" + candidate[1])
        (keep / "report.txt").write_text("\n".join(lines) + "\n")
        return str(keep)
    finally:
        subprocess.run(["chmod", "-R", "u+rwx", str(root)], capture_output=True)
        shutil.rmtree(root, ignore_errors=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--candidate", required=True)
    parser.add_argument("--oracle", default=ORACLE)
    parser.add_argument("--seed", type=int, default=1)
    parser.add_argument("-n", type=int, default=500)
    parser.add_argument("-j", type=int, default=8)
    parser.add_argument("--out", default="fuzz-cli-out")
    args = parser.parse_args()
    Path(args.out).mkdir(parents=True, exist_ok=True)
    args.work = tempfile.mkdtemp(prefix="fuzz-cli-")
    with concurrent.futures.ThreadPoolExecutor(args.j) as pool:
        fails = [f for f in pool.map(lambda i: check(args, i), range(args.n)) if f]
    shutil.rmtree(args.work, ignore_errors=True)
    print(json.dumps({"seed": args.seed, "cases": args.n, "failures": len(fails),
                      "examples": fails[:10]}, indent=1))
    return 1 if fails else 0


if __name__ == "__main__":
    raise SystemExit(main())
