#!/usr/bin/bash
# Stage the Rust candidate as an installed CMake: <prefix>/bin/cmake (copied,
# so the executable-relative CMAKE_ROOT lookup sees the prefix) and
# <prefix>/share/cmake-4.1 -> the pinned 4.1.0 distribution data (Modules,
# Templates, ...).  ctest/cpack are the pinned Kitware tools (the independent
# test runner); they resolve their own data from the same prefix.
set -eu
ROOT=/tmp/cmake-rust-validation.hSLgEZHR
PREFIX="${1:-$ROOT/rust-prefix}"
REPO="$(cd "$(dirname "$0")/.." && pwd)"
mkdir -p "$PREFIX/bin" "$PREFIX/share"
install -m 755 "$REPO/target/release/cmake" "$PREFIX/bin/cmake.new"
mv -f "$PREFIX/bin/cmake.new" "$PREFIX/bin/cmake"
for tool in ctest cpack; do
  install -m 755 "$ROOT/kitware-reference/bin/$tool" "$PREFIX/bin/$tool"
done
ln -sfn "$ROOT/kitware" "$PREFIX/share/cmake-4.1"
echo "$PREFIX/bin/cmake ($(git -C "$REPO" rev-parse --short HEAD))"
