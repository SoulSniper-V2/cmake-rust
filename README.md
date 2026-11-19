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

- zlib v1.3.1 configures, builds, tests, and installs on Linux x86-64 with GCC and macOS arm64 with Apple Clang.
- LLVM and Clang 20.1.8 and Kitware CMake 4.1.0 are the next acceptance gates. Each requires a clean configure, successful build, and its own upstream test suite. Both remain incomplete.
- `-DCMAKE_ROOT=/path/to/cmake/share` allows the interpreter to read supplementary distribution modules and templates from a directory with `Modules/` and `Templates/`. These are script inputs. The build does not invoke the Kitware `cmake` executable.
