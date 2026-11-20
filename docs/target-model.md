# Configure-time build-system model

This guide describes the state that configure leaves for the generator and for the property commands (`get_property`, `set_property`, `get_target_property`, and so on). It covers targets, source files, custom commands, tests, and directory usage requirements. The model ports `cmTarget`, `cmSourceFile`/`cmSourceFileLocation`, `cmCustomCommand`, and `cmTest`, plus the `cmMakefile` methods that create them.

| File | Contents |
|---|---|
| `src/state.rs` | Data types: `Target`, `CustomCommand`, `GeneratorAction`, `SourceFile`, `Test`, `FileSet`, and the `Ext` fields listed below |
| `src/target.rs` (`commands::target`) | The cmTarget constructor (`create_target`), target property get/set, source-file lookup, and name checks |
| `src/commands/targets.rs` | `add_executable`, `add_library`, `add_custom_target`, `add_custom_command`, `add_dependencies`, `target_*`, `enable_testing`, `add_test`, and the script-mode rejection list |
| `src/commands/directory.rs` | `add_subdirectory`, `subdirs`, `include_directories`, `add_definitions`, `remove_definitions`, `add_compile_definitions`, `add_compile_options`, `add_link_options`, `link_directories`, `link_libraries`, `source_group` |
| `src/commands/custom_actions.rs` | Generate-time custom-command actions (`cmMakefile::AddGeneratorAction` and the `detail::` helpers in cmLocalGenerator.cxx) |

## Targets

Targets live in `interp.ext.targets: HashMap<key, Target>`. `interp.ext.target_order` lists the keys in creation order (`cmMakefile::OrderedTargets` across directories). Non-imported targets are also listed per directory in `interp.makefiles[dir].targets`.

- **Keys.** The key is the target name for normal targets and for `IMPORTED GLOBAL` targets. Two kinds of target are directory-scoped and use the key `[<dir>]<name>`: non-global IMPORTED targets, and duplicates allowed by `ALLOW_DUPLICATE_CUSTOM_TARGETS`. Their lookup goes through `ext.scoped_targets[(dir, name)]`, which subdirectories inherit when they are created. Always read `Target::name` for the real name, never the key.
- **Lookup.** Resolve names with `state::find_target_key(interp, name)` or `find_target_key_at(interp, dir, name)`, which follow `cmMakefile::FindTargetToUse`. `find_target_entry` returns `(key, &Target)` and `find_target_to_use` returns `&Target`.
- **Aliases.** Global aliases are in `ext.aliases` (alias name → key). An alias of a non-global imported target is local to its directory and lives in `ext.scoped_aliases[(dir, alias)]`. `find_alias` returns `(key, is_global)`.
- **Construction.** `create_target` runs the cmTarget constructor. That covers the full `CMAKE_<PROP>` initializer table (including per-config entries, keyed on `CMAKE_BUILD_TYPE` or `CMAKE_CONFIGURATION_TYPES`), the `ENABLE_EXPORTS` and `POSITION_INDEPENDENT_CODE` special cases, `SYSTEM`, and the `INCLUDE_DIRECTORIES`/`COMPILE_OPTIONS`/`LINK_OPTIONS`/`LINK_DIRECTORIES` snapshot from the directory. To store a hand-built `Target::new(..)`, call `register_target`.
- **Properties.**
  - Raw values are in `Target::props`.
  - Every list-valued usage requirement carries one `(value, backtrace)` entry per command call in `Target::provenance[PROP]`. These include `INCLUDE_DIRECTORIES`, `COMPILE_*`, `LINK_*`, `SOURCES`, `PRECOMPILE_HEADERS`, `INTERFACE_*`, and `LINK_LIBRARIES`. Use them for `--explain` and for genex backtraces.
  - System include directories are in `Target::system_include_directories`. Prefer `target_property` over raw props, because it also computes `NAME`, `TYPE`, `IMPORTED*`, `ALIASED_TARGET`/`ALIAS_GLOBAL`, `SOURCES` (resolved locations), `LOCATION*`, `HEADER_SET*`/`HEADER_DIRS*`, and `CXX_MODULE_SET*`.
- **Property commands.** `read_target_property` is `get_target_property`/`get_property` and issues the LOCATION and file-set diagnostics. `write_target_property(.., PropertyWrite::{Set, Append, AppendString})` is `set_property`/`set_target_properties`, with the read-only and imported-target rules and the `IMPORTED_GLOBAL` promotion.
- **Link libraries.** `LINK_LIBRARIES` and `INTERFACE_LINK_LIBRARIES` are stored exactly as CMake stores them. That means `debug`/`optimized` become `$<$<CONFIG:..>:..>`, PRIVATE dependencies of static libraries are wrapped in `$<LINK_ONLY:..>`, and `$<TARGET_NAME:..>` is used for known targets.
  - Calls on a target from another directory (CMP0079) are bracketed by the markers `::@(0x<dir>)` and `::@`, where `<dir>` is the caller's makefile index in hex. Items between the markers resolve in that directory. `targets::link_lookup_directory(marker)` decodes a marker.
- **Dependencies.** `Target::dependencies` holds `add_dependencies` (name → call site). `MANUALLY_ADDED_DEPENDENCIES` reads them back sorted.
- **File sets.** `Target::file_sets[name]` is a `FileSet { kind, scope, base_dirs, files }`, each entry carrying a backtrace.

## Source files

`interp.ext.sources[dir]` is the per-directory `cmMakefile::SourceFiles` list. Each `SourceFile` holds a `SourceFileLocation` (directory, name, and whether either is ambiguous), its props, `generated`, and `custom_command`. Lookups use `cmSourceFileLocation::Matches` semantics:

- `find_source(interp, dir, path)` and `find_source_index` correspond to `cmMakefile::GetSource`.
- `find_known_source` is the same lookup with `cmSourceFileLocationKind::Known`.
- `get_or_create_source` and `source_index(.., known)` correspond to `GetOrCreateSource`.
- `source_prop(interp, dir, path, name) -> Option<&str>` returns a source's stored property, for `get_source_file_property` and the generator.
- `mark_generated` corresponds to `GetOrCreateGeneratedSource` + `MarkAsGenerated`. Full paths are also recorded in `ext.generated_sources` for CMP0163/CMP0118.
- `resolve_source(interp, dir, path)` is `cmSourceFile::ResolveFullPath`. It returns the full path, the language, the generated and external-object flags, and the CMP0115 warning, or the "Cannot find source file" error text.

## Custom commands

`add_custom_command` and `add_custom_target` validate their arguments at configure time, mark outputs and byproducts GENERATED, and queue a `GeneratorAction` (Output, Append, Target, or Utility) in `ext.generator_actions`. This matches CMake 4.1, where commands are attached only when generation starts. Errors such as "Attempt to add a custom rule to output … which already has a custom rule", "Attempt to APPEND to custom command with output …", and "OUTPUT containing a "<" is not allowed." are therefore reported during the generate step. After them, the CLI prints "-- Generating done" followed by "CMake Generate step failed".

**The generator must call `custom_actions::run_generator_actions(interp, &mut expand)` once, before computing anything.** `generator::generate` already does this. `expand` is `cmLocalGenerator::ExpandCustomCommandOutputGenex`: it evaluates a genex output for every configuration and returns full paths. Until the genex engine is wired in, `generate` passes `unsupported_output_genex`, which reports genex outputs as unsupported.

After the actions have run:

- **OUTPUT commands.** These are on `ext.sources[dir][i].custom_command`. The source is the MAIN_DEPENDENCY when one was given and not already taken by a different command. Otherwise it is `<first output>.rule`, which carries `__CMAKE_RULE=1`. `command.depends[0]` is the main dependency when one exists.
- **TARGET commands.** These are on `Target::custom_commands` in call order, with `kind` set to `PreBuild`, `PreLink`, or `PostBuild` and `target` set.
- **Utility targets** (`add_custom_target` with COMMAND or DEPENDS). The command is attached to `<bin>/CMakeFiles/<name>.rule`, and its output is the SYMBOLIC source `<bin>/CMakeFiles/<name>`, which is appended to the target's `SOURCES`. `custom_actions::utility_command(interp, key)` returns the command. The utility's comment defaults to `Some("")`.
- **Output map.** `ext.output_to_source[dir]` (`cmLocalGenerator::OutputToSource`) maps every concrete output and byproduct to its producer. Use `custom_actions::source_with_output(interp, dir, path, include_byproducts)` to query it.
- **Command fields.** Every attached command has `escape_allow_make_vars` set. Its `command_lines` are argv vectors, and its other fields are unevaluated (genex intact). Policies (`policies`) and the call-site `backtrace` are recorded at configure time.

## Tests

`ext.tests` holds the tests in call order. An old-style `add_test(name cmd...)` replaces an earlier old-style test with the same name, and `Test::old_style` records which signature was used. Each `Test` carries `command`, `configurations`, `command_expand_lists`, `props` (WORKING_DIRECTORY and others), and the CMP0178/CMP0158 settings in effect.

`enable_testing()` sets `CMAKE_TESTING_ENABLED`. At the end of each directory, `ext.testing_enabled` records which directories end up with testing on, which tells the generator whether to write `CTestTestfile.cmake` there.

## Directories

- **Usage requirements.** Directory `INCLUDE_DIRECTORIES`, `COMPILE_OPTIONS`, `COMPILE_DEFINITIONS`, `LINK_OPTIONS`, and `LINK_DIRECTORIES` keep per-entry backtraces in `ext.directory_provenance[dir]`, read through `directory_entries`. System includes are in `ext.directory_system_includes[dir]`.
- **Define flags.** `add_definitions` and `remove_definitions` keep the `cmMakefile::DefineFlags` string in `ext.directory_define_flags[dir]`. That string is what feeds `CMAKE_<LANG>_FLAGS`-style compile lines. Entries that look like `-D` definitions also go into `COMPILE_DEFINITIONS`.
- **Subdirectories.**
  - `add_subdirectory` reports its binary-directory errors, inherits directory properties, `SYSTEM`, and the scoped imported targets, and runs the child immediately.
  - `subdirs()` entries are queued in `ext.pending_subdirs` and configured when their parent directory finishes (`directory::finish_directory`).
- **Source groups.** These are stored in `ext.source_groups[dir]`.

## Script mode

`targets::register_script_mode_unexpected` ports `GetProjectCommandsInScriptMode`. In `-P` mode, every listed command fails with "command is not scriptable" before its arguments are expanded. It is registered last in `commands::register_all` so that it overrides every family.

## Verification

- Run `python3 scripts/difftest.py tests/scripts/targets` to check the script-mode errors against the oracle.
- `tests/projects/targets/<case>/` contains a `project(x NONE)` project together with the oracle's `expected-configure.{stderr,exit}`. Regenerate those files with `scripts/capture_target_cases.py`.
- The unit test `commands::targets::project_cases::oracle_cases` runs each case in-process: the `project()` line is blanked, and minimal stand-ins for the property query commands call the public API above. It then runs the generate-time actions and compares stderr with the oracle's, ignoring line wrapping. Once `project()` system detection and the property commands are on master, the same cases also run end to end with `scripts/projdiff.py`.
