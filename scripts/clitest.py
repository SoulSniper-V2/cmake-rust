#!/usr/bin/env python3
"""Run JSON argv cases against the pinned CMake and the Rust candidate.

Each side gets a private work directory. A case supplies "argv" or a sequence
of "steps" (each with "argv" and optional "cwd" or "stdin"), plus optional
"files", "dirs", "links", "env", and "inspect" paths. File values are UTF-8
strings or objects with "hex" and/or "mode". The substitutions {cmake} and
{work} work in argv, environment and fixture contents.
"""
import argparse
import concurrent.futures
import difflib
import json
import os
import re
import stat
import subprocess
import sys
import tempfile
from pathlib import Path

sys.dont_write_bytecode = True
from difftest import DATA_ROOT, ORACLE, ROOT


def run(binary, case, *, oracle):
    with tempfile.TemporaryDirectory(prefix="clitest.") as temp:
        work = Path(temp).resolve()

        def expand(value):
            return value.replace("{work}", str(work)).replace("{cmake}", str(binary))

        def normalize(data):
            return (data.replace(os.fsencode(binary), b"<CMAKE>")
                        .replace(os.fsencode(binary.parent.parent), b"<PREFIX>")
                        .replace(os.fsencode(work), b"<WORK>")
                        .replace(os.fsencode(work).replace(b"/", b"\\"), b"<WORK>")
                        .replace(os.fsencode(DATA_ROOT), b"<ROOT>"))

        for name in case.get("dirs", []):
            (work / name).mkdir(parents=True, exist_ok=True)
        for name, fixture in case.get("files", {}).items():
            path = work / name
            path.parent.mkdir(parents=True, exist_ok=True)
            if isinstance(fixture, str):
                fixture = {"text": fixture}
            data = (bytes.fromhex(fixture["hex"]) if "hex" in fixture
                    else expand(fixture.get("text", "")).encode())
            path.write_bytes(data)
            if "mode" in fixture:
                path.chmod(int(fixture["mode"], 8))
        for name, target in case.get("links", {}).items():
            path = work / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.symlink_to(expand(target))
        env = {
            "PATH": "/usr/bin:/bin",
            "HOME": str(work),
            "LANG": "C",
            "LC_ALL": "C",
            "TZ": "UTC",
            "CMAKE_RUST_DATA_ROOT": DATA_ROOT,
            **{name: expand(value) for name, value in case.get("env", {}).items()},
        }
        result = {}
        for index, step in enumerate(case.get("steps", [case]), start=1):
            args = [expand(arg) for arg in step["argv"]]
            cwd = work / step.get("cwd", ".")
            try:
                proc = subprocess.run(
                    [str(binary), *args], cwd=cwd, env={**env, "PWD": str(cwd)},
                    capture_output=True, input=step.get("stdin", "").encode(), timeout=30,
                )
            except subprocess.TimeoutExpired:
                result[f"step {index}"] = "timeout"
                continue
            stdout = normalize(proc.stdout)
            if step.get("elapsed"):
                stdout = re.sub(
                    rb"(?m)^Elapsed time \(seconds\): [0-9.eE+-]+$",
                    b"Elapsed time (seconds): <SECONDS>", stdout,
                )
            if step.get("ninja_capabilities") and oracle:
                # Project the pinned build onto this implementation's supported
                # generators (Ninja Multi-Config, Ninja). Leave the remaining bytes exact.
                stdout = re.sub(
                    rb'"generators":\[.*?\],"serverMode"',
                    b'"generators":[{"extraGenerators":[],"name":"Ninja Multi-Config",'
                    b'"platformSupport":false,"toolsetSupport":false},'
                    b'{"extraGenerators":[],"name":"Ninja",'
                    b'"platformSupport":false,"toolsetSupport":false}],"serverMode"',
                    stdout, count=1,
                )
            result[f"step {index} exit"] = proc.returncode
            result[f"step {index} stdout"] = stdout
            result[f"step {index} stderr"] = normalize(proc.stderr)
        for name in case.get("inspect", []):
            path = work / name
            if path.is_symlink():
                value = ("symlink", normalize(os.fsencode(path.readlink())))
            elif path.is_dir():
                value = ("directory", sorted(child.name for child in path.iterdir()))
            elif path.is_file():
                value = ("file", stat.S_IMODE(path.stat().st_mode), normalize(path.read_bytes()))
            else:
                value = ("absent",)
            result[f"path {name}"] = value
        return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--candidate", type=Path, default=ROOT / "target/release/cmake")
    parser.add_argument("--oracle", type=Path, default=ORACLE)
    parser.add_argument("-j", type=int, default=6)
    parser.add_argument("-v", action="store_true")
    parser.add_argument("paths", nargs="*", type=Path)
    args = parser.parse_args()
    cases = []
    for path in args.paths or sorted((ROOT / "tests/cli").glob("*.json")):
        cases.extend((f"{path.stem}/{case['name']}", case) for case in json.loads(path.read_text()))

    def compare(item):
        name, case = item
        return name, run(args.oracle.resolve(), case, oracle=True), run(
            args.candidate.resolve(), case, oracle=False
        )

    failures = 0
    with concurrent.futures.ThreadPoolExecutor(args.j) as pool:
        for name, expected, actual in pool.map(compare, cases):
            failed = expected != actual or any(value == "timeout" for value in actual.values())
            failures += failed
            print(f"{'FAIL' if failed else 'PASS'} {name}")
            if failed and args.v:
                for key in sorted(expected.keys() | actual.keys()):
                    before, after = expected.get(key), actual.get(key)
                    if before == after:
                        continue
                    print(f"  {key}")
                    if isinstance(before, bytes) and isinstance(after, bytes):
                        # repr of each line exposes CRLF and final-newline differences.
                        for line in difflib.unified_diff(
                            list(map(repr, before.splitlines(keepends=True))),
                            list(map(repr, after.splitlines(keepends=True))),
                            "oracle", "candidate", lineterm="",
                        ):
                            print(f"    {line}")
                    else:
                        print(f"    oracle={before!r}\n    candidate={after!r}")
    print(f"{len(cases) - failures}/{len(cases)} passed")
    return bool(failures)


if __name__ == "__main__":
    raise SystemExit(main())
