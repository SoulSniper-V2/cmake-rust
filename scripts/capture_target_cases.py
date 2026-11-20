#!/usr/bin/env python3
"""Regenerate tests/projects/targets/<case>/expected-configure.stderr from the
Kitware CMake 4.1.0 oracle (Ninja generator). Source/binary directories are
replaced by <SOURCE_DIR>/<BINARY_DIR>; the exit code goes to
expected-configure.exit. These goldens drive the in-process checks in
src/commands/targets/project_cases.rs.

Usage: scripts/capture_target_cases.py [case ...]
"""
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
ORACLE = "/tmp/cmake-rust-validation.hSLgEZHR/kitware-reference/bin/cmake"
CASES = ROOT / "tests/projects/targets"


def capture(case):
    with tempfile.TemporaryDirectory() as work:
        src = Path(work) / "src"
        build = Path(work) / "build"
        shutil.copytree(case, src, ignore=shutil.ignore_patterns("expected-configure.*"))
        proc = subprocess.run([ORACLE, "-S", str(src), "-B", str(build), "-G", "Ninja"],
                              capture_output=True, text=True)
        err = proc.stderr.replace(str(build), "<BINARY_DIR>").replace(str(src), "<SOURCE_DIR>")
        err = err.replace(work, "<WORK_DIR>")
        (case / "expected-configure.stderr").write_text(err.rstrip() + "\n" if err else "")
        (case / "expected-configure.exit").write_text(f"{proc.returncode}\n")
        print(f"{case.name}: exit {proc.returncode}, {len(err.splitlines())} stderr lines")


def main():
    names = sys.argv[1:] or sorted(p.name for p in CASES.iterdir() if p.is_dir())
    for name in names:
        capture(CASES / name)


if __name__ == "__main__":
    main()
