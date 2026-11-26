#!/usr/bin/env python3
"""Differential fuzzing of configure-time probes: try_compile, try_run and
the Check* modules.

Each case is a C/C++ project that runs a random sequence of probes
(check_c_source_compiles/runs, check_cxx_source_compiles,
check_include_file(s), check_symbol_exists, check_function_exists,
check_type_size, check_struct_has_member, check_compiler_flag,
check_linker_flag, check_library_exists, CheckPIESupported,
test_big_endian, try_compile with SOURCES/SOURCE_FROM_CONTENT and options,
try_run) with random code, flags, definitions, include directories and
libraries (some valid, some broken), and prints every result variable.
Configure output (with --debug-trycompile off), exit code, the probe cache
entries and the normalized CMakeConfigureLog.yaml must match.

Usage: fuzz_probe.py --candidate <staged bin/cmake> [--seed N] [-n COUNT] [-j N]
"""
import argparse
import concurrent.futures
import json
import os
import random
import re
import shutil
import subprocess
import tempfile
from pathlib import Path

ORACLE = "/tmp/cmake-rust-validation.hSLgEZHR/kitware-reference/bin/cmake"
SNIPPETS_C = [
    "int main(void) { return 0; }",
    "#include <stdio.h>\nint main(void) { printf(\"x\"); return 0; }",
    "int main(void) { return undefined_symbol(); }",
    "#include <math.h>\nint main(void) { return (int)sqrt(4.0) - 2; }",
    "int main(void) { syntax error }",
    "#include <stdint.h>\nint main(void) { int64_t x = 1; return (int)(x - 1); }",
    "int main(int argc, char** argv) { (void)argv; return argc - 1; }",
    "#include <pthread.h>\nint main(void) { pthread_t t; (void)t; return 0; }",
]
SNIPPETS_CXX = [
    "int main() { return 0; }",
    "#include <vector>\nint main() { std::vector<int> v{1}; return v[0] - 1; }",
    "#include <optional>\nint main() { std::optional<int> o; return o.has_value(); }",
    "int main() { auto x = [](){ return 0; }; return x(); }",
    "int main() { constexpr int x = 1; static_assert(x == 2, \"no\"); }",
]
FLAGS = ["-Wall", "-O2", "-fPIC", "-std=c99", "-std=c11", "-fno-such-flag", "-DFOO=1", "-Werror", "-march=native", "-pthread"]
LINK_FLAGS = ["-Wl,--as-needed", "-Wl,--no-such-option", "-pthread", "-lm", "-Wl,-z,defs"]


def q(text):
    return '"' + text.replace("\\", "\\\\").replace('"', '\\"').replace("$", "\\$").replace(";", "\\;") + '"'


def gen_probe(rng, i):
    v = f"HAVE_{i}"
    c = rng.randrange(16)
    if rng.random() < 0.3:
        pre = rng.choice([
            f"set(CMAKE_REQUIRED_FLAGS {q(rng.choice(FLAGS))})",
            f"set(CMAKE_REQUIRED_DEFINITIONS -DBAR={rng.randint(0, 3)})",
            "set(CMAKE_REQUIRED_INCLUDES /usr/include /opt/none)",
            "set(CMAKE_REQUIRED_LIBRARIES m)",
            f"set(CMAKE_REQUIRED_LINK_OPTIONS {q(rng.choice(LINK_FLAGS))})",
            "set(CMAKE_REQUIRED_QUIET ON)",
            "unset(CMAKE_REQUIRED_FLAGS)\nunset(CMAKE_REQUIRED_LIBRARIES)",
        ]) + "\n"
    else:
        pre = ""
    if c == 0:
        body = f"check_c_source_compiles({q(rng.choice(SNIPPETS_C))} {v})"
    elif c == 1:
        body = f"check_c_source_runs({q(rng.choice(SNIPPETS_C))} {v})"
    elif c == 2:
        body = f"check_cxx_source_compiles({q(rng.choice(SNIPPETS_CXX))} {v})"
    elif c == 3:
        body = f"check_include_file({rng.choice(['stdio.h', 'pthread.h', 'nope.h', 'sys/types.h'])} {v})"
    elif c == 4:
        body = f"check_include_files({q(rng.choice(['stdio.h;stdlib.h', 'nope.h;stdio.h', 'sys/types.h;sys/stat.h']))} {v})"
    elif c == 5:
        body = f"check_symbol_exists({rng.choice(['printf', 'snprintf', 'no_such_fn', 'EOF'])} {q(rng.choice(['stdio.h', 'stdlib.h']))} {v})"
    elif c == 6:
        body = f"check_function_exists({rng.choice(['printf', 'pow', 'no_such_fn', 'pthread_create'])} {v})"
    elif c == 7:
        t = rng.choice(["int", "long", "void*", "size_t", "struct nope", "long long", "__int128"])
        body = f"check_type_size({q(t)} {v}{rng.choice(['', ' BUILTIN_TYPES_ONLY', ' LANGUAGE C'])})\nmessage(STATUS \"code={v}: [${{{v}_CODE}}]\" )"
    elif c == 8:
        body = f"check_struct_has_member({q(rng.choice(['struct stat', 'struct tm', 'struct nope']))} {rng.choice(['st_size', 'tm_year', 'x'])} {q('sys/stat.h;time.h')} {v})"
    elif c == 9:
        body = f"check_compiler_flag({rng.choice(['C', 'CXX'])} {q(rng.choice(FLAGS))} {v})"
    elif c == 10:
        body = f"check_linker_flag({rng.choice(['C', 'CXX'])} {q(rng.choice(LINK_FLAGS))} {v})"
    elif c == 11:
        body = f"check_library_exists({rng.choice(['m', 'pthread', 'nope'])} {rng.choice(['sqrt', 'pthread_create', 'x'])} \"\" {v})"
    elif c == 12:
        body = f"test_big_endian({v})"
    elif c == 13:
        lang = rng.choice(["c", "cpp"])
        code = rng.choice(SNIPPETS_C if lang == "c" else SNIPPETS_CXX)
        opts = rng.sample([f"COMPILE_DEFINITIONS -DX={i}", f"CMAKE_FLAGS -DINCLUDE_DIRECTORIES=/opt/x",
                           "LINK_LIBRARIES m", f"OUTPUT_VARIABLE out{i}", f"C_STANDARD 11", "NO_CACHE",
                           f"LOG_DESCRIPTION \"probe {i}\""], rng.randint(0, 3))
        if lang == "cpp":
            opts = [o.replace("C_STANDARD", "CXX_STANDARD") for o in opts]
        body = f"try_compile({v} SOURCE_FROM_CONTENT t{i}.{lang} {q(code)} {' '.join(opts)})"
    elif c == 14:
        code = rng.choice(SNIPPETS_C)
        body = (f"try_run(RUN_{i} {v} SOURCE_FROM_CONTENT r{i}.c {q(code)} RUN_OUTPUT_VARIABLE rout{i}"
                f"{rng.choice(['', ' ARGS a b', ' WORKING_DIRECTORY ${CMAKE_BINARY_DIR}'])})\n"
                f"message(STATUS \"run{i}=[${{RUN_{i}}}] [${{rout{i}}}]\")")
    else:
        body = "include(CheckPIESupported)\ncheck_pie_supported(OUTPUT_VARIABLE pie LANGUAGES C)\nmessage(STATUS \"pie=[${CMAKE_C_LINK_PIE_SUPPORTED}]\")"
    return pre + body + f"\nmessage(STATUS \"{v}=[${{{v}}}]\")"


def make_case(seed, index):
    rng = random.Random(f"{seed}:{index}")
    lines = ["cmake_minimum_required(VERSION 3.25)", "project(p C CXX)"]
    for mod in ["CheckCSourceCompiles", "CheckCSourceRuns", "CheckCXXSourceCompiles", "CheckIncludeFile",
                "CheckIncludeFiles", "CheckSymbolExists", "CheckFunctionExists", "CheckTypeSize",
                "CheckStructHasMember", "CheckCompilerFlag", "CheckLinkerFlag", "CheckLibraryExists", "TestBigEndian"]:
        lines.append(f"include({mod})")
    for i in range(rng.randint(1, 5)):
        lines.append(gen_probe(rng, i))
    return "\n".join(lines) + "\n"


def normalize_log(data):
    data = re.sub(rb"cmTC_[0-9a-f]{5}", b"cmTC_<ID>", data)
    data = re.sub(rb"TryCompile-[A-Za-z0-9]{6}", b"TryCompile-<ID>", data)
    data = re.sub(rb"/tmp/cc[A-Za-z0-9]{6}\.", b"/tmp/cc<TMP>.", data)
    data = re.sub(rb"(?m)^(\s*)(time|timestamp|duration|elapsed): [^\n]*$", rb"\1\2: <TIME>", data)
    data = re.sub(rb"\[\d+/\d+\]", b"[N/N]", data)  # ninja progress ordering of parallel steps
    return data


def run_side(cmake, src, build):
    if build.exists():
        shutil.rmtree(build)
    env = {k: v for k, v in os.environ.items() if k != "MAKEFLAGS"}
    proc = subprocess.run([cmake, "-S", str(src), "-B", str(build), "-G", "Ninja"],
                          capture_output=True, env=env, timeout=300)
    prefix = str(Path(cmake).resolve().parent.parent).encode()
    data_root = os.path.realpath("/tmp/cmake-rust-validation.hSLgEZHR/kitware").encode()

    def norm(data):
        data = data.replace(str(build).encode(), b"<B>").replace(str(src).encode(), b"<S>")
        data = data.replace(data_root, b"<DATA>").replace(prefix + b"/share/cmake-4.1", b"<DATA>")
        data = data.replace(prefix, b"<CMAKE_PREFIX>").replace(str(Path(cmake)).encode(), b"<CMAKE>")
        return normalize_log(re.sub(rb"\(\d+\.\d+s\)", b"(<T>s)", data))
    cache = (build / "CMakeCache.txt").read_bytes() if (build / "CMakeCache.txt").exists() else b""
    cache = b"\n".join(l for l in norm(cache).splitlines() if l.startswith((b"HAVE_", b"RUN_", b"CMAKE_C_LINK_PIE")))
    log = build / "CMakeFiles/CMakeConfigureLog.yaml"
    log_data = norm(log.read_bytes()) if log.exists() else b""
    # Only the probes this project ran (the compiler identification steps
    # before them are covered elsewhere and include ABI binaries).
    marker = log_data.find(b"\n  -\n    kind: \"try_compile-v1\"")
    return proc.returncode, norm(proc.stdout + proc.stderr), cache, log_data[marker:] if marker >= 0 else log_data


def check(args, index):
    text = make_case(args.seed, index)
    root = Path(tempfile.mkdtemp(prefix="fpr-", dir=args.work))
    try:
        src = root / "src"
        src.mkdir()
        (src / "CMakeLists.txt").write_text(text)
        results = [run_side(cmake, src, root / "b") for cmake in (args.oracle, args.candidate)]
        if results[0] == results[1]:
            return None
        keep = Path(args.out) / f"case-{args.seed}-{index}"
        keep.mkdir(parents=True, exist_ok=True)
        (keep / "CMakeLists.txt").write_text(text)
        for name, r in zip(("oracle", "candidate"), results):
            (keep / f"{name}.out").write_bytes(r[1])
            (keep / f"{name}.cache").write_bytes(r[2])
            (keep / f"{name}.log").write_bytes(r[3])
            (keep / f"{name}.rc").write_text(str(r[0]))
        return str(keep)
    finally:
        shutil.rmtree(root, ignore_errors=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--candidate", required=True)
    parser.add_argument("--oracle", default=ORACLE)
    parser.add_argument("--seed", type=int, default=1)
    parser.add_argument("-n", type=int, default=100)
    parser.add_argument("-j", type=int, default=4)
    parser.add_argument("--out", default="fuzz-probe-out")
    args = parser.parse_args()
    Path(args.out).mkdir(parents=True, exist_ok=True)
    args.work = tempfile.mkdtemp(prefix="fuzz-probe-")
    with concurrent.futures.ThreadPoolExecutor(args.j) as pool:
        fails = [f for f in pool.map(lambda i: check(args, i), range(args.n)) if f]
    shutil.rmtree(args.work, ignore_errors=True)
    print(json.dumps({"seed": args.seed, "cases": args.n, "failures": len(fails),
                      "examples": fails[:10]}, indent=1))
    return 1 if fails else 0


if __name__ == "__main__":
    raise SystemExit(main())
