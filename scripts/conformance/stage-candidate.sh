#!/usr/bin/env bash
set -euo pipefail

if [[ $# != 2 ]]; then
    echo "usage: $0 <binary> <new-prefix>" >&2
    echo "Set KITWARE_SOURCE to override the pinned checkout location." >&2
    exit 2
fi

source_dir=$(realpath "${KITWARE_SOURCE:-/tmp/cmake-rust-validation.hSLgEZHR/kitware}")
binary=$(realpath -e -- "$1")
prefix=$(realpath -ms -- "$2")
revision=2a2c2e0b2658f7c07e00fcf649882fd5578c2fc1

if [[ ! -x "$binary" || ! -f "$binary" ]]; then
    echo "Expected an executable file: $binary" >&2
    exit 1
fi
if [[ $(git -C "$source_dir" rev-parse HEAD) != "$revision" ||
      -n $(git -C "$source_dir" status --porcelain) ]]; then
    echo "Expected a clean Kitware checkout at $revision: $source_dir" >&2
    exit 1
fi
if [[ -e "$prefix" || -L "$prefix" ]]; then
    echo "Stage into a new prefix: $prefix" >&2
    exit 1
fi

mkdir -p -- "$(dirname -- "$prefix")"
scratch=$(mktemp -d -- "${prefix}.stage.XXXXXXXX")
trap 'rm -rf -- "$scratch"' EXIT
mkdir -p -- "$scratch/bin" "$scratch/share/cmake-4.1"
install -m 755 -- "$binary" "$scratch/bin/cmake"
# Install layout: the independent pinned Kitware ctest/cpack sit next to
# cmake, where cmSystemTools::GetCTestCommand/GetCPackCommand look for them
# (cmake --workflow runs them).
reference_bin=$(realpath "${KITWARE_REFERENCE_BIN:-/tmp/cmake-rust-validation.hSLgEZHR/kitware-reference/bin}")
for tool in ctest cpack; do
    if [[ -x "$reference_bin/$tool" ]]; then
        ln -s -- "$reference_bin/$tool" "$scratch/bin/$tool"
    fi
done
ln -s -- "$source_dir/Modules" "$scratch/share/cmake-4.1/Modules"
ln -s -- "$source_dir/Templates" "$scratch/share/cmake-4.1/Templates"
mv -nT -- "$scratch" "$prefix"
if [[ -d "$scratch" ]]; then
    echo "Stage prefix appeared during staging: $prefix" >&2
    exit 1
fi
