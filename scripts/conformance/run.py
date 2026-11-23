#!/usr/bin/env python3
"""Measure a staged CMake against the pinned upstream RunCMake inventory."""

import argparse
import ast
from bisect import bisect_right
from collections import Counter
from concurrent.futures import ThreadPoolExecutor, as_completed
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shutil
import signal
import subprocess
import tempfile
import time


ROOT = Path(__file__).resolve().parents[2]
BASELINE_PATH = ROOT / "compatibility/baseline.json"
BASELINE = json.loads(BASELINE_PATH.read_text())
DEFAULT_BASE = Path("/tmp/cmake-rust-validation.hSLgEZHR")
SCHEMA_VERSION = 1
LOG_ENV = {"LANG": "C", "LC_ALL": "C", "TZ": "UTC", "CMAKE_BUILD_PARALLEL_LEVEL": "2"}
HOST_PATH = "/usr/bin:/bin:/usr/local/bin"
PASS = re.compile(r"^-- (.+) - PASSED$", re.MULTILINE)
STATUS_FAIL = re.compile(r"^-- (.+) - FAILED:?$", re.MULTILINE)
ERROR_FAIL = re.compile(
    r"^CMake Error[^\n]*\n((?:  [^\n]+\n)*?  .*?) -[ \t]*(?:\n  )?FAILED:$", re.MULTILINE
)
CMAKE_NAME = re.compile(r"cmake(?:[._-][0-9].*)?$", re.IGNORECASE)
EXEC_LINE = re.compile(r"^(\d+)\s+(\d+\.\d+)\s+(.*)$")
EXECVE = re.compile(r'^execve\(("(?:[^"\\]|\\.)*"), \[(.*?)\], ', re.ASCII)
QUOTED = re.compile(r'"(?:[^"\\]|\\.)*"')


def sha256(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def dump(path, data):
    path.write_text(json.dumps(data, indent=2, ensure_ascii=True) + "\n")


def checked(argv, **kwargs):
    return subprocess.check_output(argv, text=True, **kwargs).strip()


def version(path):
    return checked([path, "--version"], env={**LOG_ENV, "PATH": HOST_PATH}, timeout=15).splitlines()[0]


def pin(source):
    revision = checked(["git", "-C", source, "rev-parse", "HEAD"])
    if revision != BASELINE["oracle"]["commit"] or checked(["git", "-C", source, "status", "--porcelain"]):
        raise SystemExit(f"Expected clean Kitware checkout at {BASELINE['oracle']['commit']}: {source}")
    return revision


def capture(argv, cwd, env, timeout, logs):
    start = time.monotonic()
    with (logs / "stdout.txt").open("wb") as stdout, (logs / "stderr.txt").open("wb") as stderr:
        with subprocess.Popen(argv, cwd=cwd, env=env, stdout=stdout, stderr=stderr,
                              start_new_session=True) as process:
            try:
                code = process.wait(timeout=timeout)
            except subprocess.TimeoutExpired:
                try:
                    os.killpg(process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                process.wait()
                code = None
    return code, round(time.monotonic() - start, 3)


def audit(trace, argv, oracle, subject, cwd):
    """Identify successful execs, including strace's split/resumed records."""
    unfinished = {}
    other_cmake = set()
    unknown = []
    subject_execs = []
    driver_count = 0
    if not trace.exists():
        return {"valid": False, "reason": "missing strace output"}, subject_execs
    for line in trace.read_text(errors="replace").splitlines():
        match = EXEC_LINE.match(line)
        if not match:
            continue
        pid, timestamp, call = match.groups()
        timestamp = float(timestamp)
        if call.endswith(" <unfinished ...>"):
            unfinished[pid] = (timestamp, call.removesuffix(" <unfinished ...>"))
            continue
        if call.startswith("<... exec"):
            timestamp, prefix = unfinished.pop(pid, (timestamp, ""))
            call = prefix + call.partition(" resumed>")[2]
        if not call.endswith(" = 0"):
            continue
        match = EXECVE.match(call)
        if not match:
            unknown.append(line)
            continue
        executable = Path(ast.literal_eval(match[1]))
        resolved = (cwd / executable).resolve()
        command = [ast.literal_eval(token) for token in QUOTED.findall(match[2])]
        is_cmake = CMAKE_NAME.fullmatch(executable.name) or CMAKE_NAME.fullmatch(resolved.name)
        if resolved == oracle and command == argv:
            driver_count += 1
        elif resolved == subject and executable.is_absolute():
            subject_execs.append((timestamp, command))
        elif resolved == oracle or is_cmake:
            other_cmake.add(str(executable))
    valid = driver_count == 1 and not other_cmake and not unknown
    return {
        "valid": valid,
        "driver_execs": driver_count,
        "subject_execs": len(subject_execs),
        "other_cmake": sorted(other_cmake),
        "unparsed_successful_execs": unknown,
    }, sorted(subject_execs)


def foreign_generators(argv, supported):
    choices = set()
    for i, arg in enumerate(argv):
        if arg == "-G" and i + 1 < len(argv) and argv[i + 1] in supported:
            choices.add(argv[i + 1])
        elif arg.startswith("-G") and arg[2:] in supported:
            choices.add(arg[2:])
    return choices - {"Ninja"}


def read_cases(logs, driver, execs, generators, ctest):
    """Get upstream pass/fail events and each case's captured streams."""
    stdout = (logs / "stdout.txt").read_text(errors="replace")
    stderr = (logs / "stderr.txt").read_text(errors="replace")
    emitted = {
        "pass": Counter(" ".join(label.split()) for label in PASS.findall(stdout)),
        "fail": Counter(" ".join(label.split()) for label in STATUS_FAIL.findall(stdout) + ERROR_FAIL.findall(stderr)),
    }
    cases_dir = logs / "cases"
    cases_dir.mkdir()
    cases = {}
    seen = Counter()
    work_generators = {}
    start, workdir, streams, explicit, top_generator = 0, None, {}, set(), None
    pending_tool, case_tool = None, None
    timestamps = [item[0] for item in execs]
    trace = logs / "driver-trace.jsonl"
    if not trace.is_file():
        return cases, dict(emitted["pass"] + emitted["fail"])
    with trace.open(errors="replace") as lines:
        for line in lines:
            try:
                event = json.loads(line)
            except json.JSONDecodeError:
                continue
            cmd, args = event.get("cmd"), event.get("args", [])
            if (event.get("file") == str(driver) and event.get("global_frame") == 1
                    and cmd == "set" and args[:1] == ["RunCMake_GENERATOR"]):
                top_generator = args[1] if len(args) > 1 else None

            source_name = Path(event.get("file", "")).name
            if cmd == "run_cmake_command" and len(args) >= 2 and args[1] == str(ctest):
                if source_name == "RunCTest.cmake" or "--build-and-test" in args[2:]:
                    pending_tool = "Standalone CTest script or --build-and-test invocation"
            if source_name == "RunCMake.cmake":
                if cmd == "set" and args[:1] == ["top_src"]:
                    start, workdir, streams, explicit = event["time"], None, {}, set()
                    case_tool, pending_tool = pending_tool, None
                if cmd == "set" and args[:1] == ["ENV{PWD}"] and len(args) > 1 and workdir is None:
                    workdir = args[1]
                if cmd == "list" and args[:2] == ["APPEND", "RunCMake_TEST_COMMAND"]:
                    explicit.update(foreign_generators(args[2:], generators))
                if cmd == "string" and args[:2] == ["REGEX", "REPLACE"] and len(args) >= 6:
                    if args[4] in {"actual_stderr", "actual_stdout"}:
                        streams.setdefault(args[4], args[5])
            if cmd != "message" or len(args) < 2:
                continue
            kind, message = args[0], "".join(args[1:])
            if kind == "STATUS" and message.endswith(" - PASSED"):
                label, verdict, diagnostic = message.removesuffix(" - PASSED"), "pass", ""
            elif kind in {"STATUS", "SEND_ERROR"} and re.search(r" - FAILED(?::|$)", message):
                label, diagnostic = re.split(r" - FAILED:?", message, maxsplit=1)
                verdict = "fail"
            else:
                continue

            seen[label] += 1
            number = seen[label]
            key = json.dumps([label, number], ensure_ascii=True)
            index = f"{len(cases) + 1:04d}"
            record = {
                "name": label, "occurrence": number, "result": verdict,
                "log": f"cases/{index}.log", "standalone_tool": case_tool,
            }
            for _, command in execs[bisect_right(timestamps, start):bisect_right(timestamps, event["time"])]:
                explicit.update(foreign_generators(command, generators))
            if top_generator in generators - {"Ninja"}:
                explicit.add(top_generator)
            if explicit and workdir:
                work_generators[workdir] = explicit.copy()
            record["other_generators"] = sorted(explicit or work_generators.get(workdir, ()))
            printed_label = " ".join(label.split())
            if emitted[verdict][printed_label]:
                emitted[verdict][printed_label] -= 1
            else:
                record["result"] = "missing_result_line"

            case_stderr = streams.get("actual_stderr", "")
            case_stdout = streams.get("actual_stdout", "")
            record["diagnostic"] = first_line(case_stderr, case_stdout, diagnostic) if verdict == "fail" else ""
            (cases_dir / f"{index}.log").write_text(
                f"{label} - {verdict.upper()}\n\n{diagnostic}\n"
                f"--- captured stdout ---\n{case_stdout}\n--- captured stderr ---\n{case_stderr}\n",
                errors="replace",
            )
            cases[key] = record
    trace.unlink()
    return cases, dict(emitted["pass"] + emitted["fail"])


def first_line(stderr, stdout, report):
    lines = [line.strip() for line in stderr.splitlines() if line.strip()]
    if lines:
        if len(lines) > 1 and re.match(r"CMake (?:Error|Warning|Deprecation Warning) (?:at|in) .*:$", lines[0]):
            return lines[1]
        return lines[0]
    lines = [line.strip() for line in stdout.splitlines() if line.strip()]
    if lines and re.search(r"error|usage|unsupported|unknown|invalid|failed", lines[0], re.IGNORECASE):
        return lines[0]
    return next((line.strip() for line in report.splitlines() if line.strip()), "RunCMake assertion failed")


def normalize(line, paths):
    for path, token in paths:
        line = line.replace(str(path), token)
    line = re.sub(r"\x1b\[[0-9;]*m", "", line)
    line = re.sub(r"\b0x[0-9a-fA-F]+\b", "<address>", line)
    line = re.sub(r"\b\d+\.\d+(?=\s*(?:s|seconds)\b)", "<time>", line)
    line = re.sub(r"(?<=\.cmake):\d+", ":<line>", line)
    return " ".join(line.split())[:300]


def standalone(name):
    lane = BASELINE["linux_ninja_run_cmake"]
    exact = lane["standalone_tool_suites"].get(name)
    return exact or next((reason for prefix, reason in lane["standalone_tool_suite_prefixes"].items()
                          if name.startswith(prefix)), None)


def registered_paths(test, build):
    cwd = next(prop["value"] for prop in test["properties"] if prop["name"] == "WORKING_DIRECTORY")
    suite = next(arg.split("=", 1)[1] for arg in test["command"] if arg.startswith("-DRunCMake_BINARY_DIR="))
    return Path(cwd).relative_to(build), Path(suite).relative_to(build)


def invocation(test, context, binary, suite_build, logs):
    original = test["command"]
    injected = ("-DRunCMake_BINARY_DIR=", "-DCMAKE_COMMAND=", "-DCMAKE_CTEST_COMMAND=",
                "-DCMAKE_CPACK_COMMAND=")
    options = [arg for arg in original[1:] if not arg.startswith(injected)]
    driver = Path(options[-1])
    # Use the original driver and flags; only redirect its work and select the tool under test.
    trace_sources = (context["run_cmake"], context["run_ctest"], driver, *context["verdict_helpers"])
    return [
        str(context["oracle"]), f"-DCMAKE_COMMAND={binary}", f"-DRunCMake_BINARY_DIR={suite_build}",
        f"-DCMAKE_CTEST_COMMAND={context['ctest']}", f"-DCMAKE_CPACK_COMMAND={context['cpack']}",
        "--trace-expand", "--trace-format=json-v1", f"--trace-redirect={logs / 'driver-trace.jsonl'}",
        *(f"--trace-source={path}" for path in trace_sources), *options,
    ]


def run_once(test, context, side, repeat, directory):
    args = context["args"]
    binary = context[side]
    logs = directory / f"{side}-{repeat}"
    logs.mkdir(parents=True)
    with tempfile.TemporaryDirectory(prefix=f"{side}-", dir=context["work"]) as scratch:
        work = Path(scratch)
        for name in ("home", "tmp"):
            (work / name).mkdir()
        cwd_rel, build_rel = registered_paths(test, context["build"])
        cwd, suite_build = work / cwd_rel, work / build_rel
        cwd.mkdir(parents=True)
        suite_build.mkdir(parents=True, exist_ok=True)
        env = {
            **LOG_ENV, "PATH": f"{binary.parent}:{HOST_PATH}",
            "HOME": str(work / "home"), "TMPDIR": str(work / "tmp"),
        }
        argv = invocation(test, context, binary, suite_build, logs)
        trace = logs / "exec.log"
        traced_argv = [str(context["strace"]), "-f", "-ttt", "--kill-on-exit",
                       "-e", "trace=execve,execveat", "-s", "4096", "-o", str(trace), *argv]
        command = argv if args.no_audit else traced_argv
        code, duration = capture(command, cwd, env, args.timeout, logs)
        check, execs = (None, []) if args.no_audit else audit(trace, argv, context["oracle"], binary, cwd)
        cases, unmatched = read_cases(logs, Path(argv[-1]), execs, context["generators"], context["ctest"])
        paths = [(work, "<work>"), (binary, "<cmake>"), (context["build"], "<oracle-build>"),
                 (context["source"], "<kitware>")]
        for case in cases.values():
            case["diagnostic"] = normalize(case["diagnostic"], paths)
        result = {
            "exit_code": code, "seconds": duration, "argv": command, "cwd": str(cwd), "env": env,
            "audit": check, "unmatched_result_lines": unmatched, "cases": cases, "log": logs.name,
        }
        dump(logs / "run.json", result)
        return result


def oracle_once(test, context, repeat, directory):
    cache = context["cache"]
    if cache is None:
        return run_once(test, context, "oracle", repeat, directory)
    name = f"oracle-{repeat}"
    cached = cache / test["name"] / name
    if cached.is_dir():
        logs = directory / name
        shutil.copytree(cached, logs)
        return {**json.loads((logs / "run.json").read_text()), "cached": True}
    result = run_once(test, context, "oracle", repeat, directory)
    if (result["exit_code"] is not None and not result["unmatched_result_lines"]
            and (result["audit"] is None or result["audit"]["valid"])):
        cached.parent.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(dir=cached.parent) as temp:
            staged = Path(temp) / name
            shutil.copytree(directory / name, staged)
            try:
                staged.rename(cached)
            except OSError:
                if not cached.is_dir():
                    raise
    return result


def classify(candidate, audited):
    if not any(candidate):
        return "not_reached", "Case did not emit a result"
    outcomes = [case["result"] if case else "missing" for case in candidate]
    if len(set(outcomes)) > 1:
        return "candidate_nondeterminism", "Candidate case result changed between repetitions"
    if not audited:
        return "unverified_independence", "CMake execution independence could not be verified"
    if outcomes[0] == "pass":
        return "pass", ""
    reason = next((case["diagnostic"] for case in candidate if case and case["diagnostic"]), "")
    return "fail", reason or "Case did not emit a valid upstream verdict"


def run_suite(test, context, directory):
    runs = {"oracle": [], "candidate": []}
    for repeat in range(1, context["args"].repeat + 1):
        runs["oracle"].append(oracle_once(test, context, repeat, directory))
        runs["candidate"].append(run_once(test, context, "candidate", repeat, directory))
    audit_enabled = not context["args"].no_audit
    audited = not audit_enabled or all(
        run["audit"]["valid"] and run["audit"]["subject_execs"] > 0 for run in runs["candidate"]
    )
    oracle_valid = not audit_enabled or all(run["audit"]["valid"] for run in runs["oracle"])
    expected = set().union(*(run["cases"] for run in runs["oracle"]))
    actual = set().union(*(run["cases"] for run in runs["candidate"]))
    cases = {}
    for key in sorted(expected | actual):
        oracle = [run["cases"].get(key) for run in runs["oracle"]]
        candidate = [run["cases"].get(key) for run in runs["candidate"]]
        outcomes = [case["result"] if case else "missing" for case in oracle]
        other = sorted({gen for case in oracle if case for gen in case["other_generators"]}
                       - set(context["args"].score_generator or ()))
        tool_case = next((case["standalone_tool"] for case in oracle if case and case["standalone_tool"]), None)
        if key not in expected:
            status, reason = "unexpected_candidate_case", "No corresponding oracle result"
        elif tool_case:
            status, reason = "out_of_scope_standalone_tool", tool_case
        elif other:
            status, reason = "out_of_scope_generator", ", ".join(other)
        elif len(set(outcomes)) > 1:
            status, reason = "oracle_nondeterminism", "Oracle case result changed between repetitions"
        elif not oracle_valid or outcomes[0] != "pass":
            status, reason = "oracle_failure", "Oracle did not establish a passing result"
        else:
            status, reason = classify(candidate, audited)
        name, occurrence = json.loads(key)
        cases[key] = {
            "name": name, "occurrence": occurrence, "status": status, "reason": reason,
            "oracle": oracle, "candidate": candidate,
        }
    oracle_bad = any(run["exit_code"] != 0 or run["unmatched_result_lines"] for run in runs["oracle"])
    candidate_bad = any(run["exit_code"] != 0 or run["unmatched_result_lines"] for run in runs["candidate"])
    counts = Counter(case["status"] for case in cases.values())
    eligible = sum(counts[status] for status in SCORED)
    if expected and all(cases[key]["status"] == "out_of_scope_generator" for key in expected):
        status = "out_of_scope_generator"
    elif oracle_bad or not expected or not oracle_valid or counts["oracle_failure"] or counts["oracle_nondeterminism"]:
        status = "oracle_failure"
    elif eligible and counts["pass"] == eligible and not candidate_bad and audited and actual == expected:
        status = "pass"
    else:
        status = "incomplete"
    return {"status": status, "counts": dict(counts), "eligible": eligible, "cases": cases, "runs": runs}


SCORED = {"pass", "fail", "not_reached", "unverified_independence", "candidate_nondeterminism"}


def percentage(passed, total):
    return f"{100 * passed / total:.2f}%" if total else "—"


def cell(value):
    return str(value).replace("|", "\\|").replace("\n", " ")


def scoreboard(report):
    summary = report["summary"]
    total, passed = summary["total_cases"], summary["passed"]
    lines = [
        f"# RunCMake conformance · {report['kind']}", "",
        f"**{passed:,} / {total:,} cases passed ({percentage(passed, total)})**",
        "",
        f"{summary['registered_suites']} registered suites · {summary['selected_suites']} selected · "
        f"{report['repetitions']} repetition(s) · {'strace audit' if report['audit'] else 'audit disabled'}",
        "",
        "## Case counts", "",
        "| Result | Cases |", "| --- | ---: |",
    ]
    lines.extend(f"| {name} | {count:,} |" for name, count in sorted(summary["case_counts"].items()))
    lines += ["", "## Top failure reasons", "", "| Diagnostic | Cases |", "| --- | ---: |"]
    lines.extend(f"| {cell(reason)} | {count:,} |" for reason, count in list(summary["failure_reasons"].items())[:25])
    lines += ["", "## All registered suites", "",
              "| Suite | Status | Scored | Passed | Pass rate | Oracle failures | Other generator | Standalone cases | Reason |",
              "| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | --- |"]
    for name, suite in report["suites"].items():
        counts = suite.get("counts", {})
        eligible, passes = suite.get("eligible", 0), counts.get("pass", 0)
        status = suite["status"]
        reason = suite.get("reason", "")
        if status == "out_of_scope_standalone_tool":
            status = "out-of-scope: standalone tool"
        elif status == "oracle_failure":
            oracle = suite["runs"]["oracle"]
            if not suite["cases"]:
                reason = "No upstream case verdicts"
            if any(run["exit_code"] is None for run in oracle):
                reason = "Oracle suite timeout"
            elif any(run["exit_code"] != 0 for run in oracle):
                reason = "Oracle driver returned a nonzero exit code"
        oracle_fails = counts.get("oracle_failure", 0) + counts.get("oracle_nondeterminism", 0)
        lines.append(
            f"| {name} | {status} | {eligible} | {passes} | {percentage(passes, eligible)} | "
            f"{oracle_fails} | {counts.get('out_of_scope_generator', 0)} | "
            f"{counts.get('out_of_scope_standalone_tool', 0)} | {cell(reason)} |"
        )
    return "\n".join(lines) + "\n"


def summarize(suites):
    counts = Counter()
    histogram = Counter()
    for suite in suites.values():
        counts.update(suite.get("counts", {}))
        histogram.update(case["reason"] for case in suite.get("cases", {}).values()
                         if case["status"] in SCORED - {"pass"})
    total = sum(counts[name] for name in SCORED)
    return {
        "registered_suites": len(suites),
        "selected_suites": sum("runs" in suite for suite in suites.values()),
        "suite_counts": dict(Counter(suite["status"] for suite in suites.values())),
        "case_counts": dict(counts),
        "total_cases": total,
        "passed": counts["pass"],
        "pass_percentage": 100 * counts["pass"] / total if total else None,
        "failure_reasons": dict(sorted(histogram.items(), key=lambda item: (-item[1], item[0]))),
    }


def compare(old_path, new_path):
    def results(path):
        report = json.loads(path.read_text())
        return {(suite, case["name"], case["occurrence"]): case["status"]
                for suite, entry in report["suites"].items() for case in entry.get("cases", {}).values()
                if case["status"] in SCORED}

    old, new = results(old_path), results(new_path)
    common = old.keys() & new.keys()
    for heading, passed in (("Regressions", old), ("Newly passing", new)):
        changed = sorted(key for key in common if old[key] != new[key] and passed[key] == "pass")
        print(f"{heading} ({len(changed)})")
        for suite, case, occurrence in changed:
            print(f"  {suite} / {case} [{occurrence}]: {old[suite, case, occurrence]} → "
                  f"{new[suite, case, occurrence]}")
    print(f"Coverage changes: {len(new.keys() - old.keys())} added, {len(old.keys() - new.keys())} removed")


def tool(explicit, sibling, name):
    path = explicit or (sibling if sibling.is_file() else shutil.which(name, path=HOST_PATH))
    if path is None:
        raise SystemExit(f"Could not find {name}; supply --{name} explicitly")
    return Path(path).resolve(strict=True)


def prepare(args):
    if (platform.system(), platform.machine()) != ("Linux", "x86_64"):
        raise SystemExit("This lane requires Linux x86_64")
    source, build = args.kitware.resolve(), args.oracle_build.resolve()
    oracle = (build / "bin/cmake").resolve(strict=True)
    candidate = args.candidate.absolute()
    if candidate.name != "cmake" or candidate.parent.name != "bin" or candidate.is_symlink():
        raise SystemExit("Candidate must be the staged executable at <prefix>/bin/cmake")
    candidate = candidate.resolve(strict=True)
    revision = pin(source)
    share = candidate.parents[1] / f"share/cmake-{BASELINE['oracle']['version'].rsplit('.', 1)[0]}"
    for item in ("Modules", "Templates"):
        if not (share / item).is_symlink() or (share / item).resolve() != source / item:
            raise SystemExit(f"Expected {share / item} to link to {source / item}")
    oracle_digest, candidate_digest = sha256(oracle), sha256(candidate)
    if candidate.samefile(oracle) or (oracle_digest == candidate_digest) != args.self_test:
        raise SystemExit("Use a separately staged identical oracle binary only with --self-test")
    cache_text = (build / "CMakeCache.txt").read_text()
    for line in ("CMAKE_GENERATOR:INTERNAL=Ninja", "BUILD_TESTING:BOOL=ON",
                 f"CMAKE_HOME_DIRECTORY:INTERNAL={source}"):
        if line not in cache_text.splitlines():
            raise SystemExit(f"Oracle build cache is missing {line}")
    oracle_version = version(oracle)
    if oracle_version != f"cmake version {BASELINE['oracle']['version']}":
        raise SystemExit(f"Unexpected oracle version: {oracle_version}")
    ctest = tool(args.ctest, build / "bin/ctest", "ctest")
    cpack = tool(args.cpack, build / "bin/cpack", "cpack")
    strace = None if args.no_audit else tool(None, Path("/usr/bin/strace"), "strace")
    ctest_argv = [str(ctest), "--test-dir", str(build), "--show-only=json-v1"]
    inventory_env = {**LOG_ENV, "PATH": HOST_PATH}
    inventory = json.loads(checked(ctest_argv, env=inventory_env, timeout=30))
    entries = [test for test in inventory["tests"] if test["name"].startswith("RunCMake.")]
    tests = {test["name"]: test for test in entries}
    if not tests or len(entries) != len(tests):
        raise SystemExit("Expected a nonempty RunCMake registration with unique suite names")
    for test in tests.values():
        command = test.get("command", [])
        if not command or Path(command[0]).resolve() != oracle or command[-2] != "-P":
            raise SystemExit(f"Unexpected RunCMake registration: {test['name']} {command}")
    missing = set(args.suite or []) - tests.keys()
    if missing:
        raise SystemExit(f"Unregistered suites: {', '.join(sorted(missing))}")
    generators = json.loads(checked([oracle, "-E", "capabilities"], env=inventory_env, timeout=15))
    generator_names = {item["name"] for item in generators["generators"]}
    generator_names.update(f"{extra} - {item['name']}" for item in generators["generators"]
                           for extra in item["extraGenerators"])
    host = {"platform": platform.platform(), "machine": platform.machine(), "node": platform.node(),
            "python": platform.python_version(), "uid": os.getuid()}
    tools = {}
    for path in filter(None, (ctest, cpack, strace, Path("/usr/bin/ninja"), Path("/usr/bin/cc"),
                              Path("/usr/bin/c++"), Path("/usr/bin/make"))):
        tools[path.name] = {"path": str(path), "sha256": sha256(path), "version": version(path)}
    # A configured tree may register helper binaries that have yet to be built.
    # Incorporate their availability and bytes in the cache context.
    helpers = {}
    for test in tests.values():
        for option in test["command"][1:-2]:
            _, _, value = option.partition("=")
            path = Path(value)
            if path.is_relative_to(build):
                helpers[value] = sha256(path) if path.is_file() else "directory" if path.is_dir() else "missing"
    oracle_info = {"path": str(oracle), "sha256": oracle_digest, "version": oracle_version,
                   "commit": revision, "source": str(source), "build": str(build),
                   "config_sha256": sha256(build / "CMakeCache.txt")}
    cache_context = {
        "schema_version": SCHEMA_VERSION, "harness_sha256": sha256(Path(__file__)),
        "baseline_sha256": sha256(BASELINE_PATH), "host": host, "tools": tools, "helpers": helpers,
        "oracle": oracle_info, "environment": inventory_env, "timeout": args.timeout,
        "inventory_sha256": hashlib.sha256(json.dumps(inventory, sort_keys=True).encode()).hexdigest(),
    }
    key = hashlib.sha256(json.dumps(cache_context, sort_keys=True).encode()).hexdigest()
    out = args.out.resolve()
    out.mkdir(parents=True, exist_ok=False)
    work = out / "work"
    work.mkdir()
    dump(out / "ctest-inventory.json", inventory)
    report = {
        **cache_context,
        "kind": "harness self-test" if args.self_test else "official" if args.official else "development",
        "generator": "Ninja", "scored_generators": ["Ninja", *sorted(set(args.score_generator or ()))],
        "repetitions": args.repeat, "jobs": args.jobs, "audit": not args.no_audit,
        "workspace_head": checked(["git", "-C", ROOT, "rev-parse", "HEAD"]),
        "candidate": {"path": str(candidate), "sha256": candidate_digest, "version": version(candidate)},
        "inventory_argv": ctest_argv, "oracle_cache_key": key if args.oracle_cache else None,
        "suites": {},
    }
    run_cmake_dir = source / "Tests/RunCMake"
    # These upstream includes emit named verdicts alongside the shared RunCMake helper.
    verdict_helpers = (
        run_cmake_dir / "Autogen_common/utils.cmake",
        run_cmake_dir / "install/TARGETS-FILE_RPATH_CHANGE-check-common.cmake",
    )
    context = {
        "args": args, "source": source, "build": build, "work": work,
        "oracle": oracle, "candidate": candidate, "ctest": ctest, "cpack": cpack, "strace": strace,
        "cache": args.oracle_cache.resolve() / key if args.oracle_cache else None,
        "generators": generator_names, "run_cmake": run_cmake_dir / "RunCMake.cmake",
        "run_ctest": run_cmake_dir / "RunCTest.cmake",
        "verdict_helpers": verdict_helpers,
    }
    return out, report, tests, context


def run(args):
    out, report, tests, context = prepare(args)
    suites = report["suites"]
    selected = {}
    for name, test in sorted(tests.items()):
        reason = standalone(name)
        if reason:
            suites[name] = {"status": "out_of_scope_standalone_tool", "reason": reason}
        elif args.suite and name not in args.suite:
            suites[name] = {"status": "unselected", "reason": "Outside the requested suite subset"}
        else:
            suites[name] = {}
            selected[name] = test
    if not selected:
        raise SystemExit("No in-scope suites selected")
    print(f"{report['kind']}: {len(selected)} of {len(tests)} registered suites, "
          f"{args.repeat} repetition(s), {args.jobs} workers", flush=True)
    with ThreadPoolExecutor(max_workers=args.jobs) as pool:
        pending = {pool.submit(run_suite, test, context, out / "logs" / name): name
                   for name, test in selected.items()}
        for i, future in enumerate(as_completed(pending), 1):
            name = pending[future]
            suites[name] = future.result()
            if i % 20 == 0 or i == len(selected):
                print(f"{i}/{len(selected)} suites completed", flush=True)
    pin(context["source"])
    for side in ("oracle", "candidate"):
        if sha256(context[side]) != report[side]["sha256"]:
            raise SystemExit(f"{side} executable changed during the run")
    report["summary"] = summarize(suites)
    dump(out / "report.json", report)
    (out / "scoreboard.md").write_text(scoreboard(report))
    context["work"].rmdir()
    print(f"{report['summary']['passed']}/{report['summary']['total_cases']} cases passed. "
          f"Evidence: {out}", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--compare", nargs=2, metavar=("OLD_JSON", "NEW_JSON"), type=Path)
    parser.add_argument("--kitware", type=Path, default=DEFAULT_BASE / "kitware")
    parser.add_argument("--oracle-build", type=Path, default=DEFAULT_BASE / "kitware-reference")
    parser.add_argument("--ctest", type=Path, help="independent Kitware CTest executable")
    parser.add_argument("--cpack", type=Path, help="independent Kitware CPack executable")
    parser.add_argument("--candidate", type=Path, help="staged executable at <prefix>/bin/cmake")
    parser.add_argument("--out", type=Path, help="new evidence directory")
    parser.add_argument("--suite", action="append", help="exact registered suite name (repeatable)")
    parser.add_argument("-j", "--jobs", type=int, default=6)
    parser.add_argument("--timeout", type=int, default=240, help="seconds per suite, per side and repetition")
    parser.add_argument("--repeat", type=int, default=1)
    parser.add_argument("--no-audit", action="store_true")
    parser.add_argument("--score-generator", action="append", metavar="NAME",
                        help="also score cases that select this non-default generator (repeatable), "
                             "e.g. 'Ninja Multi-Config'")
    parser.add_argument("--oracle-cache", type=Path, help="reusable directory for pinned oracle evidence")
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--self-test", action="store_true", help="sanity run with a separately staged oracle")
    mode.add_argument("--official", action="store_true", help="require full inventory, audit and 2+ repeats")
    args = parser.parse_args()
    if args.compare:
        compare(*args.compare)
        return
    if args.candidate is None or args.out is None:
        parser.error("--candidate and --out are required to run suites")
    if min(args.timeout, args.jobs, args.repeat) < 1:
        parser.error("--timeout, --jobs and --repeat must be positive")
    if args.official and (args.suite or args.no_audit or args.repeat < 2):
        parser.error("--official requires all suites, the audit and --repeat >= 2")
    run(args)


if __name__ == "__main__":
    main()
