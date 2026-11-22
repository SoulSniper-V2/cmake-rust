//! `try_compile()` and `try_run()` — ports of `cmTryCompileCommand.cxx`,
//! `cmTryRunCommand.cxx` and the shared `cmCoreTryCompile.cxx`, including the
//! `cmArgumentParser` subset they use and the try_compile
//! `cmExportTryCompileFileGenerator` imported-target export file.

use std::collections::{BTreeMap, BTreeSet};

use crate::cache::CacheType;
use crate::interp::{CmdResult, Interp, err};
use crate::messages::MessageType;
use crate::policy::PolicyStatus;
use crate::state::{self, TargetKind};
use crate::util;

fn not_scriptable(_interp: &mut Interp, _args: &[String]) -> CmdResult {
    err("command is not scriptable")
}

pub fn register(interp: &mut Interp) {
    // Neither command is scriptable; in script mode they are stubs that
    // report "command is not scriptable" (cmState::AddUnexpectedCommand).
    if interp.mode == crate::interp::Mode::Script {
        interp.register("try_compile", not_scriptable);
        interp.register("try_run", not_scriptable);
        return;
    }
    interp.register("try_compile", try_compile_command);
    interp.register("try_run", try_run_command);
}

const UNIQUE_BINARY_DIRECTORY: &str = "CMAKE_BINARY_DIR_USE_MKDTEMP";
const TRY_COMPILE_DEFAULT_CONFIG: &str = "DEBUG";

/// `kLanguageNames` from cmCoreTryCompile.cxx.
const LANGUAGE_NAMES: [&str; 9] = [
    "C", "CUDA", "CXX", "Fortran", "HIP", "ISPC", "OBJC", "OBJCXX", "Swift",
];

/// Languages with `_STANDARD`/`_STANDARD_REQUIRED`/`_EXTENSIONS` keywords.
const STANDARD_LANGUAGES: [&str; 6] = ["C", "CUDA", "CXX", "HIP", "OBJC", "OBJCXX"];

/// `cmExperimental::Feature` entries whose variables are forwarded through
/// try_compile (`TryCompileCondition::Always` in the 4.1.0 table).
const EXPERIMENTAL_FORWARD_VARIABLES: [&str; 4] = [
    "CMAKE_EXPERIMENTAL_EXPORT_PACKAGE_DEPENDENCIES",
    "CMAKE_EXPERIMENTAL_CXX_IMPORT_STD",
    "CMAKE_EXPERIMENTAL_FIND_CPS_PACKAGES",
    "CMAKE_EXPERIMENTAL_EXPORT_PACKAGE_INFO",
];

// ---------------------------------------------------------------------------
// Argument parsing (port of cmArgumentParser for the try_compile parsers)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum SourceType {
    #[default]
    Normal,
    CxxModule,
    Directory,
}

/// `cmCoreTryCompile::Arguments` (plus `ArgumentParser::ParseResult`).
#[derive(Default)]
struct Arguments {
    compile_result_variable: Option<String>,
    binary_directory: Option<String>,
    source_directory_or_file: Option<String>,
    project_name: Option<String>,
    target_name: Option<String>,
    sources: Option<Vec<(String, SourceType)>>,
    source_from_content: Option<Vec<(String, SourceType)>>,
    source_from_var: Option<Vec<(String, SourceType)>>,
    source_from_file: Option<Vec<(String, SourceType)>>,
    /// `MaybeEmpty` list whose first element is the fake argv[0] "CMAKE_FLAGS".
    cmake_flags: Vec<String>,
    compile_defs: Vec<String>,
    link_libraries: Option<Vec<String>>,
    link_options: Vec<String>,
    linker_language: Option<String>,
    lang_props: BTreeMap<String, String>,
    cmake_internal: String,
    output_variable: Option<String>,
    copy_file_to: Option<String>,
    copy_file_error: Option<String>,
    log_description: Option<String>,
    no_cache: bool,
    no_log: bool,
    source_type_context: SourceType,
    source_type_error: String,
    keyword_errors: BTreeMap<String, String>,

    // try_run only.
    compile_output_variable: Option<String>,
    run_output_variable: Option<String>,
    run_output_stdout_variable: Option<String>,
    run_output_stderr_variable: Option<String>,
    run_working_directory: Option<String>,
    run_args: Option<Vec<String>>,
}

impl Arguments {
    fn new() -> Arguments {
        let mut args = Arguments::default();
        // fake argv[0]
        args.cmake_flags.push("CMAKE_FLAGS".to_owned());
        args
    }

    /// `Arguments::SetSourceType`.
    fn set_source_type(&mut self, source_type: &str) {
        let matched = match source_type {
            "NORMAL" => {
                self.source_type_context = SourceType::Normal;
                true
            }
            "CXX_MODULE" => {
                self.source_type_context = SourceType::CxxModule;
                true
            }
            _ => false,
        };
        if !matched && self.source_type_error.is_empty() {
            // Only remember one error at a time; all other errors related to
            // argument parsing are "indicate one error and return" anyways.
            self.source_type_error = format!(
                "Invalid 'SOURCE_TYPE' '{source_type}'; must be one of 'SOURCE' or 'CXX_MODULE'"
            );
        }
    }
}

/// Which destination a keyword binds to.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Dest {
    // Positional result variable.
    CompileResultVariable,
    // Shared keywords.
    LogDescription,
    NoCache,
    NoLog,
    CMakeFlags,
    CMakeInternal,
    OutputVariable,
    // Source-signature keywords.
    SourcesType,
    Sources,
    CompileDefs,
    LinkLibraries,
    LinkOptions,
    LinkerLanguage,
    CopyFile,
    CopyFileError,
    LangProp,
    SourceFromContent,
    SourceFromVar,
    SourceFromFile,
    // Project-signature keywords.
    ProjectKw,
    SourceDir,
    BinaryDirKw,
    TargetKw,
    // try_run keywords.
    CompileOutputVariable,
    RunOutputVariable,
    RunOutputStdOut,
    RunOutputStdErr,
    WorkingDirectory,
    RunArgs,
}

/// A keyword binding: name, destination, minimum value count.
#[derive(Clone, Copy)]
struct Kw {
    name: &'static str,
    dest: Dest,
    expect: usize,
}

const fn kw(name: &'static str, dest: Dest, expect: usize) -> Kw {
    Kw { name, dest, expect }
}

fn lang_prop_keywords() -> [Kw; 18] {
    let mut out = [kw("", Dest::LangProp, 0); 18];
    let mut index = 0;
    for lang in STANDARD_LANGUAGES {
        for suffix in ["_STANDARD", "_STANDARD_REQUIRED", "_EXTENSIONS"] {
            out[index] = Kw {
                name: match (lang, suffix) {
                    ("C", "_STANDARD") => "C_STANDARD",
                    ("C", "_STANDARD_REQUIRED") => "C_STANDARD_REQUIRED",
                    ("C", "_EXTENSIONS") => "C_EXTENSIONS",
                    ("CUDA", "_STANDARD") => "CUDA_STANDARD",
                    ("CUDA", "_STANDARD_REQUIRED") => "CUDA_STANDARD_REQUIRED",
                    ("CUDA", "_EXTENSIONS") => "CUDA_EXTENSIONS",
                    ("CXX", "_STANDARD") => "CXX_STANDARD",
                    ("CXX", "_STANDARD_REQUIRED") => "CXX_STANDARD_REQUIRED",
                    ("CXX", "_EXTENSIONS") => "CXX_EXTENSIONS",
                    ("HIP", "_STANDARD") => "HIP_STANDARD",
                    ("HIP", "_STANDARD_REQUIRED") => "HIP_STANDARD_REQUIRED",
                    ("HIP", "_EXTENSIONS") => "HIP_EXTENSIONS",
                    ("OBJC", "_STANDARD") => "OBJC_STANDARD",
                    ("OBJC", "_STANDARD_REQUIRED") => "OBJC_STANDARD_REQUIRED",
                    ("OBJC", "_EXTENSIONS") => "OBJC_EXTENSIONS",
                    ("OBJCXX", "_STANDARD") => "OBJCXX_STANDARD",
                    ("OBJCXX", "_STANDARD_REQUIRED") => "OBJCXX_STANDARD_REQUIRED",
                    ("OBJCXX", "_EXTENSIONS") => "OBJCXX_EXTENSIONS",
                    _ => unreachable!(),
                },
                dest: Dest::LangProp,
                expect: 1,
            };
            index += 1;
        }
    }
    out
}

/// `TryCompileBaseArgParser`.
fn base_keywords() -> Vec<Kw> {
    vec![
        kw("LOG_DESCRIPTION", Dest::LogDescription, 1),
        kw("NO_CACHE", Dest::NoCache, 0),
        kw("NO_LOG", Dest::NoLog, 0),
        kw("CMAKE_FLAGS", Dest::CMakeFlags, 0),
        kw("__CMAKE_INTERNAL", Dest::CMakeInternal, 1),
    ]
}

/// `TryCompileBaseSourcesArgParser`.
fn sources_keywords() -> Vec<Kw> {
    let mut kws = base_keywords();
    kws.extend_from_slice(&[
        kw("SOURCES_TYPE", Dest::SourcesType, 1),
        kw("SOURCES", Dest::Sources, 1),
        kw("COMPILE_DEFINITIONS", Dest::CompileDefs, 0),
        kw("LINK_LIBRARIES", Dest::LinkLibraries, 0),
        kw("LINK_OPTIONS", Dest::LinkOptions, 0),
        kw("LINKER_LANGUAGE", Dest::LinkerLanguage, 1),
        kw("COPY_FILE", Dest::CopyFile, 1),
        kw("COPY_FILE_ERROR", Dest::CopyFileError, 1),
    ]);
    kws.extend_from_slice(&lang_prop_keywords());
    kws
}

/// `TryCompileBaseNewSourcesArgParser`.
fn new_sources_keywords() -> Vec<Kw> {
    let mut kws = sources_keywords();
    kws.extend_from_slice(&[
        kw("SOURCE_FROM_CONTENT", Dest::SourceFromContent, 1),
        kw("SOURCE_FROM_VAR", Dest::SourceFromVar, 1),
        kw("SOURCE_FROM_FILE", Dest::SourceFromFile, 1),
    ]);
    kws
}

/// `TryCompileBaseProjectArgParser`.
fn project_keywords() -> Vec<Kw> {
    let mut kws = base_keywords();
    kws.extend_from_slice(&[
        kw("PROJECT", Dest::ProjectKw, 1),
        kw("SOURCE_DIR", Dest::SourceDir, 1),
        kw("BINARY_DIR", Dest::BinaryDirKw, 1),
        kw("TARGET", Dest::TargetKw, 1),
    ]);
    kws
}

fn try_compile_keywords() -> Vec<Kw> {
    vec![kw("OUTPUT_VARIABLE", Dest::OutputVariable, 1)]
}

fn try_run_keywords() -> Vec<Kw> {
    vec![
        kw("COMPILE_OUTPUT_VARIABLE", Dest::CompileOutputVariable, 1),
        kw("RUN_OUTPUT_VARIABLE", Dest::RunOutputVariable, 1),
        kw("RUN_OUTPUT_STDOUT_VARIABLE", Dest::RunOutputStdOut, 1),
        kw("RUN_OUTPUT_STDERR_VARIABLE", Dest::RunOutputStdErr, 1),
        kw("WORKING_DIRECTORY", Dest::WorkingDirectory, 1),
        kw("ARGS", Dest::RunArgs, 0),
    ]
}

/// The keyword/position tables of one cmArgumentParser.
struct Spec {
    keywords: Vec<Kw>,
    /// pos -> destination for old-signature positional arguments 1..=4.
    positions: [Option<Dest>; 5],
}

const fn pos(
    a: Dest,
    b: Option<Dest>,
    c: Option<Dest>,
    d: Option<Dest>,
    e: Option<Dest>,
) -> [Option<Dest>; 5] {
    [Some(a), b, c, d, e]
}

fn project_spec() -> Spec {
    let mut keywords = project_keywords();
    keywords.extend_from_slice(&try_compile_keywords());
    Spec {
        keywords,
        positions: pos(Dest::CompileResultVariable, None, None, None, None),
    }
}

fn try_compile_sources_spec() -> Spec {
    let mut keywords = new_sources_keywords();
    keywords.extend_from_slice(&try_compile_keywords());
    Spec {
        keywords,
        positions: pos(Dest::CompileResultVariable, None, None, None, None),
    }
}

fn try_compile_old_spec() -> Spec {
    let mut keywords = sources_keywords();
    keywords.extend_from_slice(&try_compile_keywords());
    Spec {
        keywords,
        positions: pos(
            Dest::CompileResultVariable,
            Some(Dest::BinaryDirKw),
            Some(Dest::SourceDir),
            Some(Dest::ProjectKw),
            Some(Dest::TargetKw),
        ),
    }
}

fn try_run_sources_spec() -> Spec {
    let mut keywords = new_sources_keywords();
    keywords.extend_from_slice(&try_run_keywords());
    Spec {
        keywords,
        positions: pos(Dest::CompileResultVariable, None, None, None, None),
    }
}

/// `TryRunOldArgParser = makeTryRunParser(TryCompileOldArgParser)`: the
/// legacy try_run also accepts try_compile's OUTPUT_VARIABLE.
fn try_run_old_spec() -> Spec {
    let mut keywords = sources_keywords();
    keywords.extend_from_slice(&try_compile_keywords());
    keywords.extend_from_slice(&try_run_keywords());
    Spec {
        keywords,
        positions: pos(
            Dest::CompileResultVariable,
            Some(Dest::BinaryDirKw),
            Some(Dest::SourceDir),
            Some(Dest::ProjectKw),
            Some(Dest::TargetKw),
        ),
    }
}

/// `Instance::Bind` outcomes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Continue {
    No,
    Yes,
}

/// Parser state while consuming one argument stream (`ArgumentParser::Instance`).
struct Parser<'a> {
    spec: &'a Spec,
    args: &'a mut Arguments,
    keyword: &'static str,
    keyword_values_seen: usize,
    keyword_values_expected: usize,
    active: Option<Dest>,
    done_with_positional: bool,
    unparsed: Vec<String>,
}

impl<'a> Parser<'a> {
    fn parse(&mut self, argv: &[String]) {
        for (position, arg) in argv.iter().enumerate() {
            self.consume(position, arg);
        }
        self.finish_keyword();
    }

    /// `Instance::Consume`.
    fn consume(&mut self, position: usize, arg: &str) {
        if let Some(spec) = self.spec.keywords.iter().find(|k| k.name == arg) {
            self.finish_keyword();
            self.keyword = spec.name;
            self.keyword_values_seen = 0;
            self.keyword_values_expected = spec.expect;
            self.done_with_positional = true;
            self.active = self.bind(spec.dest);
            return;
        }

        if !self.done_with_positional {
            if let Some(Some(dest)) = self.spec.positions.get(position) {
                self.bind_positional(*dest, arg);
                return;
            }
            // The try_compile/try_run parsers bind no trailing args.
        }

        if let Some(dest) = self.active {
            match self.value(dest, arg) {
                Continue::Yes => {}
                Continue::No => self.active = None,
            }
            self.keyword_values_seen += 1;
            return;
        }

        self.unparsed.push(arg.to_owned());
    }

    /// `Instance::FinishKeyword`.
    fn finish_keyword(&mut self) {
        if !self.done_with_positional {
            return;
        }
        if self.keyword_values_seen < self.keyword_values_expected {
            self.args
                .keyword_errors
                .entry(self.keyword.to_owned())
                .or_default()
                .push_str("  missing required value\n");
        }
    }

    fn error(&mut self, text: &str) {
        self.args
            .keyword_errors
            .entry(self.keyword.to_owned())
            .or_default()
            .push_str(text);
    }

    /// The action run when a keyword is seen (`instance.Bind(&ref)`);
    /// returns the destination of the active value function, if any.
    fn bind(&mut self, dest: Dest) -> Option<Dest> {
        match dest {
            // Bind(bool&): set true and take no values.
            Dest::NoCache => {
                self.args.no_cache = true;
                None
            }
            Dest::NoLog => {
                self.args.no_log = true;
                None
            }
            // optional<T>: record presence, then bind the value(s).
            Dest::OutputVariable => {
                self.args.output_variable = Some(String::new());
                Some(dest)
            }
            Dest::CompileOutputVariable => {
                self.args.compile_output_variable = Some(String::new());
                Some(dest)
            }
            Dest::RunOutputVariable => {
                self.args.run_output_variable = Some(String::new());
                Some(dest)
            }
            Dest::RunOutputStdOut => {
                self.args.run_output_stdout_variable = Some(String::new());
                Some(dest)
            }
            Dest::RunOutputStdErr => {
                self.args.run_output_stderr_variable = Some(String::new());
                Some(dest)
            }
            Dest::WorkingDirectory => {
                self.args.run_working_directory = Some(String::new());
                Some(dest)
            }
            Dest::ProjectKw => {
                self.args.project_name = Some(String::new());
                Some(dest)
            }
            Dest::SourceDir => {
                self.args.source_directory_or_file = Some(String::new());
                Some(dest)
            }
            Dest::BinaryDirKw => {
                self.args.binary_directory = Some(String::new());
                Some(dest)
            }
            Dest::TargetKw => {
                self.args.target_name = Some(String::new());
                Some(dest)
            }
            Dest::LinkerLanguage => {
                self.args.linker_language = Some(String::new());
                Some(dest)
            }
            Dest::CopyFile => {
                self.args.copy_file_to = Some(String::new());
                Some(dest)
            }
            Dest::CopyFileError => {
                self.args.copy_file_error = Some(String::new());
                Some(dest)
            }
            Dest::LogDescription => {
                self.args.log_description = Some(String::new());
                Some(dest)
            }
            Dest::RunArgs => {
                self.args.run_args.get_or_insert_with(Vec::new);
                Some(dest)
            }
            Dest::LinkLibraries => {
                self.args.link_libraries.get_or_insert_with(Vec::new);
                Some(dest)
            }
            Dest::Sources => {
                self.args.sources.get_or_insert_with(Vec::new);
                Some(dest)
            }
            Dest::SourceFromContent => {
                self.args.source_from_content.get_or_insert_with(Vec::new);
                Some(dest)
            }
            Dest::SourceFromVar => {
                self.args.source_from_var.get_or_insert_with(Vec::new);
                Some(dest)
            }
            Dest::SourceFromFile => {
                self.args.source_from_file.get_or_insert_with(Vec::new);
                Some(dest)
            }
            // Plain string/vector members.
            Dest::CMakeFlags
            | Dest::CMakeInternal
            | Dest::SourcesType
            | Dest::CompileDefs
            | Dest::LinkOptions
            | Dest::LangProp => Some(dest),
            // Positional-only destinations are bound by `bind_positional`.
            Dest::CompileResultVariable => unreachable!(),
        }
    }

    /// Positional binding (`Bind(pos, &optional<string>)`).
    fn bind_positional(&mut self, dest: Dest, arg: &str) {
        let value = arg.to_owned();
        match dest {
            Dest::CompileResultVariable => self.args.compile_result_variable = Some(value),
            Dest::BinaryDirKw => self.args.binary_directory = Some(value),
            Dest::SourceDir => self.args.source_directory_or_file = Some(value),
            Dest::ProjectKw => self.args.project_name = Some(value),
            Dest::TargetKw => self.args.target_name = Some(value),
            _ => unreachable!(),
        }
    }

    /// Consume one value for the active keyword.
    fn value(&mut self, dest: Dest, arg: &str) -> Continue {
        match dest {
            Dest::OutputVariable => {
                self.args.output_variable = Some(arg.to_owned());
                Continue::No
            }
            Dest::CompileOutputVariable => {
                self.args.compile_output_variable = Some(arg.to_owned());
                Continue::No
            }
            Dest::RunOutputVariable => {
                self.args.run_output_variable = Some(arg.to_owned());
                Continue::No
            }
            Dest::RunOutputStdOut => {
                self.args.run_output_stdout_variable = Some(arg.to_owned());
                Continue::No
            }
            Dest::RunOutputStdErr => {
                self.args.run_output_stderr_variable = Some(arg.to_owned());
                Continue::No
            }
            Dest::WorkingDirectory => {
                self.args.run_working_directory = Some(arg.to_owned());
                Continue::No
            }
            Dest::ProjectKw => {
                self.args.project_name = Some(arg.to_owned());
                Continue::No
            }
            Dest::SourceDir => {
                self.args.source_directory_or_file = Some(arg.to_owned());
                Continue::No
            }
            Dest::BinaryDirKw => {
                self.args.binary_directory = Some(arg.to_owned());
                Continue::No
            }
            Dest::TargetKw => {
                self.args.target_name = Some(arg.to_owned());
                Continue::No
            }
            Dest::LinkerLanguage => {
                self.args.linker_language = Some(arg.to_owned());
                Continue::No
            }
            Dest::CopyFile => {
                self.args.copy_file_to = Some(arg.to_owned());
                Continue::No
            }
            Dest::CopyFileError => {
                self.args.copy_file_error = Some(arg.to_owned());
                Continue::No
            }
            Dest::LogDescription => {
                // Bind(NonEmpty<string>&).
                if arg.is_empty() {
                    self.error("  empty string not allowed\n");
                }
                self.args.log_description = Some(arg.to_owned());
                Continue::No
            }
            Dest::CMakeInternal => {
                self.args.cmake_internal = arg.to_owned();
                Continue::No
            }
            Dest::CMakeFlags => {
                self.args.cmake_flags.push(arg.to_owned());
                Continue::Yes
            }
            Dest::LinkOptions => {
                self.args.link_options.push(arg.to_owned());
                Continue::Yes
            }
            Dest::RunArgs => {
                self.args
                    .run_args
                    .as_mut()
                    .expect("bind")
                    .push(arg.to_owned());
                Continue::Yes
            }
            Dest::LinkLibraries => {
                self.args
                    .link_libraries
                    .as_mut()
                    .expect("bind")
                    .push(arg.to_owned());
                Continue::Yes
            }
            Dest::Sources => {
                let context = self.args.source_type_context;
                self.args
                    .sources
                    .as_mut()
                    .expect("bind")
                    .push((arg.to_owned(), context));
                Continue::Yes
            }
            Dest::SourceFromContent => {
                let context = self.args.source_type_context;
                self.args
                    .source_from_content
                    .as_mut()
                    .expect("bind")
                    .push((arg.to_owned(), context));
                Continue::Yes
            }
            Dest::SourceFromVar => {
                let context = self.args.source_type_context;
                self.args
                    .source_from_var
                    .as_mut()
                    .expect("bind")
                    .push((arg.to_owned(), context));
                Continue::Yes
            }
            Dest::SourceFromFile => {
                let context = self.args.source_type_context;
                self.args
                    .source_from_file
                    .as_mut()
                    .expect("bind")
                    .push((arg.to_owned(), context));
                Continue::Yes
            }
            Dest::SourcesType => {
                self.args.set_source_type(arg);
                Continue::Yes
            }
            Dest::CompileDefs => {
                // TryCompileCompileDefs: cmList::append expands `;` items.
                util::expand_list_into(arg, false, &mut self.args.compile_defs);
                Continue::Yes
            }
            Dest::LangProp => {
                self.args
                    .lang_props
                    .insert(self.keyword.to_owned(), arg.to_owned());
                Continue::No
            }
            Dest::CompileResultVariable | Dest::NoCache | Dest::NoLog => unreachable!(),
        }
    }
}

fn parse_with(interp: &mut Interp, argv: &[String], spec: &Spec) -> Option<Arguments> {
    let mut arguments = Arguments::new();
    let mut parser = Parser {
        spec,
        args: &mut arguments,
        keyword: "",
        keyword_values_seen: 0,
        keyword_values_expected: 0,
        active: None,
        done_with_positional: false,
        unparsed: Vec::new(),
    };
    parser.parse(argv);
    let unparsed = std::mem::take(&mut parser.unparsed);
    if !arguments.keyword_errors.is_empty() {
        // ParseResult::MaybeReportError.
        let mut error = String::new();
        for (keyword, text) in &arguments.keyword_errors {
            error.push_str(&format!("Error after keyword \"{keyword}\":\n{text}"));
        }
        interp.issue(MessageType::FatalError, &error);
        interp.set_fatal();
        return None;
    }
    if !unparsed.is_empty() {
        let mut text = String::from("Unknown arguments:");
        for arg in &unparsed {
            text.push_str(&format!("\n  \"{arg}\""));
        }
        interp.issue(MessageType::AuthorWarning, &text);
    }
    Some(arguments)
}

/// `cmCoreTryCompile::ParseArgs(args, isTryRun)` — picks the signature.
fn parse_args(interp: &mut Interp, argv: &[String], is_try_run: bool) -> Option<Arguments> {
    let second = argv.get(1).map(String::as_str).unwrap_or("");

    if !is_try_run && second == "PROJECT" {
        // New PROJECT signature (try_compile only).
        let spec = project_spec();
        let mut arguments = parse_with(interp, argv, &spec)?;
        if arguments.binary_directory.is_none() {
            arguments.binary_directory = Some(UNIQUE_BINARY_DIRECTORY.to_owned());
        }
        return Some(arguments);
    }

    if second.starts_with("SOURCE") {
        // New SOURCES signature.
        let spec = if is_try_run {
            try_run_sources_spec()
        } else {
            try_compile_sources_spec()
        };
        let mut arguments = parse_with(interp, argv, &spec)?;
        arguments.binary_directory = Some(UNIQUE_BINARY_DIRECTORY.to_owned());
        return Some(arguments);
    }

    // Old signature.
    let spec = if is_try_run {
        try_run_old_spec()
    } else {
        try_compile_old_spec()
    };
    let mut arguments = parse_with(interp, argv, &spec)?;
    // For historical reasons, treat some empty-valued keyword
    // arguments as if they were not specified at all.
    if arguments.output_variable.as_deref() == Some("") {
        arguments.output_variable = None;
    }
    if is_try_run {
        if arguments.compile_output_variable.as_deref() == Some("") {
            arguments.compile_output_variable = None;
        }
        if arguments.run_output_variable.as_deref() == Some("") {
            arguments.run_output_variable = None;
        }
        if arguments.run_output_stdout_variable.as_deref() == Some("") {
            arguments.run_output_stdout_variable = None;
        }
        if arguments.run_output_stderr_variable.as_deref() == Some("") {
            arguments.run_output_stderr_variable = None;
        }
        if arguments.run_working_directory.as_deref() == Some("") {
            arguments.run_working_directory = None;
        }
    }
    Some(arguments)
}

// ---------------------------------------------------------------------------
// Filesystem helpers (cmsys::SystemTools ports)
// ---------------------------------------------------------------------------

fn fatal(interp: &mut Interp, text: String) {
    interp.issue(MessageType::FatalError, &text);
    interp.set_fatal();
}

/// `cmSystemTools::RandomNumber` — enough entropy for cmTC names.
fn random_number() -> u32 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() ^ d.as_secs() as u32)
        .unwrap_or(0);
    nanos ^ (std::process::id() << 16)
}

/// `cmSystemTools::MakeTempDirectory` — mkdtemp for a path ending in XXXXXX.
fn make_temp_directory(path: &mut String) -> bool {
    if !path.ends_with("XXXXXX") {
        return false;
    }
    let parent = util::filename_path(path);
    let _ = std::fs::create_dir_all(&parent);
    const CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    for _ in 0..100 {
        let mut rng = random_number();
        let mut suffix = String::new();
        for _ in 0..6 {
            rng = rng.wrapping_mul(1_103_515_245).wrapping_add(12345);
            suffix.push(CHARS[(rng >> 16) as usize % CHARS.len()] as char);
        }
        let candidate = format!("{}{}", &path[..path.len() - 6], suffix);
        if std::fs::create_dir(&candidate).is_ok() {
            *path = candidate;
            return true;
        }
    }
    false
}

/// `cmSystemTools::MakeDirectory`.
fn make_directory(path: &str) -> bool {
    std::fs::create_dir_all(path).is_ok()
}

/// `cmSystemTools::RemoveFile` / `RemoveADirectory` on a path.
fn remove_path(path: &str) {
    let meta = std::fs::symlink_metadata(path);
    match meta {
        Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => {
            let _ = std::fs::remove_dir_all(path);
        }
        Ok(_) => {
            let _ = std::fs::remove_file(path);
        }
        Err(_) => {}
    }
}

/// `cmSystemTools::CopyFileAlways` — returns Err((message, path-tag)).
fn copy_file_always(source: &str, destination: &str) -> Result<(), (String, &'static str)> {
    let source_path = std::path::Path::new(source);
    if source_path.is_dir() {
        return match std::fs::create_dir_all(destination) {
            Ok(()) => Ok(()),
            Err(e) => Err((e.to_string(), "output")),
        };
    }
    let dest_path = std::path::Path::new(destination);
    let mut real_destination = destination.to_owned();
    if dest_path.is_dir() {
        real_destination = format!(
            "{}/{}",
            destination.trim_end_matches('/'),
            util::filename_name(source)
        );
    }
    // If files are the same do not copy.
    if let (Ok(a), Ok(b)) = (
        source_path.canonicalize(),
        std::path::Path::new(&real_destination).canonicalize(),
    ) && a == b
    {
        return Ok(());
    }
    let dest_dir = util::filename_path(&real_destination);
    if !dest_dir.is_empty()
        && let Err(e) = std::fs::create_dir_all(&dest_dir)
    {
        return Err((e.to_string(), "output"));
    }
    match std::fs::copy(source, &real_destination) {
        Ok(_) => Ok(()),
        Err(e) => Err((e.to_string(), "input")),
    }
}

fn path_is_symlink(path: &str) -> bool {
    std::fs::symlink_metadata(path)
        .map(|meta| meta.file_type().is_symlink())
        .unwrap_or(false)
}

fn filename_last_extension(filename: &str) -> String {
    let name = util::filename_name(filename);
    match name.rfind('.') {
        Some(dot) => name[dot..].to_owned(),
        None => String::new(),
    }
}

fn filename_extension(filename: &str) -> String {
    let name = util::filename_name(filename);
    match name.find('.') {
        Some(dot) => name[dot..].to_owned(),
        None => String::new(),
    }
}

fn filename_without_extension(filename: &str) -> String {
    let name = util::filename_name(filename);
    match name.find('.') {
        Some(dot) => name[..dot].to_owned(),
        None => name,
    }
}

fn upper_case(text: &str) -> String {
    text.to_ascii_uppercase()
}

fn trim_whitespace(text: &str) -> &str {
    text.trim_matches(|c: char| c.is_ascii_whitespace())
}

/// `PolicyOptionalWarningEnabled`.
fn policy_optional_warning_enabled(interp: &Interp, var: &str) -> bool {
    interp.policy_optional_warning(interp.get_def(var))
}

// ---------------------------------------------------------------------------
// cmExportTryCompileFileGenerator
// ---------------------------------------------------------------------------

/// `cmExportFileGeneratorEscape`: EscapeForCMake plus un-escaping the
/// import-prefix variables this generator writes.
fn export_escape(text: &str) -> String {
    util::escape_for_cmake(text)
        .replace("\\${_IMPORT_PREFIX}", "${_IMPORT_PREFIX}")
        .replace(
            "\\${CMAKE_IMPORT_LIBRARY_SUFFIX}",
            "${CMAKE_IMPORT_LIBRARY_SUFFIX}",
        )
}

fn target_type_name(kind: TargetKind) -> &'static str {
    kind.type_name()
}

fn target_is_executable_with_exports(target: &state::Target) -> bool {
    target.kind == TargetKind::Executable
        && target.props.get("ENABLE_EXPORTS").is_some_and(util::is_on)
}

fn target_export_name(target: &state::Target) -> String {
    target
        .props
        .get("EXPORT_NAME")
        .filter(|name| !name.is_empty())
        .unwrap_or(&target.name)
        .to_owned()
}

/// Builtin transitive interface property names, iterated in the
/// `BuiltinTransitiveProperties` map (sorted) order.
const TRANSITIVE_INTERFACE_PROPERTIES: [&str; 13] = [
    "INTERFACE_AUTOMOC_MACRO_NAMES",
    "INTERFACE_AUTOUIC_OPTIONS",
    "INTERFACE_COMPILE_DEFINITIONS",
    "INTERFACE_COMPILE_FEATURES",
    "INTERFACE_COMPILE_OPTIONS",
    "INTERFACE_INCLUDE_DIRECTORIES",
    "INTERFACE_LINK_DEPENDS",
    "INTERFACE_LINK_DIRECTORIES",
    "INTERFACE_LINK_LIBRARIES",
    "INTERFACE_LINK_OPTIONS",
    "INTERFACE_PRECOMPILE_HEADERS",
    "INTERFACE_SOURCES",
    "INTERFACE_SYSTEM_INCLUDE_DIRECTORIES",
];

/// Finds targets referenced by a literal (non-genex) list property value and
/// queues them for export (`cmExportTryCompileFileGenerator::FindTargets` +
/// the `PopulateProperties` cmList scan).
fn find_targets_in_value<'a>(
    interp: &'a Interp,
    value: &str,
    emitted_deps: &mut BTreeSet<String>,
    exports: &mut Vec<&'a str>,
) {
    for item in util::expand_list(value, false) {
        if let Some((key, target)) = state::find_target_entry(interp, &item)
            && emitted_deps.insert(target.name.clone())
        {
            exports.push(key);
        }
    }
}

/// `cmExportCMakeConfigGenerator::GenerateImportTargetCode`.
fn generate_import_target_code(out: &mut String, target: &state::Target, target_type: TargetKind) {
    let target_name = target_export_name(target);
    out.push_str(&format!("# Create imported target {target_name}\n"));
    match target_type {
        TargetKind::Executable => {
            out.push_str(&format!("add_executable({target_name} IMPORTED)\n"))
        }
        TargetKind::StaticLibrary => {
            out.push_str(&format!("add_library({target_name} STATIC IMPORTED)\n"))
        }
        TargetKind::SharedLibrary => {
            out.push_str(&format!("add_library({target_name} SHARED IMPORTED)\n"))
        }
        TargetKind::ModuleLibrary => {
            out.push_str(&format!("add_library({target_name} MODULE IMPORTED)\n"))
        }
        TargetKind::UnknownLibrary => {
            out.push_str(&format!("add_library({target_name} UNKNOWN IMPORTED)\n"))
        }
        TargetKind::ObjectLibrary => {
            out.push_str(&format!("add_library({target_name} OBJECT IMPORTED)\n"))
        }
        TargetKind::InterfaceLibrary => {
            out.push_str(&format!("add_library({target_name} INTERFACE IMPORTED)\n"))
        }
        _ => {}
    }

    // Mark the imported executable if it has exports.
    if target_is_executable_with_exports(target)
        || (target.kind == TargetKind::SharedLibrary
            && target.props.get("ENABLE_EXPORTS").is_some_and(util::is_on)
            && has_import_library(target))
    {
        out.push_str(&format!(
            "set_property(TARGET {target_name} PROPERTY ENABLE_EXPORTS 1)\n"
        ));
    }

    // Framework/application bundle markers only apply on Apple platforms.
    #[cfg(target_os = "macos")]
    {
        let framework = matches!(
            target.kind,
            TargetKind::SharedLibrary | TargetKind::StaticLibrary
        ) && target.props.get("FRAMEWORK").is_some_and(util::is_on);
        if framework {
            out.push_str(&format!(
                "set_property(TARGET {target_name} PROPERTY FRAMEWORK 1)\n"
            ));
        }
        if target.props.get("MACOSX_BUNDLE").is_some_and(util::is_on) {
            out.push_str(&format!(
                "set_property(TARGET {target_name} PROPERTY MACOSX_BUNDLE 1)\n"
            ));
        }
        if target.props.get("BUNDLE").is_some_and(util::is_on) {
            out.push_str(&format!(
                "set_property(TARGET {target_name} PROPERTY BUNDLE 1)\n"
            ));
        }
    }

    // generate DEPRECATION
    if let Some(deprecation) = target.props.get("DEPRECATION").filter(|d| !d.is_empty()) {
        out.push_str(&format!(
            "set_property(TARGET {target_name} PROPERTY DEPRECATION {})\n",
            export_escape(deprecation)
        ));
    }

    if target
        .props
        .get("IMPORTED_NO_SYSTEM")
        .is_some_and(util::is_on)
    {
        out.push_str(&format!(
            "set_property(TARGET {target_name} PROPERTY IMPORTED_NO_SYSTEM 1)\n"
        ));
    }

    if target
        .props
        .get("EXPORT_NO_SYSTEM")
        .is_some_and(util::is_on)
    {
        out.push_str(&format!(
            "set_property(TARGET {target_name} PROPERTY SYSTEM 0)\n"
        ));
    }

    out.push('\n');
}

fn has_import_library(_target: &state::Target) -> bool {
    // SHARED_LIBRARY with exports on Windows uses IMPORTED_IMPLIB; on this
    // platform the archive is the library itself.
    false
}

/// `cmExportCMakeConfigGenerator::GenerateInterfaceProperties`.
fn generate_interface_properties(
    out: &mut String,
    target: &state::Target,
    properties: &BTreeMap<String, String>,
) {
    if !properties.is_empty() {
        let target_name = target_export_name(target);
        out.push_str(&format!("set_target_properties({target_name} PROPERTIES\n"));
        for (name, value) in properties {
            out.push_str(&format!("  {name} {}\n", export_escape(value)));
        }
        out.push_str(")\n\n");
    }
}

/// `cmExportTryCompileFileGenerator::GenerateMainFile` — exported targets and
/// their interface dependency targets, in stack order.
fn generate_targets_file(
    interp: &Interp,
    targets: &[String],
    languages: &BTreeSet<String>,
) -> String {
    let mut out = String::new();
    // RequiredCMakeVersion defaults to 2.8.3.
    out.push_str(
        "# Generated by CMake\n\n\
         if(\"${CMAKE_MAJOR_VERSION}.${CMAKE_MINOR_VERSION}\" LESS 2.8)\n\
         \x20  message(FATAL_ERROR \"CMake >= 2.8.3 required\")\n\
         endif()\n\
         if(CMAKE_VERSION VERSION_LESS \"2.8.3\")\n\
         \x20  message(FATAL_ERROR \"CMake >= 2.8.3 required\")\n\
         endif()\n\
         cmake_policy(PUSH)\n\
         cmake_policy(VERSION 2.8.3...3.31)\n",
    );
    out.push_str(
        "#----------------------------------------------------------------\n\
         # Generated CMake target import file.\n\
         #----------------------------------------------------------------\n\n",
    );
    out.push_str(
        "# Commands may need to know the format version.\n\
         set(CMAKE_IMPORT_FILE_VERSION 1)\n\n",
    );

    // CreateImportedGenerationObjects resolves names to targets.
    let mut exports: Vec<&str> = Vec::new();
    for name in targets {
        if let Some((key, _)) = state::find_target_entry(interp, name) {
            exports.push(key);
        }
    }

    let mut emitted: BTreeSet<String> = BTreeSet::new();
    let mut emitted_deps: BTreeSet<String> = BTreeSet::new();
    while let Some(name) = exports.pop() {
        if !emitted.insert(name.to_owned()) {
            continue;
        }
        let Some(target) = interp.ext.targets.get(name) else {
            continue;
        };
        emitted_deps.insert(name.to_owned());
        generate_import_target_code(&mut out, target, target.kind);

        let mut properties: BTreeMap<String, String> = BTreeMap::new();

        // FindTargets over the builtin transitive interface properties.
        for _lang in languages {
            for prop_name in TRANSITIVE_INTERFACE_PROPERTIES {
                let Some(value) = target.props.get(prop_name) else {
                    continue;
                };
                // For genex-free values the compiled expression collects
                // targets referenced by name inside $<TARGET_NAME:..>-style
                // usage; literal items are handled by PopulateProperties.
                find_targets_in_genex(interp, value, &mut emitted_deps, &mut exports);
            }
        }

        // PopulateProperties: all non-special properties plus the three
        // special link-interface properties.
        let mut keys: Vec<String> = target.props.keys().cloned().collect();
        for special in [
            "INTERFACE_LINK_LIBRARIES",
            "INTERFACE_LINK_LIBRARIES_DIRECT",
            "INTERFACE_LINK_LIBRARIES_DIRECT_EXCLUDE",
        ] {
            keys.push(special.to_owned());
        }
        for prop in keys {
            let Some(value) = target.props.get(&prop) else {
                continue;
            };
            properties.insert(prop.clone(), value.to_owned());
            if prop.starts_with("IMPORTED_LINK_INTERFACE_LIBRARIES")
                || prop.starts_with("IMPORTED_LINK_DEPENDENT_LIBRARIES")
                || prop.starts_with("INTERFACE_LINK_LIBRARIES")
            {
                find_targets_in_value(interp, value, &mut emitted_deps, &mut exports);
            }
        }

        generate_interface_properties(&mut out, target, &properties);
    }

    out.push_str(
        "# Commands beyond this point should not need to know the version.\n\
         set(CMAKE_IMPORT_FILE_VERSION)\n",
    );
    out.push_str("cmake_policy(POP)\n");
    out
}

/// Collects target names appearing inside `$<TARGET_NAME:name>` and
/// `$<TARGET_PROPERTY:name,...>` generator expressions
/// (`GetAllTargetsSeen` for a compiled genex).
fn find_targets_in_genex<'a>(
    interp: &'a Interp,
    value: &str,
    emitted_deps: &mut BTreeSet<String>,
    exports: &mut Vec<&'a str>,
) {
    let bytes = value.as_bytes();
    let mut index = 0;
    while index + 2 < bytes.len() {
        if bytes[index] == b'$' && bytes[index + 1] == b'<' {
            if let Some(end) = value[index..].find('>').map(|off| index + off) {
                let inner = &value[index + 2..end];
                let name = if let Some(rest) = inner.strip_prefix("TARGET_NAME:") {
                    Some(rest.trim_end_matches('>'))
                } else {
                    inner
                        .strip_prefix("TARGET_PROPERTY:")
                        .and_then(|rest| rest.split(',').next())
                };
                if let Some(name) = name
                    && let Some((key, target)) = state::find_target_entry(interp, name)
                    && emitted_deps.insert(target.name.clone())
                {
                    exports.push(key);
                }
                index = end;
            }
        }
        index += 1;
    }
}

// ---------------------------------------------------------------------------
// cmCoreTryCompile
// ---------------------------------------------------------------------------

/// `cmTryCompileResult`.
struct TryCompileResult {
    log_description: Option<String>,
    cmake_variables: BTreeMap<String, String>,
    source_directory: String,
    binary_directory: String,
    variable_cached: bool,
    variable: String,
    output: String,
    exit_code: i32,
}

/// Per-command state (`cmCoreTryCompile`).
struct TryCompile {
    binary_directory: String,
    output_file: String,
    find_error_message: String,
    src_file_signature: bool,
}

/// `cmCoreTryCompile::IsTemporary`.
fn is_temporary(path: &str) -> bool {
    path.contains("CMakeTmp") || path.contains("CMakeScratch")
}

/// `cmCoreTryCompile::WriteSource`.
fn write_source(
    interp: &mut Interp,
    bin_dir: &str,
    filename: &str,
    content: &str,
    command: &str,
) -> Option<String> {
    if !util::filename_path(filename).is_empty() {
        fatal(
            interp,
            format!("{command} given invalid filename \"{filename}\""),
        );
        return None;
    }
    let filepath = format!("{bin_dir}/{filename}");
    let mut file = match std::fs::File::create(&filepath) {
        Ok(file) => file,
        Err(_) => {
            fatal(
                interp,
                format!("{command} failed to open \"{filename}\" for writing"),
            );
            return None;
        }
    };
    use std::io::Write;
    match file.write_all(content.as_bytes()) {
        Ok(()) => Some(filepath),
        Err(_) => {
            fatal(interp, format!("{command} failed to write \"{filename}\""));
            None
        }
    }
}

/// `cmCoreTryCompile::CleanupFiles`.
fn cleanup_files(interp: &mut Interp, bin_dir: &str) {
    if bin_dir.is_empty() {
        return;
    }
    if !is_temporary(bin_dir) {
        crate::configure::system_error(
            Some(interp),
            &format!(
                "TRY_COMPILE attempt to remove -rf directory that does not contain CMakeTmp or CMakeScratch: \"{bin_dir}\""
            ),
        );
        return;
    }
    if let Ok(entries) = std::fs::read_dir(bin_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let file_name = name.to_string_lossy();
            if file_name == "." || file_name == ".." || file_name.starts_with(".nfs") {
                continue;
            }
            let full_path = format!("{bin_dir}/{file_name}");
            if path_is_symlink(&full_path) {
                let _ = std::fs::remove_file(&full_path);
            } else if std::path::Path::new(&full_path).is_dir() {
                cleanup_files(interp, &full_path);
                let _ = std::fs::remove_dir(&full_path);
            } else if let Err(error) = std::fs::remove_file(&full_path) {
                let detail = error
                    .raw_os_error()
                    .map(str_error)
                    .unwrap_or_else(|| error.to_string());
                fatal(
                    interp,
                    format!("The file:\n  {full_path}\ncould not be removed:\n  {detail}"),
                );
            }
        }
    }
    if bin_dir.contains("CMakeScratch") {
        let _ = std::fs::remove_dir(bin_dir);
    }
}

/// `cmsys::Status::GetString` — strerror text for an errno value.
fn str_error(code: i32) -> String {
    // SAFETY: strerror is thread-safe enough for our use and never returns
    // null for a valid errno.
    unsafe { std::ffi::CStr::from_ptr(libc::strerror(code)) }
        .to_string_lossy()
        .into_owned()
}

/// `cmCoreTryCompile::FindOutputFile` — multi-config always false (Ninja).
fn find_output_file(tc: &mut TryCompile, target_name: &str) {
    tc.find_error_message.clear();
    tc.output_file.clear();
    let mut tmp_output_file = format!("/{target_name}");
    // Single-config generator: no per-config suffix.
    tmp_output_file.push_str("_loc");

    let command = format!("{}{tmp_output_file}", tc.binary_directory);
    if !std::path::Path::new(&command).exists() {
        tc.find_error_message =
            format!("Unable to find the recorded try_compile output location:\n  {command}\n");
        return;
    }

    let output_file_location = std::fs::read_to_string(&command)
        .map(|content| content.lines().next().unwrap_or_default().to_owned())
        .unwrap_or_default();
    if !std::path::Path::new(&output_file_location).exists() {
        tc.find_error_message = format!(
            "Recorded try_compile output location doesn't exist:\n  {output_file_location}\n"
        );
        return;
    }

    tc.output_file = util::collapse_full_path(&output_file_location, "");
}

/// Emits the generated test-project CMakeLists.txt (the `fprintf` sequence
/// of `cmCoreTryCompile::TryCompileCode` for the source-file signature).
/// Returns the file text and, when imported LINK_LIBRARIES targets exist,
/// the `<target>Targets.cmake` content to write alongside it.
#[allow(clippy::too_many_arguments)]
fn emit_test_project(
    interp: &mut Interp,
    args: &mut Arguments,
    sources: &[(String, SourceType)],
    test_langs: &BTreeSet<String>,
    target_name: &str,
    target_type: TargetKind,
    targets: &[String],
    tc_config: &str,
    binary_dir: &str,
    cmake_variables: &mut BTreeMap<String, String>,
) -> Option<(String, Option<String>)> {
    let mut out = String::new();

    let module_path = interp.get_def("CMAKE_MODULE_PATH").map(str::to_owned);
    let (major, minor, patch, tweak) = {
        let (major, minor, patch) = crate::interp::VERSION;
        (major, minor, patch, 0)
    };
    out.push_str(&format!(
        "cmake_minimum_required(VERSION {major}.{minor}.{patch}.{tweak})\n"
    ));
    if let Some(def) = &module_path {
        out.push_str(&format!("set(CMAKE_MODULE_PATH \"{def}\")\n"));
        cmake_variables.insert("CMAKE_MODULE_PATH".to_owned(), def.clone());
    }

    /* Set MSVC runtime library policy to match our selection.  */
    if let Some(default) = interp.get_def("CMAKE_MSVC_RUNTIME_LIBRARY_DEFAULT") {
        out.push_str(&format!(
            "cmake_policy(SET CMP0091 {})\n",
            if !default.is_empty() { "NEW" } else { "OLD" }
        ));
    }

    /* Set Watcom runtime library policy to match our selection.  */
    if let Some(default) = interp.get_def("CMAKE_WATCOM_RUNTIME_LIBRARY_DEFAULT") {
        out.push_str(&format!(
            "cmake_policy(SET CMP0136 {})\n",
            if !default.is_empty() { "NEW" } else { "OLD" }
        ));
    }

    /* Set CUDA architectures policy to match outer project.  */
    if interp.policy_status("CMP0104") != PolicyStatus::New
        && test_langs.contains("CUDA")
        && interp
            .get_def("CMAKE_CUDA_ARCHITECTURES")
            .is_none_or(str::is_empty)
    {
        out.push_str("cmake_policy(SET CMP0104 OLD)\n");
    }

    /* Set ARMClang cpu/arch policy to match outer project.  */
    if let Some(cmp0123) = interp.get_def("CMAKE_ARMClang_CMP0123") {
        out.push_str(&format!(
            "cmake_policy(SET CMP0123 {})\n",
            if cmp0123 == "NEW" { "NEW" } else { "OLD" }
        ));
    }

    /* Set MSVC debug information format policy to match our selection.  */
    if let Some(default) = interp.get_def("CMAKE_MSVC_DEBUG_INFORMATION_FORMAT_DEFAULT") {
        out.push_str(&format!(
            "cmake_policy(SET CMP0141 {})\n",
            if !default.is_empty() { "NEW" } else { "OLD" }
        ));
    }

    /* Set MSVC runtime checks policy to match our selection.  */
    if let Some(default) = interp.get_def("CMAKE_MSVC_RUNTIME_CHECKS_DEFAULT") {
        out.push_str(&format!(
            "cmake_policy(SET CMP0184 {})\n",
            if !default.is_empty() { "NEW" } else { "OLD" }
        ));
    }

    /* Set cache/normal variable policy to match outer project.
    It may affect toolchain files.  */
    if interp.policy_status("CMP0126") != PolicyStatus::New {
        out.push_str("cmake_policy(SET CMP0126 OLD)\n");
    }

    /* Set language extensions policy to match outer project.  */
    if interp.policy_status("CMP0128") != PolicyStatus::New {
        out.push_str("cmake_policy(SET CMP0128 OLD)\n");
    }

    /* Set MSVC link -machine: policy to match outer project.  */
    if let Some(cmp0197) = interp.get_def("CMAKE_MSVC_CMP0197") {
        out.push_str(&format!(
            "cmake_policy(SET CMP0197 {})\n",
            if cmp0197 == "NEW" { "NEW" } else { "OLD" }
        ));
    }

    let mut project_langs = String::new();
    for lang in test_langs {
        project_langs.push_str(&format!(" {lang}"));
        let rules_override_base = "CMAKE_USER_MAKE_RULES_OVERRIDE";
        let rules_override_lang = format!("{rules_override_base}_{lang}");
        if let Some(path) = interp.get_def(&rules_override_lang).map(str::to_owned) {
            out.push_str(&format!("set({rules_override_lang} \"{path}\")\n"));
            cmake_variables.insert(rules_override_lang, path);
        } else if let Some(path) = interp.get_def(rules_override_base).map(str::to_owned) {
            out.push_str(&format!("set({rules_override_base} \"{path}\")\n"));
            cmake_variables.insert(rules_override_base.to_owned(), path);
        }
    }
    out.push_str(&format!("project(CMAKE_TRY_COMPILE{project_langs})\n"));
    if args.cmake_internal == "ABI" {
        // This is the ABI detection step, also used for implicit includes.
        // Erase any include_directories() calls from the toolchain file so
        // that we do not see them as implicit.  Our ABI detection source
        // does not include any system headers anyway.
        out.push_str("set_property(DIRECTORY PROPERTY INCLUDE_DIRECTORIES \"\")\n");

        // The link and compile lines for ABI detection step need to not use
        // response files so we can extract implicit includes given to
        // the underlying host compiler.
        for lang in ["CUDA", "HIP"] {
            if test_langs.contains(lang) {
                out.push_str(&format!(
                    "set(CMAKE_{lang}_USE_RESPONSE_FILE_FOR_INCLUDES OFF)\n"
                ));
                out.push_str(&format!(
                    "set(CMAKE_{lang}_USE_RESPONSE_FILE_FOR_LIBRARIES OFF)\n"
                ));
                out.push_str(&format!(
                    "set(CMAKE_{lang}_USE_RESPONSE_FILE_FOR_OBJECTS OFF)\n"
                ));
            }
        }
    }
    out.push_str("set(CMAKE_VERBOSE_MAKEFILE 1)\n");
    for lang in test_langs {
        let lang_flags = format!("CMAKE_{lang}_FLAGS");
        let flags = interp.get_def(&lang_flags).map(str::to_owned);
        out.push_str(&format!(
            "set(CMAKE_{lang}_FLAGS {})\n",
            util::escape_for_cmake(flags.as_deref().unwrap_or_default())
        ));
        out.push_str(&format!(
            "set(CMAKE_{lang}_FLAGS \"${{CMAKE_{lang}_FLAGS}} ${{COMPILE_DEFINITIONS}}\")\n"
        ));
        if let Some(flags) = flags {
            cmake_variables.insert(lang_flags, flags);
        }
    }
    match interp.policy_status("CMP0066") {
        PolicyStatus::Warn => {
            if policy_optional_warning_enabled(interp, "CMAKE_POLICY_WARNING_CMP0066") {
                let index = crate::policy::index_of("CMP0066").expect("CMP0066");
                let text = format!(
                    "{}\nFor compatibility with older versions of CMake, try_compile is not honoring caller config-specific compiler flags (e.g. CMAKE_C_FLAGS_DEBUG) in the test project.",
                    crate::policy::policy_warning(index)
                );
                interp.issue(MessageType::AuthorWarning, &text);
            }
        }
        PolicyStatus::Old => {}
        PolicyStatus::New => {
            // NEW behavior is to pass config-specific compiler flags.
            let cfg = if !tc_config.is_empty() {
                upper_case(tc_config)
            } else {
                TRY_COMPILE_DEFAULT_CONFIG.to_owned()
            };
            for lang in test_langs {
                let lang_flags_cfg = format!("CMAKE_{lang}_FLAGS_{cfg}");
                let flags_cfg = interp.get_def(&lang_flags_cfg).map(str::to_owned);
                out.push_str(&format!(
                    "set({lang_flags_cfg} {})\n",
                    util::escape_for_cmake(flags_cfg.as_deref().unwrap_or_default())
                ));
                if let Some(flags_cfg) = flags_cfg {
                    cmake_variables.insert(lang_flags_cfg, flags_cfg);
                }
            }
        }
    }
    {
        let exe_link_flags = interp.get_def("CMAKE_EXE_LINKER_FLAGS").map(str::to_owned);
        out.push_str(&format!(
            "set(CMAKE_EXE_LINKER_FLAGS {})\n",
            util::escape_for_cmake(exe_link_flags.as_deref().unwrap_or_default())
        ));
        if let Some(flags) = exe_link_flags {
            cmake_variables.insert("CMAKE_EXE_LINKER_FLAGS".to_owned(), flags);
        }
    }
    out.push_str("set(CMAKE_EXE_LINKER_FLAGS \"${CMAKE_EXE_LINKER_FLAGS} ${EXE_LINKER_FLAGS}\")\n");
    out.push_str("include_directories(${INCLUDE_DIRECTORIES})\n");
    out.push_str("set(CMAKE_SUPPRESS_REGENERATION 1)\n");
    out.push_str("link_directories(${LINK_DIRECTORIES})\n");
    // handle any compile flags we need to pass on
    if !args.compile_defs.is_empty() {
        // Pass using bracket arguments to preserve content.
        out.push_str(&format!(
            "add_definitions([==[{}]==])\n",
            args.compile_defs.join("]==] [==[")
        ));
    }

    let mut targets_file = None;
    if !targets.is_empty() {
        let fname = format!("/{target_name}Targets.cmake");
        let content = generate_targets_file(interp, targets, test_langs);
        targets_file = Some((fname.clone(), content));
        out.push_str(&format!(
            "\ninclude(\"${{CMAKE_CURRENT_LIST_DIR}}/{fname}\")\n"
        ));
        // Create all relevant alias targets.
        if let Some(link_libraries) = &args.link_libraries {
            for lib in link_libraries {
                if let Some(alias_target_name) = interp.ext.aliases.get(lib).cloned() {
                    if let Some(alias_target) =
                        state::find_target_to_use(interp, &alias_target_name)
                    {
                        // Create equivalent library/executable alias.
                        if alias_target.kind == TargetKind::Executable {
                            out.push_str(&format!(
                                "add_executable(\"{lib}\" ALIAS \"{alias_target_name}\")\n"
                            ));
                        } else {
                            out.push_str(&format!(
                                "add_library(\"{lib}\" ALIAS \"{alias_target_name}\")\n"
                            ));
                        }
                    }
                }
            }
        }
        out.push('\n');
    }

    /* Set the appropriate policy information for PIE link flags */
    out.push_str(&format!(
        "cmake_policy(SET CMP0083 {})\n",
        if interp.policy_status("CMP0083") == PolicyStatus::New {
            "NEW"
        } else {
            "OLD"
        }
    ));

    /* Set the appropriate policy information for C++ module support */
    out.push_str(&format!(
        "cmake_policy(SET CMP0155 {})\n",
        if interp.policy_status("CMP0155") == PolicyStatus::New {
            "NEW"
        } else {
            "OLD"
        }
    ));

    /* Set the appropriate policy information for Swift compilation mode */
    out.push_str(&format!(
        "cmake_policy(SET CMP0157 {})\n",
        if interp
            .get_def("CMAKE_Swift_COMPILATION_MODE_DEFAULT")
            .is_none_or(str::is_empty)
        {
            "OLD"
        } else {
            "NEW"
        }
    ));

    /* Set the appropriate policy information for the LINKER: prefix
    expansion */
    out.push_str(&format!(
        "cmake_policy(SET CMP0181 {})\n",
        if interp.policy_status("CMP0181") == PolicyStatus::New {
            "NEW"
        } else {
            "OLD"
        }
    ));

    // Workaround for -Wl,-headerpad_max_install_names issue until we can
    // avoid adding that flag in the platform and compiler language files.
    out.push_str("include(\"${CMAKE_ROOT}/Modules/Internal/HeaderpadWorkaround.cmake\")\n");

    match target_type {
        TargetKind::Executable => {
            /* Put the executable at a known location (for COPY_FILE).  */
            out.push_str(&format!(
                "set(CMAKE_RUNTIME_OUTPUT_DIRECTORY \"{binary_dir}\")\n"
            ));
            /* Create the actual executable.  */
            out.push_str(&format!("add_executable({target_name})\n"));
        }
        _ => {
            /* Put the static library at a known location (for COPY_FILE).  */
            out.push_str(&format!(
                "set(CMAKE_ARCHIVE_OUTPUT_DIRECTORY \"{binary_dir}\")\n"
            ));
            /* Create the actual static library.  */
            out.push_str(&format!("add_library({target_name} STATIC)\n"));
        }
    }
    out.push_str(&format!("target_sources({target_name} PRIVATE\n"));
    let mut file_set_name = String::new();
    let mut in_file_set = false;
    for (source, source_type) in sources {
        match source_type {
            SourceType::Normal => {
                if in_file_set {
                    out.push_str("  PRIVATE\n");
                    in_file_set = false;
                }
            }
            SourceType::CxxModule => {
                if !in_file_set {
                    file_set_name.push('a');
                    out.push_str(&format!(
                        "  PRIVATE FILE_SET {file_set_name} TYPE CXX_MODULES BASE_DIRS \"{}\" FILES\n",
                        interp.current_source_dir()
                    ));
                    in_file_set = true;
                }
            }
            SourceType::Directory => {}
        }
        out.push_str(&format!("  \"{source}\"\n"));

        // Add dependencies on any non-temporary sources.
        if !is_temporary(source) {
            interp.mf_mut().list_files.push(source.clone());
        }
    }
    out.push_str(")\n");

    /* Write out the output location of the target we are building */
    // Ninja is single-config: no per-config generator expression.
    out.push_str(&format!(
        "file(GENERATE OUTPUT \"${{CMAKE_BINARY_DIR}}/{target_name}_loc\"\n"
    ));
    out.push_str(&format!("     CONTENT $<TARGET_FILE:{target_name}>)\n"));

    let mut warn_cmp0067 = false;
    let mut honor_standard = true;

    if args.lang_props.is_empty() {
        match interp.policy_status("CMP0067") {
            PolicyStatus::Warn => {
                warn_cmp0067 =
                    policy_optional_warning_enabled(interp, "CMAKE_POLICY_WARNING_CMP0067");
                honor_standard = false;
            }
            PolicyStatus::Old => {
                honor_standard = false;
            }
            PolicyStatus::New => {}
        }
    }

    let mut warn_cmp0067_variables: Vec<String> = Vec::new();

    if honor_standard || warn_cmp0067 {
        const LANG_PROP_SUFFIXES: [&str; 3] = ["_STANDARD", "_STANDARD_REQUIRED", "_EXTENSIONS"];
        for lang in STANDARD_LANGUAGES {
            if !test_langs.contains(lang) {
                continue;
            }
            for suffix in LANG_PROP_SUFFIXES {
                let lang_prop = format!("{lang}{suffix}");
                let lang_prop_var = format!("CMAKE_{lang_prop}");
                if let std::collections::btree_map::Entry::Vacant(entry) =
                    args.lang_props.entry(lang_prop)
                {
                    let mut value = interp.get_safe(&lang_prop_var);
                    if warn_cmp0067 && !value.is_empty() {
                        value.clear();
                        warn_cmp0067_variables.push(lang_prop_var);
                    }
                    if !value.is_empty() {
                        entry.insert(value);
                    }
                }
            }
        }
    }

    if !warn_cmp0067_variables.is_empty() {
        let index = crate::policy::index_of("CMP0067").expect("CMP0067");
        let mut text = format!(
            "{}\nFor compatibility with older versions of CMake, try_compile is not honoring language standard variables in the test project:\n",
            crate::policy::policy_warning(index)
        );
        for var in &warn_cmp0067_variables {
            text.push_str(&format!("  {var}\n"));
        }
        interp.issue(MessageType::AuthorWarning, &text);
    }

    for (prop, value) in &args.lang_props {
        if value.is_empty() {
            continue;
        }
        out.push_str(&format!(
            "set_property(TARGET {target_name} PROPERTY {} {})\n",
            util::escape_for_cmake(prop),
            util::escape_for_cmake(value)
        ));
    }

    if !args.link_options.is_empty() {
        let options: Vec<String> = args
            .link_options
            .iter()
            .map(|option| util::escape_for_cmake(option))
            .collect();

        if target_type == TargetKind::StaticLibrary {
            out.push_str(&format!(
                "set_property(TARGET {target_name} PROPERTY STATIC_LIBRARY_OPTIONS {})\n",
                options.join(" ")
            ));
        } else {
            out.push_str(&format!(
                "target_link_options({target_name} PRIVATE {})\n",
                options.join(" ")
            ));
        }
    }

    if let Some(linker_language) = &args.linker_language {
        let linker_language = linker_language.clone();
        if !test_langs.contains(&linker_language) {
            fatal(
                interp,
                format!(
                    "Linker language '{linker_language}' must be enabled in project(LANGUAGES)."
                ),
            );
            return None;
        }

        out.push_str(&format!(
            "set_property(TARGET {target_name} PROPERTY LINKER_LANGUAGE {linker_language})\n"
        ));
    }

    if let Some(link_libraries) = &args.link_libraries {
        let mut libs_to_link = String::from(" ");
        for lib in link_libraries {
            libs_to_link.push_str(&format!("\"{}\" ", trim_whitespace(lib)));
        }
        out.push_str(&format!(
            "target_link_libraries({target_name} {libs_to_link})\n"
        ));
    } else {
        out.push_str(&format!(
            "target_link_libraries({target_name} ${{LINK_LIBRARIES}})\n"
        ));
    }

    Some((out, targets_file.map(|(_, content)| content)))
}

/// `cmCoreTryCompile::TryCompileCode`.
fn try_compile_code(
    interp: &mut Interp,
    args: &mut Arguments,
    mut target_type: TargetKind,
    tc: &mut TryCompile,
) -> Option<TryCompileResult> {
    tc.output_file.clear();
    // which signature were we called with ?
    tc.src_file_signature = true;

    let mut use_unique_binary_directory = false;
    let mut source_directory = String::new();
    let target_name;
    if args.project_name.is_some() {
        tc.src_file_signature = false;
        match &args.source_directory_or_file {
            Some(dir) if !dir.is_empty() => {
                source_directory = dir.clone();
            }
            _ => {
                fatal(interp, "No <srcdir> specified.".to_owned());
                return None;
            }
        }
        target_name = args.target_name.clone().unwrap_or_default();
    } else {
        /* Use a random file name to avoid rapid creation and deletion
        of the same executable name (some filesystems fail on that).  */
        target_name = format!("cmTC_{:05x}", random_number() & 0xFFFFF);
    }

    match &args.binary_directory {
        None => {
            fatal(interp, "No <bindir> specified.".to_owned());
            return None;
        }
        Some(dir) if dir.is_empty() => {
            fatal(interp, "No <bindir> specified.".to_owned());
            return None;
        }
        _ => {}
    }
    if args.binary_directory.as_deref() == Some(UNIQUE_BINARY_DIRECTORY) {
        // leave empty until we're ready to create it, so we don't try to
        // remove a non-existing directory if we abort due to e.g. bad args
        tc.binary_directory.clear();
        use_unique_binary_directory = true;
    } else {
        let dir = args.binary_directory.clone().unwrap_or_default();
        if !util::is_full_path(&dir) {
            fatal(
                interp,
                format!("<bindir> is not an absolute path:\n '{dir}'"),
            );
            return None;
        }
        tc.binary_directory = dir;
        // compute the binary dir when TRY_COMPILE is called with a src file
        // signature
        if tc.src_file_signature {
            tc.binary_directory += "/CMakeFiles/CMakeTmp";
        }
    }

    let mut targets: Vec<String> = Vec::new();
    if let Some(link_libraries) = &args.link_libraries {
        for lib in link_libraries {
            if let Some(target) = state::find_target_to_use(interp, lib) {
                let ok = match target.kind {
                    TargetKind::SharedLibrary
                    | TargetKind::StaticLibrary
                    | TargetKind::InterfaceLibrary
                    | TargetKind::UnknownLibrary => true,
                    TargetKind::Executable => target_is_executable_with_exports(target),
                    _ => false,
                };
                if !ok {
                    fatal(
                        interp,
                        format!(
                            "Only libraries may be used as try_compile or try_run IMPORTED LINK_LIBRARIES.  Got {} of type {}.",
                            target.name,
                            target_type_name(target.kind)
                        ),
                    );
                    return None;
                }
                if target.imported {
                    targets.push(lib.clone());
                }
            }
        }
    }

    if args.copy_file_to.as_deref() == Some("") {
        fatal(
            interp,
            "COPY_FILE must be followed by a file path".to_owned(),
        );
        return None;
    }

    if args.copy_file_error.as_deref() == Some("") {
        fatal(
            interp,
            "COPY_FILE_ERROR must be followed by a variable name".to_owned(),
        );
        return None;
    }

    if args.copy_file_error.is_some() && args.copy_file_to.is_none() {
        fatal(
            interp,
            "COPY_FILE_ERROR may be used only with COPY_FILE".to_owned(),
        );
        return None;
    }

    if args.sources.as_ref().is_some_and(Vec::is_empty) {
        fatal(
            interp,
            "SOURCES must be followed by at least one source file".to_owned(),
        );
        return None;
    }

    if tc.src_file_signature {
        if args
            .source_from_content
            .as_ref()
            .is_some_and(|v| v.len() % 2 == 1)
        {
            fatal(
                interp,
                "SOURCE_FROM_CONTENT requires exactly two arguments".to_owned(),
            );
            return None;
        }
        if args
            .source_from_var
            .as_ref()
            .is_some_and(|v| v.len() % 2 == 1)
        {
            fatal(
                interp,
                "SOURCE_FROM_VAR requires exactly two arguments".to_owned(),
            );
            return None;
        }
        if args
            .source_from_file
            .as_ref()
            .is_some_and(|v| v.len() % 2 == 1)
        {
            fatal(
                interp,
                "SOURCE_FROM_FILE requires exactly two arguments".to_owned(),
            );
            return None;
        }
        if !args.source_type_error.is_empty() {
            fatal(interp, args.source_type_error.clone());
            return None;
        }
    } else {
        // only valid for srcfile signatures
        if let Some(prop) = args.lang_props.keys().next() {
            fatal(
                interp,
                format!("{prop} allowed only in source file signature"),
            );
            return None;
        }
        if !args.compile_defs.is_empty() {
            fatal(
                interp,
                "COMPILE_DEFINITIONS allowed only in source file signature".to_owned(),
            );
            return None;
        }
        if args.copy_file_to.is_some() {
            fatal(
                interp,
                "COPY_FILE allowed only in source file signature".to_owned(),
            );
            return None;
        }
    }

    // make sure the binary directory exists
    if use_unique_binary_directory {
        tc.binary_directory = format!(
            "{}/CMakeFiles/CMakeScratch/TryCompile-XXXXXX",
            interp.home_binary
        );
        make_temp_directory(&mut tc.binary_directory);
    } else {
        make_directory(&tc.binary_directory);
    }

    // do not allow recursive try Compiles
    if tc.binary_directory == interp.home_binary {
        fatal(
            interp,
            format!(
                "Attempt at a recursive or nested TRY_COMPILE in directory\n  {}\n",
                tc.binary_directory
            ),
        );
        return None;
    }

    let mut cmake_variables: BTreeMap<String, String> = BTreeMap::new();

    let out_file_name = format!("{}/CMakeLists.txt", tc.binary_directory);
    // which signature are we using? If we are using var srcfile bindir
    if tc.src_file_signature {
        // remove any CMakeCache.txt files so we will have a clean test
        let cache_file = format!("{}/CMakeCache.txt", tc.binary_directory);
        remove_path(&cache_file);

        // Choose sources.
        let mut sources: Vec<(String, SourceType)> = Vec::new();
        if let Some(s) = args.sources.take() {
            sources = s;
        } else if let Some(dir_or_file) = &args.source_directory_or_file {
            sources.push((dir_or_file.clone(), SourceType::Directory));
        }
        if let Some(source_from_content) = args.source_from_content.take() {
            for chunk in source_from_content.chunks(2) {
                let name = &chunk[0].0;
                let content = &chunk[1].0;
                let out = write_source(
                    interp,
                    &tc.binary_directory,
                    name,
                    content,
                    "SOURCE_FROM_CONTENT",
                )?;
                sources.push((out, chunk[0].1));
            }
        }
        if let Some(source_from_var) = args.source_from_var.take() {
            for chunk in source_from_var.chunks(2) {
                let name = &chunk[0].0;
                let var = &chunk[1].0;
                let content = interp.get_def(var).unwrap_or_default().to_owned();
                let out = write_source(
                    interp,
                    &tc.binary_directory,
                    name,
                    &content,
                    "SOURCE_FROM_VAR",
                )?;
                sources.push((out, chunk[0].1));
            }
        }
        if let Some(source_from_file) = args.source_from_file.take() {
            for chunk in source_from_file.chunks(2) {
                let dst = &chunk[0].0;
                let src = &chunk[1].0;
                if !util::filename_path(dst).is_empty() {
                    fatal(
                        interp,
                        format!("SOURCE_FROM_FILE given invalid filename \"{dst}\""),
                    );
                    return None;
                }
                let dst_path = format!("{}/{dst}", tc.binary_directory);
                match copy_file_always(src, &dst_path) {
                    Ok(()) => {}
                    Err((message, _)) => {
                        fatal(
                            interp,
                            format!("SOURCE_FROM_FILE failed to copy \"{src}\": {message}"),
                        );
                        return None;
                    }
                }
                sources.push((dst_path, chunk[0].1));
            }
        }
        // TODO: ensure sources is not empty

        // Detect languages to enable.
        let mut test_langs: BTreeSet<String> = BTreeSet::new();
        for (source, _) in &sources {
            let ext = filename_last_extension(source);
            let lang = interp
                .langs
                .extension_to_language
                .get(ext.strip_prefix('.').unwrap_or(&ext))
                .cloned()
                .unwrap_or_default();
            if !lang.is_empty() {
                test_langs.insert(lang);
            } else {
                let mut langs: Vec<String> = interp.langs.enabled.clone();
                langs.sort();
                fatal(
                    interp,
                    format!(
                        "Unknown extension \"{ext}\" for file\n  {source}\ntry_compile() works only for enabled languages.  Currently these are:\n  {}\nSee project() command to enable other languages.",
                        langs.join(" ")
                    ),
                );
                return None;
            }
        }

        // when the only language is ISPC we know that the output
        // type must by a static library
        if test_langs.len() == 1 && test_langs.contains("ISPC") {
            target_type = TargetKind::StaticLibrary;
        }

        let tc_config = interp.get_safe("CMAKE_TRY_COMPILE_CONFIGURATION");

        // we need to create a directory and CMakeLists file etc...
        // first create the directories
        source_directory = tc.binary_directory.clone();

        // now create a CMakeLists.txt file in that directory
        let (cmakelists, targets_file) = emit_test_project(
            interp,
            args,
            &sources,
            &test_langs,
            &target_name,
            target_type,
            &targets,
            &tc_config,
            &tc.binary_directory,
            &mut cmake_variables,
        )?;
        if let Err(error) = std::fs::write(&out_file_name, &cmakelists) {
            fatal(
                interp,
                format!("Failed to open\n  {out_file_name}\n{error}"),
            );
            return None;
        }
        if let Some(content) = targets_file {
            let fname = format!("{}/{target_name}Targets.cmake", tc.binary_directory);
            if std::fs::write(&fname, content).is_err() {
                fatal(interp, "could not write export file.".to_owned());
                return None;
            }
        }
    }

    // Forward a set of variables to the inner project cache.
    if (tc.src_file_signature || interp.policy_status("CMP0137") == PolicyStatus::New)
        && !interp.is_on("CMAKE_TRY_COMPILE_NO_PLATFORM_VARIABLES")
    {
        let mut vars: BTreeSet<String> = BTreeSet::new();
        for lang in LANGUAGE_NAMES {
            vars.insert(format!("CMAKE_{lang}_COMPILER_EXTERNAL_TOOLCHAIN"));
            vars.insert(format!("CMAKE_{lang}_COMPILER_TARGET"));
        }
        for var in [
            "CMAKE_CUDA_ARCHITECTURES",
            "CMAKE_CUDA_RUNTIME_LIBRARY",
            "CMAKE_CXX_SCAN_FOR_MODULES",
            "CMAKE_ENABLE_EXPORTS",
            "CMAKE_EXECUTABLE_ENABLE_EXPORTS",
            "CMAKE_SHARED_LIBRARY_ENABLE_EXPORTS",
            "CMAKE_HIP_ARCHITECTURES",
            "CMAKE_HIP_PLATFORM",
            "CMAKE_HIP_RUNTIME_LIBRARY",
            "CMAKE_ISPC_INSTRUCTION_SETS",
            "CMAKE_ISPC_HEADER_SUFFIX",
            "CMAKE_LINK_SEARCH_END_STATIC",
            "CMAKE_LINK_SEARCH_START_STATIC",
            "CMAKE_OSX_ARCHITECTURES",
            "CMAKE_OSX_DEPLOYMENT_TARGET",
            "CMAKE_OSX_SYSROOT",
            "CMAKE_APPLE_ARCH_SYSROOTS",
            "CMAKE_POSITION_INDEPENDENT_CODE",
            "CMAKE_SYSROOT",
            "CMAKE_SYSROOT_COMPILE",
            "CMAKE_SYSROOT_LINK",
            "CMAKE_WARN_DEPRECATED",
            "CMAKE_MSVC_RUNTIME_LIBRARY",
            "CMAKE_WATCOM_RUNTIME_LIBRARY",
            "CMAKE_MSVC_DEBUG_INFORMATION_FORMAT",
            "CMAKE_MSVC_RUNTIME_CHECKS",
            "CMAKE_CXX_COMPILER_CLANG_SCAN_DEPS",
            "CMAKE_VS_USE_DEBUG_LIBRARIES",
        ] {
            vars.insert(var.to_owned());
        }

        if let Some(var_list_str) = interp.get_def("CMAKE_TRY_COMPILE_PLATFORM_VARIABLES") {
            for var in util::expand_list(var_list_str, false) {
                vars.insert(var);
            }
        }

        if interp.get_def("CMAKE_LINKER_TYPE").is_some() {
            // propagate various variables to support linker selection
            vars.insert("CMAKE_LINKER_TYPE".to_owned());
            let defs = interp.definition_names();
            let linker_type_def =
                crate::cmregex::Regex::new("^CMAKE_[A-Za-z_-]+_USING_LINKER_").ok();
            if let Some(regex) = linker_type_def {
                for def in defs {
                    if regex.find(&def).is_some() {
                        vars.insert(def);
                    }
                }
            }
        }

        if interp.policy_status("CMP0083") == PolicyStatus::New {
            // To ensure full support of PIE, propagate cache variables
            // driving the link options
            for lang in LANGUAGE_NAMES {
                vars.insert(format!("CMAKE_{lang}_LINK_NO_PIE_SUPPORTED"));
                vars.insert(format!("CMAKE_{lang}_LINK_PIE_SUPPORTED"));
            }
        }

        /* for the TRY_COMPILEs we want to be able to specify the architecture.
        So the user can set CMAKE_OSX_ARCHITECTURES to i386;ppc and then
        set CMAKE_TRY_COMPILE_OSX_ARCHITECTURES first to i386 and then to
        ppc to have the tests run for each specific architecture.  */
        if let Some(tc_archs) = interp
            .get_def("CMAKE_TRY_COMPILE_OSX_ARCHITECTURES")
            .map(str::to_owned)
        {
            vars.remove("CMAKE_OSX_ARCHITECTURES");
            args.cmake_flags
                .push(format!("-DCMAKE_OSX_ARCHITECTURES={tc_archs}"));
            cmake_variables.insert("CMAKE_OSX_ARCHITECTURES".to_owned(), tc_archs);
        }

        // Pass down CMAKE_EXPERIMENTAL_* feature flags.
        for var in EXPERIMENTAL_FORWARD_VARIABLES {
            vars.insert(var.to_owned());
        }

        for var in &vars {
            if let Some(value) = interp.get_def(var).map(str::to_owned) {
                args.cmake_flags.push(format!("-D{var}={value}"));
                cmake_variables.insert(var.clone(), value);
            }
        }
    }

    if !tc.src_file_signature
        && interp
            .global_props
            .get("PROPAGATE_TOP_LEVEL_INCLUDES_TO_TRY_COMPILE")
            .is_some_and(util::is_on)
    {
        let var = "CMAKE_PROJECT_TOP_LEVEL_INCLUDES";
        if let Some(value) = interp.get_def(var).map(str::to_owned) {
            args.cmake_flags.push(format!("-D{var}='{value}'"));
            cmake_variables.insert(var.to_owned(), value);
        }
    }

    // (UseGhsMultiIDE is always false for the Ninja generator; GHS platform
    // variables are forwarded only there.)

    if interp.debug_trycompile {
        let result_var = args.compile_result_variable.clone().unwrap_or_default();
        let text = format!(
            "Executing try_compile ({result_var}) in:\n  {}",
            tc.binary_directory
        );
        interp.issue(MessageType::Log, &text);
    }

    let erroroc = interp.error;
    interp.error = false;
    // actually do the try compile now that everything is setup
    let (res, output) = crate::configure::try_compile_project(
        interp,
        &source_directory,
        &tc.binary_directory,
        &target_name,
        tc.src_file_signature,
        Some(&args.cmake_flags),
    );
    if erroroc {
        interp.error = true;
    }

    // set the result var to the return value to indicate success or failure
    let result_var = args.compile_result_variable.clone().unwrap_or_default();
    if args.no_cache {
        interp.set_def(result_var.clone(), if res == 0 { "TRUE" } else { "FALSE" });
    } else {
        interp.add_cache_def(
            &result_var,
            Some(if res == 0 { "TRUE" } else { "FALSE" }),
            Some("Result of TRY_COMPILE"),
            CacheType::Internal,
            false,
        );
    }

    if let Some(output_variable) = &args.output_variable {
        interp.set_def(output_variable.clone(), output.clone());
    }

    if tc.src_file_signature {
        let mut copy_file_error_message = String::new();
        find_output_file(tc, &target_name);

        if res == 0
            && let Some(copy_file) = &args.copy_file_to
        {
            match copy_file_always(&tc.output_file, copy_file) {
                Ok(()) => {}
                Err((message, which)) => {
                    let mut text = message;
                    match which {
                        "input" => text.push_str(" (input)"),
                        "output" => text.push_str(" (output)"),
                        _ => {}
                    }
                    let error = format!(
                        "Cannot copy output executable\n  '{}'\nto destination specified by COPY_FILE:\n  '{copy_file}'\nbecause:\n  {text}\n{}",
                        tc.output_file, tc.find_error_message
                    );
                    if args.copy_file_error.is_none() {
                        fatal(interp, error);
                        return None;
                    }
                    copy_file_error_message = error;
                }
            }
        }

        if let Some(copy_file_error) = &args.copy_file_error {
            interp.set_def(copy_file_error.clone(), copy_file_error_message);
        }
    }

    Some(TryCompileResult {
        log_description: args.log_description.clone(),
        cmake_variables,
        source_directory,
        binary_directory: tc.binary_directory.clone(),
        variable_cached: !args.no_cache,
        variable: result_var,
        output,
        exit_code: res,
    })
}

/// `cmCoreTryCompile::WriteTryCompileEventFields`.
fn write_try_compile_event_fields(
    log: &mut crate::configure_log::ConfigureLog,
    result: &TryCompileResult,
) {
    if let Some(description) = &result.log_description {
        log.write_value_str("description", description);
    }
    log.begin_object("directories");
    log.write_value_str("source", &result.source_directory);
    log.write_value_str("binary", &result.binary_directory);
    log.end_object();
    if !result.cmake_variables.is_empty() {
        log.write_value_map("cmakeVariables", &result.cmake_variables);
    }
    log.begin_object("buildResult");
    log.write_value_str("variable", &result.variable);
    log.write_value_bool("cached", result.variable_cached);
    log.write_literal_text_block("stdout", &result.output);
    log.write_value_int("exitCode", result.exit_code.into());
    log.end_object();
}

/// `cmMakefile::GetOrCreateConfigureLog` equivalent — lazily creates the
/// configure log for this project (used on first event write).
fn configure_log<'a>(
    interp: &'a Interp,
) -> std::cell::RefMut<'a, crate::configure_log::ConfigureLog> {
    let log_dir = format!("{}/CMakeFiles", interp.home_binary);
    std::cell::RefMut::map(interp.ext.configure_log.borrow_mut(), |slot| {
        slot.get_or_insert_with(|| crate::configure_log::ConfigureLog::new(log_dir, Vec::new()))
    })
}

/// `WriteTryCompileEvent` (`LogVersionsWithTryCompileV1`).
fn write_try_compile_event(interp: &mut Interp, result: &TryCompileResult) {
    let mut log = configure_log(interp);
    if !log.is_any_log_version_enabled(&[1]) {
        return;
    }
    log.begin_event("try_compile-v1", interp);
    write_try_compile_event_fields(&mut log, result);
    log.end_event();
}

// ---------------------------------------------------------------------------
// try_compile command
// ---------------------------------------------------------------------------

fn try_compile_command(interp: &mut Interp, args: &[String]) -> CmdResult {
    if args.len() < 3 {
        return err("The try_compile() command requires at least 3 arguments.");
    }

    let mut target_type = TargetKind::Executable;
    if let Some(tt) = interp
        .get_def("CMAKE_TRY_COMPILE_TARGET_TYPE")
        .filter(|t| !t.is_empty())
    {
        if tt == "EXECUTABLE" {
            target_type = TargetKind::Executable;
        } else if tt == "STATIC_LIBRARY" {
            target_type = TargetKind::StaticLibrary;
        } else {
            return err(format!(
                "Invalid value '{tt}' for CMAKE_TRY_COMPILE_TARGET_TYPE.  Only 'EXECUTABLE' and 'STATIC_LIBRARY' are allowed."
            ));
        }
    }

    let Some(mut arguments) = parse_args(interp, args, false) else {
        return Ok(());
    };

    let mut tc = TryCompile {
        binary_directory: String::new(),
        output_file: String::new(),
        find_error_message: String::new(),
        src_file_signature: false,
    };
    let compile_result = try_compile_code(interp, &mut arguments, target_type, &mut tc);
    if let Some(result) = &compile_result
        && !arguments.no_log
    {
        write_try_compile_event(interp, result);
    }

    // if They specified clean then we clean up what we can
    if tc.src_file_signature && !interp.debug_trycompile {
        cleanup_files(interp, &tc.binary_directory);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// try_run command
// ---------------------------------------------------------------------------

struct TryRunResult {
    variable_cached: bool,
    variable: String,
    stdout: Option<String>,
    stderr: Option<String>,
    exit_code: Option<String>,
}

/// `WriteTryRunEvent`.
fn write_try_run_event(
    interp: &mut Interp,
    compile_result: &TryCompileResult,
    run_result: &TryRunResult,
) {
    let mut log = configure_log(interp);
    if !log.is_any_log_version_enabled(&[1]) {
        return;
    }
    log.begin_event("try_run-v1", interp);
    write_try_compile_event_fields(&mut log, compile_result);

    log.begin_object("runResult");
    log.write_value_str("variable", &run_result.variable);
    log.write_value_bool("cached", run_result.variable_cached);
    if let Some(stdout) = &run_result.stdout {
        log.write_literal_text_block("stdout", stdout);
    }
    if let Some(stderr) = &run_result.stderr {
        log.write_literal_text_block("stderr", stderr);
    }
    if let Some(exit_code) = &run_result.exit_code {
        if let Ok(code) = exit_code.parse::<i64>() {
            log.write_value_int("exitCode", code);
        } else {
            log.write_value_str("exitCode", exit_code);
        }
    }
    log.end_object();
    log.end_event();
}

fn try_run_command(interp: &mut Interp, args: &[String]) -> CmdResult {
    if args.len() < 4 {
        return err("The try_run() command requires at least 4 arguments.");
    }
    let mut tr = TryRun {
        run_result_variable: args[0].clone(),
        no_cache: false,
    };
    tr.try_run_code(interp, args)
}

struct TryRun {
    run_result_variable: String,
    no_cache: bool,
}

impl TryRun {
    /// `TryRunCommandImpl::TryRunCode`.
    fn try_run_code(&mut self, interp: &mut Interp, argv: &[String]) -> CmdResult {
        self.run_result_variable = argv[0].clone();
        let Some(mut arguments) = parse_args(interp, &argv[1..], true) else {
            return Ok(());
        };
        self.no_cache = arguments.no_cache;

        // although they could be used together, don't allow it, because
        // using OUTPUT_VARIABLE makes crosscompiling harder
        if arguments.output_variable.is_some()
            && (arguments.compile_output_variable.is_some()
                || arguments.run_output_variable.is_some()
                || arguments.run_output_stdout_variable.is_some()
                || arguments.run_output_stderr_variable.is_some())
        {
            crate::configure::system_error(
                Some(interp),
                "You cannot use OUTPUT_VARIABLE together with COMPILE_OUTPUT_VARIABLE , RUN_OUTPUT_VARIABLE, RUN_OUTPUT_STDOUT_VARIABLE or RUN_OUTPUT_STDERR_VARIABLE. Please use only COMPILE_OUTPUT_VARIABLE, RUN_OUTPUT_VARIABLE, RUN_OUTPUT_STDOUT_VARIABLE and/or RUN_OUTPUT_STDERR_VARIABLE.",
            );
            return Err(crate::interp::CmdErr::Reported);
        }

        if (arguments.run_output_stdout_variable.is_some()
            || arguments.run_output_stderr_variable.is_some())
            && arguments.run_output_variable.is_some()
        {
            crate::configure::system_error(
                Some(interp),
                "You cannot use RUN_OUTPUT_STDOUT_VARIABLE or RUN_OUTPUT_STDERR_VARIABLE together with RUN_OUTPUT_VARIABLE. Please use only COMPILE_OUTPUT_VARIABLE or RUN_OUTPUT_STDOUT_VARIABLE and/or RUN_OUTPUT_STDERR_VARIABLE.",
            );
            return Err(crate::interp::CmdErr::Reported);
        }

        if let Some(working_directory) = &arguments.run_working_directory {
            if !make_directory(working_directory) {
                crate::configure::system_error(
                    Some(interp),
                    &format!("Error creating working directory \"{working_directory}\"."),
                );
                return Err(crate::interp::CmdErr::Reported);
            }
        }

        let mut capture_run_output = false;
        if arguments.output_variable.is_some() {
            capture_run_output = true;
        } else if arguments.compile_output_variable.is_some() {
            arguments.output_variable = arguments.compile_output_variable.clone();
        }

        // Capture the split output for the configure log unless the caller
        // requests combined output to be captured by a variable.
        let mut capture_run_output_std_outerr = true;
        if arguments.run_output_stdout_variable.is_none()
            && arguments.run_output_stderr_variable.is_none()
        {
            if arguments.run_output_variable.is_some() {
                capture_run_output = true;
                capture_run_output_std_outerr = false;
            } else if arguments.output_variable.is_some() {
                capture_run_output_std_outerr = false;
            }
        }

        // do the try compile
        let mut tc = TryCompile {
            binary_directory: String::new(),
            output_file: String::new(),
            find_error_message: String::new(),
            src_file_signature: false,
        };
        let compile_result =
            try_compile_code(interp, &mut arguments, TargetKind::Executable, &mut tc);

        let mut run_result = TryRunResult {
            variable: self.run_result_variable.clone(),
            variable_cached: !arguments.no_cache,
            stdout: None,
            stderr: None,
            exit_code: None,
        };

        // now try running the command if it compiled
        if let Some(result) = &compile_result
            && result.exit_code == 0
        {
            if tc.output_file.is_empty() {
                crate::configure::system_error(Some(interp), &tc.find_error_message);
            } else {
                let run_args = arguments
                    .run_args
                    .as_ref()
                    .map(|run_args| format!(" {}", run_args.join(" ")))
                    .unwrap_or_default();

                // "run" it and capture the output
                let mut run_output_contents = String::new();
                let mut run_output_stdout_contents = String::new();
                let mut run_output_stderr_contents = String::new();
                let cross_compiling = interp.is_on("CMAKE_CROSSCOMPILING")
                    && !interp.is_def_set("CMAKE_CROSSCOMPILING_EMULATOR");
                if cross_compiling {
                    // We only require the stdout/stderr cache entries if the
                    // project actually asked for the values, not just for
                    // logging.
                    let std_out_err_required = arguments.run_output_stdout_variable.is_some()
                        || arguments.run_output_stderr_variable.is_some();
                    self.do_not_run_executable(
                        interp,
                        &tc,
                        &run_args,
                        arguments.source_directory_or_file.as_deref(),
                        &arguments
                            .compile_result_variable
                            .clone()
                            .unwrap_or_default(),
                        capture_run_output.then_some(&mut run_output_contents),
                        capture_run_output_std_outerr.then_some(&mut run_output_stdout_contents),
                        capture_run_output_std_outerr.then_some(&mut run_output_stderr_contents),
                        std_out_err_required,
                    );
                } else {
                    self.run_executable(
                        interp,
                        &tc,
                        &run_args,
                        arguments.run_working_directory.as_deref(),
                        capture_run_output.then_some(&mut run_output_contents),
                        capture_run_output_std_outerr.then_some(&mut run_output_stdout_contents),
                        capture_run_output_std_outerr.then_some(&mut run_output_stderr_contents),
                    );
                }

                if capture_run_output_std_outerr {
                    run_result.stdout = Some(run_output_stdout_contents.clone());
                    run_result.stderr = Some(run_output_stderr_contents.clone());
                } else {
                    run_result.stdout = Some(run_output_contents.clone());
                }

                if let Some(exit_code) = interp.get_def(&self.run_result_variable) {
                    run_result.exit_code = Some(exit_code.to_owned());
                }

                // now put the output into the variables
                if let Some(var) = &arguments.run_output_variable {
                    interp.set_def(var.clone(), run_output_contents.clone());
                }
                if let Some(var) = &arguments.run_output_stdout_variable {
                    interp.set_def(var.clone(), run_output_stdout_contents.clone());
                }
                if let Some(var) = &arguments.run_output_stderr_variable {
                    interp.set_def(var.clone(), run_output_stderr_contents.clone());
                }

                if arguments.output_variable.is_some()
                    && arguments.compile_output_variable.is_none()
                {
                    // if the TryCompileCore saved output in this outputVariable
                    // then prepend that output to this output
                    let var = arguments.output_variable.clone().expect("checked");
                    if let Some(compile_output) = interp.get_def(&var).map(str::to_owned) {
                        run_output_contents = format!("{compile_output}{run_output_contents}");
                    }
                    interp.set_def(var, run_output_contents);
                }
            }
        }

        if let Some(result) = &compile_result
            && !arguments.no_log
        {
            write_try_run_event(interp, result, &run_result);
        }

        // if we created a directory etc, then cleanup after ourselves
        if !interp.debug_trycompile {
            cleanup_files(interp, &tc.binary_directory);
        }
        Ok(())
    }

    /// `TryRunCommandImpl::RunExecutable` — runs the built executable through
    /// the emulator if one is set (`cmSystemTools::RunSingleCommand`).
    #[allow(clippy::too_many_arguments)]
    fn run_executable(
        &mut self,
        interp: &mut Interp,
        tc: &TryCompile,
        run_args: &str,
        work_dir: Option<&str>,
        out: Option<&mut String>,
        std_out: Option<&mut String>,
        std_err: Option<&mut String>,
    ) {
        let mut final_command = String::new();
        let emulator = interp.get_safe("CMAKE_CROSSCOMPILING_EMULATOR");
        if !emulator.is_empty() {
            let mut emulator_with_args = util::expand_list(&emulator, false);
            if let Some(program) = emulator_with_args.first().cloned() {
                final_command.push_str(&convert_to_run_command_path(&program));
                final_command.push(' ');
                emulator_with_args.remove(0);
                final_command.push_str(
                    &emulator_with_args
                        .iter()
                        .map(|arg| format!("\"{arg}\""))
                        .collect::<Vec<_>>()
                        .join(" "),
                );
                final_command.push(' ');
            }
        }
        final_command.push_str(&convert_to_run_command_path(&tc.output_file));
        if !run_args.is_empty() {
            final_command.push_str(run_args);
        }
        // stdOut || stdErr ? stdOut : out / stdErr : out
        let split = std_out.is_some() || std_err.is_some();
        let (capture_out, capture_err) = if split {
            (
                std_out.map(|s| s as *mut String),
                std_err.map(|s| s as *mut String),
            )
        } else {
            let shared = out.map(|s| s as *mut String);
            (shared, shared)
        };
        let worked = run_single_command(&final_command, capture_out, capture_err, work_dir);
        // set the run var
        let ret_str = match worked {
            Ok(code) => code.to_string(),
            Err(()) => "FAILED_TO_RUN".to_owned(),
        };
        if self.no_cache {
            interp.set_def(self.run_result_variable.clone(), ret_str);
        } else {
            interp.add_cache_def(
                &self.run_result_variable,
                Some(&ret_str),
                Some("Result of try_run()"),
                CacheType::Internal,
                false,
            );
        }
    }

    /// `TryRunCommandImpl::DoNotRunExecutable` — the cross-compiling path
    /// that creates the PLEASE_FILL_OUT cache variables and
    /// `TryRunResults.cmake`.
    #[allow(clippy::too_many_arguments)]
    fn do_not_run_executable(
        &mut self,
        interp: &mut Interp,
        tc: &TryCompile,
        run_args: &str,
        src_file: Option<&str>,
        compile_result_variable: &str,
        out: Option<&mut String>,
        std_out: Option<&mut String>,
        std_err: Option<&mut String>,
        std_out_err_required: bool,
    ) {
        // copy the executable out of the CMakeFiles/ directory, so it is not
        // removed at the end of try_run() and the user can run it manually
        // on the target platform.
        let copy_dest = format!(
            "{}/CMakeFiles/{}-{}{}",
            interp.home_binary,
            filename_without_extension(&tc.output_file),
            self.run_result_variable,
            filename_extension(&tc.output_file)
        );
        let _ = copy_file_always(&tc.output_file, &copy_dest);

        let result_file_name = format!("{}/TryRunResults.cmake", interp.home_binary);

        let details_string = format!("For details see {result_file_name}");

        let internal_run_output_name = format!("{}__TRYRUN_OUTPUT", self.run_result_variable);
        let internal_run_output_stdout_name =
            format!("{}__TRYRUN_OUTPUT_STDOUT", self.run_result_variable);
        let internal_run_output_stderr_name =
            format!("{}__TRYRUN_OUTPUT_STDERR", self.run_result_variable);
        let mut error = false;

        if interp.get_def(&self.run_result_variable).is_none() {
            // if the variables doesn't exist, create it with a helpful error
            // text and mark it as advanced
            let comment = format!(
                "Run result of try_run(), indicates whether the executable would have been able to run on its target platform.\n{details_string}"
            );
            interp.add_cache_def(
                &self.run_result_variable,
                Some("PLEASE_FILL_OUT-FAILED_TO_RUN"),
                Some(&comment),
                CacheType::String,
                false,
            );
            if let Some(entry) = interp.cache.entries.get_mut(&self.run_result_variable) {
                entry
                    .properties
                    .insert("ADVANCED".to_owned(), "1".to_owned());
            }
            error = true;
        }

        // is the output from the executable used ?
        if std_out_err_required {
            for (internal_name, stream) in [
                (&internal_run_output_stdout_name, "stdout"),
                (&internal_run_output_stderr_name, "stderr"),
            ] {
                if interp.get_def(internal_name).is_none() {
                    let comment = format!(
                        "Output of try_run(), contains the text, which the executable would have printed on {stream} on its target platform.\n{details_string}"
                    );
                    interp.add_cache_def(
                        internal_name,
                        Some("PLEASE_FILL_OUT-NOTFOUND"),
                        Some(&comment),
                        CacheType::String,
                        false,
                    );
                    if let Some(entry) = interp.cache.entries.get_mut(internal_name) {
                        entry
                            .properties
                            .insert("ADVANCED".to_owned(), "1".to_owned());
                    }
                    error = true;
                }
            }
        } else if out.is_some() && interp.get_def(&internal_run_output_name).is_none() {
            let comment = format!(
                "Output of try_run(), contains the text, which the executable would have printed on stdout and stderr on its target platform.\n{details_string}"
            );
            interp.add_cache_def(
                &internal_run_output_name,
                Some("PLEASE_FILL_OUT-NOTFOUND"),
                Some(&comment),
                CacheType::String,
                false,
            );
            if let Some(entry) = interp.cache.entries.get_mut(&internal_run_output_name) {
                entry
                    .properties
                    .insert("ADVANCED".to_owned(), "1".to_owned());
            }
            error = true;
        }

        let out = out.map(|s| s as *mut String);
        let std_out = std_out.map(|s| s as *mut String);
        let std_err = std_err.map(|s| s as *mut String);

        if error {
            // C++ keeps a function-static flag; only the first failed
            // try_run writes the header (later ones append).
            static FIRST_TRY_RUN: std::sync::atomic::AtomicBool =
                std::sync::atomic::AtomicBool::new(true);
            let first_try_run = FIRST_TRY_RUN.swap(false, std::sync::atomic::Ordering::Relaxed);
            let mut text = String::new();
            if first_try_run {
                text.push_str(
                    "# This file was generated by CMake because it detected try_run() commands\n# in crosscompiling mode. It will be overwritten by the next CMake run.\n# Copy it to a safe location, set the variables to appropriate values\n# and use it then to preset the CMake cache (using -C).\n\n",
                );
            }

            let mut comment = format!(
                "\n{}\n   indicates whether the executable would have been able to run on its\n   target platform. If so, set {} to\n   the exit code (in many cases 0 for success), otherwise enter \"FAILED_TO_RUN\".\n",
                self.run_result_variable, self.run_result_variable
            );
            if std_out.is_some() || std_err.is_some() {
                if std_out.is_some() {
                    comment.push_str(&format!(
                        "{internal_run_output_stdout_name}\n   contains the text the executable would have printed on stdout.\n   If the executable would not have been able to run, set {internal_run_output_stdout_name} empty.\n   Otherwise check if the output is evaluated by the calling CMake code. If so,\n   check what the source file would have printed when called with the given arguments.\n"
                    ));
                }
                if std_err.is_some() {
                    comment.push_str(&format!(
                        "{internal_run_output_stderr_name}\n   contains the text the executable would have printed on stderr.\n   If the executable would not have been able to run, set {internal_run_output_stderr_name} empty.\n   Otherwise check if the output is evaluated by the calling CMake code. If so,\n   check what the source file would have printed when called with the given arguments.\n"
                    ));
                }
            } else if out.is_some() {
                comment.push_str(&format!(
                    "{internal_run_output_name}\n   contains the text the executable would have printed on stdout and stderr.\n   If the executable would not have been able to run, set {internal_run_output_name} empty.\n   Otherwise check if the output is evaluated by the calling CMake code. If so,\n   check what the source file would have printed when called with the given arguments.\n"
                ));
            }

            comment.push_str(&format!(
                "The {compile_result_variable} variable holds the build result for this try_run().\n\n"
            ));
            if let Some(src_file) = src_file {
                comment.push_str(&format!("Source file   : {src_file}\n"));
            }
            comment.push_str(&format!(
                "Executable    : {copy_dest}\nRun arguments : {run_args}\n   Called from: {}",
                format_list_file_stack(interp)
            ));
            let comment = comment.replace('\n', "\n# ");
            text.push_str(&comment);
            text.push_str("\n\n");

            text.push_str(&format!(
                "set( {} \n     \"{}\"\n     CACHE STRING \"Result from try_run\" FORCE)\n\n",
                self.run_result_variable,
                interp.get_safe(&self.run_result_variable)
            ));

            if out.is_some() {
                text.push_str(&format!(
                    "set( {internal_run_output_name} \n     \"{}\"\n     CACHE STRING \"Output from try_run\" FORCE)\n\n",
                    interp.get_safe(&internal_run_output_name)
                ));
            }
            let mode = if first_try_run {
                std::fs::write(&result_file_name, &text)
            } else {
                use std::io::Write;
                std::fs::OpenOptions::new()
                    .append(true)
                    .open(&result_file_name)
                    .and_then(|mut file| file.write_all(text.as_bytes()))
            };
            let _ = mode;

            let mut error_message = format!(
                "try_run() invoked in cross-compiling mode, please set the following cache variables appropriately:\n   {} (advanced)\n",
                self.run_result_variable
            );
            if out.is_some() {
                error_message.push_str(&format!("   {internal_run_output_name} (advanced)\n"));
            }
            error_message.push_str(&details_string);
            crate::configure::system_error(Some(interp), &error_message);
            return;
        }

        if std_out.is_some() || std_err.is_some() {
            if let Some(std_out) = std_out {
                let value = interp
                    .get_def(&internal_run_output_stdout_name)
                    .unwrap_or_default()
                    .to_owned();
                unsafe {
                    *std_out = value;
                }
            }
            if let Some(std_err) = std_err {
                let value = interp
                    .get_def(&internal_run_output_stderr_name)
                    .unwrap_or_default()
                    .to_owned();
                unsafe {
                    *std_err = value;
                }
            }
        } else if let Some(out) = out {
            let value = interp
                .get_def(&internal_run_output_name)
                .unwrap_or_default()
                .to_owned();
            unsafe {
                *out = value;
            }
        }
    }
}

/// `cmMakefile::FormatListFileStack` — the add_subdirectory call stack.
fn format_list_file_stack(interp: &Interp) -> String {
    let mut files = Vec::new();
    let mut index = Some(interp.mf_index());
    while let Some(i) = index {
        let mf = &interp.makefiles[i];
        if let Some(file) = mf.list_files.last() {
            files.push(file.clone());
        }
        index = mf.parent;
    }
    let mut out = String::new();
    for (depth, file) in files.iter().rev().enumerate() {
        if depth > 0 {
            out.push_str("\n                ");
        }
        out.push_str(&format!("[{}]\t{file}", depth + 1));
    }
    out
}

/// `cmSystemTools::ConvertToRunCommandPath` (POSIX: unix slashes).
fn convert_to_run_command_path(path: &str) -> String {
    path.replace('\\', "/")
}

/// `cmSystemTools::ParseArguments`: splits a command line into argv,
/// handling double/single quotes and backslash escapes. No shell involved.
pub(crate) fn parse_arguments(command: &str) -> Vec<String> {
    let bytes = command.as_bytes();
    // The Windows-path special case never triggers for unix paths.
    let win_path = false;
    let mut args: Vec<String> = Vec::new();
    let mut arg: Vec<u8> = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        // Skip over whitespace.
        while index < bytes.len() && (bytes[index] == b' ' || bytes[index] == b'\t') {
            index += 1;
        }
        arg.clear();
        if index < bytes.len() && bytes[index] == b'"' {
            // Parse a quoted argument.
            index += 1;
            while index < bytes.len() && bytes[index] != b'"' {
                arg.push(bytes[index]);
                index += 1;
            }
            if index < bytes.len() {
                index += 1;
            }
            args.push(String::from_utf8_lossy(&arg).into_owned());
        } else if index < bytes.len() && bytes[index] == b'\'' {
            index += 1;
            while index < bytes.len() && bytes[index] != b'\'' {
                arg.push(bytes[index]);
                index += 1;
            }
            if index < bytes.len() {
                index += 1;
            }
            args.push(String::from_utf8_lossy(&arg).into_owned());
        } else if index < bytes.len() {
            // Parse an unquoted argument.
            while index < bytes.len() && bytes[index] != b' ' && bytes[index] != b'\t' {
                if bytes[index] == b'\\' && !win_path {
                    index += 1;
                    if index < bytes.len() {
                        arg.push(bytes[index]);
                        index += 1;
                    }
                } else {
                    arg.push(bytes[index]);
                    index += 1;
                }
            }
            args.push(String::from_utf8_lossy(&arg).into_owned());
        }
    }
    args
}

/// Runs a command directly (no shell), capturing stdout and stderr into
/// the given buffers (same buffer = merged stream,
/// `cmSystemTools::RunSingleCommand` with `OUTPUT_NONE`). `Err` means the
/// process could not be started (`FAILED_TO_RUN`).
fn run_single_command(
    command: &str,
    stdout: Option<*mut String>,
    stderr: Option<*mut String>,
    dir: Option<&str>,
) -> Result<i32, ()> {
    use std::io::Read;

    let args = parse_arguments(command);
    if args.is_empty() {
        return Err(());
    }
    let merged = match (stdout, stderr) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    };

    let mut cmd = std::process::Command::new(&args[0]);
    cmd.args(&args[1..]).stdin(std::process::Stdio::null());
    if let Some(dir) = dir {
        cmd.current_dir(dir);
    }
    let capture_out = stdout.is_some() || merged;
    let capture_err = stderr.is_some() && !merged;
    if merged {
        use std::os::unix::io::FromRawFd;
        let mut fds = [0i32; 2];
        // SAFETY: valid out-pointer for two fds.
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
            return Err(());
        }
        // SAFETY: fds from pipe() above are owned here.
        let (writer, writer2) = unsafe {
            let dup = libc::dup(fds[1]);
            (
                std::process::Stdio::from_raw_fd(fds[1]),
                std::process::Stdio::from_raw_fd(dup),
            )
        };
        cmd.stdout(writer).stderr(writer2);
        let mut child = cmd.spawn().map_err(|_| ())?;
        // Close the parent's copies of the write ends so the read sees EOF.
        drop(cmd);
        // SAFETY: reader fd from pipe().
        let mut reader = unsafe { std::fs::File::from_raw_fd(fds[0]) };
        let mut data = Vec::new();
        let _ = reader.read_to_end(&mut data);
        let status = child.wait().map_err(|_| ())?;
        unsafe {
            *stdout.expect("merged out") = nulls_to_spaces(&data);
        }
        return Ok(exit_status_code(&status));
    }
    cmd.stdout(if capture_out {
        std::process::Stdio::piped()
    } else {
        std::process::Stdio::null()
    });
    cmd.stderr(if capture_err {
        std::process::Stdio::piped()
    } else {
        std::process::Stdio::null()
    });
    let mut child = cmd.spawn().map_err(|_| ())?;
    // Read both pipes concurrently to avoid a full-pipe deadlock.
    let err_reader = child.stderr.take().map(|mut pipe| {
        std::thread::spawn(move || {
            let mut data = Vec::new();
            let _ = pipe.read_to_end(&mut data);
            data
        })
    });
    let mut out_data = Vec::new();
    if let Some(mut pipe) = child.stdout.take() {
        let _ = pipe.read_to_end(&mut out_data);
    }
    let err_data = err_reader
        .map(|handle| handle.join().unwrap_or_default())
        .unwrap_or_default();
    let status = child.wait().map_err(|_| ())?;
    unsafe {
        if let Some(out) = stdout {
            *out = nulls_to_spaces(&out_data);
        }
        if let Some(err) = stderr {
            *err = nulls_to_spaces(&err_data);
        }
    }
    Ok(exit_status_code(&status))
}

/// `cmUVProcessChain` status: exit code, or `-signal` like libuv.
fn exit_status_code(status: &std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    match status.code() {
        Some(code) => code,
        None => status.signal().map_or(-1, |signal| -signal),
    }
}

/// `RunSingleCommand` translates NUL bytes in captured output to spaces.
fn nulls_to_spaces(data: &[u8]) -> String {
    let mut data = data.to_vec();
    for byte in &mut data {
        if *byte == 0 {
            *byte = b' ';
        }
    }
    String::from_utf8_lossy(&data).into_owned()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interp::Mode;
    use crate::props::Props;

    fn test_interp() -> Interp {
        Interp::new(Mode::Project, "<SRC>", "<BIN>", "")
    }

    const N: SourceType = SourceType::Normal;
    const M: SourceType = SourceType::CxxModule;

    struct EmitCase {
        name: &'static str,
        policies: &'static str,
        defs: &'static [(&'static str, &'static str)],
        langs: &'static [&'static str],
        sources: Vec<(&'static str, SourceType)>,
        binary_dir: &'static str,
        target_type: TargetKind,
        args: fn(&mut Arguments),
    }

    fn emit(case: &EmitCase) -> (String, Option<String>) {
        let mut interp = test_interp();
        if !case.policies.is_empty() {
            assert!(crate::commands::core::apply_policy_version_ex(
                &mut interp,
                case.policies,
                "",
                false
            ));
        }
        // Match the oracle captures: their parent project had
        // CMAKE_BUILD_TYPE=Debug, so Debug flag variables were set.
        for lang in case.langs {
            if !interp.is_set(&format!("CMAKE_{lang}_FLAGS_DEBUG")) {
                interp.set_def(format!("CMAKE_{lang}_FLAGS_DEBUG"), "-g");
            }
        }
        for (name, value) in case.defs {
            interp.set_def(*name, *value);
        }
        if case.name == "link_libs_imported" {
            let mut props = Props::default();
            props.set("IMPORTED_LOCATION", "/opt/libimpl.a".to_owned());
            props.set("INTERFACE_INCLUDE_DIRECTORIES", "/opt/inc".to_owned());
            props.set("SYSTEM", "ON".to_owned());
            state::register_target(
                &mut interp,
                state::Target {
                    imported: true,
                    props,
                    ..state::Target::new("implib", TargetKind::StaticLibrary, 0)
                },
            );
            interp
                .ext
                .aliases
                .insert("impalias".to_owned(), "implib".to_owned());
        }
        let mut args = Arguments::new();
        (case.args)(&mut args);
        let test_langs: BTreeSet<String> = case.langs.iter().map(|lang| lang.to_string()).collect();
        let tc_config = interp.get_safe("CMAKE_TRY_COMPILE_CONFIGURATION");
        let mut cmake_variables = BTreeMap::new();
        let targets: Vec<String> = if case.name == "link_libs_imported" {
            vec!["implib".to_owned(), "impalias".to_owned()]
        } else {
            Vec::new()
        };
        let sources: Vec<(String, SourceType)> = case
            .sources
            .iter()
            .map(|(s, t)| (s.to_string(), *t))
            .collect();
        emit_test_project(
            &mut interp,
            &mut args,
            &sources,
            &test_langs,
            "cmTC_NNNNN",
            case.target_type,
            &targets,
            &tc_config,
            case.binary_dir,
            &mut cmake_variables,
        )
        .expect("emit failed")
    }

    fn fixture(name: &str, file: &str) -> String {
        let path = format!(
            "{}/tests/data/try_compile/{name}/{file}",
            env!("CARGO_MANIFEST_DIR")
        );
        std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("cannot read {path}: {error}"))
    }

    fn check(case: &EmitCase) {
        let (text, _) = emit(case);
        let expected = fixture(case.name, "CMakeLists.txt");
        assert_eq!(text, expected, "case {}", case.name);
    }

    const TC: &str = "<BIN>/TryCompile-XXXXXX";

    #[test]
    fn generated_sources_basic() {
        check(&EmitCase {
            name: "sources_basic",
            policies: "3.20",
            defs: &[],
            langs: &["C"],
            sources: vec![("<SRC>/main.c", N)],
            binary_dir: TC,
            target_type: TargetKind::Executable,
            args: |_| {},
        });
    }

    #[test]
    fn generated_multi_lang() {
        check(&EmitCase {
            name: "multi_lang",
            policies: "3.20",
            defs: &[],
            langs: &["C", "CXX"],
            sources: vec![("<SRC>/a.c", N), ("<SRC>/b.cxx", N)],
            binary_dir: TC,
            target_type: TargetKind::Executable,
            args: |_| {},
        });
    }

    #[test]
    fn generated_source_from_content() {
        check(&EmitCase {
            name: "sfc",
            policies: "3.20",
            defs: &[],
            langs: &["C"],
            sources: vec![("<BIN>/TryCompile-XXXXXX/genmain.c", N)],
            binary_dir: TC,
            target_type: TargetKind::Executable,
            args: |args| {
                args.source_from_content = Some(vec![
                    ("genmain.c".to_owned(), SourceType::Normal),
                    ("int main(void){return 0;}\n".to_owned(), SourceType::Normal),
                ]);
            },
        });
    }

    #[test]
    fn generated_source_from_var() {
        check(&EmitCase {
            name: "sfv",
            policies: "3.20",
            defs: &[],
            langs: &["C"],
            sources: vec![("<BIN>/TryCompile-XXXXXX/genvar.c", N)],
            binary_dir: TC,
            target_type: TargetKind::Executable,
            args: |_| {},
        });
    }

    #[test]
    fn generated_source_from_file() {
        check(&EmitCase {
            name: "sff",
            policies: "3.20",
            defs: &[],
            langs: &["C"],
            sources: vec![("<BIN>/TryCompile-XXXXXX/copied.c", N)],
            binary_dir: TC,
            target_type: TargetKind::Executable,
            args: |_| {},
        });
    }

    #[test]
    fn generated_cdefs_escape() {
        check(&EmitCase {
            name: "cdefs_escape",
            policies: "3.20",
            defs: &[],
            langs: &["C"],
            sources: vec![("<BIN>/TryCompile-XXXXXX/ok.c", N)],
            binary_dir: TC,
            target_type: TargetKind::Executable,
            args: |args| {
                args.compile_defs = vec!["X=<SRC>".to_owned(), "Y=a".to_owned(), "b".to_owned()];
            },
        });
    }

    #[test]
    fn generated_compile_defs() {
        check(&EmitCase {
            name: "compile_defs",
            policies: "3.20",
            defs: &[],
            langs: &["C"],
            sources: vec![("<BIN>/TryCompile-XXXXXX/ok.c", N)],
            binary_dir: TC,
            target_type: TargetKind::Executable,
            args: |args| {
                args.compile_defs =
                    vec!["FOO=1".to_owned(), "-DBAR=xx".to_owned(), "BAZ".to_owned()];
            },
        });
    }

    #[test]
    fn generated_cmake_flags() {
        check(&EmitCase {
            name: "cmake_flags",
            policies: "3.20",
            defs: &[],
            langs: &["C"],
            sources: vec![("<BIN>/TryCompile-XXXXXX/ok.c", N)],
            binary_dir: TC,
            target_type: TargetKind::Executable,
            args: |args| args.cmake_flags.push("-DFOO=bar".to_owned()),
        });
    }

    #[test]
    fn generated_cxx_module() {
        check(&EmitCase {
            name: "cxx_module",
            policies: "3.30",
            defs: &[],
            langs: &["CXX"],
            sources: vec![
                ("<SRC>/mod.cxx", M),
                ("<SRC>/mod2.cxx", M),
                ("<SRC>/main.cxx", N),
            ],
            binary_dir: TC,
            target_type: TargetKind::Executable,
            args: |_| {},
        });
    }

    #[test]
    fn generated_cxx_module_content() {
        check(&EmitCase {
            name: "cxx_module_content",
            policies: "3.30",
            defs: &[],
            langs: &["CXX"],
            sources: vec![
                ("<BIN>/TryCompile-XXXXXX/normal.cxx", N),
                ("<BIN>/TryCompile-XXXXXX/mymod.cxx", M),
            ],
            binary_dir: TC,
            target_type: TargetKind::Executable,
            args: |_| {},
        });
    }

    #[test]
    fn generated_fwd_vars() {
        check(&EmitCase {
            name: "fwd_vars",
            policies: "3.20",
            defs: &[
                ("CMAKE_POSITION_INDEPENDENT_CODE", "ON"),
                ("CMAKE_SYSROOT", "/opt/root"),
                (
                    "CMAKE_TRY_COMPILE_PLATFORM_VARIABLES",
                    "MY_OWN_VAR;ALSO_THIS",
                ),
                ("MY_OWN_VAR", "hello"),
            ],
            langs: &["C"],
            sources: vec![("<BIN>/TryCompile-XXXXXX/ok.c", N)],
            binary_dir: TC,
            target_type: TargetKind::Executable,
            args: |_| {},
        });
    }

    #[test]
    fn generated_lang_props() {
        check(&EmitCase {
            name: "lang_props",
            policies: "3.20",
            defs: &[("CMAKE_C_STANDARD", "99"), ("CMAKE_CXX_EXTENSIONS", "ON")],
            langs: &["C", "CXX"],
            sources: vec![
                ("<BIN>/TryCompile-XXXXXX/ok.c", N),
                ("<BIN>/TryCompile-XXXXXX/okx.cxx", N),
            ],
            binary_dir: TC,
            target_type: TargetKind::Executable,
            args: |args| {
                args.lang_props
                    .insert("C_STANDARD".to_owned(), "11".to_owned());
                args.lang_props
                    .insert("C_STANDARD_REQUIRED".to_owned(), "ON".to_owned());
                args.lang_props
                    .insert("C_EXTENSIONS".to_owned(), "OFF".to_owned());
                args.lang_props
                    .insert("CXX_STANDARD".to_owned(), "17".to_owned());
            },
        });
    }

    #[test]
    fn generated_link_opts() {
        check(&EmitCase {
            name: "link_opts",
            policies: "3.20",
            defs: &[],
            langs: &["C"],
            sources: vec![("<BIN>/TryCompile-XXXXXX/ok.c", N)],
            binary_dir: TC,
            target_type: TargetKind::Executable,
            args: |args| {
                args.link_options =
                    vec!["-Wl,--as-needed".to_owned(), "-fsanitize=nope".to_owned()];
            },
        });
    }

    #[test]
    fn generated_linker_lang() {
        check(&EmitCase {
            name: "linker_lang",
            policies: "3.20",
            defs: &[],
            langs: &["CXX"],
            sources: vec![("<SRC>/main.cxx", N)],
            binary_dir: TC,
            target_type: TargetKind::Executable,
            args: |args| args.linker_language = Some("CXX".to_owned()),
        });
    }

    #[test]
    fn generated_log_opts() {
        check(&EmitCase {
            name: "log_opts",
            policies: "3.20",
            defs: &[],
            langs: &["C"],
            sources: vec![("<BIN>/TryCompile-XXXXXX/ok.c", N)],
            binary_dir: TC,
            target_type: TargetKind::Executable,
            args: |args| {
                args.no_cache = true;
                args.log_description = Some("my test desc".to_owned());
                args.output_variable = Some("out".to_owned());
            },
        });
    }

    #[test]
    fn generated_module_path() {
        check(&EmitCase {
            name: "module_path",
            policies: "3.20",
            defs: &[
                ("CMAKE_MODULE_PATH", "/my/mod;/other"),
                ("CMAKE_TRY_COMPILE_CONFIGURATION", "release"),
                ("CMAKE_C_FLAGS_RELEASE", "-O3 -DNDEBUG"),
            ],
            langs: &["C"],
            sources: vec![("<BIN>/TryCompile-XXXXXX/ok.c", N)],
            binary_dir: TC,
            target_type: TargetKind::Executable,
            args: |_| {},
        });
    }

    #[test]
    fn generated_old_srcfile() {
        check(&EmitCase {
            name: "old_srcfile",
            policies: "3.20",
            defs: &[],
            langs: &["C"],
            sources: vec![("<SRC>/hello.c", N)],
            binary_dir: "<BIN>",
            target_type: TargetKind::Executable,
            args: |_| {},
        });
    }

    #[test]
    fn generated_sources_multi() {
        check(&EmitCase {
            name: "sources_multi",
            policies: "3.20",
            defs: &[],
            langs: &["C"],
            sources: vec![("<SRC>/a.c", N), ("<SRC>/b.c", N)],
            binary_dir: TC,
            target_type: TargetKind::Executable,
            args: |_| {},
        });
    }

    #[test]
    fn generated_static_lib() {
        check(&EmitCase {
            name: "static_lib",
            policies: "3.20",
            defs: &[],
            langs: &["C"],
            sources: vec![("<BIN>/TryCompile-XXXXXX/lib.c", N)],
            binary_dir: TC,
            target_type: TargetKind::StaticLibrary,
            args: |args| {
                args.link_options = vec![
                    "-Xlinker".to_owned(),
                    "-z".to_owned(),
                    "-Xlinker".to_owned(),
                    "defs".to_owned(),
                ];
            },
        });
    }

    #[test]
    fn generated_try_run() {
        for name in ["try_run_full", "try_run_src"] {
            check(&EmitCase {
                name,
                policies: "3.20",
                defs: &[],
                langs: &["C"],
                sources: vec![(
                    if name == "try_run_full" {
                        "<BIN>/TryCompile-XXXXXX/r.c"
                    } else {
                        "<SRC>/run.c"
                    },
                    N,
                )],
                binary_dir: TC,
                target_type: TargetKind::Executable,
                args: |_| {},
            });
        }
    }

    #[test]
    fn generated_link_libs_plain() {
        check(&EmitCase {
            name: "link_libs_plain",
            policies: "3.20",
            defs: &[],
            langs: &["C"],
            sources: vec![("<BIN>/TryCompile-XXXXXX/ok.c", N)],
            binary_dir: TC,
            target_type: TargetKind::Executable,
            args: |args| {
                args.link_libraries = Some(vec!["m".to_owned(), "-lsomelib".to_owned()]);
            },
        });
    }

    #[test]
    fn generated_link_libs_imported() {
        let case = EmitCase {
            name: "link_libs_imported",
            policies: "3.20",
            defs: &[],
            langs: &["C"],
            sources: vec![("<BIN>/TryCompile-XXXXXX/ok.c", N)],
            binary_dir: TC,
            target_type: TargetKind::Executable,
            args: |args| {
                args.link_libraries = Some(vec![
                    "implib".to_owned(),
                    "impalias".to_owned(),
                    "m".to_owned(),
                ]);
            },
        };
        let (text, targets_file) = emit(&case);
        assert_eq!(text, fixture(case.name, "CMakeLists.txt"));
        assert_eq!(
            targets_file.expect("targets file"),
            fixture(case.name, "cmTC_NNNNNTargets.cmake")
        );
    }

    #[test]
    fn generated_abi_check() {
        check(&EmitCase {
            name: "abi_check",
            policies: "3.20",
            defs: &[],
            langs: &["C"],
            sources: vec![("<CMAKE_ROOT_PARENT>/Modules/CMakeCCompilerABI.c", N)],
            binary_dir: TC,
            target_type: TargetKind::Executable,
            args: |args| {
                args.cmake_internal = "ABI".to_owned();
                args.compile_defs = vec!["-v".to_owned()];
            },
        });
    }

    // -- argument parser -------------------------------------------------

    fn parse_argv(spec: &Spec, argv: &[&str]) -> (Arguments, Vec<String>) {
        let mut arguments = Arguments::new();
        let argv: Vec<String> = argv.iter().map(|arg| arg.to_string()).collect();
        let mut parser = Parser {
            spec,
            args: &mut arguments,
            keyword: "",
            keyword_values_seen: 0,
            keyword_values_expected: 0,
            active: None,
            done_with_positional: false,
            unparsed: Vec::new(),
        };
        parser.parse(&argv);
        let unparsed = std::mem::take(&mut parser.unparsed);
        (arguments, unparsed)
    }

    #[test]
    fn parser_missing_keyword_value() {
        let spec = try_compile_sources_spec();
        let (arguments, _) = parse_argv(&spec, &["RES", "SOURCES"]);
        assert_eq!(
            arguments.keyword_errors.get("SOURCES").map(String::as_str),
            Some("  missing required value\n")
        );
    }

    #[test]
    fn parser_empty_string_not_allowed() {
        let spec = try_compile_sources_spec();
        let (arguments, _) = parse_argv(&spec, &["RES", "LOG_DESCRIPTION", ""]);
        assert_eq!(
            arguments
                .keyword_errors
                .get("LOG_DESCRIPTION")
                .map(String::as_str),
            Some("  empty string not allowed\n")
        );
    }

    #[test]
    fn parser_source_type_context() {
        let spec = try_compile_sources_spec();
        let (arguments, unparsed) = parse_argv(
            &spec,
            &[
                "RES",
                "SOURCE_FROM_CONTENT",
                "a.cxx",
                "x",
                "SOURCES_TYPE",
                "CXX_MODULE",
                "SOURCE_FROM_CONTENT",
                "b.cxx",
                "y",
            ],
        );
        assert!(unparsed.is_empty());
        let content = arguments.source_from_content.expect("present");
        assert_eq!(content[0].1, SourceType::Normal);
        assert_eq!(content[1].1, SourceType::Normal);
        assert_eq!(content[2].1, SourceType::CxxModule);
        assert_eq!(content[3].1, SourceType::CxxModule);
        assert!(arguments.source_type_error.is_empty());
    }

    #[test]
    fn parser_source_type_error() {
        let spec = try_compile_sources_spec();
        let (mut arguments, _) = parse_argv(&spec, &["RES", "SOURCES_TYPE", "BOGUS"]);
        assert_eq!(
            arguments.source_type_error,
            "Invalid 'SOURCE_TYPE' 'BOGUS'; must be one of 'SOURCE' or 'CXX_MODULE'"
        );
        // a second bad value does not replace the first error
        arguments.set_source_type("ALSO_BAD");
        assert!(arguments.source_type_error.contains("BOGUS"));
    }

    #[test]
    fn parser_old_signature_positions() {
        let spec = try_compile_old_spec();
        let (arguments, _) = parse_argv(
            &spec,
            &[
                "RES",
                "/b",
                "/s/proj",
                "ProjName",
                "tgt",
                "COMPILE_DEFINITIONS",
                "X",
            ],
        );
        assert_eq!(arguments.compile_result_variable.as_deref(), Some("RES"));
        assert_eq!(arguments.binary_directory.as_deref(), Some("/b"));
        assert_eq!(
            arguments.source_directory_or_file.as_deref(),
            Some("/s/proj")
        );
        assert_eq!(arguments.project_name.as_deref(), Some("ProjName"));
        assert_eq!(arguments.target_name.as_deref(), Some("tgt"));
        assert_eq!(arguments.compile_defs, vec!["X".to_owned()]);
    }

    #[test]
    fn parser_unparsed_arguments() {
        let spec = try_compile_sources_spec();
        let (arguments, unparsed) = parse_argv(&spec, &["RES", "EXTRA", "SOURCES", "a.c", "JUNK"]);
        assert_eq!(unparsed, vec!["EXTRA".to_owned()]);
        // "JUNK" is a value of SOURCES, not an unparsed argument.
        let sources = arguments.sources.expect("present");
        assert_eq!(sources[1].0, "JUNK");
    }

    #[test]
    fn parser_compile_defs_list_expansion() {
        let spec = try_compile_sources_spec();
        let (arguments, _) = parse_argv(
            &spec,
            &[
                "RES",
                "SOURCES",
                "a.c",
                "COMPILE_DEFINITIONS",
                "X=1;Y=2",
                "Z",
            ],
        );
        assert_eq!(
            arguments.compile_defs,
            vec!["X=1".to_owned(), "Y=2".to_owned(), "Z".to_owned()]
        );
    }

    #[test]
    fn min_args_errors() {
        let mut interp = test_interp();
        match try_compile_command(&mut interp, &["R".to_owned(), "x".to_owned()]) {
            Err(crate::interp::CmdErr::Error(text)) => {
                assert_eq!(
                    text,
                    "The try_compile() command requires at least 3 arguments."
                )
            }
            other => panic!("expected error, got {other:?}"),
        }
        match try_run_command(
            &mut interp,
            &["R".to_owned(), "C".to_owned(), "x".to_owned()],
        ) {
            Err(crate::interp::CmdErr::Error(text)) => {
                assert_eq!(text, "The try_run() command requires at least 4 arguments.")
            }
            other => panic!("expected error, got {other:?}"),
        }
    }

    #[test]
    fn invalid_target_type() {
        let mut interp = test_interp();
        interp.set_def("CMAKE_TRY_COMPILE_TARGET_TYPE", "SHARED_LIBRARY");
        match try_compile_command(
            &mut interp,
            &["R".to_owned(), "SOURCES".to_owned(), "a.c".to_owned()],
        ) {
            Err(crate::interp::CmdErr::Error(text)) => assert_eq!(
                text,
                "Invalid value 'SHARED_LIBRARY' for CMAKE_TRY_COMPILE_TARGET_TYPE.  Only 'EXECUTABLE' and 'STATIC_LIBRARY' are allowed."
            ),
            other => panic!("expected error, got {other:?}"),
        }
    }

    #[test]
    fn script_mode_stub() {
        let mut interp = Interp::new(Mode::Script, "<SRC>", "<BIN>", "");
        match try_compile_command(&mut interp, &["R".to_owned(), "x".to_owned()]) {
            Ok(()) | Err(_) => {}
        }
        // In script mode only the stub is registered.
        assert!(interp.commands.contains_key("try_compile"));
    }
}
