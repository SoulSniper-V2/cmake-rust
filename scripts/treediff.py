#!/usr/bin/env python3
"""Compare every file in fresh Ninja configure trees produced by two cmakes.

Usage: treediff.py --candidate <staged bin/cmake> [--generator G] [--json OUT] SOURCE [cmake args...]

The oracle and candidate configure the same unmodified source tree into
separate build directories.  Every regular file and symlink in those trees is
compared after the same path/scratch-name normalization used by statediff.py.
CMakeConfigureLog.yaml timing-like scalar values, .ninja_log, and .ninja_deps
are treated as inherently nondeterministic; all other file bytes matter.
"""
import argparse
import difflib
import hashlib
import json
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
    # Extra-generator project files sometimes refer to the build directory by
    # an unresolved source-relative path containing ../ components.  The tail
    # <o|c>/build is the only part visible in either slash direction there.
    for sep in ("/", "\\"):
        tail = build.parent.name + sep + build.name
        rules.append((re.compile(rb"(?<![\w.-])" + esc_bytes(tail) + rb"(?![\w.-])"), b"<BUILD-REL>"))
        # CodeBlocks virtual folders also list the bare side directory.
        side = build.parent.parent.name + sep + build.parent.name
        rules.append((re.compile(rb"(?<![\w.-])" + esc_bytes(side) + rb"(?![\w.-])"),
                      b(build.parent.parent.name + sep + "<SIDE>")))
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
            # Nested configure logs include wall-clock times in human-readable
            # progress lines.  They are scheduling noise, not generated state.
            (re.compile(rb"(-- (?:Configuring|Generating) done) \([^\n()]*\)"), rb"\1 (<TIME>)"),
        ]
    )

    def apply(data: bytes) -> bytes:
        for pattern, replacement in rules:
            data = pattern.sub(replacement, data)
        return data

    return apply


FILEAPI_QUERIES = ("codemodel-v2", "cache-v2", "cmakeFiles-v1", "toolchains-v1")


def configure(cmake: Path, source: Path, build: Path, extra: list[str], log: Path, generator: str,
              queries: tuple[str, ...] = ()) -> int:
    shutil.rmtree(build, ignore_errors=True)
    # Stateless file-API queries (e.g. codemodel-v2): their replies become
    # part of the compared tree.
    for kind in queries:
        query = build / ".cmake/api/v1/query"
        query.mkdir(parents=True, exist_ok=True)
        (query / kind).touch()
    env = {k: v for k, v in os.environ.items() if k not in ("MAKEFLAGS", "CMAKE_RUST_DATA_ROOT")}
    with log.open("wb") as stream:
        proc = subprocess.run(
            [str(cmake), "-S", str(source), "-B", str(build), "-G", generator, *extra],
            stdout=stream,
            stderr=subprocess.STDOUT,
            env=env,
        )
    return proc.returncode


def normalize_rebuild_manifest(data: bytes) -> bytes:
    """Canonicalize order-only variation in CMake regeneration dependency lists.

    Kitware sorts these dependencies before writing them.  The oracle and
    candidate necessarily sort different literal strings because their cmake
    data roots are installed in different prefixes, even when the normalized
    dependency sets are identical.  Dependency order is not semantically
    significant here, so sort the normalized tokens for comparison.
    """
    out = []
    missing_inputs = False
    for line in data.splitlines(keepends=True):
        ending = b""
        body = line
        if body.endswith(b"\n"):
            body, ending = body[:-1], b"\n"
        if body.endswith(b"\r"):
            body, ending = body[:-1], b"\r" + ending
        if b": RERUN_CMAKE" in body:
            head, deps = body.split(b": RERUN_CMAKE", 1)
            tokens = deps.strip().split()
            body = head + b": RERUN_CMAKE " + b" ".join(sorted(tokens))
        elif missing_inputs and body.startswith(b"build "):
            tokens = body.strip().split()
            body = b"build " + b" ".join(sorted(tokens[1:]))
            missing_inputs = False
        elif missing_inputs and body.strip() and not body.startswith(b"#"):
            missing_inputs = False
        if b"A missing CMake input file is not an error." in body:
            missing_inputs = True
        out.append(body + ending)
    return b"".join(out)


def ignored(rel: str) -> bool:
    name = Path(rel).name
    # FetchContent clones are inputs copied into the build tree.  Their .git
    # metadata contains timestamps, pack order, and filesystem-specific index
    # entries rather than CMake-generated configuration state.
    # ABI probe executables contain DWARF paths to the cmake installation and
    # try_compile scratch directory.  Replacing those byte strings in-place is
    # not a valid ELF transformation because it changes string lengths, so the
    # probe binaries themselves are inherently tool-location dependent.
    abi_probe = name.startswith("CMakeDetermineCompilerABI_") and name.endswith(".bin")
    return name in (".ninja_log", ".ninja_deps") or abi_probe or "/.git/" in f"/{rel}/"


def canonical_target_reply(data: bytes) -> bytes:
    """A codemodel target object with each backtrace index resolved to its
    frames and the dependencies sorted.  Upstream orders a target's
    dependencies by cmGeneratorTarget pointer (cmTargetDepend::operator<),
    which also decides the backtrace node numbering; everything else stays
    compared.  (projdiff.py byte-compares replies, layout included.)"""
    try:
        obj = json.loads(data)
    except ValueError:
        return data
    graph = obj.pop("backtraceGraph", None)
    if not isinstance(graph, dict):
        return data
    nodes, commands, files = graph.get("nodes", []), graph.get("commands", []), graph.get("files", [])

    def frames(index):
        out = []
        while isinstance(index, int) and 0 <= index < len(nodes):
            node = nodes[index]
            command = node.get("command")
            out.append([files[node["file"]] if node.get("file", -1) < len(files) else None,
                        node.get("line"), commands[command] if isinstance(command, int) else None])
            index = node.get("parent")
        return out

    def walk(value):
        if isinstance(value, dict):
            return {k: (frames(v) if k == "backtrace"
                        else [frames(i) for i in v] if k == "backtraces" and isinstance(v, list)
                        else walk(v)) for k, v in value.items()}
        if isinstance(value, list):
            return [walk(v) for v in value]
        return value

    obj = walk(obj)
    if isinstance(obj.get("dependencies"), list):
        obj["dependencies"].sort(key=lambda dep: json.dumps(dep, sort_keys=True))
    return json.dumps(obj, indent=1, sort_keys=True).encode()


def sort_makefile2_target_depends(text):
    """Sort each run of consecutive `<target>/all: <dependency>` lines that
    share one target in a CMakeFiles/Makefile2.  cmGlobalUnixMakefileGenerator3
    writes them by iterating a cmTargetDependSet, which is ordered by
    cmGeneratorTarget pointer value, so their order changes between runs of
    Kitware's own CMake; everything else in the file stays compared as is."""
    lines = text.split("\n")
    out, i = [], 0
    pattern = re.compile(r"^(\S+/all): \S+$")
    while i < len(lines):
        m = pattern.match(lines[i])
        if not m:
            out.append(lines[i])
            i += 1
            continue
        j = i
        while j < len(lines) and pattern.match(lines[j]) and pattern.match(lines[j]).group(1) == m.group(1):
            j += 1
        out.extend(sorted(lines[i:j]))
        i = j
    return "\n".join(out)


def gather(build: Path, normalize) -> dict[str, tuple[str, bytes]]:
    files: dict[str, tuple[str, bytes]] = {}
    if not build.exists():
        return files
    for path in sorted(build.rglob("*")):
        rel = path.relative_to(build).as_posix()
        if ignored(rel):
            continue
        norm_rel = normalize(rel.encode(errors="surrogateescape")).decode(errors="surrogateescape")
        if rel.startswith(".cmake/api/v1/reply/") and path.is_file():
            # Reply objects are named by content hash (the index and error
            # replies by time): key them by kind, and hide the hashed
            # references between them, as projdiff.py does.
            name = path.name
            if name.startswith("index-"):
                continue
            key = re.sub(r"-[0-9a-f]{20}\.json$", "", name)
            key = re.sub(r"^error-[0-9T:-]+\.json$", "error", key)
            data = re.sub(rb'"jsonFile" ?: "[^"]*"', b'"jsonFile" : "<HASH>"', normalize(path.read_bytes()))
            if key.startswith("target-"):
                data = canonical_target_reply(data)
            files[f".cmake/api/v1/reply/{key}"] = ("file", data)
            continue
        if path.is_symlink():
            files[norm_rel] = ("symlink", normalize(os.readlink(path).encode(errors="surrogateescape")))
        elif path.is_file():
            data = normalize(path.read_bytes())
            is_rebuild_manifest = (
                rel == "build.ninja" or rel.endswith("/build.ninja")
                or rel == "CMakeFiles/common.ninja" or rel.endswith("/CMakeFiles/common.ninja")
            )
            if is_rebuild_manifest:
                data = normalize_rebuild_manifest(data)
            if rel == "CMakeFiles/Makefile2":
                data = sort_makefile2_target_depends(data.decode(errors="surrogateescape")).encode(errors="surrogateescape")
            if rel == "CMakeFiles/CMakeRuleHashes.txt":
                # Rule hashes cover absolute build and tool paths before
                # normalization.  Their target rows, order, and count remain
                # compared; the digest bytes are inherently installation
                # dependent when two separate prefixes are compared.
                data = re.sub(rb"(?m)^[0-9a-f]{32}(?= )", b"<RULE-HASH>", data)
            files[norm_rel] = ("file", data)
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
    parser.add_argument("--generator", default="Ninja", help="CMake generator to use")
    parser.add_argument("--json", type=Path, help="write machine-readable comparison results")
    parser.add_argument("--show", type=int, default=12)
    parser.add_argument("--fileapi", action="store_true",
                        help="request codemodel-v2, cache-v2, cmakeFiles-v1 and toolchains-v1 replies")
    parser.add_argument("source", type=Path)
    parser.add_argument("cmake_args", nargs=argparse.REMAINDER)
    args = parser.parse_args()

    work = args.work or Path(tempfile.mkdtemp(prefix="treediff-"))
    work.mkdir(parents=True, exist_ok=True)
    source = args.source.resolve()
    # Same basename and same path length on both sides: extra generators
    # name projects after the build directory (Kate, Eclipse), and debug
    # info and LTO objects embed the path, so equal spellings keep those
    # comparable after normalization.
    oracle_build = work / "o" / "build"
    candidate_build = work / "c" / "build"
    oracle_build.parent.mkdir(parents=True, exist_ok=True)
    candidate_build.parent.mkdir(parents=True, exist_ok=True)
    queries = FILEAPI_QUERIES if args.fileapi else ()
    oracle_rc = configure(args.oracle, source, oracle_build, args.cmake_args, work / "oracle-configure.log",
                          args.generator, queries)
    candidate_rc = configure(args.candidate, source, candidate_build, args.cmake_args,
                             work / "candidate-configure.log", args.generator, queries)

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
    if args.json:
        missing = []
        different = []
        for key in differing:
            if key not in oracle or key not in candidate:
                missing.append(key)
            else:
                different.append(key)
        args.json.parent.mkdir(parents=True, exist_ok=True)
        args.json.write_text(json.dumps({
            "work": str(work),
            "tree_files": len(keys),
            "tree_differences": len(differing),
            "missing": missing,
            "different": different,
            "configure_diffs": [],
            "oracle_exit": oracle_rc,
            "candidate_exit": candidate_rc,
        }, indent=2) + "\n")
    return 1 if differing or oracle_rc != candidate_rc else 0


if __name__ == "__main__":
    sys.exit(main())
