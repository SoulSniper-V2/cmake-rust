#!/usr/bin/bash
# Configure pinned Kitware CMake 4.1.0 with the staged Rust candidate or the
# oracle into $ROOT/cmake-<which>-<tag>.
set -u
ROOT=/tmp/cmake-rust-validation.hSLgEZHR
which="${1:?rust|oracle}"; tag="${2:-run}"
case "$which" in
  rust) prefix="${CMAKE_RUST_STAGE_PREFIX:-$ROOT/rust-prefix}"
        "$(dirname "$0")/stage-prefix.sh" "$prefix" >/dev/null; CMAKE="$prefix/bin/cmake" ;;
  oracle) CMAKE="$ROOT/kitware-reference/bin/cmake" ;;
  *) echo "bad which" >&2; exit 2 ;;
esac
out="$ROOT/cmake-$which-$tag"
rm -rf "$out"
start=$(date +%s.%N)
env -u MAKEFLAGS -u CMAKE_RUST_DATA_ROOT "$CMAKE" -S "$ROOT/kitware" -B "$out" -G Ninja \
  -DCMAKE_BUILD_TYPE=Release > "$out.log" 2>&1
status=$?
echo "status=$status elapsed=$(echo "$(date +%s.%N) - $start" | bc)s" | tee -a "$out.log"
exit $status
