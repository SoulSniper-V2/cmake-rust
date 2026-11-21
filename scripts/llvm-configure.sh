#!/usr/bin/bash
# Configure pinned LLVM (clang, X86, Release) with either the Rust candidate
# or the Kitware oracle into $ROOT/llvm-<which>-<tag>. Logs beside it.
set -u
ROOT=/tmp/cmake-rust-validation.hSLgEZHR
which="${1:?rust|oracle}"; tag="${2:-run}"
case "$which" in
  rust) "$(dirname "$0")/stage-prefix.sh" >/dev/null
        CMAKE="$ROOT/rust-prefix/bin/cmake" ;;
  oracle) CMAKE="$ROOT/kitware-reference/bin/cmake" ;;
  *) echo "bad which" >&2; exit 2 ;;
esac
out="$ROOT/llvm-$which-$tag"
rm -rf "$out"
start=$(date +%s.%N)
env -u MAKEFLAGS "$CMAKE" -S "$ROOT/llvm-project/llvm" -B "$out" -G Ninja \
  -DCMAKE_BUILD_TYPE=Release -DCMAKE_C_COMPILER=/usr/bin/cc -DCMAKE_CXX_COMPILER=/usr/bin/c++ \
  -DLLVM_ENABLE_PROJECTS=clang -DLLVM_TARGETS_TO_BUILD=X86 -DLLVM_INCLUDE_BENCHMARKS=OFF \
  > "$out.log" 2>&1
status=$?
echo "status=$status elapsed=$(echo "$(date +%s.%N) - $start" | bc)s" | tee -a "$out.log"
exit $status
