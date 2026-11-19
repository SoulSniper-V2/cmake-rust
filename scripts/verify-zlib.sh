#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd -P)"
pin=51b7f2abdade71cd9bb0e7a373ef2610ec6f9daf
work="${WORK_ROOT:-$(mktemp -d "${TMPDIR:-/tmp}/cmake-rust-zlib.XXXXXXXX")}"
work="$(cd "$work" && pwd -P)"
reference="${UPSTREAM_CMAKE:-$(command -v cmake)}"
rust="${RUST_CMAKE:-$root/target/release/cmake}"
ctest="${CTEST:-$(command -v ctest)}"
ninja="${NINJA:-$(command -v ninja)}"
cc="${CC:-$(command -v cc)}"
ar="${AR:-$(command -v ar)}"
zlib="${ZLIB_PIN_DIR:-$work/upstream}"
for binary in "$reference" "$rust" "$ctest" "$ninja" "$cc" "$ar"; do
    test -x "$binary"
done
"$rust" --version | grep -Fq 'Rust-native implementation'
if [[ ! -d "$zlib/.git" ]]; then
    git clone --quiet --depth 1 --branch v1.3.1 https://github.com/madler/zlib.git "$zlib"
fi
test "$(git -C "$zlib" rev-parse HEAD)" = "$pin"
test -z "$(git -C "$zlib" status --porcelain)"

shim="$work/bin"
mkdir "$shim"
ln -s "$rust" "$shim/cmake"
ln -s "$ninja" "$shim/ninja"
ln -s "$ctest" "$shim/ctest"
for tool in as ld ln rm xcrun; do
    if path="$(command -v "$tool")"; then
        ln -s "$path" "$shim/$tool"
    fi
done
test "$(PATH="$shim" command -v cmake)" = "$shim/cmake"

echo "Evidence directory $work"
"$reference" --version | sed -n '1p'
"$rust" --version | sed -n '1p'
"$cc" --version | sed -n '1p'
"$ninja" --version

for side in reference rust; do
    mkdir "$work/$side-src"
    git -C "$zlib" archive "$pin" | tar -xf - -C "$work/$side-src"
    args=(-S "$work/$side-src" -B "$work/$side-build" -G Ninja
          -DCMAKE_BUILD_TYPE=Release -DCMAKE_EXPORT_COMPILE_COMMANDS=ON
          -DCMAKE_POLICY_VERSION_MINIMUM=3.5
          "-DCMAKE_INSTALL_PREFIX=$work/$side-install"
          "-DCMAKE_C_COMPILER=$cc" "-DCMAKE_AR=$ar" "-DCMAKE_MAKE_PROGRAM=$ninja")
    if [[ "$side" == rust ]]; then
        build_cmake=(env "PATH=$shim" "$shim/cmake")
    else
        build_cmake=("$reference")
    fi
    "${build_cmake[@]}" "${args[@]}" >"$work/$side-configure.log" 2>&1 ||
        { cat "$work/$side-configure.log"; exit 1; }
    "${build_cmake[@]}" --build "$work/$side-build" >"$work/$side-build.log" 2>&1 ||
        { cat "$work/$side-build.log"; exit 1; }
    "$ctest" --test-dir "$work/$side-build" --output-on-failure --no-tests=error |
        tee "$work/$side-test.log"
    "$ctest" --test-dir "$work/$side-build" --show-only=json-v1 > "$work/$side-tests.json"
    "${build_cmake[@]}" --install "$work/$side-build" >"$work/$side-install.log" 2>&1 ||
        { cat "$work/$side-install.log"; exit 1; }
    "${build_cmake[@]}" --build "$work/$side-build" >"$work/$side-rebuild.log" 2>&1
    grep -Fq 'ninja: no work to do.' "$work/$side-rebuild.log"
done

# v1.3.1 itself renames the checked-in zconf.h on an out-of-source configure.
test ! -e "$work/reference-src/zconf.h"
test ! -e "$work/rust-src/zconf.h"
git -C "$zlib" show "$pin:zconf.h" | cmp - "$work/reference-src/zconf.h.included"
git -C "$zlib" show "$pin:zconf.h" | cmp - "$work/rust-src/zconf.h.included"
diff -qr "$work/reference-src" "$work/rust-src"
cmp "$work/reference-build/zconf.h" "$work/rust-build/zconf.h"

python3 - "$work" <<'PY'
import json
import os
from pathlib import Path
import shlex
import subprocess
import sys

work = Path(sys.argv[1])

def normalize(text, side):
    for kind in ("src", "build", "install"):
        prefix = work / f"{side}-{kind}"
        text = text.replace(str(prefix), f"<{kind}>")
    return text

def builds(side):
    build = work / f"{side}-build"
    data = json.loads((build / "compile_commands.json").read_text())
    result = {}
    for item in data:
        args = item.get("arguments") or shlex.split(item["command"])
        output = args[args.index("-o") + 1]
        target = Path(output).parts[Path(output).parts.index("CMakeFiles") + 1]
        source = normalize(item["file"], side)
        assert (target, source) not in result, (target, source)
        # Compare include precedence and other ordered flags. Macro order is
        # immaterial for this pinned fixture because no macro is redefined.
        includes, macros, other = [], [], []
        iter_args = iter(args[1:])
        for arg in iter_args:
            if arg in ("-o", "-c", "-MF", "-MT", "-MQ"):
                next(iter_args)
            elif arg in ("-MD", "-MMD"):
                continue
            elif arg in ("-D", "-I"):
                (macros if arg == "-D" else includes).append(arg + normalize(next(iter_args), side))
            elif arg.startswith("-D"):
                macros.append(normalize(arg, side))
            elif arg.startswith("-I"):
                includes.append(normalize(arg, side))
            else:
                other.append(normalize(arg, side))
        keys = [flag.split("=")[0] for flag in macros]
        assert len(set(keys)) == len(keys), (side, target, macros)
        result[target, source] = includes, sorted(macros), other
    return result

reference, rust = builds("reference"), builds("rust")
assert reference.keys() == rust.keys(), (reference.keys() - rust.keys(), rust.keys() - reference.keys())
for key in reference:
    assert reference[key] == rust[key], (key, reference[key], rust[key])
print(f"PASS {len(reference)} compile commands match after path and macro normalization")

def tests(side):
    data = json.loads((work / f"{side}-tests.json").read_text())
    return {test["name"] for test in data["tests"]}

assert tests("reference") == tests("rust") and tests("rust")
print("PASS CTest names and successful test runs match", ", ".join(sorted(tests("rust"))))

def installed(side):
    install = work / f"{side}-install"
    return {
        str(path.relative_to(install)): os.readlink(path) if path.is_symlink() else None
        for path in install.rglob("*") if path.is_file() or path.is_symlink()
    }

assert installed("reference") == installed("rust")
for relative in ("include/zconf.h", "include/zlib.h", "share/man/man3/zlib.3", "share/pkgconfig/zlib.pc"):
    def text(side):
        return normalize((work / f"{side}-install" / relative).read_text(), side)
    assert text("reference") == text("rust"), relative

linux = sys.platform.startswith("linux")
def symbols(side, library, shared):
    args = ["nm", "-D", "--defined-only"] if linux and shared else (
        ["nm", "-g", "--defined-only"] if linux else ["nm", "-gU"]
    )
    output = subprocess.check_output(args + [work / f"{side}-install" / "lib" / library], text=True)
    names = {parts[-1] for line in output.splitlines()
             if len(parts := line.split()) >= 3 and len(parts[-2]) == 1}
    assert names, library
    return names

for library, shared in (("libz.a", False), ("libz.so" if linux else "libz.dylib", True)):
    assert symbols("reference", library, shared) == symbols("rust", library, shared), library
print(f"PASS {len(installed('rust'))} installed file/symlink entries, normalized text, and exported symbol sets match")

payload = bytes(range(256)) * 4096 + bytes(range(31)) * 13
for encoder in ("reference", "rust"):
    encoded = subprocess.check_output([work / f"{encoder}-build/minigzip"], input=payload)
    for decoder in ("reference", "rust"):
        decoded = subprocess.check_output([work / f"{decoder}-build/minigzip", "-d"], input=encoded)
        assert decoded == payload, (encoder, decoder)
print(f"PASS all four minigzip encode/decode pairings match the {len(payload):,}-byte input")
PY

echo "PASS stock zconf.h rename, byte-identical generated header, and no-op Ninja rebuilds"
