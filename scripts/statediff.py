#!/usr/bin/env python3
"""Compare end-of-configure interpreter state between the oracle and a candidate.

Both sides configure the same unmodified project with
CMAKE_PROJECT_TOP_LEVEL_INCLUDES=scripts/statediff/dump.cmake, which defers a
dump of every variable, cache entry, and each set directory, target and global
property (names from the oracle's --help-property-list).  Tool locations,
build directories, try_compile scratch names, compiler temp files and the
pointer-valued directory ids of `::@(0x...)` link markers are normalized; any
other difference is reported.

Usage: statediff.py --candidate <staged bin/cmake> SOURCE [cmake args...]
"""
import argparse
import difflib
import os
import re
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
VALIDATION = "/tmp/cmake-rust-validation.hSLgEZHR"
ORACLE = Path(VALIDATION) / "kitware-reference/bin/cmake"
DUMP = ROOT / "scripts/statediff/dump.cmake"


def property_list(oracle: Path, out: Path) -> None:
    names = subprocess.run([str(oracle), "--help-property-list"], check=True,
                           capture_output=True, text=True).stdout.split()
    expanded = []
    for name in names:
        if "LOCATION" in name:
            continue  # reading LOCATION is an error (CMP0026) or configure-time only
        if "<" not in name:
            expanded.append(name)
        elif name.count("<") == 1 and "<CONFIG>" in name:
            expanded += [name.replace("<CONFIG>", c)
                         for c in ("RELEASE", "DEBUG", "RELWITHDEBINFO", "MINSIZEREL")]
        elif name.count("<") == 1 and "<LANG>" in name:
            expanded += [name.replace("<LANG>", lang) for lang in ("C", "CXX", "ASM", "Fortran")]
    out.write_text("set(_cr_props\n" + "".join(f"  {n}\n" for n in expanded) + ")\n")


def normalizer(build: Path, cmake: Path):
    """Each side's own tool locations map to one placeholder."""
    prefixes = {str(cmake.absolute().parent.parent), str(cmake.resolve().parent.parent)}
    root = os.path.realpath(f"{VALIDATION}/kitware")
    rules = [
        (re.escape(str(build)), "<BUILD>"),
        (re.escape(os.path.realpath(build)), "<BUILD>"),
        *[(re.escape(p) + r"(/share/cmake-4\.1)?(?![\w.-])", "<PREFIX>") for p in prefixes],
        # The oracle runs from its build tree: CMAKE_ROOT is the source checkout
        # and _CMAKE_INSTALL_DIR (also a CMAKE_SYSTEM_PREFIX_PATH entry) is
        # CMAKE_ROOT's grandparent.
        *[(re.escape(r) + r"(?![\w.-])", "<PREFIX>") for r in (root, f"{VALIDATION}/kitware")],
        (re.escape(os.path.dirname(os.path.dirname(root))) + r"(?=[;\]])", "<PREFIX>"),
        (r"cmTC_[0-9a-f]{5}", "cmTC_<ID>"),
        (r"::@\(0x[0-9a-f]+\)", "::@(<DIRECTORY-ID>)"),
        (r"/tmp/cc[A-Za-z0-9]{6}\.", "/tmp/cc<TMP>."),
    ]
    compiled = [(re.compile(p), r) for p, r in rules]

    def apply(text: str) -> str:
        for pattern, replacement in compiled:
            text = pattern.sub(replacement, text)
        return text
    return apply


def run(cmake: Path, source: Path, build: Path, props: Path, extra: list[str]) -> list[str]:
    log = build.with_suffix(".log")
    with open(log, "w") as stream:
        proc = subprocess.run(
            [str(cmake), "-S", str(source), "-B", str(build), "-G", "Ninja",
             f"-DCMAKE_PROJECT_TOP_LEVEL_INCLUDES={DUMP}",
             f"-DCMAKE_RUST_STATEDIFF_PROPS={props}", *extra],
            stdout=stream, stderr=subprocess.STDOUT,
            env={k: v for k, v in os.environ.items() if k not in ("MAKEFLAGS", "CMAKE_RUST_DATA_ROOT")},
        )
    dump = build / "statediff-dump.txt"
    if not dump.is_file():
        sys.exit(f"{cmake}: no dump written (exit {proc.returncode}); see {log}")
    norm = normalizer(build, cmake)
    return [norm(line) for line in dump.read_text(errors="surrogateescape").splitlines()]


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--candidate", type=Path, required=True)
    parser.add_argument("--oracle", type=Path, default=ORACLE)
    parser.add_argument("--work", type=Path, help="keep build trees here")
    parser.add_argument("--show", type=int, default=40)
    parser.add_argument("source", type=Path)
    parser.add_argument("cmake_args", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    work = args.work or Path(tempfile.mkdtemp(prefix="statediff-"))
    work.mkdir(parents=True, exist_ok=True)
    props = work / "props.cmake"
    property_list(args.oracle, props)
    oracle = run(args.oracle, args.source.resolve(), work / "oracle", props, args.cmake_args)
    candidate = run(args.candidate, args.source.resolve(), work / "candidate", props, args.cmake_args)
    diff = [line for line in difflib.unified_diff(oracle, candidate, "oracle", "candidate",
                                                  n=0, lineterm="")
            if not line.startswith(("---", "+++", "@@"))]
    print(f"oracle={len(oracle)} candidate={len(candidate)} differing={len(diff)} lines; work={work}")
    for line in diff[:args.show]:
        print("  " + line[:300])
    return 1 if diff else 0


if __name__ == "__main__":
    sys.exit(main())
