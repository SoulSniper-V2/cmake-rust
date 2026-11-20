#!/usr/bin/env python3
"""Regenerate genex_usage_corpus.json and its unit-test inventory with CMake 4.1.0.

Each case is a small `project(x C)` build whose head target `main` (an
executable) consumes usage requirements. The oracle's
CMAKE_DEBUG_TARGET_PROPERTIES log records exactly what
cmGeneratorTarget::Get{IncludeDirectories,CompileDefinitions,CompileOptions,
CompileFeatures,LinkOptions,LinkDirectories} returned, entry by entry, with
each entry's backtrace line. build.ninja supplies the final flags (SHELL:
splitting, LINKER: wrapping, -isystem classification).
"""
import argparse
import json
from pathlib import Path
import re
import subprocess
import tempfile

LOGS = {
    "includes": "INCLUDE_DIRECTORIES",
    "compile definitions": "COMPILE_DEFINITIONS",
    "compile options": "COMPILE_OPTIONS",
    "compile features": "COMPILE_FEATURES",
    "link options": "LINK_OPTIONS",
    "link directories": "LINK_DIRECTORIES",
}
VARIABLES = [
    "CMAKE_C_LINKER_WRAPPER_FLAG", "CMAKE_C_LINKER_WRAPPER_FLAG_SEP",
    "CMAKE_C_COMPILER_ID", "CMAKE_C_COMPILER_VERSION", "CMAKE_C_COMPILE_FEATURES",
    "CMAKE_C_COMPILER_LOADED", "CMAKE_C_STANDARD_DEFAULT", "CMAKE_SYSTEM_NAME",
]
EXISTING = ["/usr/share", "/usr/lib", "/usr/local", "/usr/bin", "/usr/sbin", "/etc", "/var", "/opt", "/srv", "/mnt"]
MISSING = "/cmake-rust-genex/does/not/exist"


def bracket(value):
    level = "="
    while f"]{level}]" in value:
        level += "="
    return f"[{level}[{value}]{level}]"


def T(kind, imported=False, **props):
    """A target: kind, imported, and properties as {PROP: value or [values]}."""
    return {"type": kind, "imported": imported, "props": props}


def cases():
    a, b, c, d = EXISTING[:4]
    yield "own_includes_dedup", {
        "main": T("EXECUTABLE", INCLUDE_DIRECTORIES=[f"{a};{b}", f"{b};{c}//", f"{a}/"]),
    }
    yield "interface_chain_attributed_to_direct_item", {
        "main": T("EXECUTABLE", INCLUDE_DIRECTORIES=[a], LINK_LIBRARIES=["b"]),
        "b": T("INTERFACE", INTERFACE_INCLUDE_DIRECTORIES=[b], INTERFACE_LINK_LIBRARIES=["c"]),
        "c": T("INTERFACE", INTERFACE_INCLUDE_DIRECTORIES=[c], INTERFACE_LINK_LIBRARIES=["imp"]),
        "imp": T("INTERFACE", True, INTERFACE_INCLUDE_DIRECTORIES=[f"{d};{MISSING}"]),
    }
    yield "imported_direct_missing_path", {
        "main": T("EXECUTABLE", LINK_LIBRARIES=["imp"]),
        "imp": T("INTERFACE", True, INTERFACE_INCLUDE_DIRECTORIES=[f"{a};{MISSING}"]),
    }
    yield "imported_direct_existing_is_system", {
        "main": T("EXECUTABLE", INCLUDE_DIRECTORIES=[b], LINK_LIBRARIES=["imp;b"]),
        "imp": T("INTERFACE", True, INTERFACE_INCLUDE_DIRECTORIES=[f"{a};{b}"]),
        "b": T("INTERFACE", INTERFACE_INCLUDE_DIRECTORIES=[c]),
    }
    yield "no_system_from_imported", {
        "main": T("EXECUTABLE", NO_SYSTEM_FROM_IMPORTED="ON", LINK_LIBRARIES=["imp"]),
        "imp": T("INTERFACE", True, INTERFACE_INCLUDE_DIRECTORIES=[a],
                 INTERFACE_SYSTEM_INCLUDE_DIRECTORIES=[b]),
    }
    yield "imported_no_system", {
        "main": T("EXECUTABLE", LINK_LIBRARIES=["imp", "imp2"]),
        "imp": T("INTERFACE", True, IMPORTED_NO_SYSTEM="ON", INTERFACE_INCLUDE_DIRECTORIES=[a]),
        "imp2": T("INTERFACE", True, SYSTEM="OFF", INTERFACE_INCLUDE_DIRECTORIES=[b]),
    }
    yield "system_property_on_normal_dep", {
        "main": T("EXECUTABLE", LINK_LIBRARIES=["b;c"]),
        "b": T("INTERFACE", SYSTEM="ON", INTERFACE_INCLUDE_DIRECTORIES=[a]),
        "c": T("INTERFACE", INTERFACE_INCLUDE_DIRECTORIES=[b], INTERFACE_SYSTEM_INCLUDE_DIRECTORIES=[f"{b};{d}"]),
    }
    yield "system_via_transitive_closure", {
        "main": T("EXECUTABLE", LINK_LIBRARIES=["b"]),
        "b": T("INTERFACE", INTERFACE_LINK_LIBRARIES=["imp"], INTERFACE_INCLUDE_DIRECTORIES=[a]),
        "imp": T("INTERFACE", True, INTERFACE_INCLUDE_DIRECTORIES=[b]),
    }
    yield "own_system_include", {
        "main": T("EXECUTABLE", INCLUDE_DIRECTORIES=[a], SYSTEM_PRIVATE=[f"{b}", c]),
    }
    yield "relative_own_include", {
        "main": T("EXECUTABLE", INCLUDE_DIRECTORIES=[f"{a};rel/dir"]),
    }
    yield "relative_interface_include", {
        "main": T("EXECUTABLE", LINK_LIBRARIES=["b"]),
        "b": T("INTERFACE", INTERFACE_INCLUDE_DIRECTORIES=["rel"]),
    }
    yield "relative_include_through_chain", {
        "main": T("EXECUTABLE", LINK_LIBRARIES=["b"]),
        "b": T("INTERFACE", INTERFACE_LINK_LIBRARIES=["c"]),
        "c": T("INTERFACE", INTERFACE_INCLUDE_DIRECTORIES=["rel"]),
    }
    yield "diamond_already_seen", {
        "main": T("EXECUTABLE", LINK_LIBRARIES=["b;c;d"], COMPILE_DEFINITIONS=["M"]),
        "b": T("INTERFACE", INTERFACE_LINK_LIBRARIES=["d"], INTERFACE_COMPILE_DEFINITIONS=["B"],
               INTERFACE_INCLUDE_DIRECTORIES=[a]),
        "c": T("INTERFACE", INTERFACE_LINK_LIBRARIES=["d"], INTERFACE_COMPILE_DEFINITIONS=["C;D"],
               INTERFACE_INCLUDE_DIRECTORIES=[b]),
        "d": T("INTERFACE", INTERFACE_COMPILE_DEFINITIONS=["D"], INTERFACE_INCLUDE_DIRECTORIES=[c]),
    }
    yield "interface_cycle", {
        "main": T("EXECUTABLE", LINK_LIBRARIES=["b"]),
        "b": T("INTERFACE", INTERFACE_LINK_LIBRARIES=["c"], INTERFACE_COMPILE_DEFINITIONS=["B"]),
        "c": T("INTERFACE", INTERFACE_LINK_LIBRARIES=["b;main"], INTERFACE_COMPILE_DEFINITIONS=["C"]),
    }
    yield "alias_and_duplicate_links", {
        "main": T("EXECUTABLE", LINK_LIBRARIES=["alias_b", "b;b"]),
        "b": T("INTERFACE", INTERFACE_COMPILE_DEFINITIONS=["B;B2"], INTERFACE_COMPILE_OPTIONS=["-Ob"]),
        "alias_b": "b",
    }
    yield "head_sensitive_interface", {
        "main": T("EXECUTABLE", LABEL="mainlabel", LINK_LIBRARIES=["b"]),
        "b": T("INTERFACE", LABEL="blabel", INTERFACE_COMPILE_DEFINITIONS=[
            "L=$<TARGET_PROPERTY:LABEL>;C=$<COMPILE_LANGUAGE>;$<$<COMPILE_LANGUAGE:C>:ISC>;$<$<CONFIG:Debug>:DBG>"]),
    }
    yield "build_install_interface", {
        "main": T("EXECUTABLE", LINK_LIBRARIES=["b"],
                  INCLUDE_DIRECTORIES=[f"$<BUILD_INTERFACE:{a}>;$<INSTALL_INTERFACE:inc>"]),
        "b": T("INTERFACE", INTERFACE_INCLUDE_DIRECTORIES=[f"$<BUILD_INTERFACE:{b}>;$<INSTALL_INTERFACE:include>;$<BUILD_LOCAL_INTERFACE:{c}>"]),
    }
    yield "link_only_in_static_interface", {
        "main": T("EXECUTABLE", LINK_LIBRARIES=["s"]),
        "s": T("STATIC", LINK_LIBRARIES=["p"], INTERFACE_LINK_LIBRARIES=["$<LINK_ONLY:p>;q"]),
        "p": T("INTERFACE", INTERFACE_COMPILE_DEFINITIONS=["P"], INTERFACE_LINK_OPTIONS=["-p-link"],
               INTERFACE_LINK_DIRECTORIES=[a]),
        "q": T("INTERFACE", INTERFACE_COMPILE_DEFINITIONS=["Q"], INTERFACE_LINK_OPTIONS=["-q-link"],
               INTERFACE_LINK_DIRECTORIES=[b]),
    }
    yield "direct_overrides_follow_dependencies_before_injection", {
        "main": T("EXECUTABLE", LINK_LIBRARIES=["b;excluded"]),
        "b": T("INTERFACE", INTERFACE_COMPILE_DEFINITIONS=["B"],
               INTERFACE_LINK_LIBRARIES_DIRECT=["c"], INTERFACE_LINK_LIBRARIES_DIRECT_EXCLUDE=["excluded"]),
        "c": T("INTERFACE", INTERFACE_COMPILE_DEFINITIONS=["C"], INTERFACE_LINK_LIBRARIES_DIRECT=["d"]),
        "d": T("INTERFACE", INTERFACE_COMPILE_DEFINITIONS=["D"]),
        "excluded": T("INTERFACE", INTERFACE_COMPILE_DEFINITIONS=["EXCLUDED"]),
    }
    yield "imported_legacy_link_interface", {
        "main": T("EXECUTABLE", LINK_LIBRARIES=["imp"]),
        "imp": T("UNKNOWN", True, IMPORTED_LOCATION="/usr/lib/libfake.a",
                 IMPORTED_LINK_INTERFACE_LIBRARIES="b", INTERFACE_COMPILE_DEFINITIONS=["IMP"]),
        "b": T("INTERFACE", INTERFACE_COMPILE_DEFINITIONS=["B"]),
    }
    yield "compile_only", {
        "main": T("EXECUTABLE", LINK_LIBRARIES=["$<COMPILE_ONLY:b>;c"]),
        "b": T("INTERFACE", INTERFACE_COMPILE_DEFINITIONS=["B"], INTERFACE_LINK_OPTIONS=["-b-link"]),
        "c": T("INTERFACE", INTERFACE_COMPILE_DEFINITIONS=["C"], INTERFACE_LINK_OPTIONS=["-c-link"]),
    }
    yield "definitions_genex_and_empties", {
        "main": T("EXECUTABLE", COMPILE_DEFINITIONS=["A;;B=1", "$<0:X>;$<1:Y>;A", "Z=a b"], LINK_LIBRARIES=["b"]),
        "b": T("INTERFACE", INTERFACE_COMPILE_DEFINITIONS=["A;$<$<BOOL:1>:B=1>;W"]),
    }
    yield "options_shell_and_dedup", {
        "main": T("EXECUTABLE", COMPILE_OPTIONS=["-Wall;SHELL:-x 'a b';-Wall", "SHELL:-x 'a b';-x;SHELL:-f \"q r\" -g"],
                  LINK_LIBRARIES=["b"]),
        "b": T("INTERFACE", INTERFACE_COMPILE_OPTIONS=["SHELL:-x 'a b';-O2;SHELL:-x\\ c"]),
    }
    yield "compile_features", {
        "main": T("EXECUTABLE", COMPILE_FEATURES=["c_std_99;c_restrict"], LINK_LIBRARIES=["b"]),
        "b": T("INTERFACE", INTERFACE_COMPILE_FEATURES=["c_std_11;c_restrict;$<$<COMPILE_LANGUAGE:C>:c_static_assert>"]),
    }
    yield "compile_features_plain", {
        "main": T("EXECUTABLE", COMPILE_FEATURES=["c_std_99;c_restrict"], LINK_LIBRARIES=["b"]),
        "b": T("INTERFACE", INTERFACE_COMPILE_FEATURES=["c_std_11;c_restrict;$<1:c_static_assert>"]),
    }
    yield "link_options_wrappers", {
        "main": T("EXECUTABLE", LINK_OPTIONS=["LINKER:-z,defs;LINKER:SHELL:-a 'b c';-Wl,raw", "$<HOST_LINK:-h>;$<DEVICE_LINK:-dev>;LINKER:,"],
                  LINK_LIBRARIES=["b"]),
        "b": T("INTERFACE", INTERFACE_LINK_OPTIONS=["SHELL:-x 'y z';LINKER:--as-needed;-Wl,raw"]),
    }
    yield "linker_shell_in_arguments", {
        "main": T("EXECUTABLE", LINK_OPTIONS=["LINKER:a,SHELL:b"]),
    }
    yield "link_directories", {
        "main": T("EXECUTABLE", LINK_DIRECTORIES=[f"{a};{b}//x/", a], LINK_LIBRARIES=["b"]),
        "b": T("INTERFACE", INTERFACE_LINK_DIRECTORIES=[f"{c};{a}"], INTERFACE_LINK_LIBRARIES=["c"]),
        "c": T("INTERFACE", INTERFACE_LINK_DIRECTORIES=[d]),
    }
    yield "relative_own_link_directory", {
        "main": T("EXECUTABLE", LINK_DIRECTORIES=["rel"]),
    }
    yield "relative_interface_link_directory", {
        "main": T("EXECUTABLE", LINK_LIBRARIES=["b"]),
        "b": T("INTERFACE", INTERFACE_LINK_DIRECTORIES=["rel"]),
    }
    yield "self_link", {
        "main": T("EXECUTABLE", LINK_LIBRARIES=["main"]),
    }
    yield "unknown_imported_with_location", {
        "main": T("EXECUTABLE", LINK_LIBRARIES=["imp"]),
        "imp": T("UNKNOWN", True, IMPORTED_LOCATION="/usr/lib/libcmake-rust-genex.a",
                 INTERFACE_INCLUDE_DIRECTORIES=[a], INTERFACE_COMPILE_DEFINITIONS=["IMP"]),
    }
    yield "whitespace_link_item", {
        "main": T("EXECUTABLE", LINK_LIBRARIES=[" b"]),
        "b": T("INTERFACE", INTERFACE_COMPILE_DEFINITIONS=["B"]),
    }
    yield "non_target_link_items_ignored", {
        "main": T("EXECUTABLE", LINK_LIBRARIES=["m;-lfoo;/usr/lib/libz.so;b"]),
        "b": T("INTERFACE", INTERFACE_COMPILE_DEFINITIONS=["B"]),
    }
    yield "static_lib_link_implementation", {
        "main": T("EXECUTABLE", LINK_LIBRARIES=["s"]),
        "s": T("STATIC", LINK_LIBRARIES=["p"], INTERFACE_COMPILE_DEFINITIONS=["S"]),
        "p": T("INTERFACE", INTERFACE_COMPILE_DEFINITIONS=["P"]),
    }
    yield "shared_lib_interface", {
        "main": T("EXECUTABLE", LINK_LIBRARIES=["sh"]),
        "sh": T("SHARED", LINK_LIBRARIES=["p"], INTERFACE_LINK_LIBRARIES=["q"],
                INTERFACE_INCLUDE_DIRECTORIES=[a]),
        "p": T("INTERFACE", INTERFACE_INCLUDE_DIRECTORIES=[b]),
        "q": T("INTERFACE", INTERFACE_INCLUDE_DIRECTORIES=[c]),
    }
    yield "link_language_genex_in_link_libraries", {
        "main": T("EXECUTABLE", LINK_LIBRARIES=["$<$<LINK_LANGUAGE:C>:b>;$<$<LINK_LANGUAGE:CXX>:c>"]),
        "b": T("INTERFACE", INTERFACE_COMPILE_DEFINITIONS=["B"]),
        "c": T("INTERFACE", INTERFACE_COMPILE_DEFINITIONS=["C"]),
    }
    yield "target_property_genex_in_interface", {
        "main": T("EXECUTABLE", LINK_LIBRARIES=["b"]),
        "b": T("INTERFACE", INTERFACE_COMPILE_DEFINITIONS=["$<TARGET_PROPERTY:c,INTERFACE_COMPILE_DEFINITIONS>;B"]),
        "c": T("INTERFACE", INTERFACE_COMPILE_DEFINITIONS=["C1;C2"]),
    }
    yield "self_referencing_interface", {
        "main": T("EXECUTABLE", LINK_LIBRARIES=["b"]),
        "b": T("INTERFACE", INTERFACE_COMPILE_DEFINITIONS=["$<TARGET_PROPERTY:b,INTERFACE_COMPILE_DEFINITIONS>"]),
    }
    yield "own_property_references_build_property", {
        "main": T("EXECUTABLE", COMPILE_DEFINITIONS=["$<TARGET_PROPERTY:COMPILE_DEFINITIONS>"]),
    }
    yield "include_directories_via_target_property", {
        "main": T("EXECUTABLE", LINK_LIBRARIES=["b"],
                  COMPILE_DEFINITIONS=["I=$<JOIN:$<TARGET_PROPERTY:INCLUDE_DIRECTORIES>,+>"],
                  INCLUDE_DIRECTORIES=[a]),
        "b": T("INTERFACE", INTERFACE_INCLUDE_DIRECTORIES=[b], INTERFACE_LINK_LIBRARIES=["c"]),
        "c": T("INTERFACE", INTERFACE_INCLUDE_DIRECTORIES=[c]),
    }
    yield "executable_interfaces", {
        "main": T("EXECUTABLE", LINK_LIBRARIES=["tool;s"]),
        "tool": T("EXECUTABLE", ENABLE_EXPORTS="ON", INTERFACE_LINK_LIBRARIES=["p"],
                  INTERFACE_COMPILE_DEFINITIONS=["TOOL"]),
        "s": T("STATIC", INTERFACE_LINK_LIBRARIES=["noexports"]),
        "noexports": T("EXECUTABLE", INTERFACE_LINK_LIBRARIES=["q"], INTERFACE_COMPILE_DEFINITIONS=["NOEXP"]),
        "p": T("INTERFACE", INTERFACE_COMPILE_DEFINITIONS=["P"]),
        "q": T("INTERFACE", INTERFACE_COMPILE_DEFINITIONS=["Q"]),
    }
    yield "module_without_interface", {
        "main": T("EXECUTABLE", LINK_LIBRARIES=["s"]),
        "s": T("STATIC", INTERFACE_LINK_LIBRARIES=["mod"]),
        "mod": T("MODULE", INTERFACE_COMPILE_DEFINITIONS=["MOD"], LINK_LIBRARIES=["p"]),
        "p": T("INTERFACE", INTERFACE_COMPILE_DEFINITIONS=["P"]),
    }
    yield "custom_transitive_property_in_definitions", {
        "main": T("EXECUTABLE", LINK_LIBRARIES=["b"], CUSTOM="mine",
                  COMPILE_DEFINITIONS=["V=$<JOIN:$<TARGET_PROPERTY:CUSTOM>,+>"]),
        "b": T("INTERFACE", TRANSITIVE_COMPILE_PROPERTIES="CUSTOM", INTERFACE_CUSTOM="fromb",
               INTERFACE_LINK_LIBRARIES=["c"]),
        "c": T("INTERFACE", INTERFACE_CUSTOM="fromc"),
    }
    yield "genex_eval_of_interface", {
        "main": T("EXECUTABLE", LINK_LIBRARIES=["b"], RAWDEF="$<TARGET_PROPERTY:LABEL>", LABEL="lbl",
                  COMPILE_DEFINITIONS=["G=$<GENEX_EVAL:$<TARGET_PROPERTY:RAWDEF>>"]),
        "b": T("INTERFACE", INTERFACE_COMPILE_DEFINITIONS=["T=$<TARGET_GENEX_EVAL:b,$<TARGET_PROPERTY:b,RAWB>>"],
               RAWB="$<TARGET_PROPERTY:b,NAME>"),
    }


def generate(targets):
    """Returns CMakeLists.txt text and the structured case (with line numbers)."""
    lines = ["cmake_minimum_required(VERSION 4.1)", "project(x C)",
             "set(CMAKE_DEBUG_TARGET_PROPERTIES " + " ".join(LOGS.values()) + ")"]
    model = {"targets": {}, "aliases": {}}
    for name, value in targets.items():
        if isinstance(value, str):
            continue
        kind, imported = value["type"], value["imported"]
        if kind == "EXECUTABLE":
            lines.append(f"add_executable({name}{' IMPORTED' if imported else ' main.c'})")
        else:
            source = " main.c" if kind in ("STATIC", "SHARED", "MODULE", "OBJECT") and not imported else ""
            lines.append(f"add_library({name} {kind}{' IMPORTED' if imported else ''}{source})")
        model["targets"][name] = {"type": kind, "imported": imported, "entries": {}, "system": []}
    for name, value in targets.items():
        if isinstance(value, str):
            lines.append(f"add_library({name} ALIAS {value})")
            model["aliases"][name] = value
            continue
        entries = model["targets"][name]["entries"]
        for prop, values in value["props"].items():
            if isinstance(values, str):
                values = [values]
            for item in values:
                if prop == "SYSTEM_PRIVATE":
                    lines.append(f"target_include_directories({name} SYSTEM PRIVATE {bracket(item)})")
                    entries.setdefault("INCLUDE_DIRECTORIES", []).append([len(lines), item])
                    model["targets"][name]["system"].append(item)
                else:
                    lines.append(f"set_property(TARGET {name} APPEND PROPERTY {prop} {bracket(item)})")
                    entries.setdefault(prop, []).append([len(lines), item])
    lines.append("foreach(v IN ITEMS " + " ".join(VARIABLES) + ")")
    lines.append('  message(STATUS "GENEX-VAR ${v}=${${v}}")')
    lines.append("endforeach()")
    return "\n".join(lines) + "\n", model


LOG_RE = re.compile(r"CMake Debug Log at CMakeLists\.txt:(\d+) \([^)]*\):\n  Used (.+?) for target (\S+):\n\n((?:   \* .*\n)+)")


def ninja_block(text, header):
    """Variables of the first build statement whose header line matches."""
    result = {}
    lines = text.splitlines()
    for index, line in enumerate(lines):
        if re.match(header, line):
            for body in lines[index + 1:]:
                if not body.startswith("  "):
                    break
                key, _, value = body.strip().partition(" = ")
                result[key] = value
            break
    return result


def capture(oracle, root, name, targets):
    script, model = generate(targets)
    (root / "CMakeLists.txt").write_text(script)
    (root / "main.c").write_text("int main(void) { return 0; }\n")
    build = root / "b"
    subprocess.run(["rm", "-rf", str(build)], check=True)
    run = subprocess.run([oracle, "-G", "Ninja", "-S", root, "-B", build],
                         capture_output=True, text=True, check=False)
    variables = dict(re.findall(r"^-- GENEX-VAR (\w+)=(.*)$", run.stdout, re.M))
    case = {"name": name, **model}
    logs = {}
    for line, log, target, items in LOG_RE.findall(run.stderr):
        if target != "main":
            continue
        values = [item[len("   * "):] for item in items.splitlines()]
        logs.setdefault(log, []).extend([int(line), value] for value in values)
    unknown = set(logs) - set(LOGS)
    if unknown:
        raise RuntimeError(f"{name}: unexpected logs {unknown}")
    case["logs"] = logs
    errors = re.findall(r"^CMake Error[^\n]*:\n((?:  .*\n|\n)*?)\n\n", run.stderr, re.M)
    if run.returncode != 0:
        if not errors:
            raise RuntimeError(f"{name}: {run.returncode}\n{run.stderr}")
        case["error"] = errors[0]
    else:
        ninja = (build / "build.ninja").read_text()
        compile_vars = ninja_block(ninja, r"build CMakeFiles/main\.dir/main\.c\.o: ")
        link_vars = ninja_block(ninja, r"build main: ")
        case["ninja"] = {
            "DEFINES": compile_vars.get("DEFINES", ""),
            "INCLUDES": compile_vars.get("INCLUDES", ""),
            "FLAGS": compile_vars.get("FLAGS", ""),
            "LINK_FLAGS": link_vars.get("LINK_FLAGS", ""),
        }
    return case, variables


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("oracle", type=Path)
    args = parser.parse_args()
    version = subprocess.check_output([args.oracle, "--version"], text=True).splitlines()[0]
    if version != "cmake version 4.1.0":
        raise SystemExit(f"Expected CMake 4.1.0, found {version}")
    inventory = []
    variables = {}
    with tempfile.TemporaryDirectory(prefix="genex-usage-oracle-") as work:
        for name, targets in cases():
            case, found = capture(args.oracle.resolve(), Path(work), name, targets)
            variables = variables or found
            inventory.append(case)
    path = Path(__file__).resolve().parent
    document = {"variables": variables, "missing_path": MISSING, "cases": inventory}
    (path / "genex_usage_corpus.json").write_text(json.dumps(document, ensure_ascii=False, indent=1) + "\n")
    with (path / "../../src/genex/usage_corpus_tests.rs").open("w") as tests:
        tests.write("// Generated test inventory from tests/data/capture_genex_usage_corpus.py.\n")
        tests.write("usage_cases! {\n")
        for i, case in enumerate(inventory):
            tests.write(f"    usage_{i:02}_{case['name']} => {i},\n")
        tests.write("}\n")
    print(f"Captured {len(inventory)} oracle runs")


if __name__ == "__main__":
    main()
