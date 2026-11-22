//! Generator expressions for CMake 4.1.0.
mod artifacts;
mod dag;
mod list;
mod path;
mod preprocess;
mod tables;
mod target;

pub use artifacts::{ArtifactComponent, ArtifactKind};
pub use dag::DagChecker;
#[allow(unused_imports)]
pub use preprocess::{
    PreprocessContext, collect, contains_genex, find, is_valid_target_name, preprocess,
    replace_install_prefix, split, strip, strip_empty_list_elements,
};
#[allow(unused_imports)]
pub use target::{
    LinkDependency, UsageItem, system_include_directories, target_usage, usage_requirements,
};

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fmt;
use std::ops::Range;

use crate::policy::{PolicyStatus, policy_warning};
use crate::util::{self, CompareOp};

/// A host-owned index into the generator's list-file backtrace table.
pub type Backtrace = usize;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PropertyEntry {
    pub value: String,
    pub backtrace: Option<Backtrace>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Usage {
    Compile,
    Link,
}

pub trait GenexHost {
    /// cmMakefile::FindTargetToUse / cmLocalGenerator::FindGeneratorTargetToUse.
    fn target_exists(&self, name: &str) -> bool;
    /// cmGeneratorTarget::GetType, encoded as the CMake TYPE property.
    fn target_type(&self, name: &str) -> Option<&str>;
    /// cmGeneratorTarget::GetProperty, before generator expression evaluation.
    fn target_property_raw(&self, target: &str, prop: &str) -> Option<String>;
    /// cmGeneratorTarget::IsImported.
    fn is_imported(&self, target: &str) -> bool;
    /// cmMakefile::IsAlias followed by cmLocalGenerator::FindGeneratorTargetToUse.
    fn resolve_alias(&self, name: &str) -> Option<String>;
    /// cmGeneratorTarget::GetFullPath, GetSOName, GetPDBName and BuildBundleDirectory.
    fn target_artifact(
        &self,
        target: &str,
        kind: ArtifactKind,
        config: &str,
    ) -> Result<String, String>;
    /// cmGeneratorTarget::GetOutputName, GetFilePostfix, GetFilePrefix,
    /// GetFileSuffix and GetPDBOutputName. Returns the selected component.
    fn target_artifact_component(
        &self,
        target: &str,
        kind: ArtifactKind,
        component: ArtifactComponent,
        config: &str,
    ) -> Result<String, String>;
    /// cmGeneratorTarget::GetTargetObjectNames / GetObjectDirectory and
    /// cmTarget::GetMappedConfig for imported objects.
    fn target_objects(&self, target: &str, config: &str) -> Result<Vec<String>, String>;
    /// cmMakefile::GetDefinition.
    fn variable(&self, name: &str) -> Option<String>;
    /// cmMakefile::PolicyOptionalWarningEnabled for a
    /// `CMAKE_POLICY_WARNING_CMP<NNNN>` variable.
    fn policy_optional_warning(&self, var: &str) -> bool {
        self.variable(var)
            .as_deref()
            .is_some_and(crate::util::is_on)
    }
    /// cmGeneratorTarget::GetPolicyStatusCMP*.
    fn policy_status(&self, target: &str, policy: &str) -> PolicyStatus;

    /// cmLocalGenerator::GetPolicyStatus at the expression's evaluation site.
    fn local_policy_status(&self, policy: &str) -> PolicyStatus {
        self.policy_status("", policy)
    }
    /// cmGeneratorTarget::Get*Entries, preserving each entry's backtrace.
    fn property_entries(&self, target: &str, prop: &str) -> Vec<PropertyEntry> {
        self.target_property_raw(target, prop)
            .into_iter()
            .map(|value| PropertyEntry {
                value,
                backtrace: None,
            })
            .collect()
    }
    /// cmGeneratorTarget::GetBacktrace (the target's creation backtrace),
    /// for diagnostics upstream anchors there rather than at the entry.
    fn target_backtrace(&self, _target: &str) -> Option<Backtrace> {
        None
    }
    /// cmLocalGenerator::FindGeneratorTargetToUse in the named target's directory.
    fn resolve_target(&self, name: &str, _current_target: Option<&str>) -> Option<String> {
        self.resolve_alias(name)
            .or_else(|| self.target_exists(name).then(|| name.to_owned()))
    }
    /// cmGlobalGenerator::IsAlias.
    fn alias_global(&self, name: &str) -> bool {
        self.resolve_alias(name).is_some()
    }
    /// cmGlobalGenerator::GetName.
    fn generator_name(&self) -> &str {
        "Ninja"
    }
    /// cmTarget::GetMappedConfig. Returns the selected suffix including its underscore.
    fn mapped_imported_config(&self, _target: &str, _config: &str) -> Option<String> {
        None
    }
    /// cmGeneratorTarget::GetLinkerLanguage.
    fn linker_language(&self, target: &str, _config: &str) -> String {
        self.target_property_raw(target, "LINKER_LANGUAGE")
            .unwrap_or_default()
    }
    /// cmGeneratorTarget::LinkLanguagePropagatesToDependents.
    fn link_language_propagates(&self, target: &str) -> bool {
        self.target_type(target) == Some("STATIC_LIBRARY")
    }
    /// cmLinkImplementation::LanguageRuntimeLibraries for the requested
    /// compile or link language, in link order.
    fn language_runtime_dependencies(
        &self,
        _target: &str,
        _config: &str,
        _language: &str,
        _usage: Usage,
    ) -> Vec<LinkDependency> {
        Vec::new()
    }
    /// cmGeneratorTarget::GetLinkInterfaceLibraries / GetLinkImplementationLibraries.
    /// Return None to compute dependencies from raw properties. A resolved
    /// implementation list must already include direct overrides. Supply it
    /// for scoped or synthetic links and linker-language second-pass results.
    fn link_dependencies(
        &self,
        _target: &str,
        _head: &str,
        _config: &str,
        _language: Option<&str>,
        _usage: Usage,
        _interface: bool,
    ) -> Result<Option<Vec<LinkDependency>>, GenexError> {
        Ok(None)
    }
    /// cmGeneratorTarget::GetLinkInterfaceDependent*Property, including its
    /// implicit built-in properties and compatibility diagnostics.
    fn compatible_interface_property(
        &self,
        _target: &str,
        _prop: &str,
        _config: &str,
        _raw: Option<&str>,
        _evaluating_links: bool,
    ) -> Result<Option<String>, String> {
        Ok(None)
    }
    /// cmSystemTools::FileExists, used to validate imported include paths.
    fn path_exists(&self, path: &str) -> bool {
        std::path::Path::new(path).exists()
    }
    /// cmGeneratorTarget::IsDLLPlatform.
    fn dll_platform(&self, _target: &str) -> bool {
        self.variable("WIN32").as_deref().is_some_and(util::is_on)
            || self.variable("CYGWIN").as_deref().is_some_and(util::is_on)
    }
    /// cmGeneratorTarget::IsArchivedAIXSharedLibrary.
    fn archived_aix_shared_library(&self, _target: &str) -> bool {
        false
    }
    /// cmGeneratorTarget::IsBundleOnApple.
    fn apple_bundle(&self, _target: &str) -> bool {
        false
    }
    /// cmComputeLinkInformation::GetRuntimeDLLs followed by MaybeGetLocation.
    fn runtime_dlls(&self, _target: &str, _config: &str) -> Result<Vec<String>, String> {
        Ok(Vec::new())
    }
    /// cmTarget::HasKnownObjectFileLocation. An Err holds the reason suffix.
    fn known_object_location(&self, _target: &str) -> Result<(), String> {
        Ok(())
    }
    /// cmGeneratorTarget::GetObjectDirectory / ObjectDirectory (buildsystem placeholder).
    fn target_objects_for_buildsystem(
        &self,
        target: &str,
        config: &str,
    ) -> Result<Vec<String>, String> {
        self.target_objects(target, config)
    }
    /// cmMakefile::AddTargetObject.
    fn record_target_object(&mut self, _target: &str, _path: &str) {}
    /// cmMakefile::ClearMatches followed by StoreMatches in LIST:TRANSFORM,REPLACE.
    fn record_regex_match(&mut self, _input: &str, _groups: &[Option<(usize, usize)>; 10]) {}
    /// cmState::UseMSYSShell.
    fn msys_shell(&self) -> bool {
        self.generator_name() == "MSYS Makefiles"
    }
    /// cmState::UseWindowsShell.
    fn windows_shell(&self) -> bool {
        cfg!(windows)
    }
    /// cmGlobalGenerator::GetLanguageEnabled.
    fn language_enabled(&self, language: &str) -> bool {
        self.variable(&format!("CMAKE_{language}_COMPILER_LOADED"))
            .as_deref()
            .is_some_and(util::is_on)
    }
    /// cmTarget::GetSystemIncludeDirectories: the raw entries recorded by
    /// target_include_directories(SYSTEM) for the target itself.
    fn system_include_directories(&self, target: &str) -> Vec<String> {
        self.target_property_raw(target, "SYSTEM_INCLUDE_DIRECTORIES")
            .map(|value| vec![value])
            .unwrap_or_default()
    }
    /// cmGeneratorTarget::GetAllConfigCompileLanguages.
    fn compile_languages(&self, _target: &str, _config: &str) -> Vec<String> {
        Vec::new()
    }
    /// cmGeneratorTarget::GetObjectDirectory.
    fn object_directory(&self, _target: &str, _config: &str) -> String {
        String::new()
    }
    /// cmGeneratorTarget::GetSupportDirectory.
    fn support_directory(&self, _target: &str) -> String {
        String::new()
    }
    /// cmGeneratorTarget::IsInBuildSystem.
    fn in_build_system(&self, target: &str) -> bool {
        if self.is_imported(target) {
            return false;
        }
        match self.target_type(target) {
            Some("INTERFACE_LIBRARY") => self
                .target_property_raw(target, "SOURCES")
                .is_some_and(|sources| !sources.is_empty()),
            Some("UNKNOWN_LIBRARY") | None => false,
            Some(_) => true,
        }
    }
    /// cmStandardLevelResolver::HaveStandardAvailable. The returned standard
    /// is recorded if link-library evaluation succeeds despite the feature.
    fn standard_for_feature(
        &self,
        _target: &str,
        _config: &str,
        _language: &str,
        _feature: &str,
    ) -> Result<(), String> {
        Ok(())
    }
}

/// State inputs and outputs corresponding to cmGeneratorExpressionContext.
#[derive(Debug, Default)]
pub struct EvalContext<'a> {
    pub config: &'a str,
    pub head_target: Option<&'a str>,
    pub current_target: Option<&'a str>,
    pub language: Option<&'a str>,
    pub quiet: bool,
    pub dag: Option<&'a DagChecker<'a>>,
    pub backtrace: Option<Backtrace>,
    pub evaluate_for_buildsystem: bool,
    pub device_link: bool,
    pub context_sensitive: bool,
    pub head_sensitive: bool,
    pub link_language_sensitive: bool,
    pub seen_target_properties: BTreeSet<String>,
    pub source_sensitive_targets: BTreeSet<String>,
    pub referenced_targets: BTreeSet<String>,
    pub dependency_targets: BTreeSet<String>,
    pub max_language_standard: BTreeMap<(String, String), String>,
    /// Diagnostics raised via `IssueMessage` during evaluation
    /// (cmGeneratorExpressionNode WARNING/AUTHOR_WARNING paths), carrying
    /// the context backtrace in effect when they were raised.
    pub warnings: Vec<GenexWarning>,
}

/// A diagnostic issued during generator-expression evaluation, as by the
/// `$<IN_LIST>` CMP0085 warning or the `$<CONFIG>` invalid-name warning.
#[derive(Clone, Debug)]
pub struct GenexWarning {
    /// `context->Backtrace` when the diagnostic was raised.
    pub backtrace: Option<Backtrace>,
    /// true for AUTHOR_WARNING (`CMake Warning (dev)`), false for WARNING.
    pub dev: bool,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GenexError {
    pub expression: String,
    pub message: String,
    pub backtrace: Option<Backtrace>,
    pub loop_steps: Vec<(String, Option<Backtrace>)>,
    pub quiet: bool,
}

impl fmt::Display for GenexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.expression.is_empty() {
            return f.write_str(&self.message);
        }
        write!(
            f,
            "Error evaluating generator expression:\n  {}\n{}",
            self.expression, self.message
        )
    }
}

impl std::error::Error for GenexError {}

#[derive(Clone, Debug)]
pub struct Compiled {
    input: String,
    parts: Vec<Part>,
}

#[derive(Clone, Debug)]
enum Part {
    Text(Range<usize>),
    Expr {
        span: Range<usize>,
        identifier: Vec<Part>,
        parameters: Vec<Vec<Part>>,
    },
}

struct Frame {
    start: usize,
    parts: Vec<Vec<Part>>,
    separators: Vec<usize>,
}

pub fn parse(input: &str) -> Compiled {
    // cmCompiledGeneratorExpression's constructor profiles the compile.
    let _profile = crate::profiling::entry("genex_compile", input, || None);
    let mut frames = vec![Frame {
        start: 0,
        parts: vec![Vec::new()],
        separators: Vec::new(),
    }];
    let mut pos = 0;
    let mut text_start = 0;
    let bytes = input.as_bytes();
    while pos < bytes.len() {
        let nested = frames.len() > 1;
        let frame = frames.last_mut().unwrap();
        let token = bytes[pos];
        let opening = bytes[pos..].starts_with(b"$<");
        let separator = nested
            && match token {
                b':' => frame.parts.len() == 1,
                b',' => frame.parts.len() > 1,
                _ => false,
            };
        if !(opening || separator || (nested && token == b'>')) {
            pos += 1;
            continue;
        }
        if text_start < pos {
            frame
                .parts
                .last_mut()
                .unwrap()
                .push(Part::Text(text_start..pos));
        }
        if opening {
            frames.push(Frame {
                start: pos,
                parts: vec![Vec::new()],
                separators: Vec::new(),
            });
            pos += 2;
        } else if separator {
            frame.parts.push(Vec::new());
            frame.separators.push(pos);
            pos += 1;
        } else {
            let frame = frames.pop().unwrap();
            let mut parts = frame.parts.into_iter();
            pos += 1;
            frames
                .last_mut()
                .unwrap()
                .parts
                .last_mut()
                .unwrap()
                .push(Part::Expr {
                    span: frame.start..pos,
                    identifier: parts.next().unwrap(),
                    parameters: parts.collect(),
                });
        }
        text_start = pos;
    }
    if text_start < pos {
        frames
            .last_mut()
            .unwrap()
            .parts
            .last_mut()
            .unwrap()
            .push(Part::Text(text_start..pos));
    }
    // Reinsert an unmatched opener as text, retaining complete inner expressions.
    while frames.len() > 1 {
        let frame = frames.pop().unwrap();
        let last = frames.last_mut().unwrap().parts.last_mut().unwrap();
        extend_text(last, frame.start..frame.start + 2);
        let empty_termination = frame.separators.last().is_some_and(|&p| p + 1 == pos);
        for (index, mut pieces) in frame.parts.into_iter().enumerate() {
            if index != 0 {
                let sep = frame.separators[index - 1];
                extend_text(last, sep..sep + 1);
            }
            if index == 0 || !empty_termination {
                if let Some(Part::Text(span)) = pieces.first() {
                    extend_text(last, span.clone());
                    pieces.remove(0);
                }
                last.append(&mut pieces);
            }
        }
    }
    Compiled {
        input: input.to_owned(),
        parts: frames.pop().unwrap().parts.pop().unwrap(),
    }
}

fn extend_text(parts: &mut Vec<Part>, span: Range<usize>) {
    if let Some(Part::Text(previous)) = parts.last_mut() {
        previous.end += span.len();
    } else {
        parts.push(Part::Text(span));
    }
}

#[derive(Clone, Copy)]
struct Scope<'a> {
    head: Option<&'a str>,
    current: Option<&'a str>,
}

impl<'a> Scope<'a> {
    fn target(target: &'a str) -> Self {
        Self {
            head: Some(target),
            current: Some(target),
        }
    }
}

struct Evaluator<'a, 'ctx> {
    ctx: &'a mut EvalContext<'ctx>,
    host: &'a mut dyn GenexHost,
    /// cmGeneratorExpressionContext::Language, which nested link-item
    /// evaluation replaces with the head target's linker language.
    language: String,
}

pub fn evaluate(
    compiled: &Compiled,
    ctx: &mut EvalContext<'_>,
    host: &mut dyn GenexHost,
) -> Result<String, GenexError> {
    if find(&compiled.input).is_none() {
        return Ok(compiled.input.clone());
    }
    let scope = Scope {
        head: ctx.head_target,
        current: ctx.current_target.or(ctx.head_target),
    };
    let dag = ctx.dag;
    let language = ctx.language.unwrap_or_default().to_owned();
    Evaluator {
        ctx,
        host,
        language,
    }
    .parts(compiled, &compiled.parts, scope, dag)
}

#[derive(Clone, Copy)]
enum Arity {
    Ignore,
    Exactly(usize),
    Arbitrary(usize),
    AtLeast(usize),
    AtMostOne,
}

fn compiler_node(id: &str) -> Option<(&str, &str)> {
    let (lang, kind) = id.split_once("_COMPILER_")?;
    (matches!(
        lang,
        "C" | "CXX" | "OBJC" | "OBJCXX" | "CUDA" | "Fortran" | "HIP"
    ) && matches!(kind, "ID" | "VERSION" | "FRONTEND_VARIANT"))
    .then_some((lang, kind))
}

fn arity(id: &str) -> Option<Arity> {
    use Arity::*;
    Some(match id {
        "0"
        | "1"
        | "BUILD_INTERFACE"
        | "BUILD_LOCAL_INTERFACE"
        | "INSTALL_INTERFACE"
        | "LOWER_CASE"
        | "UPPER_CASE"
        | "MAKE_C_IDENTIFIER"
        | "TARGET_NAME"
        | "GENEX_EVAL" => Arbitrary(1),
        "JOIN" | "TARGET_GENEX_EVAL" => Arbitrary(2),
        "ANGLE-R" | "COMMA" | "SEMICOLON" | "QUOTE" | "CONFIGURATION" | "INSTALL_PREFIX" => Ignore,
        "NOT"
        | "BOOL"
        | "REMOVE_DUPLICATES"
        | "TARGET_EXISTS"
        | "TARGET_NAME_IF_EXISTS"
        | "TARGET_OBJECTS"
        | "TARGET_RUNTIME_DLLS"
        | "TARGET_RUNTIME_DLL_DIRS"
        | "TARGET_POLICY"
        | "LINK_ONLY"
        | "COMPILE_ONLY"
        | "SHELL_PATH" => Exactly(1),
        "STREQUAL"
        | "EQUAL"
        | "IN_LIST"
        | "PATH_EQUAL"
        | "VERSION_EQUAL"
        | "VERSION_GREATER"
        | "VERSION_GREATER_EQUAL"
        | "VERSION_LESS"
        | "VERSION_LESS_EQUAL" => Exactly(2),
        "IF" | "FILTER" => Exactly(3),
        "CONFIG" | "PLATFORM_ID" | "COMPILE_LANGUAGE" | "LINK_LANGUAGE" | "HOST_LINK"
        | "DEVICE_LINK" => AtLeast(0),
        "AND" | "OR" | "COMPILE_FEATURES" | "TARGET_PROPERTY" | "LINK_LIBRARY" | "LINK_GROUP" => {
            AtLeast(1)
        }
        "LIST" | "PATH" | "COMPILE_LANG_AND_ID" | "LINK_LANG_AND_ID" => AtLeast(2),
        _ if artifacts::artifact_node(id).is_some() => Exactly(1),
        _ => match compiler_node(id)?.1 {
            "VERSION" => AtMostOne,
            _ => AtLeast(0),
        },
    })
}

impl Evaluator<'_, '_> {
    fn error(&self, expression: &str, message: impl Into<String>) -> GenexError {
        GenexError {
            expression: expression.to_owned(),
            message: message.into(),
            backtrace: self.ctx.backtrace,
            loop_steps: Vec::new(),
            quiet: self.ctx.quiet,
        }
    }

    fn parts(
        &mut self,
        compiled: &Compiled,
        parts: &[Part],
        scope: Scope<'_>,
        dag: Option<&DagChecker>,
    ) -> Result<String, GenexError> {
        let mut result = String::new();
        for part in parts {
            match part {
                Part::Text(span) => result.push_str(&String::from_utf8_lossy(
                    &compiled.input.as_bytes()[span.clone()],
                )),
                Part::Expr {
                    span,
                    identifier,
                    parameters,
                } => {
                    let expr = &compiled.input[span.clone()];
                    // GeneratorExpressionContent::Evaluate
                    let _profile = crate::profiling::entry("genex_eval", expr, || None);
                    let id = self.parts(compiled, identifier, scope, dag)?;
                    let mut args: Vec<String> = Vec::new();
                    let count = parameters.len();
                    let Some(arity) = arity(&id) else {
                        return Err(self.error(
                            expr,
                            "Expression did not evaluate to a known generator expression",
                        ));
                    };
                    if matches!(id.as_str(), "0" | "INSTALL_INTERFACE") {
                        if count == 0 {
                            return Err(self
                                .error(expr, format!("$<{id}> expression requires a parameter.")));
                        }
                        continue;
                    }
                    for (index, parameter) in parameters.iter().enumerate() {
                        if let Arity::Arbitrary(expected) = arity {
                            if index + 1 == expected {
                                let mut tail = Vec::new();
                                for parts in &parameters[index..] {
                                    if id == "TARGET_NAME"
                                        && parts.iter().any(|p| matches!(p, Part::Expr { .. }))
                                    {
                                        return Err(self.error(
                                            expr,
                                            "$<TARGET_NAME> expression requires literal input.",
                                        ));
                                    }
                                    tail.push(self.parts(compiled, parts, scope, dag)?);
                                }
                                args.push(tail.join(","));
                                break;
                            }
                        }
                        let short_circuit_value = match id.as_str() {
                            "AND" if args.last().is_some_and(|arg| arg == "0") => Some("0"),
                            "OR" if args.last().is_some_and(|arg| arg == "1") => Some("1"),
                            "IF" if index > 0 && args[0] == (index - 1).to_string() => Some(""),
                            _ => None,
                        };
                        args.push(match short_circuit_value {
                            Some(value) => value.to_owned(),
                            None => self.parts(compiled, parameter, scope, dag)?,
                        });
                    }
                    if let Some(detail) = match arity {
                        Arity::Exactly(1) | Arity::Arbitrary(1) if args.len() != 1 => {
                            Some("exactly one parameter.".to_owned())
                        }
                        Arity::Exactly(n) | Arity::Arbitrary(n) if args.len() != n => {
                            Some(format!(
                                "{n} comma separated parameters, but got {} instead.",
                                args.len()
                            ))
                        }
                        Arity::AtLeast(1) if args.is_empty() => {
                            Some("at least one parameter.".to_owned())
                        }
                        Arity::AtLeast(2) if args.len() < 2 => {
                            Some("at least two parameters.".to_owned())
                        }
                        Arity::AtMostOne if args.len() > 1 => {
                            Some("one or zero parameters.".to_owned())
                        }
                        _ => None,
                    } {
                        return Err(
                            self.error(expr, format!("$<{id}> expression requires {detail}"))
                        );
                    }
                    let _exec = crate::profiling::entry("genex_exec", &id, || {
                        use crate::commands::string::json::Json;
                        let mut object = std::collections::BTreeMap::new();
                        if !args.is_empty() {
                            object.insert(
                                b"genexArgs".to_vec(),
                                Json::Array(
                                    args.iter()
                                        .map(|arg| Json::Str(arg.as_bytes().to_vec()))
                                        .collect(),
                                ),
                            );
                        }
                        Some(Json::Object(object))
                    });
                    result.push_str(&self.node(&id, &args, expr, scope, dag)?);
                }
            }
        }
        Ok(result)
    }

    fn dependent(
        &mut self,
        input: &str,
        scope: Scope<'_>,
        dag: Option<&DagChecker>,
    ) -> Result<String, GenexError> {
        if find(input).is_none() {
            return Ok(input.to_owned());
        }
        let compiled = parse(input);
        self.parts(&compiled, &compiled.parts, scope, dag)
    }

    fn policy_warning(&mut self, policy: &str, detail: &str) {
        if let Ok(index) = policy[3..].parse() {
            self.ctx.warnings.push(GenexWarning {
                backtrace: self.ctx.backtrace,
                dev: true,
                text: format!("{}{detail}", policy_warning(index)),
            });
        }
    }
}

fn boolean(value: bool) -> String {
    if value { "1" } else { "0" }.to_owned()
}

fn identifier(value: &str) -> bool {
    value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

fn cm_list(value: &str, keep_empty: bool) -> Vec<String> {
    if value.is_empty() {
        Vec::new()
    } else {
        util::expand_list(value, keep_empty)
    }
}

mod nodes;
#[cfg(test)]
mod tests;

/// Cross-config dependencies recorded by cmCustomCommandGenerator.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct CommandDependency {
    pub target: String,
    pub cross: bool,
    pub backtrace: Option<Backtrace>,
}

/// cmCustomCommandGenerator::EvaluateSplitConfigGenex. The config wrappers
/// affect the following top-level expressions in the same argument.
pub fn evaluate_custom_command<'a>(
    mut input: &str,
    ctx: &mut EvalContext<'a>,
    host: &mut dyn GenexHost,
    output_config: &'a str,
    command_config: &'a str,
    mut use_output_config: bool,
    dependencies: &mut BTreeSet<CommandDependency>,
) -> Result<String, GenexError> {
    let mut result = String::new();
    while let Some(start) = input.find("$<") {
        result.push_str(&input[..start]);
        let end = preprocess::balanced_end(input, start);
        let mut expression = &input[start..end.unwrap_or(input.len())];
        input = &input[end.unwrap_or(input.len())..];
        if end.is_some() {
            for (prefix, output) in [("$<COMMAND_CONFIG:", false), ("$<OUTPUT_CONFIG:", true)] {
                if let Some(inner) = expression.strip_prefix(prefix) {
                    expression = &inner[..inner.len() - 1];
                    use_output_config = output;
                    break;
                }
            }
        }
        let old_config = ctx.config;
        ctx.config = if use_output_config {
            output_config
        } else {
            command_config
        };
        let previous_dependencies = std::mem::take(&mut ctx.dependency_targets);
        let value = evaluate(&parse(expression), ctx, host);
        dependencies.extend(
            ctx.dependency_targets
                .iter()
                .map(|target| CommandDependency {
                    target: target.clone(),
                    cross: !use_output_config,
                    backtrace: ctx.backtrace,
                }),
        );
        ctx.dependency_targets.extend(previous_dependencies);
        ctx.config = old_config;
        result.push_str(&value?);
    }
    result.push_str(input);
    Ok(result)
}
