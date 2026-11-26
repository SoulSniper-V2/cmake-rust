#!/usr/bin/env python3
"""Differential fuzzing of find_file/find_path/find_library/find_program and
find_package (config mode) in project mode.

Each case builds a random search tree (prefixes with include/, lib/,
lib64/, bin/, share/ and lib/cmake/<pkg>-<ver>/ package configs with
version files, symlinks, non-executable "programs", versioned shared
libraries) and a CMakeLists.txt with project(x NONE) that runs random find
calls (NAMES, NAMES_PER_DIR, HINTS, PATHS, PATH_SUFFIXES, ENV, NO_DEFAULT_PATH
and the NO_*_PATH switches, REQUIRED, NO_CACHE, VALIDATOR, CMAKE_PREFIX_PATH
/ CMAKE_FIND_ROOT_PATH / CMAKE_FIND_USE_* variables, find_package with
versions, ranges, EXACT, COMPONENTS, NAMES, CONFIGS and PATHS) and prints
every result, the <pkg>_DIR/_FOUND/_VERSION variables and the cache.
Configure output and exit code must match byte for byte after the scratch
path is normalized.

Usage: fuzz_find.py --candidate <staged bin/cmake> [--seed N] [-n COUNT] [-j N]
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
PREFIXES = ["p1", "p2", "p3/sub", "opt"]
PKGS = ["Foo", "bar", "Baz"]


def build_tree(rng, root):
    files = {}
    for prefix in PREFIXES:
        if rng.random() < 0.2:
            continue
        for _ in range(rng.randint(1, 5)):
            kind = rng.randrange(6)
            if kind == 0:
                files[f"{prefix}/include/{rng.choice(['a.h', 'b.h', 'x/y.h'])}"] = "//h\n"
            elif kind == 1:
                d = rng.choice(["lib", "lib64", "lib/x86_64-linux-gnu"])
                name = rng.choice(["libm1.a", "libm1.so", "libm2.so.1", "libm3.so", "m4.lib"])
                files[f"{prefix}/{d}/{name}"] = "lib"
            elif kind == 2:
                files[f"{prefix}/bin/{rng.choice(['tool', 'tool2', 'tool.sh'])}"] = ("exe", rng.random() < 0.8)
            elif kind == 3:
                pkg = rng.choice(PKGS)
                ver = rng.choice(["1.0", "1.2.3", "2.0", "0.9"])
                layout = rng.choice([f"lib/cmake/{pkg}-{ver}", f"lib/cmake/{pkg}", f"share/{pkg.lower()}",
                                     f"{pkg}", f"lib/{pkg}/cmake", "cmake"])
                cfg = rng.choice([f"{pkg}Config.cmake", f"{pkg.lower()}-config.cmake"])
                files[f"{prefix}/{layout}/{cfg}"] = (
                    f'set({pkg}_FOUND_MARK "{prefix}/{layout}")\n'
                    f'message(STATUS "loaded {pkg} from {prefix}/{layout} comps=${{{pkg}_FIND_COMPONENTS}}")\n')
                if rng.random() < 0.8:
                    vfile = cfg.replace("Config.cmake", "ConfigVersion.cmake").replace("-config.cmake", "-config-version.cmake")
                    files[f"{prefix}/{layout}/{vfile}"] = (
                        f'set(PACKAGE_VERSION "{ver}")\n'
                        'if(PACKAGE_FIND_VERSION_RANGE)\n'
                        '  if(PACKAGE_VERSION VERSION_LESS PACKAGE_FIND_VERSION_MIN OR PACKAGE_VERSION VERSION_GREATER PACKAGE_FIND_VERSION_MAX)\n'
                        '    set(PACKAGE_VERSION_COMPATIBLE FALSE)\n  else()\n    set(PACKAGE_VERSION_COMPATIBLE TRUE)\n  endif()\n'
                        'elseif(PACKAGE_VERSION VERSION_LESS PACKAGE_FIND_VERSION)\n  set(PACKAGE_VERSION_COMPATIBLE FALSE)\n'
                        'else()\n  set(PACKAGE_VERSION_COMPATIBLE TRUE)\n'
                        '  if(PACKAGE_FIND_VERSION STREQUAL PACKAGE_VERSION)\n    set(PACKAGE_VERSION_EXACT TRUE)\n  endif()\nendif()\n')
            elif kind == 4:
                files[f"{prefix}/share/{rng.choice(['data.txt', 'doc/readme'])}"] = "d"
            else:
                files[f"{prefix}/lib/libm2.so"] = ("link", "libm2.so.1")
                files[f"{prefix}/lib/libm2.so.1"] = "lib"
    for rel, content in files.items():
        path = root / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        if isinstance(content, tuple) and content[0] == "link":
            if not path.exists():
                os.symlink(content[1], path)
        elif isinstance(content, tuple):
            path.write_text("#!/bin/sh\n")
            path.chmod(0o755 if content[1] else 0o644)
        else:
            path.write_text(content)


def gen_find(rng, i):
    kind = rng.choice(["find_file", "find_path", "find_library", "find_program", "find_package", "find_package"])
    var = f"R{i}"
    opts = []
    if kind == "find_package":
        pkg = rng.choice(PKGS + ["Missing"])
        args = [pkg]
        if rng.random() < 0.5:
            args.append(rng.choice(["1.0", "1.2", "2.0", "1.0...2.0", "0.5...<1.0", "3.0"]))
            if rng.random() < 0.2 and "..." not in args[-1]:
                args.append("EXACT")
        args.append(rng.choice(["CONFIG", "CONFIG", "NO_MODULE", ""]))
        if rng.random() < 0.3:
            args += ["COMPONENTS", "c1", "c2"]
        if rng.random() < 0.3:
            args += ["NAMES", pkg, pkg.lower()]
        if rng.random() < 0.2:
            args += ["CONFIGS", f"{pkg}Config.cmake"]
        if rng.random() < 0.4:
            args += ["PATHS", *rng.sample(["${S}/p1", "${S}/p2", "${S}/p3", "${S}/opt"], rng.randint(1, 2))]
        if rng.random() < 0.3:
            args += rng.sample(["NO_DEFAULT_PATH", "NO_CMAKE_PATH", "NO_CMAKE_ENVIRONMENT_PATH",
                                "NO_SYSTEM_ENVIRONMENT_PATH", "NO_CMAKE_PACKAGE_REGISTRY",
                                "NO_CMAKE_SYSTEM_PATH", "QUIET"], rng.randint(1, 2))
        if rng.random() < 0.3:
            args += rng.choice([["PATH_SUFFIXES", "cmake"], ["PATH_SUFFIXES", "lib/cmake", "share"]])
        args.append("QUIET")
        body = f"find_package({' '.join(a for a in args if a)})"
        report = (f'message(STATUS "{pkg}: found=[${{{pkg}_FOUND}}] dir=[${{{pkg}_DIR}}] ver=[${{{pkg}_VERSION}}] '
                  f'cfg=[${{{pkg}_CONFIG}}] considered=[${{{pkg}_CONSIDERED_CONFIGS}}] [${{{pkg}_CONSIDERED_VERSIONS}}]")')
        return body + "\n" + report
    names = {"find_file": ["a.h", "y.h", "data.txt", "readme", "nope.h"],
             "find_path": ["a.h", "b.h", "x/y.h", "y.h", "nope.h"],
             "find_library": ["m1", "m2", "m3", "libm1.a", "m4", "nope"],
             "find_program": ["tool", "tool2", "tool.sh", "nope"]}[kind]
    picked = rng.sample(names, rng.randint(1, 2))
    if rng.random() < 0.5:
        opts += ["NAMES", *picked]
        if rng.random() < 0.3:
            opts.append("NAMES_PER_DIR")
    else:
        opts.append(picked[0])
    if rng.random() < 0.4:
        opts += ["HINTS", *rng.sample(["${S}/p1", "${S}/p2/lib", "${S}/opt/bin", "ENV HINTV"], rng.randint(1, 2))]
    if rng.random() < 0.5:
        opts += ["PATHS", *rng.sample(["${S}/p1", "${S}/p2", "${S}/p3/sub", "${S}/opt", "${S}/missing", "/usr/include"], rng.randint(1, 3))]
    if rng.random() < 0.4:
        opts += ["PATH_SUFFIXES", *rng.sample(["include", "lib", "lib64", "bin", "share", "x", "doc"], rng.randint(1, 2))]
    if rng.random() < 0.4:
        opts += rng.sample(["NO_DEFAULT_PATH", "NO_CMAKE_PATH", "NO_CMAKE_ENVIRONMENT_PATH",
                            "NO_SYSTEM_ENVIRONMENT_PATH", "NO_CMAKE_SYSTEM_PATH", "NO_CACHE",
                            "CMAKE_FIND_ROOT_PATH_BOTH", "ONLY_CMAKE_FIND_ROOT_PATH", "NO_CMAKE_FIND_ROOT_PATH"],
                           rng.randint(1, 3))
    if rng.random() < 0.1:
        opts += ["VALIDATOR", "val"]
    if kind == "find_program" and rng.random() < 0.2:
        opts.append("NAMES_PER_DIR")
    if rng.random() < 0.15:
        opts += ["DOC", '"docstring"']
    body = f"{kind}({var} {' '.join(opts)})"
    return body + f'\nmessage(STATUS "{var}=[${{{var}}}] cache=[$CACHE{{{var}}}]")'


def make_case(seed, index):
    rng = random.Random(f"{seed}:{index}")
    lines = ["cmake_minimum_required(VERSION 3.25)", "project(x NONE)", 'set(S "${CMAKE_CURRENT_SOURCE_DIR}/tree")',
             "function(val result path)", "  if(path MATCHES \"p2\")", "    set(${result} FALSE PARENT_SCOPE)", "  endif()",
             "endfunction()"]
    for _ in range(rng.randint(0, 3)):
        lines.append(rng.choice([
            'list(APPEND CMAKE_PREFIX_PATH "${S}/p1" "${S}/p3/sub")',
            'set(CMAKE_PREFIX_PATH "${S}/opt")',
            'set(CMAKE_FIND_ROOT_PATH "${S}/p2")',
            'set(CMAKE_FIND_ROOT_PATH_MODE_LIBRARY ONLY)',
            'set(CMAKE_FIND_ROOT_PATH_MODE_INCLUDE BOTH)',
            'set(CMAKE_FIND_USE_CMAKE_PATH FALSE)',
            'set(CMAKE_FIND_LIBRARY_SUFFIXES .a)',
            'set(CMAKE_FIND_LIBRARY_PREFIXES "" lib)',
            'set(CMAKE_INCLUDE_PATH "${S}/p2/include")',
            'set(CMAKE_LIBRARY_PATH "${S}/p1/lib64")',
            'set(CMAKE_PROGRAM_PATH "${S}/opt/bin")',
            'set(CMAKE_SYSTEM_PREFIX_PATH "${S}/p2")',
            'set(CMAKE_FIND_PACKAGE_SORT_ORDER NATURAL)\nset(CMAKE_FIND_PACKAGE_SORT_DIRECTION DEC)',
            'set(CMAKE_FIND_PACKAGE_PREFER_CONFIG TRUE)',
            'set(Foo_DIR "${S}/p1/lib/cmake/Foo")',
            'set(CMAKE_FIND_DEBUG_MODE TRUE)',
        ]))
    for i in range(rng.randint(2, 7)):
        lines.append(gen_find(rng, i))
    env = {}
    if rng.random() < 0.3:
        env["CMAKE_PREFIX_PATH"] = "<S>/p2"
    if rng.random() < 0.3:
        env["HINTV"] = "<S>/p3/sub/include"
    return "\n".join(lines) + "\n", env, rng.random()


def run_side(cmake, src, build, env_extra):
    if build.exists():
        shutil.rmtree(build)
    env = {k: v for k, v in os.environ.items() if k not in ("CMAKE_PREFIX_PATH", "MAKEFLAGS")}
    env["PATH"] = "/usr/bin:/bin"
    env["HOME"] = str(src)  # isolate the package registry
    for key, value in env_extra.items():
        env[key] = value.replace("<S>", str(src / "tree"))
    proc = subprocess.run([cmake, "-S", str(src), "-B", str(build), "-G", "Ninja"],
                          capture_output=True, env=env, timeout=120)
    out = proc.stdout + proc.stderr
    out = out.replace(str(build).encode(), b"<B>").replace(str(src).encode(), b"<S>")
    out = re.sub(rb"\(\d+\.\d+s\)", b"(<T>s)", out)
    cache = (build / "CMakeCache.txt").read_bytes() if (build / "CMakeCache.txt").exists() else b""
    cache = b"\n".join(l for l in cache.replace(str(build).encode(), b"<B>").replace(str(src).encode(), b"<S>").splitlines()
                       if l.startswith((b"R", b"Foo", b"bar", b"Baz", b"Missing")))
    return proc.returncode, out, cache


def check(args, index):
    text, env, tree_seed = make_case(args.seed, index)
    root = Path(tempfile.mkdtemp(prefix="ffd-", dir=args.work))
    try:
        src = root / "src"
        (src / "tree").mkdir(parents=True)
        build_tree(random.Random(f"{args.seed}:{index}:tree"), src / "tree")
        (src / "CMakeLists.txt").write_text(text)
        results = []
        for cmake in (args.oracle, args.candidate):
            r = run_side(cmake, src, root / "b", env)
            # Each side's own module/tool locations.
            prefix = str(Path(cmake).resolve().parent.parent).encode()
            # The oracle runs from its build tree: its install prefix
            # (_CMAKE_INSTALL_DIR) is the source checkout's grandparent.
            data_root = os.path.realpath("/tmp/cmake-rust-validation.hSLgEZHR/kitware")
            install = os.path.dirname(os.path.dirname(data_root)).encode()

            def norm(x):
                x = x.replace(data_root.encode(), b"<DATA>").replace(prefix + b"/share/cmake-4.1", b"<DATA>")
                x = x.replace(prefix, b"<CMAKE_PREFIX>")
                return re.sub(re.escape(install) + rb"(?=/|\s|$)", b"<CMAKE_PREFIX>", x)
            results.append(tuple(norm(x) if isinstance(x, bytes) else x for x in r))
        if results[0] == results[1]:
            return None
        keep = Path(args.out) / f"case-{args.seed}-{index}"
        shutil.rmtree(keep, ignore_errors=True)
        shutil.copytree(src, keep / "src", symlinks=True)
        (keep / "env.json").write_text(json.dumps(env))
        for name, r in zip(("oracle", "candidate"), results):
            (keep / f"{name}.out").write_bytes(r[1])
            (keep / f"{name}.cache").write_bytes(r[2])
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
    parser.add_argument("-n", type=int, default=300)
    parser.add_argument("-j", type=int, default=6)
    parser.add_argument("--out", default="fuzz-find-out")
    args = parser.parse_args()
    Path(args.out).mkdir(parents=True, exist_ok=True)
    args.work = tempfile.mkdtemp(prefix="fuzz-find-")
    with concurrent.futures.ThreadPoolExecutor(args.j) as pool:
        fails = [f for f in pool.map(lambda i: check(args, i), range(args.n)) if f]
    shutil.rmtree(args.work, ignore_errors=True)
    print(json.dumps({"seed": args.seed, "cases": args.n, "failures": len(fails),
                      "examples": fails[:10]}, indent=1))
    return 1 if fails else 0


if __name__ == "__main__":
    raise SystemExit(main())
