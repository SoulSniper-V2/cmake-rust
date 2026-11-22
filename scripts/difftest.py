#!/usr/bin/env python3
"""Differential script-mode tests: run each tests/scripts/**/*.cmake with the
pinned Kitware CMake 4.1.0 oracle and with the candidate under `cmake -P`,
and compare exit code, stdout and stderr exactly (after replacing the
per-test working directory and binary paths with placeholders).

Each test runs in a fresh temporary directory (its cwd); the script is
copied there as `test.cmake` so diagnostics show the same relative path.
A sibling `<name>.args` file may hold extra arguments placed before -P
(one per line, e.g. -DFOO=bar).

Usage: scripts/difftest.py [--candidate PATH] [--oracle PATH] [-j N] [-v] [paths/globs...]
"""
import argparse
import concurrent.futures
import difflib
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
ORACLE = Path("/tmp/cmake-rust-validation.hSLgEZHR/kitware-reference/bin/cmake")
DATA_ROOT = os.path.realpath("/tmp/cmake-rust-validation.hSLgEZHR/kitware")


def run(binary, script, args, env_extra):
    with tempfile.TemporaryDirectory(prefix="difftest.") as temp:
        work = Path(temp).resolve()
        target = work / "test.cmake"
        shutil.copy(script, target)
        data = script.with_suffix(".data")
        if data.is_dir():
            shutil.copytree(data, work / "data", symlinks=True)
        env = {
            "PATH": "/usr/bin:/bin",
            "HOME": str(work),
            "LANG": "C",
            "LC_ALL": "C",
            "PWD": str(work),
            "CMAKE_RUST_DATA_ROOT": DATA_ROOT,
            **env_extra,
        }
        try:
            proc = subprocess.run(
                [str(binary), *args, "-P", "test.cmake"],
                cwd=work, env=env, capture_output=True, timeout=60,
            )
            code = proc.returncode
            out = proc.stdout.decode(errors="replace")
            err = proc.stderr.decode(errors="replace")
        except subprocess.TimeoutExpired:
            code, out, err = "timeout", "", ""
        def norm(text):
            return (text.replace(str(work), "<WORK>")
                        .replace(str(binary.parent.parent), "<PREFIX>")
                        .replace(DATA_ROOT, "<ROOT>"))
        return code, norm(out), norm(err)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--candidate", type=Path, default=ROOT / "target/release/cmake")
    parser.add_argument("--oracle", type=Path, default=ORACLE)
    parser.add_argument("-j", type=int, default=8)
    parser.add_argument("-v", action="store_true", help="show diffs")
    parser.add_argument("paths", nargs="*")
    args = parser.parse_args()
    patterns = args.paths or [str(ROOT / "tests/scripts")]
    scripts = []
    for pattern in patterns:
        path = Path(pattern)
        if path.is_dir():
            scripts += sorted(path.rglob("*.cmake"))
        else:
            scripts.append(path)
    scripts = [s.resolve() for s in scripts]

    def one(script):
        extra = []
        args_file = script.with_suffix(".args")
        if args_file.exists():
            extra = [line for line in args_file.read_text().splitlines() if line.strip()]
        expected = run(args.oracle.resolve(), script, extra, {})
        actual = run(args.candidate.resolve(), script, extra, {})
        return script, expected, actual

    failures = 0
    with concurrent.futures.ThreadPoolExecutor(args.j) as pool:
        for script, expected, actual in pool.map(one, scripts):
            name = script.relative_to(ROOT) if script.is_relative_to(ROOT) else script
            if expected == actual:
                print(f"PASS {name}")
                continue
            failures += 1
            print(f"FAIL {name}")
            if args.v:
                for label, e, a in zip(("exit", "stdout", "stderr"), expected, actual):
                    if e != a:
                        print(f"  --- {label} differs")
                        diff = difflib.unified_diff(str(e).splitlines(), str(a).splitlines(),
                                                    "oracle", "candidate", lineterm="")
                        for line in list(diff)[:60]:
                            print("    " + line)
    print(f"{len(scripts) - failures}/{len(scripts)} passed")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
