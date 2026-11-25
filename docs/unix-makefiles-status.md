# Unix Makefiles port status

Branch: `t3/unix-makefiles-2`

Current projdiff Unix Makefiles lane: **93/160** at the start of this work batch
(`/tmp/projdiff-umf-current.log`).  The working tree additionally fixes
`basic/c_libs`, `export/build_export`, `export/compatible_transitive`,
`targets/custom`, and one local-Makefile header regression; a fresh full lane
has not yet been run after those fixes.

Implemented/fixed in the current uncommitted batch:

- Target and directory `ADDITIONAL_CLEAN_FILES`, custom outputs/byproducts in
  target clean scripts, directory clean scripts, utility-target cleaning, and
  object-library artifact exclusions.
- Custom command rules discovered from generated sources: dependencies,
  multi-output touch rules, depfile timestamp dependencies, `CODEGEN` driver
  dependencies, utility target dependencies/commands, jobserver `+` prefixes,
  working-directory `cd` prefixes, and custom DependInfo entries.
- Unix Makefiles linker depfile placement and shared-library/executable link
  flag ordering.
- Versioned import-library names in clean files.

Known remaining projdiff failure groups: autogen, CUDA, C++ modules,
install/platform variants, ASM dependency scanning, object-library corners,
PCH/unity, Fortran modules, response files, Makefile2/progress/DependInfo
ordering, and try_compile nested logs.  `link/items` still has target-specific
link model differences.  `install/subdir_dest` is down to object-file `./`
normalization in `sub/cmake_install.cmake`.

Evidence still pending: fresh full Unix Makefiles lane after this batch, zlib
byte parity/build, Kitware CMake self-configure/build, RunCMake lane, and CLI
regression additions.

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
