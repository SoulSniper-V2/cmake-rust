#!/usr/bin/env python3
"""Differential harness for `cmake -E cmake_autogen` / `cmake_autorcc`.

Runs the upstream reference oracle and the Rust candidate through
identical build-time AUTOGEN runs with fake Qt tools and diffs every
observable result: exit codes, stdout/stderr (path-normalized), fake-tool
argv logs, and all generated files (mocs_compilation*, include/*, dep
files, ParseCache.txt, *Used.txt settings files, wrapper files, lock
files).  Standard library only.

Authoritative info files are produced by configuring the scenario
projects with the reference oracle (Qt is not installed, so fake
moc/uic/rcc shell scripts stand in).  The oracle is only used to
generate inputs and as a separately labeled reference; the candidate
never invokes it.

Usage: autogendiff.py [--oracle CMAKE] [--candidate CMAKE]
                      [--work DIR] [-v] [scenario...]
"""

import argparse
import difflib
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
ORACLE = os.environ.get(
    "AUTOGEN_ORACLE",
    "/tmp/cmake-rust-validation.hSLgEZHR/kitware-reference/bin/cmake",
)

# ---------------------------------------------------------------------------
# Fake Qt tools.  They log every invocation to $AUTOGEN_FAKE_LOG, honor -o,
# and fake moc writes a depfile when --output-dep-file is passed.
# ---------------------------------------------------------------------------

FAKE_MOC = """\
#!/bin/sh
echo "FAKEMOC $@" >&2
[ -n "$AUTOGEN_FAKE_LOG" ] && echo "moc $@" >> "$AUTOGEN_FAKE_LOG"
out=""
prev=""
for a in "$@"; do
  if [ "$prev" = "-o" ]; then out="$a"; fi
  prev="$a"
done
deps=0
for a in "$@"; do
  [ "$a" = "--output-dep-file" ] && deps=1
done
if [ -n "$out" ]; then
  mkdir -p "$(dirname "$out")"
  echo "// fake moc output" > "$out"
  if [ "$deps" = 1 ]; then
    src="${@: -1}"
    echo "$out: $src" > "$out.d"
  fi
fi
exit 0
"""

# Same, but POSIX sh has no ${@: -1}; use a portable loop.
FAKE_MOC = """\
#!/bin/sh
echo "FAKEMOC $@" >&2
[ -n "$AUTOGEN_FAKE_LOG" ] && echo "moc $@" >> "$AUTOGEN_FAKE_LOG"
out=""
prev=""
deps=0
for a in "$@"; do
  if [ "$prev" = "-o" ]; then out="$a"; fi
  [ "$a" = "--output-dep-file" ] && deps=1
  prev="$a"
  last="$a"
done
if [ -n "$out" ]; then
  mkdir -p "$(dirname "$out")"
  echo "// fake moc output" > "$out"
  if [ "$deps" = 1 ]; then
    echo "$out: $last" > "$out.d"
  fi
fi
exit 0
"""

FAKE_MOC_FAIL = """\
#!/bin/sh
echo "FAKEMOC $@" >&2
[ -n "$AUTOGEN_FAKE_LOG" ] && echo "moc $@" >> "$AUTOGEN_FAKE_LOG"
echo "fake moc failing as requested" >&2
exit 3
"""

FAKE_UIC = """\
#!/bin/sh
echo "FAKEUIC $@" >&2
[ -n "$AUTOGEN_FAKE_LOG" ] && echo "uic $@" >> "$AUTOGEN_FAKE_LOG"
out=""
prev=""
for a in "$@"; do
  if [ "$prev" = "-o" ]; then out="$a"; fi
  prev="$a"
done
if [ -n "$out" ]; then
  mkdir -p "$(dirname "$out")"
  echo "// fake uic output" > "$out"
fi
exit 0
"""

FAKE_UIC_FAIL = """\
#!/bin/sh
echo "FAKEUIC $@" >&2
[ -n "$AUTOGEN_FAKE_LOG" ] && echo "uic $@" >> "$AUTOGEN_FAKE_LOG"
echo "fake uic failing as requested" >&2
exit 4
"""

FAKE_RCC = """\
#!/bin/sh
echo "FAKERCC $@" >&2
[ -n "$AUTOGEN_FAKE_LOG" ] && echo "rcc $@" >> "$AUTOGEN_FAKE_LOG"
out=""
prev=""
list=0
src=""
for a in "$@"; do
  case "$a" in
    --list|--list-mapping) list=1 ;;
    *.qrc) src="$a" ;;
  esac
  if [ "$prev" = "-o" ]; then out="$a"; fi
  prev="$a"
done
if [ "$list" = 1 ] && [ -n "$src" ]; then
  grep -o '<file[^<]*' "$src" | sed 's/<file[^>]*>//' | grep -v '^$'
  exit 0
fi
if [ -n "$out" ]; then
  mkdir -p "$(dirname "$out")"
  echo "// fake rcc output" > "$out"
fi
exit 0
"""

FAKE_RCC_FAIL = """\
#!/bin/sh
echo "FAKERCC $@" >&2
[ -n "$AUTOGEN_FAKE_LOG" ] && echo "rcc $@" >> "$AUTOGEN_FAKE_LOG"
echo "fake rcc failing as requested" >&2
exit 5
"""

WIDGET_H = "class W { Q_OBJECT };\n"
WIDGET_H_GADGET = "struct G { Q_GADGET };\n"

CMAKE_TAIL = """\
set(QT_VERSION_MAJOR 6)
set(QT_VERSION_MINOR 3)
set_target_properties(t PROPERTIES
  AUTOMOC_EXECUTABLE {tools}/moc
  AUTOUIC_EXECUTABLE {tools}/uic
  AUTORCC_EXECUTABLE {tools}/rcc)
"""

QRC = """\
<RCC>
    <qresource prefix="/">
        <file>a.txt</file>
        <file>data/b.bin</file>
    </qresource>
</RCC>
"""

# ---------------------------------------------------------------------------
# Scenarios.  Each dict:
#   files:    relpath -> content written under <name>/src
#   cmake:    extra CMakeLists body (add_executable etc.)
#   props:    extra set_target_properties lines
#   gen:      generator name (default Ninja)
#   steps:    operations shared by oracle/candidate runs
#     ("autogen", config)             run cmake_autogen
#     ("autorcc", info-glob, config)  run cmake_autorcc on every match
#     ("touch", relpath)              bump mtime of src file
#     ("modify", relpath, text)       append text to src file
#   edit:     {info-glob: {key: value-or-None-to-delete}} applied to both
#             copies' info files after path rewrite
# ---------------------------------------------------------------------------

SCENARIOS = [
    {
        "name": "moc-header",
        "files": {"main.cpp": "int main() { return 0; }\n", "widget.h": WIDGET_H},
        "cmake": "add_executable(t main.cpp widget.h)\n",
        "props": "AUTOMOC ON AUTOUIC ON\n",
        "steps": [("autogen", ""), ("autogen", ""), ("touch", "widget.h"), ("autogen", "")],
    },
    {
        "name": "moc-includes",
        "files": {
            "main.cpp": 'class L { Q_OBJECT };\n#include "main.moc"\n'
            '#include "moc_widget.cpp"\nint main() { return 0; }\n',
            "widget.h": WIDGET_H,
        },
        "cmake": "add_executable(t main.cpp widget.h)\n",
        "props": "AUTOMOC ON\n",
        "steps": [("autogen", ""), ("autogen", ""), ("touch", "main.cpp"), ("autogen", "")],
    },
    {
        "name": "moc-relaxed",
        "files": {
            "main.cpp": 'class L { Q_OBJECT };\n#include "moc_main.cpp"\n'
            "int main() { return 0; }\n",
        },
        "cmake": "add_executable(t main.cpp)\n",
        "props": "AUTOMOC ON AUTOMOC_RELAXED_MODE ON\n",
        "steps": [("autogen", ""), ("autogen", "")],
    },
    {
        "name": "moc-missing-include",
        "files": {"main.cpp": "class L { Q_OBJECT };\nint main() { return 0; }\n"},
        "cmake": "add_executable(t main.cpp)\n",
        "props": "AUTOMOC ON\n",
        "steps": [("autogen", "")],
    },
    {
        "name": "moc-macro-names",
        "files": {
            "main.cpp": "struct X { MY_MACRO };\nint main() { return 0; }\n",
        },
        "cmake": "add_executable(t main.cpp)\n"
        'set_target_properties(t PROPERTIES AUTOMOC_MACRO_NAMES "MY_MACRO;OTHER_MACRO")\n',
        "props": "AUTOMOC ON\n",
        "steps": [("autogen", "")],
    },
    {
        "name": "moc-skip",
        "files": {"widget.h": WIDGET_H, "main.cpp": "int main() { return 0; }\n"},
        "cmake": "add_executable(t main.cpp widget.h)\n"
        'set_source_files_properties(widget.h PROPERTIES SKIP_AUTOMOC ON)\n',
        "props": "AUTOMOC ON\n",
        "steps": [("autogen", "")],
    },
    {
        "name": "moc-qt5-depfilters",
        "files": {
            "plugin.cpp": 'class P { Q_OBJECT\nQ_PLUGIN_METADATA(IID "x" FILE "meta.json") };\n'
            '#include "plugin.moc"\nint plugin() { return 0; }\n',
            "meta.json": "{}\n",
            "main.cpp": "int main() { return plugin(); }\n",
        },
        "cmake": "add_executable(t main.cpp plugin.cpp)\nset(QT_VERSION_MAJOR 5)\n"
        "set(QT_VERSION_MINOR 14)\n",
        "props": "AUTOMOC ON\n",
        "steps": [("autogen", ""), ("autogen", ""), ("touch", "meta.json"), ("autogen", "")],
    },
    {
        "name": "moc-tool-fail",
        "files": {"widget.h": WIDGET_H, "main.cpp": "int main() { return 0; }\n"},
        "cmake": "add_executable(t main.cpp widget.h)\n",
        "props": "AUTOMOC ON\n",
        "fail_moc": True,
        "steps": [("autogen", "")],
    },
    {
        "name": "uic-basic",
        "files": {
            "main.cpp": '#include "ui_form.h"\nint main() { return 0; }\n',
            "form.ui": "<ui><class>Form</class></ui>\n",
        },
        "cmake": "add_executable(t main.cpp)\n",
        "props": "AUTOUIC ON\n",
        "steps": [("autogen", ""), ("autogen", ""), ("touch", "form.ui"), ("autogen", "")],
    },
    {
        "name": "uic-search-paths",
        "files": {
            "main.cpp": '#include "ui_form.h"\nint main() { return 0; }\n',
            "uis/form.ui": "<ui><class>Form</class></ui>\n",
        },
        "cmake": "add_executable(t main.cpp)\n"
        'set_target_properties(t PROPERTIES AUTOUIC_SEARCH_PATHS "${CMAKE_CURRENT_SOURCE_DIR}/uis")\n',
        "props": "AUTOUIC ON\n",
        "steps": [("autogen", "")],
    },
    {
        "name": "uic-missing",
        "files": {"main.cpp": '#include "ui_nothere.h"\nint main() { return 0; }\n'},
        "cmake": "add_executable(t main.cpp)\n",
        "props": "AUTOUIC ON\n",
        "steps": [("autogen", "")],
    },
    {
        "name": "uic-tool-fail",
        "files": {
            "main.cpp": '#include "ui_form.h"\nint main() { return 0; }\n',
            "form.ui": "<ui><class>Form</class></ui>\n",
        },
        "cmake": "add_executable(t main.cpp)\n",
        "props": "AUTOUIC ON\n",
        "fail_uic": True,
        "steps": [("autogen", "")],
    },
    {
        "name": "rcc-basic",
        "files": {
            "res.qrc": QRC,
            "a.txt": "hello\n",
            "data/b.bin": "00\n",
            "main.cpp": "int main() { return 0; }\n",
        },
        "cmake": "add_executable(t main.cpp res.qrc)\n",
        "props": 'AUTORCC ON AUTORCC_OPTIONS "-name;res"\n',
        "steps": [("autorcc", "AutoRcc_*_Info.json", ""), ("autorcc", "AutoRcc_*_Info.json", "")],
    },
    {
        "name": "rcc-list-options",
        "files": {
            "res.qrc": QRC,
            "a.txt": "hello\n",
            "data/b.bin": "00\n",
            "main.cpp": "int main() { return 0; }\n",
        },
        "cmake": "add_executable(t main.cpp res.qrc)\n",
        "props": "AUTORCC ON\n",
        "edit": {"AutoRcc_*_Info.json": {"RCC_LIST_OPTIONS": ["--list"]}},
        "steps": [("autorcc", "AutoRcc_*_Info.json", ""), ("autorcc", "AutoRcc_*_Info.json", "")],
    },
    {
        "name": "rcc-tool-fail",
        "files": {
            "res.qrc": QRC,
            "a.txt": "hello\n",
            "main.cpp": "int main() { return 0; }\n",
        },
        "cmake": "add_executable(t main.cpp res.qrc)\n",
        "props": "AUTORCC ON\n",
        "fail_rcc": True,
        "steps": [("autorcc", "AutoRcc_*_Info.json", "")],
    },
    {
        "name": "mc-moc-rcc",
        "files": {
            "res.qrc": QRC,
            "a.txt": "hello\n",
            "main.cpp": '#include "moc_widget.cpp"\nint main() { return 0; }\n',
            "widget.h": WIDGET_H,
        },
        "cmake": "add_executable(t main.cpp widget.h res.qrc)\n",
        "props": "AUTOMOC ON AUTORCC ON\n",
        "gen": "Ninja Multi-Config",
        "steps": [
            ("autogen", "Debug"),
            ("autorcc", "AutoRcc_*_Info.json", "Debug"),
            ("autogen", "Release"),
            ("autorcc", "AutoRcc_*_Info.json", "Release"),
            ("autogen", "Debug"),
            ("autorcc", "AutoRcc_*_Info.json", "Debug"),
        ],
    },
    {
        "name": "bad-info-missing-key",
        "files": {"main.cpp": "int main() { return 0; }\n"},
        "cmake": "add_executable(t main.cpp)\n",
        "props": "AUTOMOC ON\n",
        "edit": {"AutogenInfo.json": {"MOC_COMPILATION_FILE": None}},
        "steps": [("autogen", "")],
    },
]


def write_tools(tools_dir, fail_moc=False, fail_uic=False, fail_rcc=False):
    tools_dir.mkdir(parents=True, exist_ok=True)
    scripts = {
        "moc": FAKE_MOC_FAIL if fail_moc else FAKE_MOC,
        "uic": FAKE_UIC_FAIL if fail_uic else FAKE_UIC,
        "rcc": FAKE_RCC_FAIL if fail_rcc else FAKE_RCC,
    }
    for name, text in scripts.items():
        path = tools_dir / name
        path.write_text(text)
        path.chmod(0o755)


def configure_oracle(oracle, src_dir, build_dir, generator):
    build_dir.mkdir(parents=True, exist_ok=True)
    args = [
        oracle,
        "-S",
        str(src_dir),
        "-B",
        str(build_dir),
        "-G",
        generator,
        "-DCMAKE_BUILD_TYPE=Debug",
    ]
    if generator == "Ninja Multi-Config":
        args.append("-DCMAKE_CONFIGURATION_TYPES=Debug;Release")
    proc = subprocess.run(args, capture_output=True, text=True, timeout=120)
    if proc.returncode != 0:
        raise RuntimeError(f"oracle configure failed:\n{proc.stdout}\n{proc.stderr}")


def normalize(text, rewrites):
    for old, new in rewrites:
        text = text.replace(str(old), new)
    # Settings files embed sha256 hashes over absolute paths, which differ
    # legitimately between the oracle/candidate copies (verified against
    # upstream's formula separately); normalize the digest itself.
    return re.sub(r"\b[0-9a-f]{64}\b", "SHA256", text)


def collect_tree(root):
    out = {}
    for path in sorted(root.rglob("*")):
        if path.is_file():
            rel = path.relative_to(root).as_posix()
            out[rel] = path.read_bytes()
    return out


class Failure(Exception):
    pass


def run_scenario(s, oracle, candidate, work, verbose):
    name = s["name"]
    oroot = work / name
    src = oroot / "src"
    tools = oroot / "tools"
    src.mkdir(parents=True, exist_ok=True)
    for rel, content in s["files"].items():
        path = src / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content)
    write_tools(
        tools,
        fail_moc=s.get("fail_moc"),
        fail_uic=s.get("fail_uic"),
        fail_rcc=s.get("fail_rcc"),
    )
    (src / "CMakeLists.txt").write_text(
        "cmake_minimum_required(VERSION 3.16)\n"
        "project(%s CXX)\n" % name.replace("-", "_")
        + s["cmake"]
        + CMAKE_TAIL.format(tools=tools)
        + "set_target_properties(t PROPERTIES\n  " + s.get("props", "").strip() + ")\n"
    )
    gen_build = oroot / "gen"
    configure_oracle(oracle, src, gen_build, s.get("gen", "Ninja"))

    # Authoritative info files from the oracle-configured tree.
    info_files = sorted(
        set(
            [
                *gen_build.rglob("*_Info.json"),
                *gen_build.rglob("AutogenInfo.json"),
            ]
        )
    )
    if not info_files:
        raise Failure("no info files produced by oracle configure")

    # Prepare two identical runnable copies.
    copies = {}
    for variant in ("oracle", "cand"):
        vroot = oroot / variant
        shutil.copytree(src, vroot / "src")
        for info in info_files:
            rel = info.relative_to(gen_build)
            target = vroot / "build" / rel
            target.parent.mkdir(parents=True, exist_ok=True)
            data = json.loads(info.read_text())

            def rewrite(value):
                if isinstance(value, str):
                    return value.replace(
                        str(oroot / "gen"), str(vroot / "build")
                    ).replace(str(src), str(vroot / "src"))
                if isinstance(value, list):
                    return [rewrite(v) for v in value]
                if isinstance(value, dict):
                    return {k: rewrite(v) for k, v in value.items()}
                return value

            data = rewrite(data)
            # Pin the upstream worker pool to one thread.  With several
            # workers the push order of compile jobs is deterministic but
            # their *observable* order (tool output, argv log) races;
            # upstream itself is not run-to-run deterministic there.
            # The candidate port is sequential, so this exercises the same
            # logic while keeping both sides deterministic.
            if "PARALLEL" in data:
                data["PARALLEL"] = 1
            # Keep the oracle cmake binary as CMAKE_EXECUTABLE; it exists
            # on both sides and its file time only gates cache refresh.
            target.write_text(json.dumps(data, indent=2))
        copies[variant] = vroot

    # Apply scenario info edits identically to both copies.
    for glob_pat, patch in s.get("edit", {}).items():
        for vroot in copies.values():
            for target in sorted((vroot / "build").rglob(glob_pat)):
                data = json.loads(target.read_text())
                for key, value in patch.items():
                    if value is None:
                        data.pop(key, None)
                    else:
                        data[key] = value
                target.write_text(json.dumps(data, indent=2))

    rewrites = [
        (copies["oracle"], "ROOT"),
        (copies["cand"], "ROOT"),
        (oracle, "ORACLE_CMAKE"),
        (candidate, "CAND_CMAKE"),
        (work, "WORK"),
    ]
    binaries = {"oracle": oracle, "cand": candidate}
    outputs = {}

    for variant in ("oracle", "cand"):
        vroot = copies[variant]
        tool = binaries[variant]
        transcript = []
        for i, step in enumerate(s["steps"]):
            if step[0] in ("touch", "modify"):
                path = vroot / "src" / step[1]
                if step[0] == "modify":
                    with open(path, "a") as f:
                        f.write(step[2])
                else:
                    bump = time.time() + 0.2
                    os.utime(path, (bump, bump))
                transcript.append(f"# {step[0]} {step[1]}")
                continue
            if step[0] == "autogen":
                infos = sorted((vroot / "build").rglob("AutogenInfo.json"))
                command = "cmake_autogen"
                config = step[1]
            else:
                infos = sorted((vroot / "build").rglob(step[1]))
                command = "cmake_autorcc"
                config = step[2]
            for info in infos:
                log = vroot / f"toollog-{i}-{info.name}.txt"
                log.unlink(missing_ok=True)
                env = dict(os.environ, AUTOGEN_FAKE_LOG=str(log))
                proc = subprocess.run(
                    [tool, "-E", command, str(info), config],
                    capture_output=True,
                    text=True,
                    env=env,
                    timeout=60,
                )
                argv_log = log.read_text() if log.exists() else ""
                transcript.append(
                    f"=== {command} {info.name} cfg={config!r} "
                    f"rc={proc.returncode}\n"
                    f"--- argv\n{argv_log}"
                    f"--- stdout\n{proc.stdout}"
                    f"--- stderr\n{proc.stderr}"
                )
        outputs[variant] = "\n".join(transcript)

    # Diff transcripts.
    oracle_out = normalize(outputs["oracle"], rewrites)
    cand_out = normalize(outputs["cand"], rewrites)
    if oracle_out != cand_out:
        diff = "\n".join(
            difflib.unified_diff(
                oracle_out.splitlines(),
                cand_out.splitlines(),
                "oracle-transcript",
                "cand-transcript",
                lineterm="",
            )
        )
        raise Failure(f"transcript mismatch\n{diff}")

    # Diff generated trees (build dir only; src is input).
    oracle_tree = {
        k: normalize(v.decode("utf-8", "replace"), rewrites)
        for k, v in collect_tree(copies["oracle"] / "build").items()
    }
    cand_tree = {
        k: normalize(v.decode("utf-8", "replace"), rewrites)
        for k, v in collect_tree(copies["cand"] / "build").items()
    }
    if set(oracle_tree) != set(cand_tree):
        raise Failure(
            "tree file list mismatch\n"
            f"only oracle: {sorted(set(oracle_tree) - set(cand_tree))}\n"
            f"only cand:   {sorted(set(cand_tree) - set(oracle_tree))}"
        )
    for rel in oracle_tree:
        if oracle_tree[rel] != cand_tree[rel]:
            diff = "\n".join(
                difflib.unified_diff(
                    oracle_tree[rel].splitlines(),
                    cand_tree[rel].splitlines(),
                    f"oracle:{rel}",
                    f"cand:{rel}",
                    lineterm="",
                )
            )
            raise Failure(f"file {rel} mismatch\n{diff}")
    if verbose:
        print(f"  {name}: ok")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--oracle", default=ORACLE)
    parser.add_argument(
        "--candidate", default=str(ROOT / "target" / "release" / "cmake")
    )
    parser.add_argument("--work", default=None)
    parser.add_argument("-v", "--verbose", action="store_true")
    parser.add_argument("scenarios", nargs="*")
    args = parser.parse_args()

    if not Path(args.candidate).exists():
        debug = ROOT / "target" / "debug" / "cmake"
        if debug.exists():
            args.candidate = str(debug)
        else:
            sys.exit(f"candidate {args.candidate} not found")

    names = set(args.scenarios)
    selected = [s for s in SCENARIOS if not names or s["name"] in names]
    if not selected:
        sys.exit(f"unknown scenario(s): {sorted(names)}")

    if args.work:
        work = Path(args.work)
        shutil.rmtree(work, ignore_errors=True)
        work.mkdir(parents=True)
        cleanup = False
    else:
        work = Path(tempfile.mkdtemp(prefix="autogendiff-"))
        cleanup = True

    passed = failed = 0
    try:
        for s in selected:
            try:
                run_scenario(s, args.oracle, args.candidate, work, args.verbose)
                print(f"PASS {s['name']}")
                passed += 1
            except Failure as e:
                print(f"FAIL {s['name']}\n{e}")
                failed += 1
            except Exception as e:  # noqa: BLE001 - report and continue
                print(f"FAIL {s['name']} (harness error)\n{e}")
                failed += 1
    finally:
        if cleanup:
            shutil.rmtree(work, ignore_errors=True)
    print(f"{passed}/{passed + failed} scenarios passed")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
