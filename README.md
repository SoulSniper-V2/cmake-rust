# CMake in Rust

An independent implementation of the CMake interpreter and Ninja generator. It evaluates existing `CMakeLists.txt` files, performs native compiler checks, and writes Ninja rules, CTest definitions, and an install manifest. It uses the host toolchain to compile and link the resulting projects.

## Build

```sh
cargo build --locked --release
target/release/cmake -S /path/to/source -B /path/to/build -G Ninja
target/release/cmake --build /path/to/build
ctest --test-dir /path/to/build --output-on-failure
```

Ninja and Ninja Multi-Config (with the CodeBlocks, CodeLite, Eclipse CDT4, Kate and Sublime Text 2 extra generators) are the implemented generators. Without `-G` or `CMAKE_GENERATOR`, the default is Ninja; Kitware's Linux default, Unix Makefiles, is not implemented. `cmake --help` still lists every Kitware 4.1.0 generator, and choosing an unimplemented one fails with an explicit message. Cross-compiling configurations (Darwin, MSVC/clang-cl, Android, AIX, Cygwin/MSYS and others) are generated to match the reference; the build itself runs on the host toolchain. Unsupported features report their CMake source file and line.

During configuration, `--explain <target> <flag>` prints the target dependency path and source line for compile definitions, include directories, and target compile options. The integration tests check that an explained definition reaches the real compiler.

## Differential verification

```sh
cargo test --locked
cargo clippy --locked --all-targets --all-features -- -D warnings
scripts/stage-prefix.sh   # <root>/rust-prefix/bin/cmake + share/cmake-4.1
RUST_CMAKE=<root>/rust-prefix/bin/cmake scripts/verify-zlib.sh
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
  `LLVM_TARGETS_TO_BUILD=X86`, `LLVM_INCLUDE_BENCHMARKS=OFF`), measured at `67963ae`:
  - The configured and generated tree is byte-identical to the oracle's (614 of 614 files after tool-path normalization).
  - `ninja check-clang` on the candidate's build (3808/3808 steps): 21,156 tests run, 42,430 passed, 0 failed, 3,846 unsupported, 29 expectedly failed, 10 skipped. The separately labeled reference build configured by Kitware CMake reports the identical counts.
- **Kitware CMake 4.1.0**, measured at `67963ae`: the configured tree is identical to the oracle's (312 of 312 files). The candidate-configured build completes, and the upstream CTest suite run by the built `ctest` reports 683/683 passed.
- **zlib 1.3.1**: configure, generate, build and install match the oracle, and the upstream tests pass.
- **Upstream RunCMake** (`scripts/conformance/run.py`, unfiltered inventory; Ninja, Ninja Multi-Config and extra-generator cases scored): 5,866 of 5,866 eligible cases pass at `67963ae`.

Known gaps, each failing with an explicit error rather than silently: the Unix Makefiles, Watcom WMake and Green Hills MULTI generators; Makefile-only dependency scanning for Fortran and Java in `cmake -E cmake_depends`; and XCFramework linking, which needs macOS `plutil`. Two upstream outputs are ordered by pointer or hash value and cannot be reproduced exactly: the file API `dependencies` order and the order of several reserved-name build-database errors.

`file(DOWNLOAD)` and `file(UPLOAD)` run on the host's `libcurl.so.4`, which is loaded at the first transfer. Status and error text come from the `curl_easy_strerror` table of the curl 8.14.1 that CMake vendors, generated from the pinned sources. `LOG` output comes from the host libcurl, so its wording can differ.

Harnesses:
- `scripts/difftest.py`: script mode, 1,253 cases.
- `scripts/clitest.py`: command line, 446 cases.
- `scripts/projdiff.py`: project mode, generated files byte-compared (including install-tree export files and file API replies), 135 projects.
- `scripts/dapdiff.py` (debugger protocol) and `scripts/autogendiff.py` (Qt AUTOGEN).
- `scripts/ninjadiff.py`: structural `build.ninja` comparison.
- `scripts/llvm-configure.sh` and `scripts/kitware-configure.sh`: oracle/candidate configure runners.
- `scripts/conformance/`: upstream RunCMake (see `docs/conformance.md`).
