# CMake in Rust

An independent implementation of the CMake interpreter and Ninja generator. It evaluates existing `CMakeLists.txt` files, performs native compiler checks, and writes Ninja rules, CTest definitions, and an install manifest. It uses the host toolchain to compile and link the resulting projects.

## Build

```sh
cargo build --locked --release
target/release/cmake -S /path/to/source -B /path/to/build -G Ninja
target/release/cmake --build /path/to/build
ctest --test-dir /path/to/build --output-on-failure
```

The supported build configuration is native C and C++ with Ninja on Linux or macOS. Unsupported commands report their CMake source file and line. The binary currently covers a subset of CMake, and the version reported to scripts describes the targeted language baseline.

During configuration, `--explain <target> <flag>` prints the target dependency path and source line for compile definitions, include directories, and target compile options. The integration tests check that an explained definition reaches the real compiler.

## Differential verification

```sh
cargo test --locked
cargo clippy --locked --all-targets --all-features -- -D warnings
scripts/verify-zlib.sh
```

The script requires Git, Python 3, Ninja, a native C toolchain, and a reference installation of Kitware CMake with CTest. It uses two clean copies of zlib v1.3.1 at commit `51b7f2abdade71cd9bb0e7a373ef2610ec6f9daf`, and isolates the Rust side's executable search path. The reference is invoked separately. Set `ZLIB_PIN_DIR` to use an existing clean checkout of that commit. Set `RUST_CMAKE`, `UPSTREAM_CMAKE`, `CTEST`, or `NINJA` to override tool paths.

The harness checks the generated header, normalized per-target compiler arguments, upstream test results, installed paths and exported symbols, four gzip encode/decode pairings, and no-op Ninja rebuilds. Both tools run the stock zlib script that renames `zconf.h` inside each disposable source copy.

## Compatibility milestones

Differential results are measured against Kitware CMake 4.1.0 (commit
`2a2c2e0b`) built from source, with the same flags and Ninja. The candidate is
staged as an installed CMake (`scripts/stage-prefix.sh`: `bin/cmake` plus
`share/cmake-4.1` holding the pinned 4.1.0 distribution modules, which are
read as data). The Kitware `cmake` executable is never invoked by the
candidate. The pinned Kitware `ctest` is the independent test runner.

- **LLVM + Clang 20.1.8** (`87f0227c`, Release, `LLVM_ENABLE_PROJECTS=clang`,
  `LLVM_TARGETS_TO_BUILD=X86`, `LLVM_INCLUDE_BENCHMARKS=OFF`):
  - Configure and generate succeed.
  - The configure log, `CMakeCache.txt`, `rules.ninja` and `build.ninja` (41 MB, 21,997 statements) are byte-identical to the oracle's. Tool paths are normalized, and the regeneration-dependency list sorts differently because the prefix paths differ.
  - `ninja check-clang` on the candidate's build: 42,430 passed, 0 failed, 3,846 unsupported, 29 expectedly failed.
  - Times: configure about 5 s, generate about 6 s (oracle 1.8 s).
- **Kitware CMake 4.1.0**: the configure log, `CMakeCache.txt` and `rules.ninja` are identical to the oracle's, and `build.ninja` differs only in the regeneration-dependency list. The build completes (1198/1198 steps), and the upstream CTest suite run by the built `ctest` reports 684/684 passed. The oracle-configured build gets the same result.
- **zlib 1.3.1**: configure, generate, build and install match the oracle, and the upstream tests pass.

Harnesses:
- `scripts/difftest.py`: script mode, 1239 cases.
- `scripts/clitest.py`: command line, 329 cases.
- `scripts/projdiff.py`: project mode, generated files byte-compared.
- `scripts/ninjadiff.py`: structural `build.ninja` comparison.
- `scripts/llvm-configure.sh` and `scripts/kitware-configure.sh`: oracle/candidate configure runners.
- `scripts/conformance/`: upstream RunCMake.
