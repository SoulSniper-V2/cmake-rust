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
import os
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
              "$<$<COMPILE_LANGUAGE:CXX>:CXXLANG{n}>", "$<$<COMPILE_LANG_AND_ID:CXX,GNU>:GXX{n}>",
              "$<$<BOOL:ON>:ON{n}>", "V{n}=\"x y\"", "$<TARGET_PROPERTY:NAME>_{n}"]
OPTIONS = ["-Wall", "$<$<CONFIG:Release>:-O2>", "-fno-common", "$<$<C_COMPILER_ID:GNU>:-Wextra>",
           "$<$<COMPILE_LANGUAGE:CXX>:-fno-exceptions>", "$<$<CXX_COMPILER_ID:GNU>:-Wpedantic>"]
KINDS = ["STATIC", "SHARED", "OBJECT", "INTERFACE", "MODULE", "EXE", "EXE", "STATIC"]


class Gen:
    def __init__(self, rng, cxx=False):
        self.rng = rng
        self.cxx = cxx
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
            ext = "cpp" if self.cxx and rng.random() < 0.6 else "c"
            srcs.append(self.src(d, f"{name}.{ext}",
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
            feats = ['c_std_99', 'c_std_11', 'c_std_90']
            if self.cxx:
                feats += ['cxx_std_11', 'cxx_std_14', 'cxx_std_17', 'cxx_std_20', 'cxx_constexpr']
            lines.append(f"target_compile_features({name} {sc()} {rng.choice(feats)})")
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
                "C_STANDARD 11", "C_EXTENSIONS OFF", "DEBUG_POSTFIX _d", "OUTPUT_NAME_DEBUG dbg_{name}"]
                + (["CXX_STANDARD 17", "CXX_STANDARD_REQUIRED ON", "CXX_EXTENSIONS OFF", "CXX_VISIBILITY_PRESET hidden",
                    "VISIBILITY_INLINES_HIDDEN ON", "LINKER_LANGUAGE CXX"] if self.cxx else []), rng.randint(1, 3))
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
        built = [t for t in self.targets if t[1] in ("STATIC", "SHARED", "MODULE", "EXE")]
        if built and rng.random() < 0.4:
            t = rng.choice(built)
            genex = rng.choice(["$<TARGET_FILE:{n}>", "$<TARGET_FILE_DIR:{n}>", "$<TARGET_FILE_NAME:{n}>",
                                "$<TARGET_PROPERTY:{n},NAME>", "$<TARGET_LINKER_FILE:{n}>", "$<TARGET_FILE_BASE_NAME:{n}>",
                                "$<CONFIG>", "$<IF:$<CONFIG:Debug>,dbg,rel>"]).replace("{n}", t[0])
            if t[1] in ("MODULE", "EXE") and "LINKER_FILE" in genex:
                genex = f"$<TARGET_FILE:{t[0]}>"
            self.dirs[t[2]].append(f"add_custom_command(TARGET {t[0]} POST_BUILD COMMAND ${{CMAKE_COMMAND}} -E echo \"{genex}\" VERBATIM)")
        if built and rng.random() < 0.3:
            t = rng.choice(built)
            out = f"gen_{t[0]}.txt"
            self.dirs[t[2]].append(f"file(GENERATE OUTPUT ${{CMAKE_CURRENT_BINARY_DIR}}/{out} CONTENT \"$<TARGET_FILE:{t[0]}>;$<TARGET_PROPERTY:{t[0]},COMPILE_DEFINITIONS>\")")
        if self.targets and rng.random() < 0.3:
            t = rng.choice(self.targets)
            self.dirs[t[2]].append(f"add_library(alias::{t[0]} ALIAS {t[0]})")
        if rng.random() < 0.2:
            self.files.setdefault("imp.txt", "")
            self.dirs[""].append("add_library(imp SHARED IMPORTED)\nset_target_properties(imp PROPERTIES IMPORTED_LOCATION /opt/lib/libimp.so IMPORTED_SONAME libimp.so.1 INTERFACE_COMPILE_DEFINITIONS IMP=1)")
            users = [t for t in self.targets if t[1] in ("SHARED", "EXE") and t[2] == ""]
            if users:
                self.dirs[""].append(f"target_link_libraries({rng.choice(users)[0]} PRIVATE imp)")
        statics = [t for t in self.targets if t[1] == "STATIC"]
        linkers = [t for t in self.targets if t[1] in ("SHARED", "EXE")]
        if statics and linkers and rng.random() < 0.25:
            lib, user = rng.choice(statics), rng.choice(linkers)
            feature = rng.choice(["$<LINK_LIBRARY:WHOLE_ARCHIVE,{l}>", "$<LINK_GROUP:RESCAN,{l}>"]).replace("{l}", lib[0])
            self.dirs[user[2]].append(f"target_link_libraries({user[0]} PRIVATE \"{feature}\")")
        srcs = [(t, f) for t in self.targets for f in self.files if f.endswith((".c", ".cpp")) and f.startswith(f"{t[2]}/" if t[2] else "") and f.split("/")[-1].startswith(t[0] + ".")]
        if srcs and rng.random() < 0.3:
            (t, f) = rng.choice(srcs)
            prop = rng.choice(["COMPILE_DEFINITIONS SRCDEF=1", "COMPILE_OPTIONS -Wshadow", "OBJECT_DEPENDS ${CMAKE_CURRENT_SOURCE_DIR}/inc/x.h",
                               "SKIP_PRECOMPILE_HEADERS ON", "SKIP_UNITY_BUILD_INCLUSION ON", "INCLUDE_DIRECTORIES /opt/srcinc"])
            self.dirs[t[2]].append(f"set_source_files_properties({f.split('/')[-1]} PROPERTIES {prop})")
        if self.targets and rng.random() < 0.2:
            t = rng.choice([x for x in self.targets if x[1] != "INTERFACE"] or self.targets)
            scope = "INTERFACE" if t[1] == "INTERFACE" else "PUBLIC"
            self.dirs[t[2]].append(f"target_sources({t[0]} {scope} FILE_SET HEADERS BASE_DIRS ${{CMAKE_CURRENT_SOURCE_DIR}}/inc FILES ${{CMAKE_CURRENT_SOURCE_DIR}}/inc/x.h)")
        # Features the extra generators read: source groups (creation order,
        # regex precedence, nesting), unlisted headers next to sources,
        # current-dir and directory include paths, and output paths with
        # "." components.
        if self.targets and rng.random() < 0.35:
            d = rng.choice(sorted(self.dirs))
            names = ["Zeta", "Alpha", "Mid\\\\Inner", "Beta"]
            rng.shuffle(names)
            local = [f.split("/")[-1] for f in self.files if (f.rsplit("/", 1)[0] if "/" in f else "") == d and f.endswith((".c", ".cpp"))]
            for n in names[: rng.randint(1, 4)]:
                if local and rng.random() < 0.6:
                    self.dirs[d].append(f"source_group(\"{n}\" FILES {rng.choice(local)})")
                else:
                    self.dirs[d].append(f"source_group(\"{n}\" REGULAR_EXPRESSION \"{rng.choice(['[.]c$', '_b[.]c$', 'x[.]h$', '^.*$'])}\")")
        for (name, kind, d) in self.targets:
            if kind != "INTERFACE" and rng.random() < 0.15:
                ext = rng.choice(["h", "hpp", "hh", "hxx", "txx"])
                self.files[f"{d}/{name}.{ext}" if d else f"{name}.{ext}"] = "\n"
        if rng.random() < 0.3:
            d = rng.choice(sorted(self.dirs))
            self.dirs[d].insert(0, "set(CMAKE_INCLUDE_CURRENT_DIR ON)")
            if rng.random() < 0.5:
                self.dirs[d].insert(1, "include_directories(${CMAKE_CURRENT_SOURCE_DIR}/inc)")
        if rng.random() < 0.2:
            root.insert(0, rng.choice(["set(EXECUTABLE_OUTPUT_PATH ${CMAKE_BINARY_DIR}/./bin)",
                                       "set(LIBRARY_OUTPUT_PATH ${CMAKE_BINARY_DIR}/lib/../libs)",
                                       "set(EXECUTABLE_OUTPUT_PATH ${CMAKE_BINARY_DIR})"]))
        installable = [t for t in self.targets if t[1] in ("STATIC", "SHARED", "INTERFACE", "EXE") and t[2] == ""]
        if installable and rng.random() < 0.4:
            names = " ".join(t[0] for t in installable)
            root.append(f"install(TARGETS {names} EXPORT ex RUNTIME DESTINATION bin LIBRARY DESTINATION lib ARCHIVE DESTINATION lib)")
            root.append("install(EXPORT ex NAMESPACE ns:: DESTINATION lib/cmake/ex)")
            if rng.random() < 0.5:
                root.append("export(EXPORT ex NAMESPACE ns:: FILE exTargets.cmake)")


def make_case(seed, index, cxx=False):
    rng = random.Random(f"{seed}:{index}")
    gen = Gen(rng, cxx)
    for i in range(rng.randint(2, 9)):
        name, kind, d = gen.target(i)
        gen.usage(name, kind, d)
    gen.links()
    gen.extras()
    files = dict(gen.files)
    head = f"cmake_minimum_required(VERSION 3.20)\nproject(fz C{' CXX' if cxx else ''})\n"
    subdirs = [d for d in ("sub1", "sub2") if d in gen.dirs]
    body = "\n".join(gen.dirs[""])
    # Subdirectories come first so the root can link their targets.
    files["CMakeLists.txt"] = head + "".join(f"add_subdirectory({d})\n" for d in subdirs) + body + "\n"
    for d in subdirs:
        # Sometimes a sub-project (its own project(): extra generators then
        # write a project file for it too).
        sub_project = f"project({d}p C)\n" if rng.random() < 0.3 else ""
        files[f"{d}/CMakeLists.txt"] = sub_project + "\n".join(gen.dirs[d]) + "\n"
    for d in ("", "sub1", "sub2"):
        files[f"{d}/inc/x.h" if d else "inc/x.h"] = "#define X 1\n"
    return files


def run_side(cmake, src, build, do_build, generator="Ninja"):
    log = build.parent / f"{build.name}.log"
    # The file-API replies (codemodel: targets, source groups, include
    # paths, install rules; cache; cmakeFiles; toolchains) are compared too.
    rc = treediff.configure(Path(cmake), src, build, [], log, generator, treediff.FILEAPI_QUERIES)
    out = log.read_bytes()
    norm = treediff.normalizer(build, Path(cmake))
    # Compare the configured tree before building: built binaries embed
    # each side's own build-tree paths.
    tree = treediff.gather(build, norm)
    built = None
    if do_build and rc == 0:
        proc = subprocess.run(["ninja", "-C", str(build), "-k", "0",
                               *(["-f", "build-Debug.ninja"] if generator == "Ninja Multi-Config" else [])],
                              capture_output=True)
        built = proc.returncode
    installed = None
    if do_build and rc == 0 and built == 0:
        # `cmake --install` runs this side's own install-script interpreter.
        prefix = build.parent / "inst"
        shutil.rmtree(prefix, ignore_errors=True)
        cfg = ["--config", "Debug"] if generator == "Ninja Multi-Config" else []
        proc = subprocess.run([str(cmake), "--install", str(build), "--prefix", str(prefix), *cfg],
                              capture_output=True)
        listing = []
        for path in sorted(prefix.rglob("*")) if prefix.exists() else []:
            rel = path.relative_to(prefix).as_posix()
            st = path.lstat()
            if path.is_symlink():
                listing.append(f"{rel} -> {os.readlink(path)}")
            elif path.is_dir():
                listing.append(f"{rel}/ {oct(st.st_mode & 0o777)}")
            else:
                data = path.read_bytes()
                # Binaries embed build paths; text files are compared exactly.
                text = b"\0" not in data[:4096]
                digest = norm(data).replace(str(prefix).encode(), b"<PREFIX>") if text else b"<binary>"
                listing.append(f"{rel} {oct(st.st_mode & 0o777)} {digest!r}")
        out_install = norm(proc.stdout + proc.stderr).replace(str(prefix).encode(), b"<PREFIX>")
        installed = (proc.returncode, out_install, "\n".join(listing))
        shutil.rmtree(prefix, ignore_errors=True)
    out = re.sub(rb"done \(\d+\.\d+s\)", b"done (<T>s)", norm(out).replace(str(src).encode(), b"<SRC>"))
    return rc, out, built, tree, installed


def check(args, index):
    files = make_case(args.seed, index, args.cxx)
    root = Path(tempfile.mkdtemp(prefix="fp-", dir=args.work))
    try:
        src = root / "src"
        for rel, text in files.items():
            path = src / rel
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(text)
        # Both sides use the same build directory path (one after the
        # other): extra generators name projects after its basename.
        results = []
        for cmake, side in ((args.oracle, "oracle"), (args.candidate, "candidate")):
            results.append(run_side(cmake, src, root / "b", args.build, args.generator))
            (root / "b").rename(root / side)
            (root / "b.log").rename(root / f"{side}.log")
        oracle, candidate = results
        if oracle == candidate:
            return None
        diffs = []
        if oracle[0] != candidate[0]:
            diffs.append(f"rc {oracle[0]} != {candidate[0]}")
        if oracle[1] != candidate[1]:
            diffs.append("configure output")
        if oracle[2] != candidate[2]:
            diffs.append(f"build {oracle[2]} != {candidate[2]}")
        if oracle[4] != candidate[4]:
            diffs.append("install")
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
    parser.add_argument("--generator", default="Ninja")
    parser.add_argument("--cxx", action="store_true", help="mix C++ sources and features in")
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
