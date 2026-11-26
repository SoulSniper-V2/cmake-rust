# Unix Makefiles port status

Branch: `t3/unix-makefiles-6`

Most recent completed full Unix Makefiles lane after the Batch 7 fixes and the second `main` merge: **136/163** (`/tmp/pd-umf-fresh.log`; the lane grew by two extra-generator cases on `main`).  The prior post-merge baseline was 108/161 (`/tmp/pd-umf-batch7-baseline.log`).  Ninja lane must remain green.

Known remaining projdiff failure groups: autogen, CUDA, C++ modules,
install/platform variants, ASM dependency scanning, object-library corners,
PCH/unity, Fortran modules, response files, Makefile2/progress/DependInfo
ordering, and try_compile nested logs.  `link/items` still has target-specific
link model differences.  `install/subdir_dest` is down to object-file `./`
normalization in `sub/cmake_install.cmake`.

Evidence still pending: 161/161 Unix Makefiles lane, zlib byte parity/build,
Kitware CMake self-configure/build, RunCMake lane, and CLI regression additions.



## Part 6 setup merge

Started from `t3/unix-makefiles-5` at `97e221f` and merged current `main`
`7a1028b` (ISPC generation, `UsageItem.raw`, and the compound-relative
`find_program` fix).  The merge was automatic with no conflicts.  Post-merge
gates before the merge amendment: `cargo fmt --check` clean, clippy clean,
940/940 Rust tests, difftest 1283/1283, clitest 538/538, and Ninja projdiff
167/167.  Logs: `/tmp/umf6-merge-gates-rust.log`,
`/tmp/umf6-merge-difftest.log`, `/tmp/umf6-merge-clitest.log`, and
`/tmp/umf6-merge-projdiff-ninja.log`.


## Part 6 RunCMake baseline and interface-library rules

Fresh merged-branch Unix Makefiles RunCMake lane: **5463/5582 cases passed**
(67 failed, 52 not reached; development lane, audit disabled, one repetition),
evidence `/tmp/umf6-runcmake-baseline/report.json`, driver log
`/tmp/umf6-runcmake-baseline-driver.log`, suite/failure triage
`/tmp/umf6-runcmake-buckets.txt`.  Largest failure groups are GoogleTest
discovery (20), InterfaceLibrary convenience rules (6), Instrumentation
snippet verification (5), BuildDepends freshness (4), linker-aware lint/
link-language cases, Make target messages, and add_subdirectory exclusion/
progress behavior.  Byproducts remains the dominant not-reached cascade (46).

Unix Makefiles now treats an interface library with sources/file sets as a
build-system utility-style target, matching `cmMakefileTargetGenerator::New`:
it emits `CMakeFiles/<iface>.dir/build.make`, `DependInfo.cmake`, clean and
progress files, Makefile2 all/rule/codegen/clean convenience rules, local
Makefile convenience/fast rules, `TargetDirectories.txt` entries, and
`Makefile.cmake` dependency-info entries.  Interface libraries do not gain
nominal linked artifacts in their clean scripts.  Focused RunCMake evidence:
`/tmp/umf6-runcmake-ifaceprobe/report.json` passes 25/25
(`driver log /tmp/umf6-runcmake-ifaceprobe-driver.log`); manual scratch
evidence is under `/tmp/umf6-iface-*`.


BuildDepends freshness is fixed in two upstream-aligned paths: custom-command
DEPFILE ingestion now applies `cmReadGccDepfile`'s current-binary prefix and
collapse rules to depfile targets as well as dependencies (so relative outputs
such as `subexe.c` become the real top-level Makefile target), and user
manifest sources are dependencies of link rules through
`cmMakefileTargetGenerator::AppendLinkDepends`.  Focused RunCMake evidence:
`/tmp/umf6-runcmake-bdprobe/report.json` passes 57/57
(driver log `/tmp/umf6-runcmake-bdprobe-driver.log`).

## Round 5 lane A: exact Makefile link whitespace and zlib end-to-end

Root-caused and fixed exact `<FLAGS> <LINK_FLAGS>` whitespace handling in
Unix Makefiles link rules.  `CMAKE_EXE_LINKER_FLAGS` may legitimately begin
with whitespace (the compiler-ABI probe produces this), and try-compile
modules may prepend a definition with whitespace; Makefile expansion must
preserve that leading whitespace instead of shell-word joining or trimming
it.  At the same time, target `LINK_FLAGS` properties are appended as exact
values rather than being concatenated with artificial trailing spaces, so
ordinary `link.txt` files do not gain doubled internal separators.

Fresh zlib configure evidence after this fix:
`/tmp/zlib-umf5-configure-final-diff2.log` — configure stdout equal and
76/76 configure-time generated files identical after only executable/source
binary/install path plus upstream random try-compile/timing normalization.

zlib Unix Makefiles build evidence: `/tmp/zlib-umf5-build-final-diff.log`.
Both oracle and candidate trees were built with `make -j4`; candidate
`ctest` passed 2/2; a second `make` performed no compile/link actions; and
touching `gzguts.h` rebuilt exactly the same 31 relative outputs in both
trees (the gz*/zutil objects for shared and static zlib, dependent archives,
shared library, link depfiles, and example/minigzip executables).  The raw
build/no-op/rebuild logs differ only in parallel target scheduling and
progress ordering, not in actions or rebuilt file sets.


## Round 5 lanes B and E baseline

Full RunCMake development inventory against the Unix-Makefiles oracle build:
**5291/5582 cases passed** (232/274 selected suites fully passing),
evidence `/tmp/umf5-runcmake-full`, driver log
`/tmp/umf5-runcmake-full-driver.log`, bucket summary
`/tmp/umf5-runcmake-buckets.txt`.  Largest failure buckets are linker
LINK_LIBRARY/LINK_GROUP/LINKER-prefix expansion, Makefile target collision
warnings in BuildDepends, Byproducts cascade/not-reached cases,
instrumentation/launcher/lint integration, and C++ module cases.

Makefile target and prerequisite names now use the upstream Unix Makefile
escaping conversion (`=`, `$`, backslash, spaces, and `#`), include paths in
target build files are escaped the same way, recursive make invocations shell-
quote paths containing spaces, and progress-marks arguments preserve the
upstream `//CMakeFiles` spelling.  A subdirectory whose name contains a space
now builds successfully (`/tmp/umf5-spaceprobe2`).  Scannable C++ module
targets are rejected with the upstream unsupported-generator fatal error;
`tests/projects/export/modules` now passes the Unix Makefiles projdiff lane.
AutoRcc metadata records the real generator name instead of hardcoding Ninja.
A focused RunCMake probe after the escaping work
(`/tmp/umf5-runcmake-bdprobe`) now passes 72/82 combined BuildDepends and
InterfaceLibrary cases.  BuildDepends is down to four failures (manifest and
depfile freshness plus one custom-command depends result); the six remaining
InterfaceLibrary failures are all missing interface-target convenience/build
rules (`No rule to make target 'iface'` or `CMakeFiles/iface.dir/all`).

Focused evidence `/tmp/umf5-pd-auto-mod.log`: `export/modules` passes;
`autogen/imported` and `autogen/warn` now differ only in `Makefile2` and
`TargetDirectories.txt` ordering/content, and `cxxmodules/bdb_single` still
has cascading generated-file differences after the matching configure error.

Unix Makefiles compile rules now implement the upstream launcher stack:
`RULE_LAUNCH_COMPILE` outermost with `<TARGET_NAME>`, `<TARGET_TYPE>`,
`<OBJECT>`, `<SOURCE>`, `<LANGUAGE>`, and `<CMAKE_CURRENT_BINARY_DIR>`
placeholder expansion; `cmake -E __run_co_compile` for clang-tidy, cpplint,
cppcheck, include-what-you-use, and icstat; compiler launchers either direct
or passed through `--launcher=`; `SKIP_LINTING`; clang-tidy export-fixes paths;
and removal of unknown stale export-fixes YAML files.  Link launcher
placeholders now also receive target name/type/labels and the current binary
directory.  Focused lane `/tmp/umf5-runcmake-lintprobe2`: all 102/102 cases
pass across CompilerLauncher, ClangTidy, Cpplint, Cppcheck,
IncludeWhatYouUse, and MultiLint.

Unix Makefiles link/archive commands now honor `RULE_LAUNCH_LINK` with
target/directory/global lookup, generator-expression evaluation, and one
trailing space, matching upstream and the existing Ninja behavior.  The
launcher is prefixed independently to every archive create/append/finish and
normal link command written to `link.txt`.  Focused lane
`/tmp/umf5-runcmake-linkprobe2`: `RunCMake.target_link_libraries-LINKER-prefix`
is now 25/25, eliminating the three remaining `LINKER_expansion` failures;
those were real-linker execution caused by the missing test launcher, not bad
`LINKER:` expansion.

Makefile custom-command launchers now expand `<OUTPUT>`, `<TARGET_NAME>`,
`<CMAKE_CURRENT_BINARY_DIR>`, and `<ROLE>` before use, eliminating the literal
`<ROLE>` shell redirection failure in Instrumentation.  Focused lane:
`/tmp/umf5-runcmake-instrprobe` improved Instrumentation from 26 failures to
5 verification-only failures (44/50 pass).  Compile commands now prepend the
compiler launcher, and generated Makefile/build.make/Makefile2 headers emit
`VERBOSE = 1` when `CMAKE_VERBOSE_MAKEFILE` is enabled.  Focused lane
`/tmp/umf5-runcmake-compilerprobe`: CompilerLauncher 0 failures and
LinkerLauncher 0 failures; ClangTidy still has 8 lint-integration failures.

Unix Makefiles now emit CPack `package`/`package_source` local rules and
per-directory `TargetDirectories.txt` entries when the generated CPack config
files exist.  Package commands omit the `cd <root> &&` prefix in the root
Makefile but retain it in subdirectory Makefiles, matching upstream.  When
building CMake itself (`CMake_BINARY_DIR` set and not cross-compiling),
install commands use the build-tree `bin/cmake` path relative to each
subdirectory rather than the configure-time cmake executable.  Focused
kitchen-sink evidence: all CPack rule differences disappeared in
`/tmp/umf5-pd-kitchen-cpack2.log` (remaining differences are unrelated LTO
try-compile/launcher/PCH issues).

RunCMake full lane after the `cmake --build` fix: 5362/5582 pass, 168 fail,
52 not reached, 8 out-of-scope standalone-tool, 1 oracle failure, 5
out-of-scope generator.  Evidence `/tmp/umf5-runcmake-full2/report.json` and
`/tmp/umf5-runcmake-full2-driver.log`; improvement from the first full lane
(5291 pass) is +71 cases.  Largest remaining buckets are Byproducts clean
convergence, Instrumentation (`ROLE:` shell corruption), GoogleTest discovery,
compiler/lint launcher echo mismatches, BuildDepends duplicate Makefile2
recipes, interface-library convenience rules, and `LINKER:` expansion.

`cmake --build` now generates Unix Makefiles build commands upstream-style:
`cmake -E env VERBOSE=1 <make> -f Makefile -j<N>` for verbose builds (rather
than passing GNU Make's version-only `-v`), `-f Makefile` always, and bare
`-j` for default parallelism.  Focused CLI result 2/2 in
`/tmp/umf5-clitest-buildmake.log`.  This was the dominant RunCMake cascade:
link-feature tests were never built because `gmake -v` exits after printing
its version.

`--check-build-system` now follows the upstream two-value argument error,
missing-check-file reconfigure path, process-cwd-relative dependency/output
timestamps, and ClearDependencies behavior.  Root `Makefile`,
`CMakeFiles/Makefile2`, and `CMakeFiles/Makefile.cmake` are rewritten
unconditionally where upstream deliberately avoids copy-if-different so a
reconfiguration cannot loop forever.  New focused CLI coverage is
`tests/cli/check-build-system.json`; focused result 4/4 in
`/tmp/umf5-clitest-checkbuild2.log`.

Kitware self-configure with Unix Makefiles currently has identical normalized
configure stdout (711 lines) but 52/1160 configure-tree file differences;
evidence `/tmp/umf5-self-configure-diff.log`.  Largest visible differences are
missing CPack `package`/`package_source` rules in directory Makefiles,
self-build `../bin/cmake` install command substitution, and a small set of
link/rule hash differences still being triaged.

## Batch 7 (summary labels, depend inputs, generated outputs)

Started from the fresh post-merge full lane: **108/161**
(`/tmp/pd-umf-batch7-baseline.log`).  Focused results are `ninja/labels` PASS,
`targets/wrap_commands` PASS, `targets/custom_relative_generated_dep` PASS, and
`install/target_objects` PASS (`/tmp/pd-batch7-focus1.log`).

- Unix Makefiles now writes target/global `Labels.txt` and `Labels.json`
  summaries with the same target, directory, CMAKE_DIRECTORY_LABELS, and
  source-label layout as `cmGlobalGenerator::WriteSummary`.
- Target `depend` rules depend on the sorted custom-command output set, so
  generated wrapper/moc/uic sources exist before dependency scanning.
- `progress.marks` excludes targets excluded from the default build, matching
  `InitializeProgressMarks`/`IsExcluded`; explicit target builds still use the
  target's own progress numbers.
- Generation-time output files (`file(GENERATE)`, Apple bundle/plist outputs)
  are recorded with their owning directory and appear in
  `CMAKE_MAKEFILE_PRODUCTS` in `Makefile.cmake`.
- Unix Makefiles link generation now supports object/library response files
  (`objectsN.rsp`, `linkLibs.rsp`), split archive create/append command lists,
  correct response-file dependency ordering, module-definition source
  dependencies, `TARGET_IMPLIB` and target version placeholders, executable
  exports link rule selection, and AIX archived shared-library/export-list
  templates.  Focused results: install/dll, platform/windows_dll,
  platform/cygwin_dll, platform/msys_dll, platform/windows_exe_exports,
  platform/windows_export_all, ninja/response_file_limit, install/aix, and
  platform/aix_shared all PASS (`/tmp/pd-rsp-platform-focus2.log`,
  `/tmp/pd-aix-focus.log`).  ninja/response_file still differs only in the
  upstream pointer-ordered Makefile2 dependency sequence.

Makefile2 dependency-ordering oracle nondeterminism is now directly
reproduced.  Five independent oracle configures of a ten-target probe gave
`t3,t4,t1,t2,t5,t6,t10,t7,t8,t9` four times and
`t5,t6,t1,t2,t3,t4,t7,t8,t9,t10` once; eight independent oracle configures of
`ninja/response_file` produced `sh,o,s`, `o,s,sh`, `sh,o,s`, `o,sh,s`,
`o,s,sh`, `s,sh,o`, `s,sh,o`, `sh,o,s`.  This follows upstream
`cmTargetDependSet` ordering by `cmGeneratorTarget` pointer value, so
ordering-only differences in cases such as `ninja/response_file`,
`link/items`, `export/shared_deps`, and `install/namelink` are documented
oracle-nondeterministic exceptions rather than implementable stable ordering.

Follow-up fixes now implemented: Unix Makefiles expands `<CUDA_COMPILE_MODE>`,
emits `.i`/`.s` fallback rules through `cmake_unimplemented_variable` when a
language has no preprocess/assembly rule, writes `compile_commands.json`
entries with Makefile-generator concrete flag substitution, places compiler
dependency flags in `<FLAGS>`, reports a missing non-static link rule as the
upstream bare `cmSystemTools::Error`, uses the outer generator for nested
`try_compile` projects with the correct Makefile build command, disables
build-color markup inside try_compile while retaining progress markup, and
computes top-level object convenience names independently of CUDA custom
output extensions.  `targets/defaults`, `cuda/plain`, `cuda/compile_modes`,
`try_compile/nested_log`, `export/cps`, `install/dll`, and
`ninja/response_file_limit` now pass focused projdiff runs.  Makefile compile
rules also provide target PDB placeholders and the MSVC `cmake_cl_compile_depends`
wrapper; link rules provide target PDB and install-name-directory placeholders.

## Batch 6 (custom source properties and Fortran object modules)

Uncommitted/follow-up state at first: focused results are `pch_unity/options` PASS,
`targets/fortran_objects` PASS, and `targets/custom_relative_generated_dep` down
to `CMakeFiles/progress.marks` (`/tmp/pd-finalfocus.log`).

- Output-generating custom commands now use `ConstructComment`'s default
  `Generating ...` message and allocate progress actions when COMMENT is absent.
- Source-specific INCLUDE_DIRECTORIES and COMPILE_DEFINITIONS are prepended or
  appended in compile/preprocess/assembly commands and documented with the
  oracle's `# Custom include directories` / `# Custom defines` blocks.
- Unix Makefiles Fortran compilation no longer emits `-J` merely because the
  target lives in a subdirectory; compilation executes in that directory, unlike
  Ninja.  `$<TARGET_OBJECTS>` object-library Fortran modules recursively make
  consuming targets Fortran-dependent, and source object libraries appear in
  both direct and forward linked DependInfo entries.

## Batch 5 (PCH, source flags, Fortran module fixes)

Started from the 103/161 post-merge baseline (`/tmp/pd-umf-baseline.log`).
Focused lane is now `pch_unity/basic` PASS, `asm_extension_precedence` PASS,
`targets/fortran_modules` PASS, and `link/items` down to Makefile2 dependency
ordering (`/tmp/pd-focus3.log`).

- `flags.make` now writes all compiler comments before the sorted language
  blocks and uses target-level compile variables rather than leaking per-object
  PCH options into every compilation.
- Object compile, preprocess, and assembly rules carry source-specific
  COMPILE_FLAGS/COMPILE_OPTIONS, Fortran format/preprocess flags, and PCH
  create/use options; PCH headers/files are object dependencies and dependency
  rows.  Preprocess/assembly object basenames strip the final object extension
  (`.gch` included), and generated build-tree source dependencies are
  top-relative.
- Local Makefile object convenience rules now follow
  `GetObjectFileNameWithoutTarget`/`WriteObjectConvenienceRule`: source-
  extension aliases, language-sensitive `.i`/`.s` rules, and no phantom `.o`
  suffix for PCH objects.
- PCH `REUSE_FROM` target-level dependencies are no longer misclassified as
  codegen dependencies.  Dependency-scan target definitions are quoted like
  `EscapeForCMake`.  Spurious per-target `CMAKE_Fortran_MODULE_DIRECTORY`
  entries were removed from `CMakeDirectoryInformation.cmake`.
- Validation for this batch: fmt/clippy clean, 939/939 tests, Ninja projdiff
  161/161, difftest 1279/1279, clitest 524/524 (`/tmp/validate-precommit.log`).

## Batch 2 (progress + external objects)

- `targets/usage`, `targets/object_files`, `install/subdir_dest`,
  `export/shared_deps` now pass; full lane was 100/160 before this batch.
- Fixed: codegen target dependencies in Makefile2 (add_dependencies entries are
  codegen deps), unconditional CMAKE_DEPENDS_DEPENDENCY_FILES block,
  zero-action progress.make files, external object (`$<TARGET_OBJECTS>` in
  sources) handling in build.make/link.txt, generator-aware object directory
  (`/./` only for Ninja), oracle progress.marks counting (direct deps of
  in-directory targets counted recursively).
- Remaining known: Fortran/ASM scanner-kind DependInfo layout (CMAKE_DEPENDS_CHECK_*
  blocks, depfile-format tags), per-source custom flag comments, AUTOGEN, CUDA,
  PCH/unity, install/target_objects file(GENERATE) deps.

## Batch 3 (DependInfo overhaul)

Rewrote `DependInfo.cmake` generation to follow
`cmLocalUnixMakefileGenerator3::WriteDependLanguageInfo`:

- Split implicit-dependency scanning into CMake-scanned vs compiler-scanned
  object sets; `CMAKE_DEPENDS_DEPENDENCY_FILES` rows now carry the depfile
  format (`gcc`) rather than the compiler executable, and linker depfile rows
  use top-relative paths including the subdirectory prefix.
- `CMAKE_DEPENDS_CHECK_<LANG>` source/object pairs, target compile
  definitions, and per-language include paths written for CMake-scanned
  languages.
- Per-source depfile emission now follows `CMAKE_<LANG>_DEPENDS_USE_COMPILER`
  (`cmMakefileTargetGenerator::WriteObjectRuleFiles`): ASM and Fortran no
  longer get spurious `-MD`/`-MF` compile flags, `compiler_depend.ts`
  dependencies, or `.d` entries in clean scripts.  The
  `compiler_depend.make` include and placeholder files follow the
  project-wide `CMAKE_DEPENDS_USE_COMPILER` toggle instead.

Focused results: `targets/object_files` bad 0, `asm_extension_precedence`
6->2 (remaining: flags.make language ordering, Makefile), `asm_mixed_linker`
10->3, `link/items` 9->3.  Validation before commit: fmt/clippy clean, 939/939
tests, Ninja projdiff 160/160, difftest 1263/1263, clitest 515/515.

## Batch 4 (object directories + zlib build evidence)

- Object file directories are now created at generate time
  (cmMakefileTargetGenerator behavior), so compiler-generated depfiles in
  source-mirroring subdirectories (e.g. `CMakeFiles/example.dir/test/`)
  actually work.
- zlib evidence: candidate configures zlib with `-G "Unix Makefiles"` and
  `make -j6` builds example, minigzip, libz.so.1.3.1; `ctest` 2/2 passed;
  incremental rebuild after touching compress.c rebuilds compress.c.o.
  Logs: /tmp/zlib-cand-conf.log, /tmp/zlib-cand-build.log,
  /tmp/zlib-cand-test.log, /tmp/zlib-cand-rebuild.log.
  Normalized byte parity vs oracle: 58/75 identical; remaining diffs in
  Makefile, Makefile2, Makefile.cmake, compiler_depend.make and zlib target
  build.make files.
