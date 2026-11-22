use std::collections::{BTreeSet, HashSet};
use std::path::Path;

use super::{Flags, append, append_options, cmake_space, shell, shell_with_make_vars};
use crate::generator::{ctx::GenCtx, target::SourceInfo};
use crate::policy::PolicyStatus;
use crate::props::Props;
use crate::state::{Target, TargetKind};
use crate::util::{expand_list, is_on, is_on_opt};

/// Per-object bindings for the Ninja build statement.
#[derive(Clone, Debug, Default)]
pub struct CompileVars {
    pub flags: String,
    /// FLAGS before cmGlobalNinjaGenerator::WriteVariable trims it (the
    /// value ExportObjectCompileCommand substitutes).
    pub flags_untrimmed: String,
    pub defines: String,
    pub includes: String,
}

#[cfg(test)]
pub fn compile_vars(
    ctx: &GenCtx,
    target: &Target,
    config: &str,
    source: &SourceInfo,
) -> CompileVars {
    compile_vars_with_source_props(ctx, target, config, source, &Props::default())
}

pub fn compile_vars_with_source_props(
    ctx: &GenCtx,
    target: &Target,
    config: &str,
    source: &SourceInfo,
    properties: &Props,
) -> CompileVars {
    let inputs = Flags {
        ctx,
        target,
        config,
        lang: &source.language,
    };
    let mut flags = String::new();
    if let Some(language) = properties.get("LANGUAGE")
        && inputs.policy(119) == PolicyStatus::New
    {
        Flags {
            lang: language,
            ..inputs
        }
        .append_feature(&mut flags, "EXPLICIT_LANGUAGE");
    }
    append(&mut flags, &target_flags(&inputs));
    if let Some(raw) = properties.get("COMPILE_FLAGS") {
        append(&mut flags, &inputs.eval(raw));
    }
    if let Some(options) = properties.get("COMPILE_OPTIONS") {
        // Source options have neither SHELL processing nor deduplication.
        append_options(&mut flags, expand_list(&inputs.eval(options), false));
    }
    // Precompile headers compile options (cmNinjaTargetGenerator::
    // ComputeFlagsForObject).
    let pch_source = crate::generator::pch::pch_source(ctx, target, config, &source.language);
    if !pch_source.is_empty() && properties.get("SKIP_PRECOMPILE_HEADERS").is_none() {
        let options = if source.path == pch_source {
            crate::generator::pch::pch_create_options(ctx, target, config, &source.language)
        } else {
            crate::generator::pch::pch_use_options(ctx, target, config, &source.language)
        };
        append_options(&mut flags, expand_list(&inputs.eval(&options), false));
    }
    CompileVars {
        flags_untrimmed: flags.clone(),
        flags: flags.trim_matches(cmake_space).to_owned(),
        defines: defines(&inputs, properties)
            .trim_matches(cmake_space)
            .to_owned(),
        includes: includes(&inputs, source, properties),
    }
}

fn target_flags(inputs: &Flags<'_, '_>) -> String {
    let mut flags = flags_blob(inputs);
    append_compile_options_tail(&mut flags, inputs);
    flags
}

fn target_flags_prefix(inputs: &Flags<'_, '_>) -> String {
    let mut flags = inputs.language_flags(false);
    if inputs.pic() == Some(true) {
        let pie = if inputs.target.kind == TargetKind::Executable {
            inputs
                .lang_var("COMPILE_OPTIONS_PIE")
                .filter(|value| !value.is_empty())
        } else {
            None
        };
        let pic = pie.or_else(|| inputs.lang_var("COMPILE_OPTIONS_PIC"));
        append_options(
            &mut flags,
            expand_list(pic.as_deref().unwrap_or_default(), false),
        );
    }
    if matches!(
        inputs.target.kind,
        TargetKind::SharedLibrary | TargetKind::ModuleLibrary
    ) {
        inputs.append_feature(&mut flags, "DLL");
    }
    let visibility = format!("{}_VISIBILITY_PRESET", inputs.lang);
    if let (Some(prefix), Some(preset)) = (
        inputs.lang_var("COMPILE_OPTIONS_VISIBILITY"),
        inputs.prop(&visibility),
    ) {
        if matches!(preset, "default" | "hidden" | "protected" | "internal") {
            append(&mut flags, &format!("{prefix}{preset}"));
        } else {
            // `cmLocalGenerator::AppendFeatureOptions`: cmSystemTools::Error
            // emits a bare single-line diagnostic.
            inputs.ctx.system_error(format!(
                "Target {} uses unsupported value \"{preset}\" for {visibility}. The supported values are: default, hidden, protected, and internal.",
                inputs.target.name
            ));
        }
    }
    if matches!(inputs.lang, "CXX" | "OBJCXX")
        && is_on_opt(inputs.prop("VISIBILITY_INLINES_HIDDEN"))
        && let Some(flag) = inputs.lang_var("COMPILE_OPTIONS_VISIBILITY_INLINES_HIDDEN")
    {
        append(&mut flags, &flag);
    }
    if let Some(color) = inputs.var("CMAKE_COLOR_DIAGNOSTICS")
        && !color.is_empty()
        && !crate::util::is_notfound(&color)
    {
        inputs.append_feature(
            &mut flags,
            if is_on(&color) {
                "COLOR_DIAGNOSTICS"
            } else {
                "COLOR_DIAGNOSTICS_OFF"
            },
        );
    }

    // cmMakefile::GetDefineFlags: add_definitions arguments that are not
    // -D/-/D definitions (initially " ").
    if let Some(raw) = inputs
        .ctx
        .interp
        .ext
        .directory_define_flags
        .get(&inputs.target.makefile)
    {
        append(&mut flags, raw);
    }
    flags
}

/// `cmLocalGenerator::GetTargetCompileFlags`: the `compileFlags` blob
/// (everything before `AddCompileOptions`'s per-entry appends).
fn flags_blob(inputs: &Flags<'_, '_>) -> String {
    target_flags_prefix(inputs)
}

/// The `AddCompileOptions` tail: COMPILE_FLAGS/regex-filtered options plus
/// COMPILE_WARNING_AS_ERROR feature flags.
fn append_compile_options_tail(flags: &mut String, inputs: &Flags<'_, '_>) {
    // GetCompileOptions already de-duplicated and split SHELL: options.
    let options = inputs.usage("COMPILE_OPTIONS");
    let raw = inputs.prop("COMPILE_FLAGS").unwrap_or_default();
    if let Some(pattern) = inputs.lang_var("FLAG_REGEX") {
        if let Ok(regex) = crate::cmregex::Regex::new(&pattern) {
            let allowed = windows_command_line(raw)
                .into_iter()
                .chain(options)
                .filter(|option| regex.find(option).is_some());
            append_options(flags, allowed);
        }
    } else {
        // Target COMPILE_FLAGS are literal. Source COMPILE_FLAGS are evaluated.
        append(flags, raw);
        append_options(flags, options);
    }
    if is_on_opt(inputs.prop("COMPILE_WARNING_AS_ERROR")) {
        inputs.append_feature(flags, "WARNING_AS_ERROR");
    }
}

/// cmLocalGenerator::AddCompileOptions uses Windows tokenization for the
/// legacy flag-regex path even when the generator runs on Unix.
fn windows_command_line(raw: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut input = raw.chars().peekable();
    while input.peek().is_some() {
        while input.next_if(|c| cmake_space(*c)).is_some() {}
        if input.peek().is_none() {
            break;
        }
        let mut arg = String::new();
        let mut quoted = false;
        while let Some(c) = input.peek().copied() {
            if !quoted && cmake_space(c) {
                break;
            }
            let mut slashes = 0;
            while input.next_if_eq(&'\\').is_some() {
                slashes += 1;
            }
            if input.next_if_eq(&'"').is_some() {
                for _ in 0..slashes / 2 {
                    arg.push('\\');
                }
                if slashes % 2 == 0 {
                    quoted = !quoted;
                } else {
                    arg.push('"');
                }
            } else if slashes != 0 {
                for _ in 0..slashes {
                    arg.push('\\');
                }
            } else {
                arg.extend(input.next());
            }
        }
        args.push(arg);
    }
    args
}

fn defines(inputs: &Flags<'_, '_>, properties: &Props) -> String {
    let mut direct = inputs.usage("COMPILE_DEFINITIONS");
    if matches!(
        inputs.target.kind,
        TargetKind::SharedLibrary | TargetKind::ModuleLibrary
    ) || (inputs.target.kind == TargetKind::Executable
        && is_on_opt(inputs.prop("ENABLE_EXPORTS")))
    {
        let symbol = inputs
            .prop("DEFINE_SYMBOL")
            .map(str::to_owned)
            .unwrap_or_else(|| {
                let name = format!("{}_EXPORTS", inputs.target.name);
                let mut symbol = String::new();
                if name.starts_with(|c: char| c.is_ascii_digit()) {
                    symbol.push('_');
                }
                symbol.extend(name.bytes().map(|c| {
                    if c.is_ascii_alphanumeric() {
                        char::from(c)
                    } else {
                        '_'
                    }
                }));
                symbol
            });
        direct.extend(expand_list(&symbol, false));
    }

    // CMP0043's OLD behavior was removed. Target and directory per-config
    // definitions are ignored, but Ninja still reads per-config source defs.
    let source = [
        "COMPILE_DEFINITIONS".to_owned(),
        format!("COMPILE_DEFINITIONS_{}", inputs.config.to_ascii_uppercase()),
    ]
    .iter()
    .filter_map(|name| properties.get(name))
    .flat_map(|value| expand_list(&inputs.eval(value), false))
    .collect();
    let prefix = inputs
        .lang_var("DEFINE_FLAG")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "-D".to_owned());
    let mut result = String::new();
    for definitions in [direct, source] {
        let mut sorted = BTreeSet::new();
        for definition in &definitions {
            if check_definition(definition) {
                sorted.insert(definition.strip_prefix("-D").unwrap_or(definition));
            }
        }
        for definition in sorted {
            let (name, value) = definition
                .split_once('=')
                .map_or((definition, None), |(name, value)| (name, Some(value)));
            let mut flag = format!("{prefix}{name}");
            if let Some(value) = value {
                flag.push('=');
                flag.push_str(&shell_with_make_vars(value));
            }
            append(&mut result, &flag);
        }
    }
    result
}

fn check_definition(definition: &str) -> bool {
    let reason = if definition
        .find(['(', '='])
        .is_some_and(|pos| definition.as_bytes()[pos] == b'(')
    {
        "Function-style preprocessor definitions may not be passed on the compiler command line because many compilers do not support it."
    } else if definition.contains('#') {
        "Preprocessor definitions containing '#' may not be passed on the compiler command line because many compilers do not support it."
    } else {
        return true;
    };
    eprintln!(
        "WARNING: {reason}\nCMake is dropping a preprocessor definition: {definition}\nConsider defining the macro in a (configured) header file.\n"
    );
    false
}

fn includes(inputs: &Flags<'_, '_>, source: &SourceInfo, properties: &Props) -> String {
    let mut system: HashSet<_> = inputs
        .usage("SYSTEM_INCLUDE_DIRECTORIES")
        .iter()
        .map(|dir| unix_slashes(dir))
        .collect();
    // cmGlobalGenerator::FinalizeTargetConfiguration marks the standard
    // include paths for every enabled language as system paths.
    for language in &inputs.ctx.interp.langs.enabled {
        if let Some(value) = inputs.var(&format!("CMAKE_{language}_STANDARD_INCLUDE_DIRECTORIES")) {
            system.extend(
                expand_list(&value, false)
                    .iter()
                    .map(|dir| unix_slashes(dir)),
            );
        }
    }
    // cmTarget::GetSystemIncludeDirectories: include_directories(SYSTEM) and
    // target_include_directories(SYSTEM) entries of the target itself.
    for entry in &inputs.target.system_include_directories {
        for dir in expand_list(&inputs.eval(entry), false) {
            system.insert(unix_slashes(&dir));
        }
    }
    // System classification from usage requirements
    // (INTERFACE_SYSTEM_INCLUDE_DIRECTORIES, imported targets, SYSTEM).
    for item in inputs.usage_items("INCLUDE_DIRECTORIES") {
        if item.system {
            system.insert(unix_slashes(&item.value));
        }
    }
    let mut source_dirs = Vec::new();
    let mut seen = HashSet::new();
    if let Some(value) = properties.get("INCLUDE_DIRECTORIES") {
        for dir in expand_list(&inputs.eval(value), false) {
            if !Path::new(&dir).is_absolute() {
                let name = Path::new(&source.path)
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or(&source.path);
                inputs.error(format!(
                    "Found relative path while evaluating include directories of \"{name}\":\n  \"{dir}\"\n"
                ));
                break;
            }
            let dir = unix_slashes(&dir);
            if seen.insert(dir.clone()) {
                source_dirs.push(dir);
            }
        }
    }
    let mut flags = include_flags(inputs, &source_dirs, &system);
    append(
        &mut flags,
        &include_flags(inputs, &target_includes(inputs), &system),
    );
    flags
}

fn unix_slashes(path: &str) -> String {
    let mut result = path.replace('\\', "/");
    while result.contains("//") {
        result = result.replace("//", "/");
    }
    if result.len() > 1 && result.ends_with('/') {
        result.pop();
    }
    result
}

fn real_path(path: &str) -> String {
    std::fs::canonicalize(path).map_or_else(
        |_| path.to_owned(),
        |path| path.to_string_lossy().into_owned(),
    )
}

fn target_includes(inputs: &Flags<'_, '_>) -> Vec<String> {
    let mut result = Vec::new();
    let mut seen = HashSet::new();
    let mut emit = |dir: &str| {
        if seen.insert(dir.to_owned()) {
            result.push(dir.to_owned());
        }
    };
    if is_on_opt(inputs.var("CMAKE_INCLUDE_CURRENT_DIR").as_deref()) {
        let directory = &inputs.ctx.interp.makefiles[inputs.target.makefile];
        emit(&directory.binary_dir);
        emit(&directory.source_dir);
    }
    let standard = expand_list(
        &inputs
            .lang_var("STANDARD_INCLUDE_DIRECTORIES")
            .unwrap_or_default(),
        false,
    );
    let standard: Vec<_> = standard.iter().map(|dir| unix_slashes(dir)).collect();
    let mut implicit = standard.clone();
    let implicit_start = implicit.len();
    if inputs.lang != "Fortran" {
        implicit.extend(
            expand_list(
                &inputs
                    .lang_var("IMPLICIT_INCLUDE_DIRECTORIES")
                    .unwrap_or_default(),
                false,
            )
            .iter()
            .map(|dir| unix_slashes(dir)),
        );
    }
    let exclude_usr = (inputs.lang == "HIP" && implicit.len() == implicit_start)
        || (matches!(inputs.lang, "C" | "CXX" | "CUDA")
            && implicit.iter().any(|dir| dir.ends_with("/usr/include")));
    let implicit: HashSet<_> = implicit.iter().map(|dir| real_path(dir)).collect();
    let cpath: HashSet<_> = if matches!(inputs.lang, "C" | "CXX") {
        std::env::var_os("CPATH")
            .iter()
            .flat_map(std::env::split_paths)
            .filter(|path| path.is_absolute())
            .map(|path| real_path(&path.to_string_lossy()))
            .collect()
    } else {
        HashSet::new()
    };
    let is_excluded = |dir: &str| {
        let real = real_path(dir);
        ((exclude_usr && dir == "/usr/include") || implicit.contains(&real))
            && !cpath.contains(&real)
    };

    let mut directories = Vec::new();
    for dir in inputs.usage("INCLUDE_DIRECTORIES") {
        if !Path::new(&dir).is_absolute() && !crate::util::is_off(&dir) {
            inputs.error(format!(
                "Found relative path while evaluating include directories of \"{}\":\n  \"{dir}\"\n",
                inputs.target.name
            ));
            break;
        }
        let dir = unix_slashes(&dir);
        if !is_excluded(&dir) {
            directories.push(dir);
        }
    }
    if is_on_opt(
        inputs
            .var("CMAKE_INCLUDE_DIRECTORIES_PROJECT_BEFORE")
            .as_deref(),
    ) {
        for dir in &directories {
            let path = Path::new(dir);
            if path.starts_with(&inputs.ctx.interp.home_source)
                || path.starts_with(&inputs.ctx.interp.home_binary)
            {
                emit(dir);
            }
        }
    }
    for dir in &directories {
        emit(dir);
    }
    for dir in &standard {
        emit(dir);
    }
    result
}

fn include_flags(inputs: &Flags<'_, '_>, dirs: &[String], system: &HashSet<String>) -> String {
    if inputs.lang.is_empty() {
        return String::new();
    }
    let system_dir = |dir: &str| system.contains(dir.trim_end_matches('/'));
    let prefix = inputs
        .var(&format!("CMAKE_INCLUDE_FLAG_{}", inputs.lang))
        .unwrap_or_default();
    let separator = inputs.var(&format!("CMAKE_INCLUDE_FLAG_SEP_{}", inputs.lang));
    let repeat = separator.is_none();
    let separator = separator.as_deref().unwrap_or(" ");
    let system_prefix = repeat
        .then(|| inputs.var(&format!("CMAKE_INCLUDE_SYSTEM_FLAG_{}", inputs.lang)))
        .flatten();
    let quote_paths = inputs.var("CMAKE_QUOTE_INCLUDE_PATHS").is_some();

    let mut flags = String::new();
    let mut used_system = false;
    let sorted = dirs
        .iter()
        .filter(|dir| !system_dir(dir))
        .chain(dirs.iter().filter(|dir| system_dir(dir)));
    for (index, dir) in sorted.enumerate() {
        if index == 0 || repeat {
            if let Some(flag) = system_prefix.as_ref().filter(|_| system_dir(dir)) {
                used_system = true;
                flags.push_str(flag);
            } else {
                flags.push_str(&prefix);
            }
        }
        let path = shell_with_make_vars(dir);
        if quote_paths && !path.starts_with('"') {
            flags.push('"');
            flags.push_str(&path);
            flags.push('"');
        } else {
            flags.push_str(&path);
        }
        flags.push_str(separator);
    }
    if used_system
        && let Some(warning) = inputs.var(&format!(
            "CMAKE_INCLUDE_SYSTEM_FLAG_{}_WARNING",
            inputs.lang
        ))
    {
        flags.push_str(&warning);
    }
    if let Some(first) = separator.chars().next()
        && first != ' '
        && flags.ends_with(first)
    {
        flags.pop();
    }
    flags.trim_matches(cmake_space).to_owned()
}

/// Implicit dependencies from OBJECT_DEPENDS, before mapping to Ninja paths.
/// INTEGRATION: wire into ninja.rs once SourceInfo carries source props.
#[cfg(test)]
pub fn object_dependencies(properties: &Props) -> Vec<String> {
    expand_list(properties.get("OBJECT_DEPENDS").unwrap_or_default(), false)
        .into_iter()
        .map(|path| {
            if Path::new(&path).is_absolute() {
                crate::util::collapse_full_path(&path, "")
            } else {
                path
            }
        })
        .collect()
}

// ---------------------------------------------------------------------
// File API codemodel data (cmFileAPICodemodel.cxx CompileData fields).
// These mirror the target-level portions of GetTargetCompileFlags,
// GetTargetDefines, GetIncludeDirectories and GetPrecompileHeaders while
// preserving per-fragment backtraces.

/// One backtraced codemodel fragment: (value, backtrace).
pub(crate) type BtFrag = (String, Vec<crate::messages::Frame>);

/// `GetTargetCompileFlags` as codemodel "compileCommandFragments".
pub(crate) fn flag_fragments(
    ctx: &GenCtx,
    target: &Target,
    config: &str,
    lang: &str,
) -> Vec<BtFrag> {
    let inputs = Flags {
        ctx,
        target,
        config,
        lang,
    };
    let mut out = Vec::new();
    let blob = flags_blob(&inputs);
    if !blob.is_empty() {
        out.push((blob, Vec::new()));
    }
    let raw = inputs.prop("COMPILE_FLAGS").unwrap_or_default();
    if let Some(pattern) = inputs.lang_var("FLAG_REGEX") {
        if let Ok(regex) = crate::cmregex::Regex::new(&pattern) {
            for option in windows_command_line(raw)
                .into_iter()
                .filter(|option| regex.find(option).is_some())
            {
                out.push((option, Vec::new()));
            }
            for item in inputs.usage_items("COMPILE_OPTIONS") {
                if regex.find(&item.value).is_some() {
                    out.push((shell(&item.value), ctx.backtrace(item.backtrace)));
                }
            }
        }
    } else {
        if !raw.trim_matches(cmake_space).is_empty() {
            out.push((raw.to_owned(), Vec::new()));
        }
        for item in inputs.usage_items("COMPILE_OPTIONS") {
            out.push((shell(&item.value), ctx.backtrace(item.backtrace)));
        }
    }
    if is_on_opt(inputs.prop("COMPILE_WARNING_AS_ERROR")) {
        let mut w = String::new();
        inputs.append_feature(&mut w, "WARNING_AS_ERROR");
        if !w.is_empty() {
            out.push((w, Vec::new()));
        }
    }
    out
}

/// `GetTargetDefines` as codemodel "defines" (export macro first, then
/// usage definitions; std::set<BT> sorts by value, first insertion wins).
pub(crate) fn define_items(ctx: &GenCtx, target: &Target, config: &str, lang: &str) -> Vec<BtFrag> {
    let inputs = Flags {
        ctx,
        target,
        config,
        lang,
    };
    let mut items: Vec<BtFrag> = Vec::new();
    if matches!(
        inputs.target.kind,
        TargetKind::SharedLibrary | TargetKind::ModuleLibrary
    ) || (inputs.target.kind == TargetKind::Executable
        && is_on_opt(inputs.prop("ENABLE_EXPORTS")))
    {
        let symbol = inputs
            .prop("DEFINE_SYMBOL")
            .map(str::to_owned)
            .unwrap_or_else(|| {
                let name = format!("{}_EXPORTS", inputs.target.name);
                let mut symbol = String::new();
                if name.starts_with(|c: char| c.is_ascii_digit()) {
                    symbol.push('_');
                }
                symbol.extend(name.bytes().map(|c| {
                    if c.is_ascii_alphanumeric() {
                        char::from(c)
                    } else {
                        '_'
                    }
                }));
                symbol
            });
        for define in expand_list(&symbol, false) {
            items.push((define, Vec::new()));
        }
    }
    for item in inputs.usage_items("COMPILE_DEFINITIONS") {
        let value = item
            .value
            .strip_prefix("-D")
            .unwrap_or(&item.value)
            .to_owned();
        items.push((value, ctx.backtrace(item.backtrace)));
    }
    items.retain(|(d, _)| check_definition(d));
    items.sort_by(|a, b| a.0.cmp(&b.0));
    items.dedup_by(|a, b| a.0 == b.0);
    items
}

/// `cmLocalGenerator::GetIncludeDirectories` as codemodel
/// "includes"/"frameworks" entries: (path, is_system, backtrace).
pub(crate) fn include_items(
    ctx: &GenCtx,
    target: &Target,
    config: &str,
    lang: &str,
) -> Vec<(String, bool, Vec<crate::messages::Frame>)> {
    let inputs = Flags {
        ctx,
        target,
        config,
        lang,
    };
    let standard: Vec<String> = expand_list(
        &inputs
            .lang_var("STANDARD_INCLUDE_DIRECTORIES")
            .unwrap_or_default(),
        false,
    )
    .iter()
    .map(|dir| unix_slashes(dir))
    .collect();
    let mut implicit = standard.clone();
    if lang != "Fortran" {
        implicit.extend(
            expand_list(
                &inputs
                    .lang_var("IMPLICIT_INCLUDE_DIRECTORIES")
                    .unwrap_or_default(),
                false,
            )
            .iter()
            .map(|dir| unix_slashes(dir)),
        );
    }
    let implicit: HashSet<String> = implicit.iter().map(|dir| real_path(dir)).collect();
    let cpath: HashSet<String> = if matches!(lang, "C" | "CXX") {
        std::env::var_os("CPATH")
            .iter()
            .flat_map(std::env::split_paths)
            .filter(|path| path.is_absolute())
            .map(|path| real_path(&path.to_string_lossy()))
            .collect()
    } else {
        HashSet::new()
    };
    let not_excluded = |dir: &str| {
        let real = real_path(dir);
        !implicit.contains(&real) || cpath.contains(&real)
    };

    // (path, is_system, backtrace, is_user_entry)
    let mut users: Vec<(String, bool, Vec<crate::messages::Frame>, bool)> = Vec::new();
    for item in inputs.usage_items("INCLUDE_DIRECTORIES") {
        if !Path::new(&item.value).is_absolute() && !crate::util::is_off(&item.value) {
            break;
        }
        let dir = unix_slashes(&item.value);
        let system = item.system
            || target
                .system_include_directories
                .contains(dir.trim_end_matches('/'));
        users.push((dir, system, ctx.backtrace(item.backtrace), true));
    }

    let mut result: Vec<(String, bool, Vec<crate::messages::Frame>, bool)> = Vec::new();
    let mut seen = HashSet::new();
    let mut emit =
        |entry: (String, bool, Vec<crate::messages::Frame>, bool),
         result: &mut Vec<(String, bool, Vec<crate::messages::Frame>, bool)>| {
            if seen.insert(entry.0.clone()) {
                result.push(entry);
            }
        };
    if is_on_opt(inputs.var("CMAKE_INCLUDE_CURRENT_DIR").as_deref()) {
        let directory = &ctx.interp.makefiles[target.makefile];
        emit(
            (directory.binary_dir.clone(), false, Vec::new(), false),
            &mut result,
        );
        emit(
            (directory.source_dir.clone(), false, Vec::new(), false),
            &mut result,
        );
    }
    if is_on_opt(
        inputs
            .var("CMAKE_INCLUDE_DIRECTORIES_PROJECT_BEFORE")
            .as_deref(),
    ) {
        for entry in &users {
            if not_excluded(&entry.0)
                && (Path::new(&entry.0).starts_with(&ctx.interp.home_source)
                    || Path::new(&entry.0).starts_with(&ctx.interp.home_binary))
            {
                emit(entry.clone(), &mut result);
            }
        }
    }
    for entry in &users {
        if not_excluded(&entry.0) {
            emit(entry.clone(), &mut result);
        }
    }
    // MoveSystemIncludesToEnd: non-system directories first, order kept.
    let (non_system, system): (Vec<_>, Vec<_>) = result.into_iter().partition(|e| !e.1);
    let mut result = non_system;
    result.extend(system);
    for dir in &standard {
        emit((dir.clone(), false, Vec::new(), false), &mut result);
    }
    // Re-emit user-requested dirs that are implicit (stripImplicitDirs path
    // appends implicitDirs only on demand, which the codemodel does not use).
    for entry in &users {
        if implicit.contains(&real_path(&entry.0)) {
            emit(entry.clone(), &mut result);
        }
    }
    result
        .into_iter()
        .map(|(path, system, backtrace, _)| (path, system, backtrace))
        .collect()
}

/// `cmGeneratorTarget::GetPrecompileHeaders` as codemodel
/// "precompileHeaders" entries.
pub(crate) fn pch_items(ctx: &GenCtx, target: &Target, config: &str, lang: &str) -> Vec<BtFrag> {
    let inputs = Flags {
        ctx,
        target,
        config,
        lang,
    };
    inputs
        .usage_items("PRECOMPILE_HEADERS")
        .into_iter()
        .map(|item| (item.value, ctx.backtrace(item.backtrace)))
        .collect()
}
