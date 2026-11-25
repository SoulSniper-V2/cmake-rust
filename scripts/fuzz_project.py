#!/usr/bin/env python3
"""Differential fuzzing of project mode: random target graphs.

Each case is a small random C project: static, shared, module, object and
interface libraries and executables spread over subdirectories, linked with
PUBLIC/PRIVATE/INTERFACE edges (with $<LINK_ONLY>, $<TARGET_OBJECTS> and
the occasional static-library cycle), usage requirements with generator
expressions, output-name and rpath properties, custom commands and targets,
precompiled headers, unity builds, tests, install(TARGETS/EXPORT) and
export().  Both sides configure it with Ninja; configure output and exit
code, and every file of the build tree (treediff.py normalization), must
match.  With --build, both trees are also built and the outcomes compared.

Usage: fuzz_project.py --candidate <staged bin/cmake> [--seed N] [-n COUNT] [-j N] [--build]
"""
import argparse
import concurrent.futures
import importlib.util
import json
import random
import re
import shutil
import subprocess
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("treediff", HERE / "treediff.py")
treediff = importlib.util.module_from_spec(spec)
spec.loader.exec_module(treediff)

GENEX_DEFS = ["D{n}", "$<$<CONFIG:Debug>:DBG{n}>", "$<$<COMPILE_LANGUAGE:C>:CLANG{n}>",
              "$<$<BOOL:ON>:ON{n}>", "V{n}=\"x y\"", "$<TARGET_PROPERTY:NAME>_{n}"]
OPTIONS = ["-Wall", "$<$<CONFIG:Release>:-O2>", "-fno-common", "$<$<C_COMPILER_ID:GNU>:-Wextra>"]
KINDS = ["STATIC", "SHARED", "OBJECT", "INTERFACE", "MODULE", "EXE", "EXE", "STATIC"]


class Gen:
    def __init__(self, rng):
        self.rng = rng
        self.targets = []  # (name, kind, dir)
        self.files = {}
        self.dirs = {"": []}

    def src(self, d, name, body):
        path = f"{d}/{name}" if d else name
        self.files[path] = body
        return name

    def target(self, i):
        rng = self.rng
        kind = rng.choice(KINDS)
        d = rng.choice(["", "", "sub1", "sub2"])
        name = f"t{i}"
        lines = self.dirs.setdefault(d, [])
        srcs = []
        if kind != "INTERFACE":
            srcs.append(self.src(d, f"{name}.c",
                                 f"int {name}_f(void) {{ return {i}; }}\n" if kind != "EXE"
                                 else "int main(void) { return 0; }\n"))
            if rng.random() < 0.3:
                srcs.append(self.src(d, f"{name}_b.c", f"int {name}_g(void) {{ return 1; }}\n"))
            if rng.random() < 0.2:
                out = f"{name}_gen.c"
                lines.append(f"add_custom_command(OUTPUT ${{CMAKE_CURRENT_BINARY_DIR}}/{out}\n"
                             f"  COMMAND ${{CMAKE_COMMAND}} -E echo \"int {name}_h(void) {{ return 2; }}\" > {out}\n"
                             f"  BYPRODUCTS {name}_side.txt VERBATIM)")
                srcs.append(f"${{CMAKE_CURRENT_BINARY_DIR}}/{out}")
        src_list = " ".join(srcs)
        if kind == "EXE":
            lines.append(f"add_executable({name} {src_list})")
        else:
            excl = " EXCLUDE_FROM_ALL" if rng.random() < 0.1 and kind not in ("INTERFACE", "OBJECT") else ""
            lines.append(f"add_library({name} {kind}{excl} {src_list})")
        self.targets.append((name, kind, d))
        return name, kind, d

    def usage(self, name, kind, d):
        rng = self.rng
        lines = self.dirs[d]
        scope = "INTERFACE" if kind == "INTERFACE" else None
        sc = lambda: scope or rng.choice(["PUBLIC", "PRIVATE", "INTERFACE"])
        n = rng.randrange(100)
        if rng.random() < 0.6:
            defs = " ".join(rng.choice(GENEX_DEFS).format(n=n + k) for k in range(rng.randint(1, 3)))
            lines.append(f"target_compile_definitions({name} {sc()} {defs})")
        if rng.random() < 0.4:
            lines.append(f"target_compile_options({name} {sc()} {rng.choice(OPTIONS)})")
        if rng.random() < 0.4:
            inc = rng.choice(["inc", "$<BUILD_INTERFACE:${CMAKE_CURRENT_SOURCE_DIR}/inc>$<INSTALL_INTERFACE:include>",
                              "${CMAKE_CURRENT_BINARY_DIR}", "/usr/include"])
            sys_kw = rng.choice(["", "SYSTEM ", "BEFORE "])
            lines.append(f"target_include_directories({name} {sys_kw}{sc()} {inc})")
        if rng.random() < 0.3:
            lines.append(f"target_compile_features({name} {sc()} {rng.choice(['c_std_99', 'c_std_11', 'c_std_90'])})")
        if rng.random() < 0.2:
            lines.append(f"target_link_options({name} {sc()} {rng.choice(['-Wl,--as-needed', '$<$<CONFIG:Debug>:-g>', 'LINKER:-z,defs'])})")
        if rng.random() < 0.15:
            lines.append(f"target_link_directories({name} {sc()} {rng.choice(['/opt/lib', '${CMAKE_BINARY_DIR}/libs'])})")
        if kind in ("STATIC", "SHARED", "MODULE", "EXE") and rng.random() < 0.5:
            props = rng.sample([
                "OUTPUT_NAME on_{name}", "PREFIX \"\"", "SUFFIX .so.x", "VERSION 1.2.3", "SOVERSION 1",
                "POSITION_INDEPENDENT_CODE ON", "C_VISIBILITY_PRESET hidden", "RUNTIME_OUTPUT_DIRECTORY ${CMAKE_BINARY_DIR}/bin",
                "LIBRARY_OUTPUT_DIRECTORY ${CMAKE_BINARY_DIR}/lib", "ARCHIVE_OUTPUT_DIRECTORY ${CMAKE_BINARY_DIR}/ar",
                "BUILD_RPATH /opt/rp", "INSTALL_RPATH $ORIGIN/../lib", "SKIP_BUILD_RPATH ON", "UNITY_BUILD ON",
                "C_STANDARD 11", "C_EXTENSIONS OFF", "DEBUG_POSTFIX _d", "OUTPUT_NAME_DEBUG dbg_{name}"], rng.randint(1, 3))
            props = [p.replace("{name}", name) for p in props]
            if kind not in ("SHARED", "MODULE"):
                props = [p for p in props if not p.startswith(("VERSION", "SOVERSION", "SUFFIX"))]
            if props:
                lines.append(f"set_target_properties({name} PROPERTIES {' '.join(props)})")
        if kind in ("STATIC", "SHARED", "EXE") and rng.random() < 0.1:
            self.files.setdefault(f"{d}/pch.h" if d else "pch.h", "#include <stdio.h>\n")
            lines.append(f"target_precompile_headers({name} PRIVATE pch.h)")

    def links(self):
        rng = self.rng
        for idx, (name, kind, d) in enumerate(self.targets):
            if kind == "OBJECT" and rng.random() < 0.5:
                continue
            pool = [t for t in self.targets[:idx] if t[1] != "EXE" and t[1] != "MODULE"]
            if not pool:
                continue
            for dep, dkind, _ in rng.sample(pool, min(len(pool), rng.randint(0, 3))):
                scope = "INTERFACE" if kind == "INTERFACE" else rng.choice(["PUBLIC", "PRIVATE", "INTERFACE", ""])
                item = dep
                if dkind == "STATIC" and rng.random() < 0.2:
                    item = f"$<LINK_ONLY:{dep}>"
                if dkind == "OBJECT" and kind not in ("INTERFACE", "OBJECT") and rng.random() < 0.5:
                    self.dirs[d].append(f"target_sources({name} PRIVATE $<TARGET_OBJECTS:{dep}>)")
                    continue
                if scope == "" and kind == "INTERFACE":
                    scope = "INTERFACE"
                # Plain and keyword signatures cannot be mixed on one target.
                scope = scope or "PRIVATE"
                self.dirs["" if d == "" else d].append(f"target_link_libraries({name} {scope} {item})")
        statics = [t for t in self.targets if t[1] == "STATIC"]
        if len(statics) >= 2 and rng.random() < 0.15:
            a, b = rng.sample(statics, 2)
            self.dirs[a[2]].append(f"target_link_libraries({a[0]} PUBLIC {b[0]})")
            self.dirs[b[2]].append(f"target_link_libraries({b[0]} PUBLIC {a[0]})")

    def extras(self):
        rng = self.rng
        root = self.dirs[""]
        exes = [t for t in self.targets if t[1] == "EXE"]
        if exes and rng.random() < 0.5:
            root.insert(0, "enable_testing()")
            e = rng.choice(exes)
            self.dirs[e[2]].append(f"add_test(NAME test_{e[0]} COMMAND {e[0]})")
            self.dirs[e[2]].append(f"set_tests_properties(test_{e[0]} PROPERTIES LABELS \"l1;l2\" TIMEOUT 5)")
        if rng.random() < 0.3:
            root.append("add_custom_target(ct ALL COMMAND ${CMAKE_COMMAND} -E echo hi BYPRODUCTS ct.txt)")
            if self.targets:
                root.append(f"add_dependencies(ct {rng.choice(self.targets)[0]})")
        installable = [t for t in self.targets if t[1] in ("STATIC", "SHARED", "INTERFACE", "EXE") and t[2] == ""]
        if installable and rng.random() < 0.4:
            names = " ".join(t[0] for t in installable)
            root.append(f"install(TARGETS {names} EXPORT ex RUNTIME DESTINATION bin LIBRARY DESTINATION lib ARCHIVE DESTINATION lib)")
            root.append("install(EXPORT ex NAMESPACE ns:: DESTINATION lib/cmake/ex)")
            if rng.random() < 0.5:
                root.append("export(EXPORT ex NAMESPACE ns:: FILE exTargets.cmake)")


def make_case(seed, index):
    rng = random.Random(f"{seed}:{index}")
    gen = Gen(rng)
    for i in range(rng.randint(2, 9)):
        name, kind, d = gen.target(i)
        gen.usage(name, kind, d)
    gen.links()
    gen.extras()
    files = dict(gen.files)
    head = "cmake_minimum_required(VERSION 3.20)\nproject(fz C)\n"
    subdirs = [d for d in ("sub1", "sub2") if d in gen.dirs]
    body = "\n".join(gen.dirs[""])
    # Subdirectories come first so the root can link their targets.
    files["CMakeLists.txt"] = head + "".join(f"add_subdirectory({d})\n" for d in subdirs) + body + "\n"
    for d in subdirs:
        files[f"{d}/CMakeLists.txt"] = "\n".join(gen.dirs[d]) + "\n"
    for d in ("", "sub1", "sub2"):
        files[f"{d}/inc/x.h" if d else "inc/x.h"] = "#define X 1\n"
    return files


def run_side(cmake, src, build, do_build):
    log = build.parent / f"{build.name}.log"
    rc = treediff.configure(Path(cmake), src, build, [], log, "Ninja")
    out = log.read_bytes()
    norm = treediff.normalizer(build, Path(cmake))
    # Compare the configured tree before building: built binaries embed
    # each side's own build-tree paths.
    tree = treediff.gather(build, norm)
    built = None
    if do_build and rc == 0:
        proc = subprocess.run(["ninja", "-C", str(build), "-k", "0"], capture_output=True)
        built = proc.returncode
    out = re.sub(rb"done \(\d+\.\d+s\)", b"done (<T>s)", norm(out).replace(str(src).encode(), b"<SRC>"))
    return rc, out, built, tree


def check(args, index):
    files = make_case(args.seed, index)
    root = Path(tempfile.mkdtemp(prefix="fp-", dir=args.work))
    try:
        src = root / "src"
        for rel, text in files.items():
            path = src / rel
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(text)
        oracle = run_side(args.oracle, src, root / "oracle", args.build)
        candidate = run_side(args.candidate, src, root / "candidate", args.build)
        if oracle == candidate:
            return None
        diffs = []
        if oracle[0] != candidate[0]:
            diffs.append(f"rc {oracle[0]} != {candidate[0]}")
        if oracle[1] != candidate[1]:
            diffs.append("configure output")
        if oracle[2] != candidate[2]:
            diffs.append(f"build {oracle[2]} != {candidate[2]}")
        for rel in sorted(set(oracle[3]) | set(candidate[3])):
            if oracle[3].get(rel) != candidate[3].get(rel):
                diffs.append(rel)
        keep = Path(args.out) / f"case-{args.seed}-{index}"
        shutil.rmtree(keep, ignore_errors=True)
        shutil.copytree(root, keep, symlinks=True)
        (keep / "diffs.txt").write_text("\n".join(diffs) + "\n")
        return f"{keep}: {', '.join(diffs[:4])}"
    finally:
        shutil.rmtree(root, ignore_errors=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--candidate", required=True)
    parser.add_argument("--oracle", default=str(treediff.ORACLE))
    parser.add_argument("--seed", type=int, default=1)
    parser.add_argument("-n", type=int, default=100)
    parser.add_argument("-j", type=int, default=6)
    parser.add_argument("--build", action="store_true")
    parser.add_argument("--out", default="fuzz-project-out")
    args = parser.parse_args()
    Path(args.out).mkdir(parents=True, exist_ok=True)
    args.work = tempfile.mkdtemp(prefix="fuzz-project-")
    with concurrent.futures.ThreadPoolExecutor(args.j) as pool:
        fails = [f for f in pool.map(lambda i: check(args, i), range(args.n)) if f]
    shutil.rmtree(args.work, ignore_errors=True)
    print(json.dumps({"seed": args.seed, "cases": args.n, "failures": len(fails),
                      "examples": fails[:12]}, indent=1))
    return 1 if fails else 0


if __name__ == "__main__":
    raise SystemExit(main())
