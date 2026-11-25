# Unix Makefiles port status

Branch: `t3/unix-makefiles-4`

Most recent completed full Unix Makefiles lane after merging `main` into Batch 6: **108/161** (`/tmp/pd-umf-batch7-baseline.log`).  The pre-Batch-5 baseline was 103/161 (`/tmp/pd-umf-baseline.log`).  Ninja lane must remain 161/161.

Known remaining projdiff failure groups: autogen, CUDA, C++ modules,
install/platform variants, ASM dependency scanning, object-library corners,
PCH/unity, Fortran modules, response files, Makefile2/progress/DependInfo
ordering, and try_compile nested logs.  `link/items` still has target-specific
link model differences.  `install/subdir_dest` is down to object-file `./`
normalization in `sub/cmake_install.cmake`.

Evidence still pending: 161/161 Unix Makefiles lane, zlib byte parity/build,
Kitware CMake self-configure/build, RunCMake lane, and CLI regression additions.

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
