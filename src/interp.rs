//! The CMake language interpreter core: variable scopes, policies,
//! directories, the command table, control flow, variable expansion, and
//! diagnostics. Ports `cmMakefile`, `cmStateSnapshot`/`cmDefinitions`,
//! `cmFunctionCommand`, `cmMacroCommand`, `cmIfCommand`, `cmForEachCommand`,
//! `cmWhileCommand`, `cmBlockCommand` and `cmReturnCommand` semantics.

use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;

use crate::cache::{Cache, CacheType};
use crate::lexer::{self, Arg, Command, Delim};
use crate::messages::{self, Frame, MessageType};
use crate::policy::{self, PolicyMap, PolicyStatus};
use crate::props::Props;
use crate::util;

pub const VERSION: (u32, u32, u32) = (4, 1, 0);

/// Parse-cache key: the path and the exact file bytes.  Size+mtime is not
/// enough: a file rewritten with same-length content within one filesystem
/// timestamp tick (coarse-grained on ext4) keeps its mtime.
type ParseKey = (String, Vec<u8>);

thread_local! {
    static PARSE_CACHE: std::cell::RefCell<HashMap<ParseKey, Rc<ParsedFile>>> =
        std::cell::RefCell::new(HashMap::new());
}
pub const VERSION_STRING: &str = "4.1.0";

/// Error returned by a builtin command.
#[derive(Debug)]
pub enum CmdErr {
    /// `status.SetError(msg)`: reported as `<command> <msg>`.
    Error(String),
    /// Already reported; the command failed.
    Reported,
}

impl From<String> for CmdErr {
    fn from(message: String) -> Self {
        CmdErr::Error(message)
    }
}

impl From<&str> for CmdErr {
    fn from(message: &str) -> Self {
        CmdErr::Error(message.to_owned())
    }
}

pub type CmdResult = Result<(), CmdErr>;
pub type Builtin = fn(&mut Interp, &[String]) -> CmdResult;
pub type RawBuiltin =
    fn(&mut Interp, &Rc<ParsedFile>, &Command, Option<&MacroSubst>) -> (bool, Ctl);

/// Shorthand for returning a `SetError` failure.
pub fn err<T>(message: impl Into<String>) -> Result<T, CmdErr> {
    Err(CmdErr::Error(message.into()))
}

/// A parsed list file with precomputed block structure.
#[derive(Debug)]
pub struct ParsedFile {
    pub path: Rc<str>,
    pub commands: Vec<Command>,
    /// For each command index: the indices of its block's separators and
    /// terminator (`elseif`/`else`/`endif` for `if`; the end for loops,
    /// function, macro and block). Empty for other commands.
    pub jumps: Vec<Vec<usize>>,
}

impl ParsedFile {
    pub fn new(path: Rc<str>, commands: Vec<Command>) -> ParsedFile {
        let mut jumps = vec![Vec::new(); commands.len()];
        let mut stack: Vec<usize> = Vec::new();
        for (index, command) in commands.iter().enumerate() {
            match command.lname.as_str() {
                "if" | "foreach" | "while" | "function" | "macro" | "block" => stack.push(index),
                "elseif" | "else" => {
                    if let Some(&open) = stack.last() {
                        jumps[open].push(index);
                    }
                }
                "endif" | "endforeach" | "endwhile" | "endfunction" | "endmacro" | "endblock" => {
                    if let Some(open) = stack.pop() {
                        jumps[open].push(index);
                    }
                }
                _ => {}
            }
        }
        ParsedFile {
            path,
            commands,
            jumps,
        }
    }
}

/// A user-defined function or macro.
#[derive(Debug)]
pub struct Callable {
    pub name: String,
    pub params: Vec<String>,
    pub file: Rc<ParsedFile>,
    pub start: usize,
    pub end: usize,
    pub line: usize,
    pub policies: PolicyMap,
    pub is_macro: bool,
}

#[derive(Clone)]
pub enum CommandDef {
    Raw(RawBuiltin),
    Builtin(Builtin),
    Unexpected(&'static str),
    /// Commands handled directly by the run loop (`if`, `foreach`, ...).
    Flow,
    User(Rc<Callable>),
}

/// Control-flow outcome of executing a command or block.
#[derive(Clone, Debug, PartialEq)]
pub enum Ctl {
    Normal,
    Break,
    Continue,
    Return(Option<Vec<String>>),
}

/// Text substitutions applied to macro-body arguments.
#[derive(Debug, Default)]
pub struct MacroSubst {
    pairs: Vec<(String, String)>,
    argv: Vec<(String, String)>,
}

impl MacroSubst {
    pub(crate) fn apply(&self, text: &str) -> String {
        let mut value = text.to_owned();
        for (from, to) in &self.pairs {
            if value.contains(from.as_str()) {
                value = value.replace(from.as_str(), to);
            }
        }
        if value.contains("${ARGV") {
            for (from, to) in &self.argv {
                if value.contains(from.as_str()) {
                    value = value.replace(from.as_str(), to);
                }
            }
        }
        value
    }
}

#[derive(Debug, Default)]
struct Scope {
    vars: HashMap<String, Option<String>>,
    /// Identity of the scope's cmDefinitions node, so a deferred `subdirs()`
    /// directory can see the node's final contents after it is popped.
    id: u64,
}

/// Function and block scopes that were active when deferred `subdirs()`
/// directories were added. Their cmDefinitions nodes stay alive in Kitware's
/// linked tree after the scope ends, and
/// cmStateSnapshot::InitializeFromParent_ForSubdirsCommand closes over them.
#[derive(Debug, Default)]
struct CapturedScopes {
    wanted: std::collections::HashSet<u64>,
    popped: HashMap<u64, HashMap<String, Option<String>>>,
    next_id: u64,
}

#[derive(Debug, Clone)]
pub struct PolicyFrame {
    pub map: PolicyMap,
    pub weak: bool,
}

/// RAII end of `cmMakefile::CallScope` for the debugger
/// (`cmDebuggerAdapter::OnEndFunctionCall`).
struct DebugFrameGuard {
    inner: std::sync::Arc<crate::debugger::Inner>,
}

impl Drop for DebugFrameGuard {
    fn drop(&mut self) {
        self.inner.end_function_call();
    }
}

/// A build-system directory (`cmMakefile`).
#[derive(Debug)]
pub struct Makefile {
    pub source_dir: String,
    pub binary_dir: String,
    pub parent: Option<usize>,
    pub props: Props,
    pub policies: Vec<PolicyFrame>,
    pub list_files: Vec<String>,
    /// `cmMakefile::OutputFiles` (`AddCMakeOutputFile`): configure_file and
    /// file(CONFIGURE) outputs, in call order.
    pub output_files: Vec<String>,
    pub targets: Vec<String>,
    pub children: Vec<usize>,
    pub exclude_from_all: bool,
    /// Deferred calls registered with cmake_language(DEFER).
    pub deferred: Vec<Deferred>,
    pub defer_counter: usize,
    /// `cmMakefile::Backtrace`: this directory's own diagnostic stack.  It
    /// is frozen (with the `add_subdirectory`/`include` frame on top) while
    /// a descendant directory configures, and otherwise holds the
    /// directory's permanent `CMakeLists.txt` entry frame.
    pub backtrace: Vec<Frame>,
}

#[derive(Debug, Clone)]
pub struct Deferred {
    pub id: String,
    pub file: Rc<str>,
    pub command: Command,
}

/// Working mode of the interpreter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Project,
    Script,
    /// `cmStateEnums::FIND_PACKAGE` (`cmake --find-package`).
    FindPackage,
}

/// `cmake::SetDebugOutputOn`, `SetDebugFindOutput`,
/// `SetDebugFindOutputPkgs`/`Vars` and `cmMakefile::DebugFindPkg`.
#[derive(Clone, Debug, Default)]
pub struct DebugOptions {
    pub output: bool,
    pub find: bool,
    pub find_pkgs: std::collections::BTreeSet<String>,
    pub find_vars: std::collections::BTreeSet<String>,
    /// `cmMakefile::DebugFindPkg`, set while find_package runs for a
    /// package named by --debug-find-pkg.
    pub find_pkg_mode: bool,
}

/// List-file tracing (`cmake::SetTrace`, `SetTraceExpand`,
/// `SetTraceFormat`, `AddTraceSource`, `SetTraceFile`).
pub struct TraceConfig {
    pub expand: bool,
    pub json: bool,
    pub sources: Vec<String>,
    pub file: Option<std::cell::RefCell<std::fs::File>>,
}

pub struct Interp {
    pub mode: Mode,
    pub cache: Cache,
    scopes: Vec<Scope>,
    captured_scopes: CapturedScopes,
    /// Scope-stack depth of each directory being configured (its own scope
    /// is the top one at that depth).
    directory_scope_depths: Vec<usize>,
    pub backtrace: Vec<Frame>,
    policy_barriers: Vec<(usize, usize)>,
    pub makefiles: Vec<Makefile>,
    pub mf_stack: Vec<usize>,
    pub commands: HashMap<String, CommandDef>,
    pub global_props: Props,
    pub fatal: bool,
    pub error: bool,
    pub exit_code: Option<i32>,
    pub cmake_root: String,
    pub cmake_command: String,
    pub home_source: String,
    pub project_file: String,
    pub home_binary: String,
    /// Loop nesting depth in the current function frame (for break/continue
    /// diagnostics).
    loop_depth: Vec<usize>,
    /// Native extension state owned by command modules.
    pub ext: crate::state::Ext,
    pub suppress_dev_warnings: bool,
    pub dev_warnings_as_errors: bool,
    pub warn_deprecated: bool,
    pub deprecated_as_errors: bool,
    recursion_depth: usize,
    /// Stack of macro substitution contexts; `None` entries mark function
    /// frames.
    pub stdout_log: Option<Vec<u8>>,
    /// When set, diagnostics and `message()` stderr lines are appended here
    /// instead of written to the process stderr (used by in-process tests).
    pub stderr_capture: Option<String>,
    pub sarif_log: crate::sarif::ResultsLog,
    pub instrument_build_hooks: bool,
    /// Set by builtins that end the current file/function (include_guard).
    pub request_return: bool,
    /// Configuring a try_compile project.
    pub in_try_compile: bool,
    /// `--debug-trycompile`: keep try_compile directories and log events.
    pub debug_trycompile: bool,
    /// `--debug-output` and the `--debug-find*` options.
    pub debug: DebugOptions,
    /// `cmake::DebugAdapter` (cmDebuggerAdapter): present when `--debugger`
    /// started the DAP session.
    pub debugger: Option<crate::debugger::Debugger>,
    /// `--compile-no-warning-as-error` / `--link-no-warning-as-error`.
    pub ignore_compile_warning_as_error: bool,
    pub ignore_link_warning_as_error: bool,
    /// Root CMakeLists.txt lacked cmake_minimum_required (CMP0000).
    pub root_check_cmp0000: bool,
    /// Language-enablement state (cmGlobalGenerator language maps).
    pub langs: crate::languages::LangState,
    /// Recursion depth inherited from an outer try_compile.
    pub recursion_depth_base: usize,
    /// Final variable bindings of each directory scope (index = makefile),
    /// captured when the directory finishes configuring, for generation.
    pub dir_final_vars: Vec<HashMap<String, Option<String>>>,
    /// The messenger's top source for path relativization; an inner
    /// try_compile project inherits the outer project's root because the
    /// `cmake` instance is shared (`cmMessenger::TopSource`).
    pub top_source_root: Option<String>,
    /// `--warn-uninitialized` (cmake::WarnUninitialized).
    pub warn_uninitialized: bool,
    /// `--check-system-vars` (cmake::CheckSystemVars).
    pub check_system_vars: bool,
    /// `cmake::WarnUnusedCli` (on unless --no-warn-unused-cli).
    pub warn_unused_cli: bool,
    /// `cmake::UsedCliVariables`: command-line cache variables watched for
    /// reads (`cmWarnUnusedCliWarning`).
    pub cli_vars: std::cell::RefCell<BTreeMap<String, bool>>,
    /// Tracing settings shared with try_compile instances
    /// (`cmake::SetTraceRedirect`).
    pub trace: Option<Rc<TraceConfig>>,
    /// `recursion_depth` when each directory's `cmMakefile` started
    /// executing, for the per-makefile `ExecutionStatusStack` size.
    trace_frame_base: Vec<usize>,
}

impl Interp {
    pub fn new(mode: Mode, home_source: &str, home_binary: &str, cmake_root: &str) -> Interp {
        let cmake_command = std::env::current_exe()
            .ok()
            .and_then(|path| path.canonicalize().ok())
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_else(|| "cmake".to_owned());
        let mut interp = Interp {
            mode,
            cache: Cache::default(),
            scopes: vec![Scope::default()],
            captured_scopes: CapturedScopes {
                next_id: 1,
                ..CapturedScopes::default()
            },
            directory_scope_depths: Vec::new(),
            backtrace: Vec::new(),
            policy_barriers: Vec::new(),
            makefiles: Vec::new(),
            mf_stack: Vec::new(),
            commands: HashMap::new(),
            global_props: Props::default(),
            fatal: false,
            error: false,
            exit_code: None,
            cmake_root: cmake_root.to_owned(),
            cmake_command,
            home_source: home_source.to_owned(),
            project_file: "CMakeLists.txt".to_owned(),
            home_binary: home_binary.to_owned(),
            loop_depth: vec![0],
            ext: crate::state::Ext::default(),
            suppress_dev_warnings: false,
            dev_warnings_as_errors: false,
            warn_deprecated: true,
            deprecated_as_errors: false,
            recursion_depth: 0,
            stdout_log: None,
            stderr_capture: None,
            sarif_log: crate::sarif::ResultsLog::default(),
            instrument_build_hooks: false,
            request_return: false,
            in_try_compile: false,
            debug_trycompile: false,
            debug: DebugOptions::default(),
            debugger: None,
            ignore_compile_warning_as_error: false,
            ignore_link_warning_as_error: false,
            root_check_cmp0000: false,
            langs: crate::languages::LangState::default(),
            recursion_depth_base: 0,
            dir_final_vars: Vec::new(),
            top_source_root: None,
            warn_uninitialized: false,
            check_system_vars: false,
            warn_unused_cli: false,
            cli_vars: std::cell::RefCell::new(BTreeMap::new()),
            trace: None,
            trace_frame_base: vec![0],
        };
        interp.makefiles.push(Makefile {
            source_dir: home_source.to_owned(),
            binary_dir: home_binary.to_owned(),
            parent: None,
            props: Props::default(),
            policies: vec![PolicyFrame {
                map: PolicyMap::default(),
                weak: false,
            }],
            list_files: Vec::new(),
            output_files: Vec::new(),
            targets: Vec::new(),
            children: Vec::new(),
            exclude_from_all: false,
            deferred: Vec::new(),
            defer_counter: 0,
            backtrace: vec![Frame {
                file: Rc::from(format!("{home_source}/CMakeLists.txt").as_str()),
                line: 0,
                name: None,
                deferred: false,
            }],
        });
        interp.mf_stack.push(0);
        crate::commands::register_all(&mut interp);
        interp
    }

    // ------------------------------------------------------------------
    // Variables

    /// Looks up a normal binding or a cache value without running callbacks.
    /// Use read_definition for language reads that dispatch variable watches.
    pub fn get_def(&self, name: &str) -> Option<&str> {
        self.mark_cli_used(name);
        for scope in self.scopes.iter().rev() {
            if let Some(entry) = scope.vars.get(name) {
                return match entry {
                    Some(value) => Some(value.as_str()),
                    None => self.cache.value(name),
                };
            }
        }
        self.cache.value(name)
    }

    /// Normal-variable lookup only (no cache fallback).
    pub fn get_normal_def(&self, name: &str) -> Option<&str> {
        for scope in self.scopes.iter().rev() {
            if let Some(entry) = scope.vars.get(name) {
                return entry.as_deref();
            }
        }
        None
    }

    pub fn get_safe(&self, name: &str) -> String {
        self.get_def(name).unwrap_or_default().to_owned()
    }

    pub fn is_def_set(&self, name: &str) -> bool {
        self.get_def(name).is_some()
    }

    /// `cmMakefile::IsOn`.
    pub fn is_on(&self, name: &str) -> bool {
        util::is_on_opt(self.get_def(name))
    }

    /// `cmMakefile::IsSet`: defined and not empty? (cmMakefile::IsSet: defined
    /// and not NOTFOUND-ish empty). Mirrors: def && !def->empty() && !IsNOTFOUND.
    pub fn is_set(&self, name: &str) -> bool {
        match self.get_def(name) {
            Some(value) => !value.is_empty() && !util::is_notfound(value),
            None => false,
        }
    }

    pub fn set_def(&mut self, name: impl Into<String>, value: impl Into<String>) {
        let name = name.into();
        let value = value.into();
        let watched = self.ext.variable_watches.contains_key(&name);
        let notice = watched.then(|| (name.clone(), value.clone()));
        let top = self.scopes.last_mut().expect("scope");
        top.vars.insert(name, Some(value));
        if let Some((name, value)) = notice {
            crate::commands::sysinfo::notify_variable(self, &name, "MODIFIED_ACCESS", &value);
        }
    }

    pub fn unset_def(&mut self, name: &str) {
        let top = self.scopes.last_mut().expect("scope");
        top.vars.insert(name.to_owned(), None);
        crate::commands::sysinfo::notify_variable(self, name, "REMOVED_ACCESS", "");
    }

    /// `cmMakefile::PolicyOptionalWarningEnabled` for the value of a
    /// `CMAKE_POLICY_WARNING_CMP<NNNN>` variable: unset means enabled under
    /// --debug-output, --trace or --trace-expand.
    pub fn policy_optional_warning(&self, value: Option<&str>) -> bool {
        match value {
            Some(value) => util::is_on(value),
            // Enable optional policy warnings with --debug-output, --trace,
            // or --trace-expand.
            None => self.debug.output || self.trace.is_some(),
        }
    }

    /// `cmFindCommon::ComputeIfDebugModeWanted(var)`.
    pub fn find_debug_wanted(&self, var: Option<&str>) -> bool {
        self.debug.find_pkg_mode
            || self.is_on("CMAKE_FIND_DEBUG_MODE")
            || self.debug.find
            || var.is_some_and(|var| self.debug.find_vars.contains(var))
    }

    /// `cmakemainGetStack`: with --debug-output, the call stack of the
    /// current makefile (`cmMakefile::FormatListFileStack`), if any.
    pub fn debug_stack(&self) -> String {
        if !self.debug.output {
            return String::new();
        }
        let Some(&base) = self.ext.execution_file_bases.last() else {
            return String::new();
        };
        let files = &self.ext.execution_files[base.min(self.ext.execution_files.len())..];
        if files.is_empty() {
            return String::new();
        }
        let count = files.len();
        let lines: Vec<String> = files
            .iter()
            .enumerate()
            .map(|(index, file)| format!("[{}]\t{file}", count - index))
            .collect();
        format!("\n   Called from: {}", lines.join("\n                "))
    }

    /// `cmSystemTools::Message` through cmakemainMessageCallback: the text
    /// and the --debug-output call stack on stderr.
    pub fn system_message(&mut self, text: &str) {
        let stack = self.debug_stack();
        self.stderr_attributed(crate::stdio::TermAttr::Normal.bit(), text, &stack);
    }

    /// The variable watch `cmWarnUnusedCliWarning` (`cmake::MarkCliAsUsed`).
    pub fn mark_cli_used(&self, name: &str) {
        let watched = {
            let vars = self.cli_vars.borrow();
            !vars.is_empty() && vars.get(name) == Some(&false)
        };
        if watched {
            self.cli_vars.borrow_mut().insert(name.to_owned(), true);
        }
    }

    /// `cmMakefile::MaybeWarnUninitialized`.
    fn maybe_warn_uninitialized(&mut self, variable: &str, source_file: Option<&str>) {
        if !self.warn_uninitialized {
            return;
        }
        // cmMakefile::VariableInitialized: any binding, including unset().
        if self
            .scopes
            .iter()
            .any(|scope| scope.vars.contains_key(variable))
        {
            return;
        }
        // cmMakefile::IsProjectFile
        // (The C++ tests the literal "/CMakeFiles", not the binary dir's.)
        let is_project_file = |file: &str| {
            util::is_subdirectory(file, &self.home_source)
                || (util::is_subdirectory(file, &self.home_binary)
                    && !util::is_subdirectory(file, "/CMakeFiles"))
        };
        if self.check_system_vars || source_file.is_some_and(is_project_file) {
            self.issue(
                MessageType::AuthorWarning,
                &format!("uninitialized variable '{variable}'"),
            );
        }
    }

    /// Reads an expanded variable and invokes registered watch callbacks.
    /// Read the binding again after the callback, since it can change scope state.
    pub fn read_definition(&mut self, name: &str) -> Option<String> {
        let value = self.get_def(name).map(str::to_owned);
        if !self.ext.variable_watches.contains_key(name) {
            return value;
        }
        let access = if value.is_some() {
            "READ_ACCESS"
        } else {
            "UNKNOWN_READ_ACCESS"
        };
        crate::commands::sysinfo::notify_variable(
            self,
            name,
            access,
            value.as_deref().unwrap_or_default(),
        );
        self.get_def(name).map(str::to_owned)
    }

    /// Visible normal binding (Some(None) = explicitly unset tombstone).
    fn visible(&self, name: &str, below: usize) -> Option<String> {
        for scope in self.scopes[..below].iter().rev() {
            if let Some(entry) = scope.vars.get(name) {
                return entry.clone();
            }
        }
        None
    }

    /// `cmMakefile::RaiseScope` (set or unset in the parent scope).
    pub fn raise_scope(&mut self, name: &str, value: Option<&str>) {
        let depth = self.scopes.len();
        if depth < 2 {
            let message = format!("Cannot set \"{name}\": current scope has no parent.");
            self.issue(MessageType::AuthorWarning, &message);
            return;
        }
        // Localize the current binding first.
        if !self.scopes[depth - 1].vars.contains_key(name) {
            let current = self.visible(name, depth - 1);
            self.scopes[depth - 1].vars.insert(name.to_owned(), current);
        }
        let parent = &mut self.scopes[depth - 2];
        parent
            .vars
            .insert(name.to_owned(), value.map(str::to_owned));
        crate::commands::sysinfo::notify_variable(
            self,
            name,
            "MODIFIED_ACCESS",
            value.unwrap_or_default(),
        );
    }

    /// Flattened view of all visible normal-variable bindings.
    pub fn flatten_scopes(&self) -> HashMap<String, Option<String>> {
        let mut out: HashMap<String, Option<String>> = HashMap::new();
        for scope in &self.scopes {
            for (name, value) in &scope.vars {
                out.insert(name.clone(), value.clone());
            }
        }
        out
    }

    /// Records the final variable state of the current directory.
    pub fn record_directory_vars(&mut self) {
        let index = self.mf_index();
        if self.dir_final_vars.len() <= index {
            self.dir_final_vars.resize(index + 1, HashMap::new());
        }
        self.dir_final_vars[index] = self.flatten_scopes();
    }

    pub fn push_scope(&mut self) {
        let id = self.captured_scopes.next_id;
        self.captured_scopes.next_id += 1;
        self.scopes.push(Scope {
            vars: HashMap::new(),
            id,
        });
    }

    pub fn pop_scope(&mut self) {
        if let Some(scope) = self.scopes.pop()
            && self.captured_scopes.wanted.remove(&scope.id)
        {
            self.captured_scopes.popped.insert(scope.id, scope.vars);
        }
    }

    /// The function/block scopes above the current directory's own scope,
    /// outermost first; they stay visible to a deferred `subdirs()`
    /// directory added now.
    pub fn capture_inner_scopes(&mut self) -> Vec<u64> {
        let base = self.directory_scope_depths.last().copied().unwrap_or(1);
        let ids: Vec<u64> = self.scopes[base.min(self.scopes.len())..]
            .iter()
            .map(|scope| scope.id)
            .collect();
        self.captured_scopes.wanted.extend(ids.iter().copied());
        ids
    }

    /// Overlay the final contents of captured scopes (outermost first) onto
    /// the current scope, as the closure in
    /// cmStateSnapshot::InitializeFromParent_ForSubdirsCommand sees them.
    fn apply_captured_scopes(&mut self, ids: &[u64]) {
        for id in ids {
            let vars = match self.captured_scopes.popped.get(id) {
                Some(vars) => vars.clone(),
                None => match self.scopes.iter().find(|scope| scope.id == *id) {
                    Some(scope) => scope.vars.clone(),
                    None => continue,
                },
            };
            if let Some(top) = self.scopes.last_mut() {
                top.vars.extend(vars);
            }
        }
    }

    pub fn scope_depth(&self) -> usize {
        self.scopes.len()
    }

    /// All variable names visible in the current scope (normal + cache),
    /// sorted (`cmMakefile::GetDefinitions`).
    pub fn definition_names(&self) -> Vec<String> {
        let mut seen: HashMap<&str, bool> = HashMap::new();
        for scope in self.scopes.iter().rev() {
            for (name, value) in &scope.vars {
                seen.entry(name.as_str()).or_insert(value.is_some());
            }
        }
        let mut names: Vec<String> = seen
            .into_iter()
            .filter(|(_, defined)| *defined)
            .map(|(name, _)| name.to_owned())
            .collect();
        names.extend(self.cache.entries.keys().cloned());
        names.sort();
        names
    }

    /// Names of normal variables visible in the current scope.
    pub fn normal_definition_names(&self) -> Vec<String> {
        let mut seen: HashMap<&str, bool> = HashMap::new();
        for scope in self.scopes.iter().rev() {
            for (name, value) in &scope.vars {
                seen.entry(name.as_str()).or_insert(value.is_some());
            }
        }
        let mut names: Vec<String> = seen
            .into_iter()
            .filter(|(_, defined)| *defined)
            .map(|(name, _)| name.to_owned())
            .collect();
        names.sort();
        names
    }

    /// `cmMakefile::AddCacheDefinition`.
    pub fn add_cache_def(
        &mut self,
        name: &str,
        value: Option<&str>,
        help: Option<&str>,
        kind: CacheType,
        force: bool,
    ) {
        let mut value = value.map(str::to_owned);
        let mut kind = kind;
        if let Some(existing) = self.cache.get(name)
            && existing.kind == CacheType::Uninitialized
        {
            // Existing UNINITIALIZED entry (from -D): keep its value unless
            // forced, and convert relative paths for PATH/FILEPATH.
            if !force {
                value = Some(existing.value.clone());
            }
            if matches!(kind, CacheType::Path | CacheType::Filepath)
                && let Some(current) = value.clone()
            {
                let items: Vec<String> = util::expand_list(&current, false)
                    .into_iter()
                    .map(|item| {
                        if util::is_off(&item) {
                            item
                        } else {
                            crate::pathresolver::to_normalized_path_on_disk(&item)
                        }
                    })
                    .collect();
                value = Some(util::join_list(&items));
            }
            if kind == CacheType::Uninitialized {
                kind = CacheType::String;
            }
        }
        self.cache.add(name, value.as_deref(), help, kind);
        match self.policy_status("CMP0126") {
            PolicyStatus::New => {}
            status => {
                if status == PolicyStatus::Warn
                    && self.policy_optional_warning(self.get_def("CMAKE_POLICY_WARNING_CMP0126"))
                    && self.get_normal_def(name).is_some()
                {
                    let text = format!(
                        "{}\nFor compatibility with older versions of CMake, normal variable \"{name}\" will be removed from the current scope.",
                        crate::policy::policy_warning(126)
                    );
                    self.issue(MessageType::AuthorWarning, &text);
                }
                self.unset_def(name);
            }
        }
    }

    // ------------------------------------------------------------------
    // Directories

    pub fn mf_index(&self) -> usize {
        *self.mf_stack.last().expect("makefile")
    }

    pub fn mf(&self) -> &Makefile {
        &self.makefiles[self.mf_index()]
    }

    pub fn mf_mut(&mut self) -> &mut Makefile {
        let index = self.mf_index();
        &mut self.makefiles[index]
    }

    pub fn current_source_dir(&self) -> String {
        self.mf().source_dir.clone()
    }

    pub fn current_binary_dir(&self) -> String {
        self.mf().binary_dir.clone()
    }

    /// The directory of the list file currently executing.
    pub fn current_list_dir(&self) -> String {
        self.get_def("CMAKE_CURRENT_LIST_DIR")
            .map(str::to_owned)
            .unwrap_or_else(|| self.current_source_dir())
    }

    /// Creates a child directory and runs its CMakeLists.txt
    /// (`add_subdirectory`).
    pub fn run_subdirectory(&mut self, source: &str, binary: &str, exclude_from_all: bool) -> bool {
        self.run_subdirectory_with_system(source, binary, exclude_from_all, false)
    }

    pub fn run_subdirectory_with_system(
        &mut self,
        source: &str,
        binary: &str,
        exclude_from_all: bool,
        system: bool,
    ) -> bool {
        let index = self.create_subdirectory(source, binary, exclude_from_all);
        self.configure_subdirectory(index, system)
    }

    /// `cmMakefile::AddSubDirectory`: the child cmMakefile (and its place in
    /// the parent's children and the global makefile order) is created when
    /// the directory is added, even when its configure is deferred.
    pub fn create_subdirectory(
        &mut self,
        source: &str,
        binary: &str,
        exclude_from_all: bool,
    ) -> usize {
        let parent = self.mf_index();
        let index = self.makefiles.len();
        let mut props = Props::default();
        if exclude_from_all {
            props.set("EXCLUDE_FROM_ALL", "TRUE".to_owned());
        }
        let exclude = exclude_from_all || self.makefiles[parent].exclude_from_all;
        self.makefiles.push(Makefile {
            source_dir: source.to_owned(),
            binary_dir: binary.to_owned(),
            parent: Some(parent),
            props,
            policies: Vec::new(),
            list_files: Vec::new(),
            output_files: Vec::new(),
            targets: Vec::new(),
            children: Vec::new(),
            exclude_from_all: exclude,
            deferred: Vec::new(),
            defer_counter: 0,
            // cmMakefile::Configure pushes the directory's list-file
            // context as the permanent bottom of its backtrace.
            backtrace: vec![Frame {
                file: Rc::from(self.project_list_file(source)),
                line: 0,
                name: None,
                deferred: false,
            }],
        });
        self.makefiles[parent].children.push(index);
        // cmState::CreateBuildsystemDirectorySnapshot runs
        // cmStateSnapshot::InitializeFromParent when the directory is
        // added: the child starts with the closure of the parent's
        // variables and its usage-requirement content (visible through
        // get_directory_property before a deferred `subdirs()` directory
        // configures). cmMakefile::AddSubDirectory then sets the current
        // source and binary directories.
        for key in [
            "INCLUDE_DIRECTORIES",
            "COMPILE_DEFINITIONS",
            "COMPILE_OPTIONS",
            "LINK_OPTIONS",
            "LINK_DIRECTORIES",
            "INCLUDE_REGULAR_EXPRESSION",
        ] {
            if let Some(value) = self.makefiles[parent].props.get(key).map(str::to_owned) {
                self.makefiles[index].props.set(key, value);
            }
        }
        let mut definitions: BTreeMap<String, String> = self
            .normal_definition_names()
            .into_iter()
            .filter_map(|name| {
                self.get_normal_def(&name)
                    .map(|value| (name, value.to_owned()))
            })
            .collect();
        for (name, value) in [
            ("CMAKE_SOURCE_DIR", self.makefiles[0].source_dir.clone()),
            ("CMAKE_BINARY_DIR", self.makefiles[0].binary_dir.clone()),
            ("CMAKE_CURRENT_SOURCE_DIR", source.to_owned()),
            ("CMAKE_CURRENT_BINARY_DIR", binary.to_owned()),
        ] {
            definitions.insert(name.to_owned(), value);
        }
        self.ext.directory_definitions.insert(index, definitions);
        let _ = std::fs::create_dir_all(binary);
        index
    }

    /// `cmMakefile::ConfigureSubDirectory`: InitializeFromParent, then run
    /// the child's CMakeLists.txt.
    pub fn configure_subdirectory(&mut self, index: usize, system: bool) -> bool {
        let parent = self.mf_index();
        self.save_directory_definitions(parent);
        let flattened = self.effective_policies();
        // Directory properties inherited at configure (InitializeFromParent).
        let parent_props = self.makefiles[parent].props.clone();
        for key in [
            "INCLUDE_DIRECTORIES",
            "COMPILE_DEFINITIONS",
            "COMPILE_OPTIONS",
            "LINK_OPTIONS",
            "LINK_DIRECTORIES",
            "INCLUDE_REGULAR_EXPRESSION",
        ] {
            if let Some(value) = parent_props.get(key) {
                self.makefiles[index].props.set(key, value.to_owned());
            }
        }
        for (key, value) in parent_props.iter() {
            if key.starts_with("COMPILE_DEFINITIONS_") {
                self.makefiles[index].props.set(key, value.to_owned());
            }
        }
        self.makefiles[index].policies = vec![PolicyFrame {
            map: flattened,
            weak: false,
        }];
        crate::commands::directory::initialize_subdirectory(self, parent, index, system);
        // Freeze the parent's own backtrace (its stack still holds the
        // add_subdirectory frame) for diagnostics issued on its behalf
        // while the child configures (cmMakefile::Backtrace).
        self.makefiles[parent].backtrace = self.backtrace.clone();
        let source = self.makefiles[index].source_dir.clone();
        let binary = self.makefiles[index].binary_dir.clone();
        if self.debug.output {
            self.system_message(&format!("   Entering             {source}"));
        }
        self.mf_stack.push(index);
        // cmMakefile::InitializeFromParent copies RecursionDepth; the new
        // makefile's ExecutionStatusStack starts empty.
        self.trace_frame_base.push(self.recursion_depth);
        self.push_scope();
        self.directory_scope_depths.push(self.scopes.len());
        self.set_def("CMAKE_CURRENT_SOURCE_DIR", &source);
        self.set_def("CMAKE_CURRENT_BINARY_DIR", &binary);
        // The child's backtrace starts at its own list file, independent of
        // the parent's call stack (cmMakefile::Configure).
        let saved_backtrace = std::mem::take(&mut self.backtrace);
        let list = self.project_list_file(&source);
        let ok = self.read_list_file_directory(&list);
        self.backtrace = saved_backtrace;
        self.save_directory_definitions(index);
        self.record_directory_vars();
        self.directory_scope_depths.pop();
        self.pop_scope();
        self.trace_frame_base.pop();
        self.mf_stack.pop();
        if self.debug.output {
            let parent_source = self.mf().source_dir.clone();
            self.system_message(&format!("   Returning to         {parent_source}"));
        }
        ok
    }

    fn save_directory_definitions(&mut self, index: usize) {
        let definitions = self
            .normal_definition_names()
            .into_iter()
            .filter_map(|name| {
                self.get_normal_def(&name)
                    .map(|value| (name, value.to_owned()))
            })
            .collect();
        self.ext.directory_definitions.insert(index, definitions);
    }

    // ------------------------------------------------------------------
    // Policies

    pub fn policy_status(&self, id: &str) -> PolicyStatus {
        self.policy_status_at(self.mf_index(), id)
    }

    pub fn policy_status_at(&self, dir: usize, id: &str) -> PolicyStatus {
        let Some(index) = crate::policy::index_of(id) else {
            return PolicyStatus::Warn;
        };
        if crate::policy_table::POLICIES[index].removed {
            return PolicyStatus::New;
        }
        for frame in self.makefiles[dir].policies.iter().rev() {
            if let Some(status) = frame.map.get(index) {
                return status;
            }
        }
        PolicyStatus::Warn
    }

    /// Effective status of every policy that is set, flattened.
    pub fn effective_policies(&self) -> PolicyMap {
        let mut map = PolicyMap::default();
        for frame in &self.mf().policies {
            map.overlay(&frame.map);
        }
        map
    }

    /// `cmMakefile::SetPolicy`.
    pub fn set_policy(&mut self, index: usize, status: PolicyStatus) -> bool {
        let info = &crate::policy_table::POLICIES[index];
        if info.removed && status != PolicyStatus::New {
            let message = crate::policy::removed_error(index);
            self.issue(MessageType::FatalError, &message);
            return false;
        }
        // Policies set by cmCoreTryCompile::TryCompileCode are exempt from the
        // deprecation warning inside try_compile projects.
        let try_compile_exempt =
            self.in_try_compile && matches!(index, 83 | 91 | 104 | 123 | 126 | 128 | 136 | 141);
        if status == PolicyStatus::Old
            && index <= 142
            && !try_compile_exempt
            && (!self.is_set("CMAKE_WARN_DEPRECATED") || self.is_on("CMAKE_WARN_DEPRECATED"))
        {
            let message = crate::policy::deprecated_warning(index);
            self.issue(MessageType::DeprecationWarning, &message);
        }
        let frames = &mut self.mf_mut().policies;
        for frame in frames.iter_mut().rev() {
            frame.map.set(index, status);
            if !frame.weak {
                break;
            }
        }
        true
    }

    pub fn push_policy(&mut self, weak: bool, map: PolicyMap) {
        self.mf_mut().policies.push(PolicyFrame { map, weak });
    }

    /// `cmStateSnapshot::CanPopPolicyScope`: whether `cmake_policy(POP)`
    /// could pop a non-barrier policy frame in the current scope.
    pub fn can_pop_policy_scope(&self) -> bool {
        let minimum = self
            .policy_barriers
            .iter()
            .rev()
            .find(|(dir, _)| *dir == self.mf_index())
            .map_or(1, |(_, depth)| *depth);
        self.mf().policies.len() > minimum
    }

    pub fn pop_policy(&mut self) -> bool {
        let minimum = self
            .policy_barriers
            .iter()
            .rev()
            .find(|(dir, _)| *dir == self.mf_index())
            .map_or(1, |(_, depth)| *depth);
        if self.mf().policies.len() <= minimum {
            self.issue(
                MessageType::FatalError,
                "cmake_policy POP without matching PUSH",
            );
            return false;
        }
        self.mf_mut().policies.pop();
        true
    }

    // ------------------------------------------------------------------
    // Diagnostics

    pub fn top_source(&self) -> Option<&str> {
        // cmake::SetHomeDirectory sets the messenger's top source for both
        // project and script mode (normal project kind). A try_compile
        // project uses cmState::ProjectKind::TryCompile, for which
        // SetTopSource receives nullopt, so its diagnostics keep absolute
        // paths even when they live under the outer project's source tree.
        if self.in_try_compile {
            return None;
        }
        self.top_source_root.as_deref().or(Some(&self.home_source))
    }

    /// `cmMakefile::IssueMessage` at the current backtrace.
    pub fn issue(&mut self, kind: MessageType, text: &str) {
        let backtrace = self.backtrace.clone();
        self.issue_at(kind, text, &backtrace);
    }

    /// The backtrace `mf.IssueMessage` uses for directory `dir`: the live
    /// stack when `dir` is the directory currently executing, otherwise the
    /// stack frozen when its configure ran (or its `CMakeLists.txt` entry
    /// frame for directories that already finished).
    pub fn directory_backtrace(&self, dir: usize) -> Vec<Frame> {
        if dir == self.mf_index() {
            self.backtrace.clone()
        } else {
            self.makefiles[dir].backtrace.clone()
        }
    }

    pub fn issue_at(&mut self, kind: MessageType, text: &str, backtrace: &[Frame]) {
        // `cmake::SetCacheArgs` syncs these messenger flags on every cache
        // write, so read the cache entries live here.
        let suppress_dev = self
            .cache
            .value("CMAKE_SUPPRESS_DEVELOPER_WARNINGS")
            .map(crate::util::is_on)
            .unwrap_or(self.suppress_dev_warnings);
        let dev_as_errors = self
            .cache
            .value("CMAKE_SUPPRESS_DEVELOPER_ERRORS")
            .map(crate::util::is_off)
            .unwrap_or(self.dev_warnings_as_errors);
        let kind = match kind {
            MessageType::AuthorWarning | MessageType::AuthorError => {
                if dev_as_errors {
                    MessageType::AuthorError
                } else {
                    if suppress_dev {
                        return;
                    }
                    MessageType::AuthorWarning
                }
            }
            MessageType::DeprecationWarning | MessageType::DeprecationError => {
                if self.deprecated_as_errors {
                    MessageType::DeprecationError
                } else {
                    if !self.warn_deprecated {
                        return;
                    }
                    MessageType::DeprecationWarning
                }
            }
            other => other,
        };
        self.sarif_log.message(kind, text, backtrace);
        let mut rendered = messages::render(kind, text, backtrace, self.top_source());
        // A deferred execution placeholder denotes a location, even though it
        // has no numeric line. Other contexts without a line use "in".
        if backtrace.last().is_some_and(|frame| frame.deferred)
            && let Some(index) = rendered.find(" in ")
        {
            rendered.replace_range(index..index + 4, " at ");
        }
        let dbg_text = rendered.clone();
        if messages::is_error(kind) {
            self.error = true;
        }
        let stack = self.debug_stack();
        self.stderr_attributed(messages::term_attr(kind), &rendered, &stack);
        self.debug_message(kind, &dbg_text);
    }

    /// Displays a diagnostic whose category was already selected by the caller.
    pub fn issue_at_unsuppressed(&mut self, kind: MessageType, text: &str) {
        self.sarif_log.message(kind, text, &self.backtrace);
        let rendered = messages::render(kind, text, &self.backtrace, self.top_source());
        let dbg_text = rendered.clone();
        if messages::is_error(kind) {
            self.error = true;
        }
        let stack = self.debug_stack();
        self.stderr_attributed(messages::term_attr(kind), &rendered, &stack);
        self.debug_message(kind, &dbg_text);
    }

    /// Writes one `cmSystemTools::Message` line to stderr (or to
    /// `stderr_capture`). The default message metadata carries the Normal
    /// attribute even though it produces no escape on a non-terminal.
    pub fn stderr_line(&mut self, text: &str) {
        self.stderr_attributed(crate::stdio::TermAttr::Normal.bit(), text, "");
    }

    fn stderr_attributed(&mut self, attrs: crate::stdio::TermAttrSet, text: &str, suffix: &str) {
        match &mut self.stderr_capture {
            Some(capture) => {
                capture.push_str(text);
                capture.push_str(suffix);
                capture.push('\n');
            }
            None => {
                let plain = if suffix.is_empty() {
                    "\n".to_owned()
                } else {
                    format!("{suffix}\n")
                };
                crate::stdio::print_err_then_plain(attrs, text, &plain);
            }
        }
    }

    /// `cmSystemTools::Message` to stdout without prefix.
    pub fn stdout_line_raw(&mut self, text: &str) {
        self.stdout_line(text);
    }

    /// Runs the top-level CMakeLists.txt with the root-directory rules of
    /// `cmMakefile::Configure` (CMP0000 detection and implicit project()).
    pub fn run_root_list_file(&mut self, path: &str) {
        // The directory snapshot ends the call stack (GetCallStackParent).
        self.ext
            .execution_file_bases
            .push(self.ext.execution_files.len());
        self.backtrace.push(Frame {
            file: Rc::from(path),
            line: 0,
            name: None,
            deferred: false,
        });
        let saved = self.push_list_file_vars(path);
        // `cmMakefile::Configure` sets CMAKE_PARENT_LIST_FILE to the
        // directory's own list file for the duration of the configure;
        // `pop_list_file_vars` then restores the inherited (unset) value.
        self.set_def("CMAKE_PARENT_LIST_FILE", path);
        self.debug_begin_parse(path);
        let parsed_file = self.parse_file(path);
        self.debug_end_parse();
        if let Some(parsed) = parsed_file {
            self.debug_file_parsed(path, &parsed.commands);
            let mut commands = parsed.commands.clone();
            let has_version = commands.iter().any(|c| c.lname == "cmake_minimum_required");
            if !has_version {
                const ALLOWED: [&str; 11] = [
                    "project",
                    "set",
                    "if",
                    "endif",
                    "else",
                    "elseif",
                    "add_executable",
                    "add_library",
                    "target_link_libraries",
                    "option",
                    "message",
                ];
                let problem = commands.len() >= 30
                    || commands
                        .iter()
                        .any(|c| !ALLOWED.contains(&c.lname.as_str()));
                if problem {
                    self.root_check_cmp0000 = true;
                    crate::commands::core::apply_policy_version_ex(self, "3.5", "", false);
                }
            }
            if !commands.iter().any(|c| c.lname == "project") {
                self.issue(
                    MessageType::AuthorWarning,
                    "No project() command is present.  The top-level CMakeLists.txt file must contain a literal, direct call to the project() command.  Add a line of code such as\n  project(ProjectName)\nnear the top of the file, but after cmake_minimum_required().\nCMake is pretending there is a \"project(Project)\" command on the first line.",
                );
                commands.insert(
                    0,
                    Command {
                        name: "project".into(),
                        lname: "project".into(),
                        line: 0,
                        line_end: 0,
                        args: vec![Arg {
                            value: "Project".into(),
                            delim: Delim::Unquoted,
                            line: 0,
                        }],
                    },
                );
            }
            let parsed = Rc::new(ParsedFile::new(parsed.path.clone(), commands));
            self.mf_mut().list_files.push(path.to_owned());
            self.ext.file_locks.files.push(Default::default());
            self.run_file_body(&parsed);
            self.run_deferred(path);
            crate::commands::directory::finish_directory(self);
            self.ext.file_locks.files.pop();
        }
        let depth = self.mf().policies.len();
        if depth > 1 && !self.fatal {
            self.issue(
                MessageType::FatalError,
                "cmake_policy PUSH without matching POP",
            );
            self.mf_mut().policies.truncate(1);
        }
        self.pop_list_file_vars(saved);
        self.backtrace.pop();
        self.ext.execution_file_bases.pop();
    }

    /// Writes a line to stdout (message STATUS etc.).
    pub fn stdout_line(&mut self, text: &str) {
        crate::bytes::stdout_line(text);
    }

    pub fn set_fatal(&mut self) {
        self.fatal = true;
        self.error = true;
    }

    // ------------------------------------------------------------------
    // List files

    fn parse_file(&mut self, path: &str) -> Option<Rc<ParsedFile>> {
        // Cache parses across interpreter instances (try_compile projects
        // re-read the same modules), keyed by path and content so that
        // rewritten files are always re-parsed.
        let bytes = std::fs::read(path).ok()?;
        let key = (path.to_owned(), bytes);
        if let Some(parsed) = PARSE_CACHE.with(|cache| cache.borrow().get(&key).cloned()) {
            return Some(parsed);
        }
        let bytes = &key.1;
        // `cmListFileLexer_SetFileName`/`cmListFileLexer_ReadBOM`: only a
        // UTF-8 BOM is accepted (and skipped); UTF-16/32 BOMs are rejected
        // before parsing with a file-level error that does not set the
        // fatal-error flag.
        let bytes = match bytes.as_slice() {
            [0xEF, 0xBB, 0xBF, rest @ ..] => rest,
            [0xFE, 0xFF, ..]
            | [0x00, 0x00, 0xFE, 0xFF, ..]
            | [0xFF, 0xFE, 0x00, 0x00, ..]
            | [0xFF, 0xFE, ..] => {
                self.issue(
                    MessageType::FatalError,
                    "File starts with a Byte-Order-Mark that is not UTF-8.",
                );
                return None;
            }
            _ => &bytes[..],
        };
        // `cmListFileLexerInput` converts CRLF to LF for file input only
        // (strings/EVAL keep their bytes). Do this before decoding so the
        // original byte offsets and any non-UTF-8 bytes survive exactly.
        let mut normalized;
        let bytes = if bytes.contains(&b'\r') {
            normalized = Vec::with_capacity(bytes.len());
            for line in bytes.split_inclusive(|byte| *byte == b'\n') {
                match line.strip_suffix(b"\r\n") {
                    Some(text) => {
                        normalized.extend_from_slice(text);
                        normalized.push(b'\n');
                    }
                    None => normalized.extend_from_slice(line),
                }
            }
            normalized.as_slice()
        } else {
            bytes
        };
        let rc_path: Rc<str> = Rc::from(path);
        let parsed = self.parse_bytes(rc_path, bytes)?;
        PARSE_CACHE.with(|cache| cache.borrow_mut().insert(key, parsed.clone()));
        Some(parsed)
    }

    pub fn parse_text(&mut self, path: Rc<str>, text: &str) -> Option<Rc<ParsedFile>> {
        self.parse_bytes(path, &crate::bytes::encode(text))
    }

    fn parse_bytes(&mut self, path: Rc<str>, bytes: &[u8]) -> Option<Rc<ParsedFile>> {
        let (commands, diags) = lexer::parse_bytes(&path, bytes);
        for diag in &diags {
            let mut backtrace = self.backtrace.clone();
            backtrace.push(Frame {
                file: path.clone(),
                line: diag.line,
                name: diag.command.as_deref().map(Rc::from),
                deferred: false,
            });
            let kind = if diag.warning {
                MessageType::AuthorWarning
            } else {
                MessageType::FatalError
            };
            self.issue_at(kind, &diag.message, &backtrace);
            if diag.fatal {
                self.set_fatal();
            }
        }
        commands.map(|commands| Rc::new(ParsedFile::new(path, commands)))
    }

    fn push_list_file_vars(&mut self, path: &str) -> (Option<String>, String) {
        self.ext.execution_files.push(path.to_owned());
        let old_parent = self.get_def("CMAKE_PARENT_LIST_FILE").map(str::to_owned);
        let old_current = self.get_def("CMAKE_CURRENT_LIST_FILE").map(str::to_owned);
        if let Some(current) = &old_current {
            self.set_def("CMAKE_PARENT_LIST_FILE", current.clone());
        }
        self.set_def("CMAKE_CURRENT_LIST_FILE", path);
        self.set_def("CMAKE_CURRENT_LIST_DIR", util::filename_path(path));
        (old_parent, old_current.unwrap_or_default())
    }

    fn pop_list_file_vars(&mut self, saved: (Option<String>, String)) {
        self.ext.execution_files.pop();
        match saved.0 {
            Some(parent) => self.set_def("CMAKE_PARENT_LIST_FILE", parent),
            None => self.unset_def("CMAKE_PARENT_LIST_FILE"),
        }
        self.set_def("CMAKE_CURRENT_LIST_DIR", util::filename_path(&saved.1));
        self.set_def("CMAKE_CURRENT_LIST_FILE", saved.1);
    }

    /// `cmMakefile::ReadDependentFile` (include). Returns false if the file
    /// could not be read/parsed.
    pub fn read_dependent_file(&mut self, path: &str, no_policy_scope: bool) -> bool {
        let path = util::collapse_full_path(path, &self.current_source_dir());
        let depth_before = self.mf().policies.len();
        self.backtrace.push(Frame {
            file: Rc::from(path.as_str()),
            line: 0,
            name: None,
            deferred: false,
        });
        self.policy_barriers.push((self.mf_index(), depth_before));
        if !no_policy_scope {
            self.push_policy(false, PolicyMap::default());
        }
        let saved = self.push_list_file_vars(&path);
        self.debug_begin_parse(&path);
        let parsed = self.parse_file(&path);
        self.debug_end_parse();
        let ok = parsed.is_some();
        if let Some(parsed) = parsed {
            self.debug_file_parsed(&path, &parsed.commands);
            self.mf_mut().list_files.push(path.clone());
            self.run_file_body(&parsed);
        }
        self.pop_list_file_vars(saved);
        if !no_policy_scope {
            self.pop_policy();
        }
        self.policy_barriers.pop();
        if self.mf().policies.len() > depth_before && !self.fatal {
            self.issue(
                MessageType::FatalError,
                "cmake_policy PUSH without matching POP",
            );
        }
        self.mf_mut().policies.truncate(depth_before);
        self.backtrace.pop();
        ok
    }

    /// Pops policy frames down to `depth`, diagnosing unbalanced PUSH.
    fn end_policy_scope(&mut self, depth: usize) {
        let current = self.mf().policies.len();
        if current > depth + 1 && !self.fatal {
            self.issue(
                MessageType::FatalError,
                "cmake_policy PUSH without matching POP",
            );
        }
        self.mf_mut().policies.truncate(depth);
    }

    pub fn project_list_file(&self, source: &str) -> String {
        crate::configure::project_list_file(source, &self.project_file)
    }

    /// Reads the directory's CMakeLists.txt (top-level or subdirectory).
    pub fn read_list_file_directory(&mut self, path: &str) -> bool {
        // The directory snapshot ends the call stack (GetCallStackParent).
        self.ext
            .execution_file_bases
            .push(self.ext.execution_files.len());
        self.backtrace.push(Frame {
            file: Rc::from(path),
            line: 0,
            name: None,
            deferred: false,
        });
        let saved = self.push_list_file_vars(path);
        // `cmMakefile::Configure` explicitly sets CMAKE_PARENT_LIST_FILE to
        // the directory's own list file for the duration of the configure;
        // `pop_list_file_vars` restores the inherited value afterwards.
        self.set_def("CMAKE_PARENT_LIST_FILE", path);
        self.debug_begin_parse(path);
        let parsed = self.parse_file(path);
        self.debug_end_parse();
        let ok = parsed.is_some();
        if let Some(parsed) = parsed {
            self.debug_file_parsed(path, &parsed.commands);
            self.mf_mut().list_files.push(path.to_owned());
            self.ext.file_locks.files.push(Default::default());
            self.run_file_body(&parsed);
            self.run_deferred(path);
            crate::commands::directory::finish_directory(self);
            self.ext.file_locks.files.pop();
        }
        // Directory-level end-of-file checks.
        let depth = self.mf().policies.len();
        if depth > 1 && !self.fatal {
            self.issue(
                MessageType::FatalError,
                "cmake_policy PUSH without matching POP",
            );
            self.mf_mut().policies.truncate(1);
        }
        self.pop_list_file_vars(saved);
        self.backtrace.pop();
        self.ext.execution_file_bases.pop();
        ok
    }

    /// Runs a script file in script mode (-P) or as the top-level file.
    pub fn read_list_file(&mut self, path: &str) -> bool {
        self.backtrace.push(Frame {
            file: Rc::from(path),
            line: 0,
            name: None,
            deferred: false,
        });
        let saved = self.push_list_file_vars(path);
        self.debug_begin_parse(path);
        let parsed = self.parse_file(path);
        self.debug_end_parse();
        let ok = parsed.is_some();
        if let Some(parsed) = parsed {
            self.debug_file_parsed(path, &parsed.commands);
            self.mf_mut().list_files.push(path.to_owned());
            self.ext.file_locks.files.push(Default::default());
            self.run_file_body(&parsed);
            self.ext.file_locks.files.pop();
        }
        self.pop_list_file_vars(saved);
        self.backtrace.pop();
        ok
    }

    /// `cmMakefile::ReadListFileAsString` (cmake_language EVAL).
    pub fn run_string(&mut self, code: &str, virtual_name: &str) -> bool {
        let path = util::collapse_full_path(virtual_name, &self.current_source_dir());
        self.backtrace.push(Frame {
            file: Rc::from(path.as_str()),
            line: 0,
            name: None,
            deferred: false,
        });
        let saved = self.push_list_file_vars(&path);
        let parsed = self.parse_text(Rc::from(path.as_str()), code);
        let ok = parsed.is_some();
        if let Some(parsed) = parsed {
            self.debug_file_parsed(&path, &parsed.commands);
            self.run_file_body(&parsed);
        }
        self.pop_list_file_vars(saved);
        self.backtrace.pop();
        ok
    }

    fn run_file_body(&mut self, parsed: &Rc<ParsedFile>) {
        self.loop_depth.push(0);
        let ctl = self.run_range(parsed, 0, parsed.commands.len(), None);
        self.loop_depth.pop();
        if let Ctl::Return(Some(vars)) = ctl {
            self.propagate(&vars);
        }
    }

    fn run_deferred(&mut self, path: &str) {
        loop {
            if self.fatal {
                return;
            }
            let next = {
                let mf = self.mf_mut();
                if mf.deferred.is_empty() {
                    None
                } else {
                    Some(mf.deferred.remove(0))
                }
            };
            let Some(deferred) = next else {
                return;
            };
            self.backtrace.push(Frame {
                file: Rc::from(path),
                line: 0,
                name: None,
                deferred: true,
            });
            self.ext.execution_files.push(deferred.file.to_string());
            let depth = self.mf().policies.len();
            let parsed = Rc::new(ParsedFile::new(
                deferred.file.clone(),
                vec![deferred.command],
            ));
            self.ext.defer_current_id = Some(deferred.id);
            self.loop_depth.push(0);
            self.run_range(&parsed, 0, 1, None);
            self.loop_depth.pop();
            self.ext.defer_current_id = None;
            self.ext.execution_files.pop();
            if self.mf().policies.len() > depth {
                if !self.fatal {
                    self.issue(
                        MessageType::FatalError,
                        "cmake_policy PUSH without matching POP",
                    );
                }
                self.mf_mut().policies.truncate(depth);
            }
            self.backtrace.pop();
        }
    }

    /// Sets `PROPAGATE` variables in the parent scope (`RaiseScope`).
    fn propagate(&mut self, vars: &[String]) {
        for name in vars {
            let value = self.get_normal_def(name).map(str::to_owned);
            self.raise_scope(name, value.as_deref());
        }
    }

    // ------------------------------------------------------------------
    // Execution

    /// Runs commands `start..end` of `file`.
    pub fn run_range(
        &mut self,
        file: &Rc<ParsedFile>,
        start: usize,
        end: usize,
        subst: Option<&MacroSubst>,
    ) -> Ctl {
        let mut index = start;
        while index < end {
            if self.fatal || self.exit_code.is_some() {
                return Ctl::Normal;
            }
            let command = &file.commands[index];
            let jumps = &file.jumps[index];
            // Flow commands run their own blocks here but are traced like
            // any command invocation (cmMakefile::ExecuteCommand).
            self.announce_flow_command(file, command, subst);
            let ctl = match command.lname.as_str() {
                "if" if self.is_flow("if") => {
                    let ctl = self.run_if(file, index, subst);
                    index = *jumps.last().expect("endif");
                    ctl
                }
                "foreach" if self.is_flow("foreach") => {
                    let (opened, ctl) = self.run_foreach(file, index, subst);
                    if opened {
                        index = jumps[0];
                    }
                    ctl
                }
                "while" if self.is_flow("while") => {
                    let (opened, ctl) = self.run_while(file, index, subst);
                    if opened {
                        index = jumps[0];
                    }
                    ctl
                }
                "function" | "macro" if self.is_flow(&command.lname) => {
                    if self.define_callable(file, index, subst) {
                        index = jumps[0];
                    }
                    Ctl::Normal
                }
                "block" if self.is_flow("block") => {
                    let (opened, ctl) = self.run_block(file, index, subst);
                    if opened {
                        index = jumps[0];
                    }
                    ctl
                }
                _ => self.execute(file, command, subst).1,
            };
            if ctl != Ctl::Normal {
                return ctl;
            }
            index += 1;
        }
        Ctl::Normal
    }

    fn is_flow(&self, name: &str) -> bool {
        matches!(self.commands.get(name), Some(CommandDef::Flow))
    }

    /// `cmMakefile::PrintCommandTrace`. `depth` is the recursion depth of
    /// the traced call (one more than the current depth for flow commands
    /// traced before their call frame, and for CommandMissingFromStack).
    pub fn print_command_trace(
        &mut self,
        file: &Rc<ParsedFile>,
        command: &Command,
        subst: Option<&MacroSubst>,
        depth: usize,
    ) {
        let Some(trace) = self.trace.clone() else {
            return;
        };
        let full_path: &str = &file.path;
        if !trace.sources.is_empty() {
            let only_filename = util::filename_name(full_path);
            let wanted = trace.sources.iter().any(|source| {
                full_path.ends_with(source.as_str()) && only_filename == util::filename_name(source)
            });
            if !wanted {
                return;
            }
        }
        let mut args = Vec::with_capacity(command.args.len());
        for arg in &command.args {
            let raw = match subst {
                Some(subst) if arg.delim != Delim::Bracket => subst.apply(&arg.value),
                _ => arg.value.clone(),
            };
            if trace.expand && arg.delim != Delim::Bracket {
                args.push(self.expand_variables(&raw, None, false, false));
            } else {
                args.push(raw);
            }
        }
        let defer_id = self.ext.defer_current_id.clone();
        let base = self.trace_frame_base.last().copied().unwrap_or(0);
        let msg = if trace.json {
            use crate::commands::string::json::Json;
            let mut val: std::collections::BTreeMap<Vec<u8>, Json> =
                std::collections::BTreeMap::new();
            val.insert(b"file".to_vec(), Json::Str(full_path.as_bytes().to_vec()));
            val.insert(b"line".to_vec(), Json::Int(command.line as i64));
            if command.line != command.line_end {
                val.insert(b"line_end".to_vec(), Json::Int(command.line_end as i64));
            }
            if let Some(id) = &defer_id {
                val.insert(b"defer".to_vec(), Json::Str(id.as_bytes().to_vec()));
            }
            val.insert(b"cmd".to_vec(), Json::Str(command.name.as_bytes().to_vec()));
            val.insert(
                b"args".to_vec(),
                Json::Array(
                    args.iter()
                        .map(|a| Json::Str(a.as_bytes().to_vec()))
                        .collect(),
                ),
            );
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs_f64();
            val.insert(b"time".to_vec(), Json::Real(now));
            val.insert(
                b"frame".to_vec(),
                Json::UInt(depth.saturating_sub(base) as u64),
            );
            val.insert(
                b"global_frame".to_vec(),
                Json::UInt((depth + self.recursion_depth_base) as u64),
            );
            crate::commands::string::json::write_json_compact(&Json::Object(val))
        } else {
            let mut msg = format!("{full_path}({}):", command.line);
            if let Some(id) = &defer_id {
                msg += &format!("DEFERRED:{id}:");
            }
            msg += &format!("  {}(", command.name);
            for arg in &args {
                msg += arg;
                msg.push(' ');
            }
            msg.push(')');
            msg
        };
        match &trace.file {
            Some(file) => {
                use std::io::Write;
                let _ = writeln!(file.borrow_mut(), "{msg}");
            }
            None => self.stderr_line(&msg),
        }
    }

    fn frame_for(&self, file: &Rc<ParsedFile>, command: &Command) -> Frame {
        Frame {
            file: file.path.clone(),
            line: command.line,
            name: Some(Rc::from(command.name.as_str())),
            deferred: false,
        }
    }

    /// The debugger side of `cmMakefile::CallScope`: pushes a stack frame
    /// and runs the breakpoint/step/pause stop checks
    /// (`cmDebuggerAdapter::OnBeginFunctionCall`). `None` costs nothing
    /// when the debugger is off.
    fn debug_begin_command(
        &self,
        file: &Rc<ParsedFile>,
        command: &Command,
        subst: Option<&MacroSubst>,
    ) -> Option<DebugFrameGuard> {
        let inner = self.debugger.as_ref()?.inner.clone();
        let args = command
            .args
            .iter()
            .map(|arg| {
                if arg.delim != Delim::Bracket {
                    subst.map_or_else(|| arg.value.clone(), |s| s.apply(&arg.value))
                } else {
                    arg.value.clone()
                }
            })
            .collect();
        inner.begin_function_call(
            self as *const _ as usize,
            self.mf_index(),
            &file.path,
            command.line as i64,
            &command.name,
            args,
        );
        Some(DebugFrameGuard { inner })
    }

    /// `OnBeginFileParse`/`OnEndFileParse` around a list-file parse, and
    /// `OnFileParsedSuccessfully` once it yields commands.
    fn debug_begin_parse(&self, path: &str) {
        if let Some(debugger) = &self.debugger {
            debugger
                .inner
                .begin_file_parse(self as *const _ as usize, self.mf_index(), path);
        }
    }

    fn debug_end_parse(&self) {
        if let Some(debugger) = &self.debugger {
            debugger.inner.end_file_parse();
        }
    }

    fn debug_file_parsed(&self, path: &str, commands: &[Command]) {
        if let Some(debugger) = &self.debugger {
            let functions: Vec<(i64, i64)> = commands
                .iter()
                .map(|command| (command.line as i64, command.line_end as i64))
                .collect();
            debugger.inner.file_parsed(path, &functions);
        }
    }

    /// `cmDebuggerAdapter::OnMessageOutput` (messenger `DisplayMessage`
    /// with the rendered text, or `DisplayStatus` with the raw line).
    pub fn debug_message(&self, kind: MessageType, text: &str) {
        if let Some(debugger) = &self.debugger {
            debugger.inner.message_output(kind, text);
        }
    }

    /// Expands a command's arguments (`cmMakefile::ExpandArguments`),
    /// returning values and whether each was quoted/bracketed.
    pub fn expand_arguments(
        &mut self,
        file: &str,
        args: &[Arg],
        subst: Option<&MacroSubst>,
    ) -> Option<Vec<(String, bool)>> {
        let mut out = Vec::with_capacity(args.len());
        for arg in args {
            if arg.delim == Delim::Bracket {
                out.push((arg.value.clone(), true));
                continue;
            }
            let raw = match subst {
                Some(subst) => subst.apply(&arg.value),
                None => arg.value.clone(),
            };
            let value = self.expand_variables(&raw, Some((file, arg.line)), false, false);
            match arg.delim {
                Delim::Quoted => out.push((value, true)),
                _ => {
                    for item in util::expand_list(&value, false) {
                        out.push((item, false));
                    }
                }
            }
        }
        if self.fatal { None } else { Some(out) }
    }

    /// Executes a single non-flow command. Returns (succeeded, control).
    pub fn execute(
        &mut self,
        file: &Rc<ParsedFile>,
        command: &Command,
        subst: Option<&MacroSubst>,
    ) -> (bool, Ctl) {
        self.backtrace.push(self.frame_for(file, command));
        let dbg = self.debug_begin_command(file, command, subst);
        let profile = self.profile_command(file, command, subst);
        let result = self.execute_inner(file, command, subst);
        drop(profile);
        self.backtrace.pop();
        drop(dbg);
        result
    }

    /// The profiling entry of `cmMakefile::CallScope`: the command's
    /// lower-case name, its unexpanded arguments and its location.
    fn profile_command(
        &self,
        file: &Rc<ParsedFile>,
        command: &Command,
        subst: Option<&MacroSubst>,
    ) -> crate::profiling::Entry {
        if self.in_try_compile {
            return crate::profiling::Entry::none();
        }
        crate::profiling::entry("script", &command.lname, || {
            use crate::commands::string::json::Json;
            let mut object = std::collections::BTreeMap::new();
            if !command.args.is_empty() {
                let values: Vec<String> = command
                    .args
                    .iter()
                    .map(|arg| match subst {
                        Some(subst) if arg.delim != Delim::Bracket => subst.apply(&arg.value),
                        _ => arg.value.clone(),
                    })
                    .collect();
                object.insert(
                    b"functionArgs".to_vec(),
                    Json::Str(values.join(" ").into_bytes()),
                );
            }
            object.insert(
                b"location".to_vec(),
                Json::Str(format!("{}:{}", file.path, command.line).into_bytes()),
            );
            Some(Json::Object(object))
        })
    }

    /// `cmMakefile::GetRecursionDepthLimit`: the normal variable wins over
    /// the environment; an invalid value falls back to the default without
    /// consulting the environment.
    fn recursion_depth_limit(&self) -> usize {
        const DEFAULT: usize = 1000;
        if let Some(value) = self.get_def("CMAKE_MAXIMUM_RECURSION_DEPTH") {
            return str_to_ulong(value).map_or(DEFAULT, |v| v as usize);
        }
        if let Some(value) = std::env::var_os("CMAKE_MAXIMUM_RECURSION_DEPTH")
            && let Some(value) = value.to_str()
        {
            return str_to_ulong(value).map_or(DEFAULT, |v| v as usize);
        }
        DEFAULT
    }

    fn execute_inner(
        &mut self,
        file: &Rc<ParsedFile>,
        command: &Command,
        subst: Option<&MacroSubst>,
    ) -> (bool, Ctl) {
        self.recursion_depth += 1;
        let depth = self.recursion_depth + self.recursion_depth_base;
        let limit = self.recursion_depth_limit();
        if depth > limit {
            self.recursion_depth -= 1;
            self.issue(
                MessageType::FatalError,
                &format!("Maximum recursion depth of {limit} exceeded"),
            );
            self.set_fatal();
            return (false, Ctl::Normal);
        }
        let result = self.dispatch(file, command, subst);
        self.recursion_depth -= 1;
        result
    }

    fn dispatch(
        &mut self,
        file: &Rc<ParsedFile>,
        command: &Command,
        subst: Option<&MacroSubst>,
    ) -> (bool, Ctl) {
        let Some(def) = self.commands.get(&command.lname).cloned() else {
            if !self.fatal {
                let message = format!("Unknown CMake command \"{}\".", command.name);
                self.issue(MessageType::FatalError, &message);
                self.set_fatal();
            }
            return (false, Ctl::Normal);
        };
        if self.fatal {
            return (false, Ctl::Normal);
        }
        if self.trace.is_some() {
            let depth = self.recursion_depth;
            self.print_command_trace(file, command, subst, depth);
        }
        match def {
            CommandDef::Flow => self.run_flow_command(file, command, subst),
            CommandDef::Raw(function) => function(self, file, command, subst),
            CommandDef::Unexpected(message) => {
                self.issue(
                    MessageType::FatalError,
                    &format!("{} {message}", command.name),
                );
                self.command_failed();
                (false, Ctl::Normal)
            }
            CommandDef::Builtin(function) => {
                let Some(args) = self.expand_arguments(&file.path, &command.args, subst) else {
                    return (true, Ctl::Normal);
                };
                let values: Vec<String> = args.into_iter().map(|(value, _)| value).collect();
                match function(self, &values) {
                    Ok(()) => {
                        if std::mem::take(&mut self.request_return) {
                            (true, Ctl::Return(None))
                        } else {
                            (true, Ctl::Normal)
                        }
                    }
                    Err(error) => {
                        if let CmdErr::Error(message) = error {
                            let text = format!("{} {}", command.name, message);
                            self.issue(MessageType::FatalError, &text);
                        }
                        self.command_failed();
                        (false, Ctl::Normal)
                    }
                }
            }
            CommandDef::User(callable) => {
                let Some(args) = self.expand_arguments(&file.path, &command.args, subst) else {
                    return (true, Ctl::Normal);
                };
                let values: Vec<String> = args.into_iter().map(|(value, _)| value).collect();
                self.invoke_callable(&callable, &values, &command.name)
            }
        }
    }

    pub fn set_fatal_if_script(&mut self) {
        self.command_failed();
    }

    fn command_failed(&mut self) {
        // cmake::CommandFailureAction::EXIT_CODE applies in NORMAL_MODE
        // (project configure): a command returning false reports an error
        // but the current list file keeps running. SCRIPT_MODE (-P),
        // FIND_PACKAGE_MODE and explicit SetFatalErrorOccurred sites stop it.
        // FATAL_ERROR applies outside NORMAL_MODE: -P and --find-package.
        if matches!(self.mode, Mode::Script | Mode::FindPackage) {
            self.set_fatal();
        }
    }

    /// Invokes a user function or macro with already-expanded arguments.
    pub fn invoke_callable(
        &mut self,
        callable: &Rc<Callable>,
        args: &[String],
        _call_name: &str,
    ) -> (bool, Ctl) {
        if args.len() < callable.params.len() {
            let message = if callable.is_macro {
                format!(
                    "{} Macro invoked with incorrect arguments for macro named: {}",
                    _call_name, callable.name
                )
            } else {
                format!(
                    "{} Function invoked with incorrect arguments for function named: {}",
                    _call_name, callable.name
                )
            };
            self.issue(MessageType::FatalError, &message);
            self.command_failed();
            return (false, Ctl::Normal);
        }
        self.ext
            .execution_files
            .push(callable.file.path.to_string());
        let argn = util::join_list(&args[callable.params.len()..]);
        let argv = util::join_list(args);
        if callable.is_macro {
            let mut subst = MacroSubst::default();
            for (param, value) in callable.params.iter().zip(args) {
                subst.pairs.push((format!("${{{param}}}"), value.clone()));
            }
            subst.pairs.push(("${ARGC}".into(), args.len().to_string()));
            subst.pairs.push(("${ARGN}".into(), argn));
            subst.pairs.push(("${ARGV}".into(), argv));
            for (index, value) in args.iter().enumerate() {
                subst
                    .argv
                    .push((format!("${{ARGV{index}}}"), value.clone()));
            }
            self.push_policy(true, callable.policies.clone());
            let depth = self.mf().policies.len();
            let ctl = self.run_body(callable, Some(&subst));
            self.end_policy_scope(depth - 1);
            self.ext.execution_files.pop();
            match ctl {
                BodyResult::Error => (false, Ctl::Normal),
                BodyResult::Ctl(ctl) => (true, ctl),
            }
        } else {
            self.push_scope();
            self.push_policy(true, callable.policies.clone());
            let depth = self.mf().policies.len();
            self.set_def("ARGC", args.len().to_string());
            for (index, value) in args.iter().enumerate() {
                self.set_def(format!("ARGV{index}"), value.clone());
            }
            for (param, value) in callable.params.iter().zip(args) {
                self.set_def(param.clone(), value.clone());
            }
            self.set_def("ARGV", argv);
            self.set_def("ARGN", argn);
            self.set_def("CMAKE_CURRENT_FUNCTION", callable.name.clone());
            self.set_def(
                "CMAKE_CURRENT_FUNCTION_LIST_FILE",
                callable.file.path.to_string(),
            );
            self.set_def(
                "CMAKE_CURRENT_FUNCTION_LIST_DIR",
                util::filename_path(&callable.file.path),
            );
            self.set_def(
                "CMAKE_CURRENT_FUNCTION_LIST_LINE",
                callable.line.to_string(),
            );
            self.loop_depth.push(0);
            self.ext.file_locks.functions.push(Default::default());
            let result = self.run_body(callable, None);
            self.ext.file_locks.functions.pop();
            self.loop_depth.pop();
            if let BodyResult::Ctl(Ctl::Return(Some(vars))) = &result {
                let vars = vars.clone();
                self.propagate(&vars);
            }
            self.end_policy_scope(depth - 1);
            self.pop_scope();
            self.ext.execution_files.pop();
            match result {
                BodyResult::Error => (false, Ctl::Normal),
                BodyResult::Ctl(_) => (true, Ctl::Normal),
            }
        }
    }

    /// Flow commands run their own blocks but are traced and profiled like
    /// any command invocation (cmMakefile::ExecuteCommand). Their own
    /// ExecuteCommand only opens the function blocker, so the profiling
    /// entry closes at once; the body is replayed outside its scope.
    fn announce_flow_command(
        &mut self,
        file: &Rc<ParsedFile>,
        command: &Command,
        subst: Option<&MacroSubst>,
    ) {
        if !matches!(
            command.lname.as_str(),
            "if" | "foreach" | "while" | "function" | "macro" | "block"
        ) || !self.is_flow(&command.lname)
        {
            return;
        }
        if self.trace.is_some() {
            let depth = self.recursion_depth + 1;
            self.print_command_trace(file, command, subst, depth);
        }
        if crate::profiling::enabled() {
            drop(self.profile_command(file, command, subst));
        }
    }

    /// Runs a function/macro body; a failing direct command stops it.
    fn run_body(&mut self, callable: &Rc<Callable>, subst: Option<&MacroSubst>) -> BodyResult {
        let file = callable.file.clone();
        let mut index = callable.start;
        while index < callable.end {
            if self.fatal || self.exit_code.is_some() {
                return BodyResult::Ctl(Ctl::Normal);
            }
            let command = &file.commands[index];
            let jumps = &file.jumps[index];
            self.announce_flow_command(&file, command, subst);
            let (ok, ctl) = match command.lname.as_str() {
                "if" if self.is_flow("if") => {
                    let ctl = self.run_if(&file, index, subst);
                    index = *jumps.last().expect("endif");
                    (true, ctl)
                }
                "foreach" if self.is_flow("foreach") => {
                    let (opened, ctl) = self.run_foreach(&file, index, subst);
                    if opened {
                        index = jumps[0];
                    }
                    (true, ctl)
                }
                "while" if self.is_flow("while") => {
                    let (opened, ctl) = self.run_while(&file, index, subst);
                    if opened {
                        index = jumps[0];
                    }
                    (true, ctl)
                }
                "function" | "macro" if self.is_flow(&command.lname) => {
                    if self.define_callable(&file, index, subst) {
                        index = jumps[0];
                    }
                    (true, Ctl::Normal)
                }
                "block" if self.is_flow("block") => {
                    let (opened, ctl) = self.run_block(&file, index, subst);
                    if opened {
                        index = jumps[0];
                    }
                    (true, ctl)
                }
                _ => self.execute(&file, command, subst),
            };
            if !ok {
                return BodyResult::Error;
            }
            match ctl {
                Ctl::Normal => {}
                Ctl::Return(vars) => return BodyResult::Ctl(Ctl::Return(vars)),
                other => {
                    if callable.is_macro {
                        return BodyResult::Ctl(other);
                    }
                }
            }
            index += 1;
        }
        BodyResult::Ctl(Ctl::Normal)
    }

    /// Returns false when no function blocker was created (bad arguments),
    /// in which case the body and end command run as ordinary commands.
    fn define_callable(
        &mut self,
        file: &Rc<ParsedFile>,
        index: usize,
        subst: Option<&MacroSubst>,
    ) -> bool {
        let command = &file.commands[index];
        let end = file.jumps[index][0];
        self.backtrace.push(self.frame_for(file, command));
        let dbg = self.debug_begin_command(file, command, subst);
        let args = self.expand_arguments(&file.path, &command.args, subst);
        let is_macro = command.lname == "macro";
        let Some(args) = args else {
            self.backtrace.pop();
            return false;
        };
        if args.is_empty() {
            let text = format!("{} called with incorrect number of arguments", command.name);
            self.issue(MessageType::FatalError, &text);
            self.command_failed();
            self.backtrace.pop();
            return false;
        }
        let values: Vec<String> = args.into_iter().map(|(value, _)| value).collect();
        drop(dbg);
        // Materialize the body if a macro substitution is active.
        let (body_file, start, stop) = match subst {
            Some(subst) => {
                let commands: Vec<Command> = file.commands[index + 1..end]
                    .iter()
                    .map(|cmd| {
                        let mut copy = cmd.clone();
                        for arg in &mut copy.args {
                            if arg.delim != Delim::Bracket {
                                arg.value = subst.apply(&arg.value);
                            }
                        }
                        copy
                    })
                    .collect();
                let count = commands.len();
                (
                    Rc::new(ParsedFile::new(file.path.clone(), commands)),
                    0,
                    count,
                )
            }
            None => (file.clone(), index + 1, end),
        };
        let callable = Rc::new(Callable {
            name: values[0].clone(),
            params: values[1..].to_vec(),
            file: body_file,
            start,
            end: stop,
            line: command.line,
            policies: self.effective_policies(),
            is_macro,
        });
        // `cmFunctionFunctionBlocker`/`cmMacroFunctionBlocker`: the end
        // command's first expanded argument must match the name; the
        // mismatch warning precedes `AddScriptedCommand` (which runs at
        // `endfunction`/`endmacro` time in C++ with the function context on
        // the backtrace).
        let end_command = &file.commands[end];
        let matched = match self.expand_arguments(&file.path, &end_command.args, subst) {
            Some(end_args) => {
                let end_args: Vec<String> = end_args.into_iter().map(|(value, _)| value).collect();
                end_args.is_empty() || end_args[0] == values[0]
            }
            None => true,
        };
        self.backtrace.pop();
        self.warn_end_args(file, command, end, matched, true);
        self.backtrace.push(self.frame_for(file, command));
        self.add_user_command(&values[0], callable);
        self.backtrace.pop();
        true
    }

    /// `cmState::AddScriptedCommand`: overriding renames the previous
    /// definition to `_<name>`; built-in flow control commands cannot be
    /// overridden at all.
    pub fn add_user_command(&mut self, name: &str, callable: Rc<Callable>) {
        let lname = name.to_ascii_lowercase();
        if matches!(self.commands.get(&lname), Some(CommandDef::Flow)) {
            let message =
                format!("Built-in flow control command \"{lname}\" cannot be overridden.");
            self.issue(MessageType::FatalError, &message);
            self.set_fatal();
            return;
        }
        if callable.is_macro {
            self.mf_mut().props.append("MACROS", name, false);
        }
        if let Some(old) = self.commands.get(&lname).cloned()
            && !matches!(old, CommandDef::Flow)
        {
            self.commands.insert(format!("_{lname}"), old);
        }
        self.commands.insert(lname, CommandDef::User(callable));
    }

    fn run_flow_command(
        &mut self,
        file: &Rc<ParsedFile>,
        command: &Command,
        subst: Option<&MacroSubst>,
    ) -> (bool, Ctl) {
        match command.lname.as_str() {
            "break" | "continue" => {
                let Some(args) = self.expand_arguments(&file.path, &command.args, subst) else {
                    return (true, Ctl::Normal);
                };
                let is_break = command.lname == "break";
                let in_loop = self.loop_depth.last().copied().unwrap_or(0) > 0;
                if !in_loop {
                    let message = if is_break {
                        "A BREAK command was found outside of a proper FOREACH or WHILE loop scope."
                    } else {
                        "A CONTINUE command was found outside of a proper FOREACH or WHILE loop scope."
                    };
                    self.issue(MessageType::FatalError, message);
                    self.set_fatal();
                    return (true, Ctl::Normal);
                }
                if !args.is_empty() {
                    let message = if is_break {
                        "The BREAK command does not accept any arguments."
                    } else {
                        "The CONTINUE command does not accept any arguments."
                    };
                    self.issue(MessageType::FatalError, message);
                    self.set_fatal();
                    return (true, Ctl::Normal);
                }
                (true, if is_break { Ctl::Break } else { Ctl::Continue })
            }
            "return" => {
                let Some(args) = self.expand_arguments(&file.path, &command.args, subst) else {
                    return (true, Ctl::Normal);
                };
                let values: Vec<String> = args.into_iter().map(|(value, _)| value).collect();
                if values.is_empty() {
                    return (true, Ctl::Return(None));
                }
                // `cmReturnCommand`: CMP0140 gates argument checking;
                // under OLD/WARN the arguments are ignored and no return
                // is invoked at all.
                match self.policy_status("CMP0140") {
                    PolicyStatus::Warn => {
                        let index = crate::policy::index_of("CMP0140").unwrap();
                        let text = format!(
                            "{}\nreturn() checks its arguments when the policy is set to NEW. Since the policy is not set the OLD behavior will be used so the arguments will be ignored.",
                            crate::policy::policy_warning(index)
                        );
                        self.issue(MessageType::AuthorWarning, &text);
                        return (true, Ctl::Normal);
                    }
                    PolicyStatus::Old => return (true, Ctl::Normal),
                    _ => {}
                }
                if values[0] != "PROPAGATE" {
                    let text = format!(
                        "{} called with unsupported argument \"{}\"",
                        command.name, values[0]
                    );
                    self.issue(MessageType::FatalError, &text);
                    self.command_failed();
                    return (false, Ctl::Normal);
                }
                (true, Ctl::Return(Some(values[1..].to_vec())))
            }
            "else" | "elseif" | "endif" | "endforeach" | "endwhile" | "endfunction"
            | "endmacro" | "endblock" => {
                // `cmState::AddUnexpectedCommand`: `endif` is silently
                // accepted for ancient `cmake_minimum_required` versions;
                // otherwise this is a command failure (fatal only in script
                // mode).
                if command.lname == "endif" {
                    let quiet = self
                        .get_def("CMAKE_MINIMUM_REQUIRED_VERSION")
                        .is_none_or(|value| atof_prefix(value) <= 1.4);
                    if quiet {
                        return (true, Ctl::Normal);
                    }
                }
                let error = match command.lname.as_str() {
                    "else" => {
                        "An ELSE command was found outside of a proper IF ENDIF structure. Or its arguments did not match the opening IF command."
                    }
                    "elseif" => {
                        "An ELSEIF command was found outside of a proper IF ENDIF structure."
                    }
                    "endforeach" => {
                        "An ENDFOREACH command was found outside of a proper FOREACH ENDFOREACH structure. Or its arguments did not match the opening FOREACH command."
                    }
                    "endfunction" => {
                        "An ENDFUNCTION command was found outside of a proper FUNCTION ENDFUNCTION structure. Or its arguments did not match the opening FUNCTION command."
                    }
                    "endif" => {
                        "An ENDIF command was found outside of a proper IF ENDIF structure. Or its arguments did not match the opening IF command."
                    }
                    "endmacro" => {
                        "An ENDMACRO command was found outside of a proper MACRO ENDMACRO structure. Or its arguments did not match the opening MACRO command."
                    }
                    "endwhile" => {
                        "An ENDWHILE command was found outside of a proper WHILE ENDWHILE structure. Or its arguments did not match the opening WHILE command."
                    }
                    "endblock" => {
                        "An ENDBLOCK command was found outside of a proper BLOCK ENDBLOCK structure."
                    }
                    _ => unreachable!(),
                };
                let text = format!("{} {}", command.name, error);
                self.issue(MessageType::FatalError, &text);
                self.command_failed();
                (false, Ctl::Normal)
            }
            other => {
                // Flow commands invoked indirectly (cmake_language CALL).
                let message = format!("Unknown CMake command \"{other}\".");
                self.issue(MessageType::FatalError, &message);
                self.set_fatal();
                (false, Ctl::Normal)
            }
        }
    }

    fn run_if(&mut self, file: &Rc<ParsedFile>, index: usize, subst: Option<&MacroSubst>) -> Ctl {
        let jumps = file.jumps[index].clone();
        let command = &file.commands[index];
        // `cmIfFunctionBlocker::ArgumentsMatch`: `endif` arguments must be
        // empty or identical to the `if` arguments.
        let end_index = *jumps.last().expect("endif");
        let end_command = &file.commands[end_index];
        let matched =
            end_command.args.is_empty() || raw_args_equal(&end_command.args, &command.args);
        self.warn_end_args(file, command, end_index, matched, true);
        // Evaluate the `if` condition.
        self.backtrace.push(self.frame_for(file, command));
        let dbg = self.debug_begin_command(file, command, subst);
        let mut taken = self.evaluate_if_condition(file, command, subst, "if");
        self.backtrace.pop();
        drop(dbg);
        if self.fatal {
            return Ctl::Normal;
        }
        let mut else_seen = false;
        let mut branch_start = index + 1;
        let mut has_run = false;
        for &separator in &jumps {
            let sep = &file.commands[separator];
            if taken && !has_run {
                has_run = true;
                let ctl = self.run_range(file, branch_start, separator, subst);
                if ctl != Ctl::Normal || self.fatal {
                    return ctl;
                }
            }
            match sep.lname.as_str() {
                "else" => {
                    let frame = self.frame_for(file, sep);
                    if else_seen {
                        self.issue_at(
                            MessageType::FatalError,
                            "A duplicate ELSE command was found inside an IF block.",
                            &[self.backtrace.clone(), vec![frame]].concat(),
                        );
                        self.set_fatal();
                        return Ctl::Normal;
                    }
                    else_seen = true;
                    taken = !has_run;
                    if !has_run && self.trace.is_some() {
                        // CommandMissingFromStack::Yes
                        let depth = self.recursion_depth + 1;
                        self.print_command_trace(file, sep, subst, depth);
                    }
                }
                "elseif" => {
                    let frame = self.frame_for(file, sep);
                    let backtrace = [self.backtrace.clone(), vec![frame]].concat();
                    if else_seen {
                        self.issue_at(
                            MessageType::FatalError,
                            "An ELSEIF command was found after an ELSE command.",
                            &backtrace,
                        );
                        self.set_fatal();
                        return Ctl::Normal;
                    }
                    if sep.args.is_empty() {
                        self.issue_at(
                            MessageType::AuthorWarning,
                            "ELSEIF called with no arguments, it will be skipped. ",
                            &backtrace,
                        );
                    }
                    if has_run {
                        taken = false;
                    } else {
                        if self.trace.is_some() {
                            let depth = self.recursion_depth + 1;
                            self.print_command_trace(file, sep, subst, depth);
                        }
                        let saved = std::mem::replace(&mut self.backtrace, backtrace);
                        taken = self.evaluate_if_condition(file, sep, subst, "elseif");
                        self.backtrace = saved;
                        if self.fatal {
                            return Ctl::Normal;
                        }
                    }
                }
                _ => {}
            }
            branch_start = separator + 1;
        }
        Ctl::Normal
    }

    fn evaluate_if_condition(
        &mut self,
        file: &Rc<ParsedFile>,
        command: &Command,
        subst: Option<&MacroSubst>,
        name: &str,
    ) -> bool {
        let Some(args) = self.expand_arguments(&file.path, &command.args, subst) else {
            return false;
        };
        let (result, error) = crate::condition::evaluate(self, &args);
        if let Some((kind, message)) = error {
            // `cmIfCommand` prepends "if " to the condition error;
            // `cmIfFunctionBlocker` reports elseif's error bare.
            let mut text = if name == "if" {
                "if given arguments:\n ".to_string()
            } else {
                "given arguments:\n ".to_owned()
            };
            for (value, _) in &args {
                text.push(' ');
                text.push_str(&util::escape_for_cmake(value));
            }
            text.push('\n');
            text.push_str(&message);
            self.issue(kind, &text);
            if kind == MessageType::FatalError {
                self.set_fatal();
            }
        }
        result
    }

    /// `cmFunctionBlocker::IsFunctionBlocked` end-command diagnostics: the
    /// author warning issued when the end command's arguments do not match
    /// the opener (`EndCommandSupportsArguments` is false only for `block`,
    /// which instead warns about any arguments at all).
    fn warn_end_args(
        &mut self,
        file: &Rc<ParsedFile>,
        opener: &Command,
        end_index: usize,
        args_match: bool,
        supports_args: bool,
    ) {
        let end = &file.commands[end_index];
        let warn = if supports_args {
            !end.args.is_empty() && !args_match
        } else {
            !end.args.is_empty()
        };
        if !warn {
            return;
        }
        let opening = format!("{}:{} ({})", file.path, opener.line, opener.name);
        let closing = format!("{}:{} ({})", file.path, end.line, end.name);
        let text = if supports_args {
            format!(
                "A logical block opening on the line\n  {opening}\ncloses on the line\n  {closing}\nwith mis-matching arguments."
            )
        } else {
            format!("A logical block closing on the line\n  {closing}\nhas unexpected arguments.")
        };
        self.issue(MessageType::AuthorWarning, &text);
    }

    /// Returns `(block_opened, ctl)`: `block_opened` is false when the
    /// command failed before creating its function blocker, in which case
    /// the body and end command run as ordinary commands.
    fn run_foreach(
        &mut self,
        file: &Rc<ParsedFile>,
        index: usize,
        subst: Option<&MacroSubst>,
    ) -> (bool, Ctl) {
        let end = file.jumps[index][0];
        let command = &file.commands[index];
        self.backtrace.push(self.frame_for(file, command));
        let dbg = self.debug_begin_command(file, command, subst);
        let Some(args) = self.expand_arguments(&file.path, &command.args, subst) else {
            self.backtrace.pop();
            return (true, Ctl::Normal);
        };
        let values: Vec<String> = args.into_iter().map(|(value, _)| value).collect();
        let plan = crate::flow::foreach_plan(self, &values);
        self.backtrace.pop();
        drop(dbg);
        let Some(plan) = plan else {
            return (false, Ctl::Normal);
        };
        // `cmForEachFunctionBlocker::ArgumentsMatch`: the endforeach's first
        // expanded argument must match the first loop variable name.
        let end_command = &file.commands[end];
        let matched = match self.expand_arguments(&file.path, &end_command.args, subst) {
            Some(end_args) => {
                let end_args: Vec<String> = end_args.into_iter().map(|(value, _)| value).collect();
                end_args.is_empty() || end_args[0] == values[0]
            }
            None => true,
        };
        self.warn_end_args(file, command, end, matched, true);
        // Save the loop variables (restored afterwards unless fatal).
        // CMP0124: OLD restores the loop variables to their (possibly
        // empty) prior values instead of unsetting undefined ones.
        let cmp0124_new = self.policy_status("CMP0124") == PolicyStatus::New;
        let saved: Vec<(String, Option<String>)> = plan
            .vars
            .iter()
            .map(|name| {
                let value = if cmp0124_new {
                    self.get_normal_def(name).map(str::to_owned)
                } else {
                    Some(self.get_def(name).unwrap_or_default().to_owned())
                };
                (name.clone(), value)
            })
            .collect();
        let mut result = Ctl::Normal;
        *self.loop_depth.last_mut().expect("loop depth") += 1;
        for iteration in &plan.iterations {
            for (name, value) in plan.vars.iter().zip(iteration) {
                match value {
                    Some(value) => self.set_def(name.clone(), value.clone()),
                    None => self.unset_def(name),
                }
            }
            let ctl = self.run_range(file, index + 1, end, subst);
            if self.fatal {
                *self.loop_depth.last_mut().expect("loop depth") -= 1;
                return (true, Ctl::Normal);
            }
            match ctl {
                Ctl::Break => break,
                Ctl::Return(vars) => {
                    result = Ctl::Return(vars);
                    break;
                }
                _ => {}
            }
            if self.exit_code.is_some() {
                break;
            }
        }
        *self.loop_depth.last_mut().expect("loop depth") -= 1;
        // The blocker restores only after an iteration ran.
        let saved = if plan.iterations.is_empty() {
            Vec::new()
        } else {
            saved
        };
        for (name, value) in saved {
            match value {
                Some(value) => self.set_def(name, value),
                None => self.unset_def(&name),
            }
        }
        (true, result)
    }

    /// Returns `(block_opened, ctl)` like `run_foreach`.
    fn run_while(
        &mut self,
        file: &Rc<ParsedFile>,
        index: usize,
        subst: Option<&MacroSubst>,
    ) -> (bool, Ctl) {
        let end = file.jumps[index][0];
        let command = &file.commands[index];
        // `cmWhileCommand`: no arguments means no blocker was created; the
        // body and `endwhile` then run as ordinary commands.
        self.backtrace.push(self.frame_for(file, command));
        let dbg = self.debug_begin_command(file, command, subst);
        if command.args.is_empty() {
            self.issue(
                MessageType::FatalError,
                "while called with incorrect number of arguments",
            );
            self.command_failed();
            self.backtrace.pop();
            return (false, Ctl::Normal);
        }
        self.backtrace.pop();
        drop(dbg);
        // `cmWhileFunctionBlocker::ArgumentsMatch`: raw argument equality or
        // no endwhile arguments.
        let end_command = &file.commands[end];
        let matched =
            end_command.args.is_empty() || raw_args_equal(&end_command.args, &command.args);
        self.warn_end_args(file, command, end, matched, true);
        let max_iterations: usize = 1_000_000_000;
        let mut count = 0usize;
        *self.loop_depth.last_mut().expect("loop depth") += 1;
        let mut result = Ctl::Normal;
        // `cmWhileFunctionBlocker::Replay`: the condition is re-expanded and
        // re-evaluated every iteration; the last evaluation's error is
        // enforced after the loop per CMP0130.
        let mut last_args: Vec<(String, bool)> = Vec::new();
        let mut last_error: Option<(MessageType, String)> = None;
        let mut completed = false;
        loop {
            self.backtrace.push(self.frame_for(file, command));
            let Some(args) = self.expand_arguments(&file.path, &command.args, subst) else {
                self.backtrace.pop();
                break;
            };
            last_args = args.clone();
            let (condition, error) = crate::condition::evaluate(self, &args);
            last_error = error;
            self.backtrace.pop();
            if !condition || self.fatal {
                completed = true;
                break;
            }
            count += 1;
            if count > max_iterations {
                break;
            }
            let ctl = self.run_range(file, index + 1, end, subst);
            if self.fatal || self.exit_code.is_some() {
                break;
            }
            match ctl {
                Ctl::Break => break,
                Ctl::Return(vars) => {
                    result = Ctl::Return(vars);
                    break;
                }
                _ => {}
            }
        }
        *self.loop_depth.last_mut().expect("loop depth") -= 1;
        // CMP0130 diagnostics apply only when the loop ended by a false
        // condition evaluation (early exits skip error enforcement).
        if completed && result == Ctl::Normal {
            if let Some((kind, message)) = last_error {
                match self.policy_status("CMP0130") {
                    PolicyStatus::Warn => {
                        let mut text = format!(
                            "{}\nwhile() given incorrect arguments:\n ",
                            policy::policy_warning(
                                policy::index_of("CMP0130").expect("registered policy")
                            )
                        );
                        for (value, _) in &last_args {
                            text.push(' ');
                            text.push_str(&util::escape_for_cmake(value));
                        }
                        text.push('\n');
                        text.push_str(&message);
                        self.backtrace.push(self.frame_for(file, command));
                        self.issue(MessageType::AuthorWarning, &text);
                        self.backtrace.pop();
                    }
                    PolicyStatus::Old => {}
                    PolicyStatus::New => {
                        let mut text = String::from("while() given incorrect arguments:\n ");
                        for (value, _) in &last_args {
                            text.push(' ');
                            text.push_str(&util::escape_for_cmake(value));
                        }
                        text.push('\n');
                        text.push_str(&message);
                        self.backtrace.push(self.frame_for(file, command));
                        self.issue(kind, &text);
                        if kind == MessageType::FatalError {
                            self.set_fatal();
                        }
                        self.backtrace.pop();
                    }
                }
            }
        }
        (true, result)
    }

    /// Returns `(block_opened, ctl)` like `run_foreach`.
    fn run_block(
        &mut self,
        file: &Rc<ParsedFile>,
        index: usize,
        subst: Option<&MacroSubst>,
    ) -> (bool, Ctl) {
        let end = file.jumps[index][0];
        let command = &file.commands[index];
        self.backtrace.push(self.frame_for(file, command));
        let dbg = self.debug_begin_command(file, command, subst);
        let Some(args) = self.expand_arguments(&file.path, &command.args, subst) else {
            self.backtrace.pop();
            return (true, Ctl::Normal);
        };
        let values: Vec<String> = args.into_iter().map(|(value, _)| value).collect();
        let options = match crate::flow::parse_block(&values) {
            Ok(options) => options,
            Err(message) => {
                // `Error after keyword` is issued by
                // ParseResult::MaybeReportError (no command prefix); other
                // failures go through status.SetError (command name
                // prepended). Both paths call SetFatalErrorOccurred, so the
                // body and endblock() never run.
                let text = if message.starts_with("Error after keyword") {
                    message
                } else {
                    format!("{} {}", command.name, message)
                };
                self.issue(MessageType::FatalError, &text);
                self.command_failed();
                self.set_fatal();
                self.backtrace.pop();
                return (false, Ctl::Normal);
            }
        };
        self.backtrace.pop();
        drop(dbg);
        // `cmBlockFunctionBlocker`: `endblock` takes no arguments.
        self.warn_end_args(file, command, end, true, false);
        let policy_depth = self.mf().policies.len();
        if options.variables {
            self.push_scope();
        }
        if options.policies {
            self.push_policy(false, PolicyMap::default());
        }
        let ctl = self.run_range(file, index + 1, end, subst);
        // `return()` inside a block propagates outward; block PROPAGATE
        // raises the listed variables into the enclosing scope.
        if options.variables {
            if !self.fatal {
                for name in &options.propagate {
                    let value = self.get_normal_def(name).map(str::to_owned);
                    self.raise_scope(name, value.as_deref());
                }
                // `cmBlockFunctionBlocker::Replay`: a `return(PROPAGATE)`
                // raises the named variables out of the block scope (still
                // on top) before the scope closes.
                if let Ctl::Return(Some(vars)) = &ctl {
                    let vars = vars.clone();
                    self.propagate(&vars);
                }
            }
            self.pop_scope();
        }
        if options.policies {
            self.end_policy_scope(policy_depth);
        }
        (true, ctl)
    }

    // ------------------------------------------------------------------
    // Variable expansion (port of cmMakefile::ExpandVariablesInStringImpl)

    /// Expands `${}`, `$ENV{}`, `$CACHE{}` references and escape sequences.
    /// `location` provides the file/line for CMAKE_CURRENT_LIST_LINE and
    /// error messages. Errors are reported (fatal) and the input is returned.
    pub fn expand_variables(
        &mut self,
        source: &str,
        location: Option<(&str, usize)>,
        at_only: bool,
        replace_at: bool,
    ) -> String {
        match self.expand_impl(source, location, false, at_only, replace_at, false) {
            Ok(result) => result,
            Err((kind, message)) => {
                self.issue(kind, &message);
                if kind == MessageType::FatalError {
                    self.set_fatal();
                }
                source.to_owned()
            }
        }
    }

    /// `configure_file`/`string(CONFIGURE)` style expansion: `no_escapes`
    /// and optional `@ONLY`.
    pub fn expand_configure(
        &mut self,
        source: &str,
        at_only: bool,
        escape_quotes: bool,
    ) -> Result<String, String> {
        let frame = self
            .backtrace
            .last()
            .map(|frame| (frame.file.clone(), frame.line));
        let location = frame.as_ref().map(|(file, line)| (file.as_ref(), *line));
        let source = source.split_once('\0').map_or(source, |(text, _)| text);
        self.expand_impl(source, location, escape_quotes, at_only, true, true)
            .map_err(|(_, message)| message)
    }

    fn expand_impl(
        &mut self,
        source: &str,
        location: Option<(&str, usize)>,
        escape_quotes: bool,
        at_only: bool,
        replace_at: bool,
        no_escapes: bool,
    ) -> Result<String, (MessageType, String)> {
        let encoded = crate::bytes::encode(source);
        let bytes = encoded.as_ref();
        let mut result: Vec<u8> = Vec::with_capacity(bytes.len());
        // (domain, position in result where the name begins)
        let mut open: Vec<(u8, usize)> = Vec::new();
        let mut last = 0usize;
        let mut index = 0usize;
        let mut line = location.map_or(0, |(_, line)| line);
        let mut error: Option<String> = None;
        let mut fatal = false;
        let at = |i: usize| bytes.get(i).copied().unwrap_or(0);
        while index < bytes.len() {
            let ch = bytes[index];
            let mut handled_default = false;
            match ch {
                b'}' => {
                    if let Some((domain, position)) = open.pop() {
                        result.extend_from_slice(&bytes[last..index]);
                        let lookup = crate::bytes::decode(&result[position..]);
                        let value: Option<String> = match domain {
                            b'N' => {
                                if location.is_some() && lookup == "CMAKE_CURRENT_LIST_LINE" {
                                    let deferred = self
                                        .backtrace
                                        .iter()
                                        .rev()
                                        .nth(1)
                                        .is_some_and(|frame| frame.deferred);
                                    match &self.ext.defer_current_id {
                                        Some(id) if deferred => Some(format!("DEFERRED:{id}")),
                                        _ => Some(line.to_string()),
                                    }
                                } else {
                                    let value = self.read_definition(&lookup);
                                    if value.is_none() {
                                        let file = location.map(|(file, _)| file);
                                        self.maybe_warn_uninitialized(&lookup, file);
                                    }
                                    value
                                }
                            }
                            b'E' => std::env::var_os(crate::bytes::encode_os(&lookup))
                                .map(|value| crate::bytes::decode_os(&value)),
                            _ => self.cache.get(&lookup).map(|entry| entry.value.clone()),
                        };
                        let value = value.unwrap_or_default();
                        let value = if escape_quotes {
                            value.replace('"', "\\\"")
                        } else {
                            value
                        };
                        result.truncate(position);
                        result.extend_from_slice(&crate::bytes::encode(&value));
                        last = index + 1;
                    }
                }
                b'$' if !at_only => {
                    let next = at(index + 1);
                    let mut start = None;
                    let mut domain = b'N';
                    if next == b'{' {
                        start = Some(index + 2);
                    } else if next == b'<' {
                    } else if next == 0 {
                        result.extend_from_slice(&bytes[last..index + 1]);
                        last = index + 1;
                    } else if bytes[index + 1..].starts_with(b"ENV{") {
                        start = Some(index + 5);
                        domain = b'E';
                    } else if bytes[index + 1..].starts_with(b"CACHE{") {
                        start = Some(index + 7);
                        domain = b'C';
                    } else {
                        // `$[A-Za-z_][A-Za-z0-9_]*{` is an error.
                        let rest = &bytes[index + 1..];
                        let name_len = rest
                            .iter()
                            .take_while(|byte| byte.is_ascii_alphanumeric() || **byte == b'_')
                            .count();
                        if name_len > 0
                            && (rest[0].is_ascii_alphabetic() || rest[0] == b'_')
                            && rest.get(name_len) == Some(&b'{')
                        {
                            error = Some(format!(
                                "Syntax ${}{{}} is not supported.  Only ${{}}, $ENV{{}}, and $CACHE{{}} are allowed.",
                                crate::bytes::decode(&rest[..name_len])
                            ));
                            fatal = true;
                            break;
                        }
                    }
                    if let Some(start) = start {
                        result.extend_from_slice(&bytes[last..index]);
                        last = start;
                        open.push((domain, result.len()));
                        index = start;
                        continue;
                    }
                }
                b'\\' if !no_escapes => {
                    let next = at(index + 1);
                    if next == b't' {
                        result.extend_from_slice(&bytes[last..index]);
                        result.push(b'\t');
                        last = index + 2;
                    } else if next == b'n' {
                        result.extend_from_slice(&bytes[last..index]);
                        result.push(b'\n');
                        last = index + 2;
                    } else if next == b'r' {
                        result.extend_from_slice(&bytes[last..index]);
                        result.push(b'\r');
                        last = index + 2;
                    } else if next == b';' && open.is_empty() {
                        // Handled by list expansion; keep the backslash.
                    } else if next.is_ascii_alphanumeric() || next == 0 {
                        let mut message = String::from("Invalid character escape '\\");
                        if next != 0 {
                            let ch = if next.is_ascii() {
                                (next as char).to_string()
                            } else {
                                crate::bytes::decode(&bytes[index + 1..index + 2])
                            };
                            message.push_str(&ch);
                            message.push_str("'.");
                        } else {
                            message.push_str("' (at end of input).");
                        }
                        error = Some(message);
                        break;
                    } else {
                        result.extend_from_slice(&bytes[last..index]);
                        last = index + 1;
                    }
                    if last < bytes.len() {
                        index += 1;
                    }
                }
                b'\\' | b'$' => {}
                b'\n' => line += 1,
                b'@' if replace_at => {
                    let rest = &bytes[index + 1..];
                    if let Some(close) = rest.iter().position(|byte| *byte == b'@')
                        && close > 0
                        && rest[..close].iter().all(|byte| {
                            byte.is_ascii_alphanumeric()
                                || matches!(byte, b'/' | b'_' | b'.' | b'+' | b'-')
                        })
                    {
                        let name = crate::bytes::decode(&rest[..close]);
                        let value = if location.is_some() && name == "CMAKE_CURRENT_LIST_LINE" {
                            line.to_string()
                        } else {
                            let value = self.read_definition(&name);
                            if value.is_none() {
                                let file = location.map(|(file, _)| file);
                                self.maybe_warn_uninitialized(&name, file);
                            }
                            value.unwrap_or_default()
                        };
                        let value = if escape_quotes {
                            value.replace('"', "\\\"")
                        } else {
                            value
                        };
                        result.extend_from_slice(&bytes[last..index]);
                        result.extend_from_slice(&crate::bytes::encode(&value));
                        index = index + 1 + close;
                        last = index + 1;
                    } else {
                        handled_default = true;
                    }
                }
                _ => handled_default = true,
            }
            if handled_default && !open.is_empty() {
                let ok =
                    ch.is_ascii_alphanumeric() || matches!(ch, b'_' | b'/' | b'.' | b'+' | b'-');
                if !ok {
                    let ch_text = if ch.is_ascii() {
                        (ch as char).to_string()
                    } else {
                        crate::bytes::decode(&bytes[index..index + 1])
                    };
                    result.extend_from_slice(&bytes[last..index]);
                    let partial = crate::bytes::decode(&result[open.last().unwrap().1..]);
                    error = Some(format!(
                        "Invalid character ('{ch_text}') in a variable name: '{partial}'"
                    ));
                    fatal = true;
                    break;
                }
            }
            index += 1;
        }
        if error.is_none() && !open.is_empty() {
            error = Some("There is an unterminated variable reference.".into());
        }
        if let Some(message) = error {
            let _ = fatal;
            let mut text = String::from("Syntax error in cmake code ");
            if let Some((file, _)) = location {
                text.push_str(&format!("at\n  {file}:{line}\n"));
            }
            text.push_str(&format!("when parsing string\n  {source}\n{message}"));
            return Err((MessageType::FatalError, text));
        }
        result.extend_from_slice(&bytes[last.min(bytes.len())..]);
        Ok(crate::bytes::decode(&result))
    }
}

enum BodyResult {
    Error,
    Ctl(Ctl),
}

impl Interp {
    /// Registers a command that controls when its arguments are expanded.
    pub fn register_raw(&mut self, name: &str, function: RawBuiltin) {
        self.commands
            .insert(name.to_owned(), CommandDef::Raw(function));
    }

    /// Registers a builtin command.
    pub fn register(&mut self, name: &str, function: Builtin) {
        self.commands
            .insert(name.to_owned(), CommandDef::Builtin(function));
    }

    pub fn register_unexpected(&mut self, name: &str, message: &'static str) {
        self.commands
            .insert(name.to_owned(), CommandDef::Unexpected(message));
    }

    pub fn register_flow(&mut self, name: &str) {
        self.commands.insert(name.to_owned(), CommandDef::Flow);
    }

    /// Calls a command by name with already-expanded arguments
    /// (`cmake_language(CALL)`), using the current backtrace frame.
    pub fn call_command(&mut self, name: &str, args: &[String]) -> bool {
        let lname = name.to_ascii_lowercase();
        let Some(def) = self.commands.get(&lname).cloned() else {
            let message = format!("Unknown CMake command \"{name}\".");
            self.issue(MessageType::FatalError, &message);
            self.set_fatal();
            return false;
        };
        match def {
            CommandDef::Builtin(function) => match function(self, args) {
                Ok(()) => true,
                Err(error) => {
                    if let CmdErr::Error(message) = error {
                        let text = format!("{name} {message}");
                        self.issue(MessageType::FatalError, &text);
                    }
                    self.command_failed();
                    false
                }
            },
            CommandDef::Unexpected(message) => {
                self.issue(MessageType::FatalError, &format!("{name} {message}"));
                self.command_failed();
                false
            }
            CommandDef::User(callable) => self.invoke_callable(&callable, args, name).0,
            CommandDef::Raw(_) | CommandDef::Flow => {
                // Build a synthetic command so flow commands work through CALL.
                let command = Command {
                    name: name.to_owned(),
                    lname,
                    line: self.backtrace.last().map_or(0, |frame| frame.line),
                    line_end: 0,
                    args: args
                        .iter()
                        .map(|value| Arg {
                            value: value.clone(),
                            delim: Delim::Bracket,
                            line: 0,
                        })
                        .collect(),
                };
                let file = self
                    .backtrace
                    .last()
                    .map(|frame| frame.file.clone())
                    .unwrap_or_else(|| Rc::from(""));
                let parsed = Rc::new(ParsedFile::new(file, vec![command]));
                let command = parsed.commands[0].clone();
                self.execute(&parsed, &command, None).0
            }
        }
    }

    pub fn recursion_depth_now(&self) -> usize {
        self.recursion_depth + self.recursion_depth_base
    }

    pub fn has_command(&self, name: &str) -> bool {
        self.commands.contains_key(&name.to_ascii_lowercase())
    }
}

// ---- include() helpers (langcore) ----

impl Interp {
    /// `cmMakefile::PopSnapshot` for the `NO_POLICY_SCOPE` case of
    /// `include()`: `cmMakefile::ReadDependentFile` only calls
    /// `end_policy_scope` when it pushed a policy frame itself, but
    /// `~IncludeScope` always runs `PopSnapshot`, which pops policy
    /// scopes the included file left open and reports
    /// "cmake_policy PUSH without matching POP". That diagnostic is
    /// issued while the included file is still the top backtrace entry
    /// (`Backtrace.Pop()` is the last step of `~IncludeScope`), so the
    /// frame is reconstructed here: `read_dependent_file` has already
    /// popped it by the time `include()` regains control.
    pub fn include_policy_scope_cleanup(&mut self, path: &str, depth: usize) {
        if self.mf().policies.len() <= depth {
            return;
        }
        if !self.fatal {
            let mut backtrace = self.backtrace.clone();
            backtrace.push(Frame {
                file: Rc::from(path),
                line: 0,
                name: None,
                deferred: false,
            });
            self.issue_at(
                MessageType::FatalError,
                "cmake_policy PUSH without matching POP",
                &backtrace,
            );
        }
        self.mf_mut().policies.truncate(depth);
    }

    /// `cmGlobalGenerator::IsExportedTargetsFile`: `path` names a file
    /// registered by export() as a build-export file, which CMP0024
    /// forbids passing to include(). Files generated for an export set
    /// (`export(EXPORT ...)`) are exempt; `BuildExport::set` records which
    /// kind was requested (`AddBuildExportExportSet` vs
    /// `AddBuildExportSet` in cmExportCommand.cxx).
    pub fn is_exported_targets_file(&self, path: &str) -> bool {
        self.ext
            .install
            .build_exports
            .get(path)
            .is_some_and(|export| export.set.is_none())
    }

    /// `cmMakefile::ConfigureSubDirectory` followed by the list-file part
    /// of `cmMakefile::Configure` for the child directory. The child
    /// inherits the parent's variable scope through the scope stack, so
    /// `read_list_file_directory` derives the same CMAKE_CURRENT_LIST_FILE /
    /// CMAKE_PARENT_LIST_FILE state as `BuildsystemFileScope` + the explicit
    /// `AddDefinition(kCMAKE_PARENT_LIST_FILE, ...)` in `cmMakefile::Configure`:
    /// both name the child's own list file while it executes, and the
    /// inherited values are restored once it finishes.
    pub fn configure_subdirectory_dirscope(
        &mut self,
        index: usize,
        system: bool,
        inner_scopes: &[u64],
    ) -> bool {
        let parent = self.mf_index();
        self.save_directory_definitions(parent);
        let flattened = self.effective_policies();
        // Directory properties inherited at configure (InitializeFromParent).
        let parent_props = self.makefiles[parent].props.clone();
        for key in [
            "INCLUDE_DIRECTORIES",
            "COMPILE_DEFINITIONS",
            "COMPILE_OPTIONS",
            "LINK_OPTIONS",
            "LINK_DIRECTORIES",
            "INCLUDE_REGULAR_EXPRESSION",
        ] {
            if let Some(value) = parent_props.get(key) {
                self.makefiles[index].props.set(key, value.to_owned());
            }
        }
        for (key, value) in parent_props.iter() {
            if key.starts_with("COMPILE_DEFINITIONS_") {
                self.makefiles[index].props.set(key, value.to_owned());
            }
        }
        self.makefiles[index].policies = vec![PolicyFrame {
            map: flattened,
            weak: false,
        }];
        crate::commands::directory::initialize_subdirectory(self, parent, index, system);
        // Freeze the parent's own backtrace (its stack still holds the
        // add_subdirectory frame) for diagnostics issued on its behalf
        // while the child configures (cmMakefile::Backtrace).
        self.makefiles[parent].backtrace = self.backtrace.clone();
        let source = self.makefiles[index].source_dir.clone();
        let binary = self.makefiles[index].binary_dir.clone();
        if self.debug.output {
            self.system_message(&format!("   Entering             {source}"));
        }
        self.mf_stack.push(index);
        // cmMakefile::InitializeFromParent copies RecursionDepth; the new
        // makefile's ExecutionStatusStack starts empty.
        self.trace_frame_base.push(self.recursion_depth);
        self.push_scope();
        self.directory_scope_depths.push(self.scopes.len());
        self.apply_captured_scopes(inner_scopes);
        self.set_def("CMAKE_CURRENT_SOURCE_DIR", &source);
        self.set_def("CMAKE_CURRENT_BINARY_DIR", &binary);
        // The child's backtrace starts at its own list file, independent of
        // the parent's call stack (cmMakefile::Configure).
        let saved_backtrace = std::mem::take(&mut self.backtrace);
        let list = self.project_list_file(&source);
        let ok = self.read_list_file_directory(&list);
        self.backtrace = saved_backtrace;
        self.save_directory_definitions(index);
        self.record_directory_vars();
        self.directory_scope_depths.pop();
        self.pop_scope();
        self.trace_frame_base.pop();
        self.mf_stack.pop();
        if self.debug.output {
            let parent_source = self.mf().source_dir.clone();
            self.system_message(&format!("   Returning to         {parent_source}"));
        }
        ok
    }
}

/// `cmStrToULong`: skips leading `isspace`, rejects `-`, parses a base-10
/// `strtoul` that must consume the whole string without overflow.
fn str_to_ulong(text: &str) -> Option<u64> {
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() && matches!(bytes[index], b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
    {
        index += 1;
    }
    if bytes.get(index) == Some(&b'-') {
        return None;
    }
    if bytes.get(index) == Some(&b'+') {
        index += 1;
    }
    let digits = index;
    let mut value: u64 = 0;
    while index < bytes.len() && bytes[index].is_ascii_digit() {
        value = value
            .checked_mul(10)?
            .checked_add(u64::from(bytes[index] - b'0'))?;
        index += 1;
    }
    if index == digits || index != bytes.len() {
        return None;
    }
    Some(value)
}

/// C `atof`: parses the longest valid floating-point prefix of `text`
/// (leading `isspace` skipped, no digits yields 0).
fn atof_prefix(text: &str) -> f64 {
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() && matches!(bytes[index], b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
    {
        index += 1;
    }
    let start = index;
    if matches!(bytes.get(index), Some(b'+') | Some(b'-')) {
        index += 1;
    }
    let mut digits = 0;
    while bytes.get(index).is_some_and(u8::is_ascii_digit) {
        index += 1;
        digits += 1;
    }
    if bytes.get(index) == Some(&b'.') {
        index += 1;
        while bytes.get(index).is_some_and(u8::is_ascii_digit) {
            index += 1;
            digits += 1;
        }
    }
    if digits == 0 {
        return 0.0;
    }
    if matches!(bytes.get(index), Some(b'e') | Some(b'E')) {
        let mut end = index + 1;
        if matches!(bytes.get(end), Some(b'+') | Some(b'-')) {
            end += 1;
        }
        let exp_start = end;
        while bytes.get(end).is_some_and(u8::is_ascii_digit) {
            end += 1;
        }
        if end > exp_start {
            index = end;
        }
    }
    text[start..index].parse::<f64>().unwrap_or(0.0)
}

/// `cmListFileArgument::operator==`: value and delimiter kind, ignoring the
/// line number.
fn raw_args_equal(a: &[Arg], b: &[Arg]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b.iter())
            .all(|(x, y)| x.value == y.value && x.delim == y.delim)
}
