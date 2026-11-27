#!/usr/bin/env python3
"""Compare CTest's streams, exit status and output files with Kitware CTest.

Each case configures an input project once. The oracle and candidate run against
copies of the same initial build tree at the same path. Multiple invocations in
a case share their side's Testing/ state.
"""
import argparse
import concurrent.futures
import difflib
import os
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

sys.dont_write_bytecode = True
from difftest import DATA_ROOT, ORACLE, ROOT

FIXTURES = ROOT / "tests/ctest"


def cases():
    for pattern in ("case-1$", "^nested$", "case-[2468]$", "^case-1[012]$", "^missing$", ".*"):
        for label in ("", "common", "parity-0", "parity-1", "nested"):
            for mode, flags in (("list", ["-N"]), ("json", ["--show-only=json-v1"]),
                                ("labels", ["--print-labels"])):
                yield "selection", f"{pattern}/{label}/{mode}", [
                    [*flags, "-R", pattern, *(["-L", label] if label else [])]
                ], {}
    for index in ("2,5,2", ",,3", ",,0,2,9,13", "2,3,1", "99,100,1"):
        for flags in ([], ["-R", "case-1"], ["-R", "^nested$", "-U", "1"]):
            yield "selection", f"{index}/{flags}", [["-N", "-I", index, *flags]], {}
    for name, flags in (
        ("exclude", ["-N", "-E", "^case-1"]),
        ("exclude-combined", ["-V", "-N", "-R", "case-", "-E", "1"]),
        ("labels-and", ["-N", "-L", "common", "-L", "parity-1"]),
        ("labels-exclude-and", ["-N", "-LE", "common", "-LE", "parity-1"]),
        ("include-file", ["-N", "--tests-from-file", "names.txt"]),
        ("exclude-file", ["-N", "--exclude-from-file", "names.txt"]),
        ("parallel-one", ["-j1", "-R", "case-[234]$"]),
        ("progress", ["--progress", "-R", "case-1$"]),
        ("verbose", ["-V", "-R", "case-1$"]),
        ("extra-verbose", ["-VV", "-R", "case-1$"]),
        ("quiet", ["-Q", "-R", "case-1$"]),
        ("output-log", ["-V", "-R", "case-1$", "--output-log", "console.log"]),
        ("random", ["--schedule-random", "--schedule-random-seed", "257", "-R", "case-[2345]$"]),
        ("quiet-output-log", ["-Q", "-R", "case-1$", "--output-log", "console.log"]),
    ):
        yield "selection", name, [flags], {}
    for name, env in (
        ("parallel-env-one", {"CTEST_PARALLEL_LEVEL": "1"}),
        ("parallel-env-invalid", {"CTEST_PARALLEL_LEVEL": "invalid"}),
        ("parallel-env-unlimited", {"CTEST_PARALLEL_LEVEL": "0"}),
    ):
        yield "selection", name, [["-R", "^nested$"]], env
    for name in ("pass", "fail", "will-fail", "required-regex", "missing-regex", "forbidden-regex",
                 "skipped-regex", "skipped-code", "disabled", "environment", "work-dir",
                 "missing-file", "missing-executable", "timeout", "timeout-after-match",
                 "timeout-signal", "no-timeout", "with-properties", "depender"):
        for mode, flags in (("regular", []), ("verbose", ["-V", "--output-on-failure"]),
                            ("junit", ["--output-junit", "junit.xml"]), ("json", ["--show-only=json-v1"])):
            yield "properties", f"{name}/{mode}", [["-R", f"^{name}$", *flags]], {}
    for name, flags in (
        ("eventual-pass", ["-R", "^eventual-pass$", "--repeat", "until-pass:3"]),
        ("eventual-fail", ["-R", "^eventual-fail$", "--repeat", "until-fail:3"]),
        ("retry-timeout", ["-R", "^retry-timeout$", "--repeat", "after-timeout:3"]),
        ("global-timeout", ["-R", "^global-timeout$", "--timeout", "0.05"]),
        ("stop-on-failure", ["-R", "^(fail|skipped-code)$", "--stop-on-failure"]),
        ("deps-and-cost", ["-R", "^depend", "--parallel", "2"]),
        ("quiet-failure", ["-R", "^fail$", "-Q"]),
        ("output-on-failure-env", ["-R", "^fail$"]),
    ):
        yield "properties", name, [flags], {"CTEST_OUTPUT_ON_FAILURE": "ON"} if name.endswith("-env") else {}
    yield "properties", "rerun-failed", [["-R", "^(fail|pass)$"], ["--rerun-failed"]], {}
    for name, flags in (
        ("present", ["-R", "^allocated$"]),
        ("all", ["--resource-spec-file", "resources.json", "-V"]),
        ("junit", ["--resource-spec-file", "resources.json", "--output-junit", "junit.xml"]),
        ("json", ["--resource-spec-file", "resources.json", "--show-only=json-v1"]),
        ("nonexistent", ["--resource-spec-file", "does-not-exist.json"]),
    ):
        yield "resources", name, [flags], {}
    for name, flags, env in (
        ("attached", ["-j2"], {}),
        ("long", ["--parallel", "2"], {}),
        ("environment", [], {"CTEST_PARALLEL_LEVEL": "2"}),
    ):
        yield "parallel", name, [flags], env
    for config in ("", "Debug", "Release"):
        for flags in (["-N"], ["--show-only=json-v1"], ["-V"]):
            yield "config", f"{config}/{flags}", [[*flags, *(["-C", config] if config else [])]], {}
    for name, flags in (
        ("include", ["-R", "^consumer$", "-V"]),
        ("exclude-all", ["-R", "^consumer$", "-FA", "store", "-V"]),
        ("exclude-setup", ["-R", "^consumer$", "-FS", "store", "-V"]),
        ("exclude-cleanup", ["-R", "^consumer$", "-FC", "store", "-V"]),
        ("cleanup-only", ["-R", "^cleanup$", "-V"]),
        ("failed-setup", ["-R", "^broken-consumer$", "-V"]),
        ("failed-setup-json", ["-R", "^broken-consumer$", "--show-only=json-v1"]),
    ):
        yield "fixtures", name, [flags], {}
    for mode in ("", "error", "ignore"):
        for option in ([], ["-N"], ["--show-only=json-v1"], ["-Q"], ["-V"], ["--print-labels"]):
            yield "empty", f"{mode}/{option}", [[*option, *([f"--no-tests={mode}"] if mode else [])]], {}
    for arg in ("-U", "--foo", "--test-dir=absent", "--no-tests=incorrect", "--show-only=incorrect",
                "--repeat=invalid", "--parallel=invalid"):
        yield "empty", f"invalid/{arg}", [[arg]], {}


def normalize(data, work, path):
    data = data.replace(os.fsencode(work), b"<WORK>").replace(os.fsencode(FIXTURES), b"<FIXTURES>")
    data = re.sub(rb"(?<= )\d+\.\d+(?= sec\b)", b"<SECONDS>", data)
    data = re.sub(rb"(?m)(?<=: )\w{3} \d{2} \d{2}:\d{2} UTC$", b"<DATETIME>", data)
    data = re.sub(rb"(?m)(?<=time elapsed: )\d\d:\d\d:\d\d$", b"<DURATION>", data)
    if path.name == "CTestCostData.txt":
        values, found, rest = data.partition(b"---\n")
        # A zero cost with zero runs is part of the scheduling state.
        values = re.sub(rb"(?m)( [1-9]\d* )[-0-9.e+]+$", rb"\1<SECONDS>", values)
        data = values + found + rest
    if path.suffix == ".xml":
        data = re.sub(rb'(?<=timestamp=")[^"]*', b"<DATETIME>", data)
        data = re.sub(rb'(?<=time=")[^"]*', b"<SECONDS>", data)
    return data


def run_case(case, binaries, configure):
    fixture, _, steps, extra = case
    with tempfile.TemporaryDirectory(prefix="ct-diff.") as directory:
        root = Path(directory)
        build = root / "build"
        env = {
            "PATH": "/usr/bin:/bin", "HOME": directory, "LANG": "C", "LC_ALL": "C", "TZ": "UTC",
            "CMAKE_RUST_DATA_ROOT": DATA_ROOT, **extra,
        }
        command = [str(configure), "-S", str(FIXTURES / fixture), "-B", str(build),
                   "-G", "Ninja Multi-Config" if fixture == "config" else "Ninja"]
        subprocess.run(command, check=True, capture_output=True, env=env, timeout=30)
        snapshot = root / "snapshot"
        shutil.copytree(build, snapshot)
        results = []
        for side in binaries:
            result = {}
            for number, flags in enumerate(steps):
                where = root if fixture == "config" else build
                argv = ["--test-dir", "build", *flags] if where == root else flags
                proc = subprocess.run([str(side), *argv], cwd=where, env=env, capture_output=True, timeout=15)
                result[(number, "exit")] = proc.returncode
                for key in ("stdout", "stderr"):
                    result[(number, key)] = normalize(getattr(proc, key), root, Path(key))
                for path in sorted(build.rglob("*")):
                    if path.is_file() and not (snapshot / path.relative_to(build)).exists():
                        result[(number, str(path.relative_to(build)))] = normalize(path.read_bytes(), root, path)
            results.append(result)
            shutil.rmtree(build)
            shutil.copytree(snapshot, build)
        return results


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--candidate", type=Path, default=ROOT / "target/release/ctest")
    parser.add_argument("--oracle", type=Path, default=ORACLE.with_name("ctest"))
    parser.add_argument("--oracle-cmake", type=Path, default=ORACLE)
    parser.add_argument("-j", type=int, default=4)
    parser.add_argument("-v", action="store_true")
    parser.add_argument("pattern", nargs="*", help="run cases containing these strings")
    args = parser.parse_args()
    selected = [case for case in cases() if not args.pattern or any(
        pattern in f"{case[0]}/{case[1]}" for pattern in args.pattern)]
    failures = 0
    binaries = [args.oracle.resolve(strict=True), args.candidate.resolve(strict=True)]
    configure = args.oracle_cmake.resolve(strict=True)
    with concurrent.futures.ThreadPoolExecutor(max_workers=args.j) as pool:
        for case, (reference, candidate) in zip(
                selected, pool.map(lambda case: run_case(case, binaries, configure), selected)):
            if reference == candidate:
                continue
            failures += 1
            print(f"FAIL {case[0]}/{case[1]}")
            if args.v:
                for key in sorted(reference.keys() | candidate.keys()):
                    a, b = reference.get(key), candidate.get(key)
                    if a == b:
                        continue
                    print(f"  {key}")
                    if isinstance(a, bytes) and isinstance(b, bytes):
                        print("".join(difflib.unified_diff(a.decode(errors="replace").splitlines(True),
                                                          b.decode(errors="replace").splitlines(True),
                                                          "Kitware CTest", "Rust CTest")), end="")
                    else:
                        print(f"    Kitware {a!r}\n    Rust    {b!r}")
    print(f"{len(selected) - failures}/{len(selected)} matched")
    return failures != 0


if __name__ == "__main__":
    sys.exit(main())
