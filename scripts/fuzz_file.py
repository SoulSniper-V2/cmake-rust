#!/usr/bin/env python3
"""Differential fuzzing of file() and get_filename_component() in script mode.

Each case is a `cmake -P` script that performs a random sequence of file()
operations inside its own scratch tree (write/append/read with OFFSET,
LIMIT and HEX, STRINGS with its filters, GLOB and GLOB_RECURSE with
RELATIVE/LIST_DIRECTORIES/FOLLOW_SYMLINKS, COPY, COPY_FILE, RENAME,
REMOVE, MAKE_DIRECTORY, SIZE, TOUCH, CREATE_LINK, READ_SYMLINK, REAL_PATH,
RELATIVE_PATH, TO_CMAKE_PATH/TO_NATIVE_PATH, CHMOD) and prints every result,
then lists the resulting tree.  Oracle and candidate stdout, stderr and exit
code must match byte for byte after the scratch path is normalized.

Usage: fuzz_file.py --candidate <staged bin/cmake> [--seed N] [-n COUNT] [-j N]
"""
import argparse
import concurrent.futures
import json
import random
import re
import shutil
import subprocess
import tempfile
from pathlib import Path

ORACLE = "/tmp/cmake-rust-validation.hSLgEZHR/kitware-reference/bin/cmake"
NAMES = ["a.txt", "b.bin", "dir", "dir/c.txt", "dir/sub", "dir/sub/d.txt", "e f.txt",
         "link", "dir/link2", "missing.txt", ".hidden", "x.tar.gz", "UPPER.TXT"]
CONTENT = ["hello", "line1\nline2\n", "", "a;b;c", "tab\there", "\\back\\slash",
           "bin\x01\x02ary", "é ü", "trailing  ", "\n\n\n", "x" * 300]
VARS = ["v1", "v2", "v3"]


def q(text):
    """A quoted CMake argument for arbitrary text."""
    return '"' + text.replace("\\", "\\\\").replace('"', '\\"').replace("$", "\\$").replace(";", "\\;") + '"'


def gen_op(rng):
    n1, n2 = rng.choice(NAMES), rng.choice(NAMES)
    v = rng.choice(VARS)
    c = rng.choice(CONTENT)
    choice = rng.randrange(24)
    if choice == 0:
        return f"file(WRITE {n1} {q(c)})"
    if choice == 1:
        return f"file(APPEND {n1} {q(c)})"
    if choice == 2:
        opts = rng.choice(["", " LIMIT 3", " OFFSET 2", " OFFSET 1 LIMIT 4", " HEX", " LIMIT 0 HEX", " OFFSET 999"])
        return f"file(READ {n1} {v}{opts})"
    if choice == 3:
        opts = rng.choice(["", " LIMIT_COUNT 2", " LENGTH_MINIMUM 3", " LENGTH_MAXIMUM 4",
                           " REGEX \"[a-z]+\"", " NEWLINE_CONSUME", " NO_HEX_CONVERSION",
                           " LIMIT_INPUT 5", " LIMIT_OUTPUT 6", " ENCODING UTF-8"])
        return f"file(STRINGS {n1} {v}{opts})"
    if choice == 4:
        opts = rng.choice(["", " LIST_DIRECTORIES true", " LIST_DIRECTORIES false",
                           " RELATIVE ${CMAKE_CURRENT_LIST_DIR}", " FOLLOW_SYMLINKS"])
        kind = rng.choice(["GLOB", "GLOB_RECURSE"])
        pat = rng.choice(["*", "*.txt", "dir/*", "*/*.txt", "[a-c]*", "?.*", "**"])
        return f"file({kind} {v}{opts} {q(pat)})"
    if choice == 5:
        return f"file(COPY {n1} DESTINATION {n2})"
    if choice == 6:
        opts = rng.choice(["", " ONLY_IF_DIFFERENT", " RESULT r", " INPUT_MAY_BE_RECENT"])
        return f"file(COPY_FILE {n1} {n2}{opts})"
    if choice == 7:
        opts = rng.choice(["", " RESULT r", " NO_REPLACE", " NO_REPLACE RESULT r"])
        return f"file(RENAME {n1} {n2}{opts})"
    if choice == 8:
        return f"file({rng.choice(['REMOVE', 'REMOVE_RECURSE'])} {n1} {n2})"
    if choice == 9:
        return f"file(MAKE_DIRECTORY {n1})"
    if choice == 10:
        return f"file(SIZE {n1} {v})"
    if choice == 11:
        return f"file({rng.choice(['TOUCH', 'TOUCH_NOCREATE'])} {n1})"
    if choice == 12:
        opts = rng.choice(["", " SYMBOLIC", " RESULT r", " COPY_ON_ERROR SYMBOLIC"])
        return f"file(CREATE_LINK {n1} {n2}{opts})"
    if choice == 13:
        return f"file(READ_SYMLINK {n1} {v})"
    if choice == 14:
        return f"file(REAL_PATH {n1} {v}{rng.choice(['', ' EXPAND_TILDE', ' BASE_DIRECTORY dir'])})"
    if choice == 15:
        return f"file(RELATIVE_PATH {v} {q(rng.choice(['/a/b', '/a/b/', '/', 'rel', '/a/../c']))} {q(rng.choice(['/a/b/c', '/a', '/x/y', '', 'rel/x']))})"
    if choice == 16:
        return f"file({rng.choice(['TO_CMAKE_PATH', 'TO_NATIVE_PATH'])} {q(rng.choice(['a\\b', 'a:b;c', '/x//y/', 'C:\\w', '']))} {v})"
    if choice == 17:
        perms = rng.choice(["OWNER_READ", "OWNER_READ OWNER_WRITE", "OWNER_READ OWNER_WRITE OWNER_EXECUTE GROUP_READ"])
        return f"file(CHMOD {n1} PERMISSIONS {perms})"
    if choice == 18:
        comp = rng.choice(["DIRECTORY", "NAME", "EXT", "NAME_WE", "LAST_EXT", "NAME_WLE", "PATH", "ABSOLUTE", "REALPATH"])
        return f"get_filename_component({v} {q(rng.choice(NAMES + ['/a/b.c.d', 'x/', '.f', 'a.b/c']))} {comp})"
    if choice == 19:
        return f"file({rng.choice(['MD5', 'SHA1', 'SHA256'])} {n1} {v})"
    if choice == 20:
        return f"file(TIMESTAMP {n1} {v} \"%Y\" UTC)"
    if choice == 21:
        return f"if(EXISTS ${{CMAKE_CURRENT_LIST_DIR}}/{n1})\n  message(\"exists {n1}\")\nendif()\nif(IS_DIRECTORY {n1})\n  message(\"dir {n1}\")\nendif()\nif(IS_SYMLINK ${{CMAKE_CURRENT_LIST_DIR}}/{n1})\n  message(\"link {n1}\")\nendif()"
    if choice == 22:
        return f"file(WRITE {n1} {q(c)})\nfile(CONFIGURE OUTPUT {n2} CONTENT {q('@v1@ ${{v2}} ' + c)} @ONLY)"
    return f"file(GLOB_RECURSE {v} LIST_DIRECTORIES true RELATIVE ${{CMAKE_CURRENT_LIST_DIR}} *)"


def make_case(seed, index):
    rng = random.Random(f"{seed}:{index}")
    lines = ["cmake_minimum_required(VERSION 3.20)"]
    for _ in range(rng.randint(3, 12)):
        lines.append(gen_op(rng))
        lines.append('message("v1=[${v1}] v2=[${v2}] v3=[${v3}] r=[${r}]")')
        lines.append("unset(v1)\nunset(v2)\nunset(v3)\nunset(r)")
    lines.append("file(GLOB_RECURSE all LIST_DIRECTORIES true RELATIVE ${CMAKE_CURRENT_LIST_DIR} *)")
    lines.append('list(SORT all)\nforeach(f IN LISTS all)\n  file(SIZE ${f} s)\n  message("tree ${f}")\nendforeach()')
    return "\n".join(lines) + "\n"


def wipe(path):
    """Remove a scratch tree even after file(CHMOD) dropped permissions."""
    if path.exists():
        subprocess.run(["chmod", "-R", "u+rwx", str(path)], capture_output=True)
        shutil.rmtree(path, ignore_errors=True)


def run(cmake, work):
    proc = subprocess.run([cmake, "-P", "case.cmake"], cwd=work, capture_output=True, timeout=60)
    def norm(data):
        data = data.replace(str(work).encode(), b"<W>")
        # cmGeneratedFileStream temporaries carry a random hex suffix.
        return re.sub(rb"\.tmp[0-9a-f]{5}", b".tmp<R>", data)
    return proc.returncode, norm(proc.stdout), norm(proc.stderr)


def check(args, index):
    text = make_case(args.seed, index)
    results = []
    root = Path(tempfile.mkdtemp(prefix="ff-", dir=args.work))
    try:
        for cmake in (args.oracle, args.candidate):
            work = root / "w"  # same path for both sides
            wipe(work)
            work.mkdir()
            (work / "case.cmake").write_bytes(text.encode("utf-8", "surrogateescape"))
            results.append(run(cmake, work))
        if results[0] == results[1]:
            return None
        keep = Path(args.out) / f"case-{args.seed}-{index}"
        keep.mkdir(parents=True, exist_ok=True)
        (keep / "case.cmake").write_bytes(text.encode("utf-8", "surrogateescape"))
        for name, result in zip(("oracle", "candidate"), results):
            (keep / f"{name}.out").write_bytes(result[1])
            (keep / f"{name}.err").write_bytes(result[2])
            (keep / f"{name}.rc").write_text(str(result[0]))
        return str(keep)
    finally:
        wipe(root)


def main():
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--candidate", required=True)
    parser.add_argument("--oracle", default=ORACLE)
    parser.add_argument("--seed", type=int, default=1)
    parser.add_argument("-n", type=int, default=1000)
    parser.add_argument("-j", type=int, default=8)
    parser.add_argument("--out", default="fuzz-file-out")
    args = parser.parse_args()
    Path(args.out).mkdir(parents=True, exist_ok=True)
    args.work = tempfile.mkdtemp(prefix="fuzz-file-")
    with concurrent.futures.ThreadPoolExecutor(args.j) as pool:
        fails = [f for f in pool.map(lambda i: check(args, i), range(args.n)) if f]
    shutil.rmtree(args.work, ignore_errors=True)
    print(json.dumps({"seed": args.seed, "cases": args.n, "failures": len(fails),
                      "examples": fails[:10]}, indent=1))
    return 1 if fails else 0


if __name__ == "__main__":
    raise SystemExit(main())
