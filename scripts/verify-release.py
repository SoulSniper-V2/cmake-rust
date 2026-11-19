#!/usr/bin/env python3
"""Run upstream RunCMake harnesses with the pinned oracle and candidate on Linux."""

import argparse
from collections import Counter
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shutil
import subprocess
import tempfile


ROOT = Path(__file__).resolve().parents[1]
BASELINE = json.loads((ROOT / "compatibility/baseline.json").read_text())
PASS_LINE = re.compile(r"^-- (.+?) - PASSED\s*$", re.MULTILINE)
UNSUPPORTED = re.compile(r"unsupported|not (?:yet )?implemented|targets a subset of", re.IGNORECASE)
FAIL_LINE = re.compile(
    r"^CMake Error[^\n]*\n[ \t]+([^\n]+?) - FAILED:\s*\n(.*?)(?=^CMake Error|^-- .* - PASSED\s*$|\Z)",
    re.MULTILINE | re.DOTALL,
)
EXEC = re.compile(r'\bexecve\(("(?:[^"\\]|\\.)*"), .*\)\s* = 0$')
CLI_CASES = {
    "version": ["--version"],
    "help": ["--help"],
    "unknown-option": ["--invalid-option"],
    "bad-generator": ["-G", "NoSuchGenerator"],
    "missing-script": ["-P", "missing.cmake"],
    "missing-build": ["--build", "missing-build"],
    "missing-install": ["--install", "missing-build"],
    "empty-echo": ["-E", "echo_append"],
    "true": ["-E", "true"],
    "false": ["-E", "false"],
}


def checked(args, **kwargs):
    return subprocess.check_output(args, text=True, **kwargs).strip()


def sha256(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def candidate_revision():
    paths = [ROOT / item for item in ("Cargo.toml", "Cargo.lock")]
    paths.extend((ROOT / "src").rglob("*.rs"))
    digest = hashlib.sha256()
    for path in sorted(paths):
        digest.update(path.relative_to(ROOT).as_posix().encode() + b"\0")
        digest.update(bytes.fromhex(sha256(path)))
    return digest.hexdigest()


def pin(source):
    expected = BASELINE["oracle"]["commit"]
    actual = checked(["git", "-C", source, "rev-parse", "HEAD"])
    if actual != expected or checked(["git", "-C", source, "status", "--porcelain"]):
        raise SystemExit(f"oracle checkout must be clean at {expected}, found {actual}")
    return actual


def cache_value(build, key):
    cache = (build / "CMakeCache.txt").read_text()
    match = re.search(rf"^{re.escape(key)}:[^=]+=([^\n]*)$", cache, re.MULTILINE)
    return match[1] if match else None


def capture(args, cwd, env, trace):
    command = ["strace", "-ff", "--kill-on-exit", "-e", "trace=execve,execveat", "-s", "4096", "-o", trace, *args]
    try:
        proc = subprocess.run(command, cwd=cwd, env=env, capture_output=True, timeout=240)
        return proc.returncode, proc.stdout.decode(errors="replace"), proc.stderr.decode(errors="replace")
    except subprocess.TimeoutExpired as exc:
        return None, (exc.stdout or b"").decode(errors="replace"), (exc.stderr or b"").decode(errors="replace")


def audit(trace, cwd, candidate, oracle, harness):
    paths = []
    unknown = []
    for path in sorted(trace.parent.glob(trace.name + ".*")):
        for line in path.read_text().splitlines():
            match = EXEC.search(line)
            if match:
                raw = Path(json.loads(match[1]))
                paths.append((cwd / raw).resolve())
            elif (("execve(" in line or "execveat(" in line) and line.endswith(" = 0")) or "resumed>" in line:
                unknown.append(line)
    other_cmake = sorted({
        str(path) for path in paths
        if re.fullmatch(r"cmake(?:[._-][0-9].*)?", path.name)
        and path not in (oracle, candidate)
    })
    candidate_execs = paths.count(candidate)
    oracle_execs = paths.count(oracle)
    independent = (
        candidate_execs > 0 and oracle_execs == int(harness) and not unknown and not other_cmake
    )
    return {
        "trace_prefix": str(trace),
        "independent": independent,
        "candidate_execs": candidate_execs,
        "oracle_execs": oracle_execs,
        "other_cmake": other_cmake,
        "unparsed_successful_execs": unknown,
    }


def run_one(name, side, repetition, invocation, cwd, env, logs, candidate, oracle, harness):
    stem = logs / f"{name}.{side}.{repetition}"
    trace = stem.with_suffix(stem.suffix + ".exec")
    code, stdout, stderr = capture(invocation, cwd, env, trace)
    stem.with_suffix(stem.suffix + ".stdout").write_text(stdout)
    stem.with_suffix(stem.suffix + ".stderr").write_text(stderr)
    cases = {case: "pass" for case in PASS_LINE.findall(stdout)}
    for case, diagnostic in FAIL_LINE.findall(stderr):
        cases[case] = "unsupported" if UNSUPPORTED.search(diagnostic) else "mismatch"
    record = {"exit_code": code, "argv": [str(arg) for arg in invocation], "cases": cases}
    if side == "candidate":
        record["audit"] = audit(trace, cwd, candidate, oracle, harness)
    return record, (code, stdout, stderr)


def compare_suite(name, runs):
    expected = set().union(*(run["cases"] for run in runs["oracle"]))
    actual = set().union(*(run["cases"] for run in runs["candidate"]))
    independent = all(run["audit"]["independent"] for run in runs["candidate"])
    cases = {}
    for case in sorted(expected | actual):
        reference = [run["cases"].get(case, "missing") for run in runs["oracle"]]
        candidate = [run["cases"].get(case, "missing") for run in runs["candidate"]]
        if len(set(reference)) > 1 or len(set(candidate)) > 1:
            status = "observed_nondeterminism"
        elif reference[0] != "pass":
            status = "oracle_failure" if case in expected else "unexpected_candidate_case"
        elif not independent:
            status = "unverified_independence"
        elif candidate[0] == "missing":
            status = "not_reached"
        else:
            status = candidate[0]
        cases[case] = {"status": status, "oracle": reference, "candidate": candidate}
    counts = Counter(case["status"] for case in cases.values())
    suite_passed = bool(expected) and set(counts) == {"pass"} and all(
        run["exit_code"] == 0 for side in runs.values() for run in side
    )
    print(f"{name} ({len(expected)} oracle cases) {dict(counts)}", flush=True)
    return {
        "status": "pass" if suite_passed else "incomplete",
        "expected": len(expected), "counts": dict(counts), "cases": cases, "runs": runs,
    }


def run(args):
    if platform.system() != "Linux":
        raise SystemExit("this lane requires Linux and strace")
    source = args.kitware.resolve()
    oracle_build = args.oracle_build.resolve()
    oracle = (oracle_build / "bin/cmake").resolve(strict=True)
    candidate = args.candidate.resolve(strict=True)
    revision = pin(source)
    if candidate.samefile(oracle):
        raise SystemExit("candidate and oracle must be different binaries")
    if cache_value(oracle_build, "CMAKE_HOME_DIRECTORY") != str(source):
        raise SystemExit("oracle build cache does not point to the pinned source tree")
    if cache_value(oracle_build, "CMAKE_GENERATOR") != "Ninja":
        raise SystemExit("this lane requires a Ninja oracle build")
    banner = checked([oracle, "--version"]).splitlines()[0]
    if banner != f"cmake version {BASELINE['oracle']['version']}":
        raise SystemExit(f"unexpected oracle version: {banner}")

    out = args.out.resolve()
    out.mkdir(parents=True, exist_ok=False)
    work = out / "work"
    work.mkdir()
    logs = out / "logs"
    logs.mkdir()
    home = out / "home"
    home.mkdir()
    tools = args.tool_dir.resolve()
    env = {"PATH": str(tools), "HOME": str(home), "LC_ALL": "C", "LANG": "C", "CMAKE_BUILD_PARALLEL_LEVEL": "2"}
    path_cmake = shutil.which("cmake", path=env["PATH"])
    if path_cmake is None or not Path(path_cmake).samefile(candidate):
        raise SystemExit("controlled path must resolve cmake to the candidate")
    for program in ("strace", "ninja"):
        if not shutil.which(program, path=env["PATH"]):
            raise SystemExit(f"controlled path is missing {program}")
    reference_tools = out / "oracle-tools"
    reference_tools.mkdir()
    for tool in tools.iterdir():
        (reference_tools / tool.name).symlink_to(oracle if tool.name == "cmake" else tool.resolve())
    oracle_home = out / "oracle-home"
    oracle_home.mkdir()
    environments = {
        "oracle": {**env, "PATH": str(reference_tools), "HOME": str(oracle_home)},
        "candidate": env,
    }
    candidate_input = candidate
    candidate_tools = out / "candidate-tools"
    candidate_tools.mkdir()
    candidate = candidate_tools / "cmake"
    shutil.copy2(candidate_input, candidate)
    for tool in tools.iterdir():
        if tool.name != "cmake":
            (candidate_tools / tool.name).symlink_to(tool.resolve())
    env["PATH"] = str(candidate_tools)
    version = checked([candidate, "--version"], env=env).splitlines()[0]

    inventory = json.loads(checked([args.ctest, "--test-dir", oracle_build, "--show-only=json-v1"]))
    (out / "ctest-inventory.json").write_text(json.dumps(inventory, indent=2) + "\n")
    registration = {
        test["name"]: test for test in inventory["tests"] if test["name"].startswith("RunCMake.")
    }
    lane = BASELINE["linux_ninja_run_cmake"]
    selected = args.suite or lane["initial_suites"]
    missing = sorted(set(selected) - registration.keys())
    if missing:
        raise SystemExit(f"requested suites absent from the oracle registration: {missing}")
    suites = {
        name: {
            "status": "excluded" if name.startswith(tuple(lane["standalone_tool_suite_prefixes"])) else "unmeasured",
            "reason": "standalone CTest/CPack behavior" if name.startswith(tuple(lane["standalone_tool_suite_prefixes"])) else "outside this run",
        }
        for name in sorted(registration)
    }
    reports = {
        "platform": platform.platform(),
        "generator": "Ninja",
        "oracle": {"revision": revision, "binary": str(oracle), "sha256": sha256(oracle), "version": banner},
        "candidate": {"workspace_source_digest": candidate_revision(), "input_binary": str(candidate_input), "binary": str(candidate), "sha256": sha256(candidate), "version": version},
        "repetitions": args.repeat,
        "environments": environments,
        "harness_sha256": sha256(Path(__file__)),
        "baseline_sha256": sha256(ROOT / "compatibility/baseline.json"),
        "oracle_config_sha256": sha256(oracle_build / "CMakeCache.txt"),
        "suites": suites,
        "direct_cli": {},
    }
    for name in selected:
        case_filter = lane.get("case_filters", {}).get(name)
        original = registration[name].get("command", [])
        if not original or Path(original[0]).resolve() != oracle or "-P" not in original:
            suites[name] = {"status": "excluded", "reason": "registration does not use an injectable script driver", "command": original}
            continue
        runs = {"oracle": [], "candidate": []}
        for repetition in range(1, args.repeat + 1):
            for side, binary in (("oracle", oracle), ("candidate", candidate)):
                with tempfile.TemporaryDirectory(prefix=f"{name}.", dir=work) as temp:
                    directory = Path(temp)
                    options = [
                        arg for arg in original[1:]
                        if not arg.startswith(("-DRunCMake_BINARY_DIR=", "-DCMAKE_COMMAND="))
                    ]
                    invocation = [
                        oracle, f"-DCMAKE_COMMAND={binary}", f"-DRunCMake_BINARY_DIR={directory}",
                        *options,
                    ]
                    suite_env = environments[side].copy()
                    if case_filter:
                        suite_env["RunCMake_TEST_FILTER"] = case_filter
                    result, _ = run_one(
                        name, side, repetition, invocation, directory, suite_env, logs, candidate, oracle, True,
                    )
                    runs[side].append(result)
        suites[name] = compare_suite(name, runs)
        if case_filter:
            suites[name]["filter"] = case_filter
            suites[name]["unselected_cases"] = "unmeasured"
        (out / "report.partial.json").write_text(json.dumps(reports, indent=2) + "\n")

    for name, options in CLI_CASES.items():
        raw = {"oracle": [], "candidate": []}
        runs = {"oracle": [], "candidate": []}
        for repetition in range(1, args.repeat + 1):
            for side, binary in (("oracle", oracle), ("candidate", candidate)):
                with tempfile.TemporaryDirectory(prefix="cli.", dir=work) as temp:
                    directory = Path(temp)
                    result, observation = run_one(
                        f"CLI.{name}", side, repetition, [binary, *options], directory, environments[side],
                        logs, candidate, oracle, False,
                    )
                    # Normalize only each run's temporary directory and the executable path.
                    code, stdout, stderr = observation
                    raw[side].append((code, stdout.replace(str(directory), "<work>").replace(str(binary), "<cmake>"),
                                     stderr.replace(str(directory), "<work>").replace(str(binary), "<cmake>")))
                    runs[side].append(result)
        if any(len(set(side)) > 1 for side in raw.values()):
            status = "observed_nondeterminism"
        elif not all(run["audit"]["independent"] for run in runs["candidate"]):
            status = "unverified_independence"
        elif raw["candidate"][0] == raw["oracle"][0]:
            status = "pass"
        else:
            status = "mismatch"
        reports["direct_cli"][name] = {"status": status, "runs": runs}

    pin(source)
    reports["suite_counts"] = dict(Counter(suite["status"] for suite in suites.values()))
    reports["cli_counts"] = dict(Counter(case["status"] for case in reports["direct_cli"].values()))
    (out / "report.json").write_text(json.dumps(reports, indent=2) + "\n")
    (out / "report.partial.json").unlink(missing_ok=True)
    print(f"RunCMake suites {reports['suite_counts']}, direct CLI {reports['cli_counts']}")
    if not args.report_only:
        return int(any(suites[name]["status"] != "pass" for name in selected)
                   or reports["cli_counts"].get("pass", 0) != len(CLI_CASES))
    return 0


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--kitware", type=Path, required=True)
    parser.add_argument("--oracle-build", type=Path, required=True)
    parser.add_argument("--candidate", type=Path, default=ROOT / "target/release/cmake")
    parser.add_argument("--tool-dir", type=Path, required=True, help="controlled PATH with candidate cmake and native tools")
    parser.add_argument("--out", type=Path, required=True, help="new evidence directory")
    parser.add_argument("--ctest", type=Path, default=Path("/usr/bin/ctest"))
    parser.add_argument("--suite", action="append", help="run one registered suite (repeatable)")
    parser.add_argument("--repeat", type=int, default=2)
    parser.add_argument("--report-only", action="store_true", help="record failures without a failing harness exit code")
    arguments = parser.parse_args()
    if arguments.repeat < 2:
        parser.error("--repeat must be at least 2")
    raise SystemExit(run(arguments))
