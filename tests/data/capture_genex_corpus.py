#!/usr/bin/env python3
"""Regenerate genex_corpus.json and its unit-test inventory with CMake 4.1.0."""
import argparse
import json
from pathlib import Path
import subprocess
import tempfile


def bracket(value):
    level = "="
    while f"]{level}]" in value:
        level += "="
    return f"[{level}[{value}]{level}]"


def cases():
    def expressions(node, arguments, **setup):
        for argument in arguments:
            yield f"$<{node}{argument}>", {"language": "NONE", **setup}

    for node, arguments in {
        "BOOL": [":", ":OFF", ":false", ":0", ":0.0", ":FALSE", ":IGNORE", ":ignore", ":n", ":NO", ":notfound", ":a-NOTFOUND", ":a-notfound", ":yes", ": ", ":000", ""],
        "AND": [":0,$<UNKNOWN>", ":1,0,$<BOOL>", ":1,1,1", ":1,0,1", ":2,0", ":", "", ":1,$<UNKNOWN>"],
        "OR": [":1,$<UNKNOWN>", ":0,1,$<BOOL>", ":0,0,0", ":0,1,0", ":2,1", ":", ""],
        "NOT": [":1", ":0", ":01", ":", ":one,two", ""],
        "IF": [":1,yes,no", ":0,yes,no", ":0,$<UNKNOWN>,no", ":1,yes,$<UNKNOWN>", ":bad,yes,no", ":0,,", ":1,yes", ":1,1,2,3"],
        "0": ["", ":$<UNKNOWN>,q", ":,y"],
        "1": ["", ":x:y,z", ":x$<SEMICOLON>y", ":$<UNKNOWN>"],
        "STREQUAL": [":,", ":a,A", ":a,a", ": ;, ;", ":a", ":a,b,c", ":$<COMMA>,$<COMMA>"],
        "EQUAL": [":0,00", ":0x10,16", ":+0X10,020", ":0b101,5", ":-0b101,-5", ":0B-11,-3", ":09,9", ":1 ,1", ": 1,1", ":-9223372036854775808,-0x8000000000000000", ":9223372036854775808,0", ":0.0,0", ":,0"],
        "IN_LIST": [":a;a,a;a;a", ":a,b;c;a", ":,a;;b;", ":,", ":x,[a;x];y", r":a;b,a\;b;c", ":A,a;b"],
        "JOIN": [":a;;b;,!", ":a;b,,,", ":a;b,$<COMMA>", ":,none", ":a;b", r":a\;b;c,_"],
        "FILTER": [":;a;;b;,INCLUDE,^$", ":a;b;aa,INCLUDE,^a", ":a;b;aa,EXCLUDE,a$", ":a,BAD,a", ":a,INCLUDE,["],
        "REMOVE_DUPLICATES": [":b;b;a;b;a", ":;;x;;x;", ":", ":A;a;b;A", r":a\;b;a\;b;c"],
        "UPPER_CASE": [": hello,world:ä ", "", ":$<COMMA>"],
        "LOWER_CASE": [":Hello,World:Ä ", ":$<UPPER_CASE:Word>", "", ":"],
        "MAKE_C_IDENTIFIER": [":9 chars-ä.", ":á", ":", ":abc_123", ":one,two"],
        "ANGLE-R": ["", ":"],
        "COMMA": ["", ":x"],
        "SEMICOLON": ["", ":x"],
        "QUOTE": ["", ":x"],
        "BUILD_INTERFACE": [":hello,world", "", ":$<LOWER_CASE:X>"],
        "BUILD_LOCAL_INTERFACE": [":hello,world", ":", ""],
        "INSTALL_INTERFACE": [":$<UNKNOWN>", ":hello,world", ""],
        "INSTALL_PREFIX": ["", ":/usr"],
        "COMPILE_ONLY": [":a", "", ":a,b"],
        "LINK_ONLY": [":a", "", ":a,b"],
        "HOST_LINK": ["", ":a"],
        "DEVICE_LINK": [":a", ""],
        "LINK_GROUP": [":RESCAN,a", ":"],
        "LINK_LIBRARY": [":WHOLE_ARCHIVE,a", ""],
        "SHELL_PATH": [":/a;/b", ":/some path;/more path", ":~/folder", ":/with//double/", ":/abs;relative", ":", r":C:\folder"],
        "TARGET_NAME": [":missing", ":a,b", ":a$<COMMA>b", "", ":"],
        "TARGET_EXISTS": [":a", ":alias", ":missing", ":a::b", ":a b", ":", ":a,b", ""],
        "TARGET_NAME_IF_EXISTS": [":b", ":alias", ":missing", ":a b", ":"],
        "TARGET_PROPERTY": [":a,TYPE", ":a,RAW", ":a,ABSENT", ":a,INTERFACE_COMPILE_DEFINITIONS",
            ":a,INTERFACE_LINK_OPTIONS", ":a,INTERFACE_INCLUDE_DIRECTORIES", ":a,INTERFACE_LINK_LIBRARIES",
            ":alias,ALIASED_TARGET", ":alias,ALIAS_GLOBAL", ":a,ALIASED_TARGET", ":missing,ALIASED_TARGET",
            ":missing,P", ":a,BAD-PROP", ":a,", ":a b,p", ":a b,bad-prop", ":,P", ":,", ":P", ":a,b,c"],
        "TARGET_GENEX_EVAL": [":a,$<TARGET_PROPERTY:a,RAW>", ":a,$<1:$<TARGET_PROPERTY:LABEL>>",
            ":missing,thing", ":invalid name,thing", ":a", ":a,", ":a,$<TARGET_PROPERTY:a,INTERFACE_CUSTOM>"],
        "GENEX_EVAL": [":$<TARGET_PROPERTY:a,RAW>", ":$<TARGET_PROPERTY:a,LITERAL>", ":a,b", ":", ""],
        "TARGET_OBJECTS": [":obj", ":a", ":imp", ":missing"],
        "TARGET_RUNTIME_DLLS": [":a", ":missing"],
        "TARGET_RUNTIME_DLL_DIRS": [":a", ":missing"],
        "TARGET_POLICY": [":CMP0112", ":CMP0001"],
        "COMPILE_FEATURES": [":cxx_std_23", ":no_such_feature", "", ":"],
        "COMPILE_LANGUAGE": [":CXX", "", ":C,CXX"],
        "COMPILE_LANG_AND_ID": [":CXX,GNU", ":CXX"],
        "LINK_LANGUAGE": [":CXX", ""],
        "LINK_LANG_AND_ID": [":CXX,GNU", ":CXX"],
        "PLATFORM_ID": ["", ":Linux", ":Linux,Windows", ":linux", ":", ":Windows", ":x.y,Linux"],
        "OUTPUT_CONFIG": [":$<CONFIG>"],
        "COMMAND_CONFIG": [":$<CONFIG>"],
        "CXX_MODULES": [":a"],
        "ISPC_COMPILER_ID": ["", ":Intel"],
        "PATH_EQUAL": [":a//b,a/b", ":a/b/,a/b", ":a/./b,a/b", "://a,/a", ":,", ":a,a", ":/a,b", ":a"],
    }.items():
        yield from expressions(node, arguments)
    for node in ["VERSION_EQUAL", "VERSION_GREATER", "VERSION_GREATER_EQUAL", "VERSION_LESS", "VERSION_LESS_EQUAL"]:
        yield from expressions(node, [":1.2.0,1.2", ":1.002x,1.2", ":2,11", ":1.2,1.10", ":,0", ":a,b", ":1.1,1", ":1"])
    for lang in ["C", "CXX", "OBJC", "OBJCXX", "CUDA", "Fortran", "HIP"]:
        for field in ["ID", "VERSION", "FRONTEND_VARIANT"]:
            yield from expressions(f"{lang}_COMPILER_{field}", ["", ":GNU", ":"], head="a")
            yield from expressions(f"{lang}_COMPILER_{field}", [""])
    for node, arguments in {
        "CONFIG": ["", ":debug", ":", ":release,debug", ":release,bad-config", ":bad-config,Debug", ":Debug,bad-config"],
        "CONFIGURATION": ["", ":"],
    }.items():
        yield from expressions(node, arguments, config="Debug")
    yield from expressions("CONFIG", [":Release", ":Debug", ":RelWithDebInfo", ":Release,Debug"], head="imp", config="Debug")
    for kind in ["FILE", "LINKER_FILE", "LINKER_LIBRARY_FILE", "LINKER_IMPORT_FILE", "IMPORT_FILE",
                 "SONAME_FILE", "SONAME_IMPORT_FILE", "PDB_FILE", "BUNDLE_DIR", "BUNDLE_CONTENT_DIR"]:
        suffixes = ["", "_NAME", "_DIR", "_BASE_NAME", "_PREFIX", "_SUFFIX"]
        if kind.startswith("SONAME"):
            suffixes = suffixes[:3]
        elif kind == "PDB_FILE":
            suffixes = suffixes[:4]
        elif kind == "BUNDLE_DIR":
            suffixes = suffixes[:2]
        elif kind == "BUNDLE_CONTENT_DIR":
            suffixes = [""]
        for suffix in suffixes:
            yield from expressions(f"TARGET_{kind}{suffix}", [":imp"])
    yield from expressions("TARGET_SONAME_FILE", [":shared", ":a", ":missing", ":bad name"])
    yield from expressions("TARGET_FILE", [":a", ":shared", ":missing", ":bad name"])
    yield from expressions("TARGET_POLICY", [":CMP0112", ":CMP0001"], head="a")
    yield from expressions("COMPILE_FEATURES", [":no_such_feature", ":cxx_std_23"], head="a")
    yield from expressions("TARGET_PROPERTY", [":LABEL", ":", ":a,INTERFACE_CUSTOM"], head="b")
    for a, value in [
        ("cycle", "$<TARGET_PROPERTY:cycle,INTERFACE_COMPILE_DEFINITIONS>"),
        ("a", "$<TARGET_PROPERTY:b,INTERFACE_COMPILE_DEFINITIONS>"),
    ]:
        yield f"$<TARGET_PROPERTY:{a},INTERFACE_COMPILE_DEFINITIONS>", {
            "properties": {a: {"INTERFACE_COMPILE_DEFINITIONS": value},
                           **({"b": {"INTERFACE_COMPILE_DEFINITIONS": "$<TARGET_PROPERTY:b,INTERFACE_COMPILE_DEFINITIONS>"}}
                              if a == "a" else {})}}
    for op, arguments in {
        "LENGTH": ["a;;b;", "", "a;b", "a,b", r"a\;b;c", "a;[b;c];d"],
        "GET": ["a;b;c,1;-1,0", "a;b;c,-3", "a;b;c,3", "a;b;-3,-9", "a;;c,1", ",0", "a;b,not-a-number", "a;b,;"],
        "JOIN": ["a;;b;,!", "a;b,$<COMMA>", ",!", "a;b,,x"],
        "SUBLIST": ["a;b;c,1,-1", "a;b,0,20", "a;b,1,0", "a;b,2,0", "a;b,-1,5", "a;,-1,abc", "a;b,0,-2", ",bad,bad"],
        "FIND": ["a;b;c,c", "a;b;,", ",", "x,Y"],
        "APPEND": ["a;b,c,d;e", ",a,b", "a,", ",,a", r"a\;b,c"],
        "PREPEND": ["a;b,c,d;e", ",a,b", "a,", ",,a"],
        "INSERT": ["a;b;c,-1,x", "a;b,2,x", "a;b,3,x", ",-1,x", ",0,x", "a;b,0,,x", "a;b,0,x;y"],
        "POP_BACK": ["a;b;c", "", "a;b;", ";"],
        "POP_FRONT": ["a;b;c", "", ";a;b", ";"],
        "REMOVE_DUPLICATES": ["a;a;b;a", "a;;b;;a", ""],
        "REMOVE_ITEM": ["a;;b;c;,", "a;b;c,b;c", "a;b;b;c,b,b", "a;b;A,A"],
        "REMOVE_AT": ["a;b;c,-1;0,-1", ",0", "a;b,2", "a;b,;", "a;b,abs"],
        "FILTER": [";A;AB;B;,INCLUDE,^A", ";A;AB;B;,EXCLUDE,^A", "a,BAD,r", "one,INCLUDE,["],
        "TRANSFORM": [
            "a;b,APPEND,_", "a;b,PREPEND,_,AT,-1,0", "a;b,TOUPPER,AT,0,0",
            "a;b,APPEND,_,AT,0,0", "a;b,APPEND,_,FOR,0,-1,2", "a;B,TOLOWER",
            " a ;  b,STRIP", "a;B;A;AA,TOLOWER,REGEX,^A", r"ab;bc,REPLACE,([a-z]),<\1$<ANGLE-R>",
            "aa;ba,REPLACE,^a,X", "aa;ba,REPLACE,$,X", "aa;ba,REPLACE,a*,X",
            r"a;B,REPLACE,([a-z]),\3", "a;b,TOUPPER,FOR,1,0", "a;b,TOUPPER,FOR,0,1,0",
            "a;b,TOUPPER,FOR,0", "a;b,TOUPPER,AT,-5", "a;b,TOUPPER,REGEX,[",
            "a;b,TOUPPER,REGEX", "a;b,TOUPPER,REGEX,a,REGEX,b", "a;b,TOUPPER,AT",
            "a;b,TOUPPER,AT,other", "a;b,APPEND", "a;b,UNKNOWN", ",UNKNOWN", "a,TOUPPER,foo",
        ],
        "REVERSE": ["a;;b;", ""],
        "SORT": ["a;B;b;A", "a;B;b;A,CASE:INSENSITIVE,ORDER:DESCENDING",
            "a10;a2;a1;a01;a001,COMPARE:NATURAL", "d/f;a/z;q/a,COMPARE:FILE_BASENAME",
            "a;b,ORDER:ASCENDING,ORDER:DESCENDING", "a;b,CASE:foo", "a;b,COMPARE:nope",
            "a;b,ORDER:nope", "a;b,UNKNOWN"],
        "UNKNOWN": ["abc"],
    }.items():
        yield from expressions("LIST", [f":{op},{args}" for args in arguments])
    for op, arguments in {
        "GET_ROOT_NAME": ["/", "C:/a", "//root/a", "/a;/b"],
        "GET_ROOT_DIRECTORY": ["/;a", "///tmp", "C:/a"],
        "GET_ROOT_PATH": ["//tmp", "/;a"],
        "GET_FILENAME": ["/root/file.txt;here;where/", "a/.", "a/..", ""],
        "GET_EXTENSION": ["/root/a.tar.gz;a/.hidden;.hidden.a;bare;dot.", "LAST_ONLY,a.tar.gz", "a/..", "LAST_ONLY"],
        "GET_STEM": ["/a/a.tar.gz;.hidden;.hidden.a;a/.", "LAST_ONLY,a.tar.gz", "foo.", "LAST_ONLY"],
        "GET_RELATIVE_PART": ["/foo/bar;foo;///bar//x", "/"],
        "GET_PARENT_PATH": ["a/b;a/;///;a//b//", "a//b/c", "", "/a"],
        "HAS_ROOT_NAME": ["/", "C:/foo"],
        "HAS_ROOT_DIRECTORY": ["/", "foo"],
        "HAS_ROOT_PATH": ["/", "foo"],
        "HAS_FILENAME": ["/", "foo", "foo/", "."],
        "HAS_EXTENSION": [".hidden", "foo.", "..", "a.b"],
        "HAS_STEM": ["..", "/", "a/"],
        "HAS_RELATIVE_PART": ["/foo", "///"],
        "HAS_PARENT_PATH": ["/", "a", "a/", ""],
        "IS_ABSOLUTE": ["/a", "relative", "C:/a", "~/x"],
        "IS_RELATIVE": ["/a", "relative", ""],
        "IS_PREFIX": ["/a,/a/b", "/a,/ab", "/a/,/a", "/a/,/a/b", "/a/,/a/", ",/a", "NORMALIZE,/a/../b,/b/c", "NORMALIZE"],
        "CMAKE_PATH": ["a//b//", "///foo", "", "NORMALIZE,/a//b/../x/."],
        "NATIVE_PATH": ["a//b//", "///foo", "NORMALIZE,a/b/../."],
        "APPEND": ["a;b,child,leaf", "a,b,/override,c", "a,,b", ",a", "a", "a;z,"],
        "REMOVE_FILENAME": ["/a/a.txt;a//b;only;/a/", ""],
        "REPLACE_FILENAME": ["/a/a.txt,new", "file,/absolute", "/a/,new", "/a/a.txt,", "/a/,new/leaf"],
        "REMOVE_EXTENSION": ["a.tar.gz;.hidden.x;b;", "LAST_ONLY,a.tar.gz", "a/..", "a/"],
        "REPLACE_EXTENSION": ["a.tar.gz,zip", "LAST_ONLY,a.tar.gz,.xz", "a/,txt", "a/..,txt", ".hidden,.txt", "/a/file.old,"],
        "NORMAL_PATH": ["/a//b/../c/.", "a/../../b/../", "a/..", "/..", "a/.//", "a/b/..", "a/../..", "./", "a/../b;./x"],
        "RELATIVE_PATH": ["/a/b/c,/a/d", "a,a", "/a,b", "a,b/..", "a,b/../..", "/a/b/,/a/c", "/a/b/,/a/b"],
        "ABSOLUTE_PATH": ["a,b", "a,/b", "a/../x,/b", "NORMALIZE,a/../x,/b", "/already,/b", "a;b,/base", "NORMALIZE"],
        "UNKNOWN": ["abc"],
    }.items():
        yield from expressions("PATH", [f":{op},{args}" for args in arguments])
    for expression in [
        "literal", "before:$<1:middle>after", "before:$<no end", "$<1:$<LOWER_CASE:A>>",
        "$<$<BOOL:no>:ignored>", "$<$<BOOL:yes>:hello,world>", "$<UNKNOWN:arg>", "$<>",
        "$<1:unclosed-$<COMMA>", "$<1:unclosed-$<COMMA>,", "before>$<x",
        "$<1:a,b:$<1:c,d>>", "$<1:>${missing}", "$<STRE$<1:QUAL>:x,x>",
    ]:
        yield expression, {}


TARGETS = {
    "a": {"type": "INTERFACE", "properties": {
        "INTERFACE_LINK_LIBRARIES": "b;$<LINK_ONLY:c>;b", "LABEL": "A",
        "INTERFACE_CUSTOM": "a", "TRANSITIVE_COMPILE_PROPERTIES": "CUSTOM",
        "RAW": "$<1:rewritten>", "LITERAL": "a;b",
        "INTERFACE_COMPILE_DEFINITIONS": "A;$<TARGET_PROPERTY:LABEL>;;",
        "INTERFACE_LINK_OPTIONS": "A_LINK", "INTERFACE_INCLUDE_DIRECTORIES": "/a",
    }},
    "b": {"type": "INTERFACE", "properties": {
        "INTERFACE_COMPILE_DEFINITIONS": "B;$<TARGET_PROPERTY:LABEL>", "LABEL": "B",
        "INTERFACE_LINK_OPTIONS": "B_LINK", "INTERFACE_INCLUDE_DIRECTORIES": "/b",
        "INTERFACE_LINK_LIBRARIES": "d", "INTERFACE_CUSTOM": "b",
    }},
    "c": {"type": "INTERFACE", "properties": {
        "INTERFACE_COMPILE_DEFINITIONS": "C", "INTERFACE_LINK_OPTIONS": "C_LINK",
        "INTERFACE_INCLUDE_DIRECTORIES": "/c",
    }},
    "d": {"type": "INTERFACE", "properties": {
        "INTERFACE_LINK_LIBRARIES": "a", "INTERFACE_COMPILE_DEFINITIONS": "D",
        "INTERFACE_LINK_OPTIONS": "D_LINK", "INTERFACE_INCLUDE_DIRECTORIES": "/d",
    }},
    "cycle": {"type": "INTERFACE", "properties": {}},
    "imp": {"type": "UNKNOWN", "imported": True, "properties": {
        "IMPORTED_LOCATION_RELEASE": "/opt/libsample.so.1", "IMPORTED_LOCATION": "/opt/default.so",
        "MAP_IMPORTED_CONFIG_DEBUG": "Release",
    }},
    "obj": {"type": "OBJECT", "imported": True, "properties": {
        "IMPORTED_OBJECTS": "/opt/a.o;/opt/b.o",
    }},
    "shared": {"type": "SHARED", "imported": True, "properties": {
        "IMPORTED_LOCATION": "/opt/libshared.so.1.2", "IMPORTED_SONAME": "libshared.so.1",
    }},
}
ALIASES = {"alias": "a"}


def capture(oracle, root, case, setup):
    script = ["cmake_minimum_required(VERSION 4.1)", "project(x NONE)"]
    for target, value in TARGETS.items():
        imported = " IMPORTED" if value.get("imported") else ""
        script.append(f"add_library({target} {value['type']}{imported})")
        props = value["properties"] | setup.get("properties", {}).get(target, {})
        for prop, val in props.items():
            script.append(f"set_property(TARGET {target} PROPERTY {prop} {bracket(val)})")
    for alias, target in ALIASES.items():
        script.append(f"add_library({alias} ALIAS {target})")
    head = " TARGET " + setup["head"] if "head" in setup else ""
    script.append(f"file(GENERATE OUTPUT out.txt CONTENT {bracket(case)}{head})")
    (root / "CMakeLists.txt").write_text("\n".join(script) + "\n")
    output = root / "b" / "out.txt"
    output.unlink(missing_ok=True)
    run = subprocess.run([oracle, "-G", "Ninja", "-S", root, "-B", root / "b",
                          f"-DCMAKE_BUILD_TYPE={setup.get('config', '')}"],
                         capture_output=True, text=True, check=False)
    if run.returncode == 0:
        return {"output": output.read_text()}
    marker = "  Error evaluating generator expression:\n"
    if marker not in run.stderr:
        raise RuntimeError(f"{case}: {run.returncode}\n{run.stderr}")
    error = run.stderr[run.stderr.index(marker):].split("\n\n\n", 1)[0] + "\n"
    return {"error": error}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("oracle", type=Path)
    args = parser.parse_args()
    version = subprocess.check_output([args.oracle, "--version"], text=True).splitlines()[0]
    if version != "cmake version 4.1.0":
        raise SystemExit(f"Expected CMake 4.1.0, found {version}")
    inventory = []
    with tempfile.TemporaryDirectory(prefix="genex-oracle-") as work:
        for expression, setup in cases():
            expected = capture(args.oracle.resolve(), Path(work), expression, setup)
            inventory.append({"expression": expression, "setup": setup, **expected})
    path = Path(__file__).resolve().parent
    document = {"variables": {"CMAKE_SYSTEM_NAME": "Linux"}, "targets": TARGETS,
                "aliases": ALIASES, "cases": inventory}
    (path / "genex_corpus.json").write_text(json.dumps(document, ensure_ascii=False, indent=2) + "\n")
    inventory_file = path / "../../src/genex/corpus_tests.rs"
    with inventory_file.open("w") as tests:
        tests.write("// Generated test inventory from tests/data/capture_genex_corpus.py.\n")
        tests.write("oracle_cases! {\n")
        for i in range(len(inventory)):
            tests.write(f"    oracle_{i:03} => {i},\n")
        tests.write("}\n")
    print(f"Captured {len(inventory)} oracle runs")


if __name__ == "__main__":
    main()
