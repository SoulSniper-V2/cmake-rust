# Unix Makefiles port status

Branch: `t3/unix-makefiles-10`

Most recent completed full Unix Makefiles projdiff lane after the link-information correction: **153/174** (`/tmp/umf10-projdiff-umf-final.log`).  The pre-correction part-10 lane was 133/174, and the part-7 historical baseline was 136/163 before the case count grew.  The full Unix Makefiles RunCMake lane is **5566/5566** (`/tmp/umf10-runcmake-full/report.json`).  Ninja lane must remain green.

Known remaining projdiff failure groups: autogen clean lists, CUDA,
install/platform variants (Darwin, MSVC, and clang-cl), ASM linker-language
details, ISPC, Makefile2/progress/DependInfo ordering, response-file/PCH
corners, and try-compile LTO probe link scripts.  `link/items` and
`export/shared_deps` are down to Makefile2 direct-dependency ordering.

Kitware self-configure, self-build, and self-built CTest evidence is below.

## Part 10 inherited Batch 9 validation

Part 10 began at part 9 head `8ed4a6040b944e7a0482bff4ebc0f07e8f315bb4`
(the merge of current `main` `4ca7cce`, faithful kwsys RelativePath, into the
part-8 line) and validated the previously uncommitted Batch 9 fixes.  The batch
makes link.txt use a distinct link-script shell escaping mode (no Makefile
dollar doubling; CMP0181 OLD/NEW and `$ORIGIN` handling follow Makefile vs
Ninja encoding), shell-escapes custom-command echo text, emits CMP0156 warnings
in Makefile target order, makes `test` depend on `all` when
`CMAKE_SKIP_TEST_ALL_DEPENDENCY` is false, shares NOTFOUND target-property
validation between Ninja and Unix Makefiles, reports object-rule languages not
enabled by the project, and uses a first `NeedRelinkBeforeInstall` port for
Makefile2 preinstall directory rules.  The language diagnostic uses the
directory file-scope frame, matching upstream's `CMake Error in CMakeLists.txt:`
generation diagnostic.

Focused Unix Makefiles evidence: **168/168** cases pass in the eight touched
suites (`RunCMake.project`, `RunCMake.RuntimePath`,
`RunCMake.target_link_options`, `RunCMake.target_link_libraries-LINKER-prefix`,
`RunCMake.CMP0156`, `RunCMake.include_directories`,
`RunCMake.add_custom_command`, and `RunCMake.BuiltinTargets`; report
`/tmp/umf10-focused-runcmake/report.json`, driver log
`/tmp/umf10-focused-runcmake.log`).  The same eight suites also pass **168/168**
with the default Ninja oracle, showing no regression in the shared link and
NOTFOUND code (`/tmp/umf10-focused-ninja-runcmake/report.json`, driver log
`/tmp/umf10-focused-ninja-runcmake.log`).

Gate evidence for the initial Batch 9 commit: `cargo fmt --check` clean,
clippy clean, **941/941** Rust tests (`/tmp/umf10-rust-gates.log`), difftest
**1283/1283** (`/tmp/umf10-difftest.log`), clitest **538/538**
(`/tmp/umf10-clitest.log`), and Ninja projdiff **173/174** on the full run
(`/tmp/umf10-projdiff-ninja.log`) with only the known
`pch_unity/options` `build.outputs` load flake; that case passes **1/1** in
isolation (`/tmp/umf10-projdiff-ninja-pch-rerun.log`).

The full Unix Makefiles RunCMake lane passes **5566/5566** cases
(`/tmp/umf10-runcmake-full/report.json`; development lane, audit disabled, one
repetition, oracle cache reused).  The first full Unix Makefiles projdiff lane
then exposed an echo-escaping regression in install-component lookup commands:
the batch's new shell escaping must wrap the component in literal quotes before
escaping, matching `cmGlobalGenerator.cxx`'s `cmWrap` plus `EscapeForShell`
ordering.  A focused probe passes 6/6 representative export, install, link,
and basic cases (`/tmp/umf10-projdiff-echo-focus.log`).  Post-correction gate
evidence is `cargo fmt --check` clean, clippy clean, Rust tests green,
difftest green, clitest green, and Ninja projdiff **174/174** including a
non-flaked `pch_unity/options` (`/tmp/umf10-echo-gates.log`).  The corrected
full Unix Makefiles projdiff lane is **151/174**
(`/tmp/umf10-projdiff-umf-postecho.log`).  The 23 remaining cases are the
platform/response-file clusters (Darwin, MSVC/clang-cl, CUDA, ISPC), ASM
linker-language details, autogen clean lists, Makefile2-only dependency-edge
differences (`export/shared_deps`, `link/items`, `ninja/response_file`),
LTO probe link scripts in `targets/kitchen_sink`, and the missing
`link/static_errors` diagnostics.

Makefile DependInfo generation now computes the target's memoized link
information unconditionally, matching the upstream side effect of
`cmCommonTargetGenerator::GetLinkedTargetDirectories` even when no Fortran or
C++20 module consumer uses the result.  This surfaces static-library
LINK_LIBRARY/LINK_GROUP validation exactly in `link/static_errors`.  The
Makefile link-line caller now uses that same memoized link information instead
of recomputing it, so generation diagnostics are not duplicated.  Focused
evidence: `link/static_errors` passes and `link/items` is reduced to the
pre-existing Makefile2 ordering difference (`/tmp/umf10-memo-focus.log`).
Gate evidence for this correction: `cargo fmt --check` clean, clippy clean,
**941/941** Rust tests, difftest **1283/1283**, clitest **538/538**, and Ninja
projdiff **174/174** (`/tmp/umf10-memo-gates.log`).  The final full Unix
Makefiles projdiff lane is **153/174** (`/tmp/umf10-projdiff-umf-final.log`).

Kitware CMake 4.1.0 self-configure with Unix Makefiles succeeds in both oracle
and candidate (`configure oracle=0 candidate=0`) with 1210/1207 tree files and
21 normalized differences (`/tmp/umf10-kitware-umf-treediff.log`, machine
summary `/tmp/umf10-kitware-umf-treediff.json`; the large scratch tree was
removed after capturing the logs).
The remaining self-configure differences are CMakeConfigureLog/CMakeOutput log
text, missing CMakeRuleHashes and nested-check dependency cache artifacts,
Makefile.cmake module dependency ordering, nested CheckCurses dependency
details, and `Source/../bin` versus `bin` makefile target spelling.  The
candidate-generated tree then completes `make -j6` self-build successfully
(`/tmp/umf10-kitware-umf-selfbuild.log`, rc=0), and the self-built `bin/ctest`
passes **684/684** tests in 368.40 seconds (`/tmp/umf10-kitware-umf-ctest.log`,
rc=0).

Pinned zlib v1.3.1 Unix Makefiles validation passes end to end
(`/tmp/umf10-zlib-umf.log`): oracle and
candidate configure and build, both CTest runs pass example/example64, 34
normalized compile commands match, install file/symlink entries/text/symbol
sets match, all four minigzip encode/decode pairings match, zconf.h is byte
identical, and second make runs perform no compile/link actions.

## Part 8 include-regex checkpoint

Part 8 started from part 7 head `4fbbfbd` and validated the previously
uncommitted include-regex patch.  Unix Makefiles now writes each directory's
real `INCLUDE_REGULAR_EXPRESSION` and complain regex into
`CMakeDirectoryInformation.cmake`, including inheritance into subdirectories.
Focused evidence `/tmp/umf8-incregex-out/report.json`:
`RunCMake.Make` passes 27/28, including `IncludeRegexSubdir`; the only
remaining failure is the pre-existing `CMP0113-NEW-build` case.  Gates:
`cargo fmt --check` clean, clippy clean, 940/940 Rust tests, difftest
1283/1283, clitest 538/538, and Ninja projdiff 168/168 on the full rerun
(`/tmp/umf8-incregex-projdiff-rerun.log`; one try-compile stderr flake passed
in isolation and on rerun).  Combined gate index `/tmp/umf8-incregex-gates.log`.

The part-8 branch then merged the worktree creation commit `523c513`
(source-group ordering, extra-generator fixes, `find_program`, and
imported-target lookup).  The merge was automatic; overlapping
`autogen.rs`/`flags.rs` hunks retained both behaviors.  Post-merge gates:
`cargo fmt --check` clean, clippy clean, 940/940 Rust tests, difftest
1283/1283, clitest 538/538, and Ninja projdiff 173/173 after rebuilding the
candidate (`/tmp/umf8-merge-gates.log`, `/tmp/umf8-merge-gates2.log`, and
`/tmp/umf8-merge-projdiff2.log`).

Unix Makefiles regeneration rules now include the upstream pre-checks and
build-hook startup.  `file(GLOB CONFIGURE_DEPENDS)` verification scripts and
stamps are Makefile.cmake inputs, and the verify script runs before both
`cmake_check_build_system` and `depend`; instrumentation-enabled builds define
`CTEST_COMMAND` in generated makefiles and start instrumentation after the
build-system check, with subdirectory rules using the same root `cd` prefix.
Focused evidence `/tmp/umf8-checkcmds-out/report.json`: **150/150** cases pass
across `RunCMake.file` and `RunCMake.Instrumentation`, including the glob
rerun cases and the make-program instrumentation verification.

CMP0113 custom-command deduplication now follows the upstream dependency-first
target generation order.  Each directory keeps a persistent visited-command
set seeded from its direct dependency targets, so NEW policy targets do not
repeat custom commands already emitted by a dependency while OLD/WARN targets
retain the legacy repeated rules.  Focused evidence
`/tmp/umf8-cmpmake-out/report.json`: **28/28** `RunCMake.Make` cases pass,
including the previously failing `CMP0113-NEW-build` failure-mode test.

Unix Makefiles directory rules and progress counts now use the upstream
`ComputeDirectoryTargets`/`DirectoryTargetsMap` model.  Explicit target
`EXCLUDE_FROM_ALL` values are evaluated per target, explicit false hoists the
target into ancestor directory rules, child-directory pruning uses the direct
final directory property, and progress seeding/counting is root-relative with
recursive dependency counting.  Focused evidence
`/tmp/umf8-addsub-out/report.json`: **29/29** `RunCMake.add_subdirectory`
cases pass.  The manual ExcludeFromAll probe matches the oracle dependency
lines exactly and produces progress counts root=8, ExcludeFromAll=8,
SubSub=4 (`/tmp/umf8-excl-cand` versus `/tmp/umf8-excl-oracle/build`).



## Part 7 setup, full sweep, and linker/autogen parity fixes

Started from `t3/unix-makefiles-6` at `1d8a7ad` and merged current `main`
`9a1d821`; merge commit `359e6b6` was automatic apart from the expected
overlaps in `src/generator/custom.rs` and `src/generator/target.rs`.

Fresh full Unix Makefiles RunCMake lane after the part-7 autogen and
regeneration-command fixes: **5532/5566 cases passed** (28 failed, 6 not
reached; 254/273 selected suites fully passing; development lane, audit
disabled, one repetition).  Evidence `/tmp/umf7-runcmake-full/report.json`,
driver log `/tmp/umf7-runcmake-full-driver.log`, suite/failure triage
`/tmp/umf7-runcmake-buckets.txt`.  The remaining failures in that lane were
LinkWhatYouUse (4), LINK_LANGUAGE/LINK_LANG_AND_ID dependency edges (8),
add_subdirectory exclusion/progress (3), file-glob rerun (2), CMP0113 and
include-regex (2), plus isolated RuntimePath, CMP0156, CMP0181,
dollar-option, custom-command comment, project/language, include-directory,
BuiltinTargets, ExcludeFromAll and Instrumentation cases.

Unix Makefiles autogen utility edges now follow the upstream split between
`AddUtility` and codegen dependencies.  Qt-created `_autogen` and `_arcc`
helper targets remain utility dependencies rather than unconditional codegen
dependencies, while `_autogen_timestamp_deps` is still reachable through
custom-command dependency tracing.  Focused projdiff evidence:
`/tmp/umf7-autogen-projdiff.log` passes `autogen/imported` and
`autogen/warn` (2/2).

Makefile regeneration commands now carry the configure-time
`--compile-no-warning-as-error` and `--link-no-warning-as-error` options in
the upstream positions for top-level and directory-local
`cmake_check_build_system`/`depend` rules.

Unix Makefiles link generation now appends the upstream LWYU checker command
to executable/shared/module `link.txt` files when `LINK_WHAT_YOU_USE`, the
language flag, and `CMAKE_LINK_WHAT_YOU_USE_CHECK` are enabled.  The checker
is a separate `cmake -E __run_co_compile --lwyu=... --source=<target>` line
after the real link rule and receives the link launcher prefix.  Target
dependency discovery also evaluates `$<LINK_LANGUAGE>` and
`$<LINK_LANG_AND_ID>` with the owning target's actual linker language even
when the language-sensitive expression occurs only in a transitive interface
entry.  Focused post-fix RunCMake evidence:
`/tmp/umf7-focused-probes.log` passes **76/76** across LinkWhatYouUse,
CompileWarningAsError, both GenEx LINK_LANGUAGE/LINK_LANG_AND_ID suites, and
both target_link_libraries LINK_LANGUAGE/LINK_LANG_AND_ID suites
(report `/tmp/umf7-probe-out/report.json`).

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


Linked-target custom command events now follow the executable/library target
generator ordering: PRE_BUILD and PRE_LINK commands are inserted after the
link echo and before the archive/link command, while POST_BUILD commands run
after any executable/library symlink step.  This activates GoogleTest
POST_BUILD discovery in the Unix Makefiles generator and creates the
byproducts of target build-event commands.  Focused RunCMake evidence:
`/tmp/umf6-runcmake-gtestprobe/report.json` passes 95/95 (driver log
`/tmp/umf6-runcmake-gtestprobe-driver.log`), and
`/tmp/umf6-runcmake-byprodprobe/report.json` passes 48/48 (driver log
`/tmp/umf6-runcmake-byprodprobe-driver.log`).


Makefile2 target completion echo now honors the global `TARGET_MESSAGES`
property initialized from `CMAKE_TARGET_MESSAGES`, matching upstream
`cmGlobalUnixMakefileGenerator3::WriteConvenienceRules`.  Focused RunCMake
evidence: `/tmp/umf6-runcmake-makeprobe/report.json` passes 26/28 (driver
log `/tmp/umf6-runcmake-makeprobe-driver.log`); the remaining Make failures
are the pre-existing CMP0113 and include-regex cases, not target messages.


Instrumentation launcher metadata now matches upstream compile/link rule
variables: compile rules supply `<CONFIG>`, and target labels use upstream's
comma-separated `GetTargetLabelsString` encoding in both Makefile emission
and the shared compile-rule classifier.  Focused RunCMake evidence:
`/tmp/umf6-runcmake-instrprobe/report.json` passes 49/50 (driver log
`/tmp/umf6-runcmake-instrprobe-driver.log`); the only non-pass is the
pre-existing `cmake-command-make-program-verify` not-reached cascade.


Generator-capability and generated-file parity improved in several upstream-aligned
paths.  Build-database merge utility targets are no longer created by Unix
Makefiles (matching `SupportsBuildDatabase`), CMP0155 implicit module scanning
now requires generator module support, and `TargetDirectories.txt` uses the
configure-target/global-target/generator-added-target ordering shared with
Ninja.  The selected Unix Makefiles projdiff probe
`/tmp/umf6-auto-mod-projdiff.log` now passes `cxxmodules/bdb_single`;
`autogen/imported` and `autogen/warn` are reduced to a single residual
Makefile2 dependency-edge difference.  Makefile compile `-MT` values now
compose makefile-target escaping inside shell escaping exactly as upstream
(manual evidence `/tmp/umf6-mtcheck/build/With Space/CMakeFiles/main2.dir/build.make`
contains `"With\\ Space/..."`), and Makefile2 directory comments use kwsys
space-only output-path escaping.  Build-event custom-command comments are
emitted in blue before their commands, and static-library pre-link events
follow the upstream clean-then-event ordering.

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
