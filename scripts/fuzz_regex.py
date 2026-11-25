#!/usr/bin/env python3
"""Differential fuzzing of CMake's regular-expression engine (kwsys dialect).

Each case is a `cmake -P` script that runs one random pattern against
several random inputs through string(REGEX MATCH), string(REGEX MATCHALL),
string(REGEX REPLACE), if(MATCHES) with CMAKE_MATCH_<n>, and list(FILTER
... REGEX).  Oracle and candidate stdout, stderr and exit code must match
byte for byte.  Deterministic per --seed and case index.

Usage: fuzz_regex.py --candidate <staged bin/cmake> [--seed N] [-n COUNT] [-j N]
"""
import argparse
import concurrent.futures
import json
import random
import shutil
import subprocess
import tempfile
from pathlib import Path

ORACLE = "/tmp/cmake-rust-validation.hSLgEZHR/kitware-reference/bin/cmake"
ATOMS = list("abcxyz019") + [".", "\\.", "\\\\", "\\(", "\\)", "\\[", "\\]",
                            "\\*", "\\+", "\\?", "\\|", "\\$", "\\^", "\\n", "\\t",
                            " ", "-", "_", "/", ";", "\xc3\xa9", "\xce\xb1"]
CLASSES = ["[a-c]", "[^a-c]", "[0-9]+", "[]a]", "[^]a]", "[-a]", "[a-]", "[.]",
           "[\\\\]", "[ \t]", "[^;]", "[A-Za-z_]", "[a-a]", "[z-a]", "[]]", "[^-]"]
INPUT_ATOMS = list("abcxyzABC0129 ._-/\\()[]*+?|$^\t") + [";", "\\;", "\xc3\xa9", "\xce\xb1", "\n"]


def gen_pattern(rng, depth=0):
    parts = []
    for _ in range(rng.randint(1, 4)):
        r = rng.random()
        if r < 0.45:
            item = rng.choice(ATOMS)
        elif r < 0.65:
            item = rng.choice(CLASSES)
        elif r < 0.85 and depth < 3:
            item = "(" + gen_pattern(rng, depth + 1) + ")"
        else:
            item = gen_pattern(rng, depth + 1) + "|" + gen_pattern(rng, depth + 1) if depth < 2 else "a"
        if rng.random() < 0.35:
            item += rng.choice(["*", "+", "?"])
        parts.append(item)
    pattern = "".join(parts)
    if depth == 0:
        if rng.random() < 0.2:
            pattern = "^" + pattern
        if rng.random() < 0.2:
            pattern += "$"
        if rng.random() < 0.03:
            pattern = rng.choice(["", "(", ")", "*a", "a**", "[a", "a|*", "\\"]) + pattern
    return pattern


def gen_input(rng):
    return "".join(rng.choice(INPUT_ATOMS) for _ in range(rng.randint(0, 14)))


def bracket(text):
    """Bracket argument that cannot be closed by its content."""
    eq = "="
    while f"]{eq}]" in text:
        eq += "="
    return f"[{eq}[{text}]{eq}]"


def make_case(seed, index):
    rng = random.Random(f"{seed}:{index}")
    pattern = gen_pattern(rng)
    replacement = rng.choice(["", "<\\0>", "\\1", "[\\1|\\2]", "x\\\\y", "\\9", "$", "\\"])
    lines = [f"set(p {bracket(pattern)})"]
    for k in range(rng.randint(1, 4)):
        value = gen_input(rng)
        lines += [
            f"set(s {bracket(value)})",
            'string(REGEX MATCH "${p}" m "${s}")',
            f'message("{k} match=[${{m}}]")',
            'string(REGEX MATCHALL "${p}" ma "${s}")',
            f'message("{k} all=[${{ma}}]")',
            f'string(REGEX REPLACE "${{p}}" {bracket(replacement)} r "${{s}}")',
            f'message("{k} rep=[${{r}}]")',
            'if("${s}" MATCHES "${p}")',
            f'  message("{k} if=[${{CMAKE_MATCH_COUNT}}|${{CMAKE_MATCH_0}}|${{CMAKE_MATCH_1}}|${{CMAKE_MATCH_2}}]")',
            "endif()",
            "set(l a ${s} b)",
            'list(FILTER l INCLUDE REGEX "${p}")',
            f'message("{k} filter=[${{l}}]")',
        ]
    return "\n".join(lines) + "\n"


def run(cmake, script, cwd):
    proc = subprocess.run([cmake, "-P", script], cwd=cwd, capture_output=True, timeout=60)
    return proc.returncode, proc.stdout, proc.stderr


def check(args, index):
    work = Path(tempfile.mkdtemp(prefix="rx-", dir=args.work))
    try:
        script = work / "case.cmake"
        script.write_bytes(make_case(args.seed, index).encode("utf-8", "surrogateescape"))
        oracle = run(args.oracle, script.name, work)
        candidate = run(args.candidate, script.name, work)
        if oracle == candidate:
            return None
        keep = Path(args.out) / f"case-{args.seed}-{index}"
        keep.mkdir(parents=True, exist_ok=True)
        shutil.copy(script, keep / "case.cmake")
        for name, result in (("oracle", oracle), ("candidate", candidate)):
            (keep / f"{name}.out").write_bytes(result[1])
            (keep / f"{name}.err").write_bytes(result[2])
            (keep / f"{name}.rc").write_text(str(result[0]))
        return str(keep)
    finally:
        shutil.rmtree(work, ignore_errors=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--candidate", required=True)
    parser.add_argument("--oracle", default=ORACLE)
    parser.add_argument("--seed", type=int, default=1)
    parser.add_argument("-n", type=int, default=2000)
    parser.add_argument("-j", type=int, default=8)
    parser.add_argument("--out", default="fuzz-regex-out")
    args = parser.parse_args()
    Path(args.out).mkdir(parents=True, exist_ok=True)
    args.work = tempfile.mkdtemp(prefix="fuzz-regex-")
    with concurrent.futures.ThreadPoolExecutor(args.j) as pool:
        fails = [f for f in pool.map(lambda i: check(args, i), range(args.n)) if f]
    shutil.rmtree(args.work, ignore_errors=True)
    print(json.dumps({"seed": args.seed, "cases": args.n, "failures": len(fails),
                      "examples": fails[:10]}, indent=1))
    return 1 if fails else 0


if __name__ == "__main__":
    raise SystemExit(main())
