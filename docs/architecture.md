# cmake-rust architecture and contributor guide

cmake-rust is an independent Rust implementation of the `cmake` executable that targets **exact behavioral compatibility with Kitware CMake 4.1.0** (commit `2a2c2e0b2658f7c07e00fcf649882fd5578c2fc1`). The C++ sources of that release are the specification. Port their semantics faithfully, including error texts, edge cases, and policy behavior. CMake's own `Modules/` and `Templates/` directories are *data*. They play the role TypeScript's `lib.d.ts` plays for a compiler. We execute them and never reimplement them natively.

- Specification (read-only): `/tmp/cmake-rust-validation.hSLgEZHR/kitware/Source/*.cxx`
- Oracle binary: `/tmp/cmake-rust-validation.hSLgEZHR/kitware-reference/bin/cmake` (4.1.0)
- Data root: set `CMAKE_RUST_DATA_ROOT=/tmp/cmake-rust-validation.hSLgEZHR/kitware`, or stage `<prefix>/share/cmake-4.1`.

## Hard rules

1. **No delegation to Kitware cmake.** The candidate binary never executes another `cmake`.
2. **No fixture-specific hacks.** Fake defaults, stubbed outcomes, and special cases keyed on project names are all forbidden. If something is unsupported, fail with a CMake-style error at the right file and line.
3. **Port, don't guess.** Read the corresponding `cm*.cxx` file before you implement a command. Keep messages byte-identical, since the upstream RunCMake tests match stderr with regexes.
4. **Prove it differentially.** Add `tests/scripts/<area>/*.cmake` cases. Run `scripts/difftest.py` to compare exit code, stdout and stderr against the oracle. Run the relevant upstream suites with `scripts/conformance/run.py --suite RunCMake.<name>` (see `docs/conformance.md`).

## Layout

| File | Role |
|---|---|
| `src/lexer.rs` | List-file lexer and parser (port of `cmListFileLexer.in.l` / `cmListFileCache.cxx`) |
| `src/interp.rs` | Interpreter core: scopes, policies, directories, control flow, variable expansion, diagnostics |
| `src/condition.rs` | `if()` / `while()` conditions (`cmConditionEvaluator`) |
| `src/flow.rs` | `foreach` / `block` argument handling |
| `src/cache.rs` | `CMakeCache.txt` model and file format |
| `src/policy.rs`, `src/policy_table.rs` | Policies (table generated from `cmPolicies.h` by `scripts/gen/policies.py`) |
| `src/messages.rs` | `cmMessenger` diagnostics and the `cmDocumentationFormatter` text wrapper |
| `src/props.rs` | Generic property maps |
| `src/state.rs` | Targets, tests, and shared extension state (`interp.ext`) |
| `src/util.rs` | `cmList` expansion, `IsOn`/`IsOff`, C number parsing, version compare, path helpers |
| `src/cmregex.rs` | Port of kwsys `RegularExpression` (the CMake regex dialect) |
| `src/commands/<family>.rs` | Builtin commands; each module has `pub fn register(interp: &mut Interp)` |
| `src/commands/mod.rs` | `register_all`, which calls each family's `register` |

## Writing a command

```rust
use crate::interp::{CmdErr, CmdResult, Interp, err};

pub fn register(interp: &mut Interp) {
    interp.register("string", string_command);
}

fn string_command(interp: &mut Interp, args: &[String]) -> CmdResult {
    // `args` are already expanded (cmMakefile::ExpandArguments).
    if args.is_empty() {
        return err("must be called with at least one argument."); // == status.SetError(...)
    }
    interp.set_def("OUT", "value");   // AddDefinition
    Ok(())
}
```

- `err(msg)` reproduces `status.SetError(msg); return false;`. The core prints `CMake Error at file:line (cmd):\n  <cmd> <msg>`.
- `Err(CmdErr::Reported)` means you already issued a message and the command failed (returned false).
- To issue a diagnostic without failing, call `interp.issue(MessageType::..., text)` (`cmMakefile::IssueMessage`). Call `interp.set_fatal()` for `cmSystemTools::SetFatalErrorOccurred()`.
- Variables:
  - `get_def` (normal variable, else cache), `get_normal_def`, `set_def`, `unset_def`.
  - `raise_scope(name, Option<&str>)` for `PARENT_SCOPE`.
  - `is_on`, `is_set`, `is_def_set`.
- Cache: `interp.cache`, plus `interp.add_cache_def(name, value, help, CacheType, force)` (`cmMakefile::AddCacheDefinition`).
- Policies: `interp.policy_status("CMP0xxx") -> PolicyStatus::{Old, Warn, New}`. For a WARN message, use `crate::policy::policy_warning(index)`.
- Directories:
  - `interp.current_source_dir()`, `current_binary_dir()`, `current_list_dir()`.
  - `interp.mf()` / `mf_mut()` for the current `Makefile`, which holds directory properties.
- Calling back into the language:
  - `interp.read_dependent_file(path, no_policy_scope)` for include.
  - `interp.run_string(code, virtual_file)` for `cmake_language(EVAL)`.
  - `interp.call_command(name, args)`.
- Matches: `crate::state::{clear_matches, store_matches}`. Lists: `crate::util::{expand_list, join_list}`.
- Global state for your family goes in `interp.ext`. Add a field in `src/state.rs`, and keep that diff minimal because it's a merge hotspot.

## Workflow for parallel workers

- Each worker owns specific files. Do not edit files you don't own, except for these one-line integrations:
  - `pub mod <family>;` and `<family>::register(interp);` in `src/commands/mod.rs`
  - a field in `state::Ext`
  - small exposures in `interp.rs`. Describe these in your final message.
- Build with `cargo build --release -j 6` (the host is shared). Run `cargo test --release`.
- Commit in your worktree, ending the message with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.
- The lead integrates and verifies everything.
