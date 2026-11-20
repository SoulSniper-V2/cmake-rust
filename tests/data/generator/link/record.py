#!/usr/bin/env python3
"""Configure, build, and record the CMake 4.1.0 Ninja link fixtures."""
import concurrent.futures
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile

OUTPUT = Path(__file__).resolve().parent
ORACLE = sys.argv[1]
KEY = re.compile(r"^CMAKE_(?:BUILD_TYPE$|EXECUTABLE_FORMAT$|INSTALL_PREFIX$|STAGING_PREFIX$|SYSROOT|SKIP_|NO_BUILTIN_CHRPATH$|LIBRARY_(?:ARCHITECTURE|PATH_.*)$|(?:STATIC|SHARED|IMPORT)_LIBRARY_(?:PREFIX|SUFFIX)$|EXTRA_(?:LINK_EXTENSIONS|SHARED_LIBRARY_SUFFIXES)$|PLATFORM_(?:IMPLICIT_LINK|RUNTIME|REQUIRED_RUNTIME|USES_PATH)|(?:EXE|EXECUTABLE|SHARED_LIBRARY|SHARED_MODULE)_(?:LINK|RPATH|RUNTIME)|LINK_|(?:C|CXX)_(?:LINK|IMPLICIT_LINK|PLATFORM_LINKER|STANDARD_LIBRARIES|USE_IMPLICIT))")
FIXTURES = []


class Project:
    def __init__(self, name, config="", policy="NEW"):
        self.name = name
        self.config = config
        self.policy = policy
        self.code = ["cmake_minimum_required(VERSION 4.1)", "project(probe LANGUAGES C)",
                     f"cmake_policy(SET CMP0179 {policy})"]
        self.targets = {}
        self.files = {}
        FIXTURES.append(self)

    def target(self, name, kind="EXECUTABLE", imported=False, directory=""):
        data = {"kind": kind, "imported": imported, "properties": {}, "directory": directory}
        self.targets[name] = data
        if kind == "EXECUTABLE":
            self.code.append(f"add_executable({name} main.c)")
        elif kind == "UTILITY":
            self.code.append(f"add_custom_target({name})")
        else:
            source = "" if kind == "INTERFACE" or imported else " empty.c"
            external = " IMPORTED" if imported else ""
            self.code.append(f"add_library({name} {kind}{external}{source})")
        return self

    def props(self, target, **properties):
        for key, value in properties.items():
            self.code.append(f'set_property(TARGET {target} PROPERTY {key} "{value}")')
            self.targets[target]["properties"][key] = value
        return self

    def set(self, variable, value):
        self.code.append(f'set({variable} "{value}")')
        return self

    def link(self, name, visibility, *items, evaluated=None):
        self.code.append(f"target_link_libraries({name} {visibility} {' '.join(items)})")
        items = items if evaluated is None else evaluated
        props = self.targets[name]["properties"]
        if visibility != "INTERFACE":
            self.append(props, "LINK_LIBRARIES", items)
        if visibility != "PRIVATE":
            self.append(props, "INTERFACE_LINK_LIBRARIES", items)
        elif self.targets[name]["kind"] in ("STATIC", "OBJECT"):
            self.append(props, "INTERFACE_LINK_LIBRARIES", items)
        return self

    @staticmethod
    def append(props, key, items):
        values = [props[key]] if props.get(key) else []
        props[key] = ";".join([*values, *items])

    def install(self, name):
        self.code.append(f"install(TARGETS {name} DESTINATION bin)")
        self.targets[name]["properties"]["HAVE_INSTALL_RULE"] = "ON"
        return self

    def add_dependency(self, name, dependency):
        self.code.append(f"add_dependencies({name} {dependency})")
        self.append(self.targets[name]["properties"], "MANUALLY_ADDED_DEPENDENCIES", [dependency])
        return self


for left in ("STATIC", "SHARED", "OBJECT", "INTERFACE"):
    for right in ("STATIC", "SHARED", "OBJECT", "INTERFACE"):
        for scope in ("PUBLIC", "INTERFACE"):
            p = Project(f"{left.lower()}_{right.lower()}_{scope.lower()}")
            p.target("a", left).target("b", right).target("app")
            p.link("a", "INTERFACE" if left == "INTERFACE" else scope, "b").link(
                "app", "PUBLIC" if left == "INTERFACE" and scope == "PUBLIC" else "PRIVATE", "a")

for count in (2, 3, 4):
    for start in ("a", "b", "both"):
        p = Project(f"cycle_{count}_{start}")
        p.target("a", "STATIC").target("b", "STATIC").target("app")
        p.props("a", LINK_INTERFACE_MULTIPLICITY=str(count))
        p.link("a", "PRIVATE", "b").link("b", "PRIVATE", "a")
        p.link("app", "PRIVATE", *(("a", "b") if start == "both" else (start,)))

for config in ("Debug", "Release", "RelWithDebInfo", ""):
    p = Project(f"configuration_{config.lower() or 'empty'}", config)
    p.target("d", "STATIC").target("o", "STATIC").target("g", "STATIC").target("app")
    p.link("app", "PRIVATE", "debug", "d", "optimized", "o", "general", "g",
           evaluated=("d" if config == "Debug" else "o", "g"))

for mode in ("default", "skip_build", "skip_all", "origin", "build", "install",
             "pad", "install_skip", "target_install"):
    p = Project(f"rpath_{mode}").target("a", "SHARED").target("app")
    p.props("a", LIBRARY_OUTPUT_DIRECTORY="${CMAKE_BINARY_DIR}/lib")
    p.link("app", "PRIVATE", "a")
    if mode == "skip_build":
        p.props("app", SKIP_BUILD_RPATH="ON")
    if mode == "skip_all":
        p.set("CMAKE_SKIP_RPATH", "ON")
    if mode == "origin":
        p.props("app", BUILD_RPATH_USE_ORIGIN="ON")
    if mode == "build":
        p.props("app", BUILD_RPATH="/opt/custom;/opt/other;/opt/custom")
    if mode in ("install", "pad", "install_skip", "target_install"):
        p.props("app", INSTALL_RPATH="/opt/install/long_directory_name")
        p.install("app")
    if mode in ("install", "install_skip"):
        p.props("app", BUILD_WITH_INSTALL_RPATH="ON")
    if mode == "install_skip":
        p.set("CMAKE_SKIP_INSTALL_RPATH", "ON")
    if mode == "target_install":
        p.props("app", INSTALL_RPATH="/opt/local")

for mode in ("default", "order_forward", "reverse_first", "reverse_last", "all_old", "none", "free"):
    p = Project(f"dedup_{mode}", policy="OLD" if mode == "reverse_last" else "NEW")
    p.target("a", "STATIC").target("b", "STATIC").target("s", "SHARED").target("app")
    p.link("a", "PRIVATE", "b", "s").link("app", "PRIVATE", "b", "s", "a", "a")
    if mode == "free":
        p.props("app", LINK_LIBRARIES_STRATEGY="REORDER_FREELY")
    elif mode == "all_old":
        p.code.append("cmake_policy(SET CMP0156 OLD)")
        p.code.insert(3, p.code.pop())
        p.policy = "CMP0156_OLD"
        p.set("CMAKE_C_LINK_LIBRARIES_PROCESSING", "ORDER=FORWARD;DEDUPLICATION=ALL")
    elif mode != "default":
        order = "FORWARD" if mode == "order_forward" else "REVERSE"
        dedup = "NONE" if mode == "none" else "ALL"
        p.set("CMAKE_C_LINK_LIBRARIES_PROCESSING", f"ORDER={order};DEDUPLICATION={dedup}")

for mode in ("whole", "group", "group_replace", "override", "custom", "custom_name"):
    p = Project(f"feature_{mode}").target("a", "STATIC").target("b", "STATIC").target("app")
    if mode in ("whole", "override"):
        p.link("app", "PRIVATE", '"$<LINK_LIBRARY:WHOLE_ARCHIVE,a,b>"',
               evaluated=("<LINK_LIBRARY:WHOLE_ARCHIVE>", "a", "b", "</LINK_LIBRARY:WHOLE_ARCHIVE>"))
        if mode == "override":
            p.link("app", "PRIVATE", "a").props("app", LINK_LIBRARY_OVERRIDE_a="WHOLE_ARCHIVE")
    elif mode in ("group", "group_replace"):
        p.link("app", "PRIVATE", '"$<LINK_GROUP:RESCAN,a,b>"',
               evaluated=("<LINK_GROUP:RESCAN:a|b>", "a", "b", "</LINK_GROUP:RESCAN>"))
        if mode == "group_replace":
            p.link("app", "PRIVATE", "a")
    else:
        p.set("CMAKE_C_LINK_LIBRARY_USING_wrap_SUPPORTED", "ON")
        p.set("CMAKE_C_LINK_LIBRARY_USING_wrap", "LINKER:--push-state;PATH{<LIBRARY>}NAME{<LINK_ITEM>};LINKER:--pop-state")
        item = "a" if mode == "custom" else "m"
        p.link("app", "PRIVATE", f'"$<LINK_LIBRARY:wrap,{item}>"',
               evaluated=("<LINK_LIBRARY:wrap>", item, "</LINK_LIBRARY:wrap>"))

for mode in ("static", "shared", "nosoname", "mapping", "legacy", "interface"):
    p = Project(f"imported_{mode}", "Debug").target("app")
    kind = {"shared": "SHARED", "nosoname": "SHARED", "interface": "INTERFACE"}.get(mode, "STATIC")
    p.target("external", kind, imported=True)
    path = "/usr/lib/x86_64-linux-gnu/libm.so.6" if kind == "SHARED" else "${CMAKE_BINARY_DIR}/libexternal.a"
    if mode == "mapping":
        p.props("external", MAP_IMPORTED_CONFIG_DEBUG="Release", IMPORTED_LOCATION_RELEASE=path,
                IMPORTED_LINK_INTERFACE_LIBRARIES_RELEASE="m")
    elif mode == "interface":
        p.props("external", IMPORTED_LIBNAME="m")
    else:
        p.props("external", IMPORTED_LOCATION=path)
    if mode == "nosoname":
        path = "${CMAKE_BINARY_DIR}/external/libexternal.so"
        p.props("external", IMPORTED_LOCATION=path, IMPORTED_NO_SONAME="ON")
    if mode == "legacy":
        p.props("external", IMPORTED_LINK_INTERFACE_LIBRARIES="m")
    p.link("app", "PRIVATE", "external")

for mode in ("name", "explicit", "full", "dirs", "switches", "no_shared_dep",
             "link_depends", "shared_chain", "module", "utility", "inject", "exclude",
             "soname", "subdir", "escaped_path"):
    p = Project(f"link_{mode}").target("app")
    if mode == "name":
        p.link("app", "PRIVATE", "m", "pthread", "dl")
    elif mode == "explicit":
        p.link("app", "PRIVATE", "-lm", "-pthread", "-Wl,--no-undefined", "LINKER:--as-needed")
    elif mode == "full":
        p.link("app", "PRIVATE", "/usr/lib/x86_64-linux-gnu/libm.so.6")
    elif mode == "dirs":
        p.target("i", "INTERFACE").link("app", "PRIVATE", "i")
        p.props("app", LINK_DIRECTORIES="${CMAKE_BINARY_DIR}/search;/usr/lib")
        p.props("i", INTERFACE_LINK_DIRECTORIES="/opt/somewhere")
    elif mode == "switches":
        p.props("app", LINK_SEARCH_END_STATIC="OFF")
        p.link("app", "PRIVATE", "libm.a", "libm.so", "m")
    elif mode == "no_shared_dep":
        p.target("s", "SHARED").props("app", LINK_DEPENDS_NO_SHARED="ON").link("app", "PRIVATE", "s")
    elif mode == "link_depends":
        p.props("app", LINK_DEPENDS="${CMAKE_SOURCE_DIR}/empty.c;${CMAKE_SOURCE_DIR}/main.c")
    elif mode == "shared_chain":
        p.target("s", "SHARED").target("t", "SHARED")
        p.props("s", LIBRARY_OUTPUT_DIRECTORY="${CMAKE_BINARY_DIR}/s")
        p.props("t", LIBRARY_OUTPUT_DIRECTORY="${CMAKE_BINARY_DIR}/t")
        p.link("s", "PRIVATE", "t").link("app", "PRIVATE", "s")
    elif mode == "module":
        p.target("s", "SHARED").target("plugin", "MODULE")
        p.link("plugin", "PRIVATE", "s")
    elif mode == "utility":
        p.target("stage", "UTILITY").add_dependency("app", "stage")
    elif mode in ("inject", "exclude"):
        p.target("a", "STATIC").target("b", "STATIC")
        p.props("a", INTERFACE_LINK_LIBRARIES_DIRECT="b")
        if mode == "exclude":
            p.props("a", INTERFACE_LINK_LIBRARIES_DIRECT_EXCLUDE="a")
        p.link("app", "PRIVATE", "a")
    elif mode == "soname":
        p.target("s", "SHARED").props("s", VERSION="2.3.4", SOVERSION="2").link("app", "PRIVATE", "s")
    elif mode == "subdir":
        p.files["sub/CMakeLists.txt"] = "add_library(a STATIC ../empty.c)\ntarget_link_libraries(app PRIVATE a)\n"
        p.code.append("add_subdirectory(sub)")
        p.targets["a"] = {"kind": "STATIC", "imported": False, "properties": {}, "directory": "sub"}
        p.targets["app"]["properties"]["LINK_LIBRARIES"] = "::@(0x1);a;::@"
    elif mode == "escaped_path":
        p.target("a", "STATIC").props("a", ARCHIVE_OUTPUT_DIRECTORY="${CMAKE_BINARY_DIR}/space dir").link("app", "PRIVATE", "a")


def record(project):
    with tempfile.TemporaryDirectory(prefix="cmake-link-oracle-") as temp:
        root = Path(temp)
        source, build = root / "src", root / "build"
        source.mkdir()
        build.mkdir()
        (build / "external").mkdir()
        subprocess.run(["ar", "rcs", str(build / "libexternal.a")], check=True)
        (source / "main.c").write_text("int main(void) { return 0; }\n")
        (source / "empty.c").write_text("static int unused;\n")
        for path, value in project.files.items():
            file = source / path
            file.parent.mkdir(parents=True, exist_ok=True)
            file.write_text(value)
        subprocess.run(["cc", "-fPIC", "-shared", str(source / "empty.c"),
                        "-o", str(build / "external/libexternal.so")], check=True)
        dump = """
get_cmake_property(all VARIABLES)
file(WRITE "${CMAKE_BINARY_DIR}/variables.txt" "")
foreach(key IN LISTS all)
  if(key MATCHES "^CMAKE_")
    file(APPEND "${CMAKE_BINARY_DIR}/variables.txt" "${key}=${${key}}\n")
  endif()
endforeach()
"""
        cmakelists = "\n".join(project.code) + "\n" + dump
        (source / "CMakeLists.txt").write_text(cmakelists)
        configured = subprocess.run([ORACLE, "-S", str(source), "-B", str(build), "-G", "Ninja",
                                     f"-DCMAKE_BUILD_TYPE={project.config}"], text=True, capture_output=True)
        if configured.returncode:
            raise RuntimeError(f"{project.name}: {configured.stderr}")
        built = subprocess.run([ORACLE, "--build", str(build), "-j", "1"], text=True, capture_output=True)
        if built.returncode:
            raise RuntimeError(f"{project.name}: {built.stdout}\n{built.stderr}")
        compiler = {}
        for file in (build / "CMakeFiles" / "4.1.0").glob("*.cmake"):
            compiler.update(re.findall(r'^set\((CMAKE_[A-Z_a-z0-9]+) "(.*)"\)$', file.read_text(), re.M))
        cache = dict(re.findall(r"^(CMAKE_[^:=]+):[^=]+=(.*)$", (build / "CMakeCache.txt").read_text(), re.M))
        variables = dict(re.findall(r"^(CMAKE_[^=]+)=(.*)$", (build / "variables.txt").read_text(), re.M))
        variables = {key: value for key, value in (cache | compiler | variables).items() if KEY.match(key)}

        edges = {}
        link = re.compile(r"^build (.*?): (?:C_(?:EXECUTABLE|STATIC_LIBRARY|SHARED_LIBRARY|MODULE_LIBRARY)_LINKER__)([^ ]+)(.*)$")
        current = None
        for line in (build / "build.ninja").read_text().splitlines():
            if match := link.match(line):
                output, rule, inputs = match.groups()
                name = rule.split("_", 1)[0]
                current = {"output": output, "explicit": [], "implicit": [], "order_only": [],
                           "LINK_LIBRARIES": "", "LINK_PATH": ""}
                edges[name] = current
                tokens = re.findall(r"(?:\$[ :$]|[^ ])+", inputs)
                key = "explicit"
                for token in tokens:
                    if token in ("|", "||"):
                        key = "implicit" if token == "|" else "order_only"
                    else:
                        current[key].append(token)
            elif current and line.startswith(("  LINK_LIBRARIES = ", "  LINK_PATH = ")):
                key, value = line.strip().split(" = ", 1)
                current[key] = value
            elif not line.startswith("  "):
                current = None
        def normalize(value):
            return value.replace(str(root), "@ROOT@").replace("${CMAKE_BINARY_DIR}", "@ROOT@/build").replace("${CMAKE_SOURCE_DIR}", "@ROOT@/src")
        data = {"config": project.config, "policy": project.policy, "cmakelists": project.code,
                "targets": project.targets, "variables": variables, "edges": edges}
        (OUTPUT / (project.name + ".json")).write_text(normalize(json.dumps(data, indent=2, sort_keys=True)) + "\n")


assert len(FIXTURES) >= 60 and len({p.name for p in FIXTURES}) == len(FIXTURES)
with concurrent.futures.ThreadPoolExecutor(max_workers=6) as pool:
    list(pool.map(record, FIXTURES))
print(f"Recorded {len(FIXTURES)} built projects")
