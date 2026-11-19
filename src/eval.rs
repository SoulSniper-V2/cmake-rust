use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use crate::cache::{Cache, CacheEntry};
use crate::graph::{Graph, Tools};
use crate::syntax::{self, Argument, Command, Form, split_list};
use crate::{Result, fail};

mod arithmetic;
mod commands;
mod condition;
mod find;
mod modules;
mod parse_arguments;
mod probe;
mod process;
mod targets;

#[derive(Clone, Debug)]
pub struct Directory {
    pub source: PathBuf,
    pub binary: PathBuf,
    pub includes: Vec<crate::graph::Setting>,
    pub definitions: Vec<crate::graph::Setting>,
    pub properties: BTreeMap<String, String>,
}

#[derive(Clone, Debug)]
struct Callable {
    parameters: Vec<String>,
    body: Vec<Command>,
    scoped: bool,
    policies: BTreeMap<String, String>,
}

#[derive(Debug)]
pub struct Interpreter {
    pub graph: Graph,
    pub tools: Tools,
    pub config: String,
    directories: Vec<Directory>,
    files: Vec<PathBuf>,
    scopes: Vec<BTreeMap<String, String>>,
    cache: Cache,
    policies: BTreeMap<String, String>,
    policy_stack: Vec<BTreeMap<String, String>>,
    policy_barrier: usize,
    functions: BTreeMap<String, Callable>,
    macros: Vec<BTreeMap<String, String>>,
    function_args: Vec<Vec<String>>,
    native_modules: BTreeSet<String>,
    global_properties: BTreeMap<String, String>,
    include_guards: BTreeSet<(PathBuf, Option<PathBuf>)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Flow {
    Continue,
    Break,
    Next,
    Return,
}

impl Interpreter {
    pub fn new(source: PathBuf, build: PathBuf, mut cache: Cache, tools: Tools) -> Result<Self> {
        cache
            .entry("CMAKE_INSTALL_PREFIX".into())
            .or_insert_with(|| {
                CacheEntry::new(
                    "/usr/local",
                    "PATH",
                    "Install path prefix, prepended onto install directories.",
                )
            });
        let system = if cfg!(target_os = "macos") {
            "Darwin"
        } else {
            "Linux"
        };
        let processor = if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
            "arm64"
        } else {
            env::consts::ARCH
        };
        let exe = env::current_exe()?.display().to_string();
        let make_program = cache
            .get("CMAKE_MAKE_PROGRAM")
            .map(|entry| entry.value.clone())
            .unwrap_or_else(|| "ninja".into());
        let mut root = BTreeMap::from([
            ("CMAKE_SOURCE_DIR".to_owned(), path_string(&source)),
            ("CMAKE_BINARY_DIR".to_owned(), path_string(&build)),
            ("CMAKE_COMMAND".to_owned(), exe),
            ("CMAKE_CTEST_COMMAND".to_owned(), "ctest".to_owned()),
            ("CMAKE_GENERATOR".to_owned(), "Ninja".to_owned()),
            ("CMAKE_MAKE_PROGRAM".to_owned(), make_program),
            ("CMAKE_SYSTEM_NAME".to_owned(), system.to_owned()),
            ("CMAKE_HOST_SYSTEM_NAME".to_owned(), system.to_owned()),
            ("CMAKE_SYSTEM_PROCESSOR".to_owned(), processor.to_owned()),
            (
                "CMAKE_HOST_SYSTEM_PROCESSOR".to_owned(),
                processor.to_owned(),
            ),
            ("CMAKE_VERSION".to_owned(), "3.20.0".to_owned()),
            ("UNIX".to_owned(), "1".to_owned()),
            ("CMAKE_HOST_UNIX".to_owned(), "1".to_owned()),
            ("CMAKE_CFG_INTDIR".to_owned(), ".".to_owned()),
            ("CMAKE_FILES_DIRECTORY".to_owned(), "/CMakeFiles".to_owned()),
        ]);
        if cfg!(target_os = "macos") {
            root.insert("APPLE".to_owned(), "1".to_owned());
            root.insert("CMAKE_HOST_APPLE".to_owned(), "1".to_owned());
        }
        let mut graph = Graph::default();
        graph.directories.insert(build.clone());
        Ok(Self {
            graph,
            tools,
            config: String::new(),
            directories: vec![Directory {
                source,
                binary: build,
                includes: Vec::new(),
                definitions: Vec::new(),
                properties: BTreeMap::new(),
            }],
            files: Vec::new(),
            scopes: vec![root],
            cache,
            policies: BTreeMap::new(),
            policy_stack: Vec::new(),
            policy_barrier: 0,
            functions: BTreeMap::new(),
            macros: Vec::new(),
            function_args: Vec::new(),
            native_modules: BTreeSet::new(),
            global_properties: BTreeMap::new(),
            include_guards: BTreeSet::new(),
        })
    }

    pub fn configure(&mut self) -> Result<()> {
        let list = self.directory().source.join("CMakeLists.txt");
        self.run_file(list)?;
        if !self.policy_stack.is_empty() {
            return fail("cmake_policy PUSH without matching POP");
        }
        self.config = self.get("CMAKE_BUILD_TYPE").unwrap_or_default();
        self.graph.install_prefix = self.get("CMAKE_INSTALL_PREFIX").unwrap_or_default();
        Ok(())
    }

    pub fn run_script(&mut self, script: PathBuf) -> Result<()> {
        self.set("CMAKE_SCRIPT_MODE_FILE", path_string(&script));
        self.run_file(script)?;
        if !self.policy_stack.is_empty() {
            return fail("cmake_policy PUSH without matching POP");
        }
        Ok(())
    }

    fn directory(&self) -> &Directory {
        self.directories.last().expect("root directory exists")
    }

    fn directory_mut(&mut self) -> &mut Directory {
        self.directories.last_mut().expect("root directory exists")
    }

    fn variables(&self) -> &BTreeMap<String, String> {
        self.scopes.last().expect("root scope exists")
    }

    fn variables_mut(&mut self) -> &mut BTreeMap<String, String> {
        self.scopes.last_mut().expect("root scope exists")
    }

    fn get(&self, name: &str) -> Option<String> {
        let dynamic = match name {
            "CMAKE_CURRENT_SOURCE_DIR" => Some(path_string(&self.directory().source)),
            "CMAKE_CURRENT_BINARY_DIR" => Some(path_string(&self.directory().binary)),
            "CMAKE_CURRENT_LIST_FILE" => self.files.last().map(|path| path_string(path)),
            "CMAKE_CURRENT_LIST_DIR" => self
                .files
                .last()
                .and_then(|path| path.parent())
                .map(path_string),
            "CMAKE_PARENT_LIST_FILE" => {
                self.files.iter().rev().nth(1).map(|path| path_string(path))
            }
            _ => None,
        };
        dynamic
            .or_else(|| self.variables().get(name).cloned())
            .or_else(|| self.cache_value(name))
    }

    fn cache_value(&self, name: &str) -> Option<String> {
        self.cache.get(name).map(|entry| entry.value.clone())
    }

    fn cache_set(
        &mut self,
        name: impl Into<String>,
        value: impl Into<String>,
        kind: &str,
        help: &str,
    ) {
        let entry = self
            .cache
            .entry(name.into())
            .or_insert_with(|| CacheEntry::new("", kind, help));
        entry.value = value.into();
        entry.kind = kind.to_owned();
        entry.help = help.to_owned();
    }

    fn set(&mut self, name: impl Into<String>, value: impl Into<String>) {
        self.variables_mut().insert(name.into(), value.into());
    }

    fn push_scope(&mut self) {
        self.scopes.push(self.variables().clone());
    }

    fn policy_scope<T>(&mut self, run: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        let saved = self.policies.clone();
        let depth = self.policy_stack.len();
        let barrier = std::mem::replace(&mut self.policy_barrier, depth);
        let result = run(self);
        let balanced = self.policy_stack.len() == depth;
        self.policy_stack.truncate(depth);
        self.policy_barrier = barrier;
        self.policies = saved;
        let value = result?;
        if !balanced {
            return fail("cmake_policy PUSH without matching POP");
        }
        Ok(value)
    }

    fn run_include(&mut self, path: PathBuf, no_policy_scope: bool) -> Result<()> {
        if no_policy_scope {
            return self.run_file(path);
        }
        self.policy_scope(|state| state.run_file(path))
    }

    fn run_file(&mut self, path: PathBuf) -> Result<()> {
        if self.files.len() > 128 {
            return fail("include depth exceeded 128");
        }
        let path =
            fs::canonicalize(&path).map_err(|error| format!("{}: {error}", path.display()))?;
        let text = fs::read_to_string(&path)?;
        let commands = syntax::parse(&path, &text)?;
        self.graph.inputs.insert(path.clone());
        self.files.push(path);
        let result = self.run(&commands);
        self.files.pop();
        match result? {
            Flow::Continue | Flow::Return => Ok(()),
            _ => fail("loop control outside a loop"),
        }
    }

    fn run(&mut self, commands: &[Command]) -> Result<Flow> {
        let mut index = 0;
        while let Some(command) = commands.get(index) {
            let action = (|| {
                match command.name.as_str() {
                    "if" => {
                        let branches = block(commands, index, "endif", &["elseif", "else"])?;
                        for pair in branches.windows(2) {
                            let branch = &commands[pair[0]];
                            if branch.name == "else" || self.condition(&self.arguments(branch)?)? {
                                let flow = self.run(&commands[pair[0] + 1..pair[1]])?;
                                index = *branches.last().expect("if block terminator");
                                return Ok(flow);
                            }
                        }
                        index = *branches.last().expect("if block terminator");
                    }
                    "function" | "macro" => {
                        let scoped = command.name == "function";
                        let end_name = if scoped { "endfunction" } else { "endmacro" };
                        let end = block(commands, index, end_name, &[])?[1];
                        let args = self.values(command)?;
                        need(&args, 1)?;
                        self.functions.insert(
                            args[0].to_ascii_lowercase(),
                            Callable {
                                parameters: args[1..].to_vec(),
                                body: commands[index + 1..end].to_vec(),
                                scoped,
                                policies: self.policies.clone(),
                            },
                        );
                        index = end;
                    }
                    "foreach" => {
                        let end = block(commands, index, "endforeach", &[])?[1];
                        let args = self.values(command)?;
                        need(&args, 1)?;
                        let old = self.variables().get(&args[0]).cloned();
                        for value in self.loop_values(&args[1..])? {
                            self.set(&args[0], value);
                            match self.run(&commands[index + 1..end])? {
                                Flow::Continue | Flow::Next => {}
                                Flow::Break => break,
                                Flow::Return => return Ok(Flow::Return),
                            }
                        }
                        if let Some(value) = old {
                            self.set(&args[0], value);
                        } else {
                            self.variables_mut().remove(&args[0]);
                        }
                        index = end;
                    }
                    "while" => {
                        let end = block(commands, index, "endwhile", &[])?[1];
                        while self.condition(&self.arguments(command)?)? {
                            match self.run(&commands[index + 1..end])? {
                                Flow::Continue | Flow::Next => {}
                                Flow::Break => break,
                                Flow::Return => return Ok(Flow::Return),
                            }
                        }
                        index = end;
                    }
                    "include_guard" => {
                        if self.include_guard(&self.values(command)?)? {
                            return Ok(Flow::Return);
                        }
                    }
                    "return" if command.args.is_empty() => return Ok(Flow::Return),
                    "continue" => return Ok(Flow::Next),
                    "break" => return Ok(Flow::Break),
                    _ => {
                        let args = self.values(command)?;
                        if let Some(callable) = self.functions.get(&command.name).cloned() {
                            return self.invoke(&callable, &args);
                        }
                        self.execute(command, &args)?;
                    }
                }
                Ok(Flow::Continue)
            })()
            .map_err(|error: crate::Error| {
                format!("{}: {}(): {error}", command.location, command.name)
            })?;
            if action != Flow::Continue {
                return Ok(action);
            }
            index += 1;
        }
        Ok(Flow::Continue)
    }

    fn invoke(&mut self, callable: &Callable, args: &[String]) -> Result<Flow> {
        // Callables use the settings recorded when they were defined.
        self.policy_scope(|state| {
            state.policies.extend(callable.policies.clone());
            state.invoke_body(callable, args)
        })
    }

    fn invoke_body(&mut self, callable: &Callable, args: &[String]) -> Result<Flow> {
        need(args, callable.parameters.len())?;
        if self.macros.len() + self.scopes.len() > 128 {
            return fail("call depth exceeded 128");
        }
        let variables = callable
            .parameters
            .iter()
            .cloned()
            .zip(args.iter().cloned())
            .chain(
                args.iter()
                    .enumerate()
                    .map(|(index, arg)| (format!("ARGV{index}"), arg.clone())),
            )
            .chain([
                ("ARGC".into(), args.len().to_string()),
                ("ARGV".into(), args.join(";")),
                ("ARGN".into(), args[callable.parameters.len()..].join(";")),
            ]);
        if callable.scoped {
            self.push_scope();
            self.variables_mut().retain(|name, _| !is_arg_var(name));
            self.variables_mut().extend(variables);
            self.function_args.push(args.to_vec());
            let result = self.run(&callable.body);
            self.function_args.pop();
            self.scopes.pop();
            match result? {
                Flow::Return | Flow::Continue => Ok(Flow::Continue),
                _ => fail("loop control crossed a function boundary"),
            }
        } else {
            self.macros.push(variables.collect());
            let result = self.run(&callable.body);
            self.macros.pop();
            result
        }
    }

    fn include_guard(&mut self, args: &[String]) -> Result<bool> {
        let file = self
            .files
            .last()
            .ok_or("include_guard needs a current file")?
            .clone();
        match args {
            [] => {
                let marker = format!("_cmake_rust_include_guard_{}", file.display());
                let repeated = self.get(&marker).is_some();
                self.set(&marker, "1");
                Ok(repeated)
            }
            [scope] if scope == "GLOBAL" => Ok(!self.include_guards.insert((file, None))),
            [scope] if scope == "DIRECTORY" => {
                let repeated = self.directories.iter().any(|directory| {
                    self.include_guards
                        .contains(&(file.clone(), Some(directory.source.clone())))
                });
                if !repeated {
                    self.include_guards
                        .insert((file, Some(self.directory().source.clone())));
                }
                Ok(repeated)
            }
            _ => fail("include_guard expects DIRECTORY, GLOBAL or no arguments"),
        }
    }

    fn loop_values(&self, args: &[String]) -> Result<Vec<String>> {
        match args.first().map(String::as_str) {
            Some("IN") => {
                let mut items = Vec::new();
                let mut lists = false;
                for arg in &args[1..] {
                    match arg.as_str() {
                        "LISTS" => lists = true,
                        "ITEMS" => lists = false,
                        _ if lists => {
                            if let Some(value) = self.get(arg).filter(|value| !value.is_empty()) {
                                items.extend(split_list(&value));
                            }
                        }
                        _ => items.push(arg.clone()),
                    }
                }
                Ok(items)
            }
            Some("RANGE") => {
                let values = args[1..]
                    .iter()
                    .map(|value| value.parse::<i64>())
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                let (start, stop, step) = match values.as_slice() {
                    [stop] => (0, *stop, 1),
                    [start, stop] => (*start, *stop, 1),
                    [start, stop, step] => (*start, *stop, *step),
                    _ => return fail("RANGE expects one to three numeric arguments"),
                };
                if start < 0 || start > stop || step <= 0 || (stop - start) / step > 1_000_000 {
                    return fail("invalid or oversized foreach RANGE");
                }
                Ok((start..=stop)
                    .step_by(usize::try_from(step)?)
                    .map(|value| value.to_string())
                    .collect())
            }
            _ => Ok(args.to_vec()),
        }
    }

    fn values(&self, command: &Command) -> Result<Vec<String>> {
        Ok(self
            .arguments(command)?
            .into_iter()
            .map(|argument| argument.text)
            .collect())
    }

    fn arguments(&self, command: &Command) -> Result<Vec<Argument>> {
        let mut args = Vec::new();
        for arg in &command.args {
            let expanded = if arg.form == Form::Bracket {
                arg.text.clone()
            } else {
                self.expand(&arg.text)?
            };
            if arg.form == Form::Bare {
                args.extend(
                    split_list(&expanded)
                        .into_iter()
                        .filter(|value| !value.is_empty())
                        .map(|text| Argument {
                            text,
                            form: Form::Bare,
                        }),
                );
            } else {
                args.push(Argument {
                    text: expanded,
                    form: arg.form.clone(),
                });
            }
        }
        Ok(args)
    }

    fn expand(&self, text: &str) -> Result<String> {
        self.expand_inner(text, 0)
    }

    fn expand_inner(&self, mut text: &str, depth: usize) -> Result<String> {
        if depth > 128 {
            return fail("variable expansion depth exceeded 128");
        }
        let mut result = String::new();
        while let Some(ch) = text.chars().next() {
            if let Some(after) = text.strip_prefix('\\') {
                let Some(escape) = after.chars().next() else {
                    return fail("dangling escape");
                };
                match escape {
                    'n' => result.push('\n'),
                    'r' => result.push('\r'),
                    't' => result.push('\t'),
                    '\n' => {}
                    ';' => result.push_str("\\;"),
                    _ if !escape.is_ascii_alphanumeric() => result.push(escape),
                    _ => return fail(format!("unsupported escape \\{escape}")),
                }
                text = &after[escape.len_utf8()..];
                continue;
            }
            if let Some((kind, remainder)) =
                ["$ENV{", "$CACHE{", "${"].into_iter().find_map(|prefix| {
                    text.strip_prefix(prefix)
                        .map(|remainder| (prefix, remainder))
                })
            {
                let close = reference_end(remainder).ok_or("unclosed variable reference")?;
                let key = self.expand_inner(&remainder[..close], depth + 1)?;
                let value = match kind {
                    "$ENV{" => {
                        if key.is_empty() {
                            None
                        } else {
                            env::var(&key).ok()
                        }
                    }
                    "$CACHE{" => self.cache_value(&key),
                    _ => self
                        .macros
                        .iter()
                        .rev()
                        .find_map(|scope| scope.get(&key).cloned())
                        .or_else(|| self.get(&key)),
                };
                result.push_str(&value.unwrap_or_default());
                text = &remainder[close + 1..];
                continue;
            }
            result.push(ch);
            text = &text[ch.len_utf8()..];
        }
        Ok(result)
    }
}

fn reference_end(text: &str) -> Option<usize> {
    let mut depth = 0;
    for (index, ch) in text.char_indices() {
        match ch {
            '{' => depth += 1,
            '}' if depth == 0 => return Some(index),
            '}' => depth -= 1,
            _ => {}
        }
    }
    None
}

fn is_arg_var(name: &str) -> bool {
    matches!(name, "ARGC" | "ARGN" | "ARGV")
        || name
            .strip_prefix("ARGV")
            .is_some_and(|index| index.parse::<usize>().is_ok())
}

fn block(commands: &[Command], start: usize, end: &str, separators: &[&str]) -> Result<Vec<usize>> {
    let mut indices = vec![start];
    let start_name = &commands[start].name;
    let mut depth = 0;
    for (index, command) in commands.iter().enumerate().skip(start + 1) {
        match command.name.as_str() {
            name if name == start_name => depth += 1,
            name if name == end && depth == 0 => {
                indices.push(index);
                return Ok(indices);
            }
            name if name == end => depth -= 1,
            name if depth == 0 && separators.contains(&name) => indices.push(index),
            _ => {}
        }
    }
    fail(format!("missing {end}()"))
}

fn need(args: &[String], minimum: usize) -> Result<()> {
    if args.len() < minimum {
        return fail(format!(
            "expected at least {minimum} arguments, got {}",
            args.len()
        ));
    }
    Ok(())
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn is_false(value: &str) -> bool {
    let upper = value.to_ascii_uppercase();
    matches!(
        upper.as_str(),
        "" | "0" | "OFF" | "NO" | "FALSE" | "N" | "IGNORE" | "NOTFOUND"
    ) || upper.ends_with("-NOTFOUND")
}

fn is_true_literal(value: &str) -> bool {
    matches!(
        value.to_ascii_uppercase().as_str(),
        "ON" | "YES" | "TRUE" | "Y"
    ) || value.parse::<f64>().is_ok_and(|number| number != 0.0)
}
