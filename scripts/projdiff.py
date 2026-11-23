#!/usr/bin/env python3
"""Project-mode differential test: configure (and optionally build/test) a
project with the pinned Kitware CMake 4.1.0 oracle and with the candidate,
then compare stdout/stderr/exit code and the generated files byte-for-byte
after normalizing tool paths.

Each case is a directory under tests/projects/<area>/<case>/ containing a
CMakeLists.txt (and sources). Optional files in the case directory:
  args.txt     extra configure arguments, one per line
  generator.txt  the generator to configure with (default: Ninja)
  build.txt    if present, run `<make-program>` after configure and compare
               the build exit code and the set of produced files
  ctest.txt    if present, run the oracle's ctest in both trees and compare
               the pass/fail summary; its lines are extra ctest arguments
Generated files compared (when present on either side): every *.ninja file
(build.ninja and CMakeFiles/rules.ninja, plus the Ninja Multi-Config
build-<Config>.ninja, CMakeFiles/impl-<Config>.ninja and common.ninja),
CMakeCache.txt (with volatile entries dropped), CTestTestfile.cmake,
cmake_install.cmake (all directories), CMakeFiles/clean_additional.cmake,
compile_commands.json, AutogenInfo.json, AutoRcc_*_Info.json,
TargetDirectories.txt, InstallScripts.json, Android.mk-style `*.mk` exports, every file under
CMakeFiles/Export/ (install-tree export files), and extra generator project
files (*.cbp, *.workspace, *.project, .project, .cproject, .kateproject*,
*.sublime-project, .settings/*).

Usage: scripts/projdiff.py [--candidate PATH] [-j N] [-v] [--keep DIR] [cases...]
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

ROOT = Path(__file__).resolve().parents[1]
ORACLE = Path("/tmp/cmake-rust-validation.hSLgEZHR/kitware-reference/bin/cmake")
ORACLE_CTEST = ORACLE.parent / "ctest"
DATA_ROOT = os.path.realpath("/tmp/cmake-rust-validation.hSLgEZHR/kitware")
VOLATILE_CACHE = re.compile(
    r"^(CMAKE_COMMAND|CMAKE_CTEST_COMMAND|CMAKE_CPACK_COMMAND|CMAKE_ROOT|CMAKE_CACHEFILE_DIR"
    r"|CMAKE_PLATFORM_INFO_INITIALIZED)[:=]")


def normalize(text, work, binary):
    text = text.replace(str(work), "<WORK>")
    text = text.replace(str(binary.parent.parent), "<PREFIX>")
    text = text.replace(DATA_ROOT, "<ROOT>")
    text = text.replace(str(ORACLE.parent.parent), "<PREFIX>")
    # CMP0116 internal depfile names hash the absolute depfile path, which
    # differs between the two work directories.
    text = re.sub(r"CMakeFiles/d/[0-9a-f]{64}\.d", "CMakeFiles/d/<SHA256>.d", text)
    # try_compile scratch directories and generated test targets carry random
    # names (mkdtemp / cmSystemTools::RandomNumber).
    text = re.sub(r"TryCompile-[0-9A-Za-z]+", "TryCompile-<RAND>", text)
    text = re.sub(r"cmTC_[0-9a-f]+", "cmTC_<RAND>", text)
    return text


def gather(build_dir):
    files = {}
    if not build_dir.exists():
        return files
    for path in sorted(build_dir.rglob("*")):
        rel = path.relative_to(build_dir).as_posix()
        if "CMakeFiles/CMakeScratch" in rel or "/4.1.0/" in rel or rel.startswith("CMakeFiles/4.1.0"):
            continue
        name = path.name
        if not path.is_file():
            continue
        if (name.endswith(".ninja") or name in ("CMakeCache.txt", "CTestTestfile.cmake", "clean_additional.cmake",
                     "cmake_install.cmake", "compile_commands.json", "FortranDependInfo.json",
                     "AutogenInfo.json", "InstallScripts.json", "TargetDirectories.txt")
                or (name.startswith("AutoRcc_") and name.endswith("_Info.json"))
                or name.endswith(".mk") or rel.startswith("CMakeFiles/Export/")
                # Extra generator outputs (CodeBlocks, CodeLite, Eclipse CDT4,
                # Kate, Sublime Text 2).
                or name.endswith((".cbp", ".workspace", ".project", ".cproject", ".sublime-project"))
                or name.startswith(".kateproject") or rel.startswith(".settings/")):
            files[rel] = path.read_text(errors="replace")
    return files


def run_side(binary, case, work):
    src = work / "src"
    shutil.copytree(case, src, symlinks=True)
    build = work / "build"
    args = []
    args_file = case / "args.txt"
    if args_file.exists():
        args = [line for line in args_file.read_text().splitlines() if line.strip()]
    env = {
        "PATH": "/usr/bin:/bin",
        "HOME": str(work),
        "LANG": "C",
        "LC_ALL": "C",
        "CMAKE_RUST_DATA_ROOT": DATA_ROOT,
    }
    generator_file = case / "generator.txt"
    generator = generator_file.read_text().strip() if generator_file.exists() else "Ninja"
    proc = subprocess.run([str(binary), "-S", str(src), "-B", str(build), "-G", generator, *args],
                          cwd=work, env=env, capture_output=True, timeout=600)
    result = {
        "configure.exit": str(proc.returncode),
        "configure.stdout": re.sub(r"\(\d+\.\ds\)", "(Xs)", proc.stdout.decode(errors="replace")),
        "configure.stderr": proc.stderr.decode(errors="replace"),
    }
    for rel, text in gather(build).items():
        if rel == "CMakeCache.txt":
            text = "\n".join(line for line in text.splitlines() if not VOLATILE_CACHE.match(line))
            text = re.sub(r"# It was generated by CMake: .*", "# It was generated by CMake: <CMAKE>", text)
        result[f"file:{rel}"] = text
    if (case / "build.txt").exists() and proc.returncode == 0:
        b = subprocess.run(["ninja", "-C", str(build)], cwd=work, env=env, capture_output=True, timeout=1800)
        result["build.exit"] = str(b.returncode)
        produced = sorted(p.relative_to(build).as_posix() for p in build.rglob("*")
                          if p.is_file() and "CMakeFiles" not in p.parts and not p.name.startswith(".ninja"))
        result["build.outputs"] = "\n".join(produced)
        # Compare the actual module edges, not just the final binary names.
        for tdi in build.rglob("FortranDependInfo.json"):
            for path in tdi.parent.rglob("*"):
                scan_dep = path.suffix == ".d" and ("-pp." in path.name or path.name.endswith(".ddi.d"))
                if path.name in ("Fortran.dd", "FortranModules.json") or path.suffix == ".ddi" or scan_dep:
                    result[f"file:{path.relative_to(build)}"] = path.read_text()
    if (case / "ctest.txt").exists() and proc.returncode == 0:
        ctest_args = [line for line in (case / "ctest.txt").read_text().splitlines() if line.strip()]
        t = subprocess.run([str(ORACLE_CTEST), "--test-dir", str(build), *ctest_args], cwd=work, env=env,
                           capture_output=True, timeout=1800)
        summary = [line for line in t.stdout.decode(errors="replace").splitlines()
                   if "tests passed" in line or "Passed" in line or "Failed" in line]
        result["ctest"] = re.sub(r"\s+[\d.]+ sec", "", "\n".join(summary))
    return {key: normalize(value, work, binary) for key, value in result.items()}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--candidate", type=Path, default=ROOT / "target/release/cmake")
    parser.add_argument("-j", type=int, default=4)
    parser.add_argument("-v", action="store_true")
    parser.add_argument("cases", nargs="*")
    args = parser.parse_args()
    # Stage the candidate as <prefix>/bin/cmake (with the oracle's ctest and
    # cpack beside it) so tool paths normalize identically on both sides.
    stage = Path(tempfile.mkdtemp(prefix="projdiff.stage.")) / "bin"
    stage.mkdir(parents=True)
    shutil.copy2(args.candidate.resolve(), stage / "cmake")
    for tool in ("ctest", "cpack"):
        (stage / tool).symlink_to(ORACLE.parent / tool)
    share = stage.parent / "share" / "cmake-4.1"
    share.mkdir(parents=True)
    for sub in ("Modules", "Templates"):
        (share / sub).symlink_to(Path(DATA_ROOT) / sub)
    args.candidate = stage / "cmake"
    roots = [Path(c) for c in args.cases] or [ROOT / "tests/projects"]
    cases = []
    for root in roots:
        if (root / "CMakeLists.txt").exists():
            cases.append(root.resolve())
        else:
            cases += sorted(p.parent.resolve() for p in root.rglob("CMakeLists.txt")
                            if not any((q / "CMakeLists.txt").exists() for q in p.parent.parents
                                       if q in [r.resolve() for r in roots] and q != p.parent and False))
    # Keep only top-level case directories (skip nested subdirectory CMakeLists).
    tops = []
    for case in cases:
        if not any(case != other and case.is_relative_to(other) for other in cases):
            tops.append(case)

    def one(case):
        with tempfile.TemporaryDirectory(prefix="projdiff.o.") as a, tempfile.TemporaryDirectory(prefix="projdiff.c.") as b:
            wa, wb = Path(a).resolve(), Path(b).resolve()
            expected = run_side(ORACLE, case, wa)
            actual = run_side(args.candidate.resolve(), case, wb)
            # Normalize work dirs to the same placeholder already.
            return case, expected, actual

    failures = 0
    with concurrent.futures.ThreadPoolExecutor(args.j) as pool:
        for case, expected, actual in pool.map(one, tops):
            name = case.relative_to(ROOT) if case.is_relative_to(ROOT) else case
            keys = sorted(set(expected) | set(actual))
            bad = [k for k in keys if expected.get(k) != actual.get(k)]
            if not bad:
                print(f"PASS {name}")
                continue
            failures += 1
            print(f"FAIL {name} ({len(bad)} differing: {', '.join(bad[:6])}{'...' if len(bad) > 6 else ''})")
            if args.v:
                for key in bad:
                    print(f"  --- {key}")
                    diff = difflib.unified_diff((expected.get(key) or "<missing>").splitlines(),
                                                (actual.get(key) or "<missing>").splitlines(),
                                                "oracle", "candidate", lineterm="", n=1)
                    for line in list(diff)[:40]:
                        print("    " + line)
    print(f"{len(tops) - failures}/{len(tops)} passed")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
