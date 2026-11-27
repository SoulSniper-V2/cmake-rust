#!/usr/bin/env python3
"""Differential fuzzing of installation and package export.

Each case builds a random C/C++ project exercising install(TARGETS/FILES/
PROGRAMS/DIRECTORY/SCRIPT/CODE/EXPORT/PACKAGE_INFO/RUNTIME_DEPENDENCY_SET),
export(TARGETS/EXPORT/PACKAGE_INFO/PACKAGE), CMakePackageConfigHelpers
(configure_package_config_file / write_basic_package_version_file),
components (including names with spaces), SOVERSION/VERSION symlinks,
RPATH/INSTALL_RPATH, DESTDIR, CMAKE_INSTALL_MESSAGE, and policies
(CMP0022/CMP0082/CMP0087/CMP0095/CMP0177).

Both the Kitware oracle and the Rust candidate:
1. Configure the project (Ninja or Ninja Multi-Config) with file-API queries
   and compare exit code, normalized configure output, and the entire
   configured build tree (including cmake_install.cmake and export files) via
   treediff.py.
2. Build with ninja and compare build exit codes.
3. Run `cmake --install` with randomized CLI flags (--prefix, --component,
   --config, --strip, --default-directory-permissions, -v, -j) and env vars
   (DESTDIR, CMAKE_INSTALL_ALWAYS, CMAKE_INSTALL_MODE) — plus an optional
   second incremental install — comparing exit codes, normalized output,
   install manifests, and the installed tree (names, symlink targets, modes,
   text contents, binary presence/modes).
4. If packages were exported, run a consumer project that find_package()s
   both the installed package and the build-tree exported package, links the
   exported targets, and builds with ninja, comparing consumer configure
   output/tree and build exit code.

Usage: fuzz_install.py --candidate <staged bin/cmake> [--seed N] [-n COUNT] [-j N] [--generator G]
"""
import argparse
import concurrent.futures
import difflib
import hashlib
import importlib.util
import json
import os
import random
import re
import shutil
import stat
import subprocess
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("treediff", HERE / "treediff.py")
treediff = importlib.util.module_from_spec(spec)
spec.loader.exec_module(treediff)

CPS_EXPORT_UUID = "b80be207-778e-46ba-8080-b23bba22639e"
CPS_FIND_UUID = "e82e467b-f997-4464-8ace-b00808fff261"
PKG_DEPS_UUID = "1942b4fa-b2c5-4546-9385-83f254070067"

PERMS_CHOICES = [
    "OWNER_READ OWNER_WRITE",
    "OWNER_READ OWNER_WRITE OWNER_EXECUTE GROUP_READ GROUP_EXECUTE WORLD_READ WORLD_EXECUTE",
    "OWNER_READ GROUP_READ WORLD_READ",
    "OWNER_READ OWNER_EXECUTE GROUP_READ WORLD_READ",
]
DIR_PERMS_CLI = [
    "u=rwx,g=rx,o=rx",
    "u=rwx,g=rx,o=",
    "u=rwx,g=,o=",
    "u=rx,g=rx,o=rx",
]
COMPONENTS = [
    "",
    "Runtime",
    "Development",
    "Docs",
    "Runtime Libs",
    "Dev Headers",
    "c++_dev@1",
]
INSTALL_TYPES = [
    "BIN", "SBIN", "LIB", "INCLUDE", "SYSCONF",
    "SHAREDSTATE", "LOCALSTATE", "RUNSTATE",
    "DATA", "INFO", "LOCALE", "MAN", "DOC", "LIBEXEC",
]


def q(text: str) -> str:
    return (
        '"'
        + text.replace("\\", "\\\\")
        .replace('"', '\\"')
        .replace("$", "\\$")
        .replace(";", "\\;")
        + '"'
    )


def sanitize_ident(name: str) -> str:
    return re.sub(r"[^A-Za-z0-9_]", "_", name)


class ProjectGen:
    def __init__(self, rng: random.Random, generator: str, index: int):
        self.rng = rng
        self.generator = generator
        self.index = index
        self.multi_config = generator == "Ninja Multi-Config"
        self.cxx = rng.random() < 0.45
        self.cps_mode = rng.random() < 0.25
        self.use_gnu_install_dirs = rng.random() < 0.45
        self.use_pkg_helpers = (not self.cps_mode) and (rng.random() < 0.75)
        self.pkg_name = rng.choice(["DemoPkg", "alpha_lib", "WidgetCore", "MyMod"])
        self.dep_pkg_name = f"{self.pkg_name}Dep"
        self.pkg_version = rng.choice(["1.2.3", "2.0.0", "0.4.1", "3.1"])
        self.compat_mode = rng.choice(["AnyNewerVersion", "SameMajorVersion", "SameMinorVersion", "ExactVersion"])
        self.arch_independent = rng.random() < 0.3
        self.namespace = rng.choice([f"{self.pkg_name}::", "ns::", ""]) if not self.cps_mode else f"{self.pkg_name}::"
        self.use_old_cmp0022 = (not self.cps_mode) and rng.random() < 0.15
        self.split_dep_set = (not self.cps_mode) and not self.use_old_cmp0022 and rng.random() < 0.3
        self.install_parallel = rng.random() < 0.25
        self.cmp0082 = rng.choice(["NEW", "NEW", "OLD"])
        self.cmp0087 = rng.choice(["NEW", "NEW", "OLD"])
        self.cmp0095 = rng.choice(["NEW", "NEW", "OLD"])
        self.cmp0177 = rng.choice(["NEW", "NEW", "OLD"])

        self.files: dict[str, object] = {}  # str | tuple("link", target) | tuple("bin", bytes, mode)
        self.dirs: dict[str, list[str]] = {"": []}
        self.post_subdir: list[str] = []
        self.targets: list[dict] = []
        self.used_components: set[str] = set()
        self.default_component = rng.choice(["", "", "Core", "App Core"])
        self.configs = ["Debug", "Release"] if self.multi_config else [rng.choice(["Debug", "Release", "RelWithDebInfo", ""])]
        self.export_set_name = f"{self.pkg_name}Targets"
        self.dep_export_set_name = f"{self.dep_pkg_name}Targets"
        self.exported_targets: list[dict] = []
        self.dep_exported_targets: list[dict] = []
        self.has_build_tree_package = False
        self.has_install_tree_package = False
        self.build_export_files: list[str] = []
        self.uses_user_registry = False
        self.uses_runtime_deps = False

    def add_text(self, rel: str, content: str, mode: int | None = None):
        self.files[rel] = (content, mode) if mode is not None else content

    def add_link(self, rel: str, target: str):
        self.files[rel] = ("link", target)

    def pick_comp(self, allow_empty=True) -> str:
        c = self.rng.choice(COMPONENTS if allow_empty else COMPONENTS[1:])
        if c:
            self.used_components.add(c)
        return c

    def comp_clause(self, comp: str, kw="COMPONENT") -> str:
        if not comp:
            return ""
        return f" {kw} {q(comp)}" if " " in comp or any(ch in comp for ch in "@+;") else f" {kw} {comp}"

    def config_clause(self) -> str:
        if self.rng.random() < 0.72:
            return ""
        if self.multi_config:
            choices = ["Debug", "Release", "Debug Release", "debug", "RELEASE"]
        else:
            choices = ["Debug", "Release", "Debug Release", "RelWithDebInfo"]
        return f" CONFIGURATIONS {self.rng.choice(choices)}"

    def perm_clause(self) -> str:
        if self.rng.random() < 0.75:
            return ""
        return f" PERMISSIONS {self.rng.choice(PERMS_CHOICES)}"

    def build_targets(self):
        rng = self.rng
        count = rng.randint(3, 6)
        subdirs = []
        if rng.random() < 0.65:
            subdirs.append("sub1")
        if rng.random() < 0.35:
            subdirs.append("sub2")
        for sd in subdirs:
            self.dirs[sd] = []

        # Create targets
        kinds_pool = ["STATIC", "SHARED", "OBJECT", "INTERFACE", "MODULE", "EXE"]
        # Ensure at least one STATIC or SHARED library for consumer linking.
        chosen_kinds = [rng.choice(["STATIC", "SHARED"])]
        for _ in range(count - 1):
            chosen_kinds.append(rng.choice(kinds_pool))

        for i, kind in enumerate(chosen_kinds):
            d = rng.choice(["", *subdirs]) if i > 0 else ""
            name = f"tgt_{i}"
            export_name = f"exp_{i}" if not self.cps_mode and rng.random() < 0.25 else name
            lines = self.dirs[d]
            ext = "cpp" if (self.cxx and rng.random() < 0.5) else "c"
            sym = f"fn_{name}"

            header_rel_dir = f"{d}/inc" if d else "inc"
            header_name = f"{name}_api.h"
            header_rel = f"{header_rel_dir}/{header_name}"
            self.add_text(header_rel, f"#ifndef {name.upper()}_H\n#define {name.upper()}_H\nint {sym}(void);\n#endif\n")

            if kind != "INTERFACE":
                src_name = f"{name}.{ext}"
                src_rel = f"{d}/{src_name}" if d else src_name
                if kind == "EXE":
                    body = f"int main(void) {{ return 0; }}\n"
                else:
                    body = f"int {sym}(void) {{ return {i + 1}; }}\n"
                self.add_text(src_rel, body)
                if kind == "EXE":
                    lines.append(f"add_executable({name} {src_name})")
                else:
                    lines.append(f"add_library({name} {kind} {src_name})")
            else:
                lines.append(f"add_library({name} INTERFACE)")

            enable_exports = (kind == "EXE" and rng.random() < 0.35)
            has_version = (kind == "SHARED" and rng.random() < 0.6) or (kind == "MODULE" and rng.random() < 0.2)
            props = []
            if export_name != name and not self.cps_mode:
                props.append(f"EXPORT_NAME {export_name}")
            if enable_exports:
                props.append("ENABLE_EXPORTS ON")
            if has_version:
                sv = rng.choice(["1", "2"])
                props.append(f"VERSION {sv}.{i}.0 SOVERSION {sv}")
            elif kind == "SHARED" and rng.random() < 0.2:
                props.append("NO_SONAME ON")
            if kind != "INTERFACE" and rng.random() < 0.3:
                props.append(f"OUTPUT_NAME out_{name}")
            if kind in ("STATIC", "SHARED") and rng.random() < 0.25:
                props.append("DEBUG_POSTFIX _dbg")
            if kind in ("SHARED", "EXE") and rng.random() < 0.45:
                # RPATH variations; keep build rpath >= install rpath when testing RPATH_CHANGE
                rpath_choice = rng.randrange(4)
                if rpath_choice == 0:
                    props.append('INSTALL_RPATH "$ORIGIN/../lib"')
                elif rpath_choice == 1:
                    props.append('BUILD_RPATH "/opt/very/long/build/rpath/entry" INSTALL_RPATH "$ORIGIN/../lib"')
                elif rpath_choice == 2:
                    props.append('INSTALL_RPATH "$ORIGIN/../lib" BUILD_WITH_INSTALL_RPATH ON')
                else:
                    props.append('SKIP_BUILD_RPATH ON INSTALL_RPATH "/opt/rp"')
                if rng.random() < 0.3:
                    props.append("INSTALL_REMOVE_ENVIRONMENT_RPATH ON")
            if self.use_old_cmp0022 and kind in ("STATIC", "SHARED") and rng.random() < 0.5:
                props.append('LINK_INTERFACE_LIBRARIES ""')
            if rng.random() < 0.2 and not self.cps_mode:
                props.append('DEPRECATION "Use something newer"')
            if rng.random() < 0.2 and not self.cps_mode:
                props.append('MY_CUSTOM_PROP "custom_val" EXPORT_PROPERTIES "MY_CUSTOM_PROP"')

            # PUBLIC_HEADER / PRIVATE_HEADER / RESOURCE
            has_pub_hdr = False
            has_priv_hdr = False
            has_res = False
            if kind in ("STATIC", "SHARED", "EXE") and rng.random() < 0.35:
                ph = f"{name}_pub.h"
                self.add_text(f"{d}/{ph}" if d else ph, f"#define {name.upper()}_PUB 1\n")
                props.append(f"PUBLIC_HEADER {ph}")
                has_pub_hdr = True
            if kind in ("STATIC", "SHARED") and rng.random() < 0.25:
                pvh = f"{name}_priv.h"
                self.add_text(f"{d}/{pvh}" if d else pvh, f"#define {name.upper()}_PRIV 1\n")
                props.append(f"PRIVATE_HEADER {pvh}")
                has_priv_hdr = True
            if kind in ("SHARED", "EXE") and rng.random() < 0.2:
                res = f"{name}.dat"
                self.add_text(f"{d}/{res}" if d else res, f"res_{name}\n")
                props.append(f"RESOURCE {res}")
                has_res = True

            if props:
                lines.append(f"set_target_properties({name} PROPERTIES {' '.join(props)})")

            scope = "INTERFACE" if kind == "INTERFACE" else rng.choice(["PUBLIC", "INTERFACE"])
            # Compile definitions
            if self.cps_mode:
                lines.append(f"target_compile_definitions({name} {scope} DEF_{name.upper()}=1)")
            else:
                if rng.random() < 0.5:
                    lines.append(f"target_compile_definitions({name} {scope} DEF_{name.upper()}=1 $<$<CONFIG:Debug>:DBG_{name.upper()}>)")
                else:
                    lines.append(f"target_compile_definitions({name} {scope} DEF_{name.upper()}=1)")

            # Include dirs / FILE_SET HEADERS
            use_file_set = rng.random() < 0.45
            has_file_set = False
            file_set_name = "HEADERS"
            if use_file_set:
                fs_sub = f"{header_rel_dir}/{name}_sub"
                fs_hdr = f"{fs_sub}/{name}_fs.h"
                self.add_text(fs_hdr, f"#define {name.upper()}_FS 1\n")
                file_set_name = "HEADERS" if rng.random() < 0.6 else f"fs_{name}"
                fs_type = "" if file_set_name == "HEADERS" else " TYPE HEADERS"
                fs_scope = "INTERFACE" if kind == "INTERFACE" else "PUBLIC"
                lines.append(
                    f"target_sources({name} {fs_scope} FILE_SET {file_set_name}{fs_type}"
                    f" BASE_DIRS ${{CMAKE_CURRENT_SOURCE_DIR}}/inc FILES {fs_hdr.removeprefix(d + '/') if d else fs_hdr})"
                )
                has_file_set = True
            elif not self.cps_mode:
                lines.append(
                    f"target_include_directories({name} {scope} "
                    f"$<BUILD_INTERFACE:${{CMAKE_CURRENT_SOURCE_DIR}}/inc> "
                    f"$<INSTALL_INTERFACE:include/{self.pkg_name}>)"
                )

            if kind != "INTERFACE" and rng.random() < 0.3:
                lines.append(f"target_compile_features({name} {scope} {'cxx_std_17' if self.cxx else 'c_std_11'})")

            self.targets.append({
                "name": name,
                "export_name": export_name,
                "kind": kind,
                "dir": d,
                "sym": sym,
                "enable_exports": enable_exports,
                "has_version": has_version,
                "has_file_set": has_file_set,
                "file_set_name": file_set_name,
                "has_pub_hdr": has_pub_hdr,
                "has_priv_hdr": has_priv_hdr,
                "has_res": has_res,
                "export_set": None,
                "link_deps": [],
                "install_objects": kind == "OBJECT" and rng.random() < 0.6,
            })

        # Add acyclic link dependencies
        for idx, t in enumerate(self.targets):
            if t["kind"] in ("OBJECT",):
                continue
            candidates = [u for u in self.targets[:idx] if u["kind"] not in ("EXE", "MODULE")]
            if not candidates:
                continue
            picks = rng.sample(candidates, min(len(candidates), rng.randint(0, 2)))
            lines = self.dirs[t["dir"]]
            for dep in picks:
                if dep["kind"] == "OBJECT":
                    if t["kind"] == "INTERFACE":
                        continue
                    if rng.random() < 0.5:
                        lines.append(f"target_sources({t['name']} PRIVATE $<TARGET_OBJECTS:{dep['name']}>)")
                    else:
                        lines.append(f"target_link_libraries({t['name']} PRIVATE {dep['name']})")
                        t["link_deps"].append(dep["name"])
                    continue
                if t["kind"] == "INTERFACE":
                    sc = "INTERFACE"
                elif t["kind"] == "EXE":
                    sc = "PRIVATE"
                else:
                    sc = rng.choice(["PUBLIC", "PRIVATE", "INTERFACE"])
                item = dep["name"]
                if dep["kind"] == "STATIC" and sc != "PRIVATE" and rng.random() < 0.25:
                    item = f"$<LINK_ONLY:{dep['name']}>"
                lines.append(f"target_link_libraries({t['name']} {sc} {item})")
                t["link_deps"].append(dep["name"])

        return subdirs

    def emit_installs_and_exports(self, subdirs: list[str]):
        rng = self.rng
        root = self.dirs[""]

        # Decide which targets go to export sets
        # Note: all targets transitively required by an exported target in PUBLIC/INTERFACE must be exported too.
        target_by_name = {t["name"]: t for t in self.targets}

        # Pick exportable targets (everything or most targets; skip MODULE if CPS? Wait: CPS supports module!)
        # If split_dep_set: first static/shared/interface target goes to dep_export_set, rest to main export_set
        export_candidates = [t for t in self.targets]
        if self.split_dep_set and len(export_candidates) >= 2:
            # Find a leaf target with no link_deps for dep set so dep set is self-contained
            leaves = [t for t in export_candidates if not t["link_deps"] and t["kind"] in ("STATIC", "SHARED", "INTERFACE")]
            if leaves:
                dep_root = leaves[0]
                dep_root["export_set"] = self.dep_export_set_name
                self.dep_exported_targets.append(dep_root)
        for t in export_candidates:
            if t["export_set"] is None:
                # OBJECT without OBJECTS DESTINATION transforms to INTERFACE on export
                # Note: in CPS mode, OBJECT targets without OBJECTS dest become interface; with OBJECTS dest, CPS isn't typed for objects
                if self.cps_mode and t["kind"] == "OBJECT" and t["install_objects"]:
                    t["install_objects"] = False
                t["export_set"] = self.export_set_name
                self.exported_targets.append(t)

        # Install targets
        for t in self.targets:
            d = t["dir"]
            # Sometimes install a subdirectory target from the root (CMake >= 3.13).
            # Such a command has to execute after add_subdirectory() created the target.
            install_dir = "" if (d and rng.random() < 0.25) else d
            lines = self.post_subdir if install_dir == "" and d else self.dirs[install_dir]

            comp_rt = self.pick_comp()
            comp_lib = self.pick_comp()
            comp_dev = self.pick_comp()
            nl_comp = self.pick_comp(allow_empty=False) if rng.random() < 0.5 else ""
            nl_mode = ""
            if t["kind"] == "SHARED" and t["has_version"] and rng.random() < 0.35:
                nl_mode = rng.choice(["NAMELINK_COMPONENT " + q(nl_comp) if nl_comp else "", "SPLIT_NL"])

            exp_clause = f" EXPORT {t['export_set']}" if t["export_set"] else ""
            opt_clause = " OPTIONAL" if rng.random() < 0.15 else ""
            excl_clause = " EXCLUDE_FROM_ALL" if rng.random() < 0.08 else ""
            cfg_clause = self.config_clause()
            # Keep exported targets unconditional / in all configs so consumer find_package works reliably
            if t["export_set"] and (excl_clause or cfg_clause):
                excl_clause = ""
                cfg_clause = ""

            lib_dest = "${CMAKE_INSTALL_LIBDIR}" if self.use_gnu_install_dirs else "lib"
            bin_dest = "${CMAKE_INSTALL_BINDIR}" if self.use_gnu_install_dirs else "bin"
            inc_dest = "${CMAKE_INSTALL_INCLUDEDIR}" if self.use_gnu_install_dirs else f"include/{self.pkg_name}"

            # Runtime dependency set / RUNTIME_DEPENDENCIES on executables (Linux-safe filters)
            rd_clause = ""
            if t["kind"] == "EXE" and not t["export_set"] and rng.random() < 0.4:
                self.uses_runtime_deps = True
                if rng.random() < 0.5:
                    rd_clause = (
                        ' RUNTIME_DEPENDENCIES PRE_EXCLUDE_REGEXES "^libc\\\\." "^libm\\\\." '
                        '"^libstdc\\\\+\\\\+\\\\." "^libgcc_s\\\\." "^ld-linux"'
                    )
                else:
                    rd_clause = " RUNTIME_DEPENDENCY_SET rdeps"
                    lines.append(
                        f"install(RUNTIME_DEPENDENCY_SET rdeps"
                        f' PRE_EXCLUDE_REGEXES "^libc\\\\." "^libm\\\\." "^libstdc\\\\+\\\\+\\\\." "^libgcc_s\\\\." "^ld-linux"'
                        f" LIBRARY DESTINATION {lib_dest}/rdeps{self.comp_clause(comp_lib)})"
                    )

            if nl_mode == "SPLIT_NL" and t["kind"] == "SHARED":
                # Separate NAMELINK_SKIP and NAMELINK_ONLY
                lines.append(
                    f"install(TARGETS {t['name']}{exp_clause}"
                    f" LIBRARY DESTINATION {lib_dest}{self.comp_clause(comp_lib)} NAMELINK_SKIP{self.perm_clause()}"
                    f" RUNTIME DESTINATION {bin_dest}{self.comp_clause(comp_rt)}"
                    f" ARCHIVE DESTINATION {lib_dest}{self.comp_clause(comp_dev)})"
                )
                lines.append(
                    f"install(TARGETS {t['name']}"
                    f" LIBRARY DESTINATION {lib_dest}{self.comp_clause(nl_comp or comp_dev)} NAMELINK_ONLY)"
                )
                continue

            parts = [f"install(TARGETS {t['name']}{exp_clause}{rd_clause}"]
            parts.append(f"  RUNTIME DESTINATION {bin_dest}{self.comp_clause(comp_rt)}{self.perm_clause()}")
            nl_kw = ""
            if t["kind"] == "SHARED":
                if nl_mode.startswith("NAMELINK_COMPONENT"):
                    nl_kw = " " + nl_mode
                elif rng.random() < 0.15 and not t["export_set"]:
                    nl_kw = rng.choice([" NAMELINK_SKIP", " NAMELINK_ONLY"])
            parts.append(f"  LIBRARY DESTINATION {lib_dest}{self.comp_clause(comp_lib)}{nl_kw}{self.perm_clause()}")
            parts.append(f"  ARCHIVE DESTINATION {lib_dest}{self.comp_clause(comp_dev)}{self.perm_clause()}")
            if t["install_objects"]:
                parts.append(f"  OBJECTS DESTINATION obj{self.comp_clause(comp_dev)}")
            if t["has_file_set"]:
                parts.append(f"  FILE_SET {t['file_set_name']} DESTINATION {inc_dest}{self.comp_clause(comp_dev)}")
            if t["has_pub_hdr"]:
                parts.append(f"  PUBLIC_HEADER DESTINATION {inc_dest}{self.comp_clause(comp_dev)}")
            if t["has_priv_hdr"]:
                parts.append(f"  PRIVATE_HEADER DESTINATION {inc_dest}/private{self.comp_clause(comp_dev)}")
            if t["has_res"]:
                parts.append(f"  RESOURCE DESTINATION share/{self.pkg_name}{self.comp_clause(comp_rt)}")
            if not self.cps_mode and rng.random() < 0.4:
                parts.append(f"  INCLUDES DESTINATION include/{self.pkg_name}")
            if opt_clause or excl_clause or cfg_clause:
                parts.append(f" {opt_clause}{excl_clause}{cfg_clause}")
            parts.append(")")
            lines.append("\n".join(parts))

        # install(FILES ...) and install(PROGRAMS ...)
        for idx in range(rng.randint(1, 3)):
            d = rng.choice(["", *subdirs])
            lines = self.dirs[d]
            is_prog = rng.random() < 0.4
            kw = "PROGRAMS" if is_prog else "FILES"
            fname = f"doc_{idx}.txt" if not is_prog else f"tool_{idx}.sh"
            if rng.random() < 0.35:
                fname = f"sp {fname}"
            frel = f"{d}/{fname}" if d else fname
            self.add_text(frel, "#!/bin/sh\necho ok\n" if is_prog else f"doc {idx}\n", mode=0o755 if is_prog else 0o644)
            comp = self.pick_comp()
            use_type = rng.random() < 0.3
            if use_type:
                dest_arg = f"TYPE {rng.choice(INSTALL_TYPES)}"
            else:
                dest_path = rng.choice([
                    "share/docs",
                    "share/docs/$<CONFIG>",
                    f"bin/{self.pkg_name}",
                ])
                dest_arg = f"DESTINATION {dest_path}"
            rename_arg = f" RENAME renamed_{idx}" if rng.random() < 0.4 else ""
            opt = " OPTIONAL" if rng.random() < 0.2 else ""
            excl = " EXCLUDE_FROM_ALL" if rng.random() < 0.1 else ""
            lines.append(
                f"install({kw} {q(fname)} {dest_arg}{rename_arg}{self.comp_clause(comp)}"
                f"{self.perm_clause()}{self.config_clause()}{opt}{excl})"
            )

        if rng.random() < 0.55:
            self.add_text("early_file.txt", "early\n", mode=rng.choice([0o600, 0o644, 0o755]))
            self.add_text("early_dir/keep.h", "early hdr\n", mode=0o640)
            self.add_text("early_dir/skip.tmp", "early tmp\n", mode=0o600)
            self.add_link("early_dir/link.h", "keep.h")
            inputs = [q("early_file.txt")]
            if rng.random() < 0.7:
                inputs.append(q("early_dir" + ("/" if rng.random() < 0.4 else "")))
            perm = ""
            perm_choice = rng.randrange(4)
            if perm_choice == 1:
                perm = " NO_SOURCE_PERMISSIONS"
            elif perm_choice == 2:
                perm = " USE_SOURCE_PERMISSIONS"
            elif perm_choice == 3:
                perm = f" FILE_PERMISSIONS {rng.choice(PERMS_CHOICES)}"
            follow = " FOLLOW_SYMLINK_CHAIN" if rng.random() < 0.25 else ""
            matching = ""
            if rng.random() < 0.45:
                if rng.random() < 0.5:
                    matching = ' FILES_MATCHING PATTERN "*.h"'
                else:
                    matching = ' PATTERN "*.tmp" EXCLUDE'
            root.append(
                f"file(INSTALL {' '.join(inputs)} DESTINATION ${{CMAKE_CURRENT_BINARY_DIR}}/early_copy"
                f"{perm}{follow}{matching})"
            )

        # Optional missing file install
        if rng.random() < 0.3:
            root.append("install(FILES does_not_exist.txt DESTINATION share/opt OPTIONAL)")

        # Symlink file install
        if rng.random() < 0.35:
            self.add_text("data/real.txt", "real target\n")
            self.add_link("data/sym.txt", "real.txt")
            root.append("install(FILES data/real.txt data/sym.txt DESTINATION share/links)")

        # install(DIRECTORY ...)
        for didx in range(rng.randint(1, 2)):
            dir_name = f"assets_{didx}"
            self.add_text(f"{dir_name}/keep.txt", "keep\n", mode=0o644)
            self.add_text(f"{dir_name}/skip.tmp", "skip\n", mode=0o644)
            self.add_text(f"{dir_name}/sub/nested.cfg", "cfg\n", mode=0o750)
            self.add_link(f"{dir_name}/sub/link.txt", "../keep.txt")
            trailing = "/" if rng.random() < 0.5 else ""
            use_source_perms = " USE_SOURCE_PERMISSIONS" if rng.random() < 0.4 else ""
            msg_never = " MESSAGE_NEVER" if rng.random() < 0.25 else ""
            fperms = f" FILE_PERMISSIONS {rng.choice(PERMS_CHOICES)}" if not use_source_perms and rng.random() < 0.4 else ""
            dperms = f" DIRECTORY_PERMISSIONS {rng.choice(PERMS_CHOICES)}" if not use_source_perms and rng.random() < 0.3 else ""
            matching = ""
            if rng.random() < 0.6:
                if rng.random() < 0.5:
                    matching = ' FILES_MATCHING PATTERN "*.txt" PATTERN "*.cfg" PERMISSIONS OWNER_READ OWNER_WRITE'
                else:
                    matching = ' PATTERN "*.tmp" EXCLUDE REGEX "nested\\\\.cfg$" PERMISSIONS OWNER_READ'
            comp = self.pick_comp()
            root.append(
                f"install(DIRECTORY {dir_name}{trailing} DESTINATION share/{dir_name}{self.comp_clause(comp)}"
                f"{use_source_perms}{fperms}{dperms}{msg_never}{self.config_clause()}{matching})"
            )
        if rng.random() < 0.25:
            root.append('install(DIRECTORY DESTINATION share/empty_dir)')

        # install(SCRIPT ...) and install(CODE ...)
        if rng.random() < 0.55:
            self.add_text(
                "inst_hook.cmake",
                'message(STATUS "hook_cfg=${CMAKE_INSTALL_CONFIG_NAME} comp=${CMAKE_INSTALL_COMPONENT}")\n',
            )
            hook_comp = self.pick_comp() if rng.random() < 0.5 else ""
            all_comps = " ALL_COMPONENTS" if (not hook_comp and rng.random() < 0.4) else ""
            root.append(f"install(SCRIPT inst_hook.cmake{self.comp_clause(hook_comp)}{all_comps})")
        if rng.random() < 0.55:
            genex_target = next((t["name"] for t in self.targets if t["kind"] != "INTERFACE"), None)
            msg_part = f"$<CONFIG>:$<TARGET_FILE_NAME:{genex_target}>" if genex_target else "$<CONFIG>"
            root.append(
                f'install(CODE "message(STATUS \\"code_genex={msg_part} prefix=$<INSTALL_PREFIX>\\")")'
            )

        # Export sets & Package Configs
        if self.dep_exported_targets:
            dest = f"lib/cmake/{self.dep_pkg_name}"
            root.append(
                f"install(EXPORT {self.dep_export_set_name} NAMESPACE {self.namespace}"
                f" FILE {self.dep_pkg_name}Targets.cmake DESTINATION {dest})"
            )
            self.add_text(
                f"{self.dep_pkg_name}Config.cmake.in",
                "@PACKAGE_INIT@\n"
                f'include("${{CMAKE_CURRENT_LIST_DIR}}/{self.dep_pkg_name}Targets.cmake")\n'
                f"check_required_components({self.dep_pkg_name})\n",
            )
            root.append(
                f"include(CMakePackageConfigHelpers)\n"
                f"configure_package_config_file({self.dep_pkg_name}Config.cmake.in"
                f" ${{CMAKE_CURRENT_BINARY_DIR}}/{self.dep_pkg_name}Config.cmake"
                f" INSTALL_DESTINATION {dest})\n"
                f"install(FILES ${{CMAKE_CURRENT_BINARY_DIR}}/{self.dep_pkg_name}Config.cmake DESTINATION {dest})"
            )
            root.append(
                f"export(EXPORT {self.dep_export_set_name} NAMESPACE {self.namespace}"
                f" FILE ${{CMAKE_CURRENT_BINARY_DIR}}/{self.dep_pkg_name}Targets.cmake)"
            )

        if self.exported_targets:
            if self.cps_mode:
                cps_dest = f"lib/cps/{self.pkg_name}" if not rng.random() < 0.3 else ""
                dest_clause = f" DESTINATION {cps_dest}" if cps_dest else ""
                lower_flag = " LOWER_CASE_FILE" if rng.random() < 0.3 else ""
                default_tgts = f" DEFAULT_TARGETS {self.exported_targets[0]['export_name']}" if rng.random() < 0.4 else ""
                ver_schema = ' VERSION_SCHEMA "simple"' if rng.random() < 0.3 else ""
                compat_ver = ' COMPAT_VERSION "1.0.0"' if rng.random() < 0.4 else ""
                root.append(
                    f"install(PACKAGE_INFO {self.pkg_name} EXPORT {self.export_set_name}"
                    f" VERSION {self.pkg_version}{compat_ver}{ver_schema}{default_tgts}{lower_flag}{dest_clause})"
                )
                self.has_install_tree_package = True
                if rng.random() < 0.7:
                    root.append(
                        f"export(EXPORT {self.export_set_name} PACKAGE_INFO {self.pkg_name}"
                        f" VERSION {self.pkg_version}{compat_ver}{default_tgts}{lower_flag})"
                    )
                    self.has_build_tree_package = True
            else:
                dest = f"lib/cmake/{self.pkg_name}"
                old_iface = " EXPORT_LINK_INTERFACE_LIBRARIES" if self.use_old_cmp0022 else ""
                cxx_mods = " CXX_MODULES_DIRECTORY cxxmods" if rng.random() < 0.25 else ""
                tgt_file = f"{self.pkg_name}Targets.cmake" if rng.random() < 0.7 else ""
                file_clause = f" FILE {tgt_file}" if tgt_file else ""
                ns_clause = f" NAMESPACE {self.namespace}" if self.namespace else ""
                root.append(
                    f"install(EXPORT {self.export_set_name}{ns_clause}{file_clause}"
                    f" DESTINATION {dest}{old_iface}{cxx_mods})"
                )
                self.has_install_tree_package = True

                # Build-tree export via export(EXPORT ...) or export(TARGETS ...)
                if rng.random() < 0.8:
                    targets_filename = tgt_file or f"{self.export_set_name}.cmake"
                    if rng.random() < 0.6 and not self.use_old_cmp0022:
                        root.append(
                            f"export(EXPORT {self.export_set_name}{ns_clause}"
                            f" FILE ${{CMAKE_CURRENT_BINARY_DIR}}/{targets_filename}{cxx_mods})"
                        )
                    else:
                        # export(TARGETS ...) with optional APPEND
                        build_tgts = [t["name"] for t in self.exported_targets]
                        if len(build_tgts) >= 2 and rng.random() < 0.4:
                            mid = len(build_tgts) // 2
                            self.post_subdir.append(
                                f"export(TARGETS {' '.join(build_tgts[:mid])}{ns_clause}"
                                f" FILE ${{CMAKE_CURRENT_BINARY_DIR}}/{targets_filename}{old_iface})"
                            )
                            self.post_subdir.append(
                                f"export(TARGETS {' '.join(build_tgts[mid:])}{ns_clause}"
                                f" APPEND FILE ${{CMAKE_CURRENT_BINARY_DIR}}/{targets_filename}{old_iface})"
                            )
                        else:
                            self.post_subdir.append(
                                f"export(TARGETS {' '.join(build_tgts)}{ns_clause}"
                                f" FILE ${{CMAKE_CURRENT_BINARY_DIR}}/{targets_filename}{old_iface}{cxx_mods})"
                            )
                    self.has_build_tree_package = True

                # Config and version helpers
                if self.use_pkg_helpers:
                    targets_filename = tgt_file or f"{self.export_set_name}.cmake"
                    extra_dep_find = ""
                    if self.dep_exported_targets:
                        extra_dep_find = (
                            "include(CMakeFindDependencyMacro)\n"
                            f"find_dependency({self.dep_pkg_name})\n"
                        )
                    no_sac = " NO_SET_AND_CHECK_MACRO" if rng.random() < 0.25 else ""
                    no_crc = " NO_CHECK_REQUIRED_COMPONENTS_MACRO" if rng.random() < 0.25 else ""
                    set_check_line = (
                        'set_and_check(PKG_INC_DIR "@PACKAGE_PKG_INCLUDE_DIR@")\n'
                        if not no_sac
                        else 'set(PKG_INC_DIR "@PACKAGE_PKG_INCLUDE_DIR@")\n'
                    )
                    crc_line = f"check_required_components({self.pkg_name})\n" if not no_crc else ""
                    self.add_text(
                        f"{self.pkg_name}Config.cmake.in",
                        "@PACKAGE_INIT@\n"
                        + extra_dep_find
                        + set_check_line
                        + f'include("${{CMAKE_CURRENT_LIST_DIR}}/{targets_filename}")\n'
                        + crc_line,
                    )
                    inc_var_val = f"include/{self.pkg_name}"
                    if not any(t["has_file_set"] or t["has_pub_hdr"] for t in self.targets):
                        # Ensure PKG_INC_DIR exists on install for set_and_check
                        self.add_text("pkg_anchor.h", "/* pkg */\n")
                        root.append(f"install(FILES pkg_anchor.h DESTINATION {inc_var_val})")
                    root.append(
                        "include(CMakePackageConfigHelpers)\n"
                        f'set(PKG_INCLUDE_DIR "{inc_var_val}")\n'
                        f"configure_package_config_file({self.pkg_name}Config.cmake.in\n"
                        f"  ${{CMAKE_CURRENT_BINARY_DIR}}/install_cfg/{self.pkg_name}Config.cmake\n"
                        f"  INSTALL_DESTINATION {dest}\n"
                        f"  PATH_VARS PKG_INCLUDE_DIR{no_sac}{no_crc})"
                    )
                    arch_indep = " ARCH_INDEPENDENT" if self.arch_independent else ""
                    root.append(
                        f"write_basic_package_version_file(\n"
                        f"  ${{CMAKE_CURRENT_BINARY_DIR}}/{self.pkg_name}ConfigVersion.cmake\n"
                        f"  VERSION {self.pkg_version}\n"
                        f"  COMPATIBILITY {self.compat_mode}{arch_indep})"
                    )
                    root.append(
                        f"install(FILES\n"
                        f"  ${{CMAKE_CURRENT_BINARY_DIR}}/install_cfg/{self.pkg_name}Config.cmake\n"
                        f"  ${{CMAKE_CURRENT_BINARY_DIR}}/{self.pkg_name}ConfigVersion.cmake\n"
                        f"  DESTINATION {dest})"
                    )
                    if self.has_build_tree_package:
                        # Build-tree Config.cmake using INSTALL_PREFIX=${CMAKE_CURRENT_BINARY_DIR}
                        self.add_text(
                            f"{self.pkg_name}BuildConfig.cmake.in",
                            "@PACKAGE_INIT@\n"
                            + extra_dep_find
                            + f'include("${{CMAKE_CURRENT_LIST_DIR}}/{targets_filename}")\n'
                            + crc_line,
                        )
                        root.append(
                            f"configure_package_config_file({self.pkg_name}BuildConfig.cmake.in\n"
                            f"  ${{CMAKE_CURRENT_BINARY_DIR}}/{self.pkg_name}Config.cmake\n"
                            f"  INSTALL_DESTINATION .\n"
                            f"  INSTALL_PREFIX ${{CMAKE_CURRENT_BINARY_DIR}}{no_sac}{no_crc})"
                        )
                        if rng.random() < 0.45:
                            root.append("set(CMAKE_EXPORT_PACKAGE_REGISTRY ON)")
                            root.append(f"export(PACKAGE {self.pkg_name})")
                            self.uses_user_registry = True
                else:
                    # Install Config.cmake directly so consumer find_package() works even without helpers
                    targets_filename = tgt_file or f"{self.export_set_name}.cmake"
                    dep_line = f"find_package({self.dep_pkg_name} CONFIG REQUIRED)\n" if self.dep_exported_targets else ""
                    self.add_text(
                        f"{self.pkg_name}Config.cmake",
                        dep_line + f'include("${{CMAKE_CURRENT_LIST_DIR}}/{targets_filename}")\n',
                    )
                    root.append(f"install(FILES {self.pkg_name}Config.cmake DESTINATION {dest})")
                    if self.has_build_tree_package:
                        root.append(
                            f'file(WRITE ${{CMAKE_CURRENT_BINARY_DIR}}/{self.pkg_name}Config.cmake'
                            f' "{dep_line.replace(chr(10), "")}include(\\"${{CMAKE_CURRENT_BINARY_DIR}}/{targets_filename}\\")\\n")'
                        )

    def generate(self):
        rng = self.rng
        subdirs = self.build_targets()
        self.emit_installs_and_exports(subdirs)

        min_ver = rng.choice(["3.20", "3.23", "3.28", "3.30", "4.1"])
        preamble = [f"cmake_minimum_required(VERSION {min_ver})"]
        preamble.append(f"cmake_policy(SET CMP0082 {self.cmp0082})")
        preamble.append(f"cmake_policy(SET CMP0087 {self.cmp0087})")
        preamble.append(f"cmake_policy(SET CMP0095 {self.cmp0095})")
        preamble.append(f"cmake_policy(SET CMP0177 {self.cmp0177})")
        if self.cps_mode:
            preamble.append(f'set(CMAKE_EXPERIMENTAL_EXPORT_PACKAGE_INFO "{CPS_EXPORT_UUID}")')
        langs = "C CXX" if self.cxx else "C"
        preamble.append(
            f'project({self.pkg_name} VERSION {self.pkg_version} DESCRIPTION "Fuzz pkg" HOMEPAGE_URL "https://example.org" LANGUAGES {langs})'
        )
        if self.use_gnu_install_dirs:
            preamble.append("include(GNUInstallDirs)")
        if self.default_component:
            preamble.append(f"set(CMAKE_INSTALL_DEFAULT_COMPONENT_NAME {q(self.default_component)})")
        msg_mode = rng.choice(["", "ALWAYS", "LAZY", "NEVER"])
        if msg_mode:
            preamble.append(f"set(CMAKE_INSTALL_MESSAGE {msg_mode})")
        if self.install_parallel:
            preamble.append("set_property(GLOBAL PROPERTY INSTALL_PARALLEL ON)")
        if rng.random() < 0.2:
            preamble.append("set(CMAKE_WARN_ON_ABSOLUTE_INSTALL_DESTINATION ON)")
        if rng.random() < 0.2:
            preamble.append("set(CMAKE_INSTALL_DEFAULT_DIRECTORY_PERMISSIONS OWNER_READ OWNER_WRITE OWNER_EXECUTE GROUP_READ GROUP_EXECUTE)")

        # Interleave add_subdirectory before and after root commands to exercise CMP0082
        root_cmds = self.dirs[""]
        split_at = rng.randint(0, len(root_cmds))
        sub_calls = [f"add_subdirectory({sd})" for sd in subdirs]
        body_lines = preamble + root_cmds[:split_at] + sub_calls + self.post_subdir + root_cmds[split_at:]
        self.add_text("CMakeLists.txt", "\n".join(body_lines) + "\n")

        for sd in subdirs:
            self.add_text(f"{sd}/CMakeLists.txt", "\n".join(self.dirs[sd]) + "\n")

        # Configure args
        config_args = []
        if not self.multi_config and self.configs[0]:
            config_args.append(f"-DCMAKE_BUILD_TYPE={self.configs[0]}")
        elif self.multi_config:
            config_args.append("-DCMAKE_CONFIGURATION_TYPES=Debug;Release")
            if rng.random() < 0.3:
                config_args.append("-DCMAKE_DEFAULT_BUILD_TYPE=Release")
            if rng.random() < 0.25:
                config_args.append("-DCMAKE_CROSS_CONFIGS=all")
                config_args.append("-DCMAKE_DEFAULT_CONFIGS=Debug;Release")
        if self.cps_mode:
            config_args.append("-Wno-dev")

        # Build configs
        if self.multi_config:
            if rng.random() < 0.4:
                build_configs = ["Debug", "Release"]
            else:
                build_configs = [rng.choice(["Debug", "Release"])]
        else:
            build_configs = [""]

        # Randomize cmake --install run(s)
        install_runs = []
        for run_idx in range(2 if rng.random() < 0.35 else 1):
            use_prefix_override = rng.random() < 0.5
            use_destdir = rng.random() < 0.4
            comp = ""
            if rng.random() < 0.35:
                pool = list(self.used_components) + ["Unspecified", "NoSuchComp"]
                if self.default_component:
                    pool.append(self.default_component)
                comp = rng.choice(pool)
            cfg = ""
            if self.multi_config:
                if len(build_configs) == 1 and rng.random() < 0.8:
                    cfg = build_configs[0]
                elif len(build_configs) > 1 and rng.random() < 0.7:
                    cfg = rng.choice(build_configs)
            strip = rng.random() < 0.35
            verbose = rng.random() < 0.3
            ddp = rng.choice(DIR_PERMS_CLI) if rng.random() < 0.25 else ""
            jobs = rng.choice([0, 1, 2]) if self.install_parallel else 0
            always = rng.random() < 0.25
            inst_mode = rng.choice(["", "", "", "COPY", "REL_SYMLINK", "ABS_SYMLINK_OR_COPY"])
            install_runs.append({
                "incremental": run_idx > 0,
                "prefix_override": use_prefix_override,
                "destdir": use_destdir,
                "component": comp,
                "config": cfg,
                "strip": strip,
                "verbose": verbose,
                "default_dir_perms": ddp,
                "jobs": jobs,
                "always": always,
                "install_mode": inst_mode,
            })

        return {
            "files": self.files,
            "config_args": config_args,
            "build_configs": build_configs,
            "install_runs": install_runs,
            "install_parallel": self.install_parallel,
            "consumer": self.make_consumer(build_configs),
        }

    def make_consumer(self, build_configs: list[str]):
        if not self.has_install_tree_package and not self.has_build_tree_package:
            return None
        # Collect linkable exported targets
        linkable = [
            t for t in (self.dep_exported_targets + self.exported_targets)
            if t["kind"] in ("STATIC", "SHARED", "INTERFACE")
            or (t["kind"] == "OBJECT" and not t["install_objects"])
            or (t["kind"] == "EXECUTABLE" and t["enable_exports"])
        ]
        # Exclude any target whose dependencies weren't built in all configs, or keep only non-conditional ones
        # (Our generator cleared EXCLUDE_FROM_ALL and CONFIGURATIONS for exported targets.)
        if not linkable:
            return None

        # Choose which targets to link (can link all linkable targets from main pkg)
        main_linkable = [t for t in self.exported_targets if t in linkable]
        if not main_linkable:
            return None

        ext = "cpp" if self.cxx else "c"
        decls = []
        calls = []
        target_refs = []
        for t in main_linkable:
            qname = f"{self.namespace}{t['export_name']}"
            target_refs.append(qname)
            if t["kind"] != "EXECUTABLE":
                decls.append(f"extern int {t['sym']}(void);")
                calls.append(f"sum += {t['sym']}();")

        src_body = "\n".join(decls) + "\nint main(void) {\n  int sum = 0;\n  " + "\n  ".join(calls) + "\n  return sum == 0 ? 0 : 0;\n}\n"

        ver_req = ""
        if self.cps_mode or self.use_pkg_helpers:
            if self.rng.random() < 0.45:
                ver_req = f" {self.pkg_version}"
                if self.rng.random() < 0.35:
                    ver_req += " EXACT"

        cps_setup = f'set(CMAKE_EXPERIMENTAL_FIND_CPS_PACKAGES "{CPS_FIND_UUID}")\n' if self.cps_mode else ""
        cm_lines = [
            "cmake_minimum_required(VERSION 3.20)",
            f"project(consumer LANGUAGES {'C CXX' if self.cxx else 'C'})",
            cps_setup.rstrip("\n") if cps_setup else "",
            f"find_package({self.pkg_name}{ver_req} CONFIG REQUIRED)",
            f'message(STATUS "FOUND=${{{self.pkg_name}_FOUND}} VER=${{{self.pkg_name}_VERSION}}")',
            f"add_executable(consumer_app main.{ext})",
            f"target_link_libraries(consumer_app PRIVATE {' '.join(target_refs)})",
        ]
        if self.multi_config:
            cfg = build_configs[0]
            build_cfg_arg = ["-DCMAKE_BUILD_TYPE=" + cfg] if False else []
            consumer_build_cfg = cfg
        else:
            consumer_build_cfg = ""

        return {
            "files": {
                "CMakeLists.txt": "\n".join(line for line in cm_lines if line) + "\n",
                f"main.{ext}": src_body,
            },
            "test_install_tree": self.has_install_tree_package,
            "test_build_tree": self.has_build_tree_package and (not self.multi_config or len(build_configs) == 1),
            "uses_user_registry": self.uses_user_registry,
            "pkg_name": self.pkg_name,
            "dep_pkg_name": self.dep_pkg_name if self.dep_exported_targets else None,
            "config": consumer_build_cfg,
            "cps_mode": self.cps_mode,
        }


def make_case(seed: int, index: int, generator: str):
    rng = random.Random(f"{seed}:{index}")
    gen_name = generator
    if generator == "mixed":
        gen_name = "Ninja Multi-Config" if rng.random() < 0.45 else "Ninja"
    case = ProjectGen(rng, gen_name, index).generate()
    case["generator"] = gen_name
    return case


def materialize_tree(base: Path, files: dict[str, object]):
    for rel, content in files.items():
        path = base / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        if isinstance(content, tuple) and content[0] == "link":
            if path.is_symlink() or path.exists():
                path.unlink()
            os.symlink(content[1], path)
        elif isinstance(content, tuple):
            text, mode = content
            path.write_text(text)
            if mode is not None:
                path.chmod(mode)
        else:
            path.write_text(content)


def base_env(home: Path) -> dict[str, str]:
    drop = {
        "MAKEFLAGS", "CMAKE_RUST_DATA_ROOT", "DESTDIR", "CMAKE_INSTALL_ALWAYS",
        "CMAKE_INSTALL_MODE", "CMAKE_INSTALL_PARALLEL_LEVEL", "CMAKE_PREFIX_PATH",
        "VERBOSE", "CMAKE_BUILD_PARALLEL_LEVEL",
    }
    env = {k: v for k, v in os.environ.items() if k not in drop}
    env["HOME"] = str(home)
    env["LANG"] = "C"
    env["LC_ALL"] = "C"
    env["TZ"] = "UTC"
    return env


def wipe(path: Path):
    if path.exists() or path.is_symlink():
        subprocess.run(["chmod", "-R", "u+rwx", str(path)], capture_output=True)
        shutil.rmtree(path, ignore_errors=True)


def run_configure(cmake: Path, src: Path, build: Path, extra: list[str], log: Path, generator: str, env: dict[str, str], queries=treediff.FILEAPI_QUERIES) -> int:
    wipe(build)
    for kind in queries:
        qdir = build / ".cmake/api/v1/query"
        qdir.mkdir(parents=True, exist_ok=True)
        (qdir / kind).touch()
    with log.open("wb") as stream:
        proc = subprocess.run(
            [str(cmake), "-S", str(src), "-B", str(build), "-G", generator, *extra],
            stdout=stream,
            stderr=subprocess.STDOUT,
            env=env,
            timeout=120,
        )
    return proc.returncode


def normalize_install_output(data: bytes, norm, extra_replacements: list[tuple[bytes, bytes]], parallel: bool, jobs: int) -> bytes:
    out = norm(data)
    for pat, rep in extra_replacements:
        out = out.replace(pat, rep)
    out = re.sub(rb"\.tmp[0-9a-f]{5}\b", b".tmp<R>", out)
    if parallel and jobs > 1:
        # Parallel install prints `[k/total] script\n...` in completion order.
        blocks = re.split(rb"(?m)^(?=\[\d+/\d+\] )", out)
        norm_blocks = [re.sub(rb"^\[\d+/(\d+)\] ", rb"[K/\1] ", b, count=1) for b in blocks if b]
        out = b"".join(sorted(norm_blocks))
    return out


def snapshot_install_tree(tree_root: Path, norm, extra_replacements: list[tuple[bytes, bytes]]) -> str:
    if not tree_root.exists():
        return ""
    entries = {}

    def scan(path: Path, rel: str):
        st = path.lstat()
        mode = stat.S_IMODE(st.st_mode)
        if stat.S_ISLNK(st.st_mode):
            entries[rel] = ("link", mode, os.readlink(path).encode(errors="surrogateescape"))
            return
        if stat.S_ISDIR(st.st_mode):
            entries[rel] = ("dir", mode, None)
            changed = (mode & 0o500) != 0o500
            if changed:
                try:
                    path.chmod(mode | 0o700)
                except OSError:
                    return
            try:
                children = sorted(path.iterdir(), key=lambda p: p.name)
                for child in children:
                    child_rel = f"{rel}/{child.name}" if rel else child.name
                    scan(child, child_rel)
            except OSError:
                pass
            if changed:
                try:
                    path.chmod(mode)
                except OSError:
                    pass
            return
        data = None
        try:
            data = path.read_bytes()
        except PermissionError:
            try:
                path.chmod(mode | 0o400)
                data = path.read_bytes()
                path.chmod(mode)
            except OSError:
                pass
        entries[rel] = ("file", mode, data)

    for child in sorted(tree_root.iterdir(), key=lambda p: p.name):
        scan(child, child.name)

    lines = []
    for rel in sorted(entries):
        kind, mode, data = entries[rel]
        if kind == "link":
            target = norm(data)
            for pat, rep in extra_replacements:
                target = target.replace(pat, rep)
            lines.append(f"{rel} -> {target!r}")
        elif kind == "dir":
            lines.append(f"{rel}/ {oct(mode)}")
        else:
            if data is None:
                lines.append(f"{rel} {oct(mode)} <unreadable>")
                continue
            is_binary = (
                data.startswith((b"\x7fELF", b"!<arch>\n"))
                or b"\0" in data[:4096]
            )
            if is_binary:
                lines.append(f"{rel} {oct(mode)} <binary>")
            else:
                text = norm(data)
                for pat, rep in extra_replacements:
                    text = text.replace(pat, rep)
                lines.append(f"{rel} {oct(mode)} {text!r}")
    return "\n".join(lines)


def snapshot_manifests(build: Path, norm, extra_replacements: list[tuple[bytes, bytes]]) -> dict[str, bytes]:
    out = {}
    for p in sorted(build.glob("install_manifest*.txt")):
        data = norm(p.read_bytes())
        for pat, rep in extra_replacements:
            data = data.replace(pat, rep)
        out[p.name] = data
    return out


def run_side(cmake: Path, src: Path, root: Path, case: dict) -> dict:
    build = root / "b"
    inst_default = root / "inst_default"
    inst_override = root / "inst_override"
    destdir = root / "destdir"
    home = root / "home"
    for p in (build, inst_default, inst_override, destdir, home):
        wipe(p)
    home.mkdir(parents=True)

    env = base_env(home)
    generator = case["generator"]
    config_args = list(case["config_args"])
    if not any(a.startswith(("-DCMAKE_INSTALL_PREFIX", "--install-prefix")) for a in config_args):
        config_args.append(f"-DCMAKE_INSTALL_PREFIX={inst_default}")

    log = root / "configure.log"
    rc = run_configure(cmake, src, build, config_args, log, generator, env)
    norm = treediff.normalizer(build, cmake)
    extra_reps = [
        (str(src).encode(), b"<SRC>"),
        (str(inst_default).encode(), b"<INST>"),
        (str(inst_override).encode(), b"<INST_OVR>"),
        (str(destdir).encode(), b"<DESTDIR>"),
        (str(home).encode(), b"<HOME>"),
    ]
    conf_out = norm(log.read_bytes())
    for pat, rep in extra_reps:
        conf_out = conf_out.replace(pat, rep)
    conf_out = re.sub(rb"done \(\d+\.\d+s\)", b"done (<T>s)", conf_out)
    conf_out = re.sub(rb"cmTC_[0-9a-f]{5}", b"cmTC_<ID>", conf_out)

    tree = treediff.gather(build, norm)

    build_rcs = []
    if rc == 0:
        for cfg in case["build_configs"]:
            cmd = ["ninja", "-C", str(build), "-k", "0"]
            if generator == "Ninja Multi-Config" and cfg:
                cmd += ["-f", f"build-{cfg}.ninja"]
            proc = subprocess.run(cmd, capture_output=True, env=env, timeout=180)
            build_rcs.append((cfg, proc.returncode))

    install_results = []
    all_built = rc == 0 and all(brc == 0 for _, brc in build_rcs)
    if all_built:
        for run_spec in case["install_runs"]:
            if not run_spec["incremental"]:
                wipe(inst_default)
                wipe(inst_override)
                wipe(destdir)
                for m in build.glob("install_manifest*.txt"):
                    m.unlink()
            cmd = [str(cmake), "--install", str(build)]
            if run_spec["prefix_override"]:
                cmd += ["--prefix", str(inst_override)]
            if run_spec["component"]:
                cmd += ["--component", run_spec["component"]]
            if run_spec["config"]:
                cmd += ["--config", run_spec["config"]]
            if run_spec["strip"]:
                cmd.append("--strip")
            if run_spec["verbose"]:
                cmd.append("-v")
            if run_spec["default_dir_perms"]:
                cmd += ["--default-directory-permissions", run_spec["default_dir_perms"]]
            if run_spec["jobs"]:
                cmd += ["-j", str(run_spec["jobs"])]

            inst_env = dict(env)
            if run_spec["destdir"]:
                inst_env["DESTDIR"] = str(destdir)
            if run_spec["always"]:
                inst_env["CMAKE_INSTALL_ALWAYS"] = "1"
            if run_spec["install_mode"]:
                inst_env["CMAKE_INSTALL_MODE"] = run_spec["install_mode"]

            proc = subprocess.run(cmd, capture_output=True, env=inst_env, timeout=120)
            out_i = normalize_install_output(
                proc.stdout + proc.stderr, norm, extra_reps, case["install_parallel"], run_spec["jobs"] or (1 if case["install_parallel"] else 0)
            )
            if run_spec["destdir"]:
                snap_root = destdir
            elif run_spec["prefix_override"]:
                snap_root = inst_override
            else:
                snap_root = inst_default
            snap = snapshot_install_tree(snap_root, norm, extra_reps)
            manifests = snapshot_manifests(build, norm, extra_reps)
            install_results.append((proc.returncode, out_i, snap, manifests))

    consumer_results = {}
    consumer = case.get("consumer")
    if all_built and consumer is not None:
        csrc = root / "consumer_src"
        wipe(csrc)
        csrc.mkdir(parents=True)
        materialize_tree(csrc, consumer["files"])

        if consumer["test_install_tree"]:
            inst_pkg = root / "inst_pkg"
            wipe(inst_pkg)
            cfg_flag = ["--config", consumer["config"]] if consumer["config"] else []
            proc = subprocess.run(
                [str(cmake), "--install", str(build), "--prefix", str(inst_pkg), *cfg_flag],
                capture_output=True, env=env, timeout=120,
            )
            cb = root / "consumer_inst_b"
            clog = root / "consumer_inst.log"
            cargs = [f"-DCMAKE_PREFIX_PATH={inst_pkg}"]
            if consumer["cps_mode"]:
                cargs.append("-Wno-dev")
            if consumer["config"]:
                cargs.append(f"-DCMAKE_BUILD_TYPE={consumer['config']}")
            crc = run_configure(cmake, csrc, cb, cargs, clog, generator, env, queries=())
            cnorm = treediff.normalizer(cb, cmake)
            cout = cnorm(clog.read_bytes()).replace(str(csrc).encode(), b"<CSRC>").replace(str(inst_pkg).encode(), b"<INST_PKG>")
            cout = re.sub(rb"done \(\d+\.\d+s\)", b"done (<T>s)", cout)
            ctree = treediff.gather(cb, cnorm)
            cb_rc = None
            if crc == 0:
                bcmd = ["ninja", "-C", str(cb), "-k", "0"]
                if generator == "Ninja Multi-Config" and consumer["config"]:
                    bcmd += ["-f", f"build-{consumer['config']}.ninja"]
                bproc = subprocess.run(bcmd, capture_output=True, env=env, timeout=120)
                cb_rc = bproc.returncode
            consumer_results["install"] = (crc, cout, cb_rc, ctree)

        if consumer["test_build_tree"]:
            cb = root / "consumer_build_b"
            clog = root / "consumer_build.log"
            cargs = []
            if not consumer["uses_user_registry"]:
                cargs.append(f"-DCMAKE_PREFIX_PATH={build}")
            if consumer["dep_pkg_name"] and not consumer["uses_user_registry"]:
                pass  # already inside `build`
            if consumer["cps_mode"]:
                cargs.append("-Wno-dev")
            if consumer["config"]:
                cargs.append(f"-DCMAKE_BUILD_TYPE={consumer['config']}")
            crc = run_configure(cmake, csrc, cb, cargs, clog, generator, env, queries=())
            cnorm = treediff.normalizer(cb, cmake)
            cout = cnorm(clog.read_bytes()).replace(str(csrc).encode(), b"<CSRC>").replace(str(build).encode(), b"<PBUILD>")
            cout = re.sub(rb"done \(\d+\.\d+s\)", b"done (<T>s)", cout)
            ctree = treediff.gather(cb, cnorm)
            cb_rc = None
            if crc == 0:
                bcmd = ["ninja", "-C", str(cb), "-k", "0"]
                if generator == "Ninja Multi-Config" and consumer["config"]:
                    bcmd += ["-f", f"build-{consumer['config']}.ninja"]
                bproc = subprocess.run(bcmd, capture_output=True, env=env, timeout=120)
                cb_rc = bproc.returncode
            consumer_results["build_tree"] = (crc, cout, cb_rc, ctree)

    return {
        "rc": rc,
        "conf_out": conf_out,
        "tree": tree,
        "build_rcs": build_rcs,
        "install_results": install_results,
        "consumer_results": consumer_results,
    }


def compare_sides(oracle: dict, candidate: dict) -> list[str]:
    diffs = []
    if oracle["rc"] != candidate["rc"]:
        diffs.append(f"configure rc {oracle['rc']} != {candidate['rc']}")
    if oracle["conf_out"] != candidate["conf_out"]:
        diffs.append("configure output")
    for rel in sorted(set(oracle["tree"]) | set(candidate["tree"])):
        if oracle["tree"].get(rel) != candidate["tree"].get(rel):
            diffs.append(f"tree:{rel}")
    if oracle["build_rcs"] != candidate["build_rcs"]:
        diffs.append(f"build rc {oracle['build_rcs']} != {candidate['build_rcs']}")
    for idx, (ores, cres) in enumerate(zip(oracle["install_results"], candidate["install_results"])):
        if ores[0] != cres[0]:
            diffs.append(f"install[{idx}] rc {ores[0]} != {cres[0]}")
        if ores[1] != cres[1]:
            diffs.append(f"install[{idx}] output")
        if ores[2] != cres[2]:
            diffs.append(f"install[{idx}] tree")
        if ores[3] != cres[3]:
            diffs.append(f"install[{idx}] manifests")
    if len(oracle["install_results"]) != len(candidate["install_results"]):
        diffs.append("install_results len differs")
    for kind in sorted(set(oracle["consumer_results"]) | set(candidate["consumer_results"])):
        oc = oracle["consumer_results"].get(kind)
        cc = candidate["consumer_results"].get(kind)
        if oc is None or cc is None:
            diffs.append(f"consumer:{kind} missing")
            continue
        if oc[0] != cc[0]:
            diffs.append(f"consumer:{kind} rc {oc[0]} != {cc[0]}")
        if oc[1] != cc[1]:
            diffs.append(f"consumer:{kind} configure output")
        if oc[2] != cc[2]:
            diffs.append(f"consumer:{kind} build rc {oc[2]} != {cc[2]}")
        for rel in sorted(set(oc[3]) | set(cc[3])):
            if oc[3].get(rel) != cc[3].get(rel):
                diffs.append(f"consumer:{kind} tree:{rel}")
    return diffs


def format_diff(label: str, a: bytes | str, b: bytes | str) -> str:
    if isinstance(a, bytes):
        a_lines = a.decode(errors="replace").splitlines()
    else:
        a_lines = a.splitlines()
    if isinstance(b, bytes):
        b_lines = b.decode(errors="replace").splitlines()
    else:
        b_lines = b.splitlines()
    diff = list(difflib.unified_diff(a_lines, b_lines, "oracle", "candidate", n=2, lineterm=""))
    return f"=== {label} ===\n" + "\n".join(diff[:80]) + "\n"


def check(args, index: int):
    case = make_case(args.seed, index, args.generator)
    root = Path(tempfile.mkdtemp(prefix="fi-", dir=args.work))
    try:
        src = root / "src"
        src.mkdir()
        materialize_tree(src, case["files"])

        oracle = run_side(Path(args.oracle), src, root, case)
        # Preserve oracle's configure log & install trees only if needed; check after candidate
        candidate = run_side(Path(args.candidate), src, root, case)
        diffs = compare_sides(oracle, candidate)
        if not diffs:
            return None

        keep = Path(args.out) / f"case-{args.seed}-{index}"
        wipe(keep)
        keep.mkdir(parents=True, exist_ok=True)
        shutil.copytree(src, keep / "src", symlinks=True)
        (keep / "case.json").write_text(
            json.dumps(
                {
                    "seed": args.seed,
                    "index": index,
                    "generator": case["generator"],
                    "config_args": case["config_args"],
                    "build_configs": case["build_configs"],
                    "install_runs": case["install_runs"],
                },
                indent=2,
            )
            + "\n"
        )
        (keep / "diffs.txt").write_text("\n".join(diffs) + "\n")

        detail_parts = []
        if oracle["conf_out"] != candidate["conf_out"]:
            detail_parts.append(format_diff("configure output", oracle["conf_out"], candidate["conf_out"]))
        for rel in sorted(set(oracle["tree"]) | set(candidate["tree"])):
            ov, cv = oracle["tree"].get(rel), candidate["tree"].get(rel)
            if ov != cv:
                detail_parts.append(
                    format_diff(
                        f"tree:{rel}",
                        ov[1] if ov else b"<missing>",
                        cv[1] if cv else b"<missing>",
                    )
                )
        for idx, (ores, cres) in enumerate(zip(oracle["install_results"], candidate["install_results"])):
            if ores[1] != cres[1]:
                detail_parts.append(format_diff(f"install[{idx}] output", ores[1], cres[1]))
            if ores[2] != cres[2]:
                detail_parts.append(format_diff(f"install[{idx}] tree", ores[2], cres[2]))
            if ores[3] != cres[3]:
                for mname in sorted(set(ores[3]) | set(cres[3])):
                    if ores[3].get(mname) != cres[3].get(mname):
                        detail_parts.append(
                            format_diff(
                                f"install[{idx}] manifest:{mname}",
                                ores[3].get(mname, b"<missing>"),
                                cres[3].get(mname, b"<missing>"),
                            )
                        )
        for kind in sorted(set(oracle["consumer_results"]) | set(candidate["consumer_results"])):
            oc = oracle["consumer_results"].get(kind)
            cc = candidate["consumer_results"].get(kind)
            if oc and cc:
                if oc[1] != cc[1]:
                    detail_parts.append(format_diff(f"consumer:{kind} configure", oc[1], cc[1]))
                for rel in sorted(set(oc[3]) | set(cc[3])):
                    ov, cv = oc[3].get(rel), cc[3].get(rel)
                    if ov != cv:
                        detail_parts.append(
                            format_diff(
                                f"consumer:{kind} tree:{rel}",
                                ov[1] if ov else b"<missing>",
                                cv[1] if cv else b"<missing>",
                            )
                        )
        (keep / "details.diff").write_text("\n".join(detail_parts[:20]))
        return f"{keep}: {', '.join(diffs[:4])}"
    finally:
        wipe(root)


def main():
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    parser.add_argument("--candidate", required=True)
    parser.add_argument("--oracle", default=str(treediff.ORACLE))
    parser.add_argument("--seed", type=int, default=1)
    parser.add_argument("-n", type=int, default=100)
    parser.add_argument("-j", type=int, default=3)
    parser.add_argument("--generator", default="Ninja")
    parser.add_argument("--out", default="fuzz-install-out")
    args = parser.parse_args()
    Path(args.out).mkdir(parents=True, exist_ok=True)
    args.work = tempfile.mkdtemp(prefix="fuzz-install-")
    with concurrent.futures.ThreadPoolExecutor(args.j) as pool:
        fails = [f for f in pool.map(lambda i: check(args, i), range(args.n)) if f]
    wipe(Path(args.work))
    print(
        json.dumps(
            {
                "seed": args.seed,
                "generator": args.generator,
                "cases": args.n,
                "failures": len(fails),
                "examples": fails[:12],
            },
            indent=1,
        )
    )
    return 1 if fails else 0


if __name__ == "__main__":
    raise SystemExit(main())
