#!/usr/bin/env python3
"""Capture oracle ground truth for src/generator/{target,custom}.rs.

Each case is a small project described as a model (directories, variables,
policies, targets with properties, source-file properties, custom commands).
The script renders the model to CMakeLists.txt files, runs the Kitware
oracle with the Ninja generator, and records:

  * per-directory values of the platform variables the port reads,
  * language tables (source extensions, output extension, linker preference),
  * artifact paths from $<TARGET_FILE>/$<TARGET_LINKER_FILE>/$<TARGET_SONAME_FILE>,
  * object files, external objects and the linker language from build.ninja,
  * custom-command COMMAND/DESC/outputs/inputs from build.ninja,
  * the oracle's error/warning messages.

The Rust tests rebuild the same model in memory and compare.

Usage: generator_target_corpus.py <oracle-cmake> <out-dir> [case...]
"""

import concurrent.futures
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile

PLATFORM_VARS = [
    "CMAKE_BUILD_TYPE",
    "CMAKE_EXECUTABLE_SUFFIX",
    "CMAKE_STATIC_LIBRARY_PREFIX",
    "CMAKE_STATIC_LIBRARY_SUFFIX",
    "CMAKE_SHARED_LIBRARY_PREFIX",
    "CMAKE_SHARED_LIBRARY_SUFFIX",
    "CMAKE_SHARED_MODULE_PREFIX",
    "CMAKE_SHARED_MODULE_SUFFIX",
    "CMAKE_IMPORT_LIBRARY_PREFIX",
    "CMAKE_IMPORT_LIBRARY_SUFFIX",
    "CMAKE_SHARED_LIBRARY_SONAME_FLAG",
    "CMAKE_SHARED_LIBRARY_SONAME_C_FLAG",
    "CMAKE_SHARED_LIBRARY_SONAME_CXX_FLAG",
    "CMAKE_PLATFORM_NO_VERSIONED_SONAME",
    "CMAKE_SHARED_LIBRARY_NAME_WITH_VERSION",
    "CMAKE_OBJECT_PATH_MAX",
    "CMAKE_MANGLE_OBJECT_FILE_NAMES",
    "CMAKE_C_OUTPUT_EXTENSION_REPLACE",
    "CMAKE_CXX_OUTPUT_EXTENSION_REPLACE",
    "CMAKE_C_LINKER_PREFERENCE_PROPAGATES",
    "CMAKE_CXX_LINKER_PREFERENCE_PROPAGATES",
    "CMAKE_CROSSCOMPILING",
    "EXECUTABLE_OUTPUT_PATH",
    "LIBRARY_OUTPUT_PATH",
    "CMAKE_CUDA_COMPILER_ID",
]
LANG_VARS = ["SOURCE_FILE_EXTENSIONS", "OUTPUT_EXTENSION", "LINKER_PREFERENCE", "IGNORE_EXTENSIONS"]
POLICIES = ["CMP0111", "CMP0115", "CMP0118", "CMP0163"]

DUMP = """
foreach(_gt_v IN LISTS _gt_vars)
  if(DEFINED ${_gt_v})
    file(APPEND "${_gt_dump}" "V\\t${_gt_dir}\\t${_gt_v}\\t${${_gt_v}}\\n")
  endif()
endforeach()
foreach(_gt_p IN ITEMS %s)
  cmake_policy(GET ${_gt_p} _gt_s)
  file(APPEND "${_gt_dump}" "P\\t${_gt_dir}\\t${_gt_p}\\t${_gt_s}\\n")
endforeach()
""" % " ".join(POLICIES)


def q(value):
    """Quote a CMake argument (literal, except ${CMAKE_COMMAND})."""
    if value == "${CMAKE_COMMAND}":
        return value
    return '"' + value.replace("\\", "\\\\").replace('"', '\\"').replace("$", "\\$") + '"'


KIND_ARGS = {
    "EXECUTABLE": None,
    "STATIC_LIBRARY": "STATIC",
    "SHARED_LIBRARY": "SHARED",
    "MODULE_LIBRARY": "MODULE",
    "OBJECT_LIBRARY": "OBJECT",
    "INTERFACE_LIBRARY": "INTERFACE",
    "UNKNOWN_LIBRARY": "UNKNOWN",
}

ARTIFACT_KINDS = {"EXECUTABLE", "STATIC_LIBRARY", "SHARED_LIBRARY", "MODULE_LIBRARY"}


def render_dir(case, path):
    d = case["dirs"][path]
    out = []
    if path == "":
        out.append("cmake_minimum_required(VERSION %s)" % case.get("min_version", "3.31"))
        out.append("project(p LANGUAGES %s)" % case.get("languages", "C CXX"))
    for name, value in d.get("policies", {}).items():
        out.append("cmake_policy(SET %s %s)" % (name, value))
    for name, value in d.get("vars", {}).items():
        out.append("set(%s %s)" % (name, q(value)))
    for t in case["targets"]:
        if t.get("dir", "") != path:
            continue
        kind = t["kind"]
        imported = " IMPORTED" if t.get("imported") else ""
        if t.get("global"):
            imported += " GLOBAL"
        if kind == "EXECUTABLE":
            out.append("add_executable(%s%s)" % (t["name"], imported))
        else:
            out.append("add_library(%s %s%s)" % (t["name"], KIND_ARGS[kind], imported))
        for prop, value in t.get("props", {}).items():
            out.append("set_property(TARGET %s PROPERTY %s %s)" % (t["name"], prop, q(value)))
        out.extend(t.get("cmake", []))
    for sp in case.get("source_props", []):
        if sp.get("dir", "") != path:
            continue
        files = " ".join(q(f) for f in sp["files"])
        props = " ".join("%s %s" % (k, q(v)) for k, v in sp["props"].items())
        out.append("set_source_files_properties(%s PROPERTIES %s)" % (files, props))
    for cc in case.get("custom", []):
        if cc.get("dir", "") != path:
            continue
        out.append(render_custom(cc))
    out.extend(d.get("cmake", []))
    for sub in d.get("subdirs", []):
        out.append("add_subdirectory(%s)" % sub)
    # Probe artifact paths.
    probes = []
    for t in case["targets"]:
        if t.get("dir", "") != path or t["kind"] not in ARTIFACT_KINDS or not t.get("probe", True):
            continue
        n = t["name"]
        lines = ["file=$<TARGET_FILE:%s>" % n]
        if t["kind"] != "EXECUTABLE" or t.get("props", {}).get("ENABLE_EXPORTS"):
            lines.append("linker=$<TARGET_LINKER_FILE:%s>" % n)
        if t["kind"] == "SHARED_LIBRARY":
            lines.append("soname=$<TARGET_SONAME_FILE:%s>" % n)
        probes.append((n, "\n".join(lines)))
    for n, content in probes:
        out.append('file(GENERATE OUTPUT "${CMAKE_BINARY_DIR}/_gt_probe/%s.txt" CONTENT %s)'
                   % (n, q(content).replace("\\$", "$")))
    out.append('set(_gt_dir %s)' % q(path))
    out.append(DUMP)
    return "\n".join(out) + "\n"


def render_custom(cc):
    args = ["add_custom_command("]
    if cc.get("target"):
        args.append("TARGET %s %s" % (cc["target"], cc.get("when", "POST_BUILD")))
    else:
        args.append("OUTPUT " + " ".join(q(o) for o in cc["outputs"]))
    for line in cc["commands"]:
        args.append("COMMAND " + " ".join(q(a) for a in line))
    for key, field in [("BYPRODUCTS", "byproducts"), ("DEPENDS", "depends")]:
        if cc.get(field):
            args.append(key + " " + " ".join(q(v) for v in cc[field]))
    if cc.get("main_dependency"):
        args.append("MAIN_DEPENDENCY " + q(cc["main_dependency"]))
    if cc.get("working_directory") is not None:
        args.append("WORKING_DIRECTORY " + q(cc["working_directory"]))
    if cc.get("comment") is not None:
        args.append("COMMENT " + q(cc["comment"]))
    if cc.get("depfile"):
        args.append("DEPFILE " + q(cc["depfile"]))
    if cc.get("verbatim"):
        args.append("VERBATIM")
    if cc.get("command_expand_lists"):
        args.append("COMMAND_EXPAND_LISTS")
    args.append(")")
    text = "\n  ".join(args)
    if not cc.get("target"):
        text += "\nadd_custom_target(%s ALL DEPENDS %s)" % (
            cc["driver"], " ".join(q(o) for o in cc["outputs"]))
    return text


def unescape_ninja(text):
    return re.sub(r"\$(.)", lambda m: m.group(1), text)


def split_ninja(text):
    parts, cur, i = [], "", 0
    while i < len(text):
        c = text[i]
        if c == "$" and i + 1 < len(text):
            cur += text[i:i + 2]
            i += 2
            continue
        if c == " ":
            if cur:
                parts.append(cur)
            cur = ""
        else:
            cur += c
        i += 1
    if cur:
        parts.append(cur)
    return parts


def parse_ninja(path):
    builds = []
    current = None
    with open(path) as f:
        lines = f.read().split("\n")
    for line in lines:
        if line.startswith("build "):
            body = line[len("build "):]
            # Split at the first unescaped ':'.
            i = 0
            while True:
                i = body.index(":", i)
                if body[i - 1] != "$":
                    break
                i += 1
            outs, rest = body[:i], body[i + 1:].strip()
            out_parts = split_ninja(outs)
            explicit_outs = []
            for p in out_parts:
                if p == "|":
                    break
                explicit_outs.append(unescape_ninja(p))
            parts = split_ninja(rest)
            rule = parts[0]
            inputs, implicit, order = [], [], []
            mode = inputs
            for p in parts[1:]:
                if p == "|":
                    mode = implicit
                elif p == "||":
                    mode = order
                else:
                    mode.append(unescape_ninja(p))
            current = {"outputs": explicit_outs, "rule": rule, "inputs": inputs,
                       "implicit": implicit, "order": order, "vars": {}}
            builds.append(current)
        elif line.startswith("  ") and current is not None and " = " in line:
            k, v = line.strip().split(" = ", 1)
            current["vars"][k] = v
        elif not line.startswith("  "):
            current = None
    return builds


def run_case(oracle, root_base, case):
    root = os.path.join(root_base, case["name"])
    if os.path.exists(root):
        shutil.rmtree(root)
    src = os.path.join(root, "src")
    build = os.path.join(root, "build")
    os.makedirs(src)
    for f in case.get("files", []):
        p = os.path.join(root, f)
        os.makedirs(os.path.dirname(p), exist_ok=True)
        open(p, "w").close()
    langs = case.get("languages", "C CXX").split()
    var_names = list(PLATFORM_VARS)
    for l in langs:
        var_names += ["CMAKE_%s_%s" % (l, v) for v in LANG_VARS]
    for d in case["dirs"].values():
        var_names += [v for v in d.get("vars", {}) if v not in var_names]
    var_names += [v for v in case.get("extra_vars", []) if v not in var_names]
    dump = os.path.join(root, "dump.txt")
    for path in case["dirs"]:
        d = os.path.join(src, path)
        os.makedirs(d, exist_ok=True)
        text = render_dir(case, path)
        if path == "":
            text = ('set(_gt_dump %s)\nset(_gt_vars %s)\n' % (q(dump), q(";".join(var_names)))) + text
        with open(os.path.join(d, "CMakeLists.txt"), "w") as f:
            f.write(text)
    cmd = [oracle, "-S", src, "-B", build, "-G", "Ninja",
           "-DCMAKE_BUILD_TYPE=%s" % case.get("config", "Debug")]
    proc = subprocess.run(cmd, capture_output=True, text=True)
    result = {"exit": proc.returncode, "stderr": proc.stderr.replace(root, "<ROOT>").replace(oracle, "<CMAKE>")}
    dirs = {p: {"vars": {}, "policies": {}} for p in case["dirs"]}
    if os.path.exists(dump):
        for line in open(dump).read().split("\n"):
            if not line:
                continue
            kind, d, name, value = line.split("\t", 3)
            value = value.replace(root, "<ROOT>")
            (dirs[d]["vars"] if kind == "V" else dirs[d]["policies"])[name] = value
    result["dirs"] = dirs
    probes = {}
    probe_dir = os.path.join(build, "_gt_probe")
    if os.path.isdir(probe_dir):
        for name in sorted(os.listdir(probe_dir)):
            entry = {}
            for line in open(os.path.join(probe_dir, name)).read().split("\n"):
                if "=" in line:
                    k, v = line.split("=", 1)
                    entry[k] = v.replace(root, "<ROOT>")
            probes[name[:-4]] = entry
    result["artifacts"] = probes
    ninja = os.path.join(build, "build.ninja")
    targets = {}
    customs = []
    if proc.returncode == 0 and os.path.exists(ninja):
        def absol(p, base=build):
            if p.startswith("${cmake_ninja_workdir}"):
                p = p[len("${cmake_ninja_workdir}"):]
            full = p if os.path.isabs(p) else os.path.normpath(os.path.join(base, p))
            return full.replace(root, "<ROOT>").replace(oracle, "<CMAKE>")
        for b in parse_ninja(ninja):
            m = re.match(r"^(\w+)_COMPILER__(.+)_unscanned_(\w*)$", b["rule"])
            if m:
                t = targets.setdefault(m.group(2), {"objects": [], "externals": []})
                t["objects"].append({"object": absol(b["outputs"][0]), "source": absol(b["inputs"][0]),
                                     "language": m.group(1)})
                continue
            m = re.match(r"^(\w+)_(STATIC_LIBRARY|SHARED_LIBRARY|MODULE_LIBRARY|EXECUTABLE)_LINKER__(.+)_(\w*)$",
                         b["rule"])
            if m:
                t = targets.setdefault(m.group(3), {"objects": [], "externals": []})
                t["linker_language"] = m.group(1)
                t["link_output"] = absol(b["outputs"][0])
                objs = {o["object"] for o in t["objects"]}
                t["externals"] = [absol(i) for i in b["inputs"] if absol(i) not in objs]
                continue
            if b["rule"] == "CUSTOM_COMMAND" and "COMMAND" in b["vars"] and not b["outputs"][0].endswith(".util"):
                customs.append({
                    "outputs": [absol(o) for o in b["outputs"]],
                    "inputs": [absol(i) for i in b["inputs"]],
                    "command": b["vars"]["COMMAND"].replace(root, "<ROOT>").replace(oracle, "<CMAKE>"),
                    "desc": b["vars"].get("DESC", "").replace(root, "<ROOT>"),
                })
    result["targets"] = targets
    result["custom"] = customs
    return {"oracle": "Kitware CMake 4.1.0, Linux, Ninja", "root_len": len(root), "case": case, "expected": result}


# `rust_props` are properties the configure-time target model is expected to
# hold that the CMake code above does not set directly (initialization from
# CMAKE_<PROP> variables, file-set projections).
def T(name, kind, dir="", **kw):
    t = {"name": name, "kind": kind, "dir": dir}
    t.update(kw)
    return t


def case(name, targets, files, dirs=None, **kw):
    c = {"name": name, "targets": targets, "files": files, "dirs": dirs or {"": {}}}
    c.update(kw)
    return c


EXE, STA, SHA, MOD, OBJ, IFC, UNK = ("EXECUTABLE", "STATIC_LIBRARY", "SHARED_LIBRARY", "MODULE_LIBRARY",
                                     "OBJECT_LIBRARY", "INTERFACE_LIBRARY", "UNKNOWN_LIBRARY")


def cases():
    C = []
    a = ["src/a.c"]
    C.append(case("basic_exe", [T("app", EXE, props={"SOURCES": "a.c"})], a))
    C.append(case("basic_static", [T("lib", STA, props={"SOURCES": "a.c"})], a))
    C.append(case("basic_shared", [T("lib", SHA, props={"SOURCES": "a.c"})], a))
    C.append(case("basic_module", [T("plug", MOD, props={"SOURCES": "a.c"})], a))
    C.append(case("shared_version_soversion",
                  [T("v", SHA, props={"SOURCES": "a.c", "VERSION": "1.2.3", "SOVERSION": "1"})], a))
    C.append(case("shared_version_only", [T("v", SHA, props={"SOURCES": "a.c", "VERSION": "2.0"})], a))
    C.append(case("shared_soversion_only", [T("v", SHA, props={"SOURCES": "a.c", "SOVERSION": "5"})], a))
    C.append(case("shared_no_soname",
                  [T("v", SHA, props={"SOURCES": "a.c", "VERSION": "3.4", "SOVERSION": "3", "NO_SONAME": "ON"})], a))
    C.append(case("module_version", [T("m", MOD, props={"SOURCES": "a.c", "VERSION": "1.0", "SOVERSION": "1"})], a))
    C.append(case("exe_version", [T("tool", EXE, props={"SOURCES": "a.c", "VERSION": "1.5"})], a))
    C.append(case("output_name",
                  [T("a1", EXE, props={"SOURCES": "a.c", "OUTPUT_NAME": "renamed"}),
                   T("a2", STA, props={"SOURCES": "a.c", "OUTPUT_NAME": "core"}),
                   T("a3", SHA, props={"SOURCES": "a.c", "OUTPUT_NAME": "shr", "VERSION": "1"})], a))
    C.append(case("output_name_config",
                  [T("a1", EXE, props={"SOURCES": "a.c", "OUTPUT_NAME": "x", "OUTPUT_NAME_DEBUG": "dbg"}),
                   T("a2", STA, props={"SOURCES": "a.c", "OUTPUT_NAME": "x", "DEBUG_OUTPUT_NAME": "dbg2"}),
                   T("a3", STA, props={"SOURCES": "a.c", "OUTPUT_NAME_DEBUG": "one", "DEBUG_OUTPUT_NAME": "two"})],
                  a))
    C.append(case("kind_output_name",
                  [T("r", EXE, props={"SOURCES": "a.c", "RUNTIME_OUTPUT_NAME": "run", "OUTPUT_NAME": "no"}),
                   T("l", SHA, props={"SOURCES": "a.c", "LIBRARY_OUTPUT_NAME": "lib", "ARCHIVE_OUTPUT_NAME": "no"}),
                   T("s", STA, props={"SOURCES": "a.c", "ARCHIVE_OUTPUT_NAME": "arc",
                                      "ARCHIVE_OUTPUT_NAME_DEBUG": "arcd", "OUTPUT_NAME_DEBUG": "no"}),
                   T("m", MOD, props={"SOURCES": "a.c", "RUNTIME_OUTPUT_NAME": "no", "LIBRARY_OUTPUT_NAME": "mod"})],
                  a))
    C.append(case("prefix_suffix",
                  [T("e", EXE, props={"SOURCES": "a.c", "PREFIX": "pre_", "SUFFIX": ".run"}),
                   T("s", STA, props={"SOURCES": "a.c", "PREFIX": "", "SUFFIX": ".lib"}),
                   T("d", SHA, props={"SOURCES": "a.c", "PREFIX": "x", "SUFFIX": ".plugin", "VERSION": "2",
                                      "IMPORT_PREFIX": "imp", "IMPORT_SUFFIX": ".imp"})], a))
    C.append(case("var_prefix_suffix",
                  [T("e", EXE, props={"SOURCES": "a.c"}), T("s", STA, props={"SOURCES": "a.c"}),
                   T("d", SHA, props={"SOURCES": "a.c"}), T("m", MOD, props={"SOURCES": "a.c"})], a,
                  dirs={"": {"vars": {"CMAKE_EXECUTABLE_SUFFIX": ".exe", "CMAKE_STATIC_LIBRARY_PREFIX": "S",
                                      "CMAKE_STATIC_LIBRARY_SUFFIX": ".sa", "CMAKE_SHARED_LIBRARY_PREFIX": "D",
                                      "CMAKE_SHARED_LIBRARY_SUFFIX": ".dso", "CMAKE_SHARED_MODULE_PREFIX": "",
                                      "CMAKE_SHARED_MODULE_SUFFIX": ".mod"}}}))
    C.append(case("lang_prefix_suffix",
                  [T("c", STA, props={"SOURCES": "a.c"}), T("x", STA, props={"SOURCES": "b.cpp"}),
                   T("y", SHA, props={"SOURCES": "b.cpp"})], ["src/a.c", "src/b.cpp"],
                  dirs={"": {"vars": {"CMAKE_STATIC_LIBRARY_PREFIX_CXX": "cxx", "CMAKE_STATIC_LIBRARY_SUFFIX_CXX": ".cxa",
                                      "CMAKE_SHARED_LIBRARY_SUFFIX_CXX": "", "CMAKE_SHARED_LIBRARY_PREFIX_C": "nope"}}}))
    C.append(case("postfix",
                  [T("s", STA, props={"SOURCES": "a.c", "DEBUG_POSTFIX": "_d"}),
                   T("e", EXE, props={"SOURCES": "a.c", "DEBUG_POSTFIX": "-dbg"}),
                   T("d", SHA, props={"SOURCES": "a.c", "DEBUG_POSTFIX": "d", "VERSION": "1.0", "SOVERSION": "1",
                                      "OUTPUT_NAME": "dyn"}),
                   T("r", STA, props={"SOURCES": "a.c", "RELEASE_POSTFIX": "_r"}),
                   T("f", STA, props={"SOURCES": "a.c", "DEBUG_POSTFIX": "_d",
                                      "FRAMEWORK_MULTI_CONFIG_POSTFIX_DEBUG": "_fw"})], a))
    C.append(case("postfix_variable",
                  [T("s", STA, props={"SOURCES": "a.c"}, rust_props={"DEBUG_POSTFIX": "_dbg"}),
                   T("e", EXE, props={"SOURCES": "a.c"})], a,
                  dirs={"": {"vars": {"CMAKE_DEBUG_POSTFIX": "_dbg"}}}))
    C.append(case("name_with_version",
                  [T("d", SHA, props={"SOURCES": "a.c", "SOVERSION": "4", "VERSION": "4.1"}),
                   T("n", SHA, props={"SOURCES": "a.c"})], a,
                  dirs={"": {"vars": {"CMAKE_SHARED_LIBRARY_NAME_WITH_VERSION": "ON"}}}))
    C.append(case("no_versioned_soname",
                  [T("d", SHA, props={"SOURCES": "a.c", "SOVERSION": "4", "VERSION": "4.1"})], a,
                  dirs={"": {"vars": {"CMAKE_PLATFORM_NO_VERSIONED_SONAME": "ON"}}}))
    C.append(case("output_dirs",
                  [T("e", EXE, props={"SOURCES": "a.c", "RUNTIME_OUTPUT_DIRECTORY": "bin"}),
                   T("s", STA, props={"SOURCES": "a.c", "ARCHIVE_OUTPUT_DIRECTORY": "arch/x"}),
                   T("d", SHA, props={"SOURCES": "a.c", "LIBRARY_OUTPUT_DIRECTORY": "../lib", "VERSION": "1",
                                      "RUNTIME_OUTPUT_DIRECTORY": "no", "ARCHIVE_OUTPUT_DIRECTORY": "no2"}),
                   T("m", MOD, props={"SOURCES": "a.c", "LIBRARY_OUTPUT_DIRECTORY": "plugins/"})], a))
    C.append(case("output_dirs_config",
                  [T("e", EXE, props={"SOURCES": "a.c", "RUNTIME_OUTPUT_DIRECTORY": "bin",
                                      "RUNTIME_OUTPUT_DIRECTORY_DEBUG": "bin-debug"}),
                   T("s", STA, props={"SOURCES": "a.c", "ARCHIVE_OUTPUT_DIRECTORY_DEBUG": "dbg-arch"}),
                   T("r", STA, props={"SOURCES": "a.c", "ARCHIVE_OUTPUT_DIRECTORY_RELEASE": "rel"})], a))
    C.append(case("output_dirs_variables",
                  [T("e", EXE, props={"SOURCES": "a.c"},
                     rust_props={"RUNTIME_OUTPUT_DIRECTORY": "<ROOT>/build/out/bin"}),
                   T("s", STA, props={"SOURCES": "a.c"}, rust_props={"ARCHIVE_OUTPUT_DIRECTORY": "out/arch"})], a,
                  dirs={"": {"vars": {"CMAKE_RUNTIME_OUTPUT_DIRECTORY": "<ROOT>/build/out/bin",
                                      "CMAKE_ARCHIVE_OUTPUT_DIRECTORY": "out/arch"}}}))
    C.append(case("legacy_output_paths",
                  [T("e", EXE, props={"SOURCES": "a.c"}), T("s", STA, props={"SOURCES": "a.c"}),
                   T("d", SHA, props={"SOURCES": "a.c"}),
                   T("o", EXE, props={"SOURCES": "a.c", "RUNTIME_OUTPUT_DIRECTORY": "override"})], a,
                  dirs={"": {"vars": {"EXECUTABLE_OUTPUT_PATH": "exes", "LIBRARY_OUTPUT_PATH": "<ROOT>/build/libs"}}}))
    C.append(case("abs_output_dir",
                  [T("e", EXE, props={"SOURCES": "a.c", "RUNTIME_OUTPUT_DIRECTORY": "<ROOT>/elsewhere/./bin/.."})], a))
    C.append(case("subdirs",
                  [T("top", EXE, props={"SOURCES": "a.c", "LINK_LIBRARIES": "sublib"}),
                   T("sublib", STA, "sub", props={"SOURCES": "s.c;../a.c", "ARCHIVE_OUTPUT_DIRECTORY": "out"}),
                   T("deep", SHA, "sub/deeper", props={"SOURCES": "d.c", "VERSION": "1.1"})],
                  ["src/a.c", "src/sub/s.c", "src/sub/deeper/d.c"],
                  dirs={"": {"subdirs": ["sub"]}, "sub": {"subdirs": ["deeper"],
                                                          "vars": {"LIBRARY_OUTPUT_PATH": "sublibs"}},
                        "sub/deeper": {}}))
    C.append(case("object_names_paths",
                  [T("t", STA, props={"SOURCES": "dir/x.c;dir/sub/y.c;../outside.c;<ROOT>/elsewhere/abs.c;"
                                                 "<ROOT>/build/inbin.c;with space.c;a+b.c"})],
                  ["src/dir/x.c", "src/dir/sub/y.c", "outside.c", "elsewhere/abs.c", "build/inbin.c",
                   "src/with space.c", "src/a+b.c"]))
    C.append(case("object_names_subdir_sources",
                  [T("t", STA, "sub", props={"SOURCES": "../a.c;../other/o.c;local.c;<ROOT>/build/gen.c;"
                                                       "<ROOT>/build/sub/bgen.c"})],
                  ["src/a.c", "src/other/o.c", "src/sub/local.c", "build/gen.c", "build/sub/bgen.c"],
                  dirs={"": {"subdirs": ["sub"]}, "sub": {}}))
    C.append(case("object_names_generated",
                  [T("t", STA, props={"SOURCES": "gen/g.c;plain.c"})],
                  ["src/plain.c"], source_props=[{"files": ["gen/g.c"], "props": {"GENERATED": "ON"}}]))
    long_dir = "src/" + "/".join(["d%02d" % i + "x" * 20 for i in range(6)])
    C.append(case("object_path_max",
                  [T("long_target_name", STA, props={"SOURCES": long_dir[4:] + "/long_source_file_name.c;a.c"})],
                  ["src/a.c", long_dir + "/long_source_file_name.c"],
                  dirs={"": {"vars": {"CMAKE_OBJECT_PATH_MAX": "200"}}}))
    C.append(case("object_path_max_invalid",
                  [T("t", STA, props={"SOURCES": "a.c"})], a,
                  dirs={"": {"vars": {"CMAKE_OBJECT_PATH_MAX": "abc"}}}))
    C.append(case("object_path_max_small",
                  [T("t", STA, props={"SOURCES": "a.c"})], a,
                  dirs={"": {"vars": {"CMAKE_OBJECT_PATH_MAX": "100"}}}))
    C.append(case("object_ext_replace",
                  [T("t", STA, props={"SOURCES": "a.c;b.cpp;dir/c.c"})], ["src/a.c", "src/b.cpp", "src/dir/c.c"],
                  dirs={"": {"vars": {"CMAKE_C_OUTPUT_EXTENSION_REPLACE": "1"}}}))
    C.append(case("keep_extension",
                  [T("t", STA, props={"SOURCES": "a.c;b.c"})], ["src/a.c", "src/b.c"],
                  source_props=[{"files": ["a.c"], "props": {"KEEP_EXTENSION": "ON"}}]))
    C.append(case("language_property",
                  [T("t", STA, props={"SOURCES": "data.txt;noext;b.cpp"})], ["src/data.txt", "src/noext", "src/b.cpp"],
                  source_props=[{"files": ["data.txt"], "props": {"LANGUAGE": "C"}},
                                {"files": ["b.cpp"], "props": {"LANGUAGE": "C"}}]))
    C.append(case("header_file_only",
                  [T("t", STA, props={"SOURCES": "a.c;h.c;x.h;y.hpp;z.inl;t.txx;w.in"})],
                  ["src/a.c", "src/h.c", "src/x.h", "src/y.hpp", "src/z.inl", "src/t.txx", "src/w.in"],
                  source_props=[{"files": ["h.c"], "props": {"HEADER_FILE_ONLY": "ON"}}]))
    C.append(case("external_objects",
                  [T("e", EXE, props={"SOURCES": "a.c;x.o;y.obj;z.lo;w.bin;v.c"})],
                  ["src/a.c", "src/x.o", "src/y.obj", "src/z.lo", "src/w.bin", "src/v.c"],
                  source_props=[{"files": ["w.bin", "v.c"], "props": {"EXTERNAL_OBJECT": "ON"}}]))
    C.append(case("linker_language_mixed",
                  [T("e", EXE, props={"SOURCES": "a.c;b.cpp"}), T("c", EXE, props={"SOURCES": "a.c"}),
                   T("x", SHA, props={"SOURCES": "b.cpp"})], ["src/a.c", "src/b.cpp"]))
    C.append(case("linker_language_property",
                  [T("e", EXE, props={"SOURCES": "b.cpp", "LINKER_LANGUAGE": "C"}),
                   T("s", STA, props={"SOURCES": "a.c", "LINKER_LANGUAGE": "CXX"}),
                   T("h", EXE, props={"SOURCES": "a.c", "HAS_CXX": "1"})], ["src/a.c", "src/b.cpp"]))
    C.append(case("linker_language_static_propagates",
                  [T("e", EXE, props={"SOURCES": "a.c", "LINK_LIBRARIES": "cxxlib"}),
                   T("cxxlib", STA, props={"SOURCES": "b.cpp"})], ["src/a.c", "src/b.cpp"]))
    C.append(case("linker_language_shared_no_propagate",
                  [T("e", EXE, props={"SOURCES": "a.c", "LINK_LIBRARIES": "cxxlib"}),
                   T("cxxlib", SHA, props={"SOURCES": "b.cpp"})], ["src/a.c", "src/b.cpp"]))
    C.append(case("linker_language_transitive",
                  [T("e", EXE, props={"SOURCES": "a.c", "LINK_LIBRARIES": "mid"}),
                   T("mid", STA, props={"SOURCES": "a.c", "INTERFACE_LINK_LIBRARIES": "leaf"}),
                   T("leaf", STA, props={"SOURCES": "b.cpp"}),
                   T("viashared", EXE, props={"SOURCES": "a.c", "LINK_LIBRARIES": "shr"}),
                   T("shr", SHA, props={"SOURCES": "a.c", "INTERFACE_LINK_LIBRARIES": "leaf",
                                        "LINK_LIBRARIES": "leaf"})], ["src/a.c", "src/b.cpp"]))
    C.append(case("linker_language_no_propagate_var",
                  [T("e", EXE, props={"SOURCES": "a.c", "LINK_LIBRARIES": "cxxlib"}),
                   T("cxxlib", STA, props={"SOURCES": "b.cpp"})], ["src/a.c", "src/b.cpp"],
                  dirs={"": {"vars": {"CMAKE_CXX_LINKER_PREFERENCE_PROPAGATES": "0"}}}))
    C.append(case("object_library_link",
                  [T("e", EXE, props={"SOURCES": "a.c", "LINK_LIBRARIES": "objs"}),
                   T("objs", OBJ, props={"SOURCES": "b.cpp;o/c.c"}),
                   T("s", SHA, props={"SOURCES": "a.c", "LINK_LIBRARIES": "objs;e2"}),
                   T("e2", STA, props={"SOURCES": "a.c", "INTERFACE_LINK_LIBRARIES": "objs"})],
                  ["src/a.c", "src/b.cpp", "src/o/c.c"]))
    C.append(case("object_library_subdir",
                  [T("e", EXE, props={"SOURCES": "a.c", "LINK_LIBRARIES": "objs"}),
                   T("objs", OBJ, "sub", props={"SOURCES": "x.c"})],
                  ["src/a.c", "src/sub/x.c"], dirs={"": {"subdirs": ["sub"]}, "sub": {}}))
    imp = {"IMPORTED_LOCATION_DEBUG": "<ROOT>/imp/libd.a", "IMPORTED_LOCATION_RELEASE": "<ROOT>/imp/libr.a",
           "IMPORTED_LOCATION": "<ROOT>/imp/libg.a"}
    C.append(case("imported_location",
                  [T("dep", STA, imported=True, props=dict(imp)),
                   T("gen", STA, imported=True, props={"IMPORTED_LOCATION": "<ROOT>/imp/libg.a"}),
                   T("e", EXE, props={"SOURCES": "a.c", "LINK_LIBRARIES": "dep;gen"})], a))
    C.append(case("imported_map_config",
                  [T("dep", STA, imported=True, props=dict(imp, MAP_IMPORTED_CONFIG_DEBUG="missing;Release")),
                   T("e", EXE, props={"SOURCES": "a.c", "LINK_LIBRARIES": "dep"})], a))
    C.append(case("imported_configurations",
                  [T("dep", STA, imported=True, props={"IMPORTED_CONFIGURATIONS": "RELEASE;MINSIZEREL",
                                                       "IMPORTED_LOCATION_RELEASE": "<ROOT>/imp/libr.a",
                                                       "IMPORTED_LOCATION_MINSIZEREL": "<ROOT>/imp/libm.a"}),
                   T("e", EXE, props={"SOURCES": "a.c", "LINK_LIBRARIES": "dep"})], a))
    C.append(case("imported_release_config",
                  [T("dep", STA, imported=True, props=dict(imp)),
                   T("e", EXE, props={"SOURCES": "a.c", "LINK_LIBRARIES": "dep", "RELEASE_POSTFIX": "_r"})], a,
                  config="Release"))
    C.append(case("imported_noconfig",
                  [T("dep", STA, imported=True, props={"IMPORTED_LOCATION_NOCONFIG": "<ROOT>/imp/libn.a",
                                                       "IMPORTED_LOCATION_DEBUG": "<ROOT>/imp/libd.a"}),
                   T("e", EXE, props={"SOURCES": "a.c", "LINK_LIBRARIES": "dep", "DEBUG_POSTFIX": "_d"})], a,
                  config=""))
    C.append(case("imported_shared",
                  [T("sh", SHA, imported=True, props={"IMPORTED_LOCATION": "<ROOT>/imp/libsh.so.1",
                                                      "IMPORTED_SONAME": "libsh.so.1"}),
                   T("im", SHA, imported=True, props={"IMPORTED_IMPLIB": "<ROOT>/imp/libim.so"}),
                   T("e", EXE, props={"SOURCES": "a.c", "LINK_LIBRARIES": "sh;im"})], a))
    C.append(case("imported_missing_location",
                  [T("miss", STA, imported=True, probe=False),
                   T("e", EXE, props={"SOURCES": "a.c", "LINK_LIBRARIES": "miss"})], a))
    C.append(case("imported_missing_shared",
                  [T("miss", SHA, imported=True, probe=False,
                     props={"IMPORTED_LOCATION_RELEASE": "<ROOT>/x.so", "MAP_IMPORTED_CONFIG_DEBUG": "NONE"}),
                   T("e", EXE, props={"SOURCES": "a.c", "LINK_LIBRARIES": "miss"})], a))
    C.append(case("imported_link_languages",
                  [T("dep", STA, imported=True, props={"IMPORTED_LOCATION": "<ROOT>/imp/libx.a",
                                                       "IMPORTED_LINK_INTERFACE_LANGUAGES_DEBUG": "CXX"}),
                   T("e", EXE, props={"SOURCES": "a.c", "LINK_LIBRARIES": "dep"})], a))
    C.append(case("interface_sources",
                  [T("iface", IFC, props={"INTERFACE_SOURCES": "<ROOT>/src/i.c;<ROOT>/src/i.h"}),
                   T("e", EXE, props={"SOURCES": "a.c", "LINK_LIBRARIES": "iface"}),
                   T("s", STA, props={"SOURCES": "a.c;<ROOT>/src/i.c", "LINK_LIBRARIES": "iface"})],
                  ["src/a.c", "src/i.c", "src/i.h"]))
    C.append(case("file_set_headers",
                  [T("t", STA, props={"SOURCES": "a.c"},
                     cmake=["target_sources(t PUBLIC FILE_SET HEADERS FILES inc/h.h inc/c.c)"],
                     rust_props={"HEADER_SETS": "HEADERS", "HEADER_SET": "inc/h.h;inc/c.c"})],
                  ["src/a.c", "src/inc/h.h", "src/inc/c.c"]))
    C.append(case("missing_source_new", [T("t", STA, probe=False, props={"SOURCES": "a.c;missing.c"})], a))
    C.append(case("missing_source_old_ext",
                  [T("t", STA, props={"SOURCES": "a;b"})], ["src/a.c", "src/b.cpp"],
                  dirs={"": {"policies": {"CMP0115": "OLD"}}}))
    C.append(case("no_linker_language",
                  [T("t", EXE, probe=False, props={"SOURCES": "x.h"})], ["src/x.h"]))
    C.append(case("utility_and_interface",
                  [T("i", IFC, props={"SOURCES": "a.c"}),
                   T("e", EXE, props={"SOURCES": "a.c"})], a))
    # CMAKE_MANGLE_OBJECT_FILE_NAMES collisions depend on cmSourceFile pointer
    # order in cmLocalCommonGenerator::ComputeObjectFilenames; not captured.
    C.append(case("imported_global_visibility",
                  [T("g", STA, "sub", imported=True, props={"IMPORTED_LOCATION": "<ROOT>/imp/libg.a"},
                     **{"global": True}),
                   T("e", EXE, props={"SOURCES": "a.c", "LINK_LIBRARIES": "g"})],
                  a, dirs={"": {"subdirs": ["sub"]}, "sub": {}}))
    C.append(case("exe_enable_exports",
                  [T("e", EXE, props={"SOURCES": "a.c", "ENABLE_EXPORTS": "ON", "VERSION": "2"})], a))
    C.append(case("relwithdebinfo_config",
                  [T("s", STA, props={"SOURCES": "a.c", "RELWITHDEBINFO_POSTFIX": "_rd",
                                      "ARCHIVE_OUTPUT_DIRECTORY_RELWITHDEBINFO": "rd",
                                      "OUTPUT_NAME_RELWITHDEBINFO": "named"})], a, config="RelWithDebInfo"))

    # Custom commands.
    C.append(case("custom_basic", [], ["src/in.txt"],
                  custom=[{"outputs": ["out.txt"], "commands": [["${CMAKE_COMMAND}", "-E", "copy", "in.txt", "out.txt"]],
                           "depends": ["in.txt"], "verbatim": True, "driver": "drv"}]))
    C.append(case("custom_no_verbatim", [], [],
                  custom=[{"outputs": ["o1"], "commands": [["echo", "a b", "c\"d", "$x", "e;f", "#g"]],
                           "driver": "drv"}]))
    C.append(case("custom_verbatim_escaping", [], [],
                  custom=[{"outputs": ["o1", "sub/o2"],
                           "commands": [["echo", "a b", "c\"d", "$x", "e&f", "#g", "", "'q'", "x>y"],
                                        ["touch", "o1"]],
                           "verbatim": True, "comment": "Doing stuff", "driver": "drv"}]))
    C.append(case("custom_target_executable",
                  [T("gen", EXE, props={"SOURCES": "a.c", "RUNTIME_OUTPUT_DIRECTORY": "bin", "OUTPUT_NAME": "g2"}),
                   T("lib", SHA, props={"SOURCES": "a.c", "VERSION": "1.0"})], a,
                  custom=[{"outputs": ["gen.out"], "commands": [["gen", "arg"], ["lib", "x"]],
                           "depends": ["gen", "lib", "a.c", "nonexist.txt"], "verbatim": True, "driver": "drv"}]))
    C.append(case("custom_working_directory", [], ["src/in.txt"],
                  custom=[{"outputs": ["w1"], "commands": [["touch", "w1"]], "working_directory": "work/dir",
                           "verbatim": True, "driver": "d1"},
                          {"outputs": ["<ROOT>/build/w2"], "commands": [["touch", "w2"]],
                           "working_directory": "<ROOT>/abs", "byproducts": ["by.txt"], "verbatim": True,
                           "driver": "d2"}]))
    C.append(case("custom_emulator",
                  [T("tool", EXE, props={"SOURCES": "a.c", "CROSSCOMPILING_EMULATOR": "qemu-arm;-L;/sysroot"})], a,
                  dirs={"": {"cmake": ["set(CMAKE_CROSSCOMPILING ON)"]}},
                  extra_vars=["CMAKE_CROSSCOMPILING"],
                  custom=[{"outputs": ["emu.out"], "commands": [["tool", "--go"]], "verbatim": True,
                           "driver": "drv"}]))
    C.append(case("custom_depends_resolution", [], ["src/exists.txt", "build/inbin.txt"],
                  source_props=[{"files": ["gen.h"], "props": {"GENERATED": "ON"}}],
                  custom=[{"outputs": ["dep.out"], "commands": [["touch", "dep.out"]],
                           "depends": ["exists.txt", "inbin.txt", "gen.h", "<ROOT>/abs/file.txt", "sub/../x.txt"],
                           "verbatim": True, "driver": "drv"}]))
    C.append(case("custom_main_dependency", [], ["src/m.in"],
                  custom=[{"outputs": ["m.out"], "commands": [["cp", "m.in", "m.out"]], "main_dependency": "m.in",
                           "comment": "", "verbatim": True, "driver": "drv"}]))
    C.append(case("custom_subdir", [], ["src/sub/in.txt"],
                  dirs={"": {"subdirs": ["sub"]}, "sub": {}},
                  custom=[{"dir": "sub", "outputs": ["s.out", "../up.out"], "commands": [["touch", "s.out"]],
                           "depends": ["in.txt"], "verbatim": True, "driver": "drv"}]))
    C.append(case("custom_expand_lists", [], [],
                  custom=[{"outputs": ["l.out"], "commands": [["echo", "a;b;c", "d"]], "command_expand_lists": True,
                           "verbatim": True, "driver": "drv"},
                          {"outputs": ["l2.out"], "commands": [["echo", "a;b;c", "d"]], "verbatim": True,
                           "driver": "drv2"}]))
    return C


def main():
    oracle, out_dir = sys.argv[1], sys.argv[2]
    wanted = set(sys.argv[3:])
    all_cases = [c for c in cases() if not wanted or c["name"] in wanted]
    root_base = tempfile.mkdtemp(prefix="gt-corpus-")

    def materialize(c):
        root = os.path.join(root_base, c["name"])
        text = json.dumps(c).replace("<ROOT>", root)
        return json.loads(text)

    os.makedirs(out_dir, exist_ok=True)
    with concurrent.futures.ThreadPoolExecutor(max_workers=6) as pool:
        futures = {pool.submit(run_case, oracle, root_base, materialize(c)): c for c in all_cases}
        for future in concurrent.futures.as_completed(futures):
            c = futures[future]
            result = future.result()
            result["case"] = c
            path = os.path.join(out_dir, c["name"] + ".json")
            with open(path, "w") as f:
                json.dump(result, f, indent=1, sort_keys=True)
                f.write("\n")
            print(c["name"], "exit", result["expected"]["exit"])
    shutil.rmtree(root_base)


if __name__ == "__main__":
    main()
