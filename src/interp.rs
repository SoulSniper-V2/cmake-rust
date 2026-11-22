//! The CMake language interpreter core: variable scopes, policies,
//! directories, the command table, control flow, variable expansion, and
//! diagnostics. Ports `cmMakefile`, `cmStateSnapshot`/`cmDefinitions`,
//! `cmFunctionCommand`, `cmMacroCommand`, `cmIfCommand`, `cmForEachCommand`,
//! `cmWhileCommand`, `cmBlockCommand` and `cmReturnCommand` semantics.

use std::collections::HashMap;
use std::rc::Rc;

use crate::cache::{Cache, CacheType};
use crate::lexer::{self, Arg, Command, Delim};
use crate::messages::{self, Frame, MessageType};
use crate::policy::{PolicyMap, PolicyStatus};
use crate::props::Props;
use crate::util;

pub const VERSION: (u32, u32, u32) = (4, 1, 0);

type ParseKey = (String, u64, i64, i64);

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
pub type RawBuiltin = fn(&mut Interp, &Rc<ParsedFile>, &Command, Option<&MacroSubst>) -> (bool, Ctl);

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
}

#[derive(Debug, Clone)]
pub struct PolicyFrame {
    pub map: PolicyMap,
    pub weak: bool,
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
    pub targets: Vec<String>,
    pub children: Vec<usize>,
    pub exclude_from_all: bool,
    /// Deferred calls registered with cmake_language(DEFER).
    pub deferred: Vec<Deferred>,
    pub defer_counter: usize,
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
}

pub struct Interp {
    pub mode: Mode,
    pub cache: Cache,
    scopes: Vec<Scope>,
    pub backtrace: Vec<Frame>,
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
    /// Set by builtins that end the current file/function (include_guard).
    pub request_return: bool,
    /// Configuring a try_compile project.
    pub in_try_compile: bool,
    /// `--debug-trycompile`: keep try_compile directories and log events.
    pub debug_trycompile: bool,
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
            backtrace: Vec::new(),
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
            request_return: false,
            in_try_compile: false,
            debug_trycompile: false,
            root_check_cmp0000: false,
            langs: crate::languages::LangState::default(),
            recursion_depth_base: 0,
            dir_final_vars: Vec::new(),
            top_source_root: None,
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
            targets: Vec::new(),
            children: Vec::new(),
            exclude_from_all: false,
            deferred: Vec::new(),
            defer_counter: 0,
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

    /// Reads an expanded variable and invokes registered watch callbacks.
    /// Read the binding again after the callback, since it can change scope state.
    pub fn read_definition(&mut self, name: &str) -> Option<String> {
        let value = self.get_def(name).map(str::to_owned);
        if !self.ext.variable_watches.contains_key(name) {
            return value;
        }
        let access = if value.is_some() { "READ_ACCESS" } else { "UNKNOWN_READ_ACCESS" };
        crate::commands::sysinfo::notify_variable(self, name, access, value.as_deref().unwrap_or_default());
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
            let message = format!(
                "Cannot set \"{name}\": current scope has no parent."
            );
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
        crate::commands::sysinfo::notify_variable(self, name, "MODIFIED_ACCESS", value.unwrap_or_default());
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
        self.scopes.push(Scope::default());
    }

    pub fn pop_scope(&mut self) {
        self.scopes.pop();
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
    pub fn add_cache_def(&mut self, name: &str, value: Option<&str>, help: Option<&str>, kind: CacheType, force: bool) {
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
                        if item.is_empty() || util::is_full_path(&item) {
                            item
                        } else {
                            util::collapse_full_path(&item, "")
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
                    && self.is_on("CMAKE_POLICY_WARNING_CMP0126")
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

    pub fn run_subdirectory_with_system(&mut self, source: &str, binary: &str, exclude_from_all: bool, system: bool) -> bool {
        let index = self.create_subdirectory(source, binary, exclude_from_all);
        self.configure_subdirectory(index, system)
    }

    /// `cmMakefile::AddSubDirectory`: the child cmMakefile (and its place in
    /// the parent's children and the global makefile order) is created when
    /// the directory is added, even when its configure is deferred.
    pub fn create_subdirectory(&mut self, source: &str, binary: &str, exclude_from_all: bool) -> usize {
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
            targets: Vec::new(),
            children: Vec::new(),
            exclude_from_all: exclude,
            deferred: Vec::new(),
            defer_counter: 0,
        });
        self.makefiles[parent].children.push(index);
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
        let source = self.makefiles[index].source_dir.clone();
        let binary = self.makefiles[index].binary_dir.clone();
        self.mf_stack.push(index);
        self.push_scope();
        self.set_def("CMAKE_CURRENT_SOURCE_DIR", &source);
        self.set_def("CMAKE_CURRENT_BINARY_DIR", &binary);
        // The child's backtrace starts at its own list file, independent of
        // the parent's call stack (cmMakefile::Configure).
        let saved_backtrace = std::mem::take(&mut self.backtrace);
        let list = format!("{source}/CMakeLists.txt");
        let ok = self.read_list_file_directory(&list);
        self.backtrace = saved_backtrace;
        self.save_directory_definitions(index);
        self.record_directory_vars();
        self.pop_scope();
        self.mf_stack.pop();
        ok
    }

    fn save_directory_definitions(&mut self, index: usize) {
        let definitions = self.normal_definition_names().into_iter()
            .filter_map(|name| self.get_normal_def(&name).map(|value| (name, value.to_owned())))
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
        let try_compile_exempt = self.in_try_compile && matches!(index, 83 | 91 | 104 | 123 | 126 | 128 | 136 | 141);
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

    pub fn pop_policy(&mut self) -> bool {
        if self.mf().policies.len() <= 1 {
            self.issue(MessageType::FatalError, "cmake_policy POP without matching PUSH");
            return false;
        }
        self.mf_mut().policies.pop();
        true
    }

    // ------------------------------------------------------------------
    // Diagnostics

    pub fn top_source(&self) -> Option<&str> {
        // cmake::SetHomeDirectory sets the messenger's top source for both
        // project and script mode (normal project kind).
        self.top_source_root.as_deref().or(Some(&self.home_source))
    }

    /// `cmMakefile::IssueMessage` at the current backtrace.
    pub fn issue(&mut self, kind: MessageType, text: &str) {
        let backtrace = self.backtrace.clone();
        self.issue_at(kind, text, &backtrace);
    }

    pub fn issue_at(&mut self, kind: MessageType, text: &str, backtrace: &[Frame]) {
        let kind = match kind {
            MessageType::AuthorWarning | MessageType::AuthorError => {
                if self.dev_warnings_as_errors {
                    MessageType::AuthorError
                } else {
                    if self.suppress_dev_warnings {
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
        let mut rendered = messages::render(kind, text, backtrace, self.top_source());
        // A deferred execution placeholder denotes a location, even though it
        // has no numeric line. Other contexts without a line use "in".
        if backtrace.last().is_some_and(|frame| frame.deferred)
            && let Some(index) = rendered.find(" in ")
        {
            rendered.replace_range(index..index + 4, " at ");
        }
        if messages::is_error(kind) {
            self.error = true;
        }
        self.stderr_line(&rendered);
    }

    /// Displays a diagnostic whose category was already selected by the caller.
    pub fn issue_at_unsuppressed(&mut self, kind: MessageType, text: &str) {
        let rendered = messages::render(kind, text, &self.backtrace, self.top_source());
        if messages::is_error(kind) {
            self.error = true;
        }
        self.stderr_line(&rendered);
    }

    /// Writes one line to stderr (or to `stderr_capture`).
    pub fn stderr_line(&mut self, text: &str) {
        match &mut self.stderr_capture {
            Some(capture) => {
                capture.push_str(text);
                capture.push('\n');
            }
            None => eprintln!("{text}"),
        }
    }

    /// `cmSystemTools::Message` to stdout without prefix.
    pub fn stdout_line_raw(&mut self, text: &str) {
        self.stdout_line(text);
    }

    /// Runs the top-level CMakeLists.txt with the root-directory rules of
    /// `cmMakefile::Configure` (CMP0000 detection and implicit project()).
    pub fn run_root_list_file(&mut self, path: &str) {
        self.backtrace.push(Frame {
            file: Rc::from(path),
            line: 0,
            name: None,
            deferred: false,
        });
        self.set_def("CMAKE_PARENT_LIST_FILE", path);
        let saved = self.push_list_file_vars(path);
        if let Some(parsed) = self.parse_file(path) {
            let mut commands = parsed.commands.clone();
            let has_version = commands.iter().any(|c| c.lname == "cmake_minimum_required");
            if !has_version {
                const ALLOWED: [&str; 11] = [
                    "project", "set", "if", "endif", "else", "elseif", "add_executable",
                    "add_library", "target_link_libraries", "option", "message",
                ];
                let problem = commands.len() >= 30
                    || commands.iter().any(|c| !ALLOWED.contains(&c.lname.as_str()));
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
            self.issue(MessageType::FatalError, "cmake_policy PUSH without matching POP");
            self.mf_mut().policies.truncate(1);
        }
        self.pop_list_file_vars(saved);
        self.backtrace.pop();
    }

    /// Writes a line to stdout (message STATUS etc.).
    pub fn stdout_line(&mut self, text: &str) {
        use std::io::Write;
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "{text}");
        let _ = out.flush();
    }

    pub fn set_fatal(&mut self) {
        self.fatal = true;
        self.error = true;
    }

    // ------------------------------------------------------------------
    // List files

    fn parse_file(&mut self, path: &str) -> Option<Rc<ParsedFile>> {
        // Cache parses across interpreter instances (try_compile projects
        // re-read the same modules), keyed by path, size and mtime so that
        // rewritten files are re-parsed.
        let meta = std::fs::metadata(path).ok()?;
        use std::os::unix::fs::MetadataExt;
        let key = (path.to_owned(), meta.len(), meta.mtime(), meta.mtime_nsec());
        if let Some(parsed) = PARSE_CACHE.with(|cache| cache.borrow().get(&key).cloned()) {
            return Some(parsed);
        }
        let bytes = std::fs::read(path).ok()?;
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let rc_path: Rc<str> = Rc::from(path);
        let parsed = self.parse_text(rc_path, &text)?;
        PARSE_CACHE.with(|cache| cache.borrow_mut().insert(key, parsed.clone()));
        Some(parsed)
    }

    pub fn parse_text(&mut self, path: Rc<str>, text: &str) -> Option<Rc<ParsedFile>> {
        let (commands, diags) = lexer::parse(&path, text);
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
            if !diag.warning {
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
        if !no_policy_scope {
            self.push_policy(false, PolicyMap::default());
        }
        let saved = self.push_list_file_vars(&path);
        let parsed = self.parse_file(&path);
        let ok = parsed.is_some();
        if let Some(parsed) = parsed {
            self.mf_mut().list_files.push(path.clone());
            self.run_file_body(&parsed);
        }
        self.pop_list_file_vars(saved);
        if !no_policy_scope {
            self.end_policy_scope(depth_before);
        }
        self.backtrace.pop();
        ok
    }

    /// Pops policy frames down to `depth`, diagnosing unbalanced PUSH.
    fn end_policy_scope(&mut self, depth: usize) {
        let current = self.mf().policies.len();
        if current > depth + 1 && !self.fatal {
            self.issue(MessageType::FatalError, "cmake_policy PUSH without matching POP");
        }
        self.mf_mut().policies.truncate(depth);
    }

    /// Reads the directory's CMakeLists.txt (top-level or subdirectory).
    pub fn read_list_file_directory(&mut self, path: &str) -> bool {
        self.backtrace.push(Frame {
            file: Rc::from(path),
            line: 0,
            name: None,
            deferred: false,
        });
        let saved = self.push_list_file_vars(path);
        let parsed = self.parse_file(path);
        let ok = parsed.is_some();
        if let Some(parsed) = parsed {
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
            self.issue(MessageType::FatalError, "cmake_policy PUSH without matching POP");
            self.mf_mut().policies.truncate(1);
        }
        self.pop_list_file_vars(saved);
        self.backtrace.pop();
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
        let parsed = self.parse_file(path);
        let ok = parsed.is_some();
        if let Some(parsed) = parsed {
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
            let parsed = Rc::new(ParsedFile::new(deferred.file.clone(), vec![deferred.command]));
            self.ext.defer_current_id = Some(deferred.id);
            self.loop_depth.push(0);
            self.run_range(&parsed, 0, 1, None);
            self.loop_depth.pop();
            self.ext.defer_current_id = None;
            self.ext.execution_files.pop();
            if self.mf().policies.len() > depth {
                if !self.fatal {
                    self.issue(MessageType::FatalError, "cmake_policy PUSH without matching POP");
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
            let ctl = match command.lname.as_str() {
                "if" if self.is_flow("if") => {
                    let ctl = self.run_if(file, index, subst);
                    index = *jumps.last().expect("endif");
                    ctl
                }
                "foreach" if self.is_flow("foreach") => {
                    let ctl = self.run_foreach(file, index, subst);
                    index = jumps[0];
                    ctl
                }
                "while" if self.is_flow("while") => {
                    let ctl = self.run_while(file, index, subst);
                    index = jumps[0];
                    ctl
                }
                "function" | "macro" if self.is_flow(&command.lname) => {
                    self.define_callable(file, index, subst);
                    index = jumps[0];
                    Ctl::Normal
                }
                "block" if self.is_flow("block") => {
                    let ctl = self.run_block(file, index, subst);
                    index = jumps[0];
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

    fn frame_for(&self, file: &Rc<ParsedFile>, command: &Command) -> Frame {
        Frame {
            file: file.path.clone(),
            line: command.line,
            name: Some(Rc::from(command.name.as_str())),
            deferred: false,
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
    pub fn execute(&mut self, file: &Rc<ParsedFile>, command: &Command, subst: Option<&MacroSubst>) -> (bool, Ctl) {
        self.backtrace.push(self.frame_for(file, command));
        let result = self.execute_inner(file, command, subst);
        self.backtrace.pop();
        result
    }

    fn execute_inner(&mut self, file: &Rc<ParsedFile>, command: &Command, subst: Option<&MacroSubst>) -> (bool, Ctl) {
        self.recursion_depth += 1;
        let depth = self.recursion_depth + self.recursion_depth_base;
        let limit = self
            .get_def("CMAKE_MAXIMUM_RECURSION_DEPTH")
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(1000);
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

    fn dispatch(&mut self, file: &Rc<ParsedFile>, command: &Command, subst: Option<&MacroSubst>) -> (bool, Ctl) {
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
        match def {
            CommandDef::Flow => self.run_flow_command(file, command, subst),
            CommandDef::Raw(function) => function(self, file, command, subst),
            CommandDef::Unexpected(message) => {
                self.issue(MessageType::FatalError, &format!("{} {message}", command.name));
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
        if self.mode == Mode::Script {
            self.set_fatal();
        }
    }

    /// Invokes a user function or macro with already-expanded arguments.
    pub fn invoke_callable(&mut self, callable: &Rc<Callable>, args: &[String], _call_name: &str) -> (bool, Ctl) {
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
        self.ext.execution_files.push(callable.file.path.to_string());
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
                subst.argv.push((format!("${{ARGV{index}}}"), value.clone()));
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
            self.set_def("CMAKE_CURRENT_FUNCTION_LIST_FILE", callable.file.path.to_string());
            self.set_def(
                "CMAKE_CURRENT_FUNCTION_LIST_DIR",
                util::filename_path(&callable.file.path),
            );
            self.set_def("CMAKE_CURRENT_FUNCTION_LIST_LINE", callable.line.to_string());
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
            let (ok, ctl) = match command.lname.as_str() {
                "if" if self.is_flow("if") => {
                    let ctl = self.run_if(&file, index, subst);
                    index = *jumps.last().expect("endif");
                    (true, ctl)
                }
                "foreach" if self.is_flow("foreach") => {
                    let ctl = self.run_foreach(&file, index, subst);
                    index = jumps[0];
                    (true, ctl)
                }
                "while" if self.is_flow("while") => {
                    let ctl = self.run_while(&file, index, subst);
                    index = jumps[0];
                    (true, ctl)
                }
                "function" | "macro" if self.is_flow(&command.lname) => {
                    self.define_callable(&file, index, subst);
                    index = jumps[0];
                    (true, Ctl::Normal)
                }
                "block" if self.is_flow("block") => {
                    let ctl = self.run_block(&file, index, subst);
                    index = jumps[0];
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

    fn define_callable(&mut self, file: &Rc<ParsedFile>, index: usize, subst: Option<&MacroSubst>) {
        let command = &file.commands[index];
        let end = file.jumps[index][0];
        self.backtrace.push(self.frame_for(file, command));
        let args = self.expand_arguments(&file.path, &command.args, subst);
        let is_macro = command.lname == "macro";
        let Some(args) = args else {
            self.backtrace.pop();
            return;
        };
        if args.is_empty() {
            let text = format!("{} called with incorrect number of arguments", command.name);
            self.issue(MessageType::FatalError, &text);
            self.command_failed();
            self.backtrace.pop();
            return;
        }
        let values: Vec<String> = args.into_iter().map(|(value, _)| value).collect();
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
                (Rc::new(ParsedFile::new(file.path.clone(), commands)), 0, count)
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
        self.add_user_command(&values[0], callable);
        self.backtrace.pop();
    }

    /// `cmState::AddScriptedCommand`: overriding renames the previous
    /// definition to `_<name>`.
    pub fn add_user_command(&mut self, name: &str, callable: Rc<Callable>) {
        let lname = name.to_ascii_lowercase();
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

    fn run_flow_command(&mut self, file: &Rc<ParsedFile>, command: &Command, subst: Option<&MacroSubst>) -> (bool, Ctl) {
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
                if values[0] != "PROPAGATE" {
                    let text = format!(
                        "{} called with unsupported argument \"{}\"",
                        command.name, values[0]
                    );
                    self.issue(MessageType::FatalError, &text);
                    self.command_failed();
                    return (false, Ctl::Normal);
                }
                if self.policy_status("CMP0140") != PolicyStatus::New {
                    return (true, Ctl::Return(None));
                }
                (true, Ctl::Return(Some(values[1..].to_vec())))
            }
            "else" | "elseif" | "endif" | "endforeach" | "endwhile" | "endfunction" | "endmacro" | "endblock" => {
                let message = format!("Flow control statements are not properly nested.");
                self.issue(MessageType::FatalError, &message);
                self.set_fatal();
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
        // Evaluate the `if` condition.
        self.backtrace.push(self.frame_for(file, command));
        let mut taken = self.evaluate_if_condition(file, command, subst, "if");
        self.backtrace.pop();
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

    fn evaluate_if_condition(&mut self, file: &Rc<ParsedFile>, command: &Command, subst: Option<&MacroSubst>, name: &str) -> bool {
        let Some(args) = self.expand_arguments(&file.path, &command.args, subst) else {
            return false;
        };
        let (result, error) = crate::condition::evaluate(self, &args);
        if let Some((kind, message)) = error {
            let mut text = format!("{name} given arguments:\n ");
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

    fn run_foreach(&mut self, file: &Rc<ParsedFile>, index: usize, subst: Option<&MacroSubst>) -> Ctl {
        let end = file.jumps[index][0];
        let command = &file.commands[index];
        self.backtrace.push(self.frame_for(file, command));
        let Some(args) = self.expand_arguments(&file.path, &command.args, subst) else {
            self.backtrace.pop();
            return Ctl::Normal;
        };
        let values: Vec<String> = args.into_iter().map(|(value, _)| value).collect();
        let plan = crate::flow::foreach_plan(self, &values);
        self.backtrace.pop();
        let Some(plan) = plan else {
            return Ctl::Normal;
        };
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
                return Ctl::Normal;
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
        let saved = if plan.iterations.is_empty() { Vec::new() } else { saved };
        for (name, value) in saved {
            match value {
                Some(value) => self.set_def(name, value),
                None => self.unset_def(&name),
            }
        }
        result
    }

    fn run_while(&mut self, file: &Rc<ParsedFile>, index: usize, subst: Option<&MacroSubst>) -> Ctl {
        let end = file.jumps[index][0];
        let command = &file.commands[index];
        let max_iterations: usize = 1_000_000_000;
        let mut count = 0usize;
        *self.loop_depth.last_mut().expect("loop depth") += 1;
        let mut result = Ctl::Normal;
        loop {
            self.backtrace.push(self.frame_for(file, command));
            let condition = self.evaluate_if_condition(file, command, subst, "while");
            self.backtrace.pop();
            if !condition || self.fatal {
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
        result
    }

    fn run_block(&mut self, file: &Rc<ParsedFile>, index: usize, subst: Option<&MacroSubst>) -> Ctl {
        let end = file.jumps[index][0];
        let command = &file.commands[index];
        self.backtrace.push(self.frame_for(file, command));
        let Some(args) = self.expand_arguments(&file.path, &command.args, subst) else {
            self.backtrace.pop();
            return Ctl::Normal;
        };
        let values: Vec<String> = args.into_iter().map(|(value, _)| value).collect();
        let options = match crate::flow::parse_block(&values) {
            Ok(options) => options,
            Err(message) => {
                let text = format!("{} {}", command.name, message);
                self.issue(MessageType::FatalError, &text);
                self.command_failed();
                self.backtrace.pop();
                return Ctl::Normal;
            }
        };
        self.backtrace.pop();
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
            }
            self.pop_scope();
        }
        if options.policies {
            self.end_policy_scope(policy_depth);
        }
        ctl
    }

    // ------------------------------------------------------------------
    // Variable expansion (port of cmMakefile::ExpandVariablesInStringImpl)

    /// Expands `${}`, `$ENV{}`, `$CACHE{}` references and escape sequences.
    /// `location` provides the file/line for CMAKE_CURRENT_LIST_LINE and
    /// error messages. Errors are reported (fatal) and the input is returned.
    pub fn expand_variables(&mut self, source: &str, location: Option<(&str, usize)>, at_only: bool, replace_at: bool) -> String {
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
    pub fn expand_configure(&mut self, source: &str, at_only: bool, escape_quotes: bool) -> Result<String, String> {
        let frame = self.backtrace.last().map(|frame| (frame.file.clone(), frame.line));
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
        let bytes = source.as_bytes();
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
                        let lookup = String::from_utf8_lossy(&result[position..]).into_owned();
                        let value: Option<String> = match domain {
                            b'N' => {
                                if location.is_some() && lookup == "CMAKE_CURRENT_LIST_LINE" {
                                    let deferred = self.backtrace.iter().rev().nth(1).is_some_and(|frame| frame.deferred);
                                    match &self.ext.defer_current_id {
                                        Some(id) if deferred => Some(format!("DEFERRED:{id}")),
                                        _ => Some(line.to_string()),
                                    }
                                } else {
                                    self.read_definition(&lookup)
                                }
                            }
                            b'E' => std::env::var_os(&lookup)
                                .map(|value| value.to_string_lossy().into_owned()),
                            _ => self.cache.get(&lookup).map(|entry| entry.value.clone()),
                        };
                        let value = value.unwrap_or_default();
                        let value = if escape_quotes {
                            value.replace('"', "\\\"")
                        } else {
                            value
                        };
                        result.truncate(position);
                        result.extend_from_slice(value.as_bytes());
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
                                String::from_utf8_lossy(&rest[..name_len])
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
                            let ch = source[index + 1..].chars().next().unwrap_or('?');
                            message.push(ch);
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
                            byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'_' | b'.' | b'+' | b'-')
                        })
                    {
                        let name = String::from_utf8_lossy(&rest[..close]).into_owned();
                        let value = if location.is_some() && name == "CMAKE_CURRENT_LIST_LINE" {
                            line.to_string()
                        } else {
                            self.read_definition(&name).unwrap_or_default()
                        };
                        let value = if escape_quotes {
                            value.replace('"', "\\\"")
                        } else {
                            value
                        };
                        result.extend_from_slice(&bytes[last..index]);
                        result.extend_from_slice(value.as_bytes());
                        index = index + 1 + close;
                        last = index + 1;
                    } else {
                        handled_default = true;
                    }
                }
                _ => handled_default = true,
            }
            if handled_default && !open.is_empty() {
                let ok = ch.is_ascii_alphanumeric() || matches!(ch, b'_' | b'/' | b'.' | b'+' | b'-');
                if !ok {
                    let ch_text = source[index..].chars().next().unwrap_or('?');
                    result.extend_from_slice(&bytes[last..index]);
                    let partial = String::from_utf8_lossy(&result[open.last().unwrap().1..]).into_owned();
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
        Ok(String::from_utf8_lossy(&result).into_owned())
    }
}

enum BodyResult {
    Error,
    Ctl(Ctl),
}

impl Interp {
    /// Registers a command that controls when its arguments are expanded.
    pub fn register_raw(&mut self, name: &str, function: RawBuiltin) {
        self.commands.insert(name.to_owned(), CommandDef::Raw(function));
    }

    /// Registers a builtin command.
    pub fn register(&mut self, name: &str, function: Builtin) {
        self.commands.insert(name.to_owned(), CommandDef::Builtin(function));
    }

    pub fn register_unexpected(&mut self, name: &str, message: &'static str) {
        self.commands.insert(name.to_owned(), CommandDef::Unexpected(message));
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
