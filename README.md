# cmake-rust

[![CI](https://github.com/SoulSniper-V2/cmake-rust/actions/workflows/ci.yml/badge.svg)](https://github.com/SoulSniper-V2/cmake-rust/actions/workflows/ci.yml)

**A reimplementation of CMake in Rust.** It reads ordinary `CMakeLists.txt` files, runs the real compiler checks, and writes Ninja build files that are byte-for-byte identical to the ones Kitware CMake 4.1.0 writes for the same project.

It is good enough to configure and build **LLVM + Clang 20**. Clang's own test suite then reports exactly the same counts as on a build configured by real CMake.

| Project | What was compared | Result |
|---|---|---|
| **LLVM + Clang 20.1.8** | every configured and generated file | **614 / 614 identical** (after tool-path normalization) |
| | `ninja check-clang` on the candidate's build | 3,808 / 3,808 steps; 42,430 tests pass, 0 fail, counts identical to the Kitware-configured build |
| **Kitware CMake 4.1.0** (CMake building itself) | configured tree, build, then the upstream CTest suite | **312 / 312** files identical; 1,198 / 1,198 build steps; **683 / 683** tests pass |
| **zlib 1.3.1** | configure, generate, build, install, tests | all match |
| **Upstream RunCMake suite** | Ninja, Ninja Multi-Config and extra-generator cases | **5,866 / 5,866** eligible cases pass |

The configured trees, the CMake self-build and RunCMake were measured at commit `b24ded6`; `check-clang` was last run at `a609c6b`, whose LLVM build tree is byte-identical to `b24ded6`'s. The reference is Kitware CMake 4.1.0 (`2a2c2e0b`) built from source with the same flags and the same Ninja.

### Real-world projects

Beyond those four, about 60 unmodified open-source projects were configured with both tools. For each one, the end-of-configure interpreter state (`statediff.py`) and every file in the generated build tree (`treediff.py`) were compared, then the candidate's tree was built and its tests run. All of them match, except as noted below:

| Group | Projects |
|---|---|
| Libraries | fmt, spdlog, nlohmann/json, abseil, protobuf, libpng, curl, SDL2, glfw, Catch2, google benchmark, yaml-cpp, tinyxml2, libjpeg-turbo, brotli, zstd, lz4, libuv, pugixml, cJSON, flatbuffers, OpenCV (core, imgproc), googletest, freetype2, xz |
| More libraries | libgit2, libarchive, expat, libxml2, mbedtls, wolfssl, libssh2, jsoncpp, eigen, gflags, glog, leveldb, raylib, assimp, box2d, bullet3, llama.cpp, whisper.cpp, libwebp, openjpeg, harfbuzz, double-conversion, snappy, nghttp2, libevent, SQLiteCpp, cmocka, Unity, CUDA deviceQuery |
| Fetch at configure time | CLI11, cpr, magic_enum, a CPM.cmake project, an ExternalProject superbuild |
| Install, then `find_package` from a consumer | fmt, spdlog, yaml-cpp, glfw, nlohmann/json, libjpeg-turbo: the installed trees match too |
| Host packages via Find modules | SDL2, TBB, GTest, Vulkan, BLAS, OpenBLAS, LAPACK, PkgConfig, Threads, OpenSSL, ZLIB, CURL, Python3 |

Twelve of the libraries were also run in Ninja Multi-Config, Debug and RelWithDebInfo with shared libraries, and the CodeBlocks generator, comparing installed trees as well. LLVM was compared in ten more configurations: more subprojects (lld, mlir, polly, bolt, flang, lldb, clang-tools-extra), the runtimes (both from the top level and standalone), Debug with assertions and shared libraries, the dylib build, all targets, Ninja Multi-Config, and the nested native tablegen build. Every one of those is identical as well.

The sweeps found and fixed six bugs, each with a regression test. The only remaining byte differences are outside CMake's control: a Meson subbuild that cpr runs at configure time (timestamps, random directory names), and linker build IDs in libjpeg-turbo's installed executables, which embed the differing install prefix. The few remaining test failures were checked against the Kitware-configured build and are environmental (fixed ports, missing model files, Docker).

About 134,000 lines of Rust. The only dependencies are small crates for hashing, compression, tar, regex and JSON; the CMake language, generator expressions, policies, the target model, link computation and the Ninja generator are all implemented here.

---

## Quick start

```sh
cargo build --locked --release
target/release/cmake -S path/to/source -B path/to/build -G Ninja
target/release/cmake --build path/to/build
ctest --test-dir path/to/build --output-on-failure
```

You need Rust 1.87+ (edition 2024), Ninja, and a host C/C++ toolchain.

## How it is verified

The project has one rule: **real CMake is the oracle.** Kitware CMake 4.1.0 is treated as the specification, and every change is tested by running both tools on the same input and diffing what comes out. A feature counts as done when the outputs match exactly, not when its code compiles.

```
            CMakeLists.txt
           /              \
  Kitware CMake 4.1.0   cmake-rust
           \              /
       build.ninja, CTest files, install manifests,
       file-API replies, exported targets, interpreter state
                    |
              byte-for-byte diff
```

The harnesses in `scripts/`:

| Harness | Compares | Size |
|---|---|---|
| `difftest.py` | script mode (`cmake -P`): output, errors, exit codes | 1,261 cases |
| `clitest.py` | the command line: `-E` tools, `--build`, `--install`, presets | 512 cases |
| `projdiff.py` | project mode: generated files, install-tree exports, file-API replies | 160 projects (14 CUDA) |
| `statediff.py` | end-of-configure interpreter state: every variable, cache entry and property | LLVM: 94,998 lines, 0 differences |
| `conformance/` | Kitware's own RunCMake test suite | see [`docs/conformance.md`](docs/conformance.md) |
| `ninjadiff.py`, `dapdiff.py`, `autogendiff.py` | `build.ninja` structure, the debugger protocol, Qt AUTOGEN | |

`statediff.py` injects itself through `CMAKE_PROJECT_TOP_LEVEL_INCLUDES`, so it compares the full interpreter state without modifying the project under test. At `daa1f60` it shows no differing lines for LLVM, Kitware CMake, zlib, or any of the 128 `tests/projects` cases whose configure completes, including on a second `--reconfigure` pass.

To reproduce the zlib check:

```sh
cargo test --locked
scripts/stage-prefix.sh                     # <root>/rust-prefix/bin/cmake + share/cmake-4.1
RUST_CMAKE=<root>/rust-prefix/bin/cmake scripts/verify-zlib.sh
```

`verify-zlib.sh` needs Git, Python 3, Ninja, a C toolchain and a reference Kitware CMake with CTest. It uses two clean copies of zlib v1.3.1 (`51b7f2ab`) and isolates the Rust side's executable search path. It checks the generated header, the normalized per-target compiler arguments, the upstream tests, the installed paths and exported symbols, four gzip encode/decode pairings, and that a second `ninja` run is a no-op. Override tool paths with `RUST_CMAKE`, `UPSTREAM_CMAKE`, `CTEST` or `NINJA`, and set `ZLIB_PIN_DIR` to reuse a checkout.

The candidate is staged as an installed CMake: `bin/cmake`, plus `share/cmake-4.1` holding the pinned 4.1.0 distribution modules, which are read as data. The Kitware `cmake` binary is never called by the candidate. The pinned Kitware `ctest` is used only as an independent test runner.

## What's implemented

- **The CMake language:** lexer, interpreter, scoping, `if()` conditions, lists, strings, math, regex (a port of CMake's own regex engine), `file()`, and the policy table up to 4.1.
- **Generator expressions:** the full evaluator, including context checks and diagnostics.
- **The target model:** usage requirements, link features and strategies, link-line and runtime-path computation, imported targets, `install()` and `export()`, CPS and pkg-config package info.
- **Generators:** Ninja and Ninja Multi-Config, with the CodeBlocks, CodeLite, Eclipse CDT4, Kate and Sublime Text 2 extra generators. Also C++20 module scanning, precompiled headers and unity builds, the Fortran pipeline, and CUDA (separable compilation and device linking, checked against nvcc 12; HIP is ported but untested here).
- **Around the generator:** `try_compile` / `try_run`, CMake presets, the file API, the DAP debugger, Qt AUTOGEN, instrumentation, SARIF output, `--system-information`, `--graphviz`, and `cmake -E` including the MSVC-side `vs_link_*` / `__create_def` tools.
- **Byte-exact values:** CMake strings are byte strings, not UTF-8. Latin-1 files, high-byte tool output and non-UTF-8 cache entries round-trip unchanged, and terminal colors match CMake 4.1.
- **Cross-compiling configurations** (Darwin, MSVC/clang-cl, Android, AIX, Cygwin/MSYS and others) are generated to match the reference. The build itself runs on the host toolchain.
- **`--explain <target> <flag>`** prints the dependency path and the source line that put a compile definition, include directory or option on a target. The integration tests check that an explained definition really reaches the compiler.

## Known gaps

Each of these fails with an explicit error, never silently:

- The Unix Makefiles, Watcom WMake and Green Hills MULTI generators. Ninja is the default when no `-G` is given. `cmake --help` still lists every 4.1.0 generator, and picking an unimplemented one fails with a clear message.
- Makefile-only dependency scanning for Fortran and Java in `cmake -E cmake_depends`.
- XCFramework linking, which needs macOS `plutil`.
- ISPC and Swift sources in the Ninja generator.

Two upstream outputs are ordered by pointer or hash value and can't be reproduced exactly: the file-API `dependencies` order, and the order of some reserved-name build-database errors.

`file(DOWNLOAD)` and `file(UPLOAD)` load the host's `libcurl.so.4` at the first transfer. Status and error strings come from the `curl_easy_strerror` table of the curl 8.14.1 that CMake vendors. `LOG` output comes from the host libcurl, so its wording can differ.

## Repository layout

```
src/            the implementation (interpreter, commands/, genex/, generator/, presets/, fileapi/, debugger/ ...)
tests/          integration tests: cli/ JSON cases, projects/ end-to-end projects, scripts/ cmake -P cases
scripts/        differential harnesses and oracle runners
compatibility/  the RunCMake conformance baseline
docs/           architecture, target model, conformance notes
legacy/         the first interpreter, kept for reference
```

Start with [`docs/architecture.md`](docs/architecture.md).

## How this was built

I wrote this with AI coding agents. I designed the harness and the rule that real CMake is the oracle. I split the work into scoped tasks (one CMake subsystem, or one RunCMake suite, per task). The agents ran in parallel worktrees, and a change landed only when the diffs against Kitware came back clean. The commit history shows that loop, one subsystem at a time.

## License and attribution

BSD-3-Clause; see [LICENSE](LICENSE). This is not Kitware's CMake and is not affiliated with or endorsed by Kitware. "CMake" is a trademark of Kitware, Inc. Behaviour is ported from CMake 4.1.0, and some test inputs are copied verbatim from its test suite with their origin noted in each file. See [NOTICE](NOTICE).
