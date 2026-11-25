#!/usr/bin/env python3
"""Compare every file in fresh Ninja configure trees produced by two cmakes.

Usage: treediff.py --candidate <staged bin/cmake> SOURCE [cmake args...]

The oracle and candidate configure the same unmodified source tree into
separate build directories.  Every regular file and symlink in those trees is
compared after the same path/scratch-name normalization used by statediff.py.
CMakeConfigureLog.yaml timing-like scalar values, .ninja_log, and .ninja_deps
are treated as inherently nondeterministic; all other file bytes matter.
"""
import argparse
import difflib
import hashlib
import os
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

VALIDATION = "/tmp/cmake-rust-validation.hSLgEZHR"
ORACLE = Path(VALIDATION) / "kitware-reference/bin/cmake"


def b(s: str) -> bytes:
    return s.encode(errors="surrogateescape")


def normalizer(build: Path, cmake: Path):
    """Map each side's own build and tool locations to fixed placeholders."""
    prefixes = {str(cmake.absolute().parent.parent), str(cmake.resolve().parent.parent)}
    root = os.path.realpath(f"{VALIDATION}/kitware")

    def esc_bytes(text: str) -> bytes:
        return re.escape(b(text))

    rules: list[tuple[re.Pattern[bytes], bytes]] = []
    for text, replacement in (
        (str(build), "<BUILD>"),
        (os.path.realpath(build), "<BUILD>"),
    ):
        rules.append((re.compile(esc_bytes(text)), b(replacement)))
    for prefix in prefixes:
        rules.append((re.compile(esc_bytes(prefix) + rb"(/share/cmake-4\.1)?(?![\w.-])"), b"<PREFIX>"))
    for prefix_root in (root, f"{VALIDATION}/kitware"):
        rules.append((re.compile(esc_bytes(prefix_root) + rb"(?![\w.-])"), b"<PREFIX>"))
    # The oracle runs from its build tree, so the install-prefix component
    # derived from CMAKE_ROOT is the validation checkout's grandparent rather
    # than an installed prefix.  statediff.py normalizes it in list context;
    # configure logs also spell it as a path prefix.
    grandparent = os.path.dirname(os.path.dirname(root))
    rules.append((re.compile(esc_bytes(grandparent) + rb'(?![\w.-])'), b"<PREFIX>"))
    rules.extend(
        [
            (re.compile(rb"cmTC_[0-9a-f]{5}"), b"cmTC_<ID>"),
            (re.compile(rb"TryCompile-[A-Za-z0-9]{6}"), b"TryCompile-<ID>"),
            (re.compile(rb"::@\(0x[0-9a-f]+\)"), b"::@(<DIRECTORY-ID>)"),
            (re.compile(rb"/tmp/cc[A-Za-z0-9]{6}\."), b"/tmp/cc<TMP>."),
            # nvcc derives its anonymous-namespace prefix from the object
            # directory and names intermediate files after its process id.
            (re.compile(rb"_GLOBAL__N__[0-9a-f]{8}"), b"_GLOBAL__N__<NVHASH>"),
            (re.compile(rb"(?m)^_[0-9a-f]{8}(_\d+_.*_cu_[0-9a-f]{8})"), b"_<NVHASH>\1"),
            (re.compile(rb"tmpxft_[0-9a-f]{8}"), b"tmpxft_<PID>"),
            # CMP0116 internal depfile names hash the absolute depfile path.
            (re.compile(rb"CMakeFiles/d/[0-9a-f]{64}\.d"), b"CMakeFiles/d/<SHA256>.d"),
            # Configure-check executables may record process timing in YAML-like output.
            (re.compile(rb"(?m)^(\s*)(time|timestamp|duration|elapsed): [^\n]*$"), rb"\1\2: <TIME>"),
        ]
    )

    def apply(data: bytes) -> bytes:
        for pattern, replacement in rules:
            data = pattern.sub(replacement, data)
        return data

    return apply


def configure(cmake: Path, source: Path, build: Path, extra: list[str], log: Path) -> int:
    shutil.rmtree(build, ignore_errors=True)
    env = {k: v for k, v in os.environ.items() if k not in ("MAKEFLAGS", "CMAKE_RUST_DATA_ROOT")}
    with log.open("wb") as stream:
        proc = subprocess.run(
            [str(cmake), "-S", str(source), "-B", str(build), "-G", "Ninja", *extra],
            stdout=stream,
            stderr=subprocess.STDOUT,
            env=env,
        )
    return proc.returncode


def ignored(rel: str) -> bool:
    name = Path(rel).name
    # FetchContent clones are inputs copied into the build tree.  Their .git
    # metadata contains timestamps, pack order, and filesystem-specific index
    # entries rather than CMake-generated configuration state.
    return name in (".ninja_log", ".ninja_deps") or "/.git/" in f"/{rel}/"


def gather(build: Path, normalize) -> dict[str, tuple[str, bytes]]:
    files: dict[str, tuple[str, bytes]] = {}
    if not build.exists():
        return files
    for path in sorted(build.rglob("*")):
        rel = path.relative_to(build).as_posix()
        if ignored(rel):
            continue
        norm_rel = normalize(rel.encode(errors="surrogateescape")).decode(errors="surrogateescape")
        if path.is_symlink():
            files[norm_rel] = ("symlink", normalize(os.readlink(path).encode(errors="surrogateescape")))
        elif path.is_file():
            files[norm_rel] = ("file", normalize(path.read_bytes()))
    return files


def printable(data: bytes) -> bool:
    if b"\0" in data:
        return False
    try:
        data.decode("utf-8")
        return True
    except UnicodeDecodeError:
        return False


def describe_diff(rel: str, oracle: tuple[str, bytes] | None, candidate: tuple[str, bytes] | None) -> list[str]:
    if oracle is None:
        return [f"only in candidate: {rel}"]
    if candidate is None:
        return [f"only in oracle: {rel}"]
    if oracle[0] != candidate[0]:
        return [f"type differs: {rel}: oracle {oracle[0]}, candidate {candidate[0]}"]
    oh = hashlib.sha256(oracle[1]).hexdigest()[:16]
    ch = hashlib.sha256(candidate[1]).hexdigest()[:16]
    header = f"content differs: {rel} (oracle sha256 {oh}, candidate sha256 {ch})"
    if printable(oracle[1]) and printable(candidate[1]):
        diff = difflib.unified_diff(
            oracle[1].decode(errors="replace").splitlines(),
            candidate[1].decode(errors="replace").splitlines(),
            "oracle",
            "candidate",
            n=2,
            lineterm="",
        )
        return [header, *list(diff)[:30]]
    return [header, f"  binary content lengths: oracle {len(oracle[1])}, candidate {len(candidate[1])}"]


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--candidate", type=Path, required=True)
    parser.add_argument("--oracle", type=Path, default=ORACLE)
    parser.add_argument("--work", type=Path, help="keep configure trees and logs here")
    parser.add_argument("--show", type=int, default=12)
    parser.add_argument("source", type=Path)
    parser.add_argument("cmake_args", nargs=argparse.REMAINDER)
    args = parser.parse_args()

    work = args.work or Path(tempfile.mkdtemp(prefix="treediff-"))
    work.mkdir(parents=True, exist_ok=True)
    source = args.source.resolve()
    oracle_build = work / "oracle"
    candidate_build = work / "candidate"
    oracle_rc = configure(args.oracle, source, oracle_build, args.cmake_args, work / "oracle-configure.log")
    candidate_rc = configure(args.candidate, source, candidate_build, args.cmake_args, work / "candidate-configure.log")

    oracle = gather(oracle_build, normalizer(oracle_build, args.oracle))
    candidate = gather(candidate_build, normalizer(candidate_build, args.candidate))
    keys = sorted(set(oracle) | set(candidate))
    differing = [key for key in keys if oracle.get(key) != candidate.get(key)]
    print(
        f"configure oracle={oracle_rc} candidate={candidate_rc}; "
        f"files oracle={len(oracle)} candidate={len(candidate)}; differing={len(differing)}; work={work}"
    )
    for key in differing[: args.show]:
        for line in describe_diff(key, oracle.get(key), candidate.get(key)):
            print("  " + line[:500])
    return 1 if differing or oracle_rc != candidate_rc else 0


if __name__ == "__main__":
    sys.exit(main())
