#!/usr/bin/env python3
"""Capture flag, rule, and configuration inputs from Kitware CMake 4.1.0."""
import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile

ORACLE = "/tmp/cmake-rust-validation.hSLgEZHR/kitware-reference/bin/cmake"
OUT = Path(__file__).resolve().with_name("oracle.json")
FLAGS = {"FLAGS", "DEFINES", "INCLUDES", "LAUNCHER", "LINK_FLAGS",
         "LANGUAGE_COMPILE_FLAGS", "ARCH_FLAGS", "DEP_FILE"}


def cases():
    for language, standards in [("C", ("90", "99", "11", "17", "23")),
                                ("CXX", ("98", "11", "14", "17", "20", "23", "26"))]:
        for standard in standards:
            for extensions in ("ON", "OFF"):
                yield dict(name=f"{language}-{standard}-extensions-{extensions}", language=language,
                           target={f"{language}_STANDARD": standard,
                                   f"{language}_EXTENSIONS": extensions})
        for old in (False, True):
            yield dict(name=f"{language}-extensions-only-{old}", language=language,
                       policies={"CMP0128": "OLD"} if old else {},
                       target={f"{language}_EXTENSIONS": "OFF"})
            yield dict(name=f"{language}-standard-default-{old}", language=language,
                       policies={"CMP0128": "OLD"} if old else {},
                       target={f"{language}_STANDARD": "17"})
        yield dict(name=f"{language}-required-default", language=language,
                   target={f"{language}_STANDARD": "17", f"{language}_STANDARD_REQUIRED": "ON"})
    for language, features in [("C", ["c_std_23", "c_restrict", "c_std_17"]),
                               ("CXX", ["cxx_std_20", "cxx_variadic_templates", "cxx_std_23"])]:
        for feature in features:
            yield dict(name=f"feature-{feature}", language=language,
                       target={f"{language}_STANDARD": "11", "COMPILE_FEATURES": feature})
    for language in ("C", "CXX"):
        for config in ("", "Debug", "Release", "RelWithDebInfo"):
            for kind in ("EXECUTABLE", "STATIC_LIBRARY", "SHARED_LIBRARY", "MODULE_LIBRARY"):
                yield dict(name=f"{language}-{kind}-{config or 'empty'}", language=language,
                           kind=kind, config=config, variables={f"CMAKE_{language}_FLAGS": "-fno-common"},
                           target={"POSITION_INDEPENDENT_CODE": "ON"} if kind == "EXECUTABLE" else {})
    for kind in ("EXECUTABLE", "SHARED_LIBRARY", "MODULE_LIBRARY"):
        key = kind.replace("_LIBRARY", "") if kind != "EXECUTABLE" else "EXE"
        yield dict(name=f"link-flags-{kind}", kind=kind, config="Debug",
                   variables={f"CMAKE_{key}_LINKER_FLAGS": "LINKER:-z,relro",
                              f"CMAKE_{key}_LINKER_FLAGS_DEBUG": "LINKER:SHELL:-z now",
                              "CMAKE_C_LINK_FLAGS": "LINKER:--export-dynamic"},
                   target={"LINK_FLAGS": "-legacy", "LINK_FLAGS_DEBUG": "-cfg",
                           "LINK_OPTIONS": '-first;-first;LINKER:--as-needed;SHELL:-x "a b";LINKER:SHELL:-u xyz'})
    yield dict(name="static-archiver", kind="STATIC_LIBRARY", config="Release",
               variables={"CMAKE_STATIC_LINKER_FLAGS": "-global",
                          "CMAKE_STATIC_LINKER_FLAGS_RELEASE": "-global-release",
                          "CMAKE_C_ARCHIVER_WRAPPER_FLAG": "--arch=",
                          "CMAKE_C_ARCHIVER_WRAPPER_FLAG_SEP": ","},
               target={"STATIC_LIBRARY_FLAGS": "-local",
                       "STATIC_LIBRARY_FLAGS_RELEASE": "-local-release",
                       "STATIC_LIBRARY_OPTIONS": "ARCHIVER:one,two;SHELL:-s --extra;-s;-s"})
    for pic in ("ON", "OFF"):
        for old in (False, True):
            yield dict(name=f"pie-{pic}-old-{old}", policies={"CMP0083": "OLD"} if old else {},
                       variables={"CMAKE_C_LINK_PIE_SUPPORTED": "YES", "CMAKE_C_LINK_NO_PIE_SUPPORTED": "YES"},
                       target={"POSITION_INDEPENDENT_CODE": pic})
    for preset in ("hidden", "internal", "default", "protected"):
        yield dict(name=f"visibility-{preset}", language="CXX",
                   target={"CXX_VISIBILITY_PRESET": preset, "VISIBILITY_INLINES_HIDDEN": "ON"})
    yield dict(name="visibility-c-disabled-pic", kind="MODULE_LIBRARY",
               target={"C_VISIBILITY_PRESET": "hidden", "POSITION_INDEPENDENT_CODE": "OFF",
                       "DEFINE_SYMBOL": "MODULE_SYMBOL"})
    yield dict(name="compile-options", target={"COMPILE_OPTIONS":
               '-Wextra;-Wextra;SHELL:-x "a b";SHELL:-x "a b";SHELL:-x xyz;-x;LINKER:-z,now;-DRAW=123',
               "COMPILE_FLAGS": "-Wno-unused -DFROM_RAW", "COMPILE_WARNING_AS_ERROR": "ON"},
               source={"COMPILE_OPTIONS": "-Wextra;-Wextra;SHELL:-x raw", "COMPILE_FLAGS": "-DSOURCE_RAW"})
    yield dict(name="flag-regex", variables={"CMAKE_C_FLAG_REGEX": "^-D"},
               target={"COMPILE_OPTIONS": 'SHELL:-Da -b "-Dvalue=x y";-Wall;-DOPT',
                       "COMPILE_FLAGS": '-Wall "/Ddiscard=1" "-DRAW=two words" -DNEXT'})
    yield dict(name="defines-special", kind="SHARED_LIBRARY", target_name="3my-lib",
               config="Debug", target={"COMPILE_DEFINITIONS":
               'Z=last;-DA=two words;B=x=y;C=;D="str";E=$literal;F=semi\\;colon;FN(x)=bad;HASH=a#b;A=two words',
               "COMPILE_DEFINITIONS_DEBUG": "IGNORED"},
               source={"COMPILE_DEFINITIONS": "A=two words;S=source;D=dup",
                       "COMPILE_DEFINITIONS_DEBUG": "CFG=debug;D=dup"})
    yield dict(name="define-flag-different", variables={"CMAKE_C_DEFINE_FLAG": "--define="},
               target={"COMPILE_DEFINITIONS": "-DFIRST;SECOND=two words"})
    yield dict(name="directory-definitions",
               commands='add_definitions(-DFIRST=1 /DSECOND -Wextra)\nadd_compile_definitions(THIRD=3)',
               target={"COMPILE_DEFINITIONS": "FIRST=1;SECOND;THIRD=3"}, directory={"DEFINE_FLAGS": " -Wextra"})
    for mode in ("system", "separator", "project-before", "quote", "directory-before"):
        variables = {"CMAKE_INCLUDE_CURRENT_DIR": "ON", "CMAKE_C_STANDARD_INCLUDE_DIRECTORIES": "/standard/first;/standard/last"}
        dirs = "/external;@SRC@/one;/usr/include;@SRC@/with space;/standard/first;@SRC@/one;/sysroot/usr/include"
        variables["CMAKE_C_IMPLICIT_INCLUDE_DIRECTORIES"] = "/sysroot/usr/include;/usr/include"
        target = {"INCLUDE_DIRECTORIES": dirs, "SYSTEM_INCLUDE_DIRECTORIES": "/external;@SRC@/with space"}
        commands = ""
        if mode == "system":
            variables["CMAKE_INCLUDE_SYSTEM_FLAG_C_WARNING"] = "-Wsystem-headers"
        if mode == "separator":
            variables["CMAKE_INCLUDE_FLAG_C"] = "--includes="
            variables["CMAKE_INCLUDE_FLAG_SEP_C"] = ":"
        if mode == "quote":
            variables["CMAKE_QUOTE_INCLUDE_PATHS"] = "0"
        if mode == "project-before":
            target["SYSTEM_INCLUDE_DIRECTORIES"] = "@SRC@/with space"
            variables["CMAKE_INCLUDE_DIRECTORIES_PROJECT_BEFORE"] = "ON"
        if mode == "directory-before":
            variables["CMAKE_INCLUDE_DIRECTORIES_BEFORE"] = "ON"
            commands = "include_directories(/first)\ninclude_directories(/second)\ninclude_directories(AFTER /last)"
            target = {"INCLUDE_DIRECTORIES": "/second;/first;/last"}
        yield dict(name=f"includes-{mode}", variables=variables, target=target, commands=commands,
                   source={"INCLUDE_DIRECTORIES": "@SRC@/at source;@SRC@/at source;/usr/include;@SRC@/one",
                           "OBJECT_DEPENDS": "@SRC@/dummy/../generated.h;relative.h"})
    yield dict(name="compiler-and-linker-launchers",
               variables={"CMAKE_C_COMPILER_ARG1": "-some value",
                          "CMAKE_C_COMPILER_TARGET": "custom-triple", "CMAKE_C_COMPILE_OPTIONS_TARGET": "--target=",
                          "CMAKE_C_COMPILER_EXTERNAL_TOOLCHAIN": "/compiler path",
                          "CMAKE_C_COMPILE_OPTIONS_EXTERNAL_TOOLCHAIN": "--gcc-toolchain=",
                          "CMAKE_C_COMPILE_OPTIONS_SYSROOT": "--sysroot=",
                          "CMAKE_SYSROOT_COMPILE": "/compile root", "CMAKE_SYSROOT_LINK": "/link root"},
               target={"C_COMPILER_LAUNCHER": "/compiler launcher;--option;two words;",
                       "C_LINKER_LAUNCHER": "/linker launcher;one=two words"})
    yield dict(name="explicit-source-language", source={"LANGUAGE": "C"})
    yield dict(name="explicit-source-language-old", source={"LANGUAGE": "C"}, policies={"CMP0119": "OLD"})
    yield dict(name="linker-type", target={"LINKER_TYPE": "custom"},
               variables={"CMAKE_C_USING_LINKER_custom": "LINKER:-u,symbol;-fuse-ld=bfd"})
    for selection in ("auto", "off", "no-shared"):
        yield dict(name=f"link-depfile-{selection}",
                   variables={"CMAKE_C_LINK_DEPENDS_USE_LINKER": "ON",
                              "CMAKE_C_LINKER_DEPFILE_FORMAT": "gcc",
                              "CMAKE_C_LINKER_DEPFILE_FLAGS": "LINKER:--dependency-file=<DEP_FILE>",
                              **({"CMAKE_LINK_DEPENDS_USE_LINKER": "OFF"} if selection == "off" else {})},
                   target={"LINK_DEPENDS_NO_SHARED": "ON"} if selection == "no-shared" else {})
    yield dict(name="empty-build-type-source-definitions", source={"COMPILE_DEFINITIONS_": "EMPTY=1"})
    yield dict(name="flags-init-release", config="Release",
               before_project={"CMAKE_C_FLAGS_INIT": "-DFROM_INIT",
                               "CMAKE_C_FLAGS_RELEASE_INIT": "-DFROM_RELEASE_INIT"})
    yield dict(name="link-with-dialect", language="CXX",
               variables={"CMAKE_CXX_LINK_WITH_STANDARD_COMPILE_OPTION": "ON"},
               target={"CXX_STANDARD": "20", "CXX_EXTENSIONS": "OFF"})
    yield dict(name="link-variable-policy-old", policies={"CMP0181": "OLD"},
               variables={"CMAKE_EXE_LINKER_FLAGS": 'LINKER:-z,relro  "-Wl,with space"'},
               target={"LINK_FLAGS": "-literal=$word", "LINK_OPTIONS": "LINKER:-rpath,/somewhere"})
    yield dict(name="direct-linker-and-placeholders",
               variables={"CMAKE_C_LINK_MODE": "LINKER",
                          "CMAKE_C_USING_LINKER_custom": "/some linker",
                          "CMAKE_C_LINK_EXECUTABLE":
                              "<CMAKE_LINKER> <OBJECTS> -o <TARGET> <TARGET_QUOTED> "
                              "<TARGET_UNQUOTED> <TARGET_BASE> <TARGET_VERSION_MAJOR> "
                              "<TARGET_VERSION_MINOR> <TARGET_IMPLIB> <ROLE> <UNKNOWN>"},
               target={"LINKER_TYPE": "custom", "C_LINKER_LAUNCHER": "launcher;has space"})
    yield dict(name="link-warning-as-error", target={"LINK_WARNING_AS_ERROR": "ON"})
    yield dict(name="ipo", target={"INTERPROCEDURAL_OPTIMIZATION": "ON"})
    for global_value in ("", "MISSING-NOTFOUND"):
        yield dict(name=f"link-depfile-defined-{global_value or 'empty'}",
                   variables={"CMAKE_LINK_DEPENDS_USE_LINKER": global_value})



def bracket(value):
    return f"[====[{value}]====]"


def parse_ninja(path, prefix, attributes):
    result = {}
    current = None
    for line in path.read_text().splitlines():
        if line.startswith(prefix):
            name = line[len(prefix):]
            current = {"statement": name}
            result[name.split(":", 1)[0]] = current
        elif current is not None and line.startswith("  ") and " = " in line:
            attr, value = line.strip("\n").lstrip().split(" = ", 1)
            if attr in attributes:
                current[attr] = value
        else:
            current = None
    return result


def capture(oracle, root, number, case):
    root = root / f"{number:02d}"
    source = root / "source"
    build = root / "build"
    source.mkdir(parents=True)
    language = case.get("language", "C")
    extension = "cpp" if language == "CXX" else "c"
    source_file = f"main.{extension}"
    (source / source_file).write_text("int main(void) { return 0; }\n")
    kind = case.get("kind", "EXECUTABLE")
    name = case.get("target_name", "x")
    config = case.get("config", "")
    def replace_paths(mapping):
        return {key: value.replace("@SRC@", str(source)).replace("@BIN@", str(build))
                for key, value in mapping.items()}
    variables = replace_paths(case.get("variables", {}))
    target_props = replace_paths(case.get("target", {}))
    source_props = replace_paths(case.get("source", {}))
    policies = case.get("policies", {})
    lines = ["cmake_minimum_required(VERSION 4.1)"]
    before_project = replace_paths(case.get("before_project", {}))
    lines += [f"set({key} {bracket(value)})" for key, value in before_project.items()]
    lines.append(f"project(flags {language})")
    lines += [f"cmake_policy(SET {key} {value})" for key, value in policies.items()]
    lines += [f"set({key} {bracket(value)})" for key, value in variables.items()]
    commands = case.get("commands", "")
    lines.append(commands)
    if kind == "EXECUTABLE":
        lines.append(f"add_executable({name} {source_file})")
    else:
        lines.append(f"add_library({name} {kind.removesuffix('_LIBRARY')} {source_file})")
    for key, value in target_props.items():
        if (key == "COMPILE_DEFINITIONS" and commands.startswith("add_definitions")) or (
                key == "INCLUDE_DIRECTORIES" and commands.startswith("include_directories")):
            continue
        if key == "SYSTEM_INCLUDE_DIRECTORIES":
            lines.append(f"target_include_directories({name} SYSTEM PRIVATE {bracket(value)})")
        elif key == "COMPILE_FEATURES":
            lines.append(f"target_compile_features({name} PRIVATE {value})")
        else:
            lines.append(f"set_property(TARGET {name} PROPERTY {key} {bracket(value)})")
    lines += [f"set_property(SOURCE {source_file} PROPERTY {key} {bracket(value)})"
              for key, value in source_props.items()]
    # Variables are captured after CMake has loaded Modules, compiler files,
    # and the cache, and after this case's explicit overrides.
    lines.append(fr'''
get_cmake_property(_vars VARIABLES)
foreach(_var IN LISTS _vars)
  if(_var MATCHES "^(_?CMAKE_({language}([0-9]+)?_|INCLUDE_|DEPFILE_FLAGS_|(EXE|SHARED|MODULE|STATIC)_LINKER_FLAGS|SHARED_(LIBRARY|MODULE|BUILD)_|EXE_EXPORTS_|SYSROOT|QUOTE_INCLUDE_PATHS|COLOR_DIAGNOSTICS|LINK_DEPENDS_USE_LINKER|AR$|RANLIB$|LINKER$|COMMAND$))")
    if(NOT _var MATCHES "__|INTERNAL_PLATFORM|IMPLICIT_LINK")
      string(HEX "${{${{_var}}}}" _value)
      file(APPEND "${{CMAKE_CURRENT_BINARY_DIR}}/flags-input.tsv" "${{_var}}\t${{_value}}\n")
    endif()
  endif()
endforeach()
''')
    (source / "CMakeLists.txt").write_text("\n".join(lines))
    env = {key: value for key, value in os.environ.items() if key not in {
        "CC", "CXX", "CFLAGS", "CXXFLAGS", "CPPFLAGS", "LDFLAGS", "CPATH", "CMAKE_GENERATOR", "CMAKE_GENERATOR_INSTANCE", "CMAKE_GENERATOR_PLATFORM",
        "CMAKE_GENERATOR_TOOLSET", "CMAKE_TOOLCHAIN_FILE"}}
    run = subprocess.run([oracle, "-G", "Ninja", "-S", str(source), "-B", str(build),
                          f"-DCMAKE_BUILD_TYPE={config}"], env=env, capture_output=True, text=True)
    if run.returncode:
        raise RuntimeError(f"{case['name']}:\n{run.stdout}\n{run.stderr}")
    edges = parse_ninja(build / "build.ninja", "build ", FLAGS)
    obj = next(edge for edge in edges.values() if f"_COMPILER__{name}_" in edge["statement"])
    link = next(edge for edge in edges.values() if f"_LINKER__{name}_" in edge["statement"])
    rules = parse_ninja(build / "CMakeFiles/rules.ninja", "rule ", {"command", "depfile", "deps"})
    observed = {}
    for line in (build / "flags-input.tsv").read_text().splitlines():
        key, value = line.split("\t", 1)
        observed[key] = bytes.fromhex(value).decode()
    cache = dict(re.findall(r"^([\w]+):[^=\n]+=(.*)$", (build / "CMakeCache.txt").read_text(), re.M))
    # Include only variables consumed by the flags layer or needed to rebuild
    # the rule. Multi-line feature lists and raw flags retain their exact text.
    useful = re.compile(fr'(^CMAKE_{language}([0-9]+_(COMPILE_FEATURES|(STANDARD|EXTENSION)_COMPILE_OPTION)$|_(FLAGS($|_[A-Z_]+$)|LINK_|COMPILE_OPTIONS_|ARCHIV|FLAG_REGEX|STANDARD_DEFAULT|EXTENSIONS_DEFAULT|COMPILER($|_ID$|_ARG1$|_TARGET$|_EXTERNAL_TOOLCHAIN$)|IMPLICIT_INCLUDE|STANDARD_INCLUDE|USING_LINKER|LINKER_(DEPFILE|WRAPPER)|DEFINE_FLAG|EXTENSION_COMPILE_OPTION|CREATE_|COMPILE_OBJECT)))|(^CMAKE_(INCLUDE_|DEPFILE_FLAGS_|(EXE|SHARED|MODULE|STATIC)_LINKER_FLAGS|SHARED_(LIBRARY|MODULE|BUILD)_|EXE_EXPORTS_|SYSROOT|QUOTE_INCLUDE_PATHS|COLOR_DIAGNOSTICS|LINK_DEPENDS_USE_LINKER|AR$|RANLIB$|LINKER$|COMMAND$))|(^_CMAKE_.*_IPO_)')
    captured_vars = {key: value for key, value in observed.items() if useful.search(key)}
    captured_vars.update(variables)
    cache_inputs = {key: cache[key] for key in captured_vars if key in cache}
    rule_names = [edge["statement"].split(": ", 1)[1].split(" ", 1)[0] for edge in (obj, link)]
    return dict(name=case["name"], language=language, kind=kind, config=config, target_name=name,
                source=str(source), binary=str(build), source_file=source_file, policies=policies,
                target_props=target_props, source_props=source_props, directory_props=case.get("directory", {}),
                variables=captured_vars, cache=cache_inputs,
                object_edge=obj, link_edge=link, compile_rule=rules[rule_names[0]], link_rule=rules[rule_names[1]])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cmake", default=ORACLE)
    args = parser.parse_args()
    version = subprocess.check_output([args.cmake, "--version"], text=True).splitlines()[0]
    if version != "cmake version 4.1.0":
        raise SystemExit(f"Expected Kitware 4.1.0, found {version!r}")
    with tempfile.TemporaryDirectory(prefix="cmake-rust-flags-") as root:
        results = [capture(args.cmake, Path(root), number, case) for number, case in enumerate(cases())]
    # Common observed values are identical within each compiler setup. Keep
    # a shared table and store the changed inputs alongside each project.
    common = {}
    for language in sorted({case["language"] for case in results}):
        projects = [case for case in results if case["language"] == language]
        common[language] = {}
        for field in ("variables", "cache"):
            first = projects[0][field]
            defaults = {key: val for key, val in first.items()
                        if all(case[field].get(key) == val for case in projects)}
            common[language][field] = defaults
            for case in projects:
                case[field] = {key: val for key, val in case[field].items() if key not in defaults}
    OUT.write_text(json.dumps({"common": common, "projects": results}, indent=2, sort_keys=True) + "\n")
    print(f"Captured {len(results)} projects in {OUT}")


if __name__ == "__main__":
    main()
