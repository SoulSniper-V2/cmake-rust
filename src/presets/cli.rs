//! The presets parts of `cmake::SetArgs`, `cmake::Run`, `cmake::Build`,
//! `cmake::Workflow` (cmake.cxx) and `do_workflow` (cmakemain.cxx).

use std::io::Write;

use super::{ArchToolsetStrategy, ConfigurePreset, Graph, StepType, TraceEnableMode};
use crate::cache::CacheType;
use crate::configure::ConfigureOptions;

fn error(message: impl AsRef<str>) {
    crate::cmcmd::error(message.as_ref());
}

/// `--list-presets[=<type>]` values.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ListPresets {
    Configure,
    Build,
    Test,
    Package,
    Workflow,
    All,
}

impl ListPresets {
    /// The `--list-presets` argument handler; reports invalid values.
    pub fn parse(value: &str) -> Option<ListPresets> {
        Some(match value {
            "" | "configure" => ListPresets::Configure,
            "build" => ListPresets::Build,
            "test" => ListPresets::Test,
            "package" => ListPresets::Package,
            "workflow" => ListPresets::Workflow,
            "all" => ListPresets::All,
            _ => {
                error(
                    "Invalid value specified for --list-presets.\nValid values are configure, build, test, package, or all. When no value is passed the default is configure.",
                );
                return None;
            }
        })
    }
}

fn read_graph(home: &str) -> Option<Graph> {
    let mut graph = Graph::default();
    if !graph.read_project_presets(home, false) {
        error(format!(
            "Could not read presets from {home}:\n{}",
            graph.parse_state.error_message()
        ));
        return None;
    }
    Some(graph)
}

/// `cmake::PrintPresetList`: configure presets whose generator is known.
pub fn print_preset_list(graph: &Graph) {
    let filter = |preset: &ConfigurePreset| -> bool {
        preset.generator.is_empty() || crate::cli::known_generator(&preset.generator)
    };
    graph.print_configure_preset_list(&filter, None);
}

/// Diagnostic level of `cmake::DiagLevels` (`DIAG_IGNORE < DIAG_WARN <
/// DIAG_ERROR`), mapped onto the `-W` option pair.
fn diag_level(warn: Option<bool>, error: Option<bool>) -> Option<u8> {
    if error == Some(true) {
        Some(2)
    } else if warn == Some(true) {
        Some(1)
    } else if warn == Some(false) {
        Some(0)
    } else {
        None
    }
}

fn set_diag_level(level: Option<u8>, warn: &mut Option<bool>, error: &mut Option<bool>) {
    match level {
        None => {}
        Some(0) => {
            *warn = Some(false);
            *error = Some(false);
        }
        Some(1) => {
            *warn = Some(true);
            *error = None;
        }
        Some(_) => {
            *warn = Some(true);
            *error = Some(true);
        }
    }
}

/// `cmake::SetWarningFromPreset`.
fn set_warning_from_preset(warn_option: &mut Option<bool>, error_option: &mut Option<bool>, warning: Option<bool>, error: Option<bool>) {
    let mut level = diag_level(*warn_option, *error_option);
    if let Some(warning) = warning {
        level = Some(if warning { level.unwrap_or(0).max(1) } else { 0 });
    }
    if let Some(error) = error {
        level = Some(if error { 2 } else { level.unwrap_or(0).min(1) });
    }
    set_diag_level(level, warn_option, error_option);
}

/// The result of the configure-preset part of `cmake::SetArgs`.
pub struct ConfigurePresetState {
    /// `UnprocessedPresetVariables`, for `PrintPresetVariables`.
    pub variables: Vec<(String, Option<CacheType>, String)>,
    /// `UnprocessedPresetEnvironment`, for `PrintPresetEnvironment`.
    pub environment: Vec<(String, String)>,
}

/// The `--list-presets` / `--preset` block of `cmake::SetArgs`. Returns
/// `Err(code)` when cmake must exit.
pub fn apply_configure_preset(
    home: &str,
    list: Option<ListPresets>,
    preset_name: &str,
    have_b_arg: bool,
    options: &mut ConfigureOptions,
) -> Result<Option<ConfigurePresetState>, i32> {
    if list.is_none() && preset_name.is_empty() {
        return Ok(None);
    }
    let graph = read_graph(home).ok_or(1)?;
    if let Some(list) = list {
        match list {
            ListPresets::Configure => print_preset_list(&graph),
            ListPresets::Build => graph.print_build_preset_list(None),
            ListPresets::Test => graph.print_test_preset_list(None),
            ListPresets::Package => graph.print_package_preset_list(None),
            ListPresets::Workflow => graph.print_workflow_preset_list(None),
            ListPresets::All => graph.print_all_presets(),
        }
        let _ = std::io::stdout().flush();
        return Err(0);
    }
    let Some(pair) = graph.configure_presets.get(preset_name) else {
        error(format!("No such preset in {home}: \"{preset_name}\""));
        print_preset_list(&graph);
        return Err(1);
    };
    if pair.unexpanded.base.hidden {
        error(format!("Cannot use hidden preset in {home}: \"{preset_name}\""));
        print_preset_list(&graph);
        return Err(1);
    }
    let Some(expanded) = &pair.expanded else {
        error(format!(
            "Could not evaluate preset \"{}\": Invalid macro expansion",
            pair.unexpanded.base.name
        ));
        return Err(1);
    };
    if !expanded.base.condition_result {
        error(format!(
            "Could not use disabled preset \"{}\"",
            pair.unexpanded.base.name
        ));
        return Err(1);
    }

    if !options.cache_loaded && !have_b_arg && !expanded.binary_dir.is_empty() {
        options.binary_dir = expanded.binary_dir.clone();
    }
    if options.generator.is_none() && !expanded.generator.is_empty() {
        // cmake::CreateAndSetGlobalGenerator
        if !crate::cli::known_generator(&expanded.generator) {
            error(format!("Could not create named generator {}", expanded.generator));
            crate::cli::print_generators();
            return Err(1);
        }
        options.generator = Some(expanded.generator.clone());
    }

    let mut variables = expanded.cache_variables.clone();
    if !expanded.install_dir.is_empty() {
        variables.insert(
            "CMAKE_INSTALL_PREFIX".to_owned(),
            Some(super::CacheVariable {
                type_: "PATH".to_owned(),
                value: expanded.install_dir.clone(),
            }),
        );
    }
    if !expanded.toolchain_file.is_empty() {
        variables.insert(
            "CMAKE_TOOLCHAIN_FILE".to_owned(),
            Some(super::CacheVariable {
                type_: "FILEPATH".to_owned(),
                value: expanded.toolchain_file.clone(),
            }),
        );
    }

    if (expanded.architecture_strategy.is_none()
        || expanded.architecture_strategy == Some(ArchToolsetStrategy::Set))
        && options.platform.is_none()
        && !expanded.architecture.is_empty()
    {
        options.platform = Some(expanded.architecture.clone());
    }
    if (expanded.toolset_strategy.is_none() || expanded.toolset_strategy == Some(ArchToolsetStrategy::Set))
        && options.toolset.is_none()
        && !expanded.toolset.is_empty()
    {
        options.toolset = Some(expanded.toolset.clone());
    }
    if !expanded.graphviz_file.is_empty() && options.graphviz.is_none() {
        options.graphviz = Some(crate::util::collapse_full_path(&expanded.graphviz_file, ""));
    }

    set_warning_from_preset(
        &mut options.warn_dev,
        &mut options.warn_dev_error,
        expanded.warn_dev,
        expanded.error_dev,
    );
    set_warning_from_preset(
        &mut options.warn_deprecated,
        &mut options.warn_deprecated_error,
        expanded.warn_deprecated,
        expanded.error_deprecated,
    );
    if expanded.warn_uninitialized == Some(true) {
        options.warn_uninitialized = true;
    }
    if expanded.warn_unused_cli == Some(false) {
        options.no_warn_unused_cli = true;
    }
    if expanded.warn_system_vars == Some(true) {
        options.check_system_vars = true;
    }
    // --debug-output and --debug-find have no implementation to enable.
    for (field, enabled) in [
        ("debug.output", expanded.debug_output == Some(true)),
        ("debug.find", expanded.debug_find == Some(true)),
    ] {
        if enabled {
            error(format!("Preset field \"{field}\" is not supported"));
            return Err(1);
        }
    }
    if expanded.debug_try_compile == Some(true) {
        options.debug_trycompile = true;
    }
    if let Some(mode) = expanded.trace_mode
        && mode != TraceEnableMode::Disable
    {
        options.trace = true;
        if mode == TraceEnableMode::Expand {
            options.trace_expand = true;
        }
    }
    if let Some(format) = expanded.trace_format {
        options.trace = true;
        options.trace_json = format == super::TraceOutputFormat::JsonV1;
    }
    if !expanded.trace_source.is_empty() {
        options.trace = true;
        options.trace_sources.extend(expanded.trace_source.iter().cloned());
    }
    if !expanded.trace_redirect.is_empty() {
        options.trace = true;
        // cmake::SetTraceFile
        let file = &expanded.trace_redirect;
        match std::fs::File::create(file) {
            Ok(_) => {
                println!("Trace will be written to {file}");
                options.trace_redirect = Some(file.clone());
            }
            Err(e) => {
                error(format!(
                    "Error opening trace file {file}: {}",
                    crate::cmcmd::os_error(&e)
                ));
                return Err(1);
            }
        }
    }

    // cmake::ProcessPresetVariables, ahead of the command-line cache args.
    let mut definitions = Vec::new();
    let mut printed = Vec::new();
    for (name, var) in &variables {
        let Some(var) = var else {
            continue;
        };
        let kind = (!var.type_.is_empty()).then(|| CacheType::parse(&var.type_));
        definitions.push(crate::cli::cache_definition_from_parts(name, &var.value, kind));
        printed.push((name.clone(), kind, var.value.clone()));
    }
    definitions.append(&mut options.definitions);
    options.definitions = definitions;

    // cmake::ProcessPresetEnvironment
    let mut environment = Vec::new();
    for (name, value) in &expanded.base.environment {
        if let Some(value) = value {
            // SAFETY: single-threaded at this point of argument processing.
            unsafe { std::env::set_var(name, value) };
            environment.push((name.clone(), value.clone()));
        }
    }
    Ok(Some(ConfigurePresetState {
        variables: printed,
        environment,
    }))
}

impl ConfigurePresetState {
    /// `cmake::PrintPresetVariables` + `PrintPresetEnvironment`, after the
    /// command-line cache arguments removed overridden entries.
    pub fn print(&self, overridden: &dyn Fn(&str) -> bool) {
        let mut out = String::new();
        let mut first = true;
        for (name, kind, value) in &self.variables {
            if overridden(name) {
                continue;
            }
            if first {
                out += "Preset CMake variables:\n\n";
                first = false;
            }
            out += &format!("  {name}");
            if let Some(kind) = kind
                && *kind != CacheType::Uninitialized
            {
                out += &format!(":{}", kind.as_str());
            }
            out += &format!("=\"{value}\"\n");
        }
        if !first {
            out.push('\n');
        }
        first = true;
        for (name, value) in &self.environment {
            if first {
                out += "Preset environment variables:\n\n";
                first = false;
            }
            out += &format!("  {name}=\"{value}\"\n");
        }
        if !first {
            out.push('\n');
        }
        print!("{out}");
    }
}

/// The settings `cmake::Build` takes from a build preset.
pub struct BuildSettings<'a> {
    pub dir: &'a mut String,
    /// `None` is NO_BUILD_PARALLEL_LEVEL, `Some(0)` DEFAULT_BUILD_PARALLEL_LEVEL.
    pub jobs: &'a mut Option<u32>,
    pub targets: &'a mut Vec<String>,
    pub config: &'a mut String,
    pub clean_first: &'a mut bool,
    pub verbose: &'a mut bool,
    pub native: &'a mut Vec<String>,
}

/// The presets part of `cmake::Build`. `Err(code)` ends the build.
pub fn apply_build_preset(cwd: &str, preset_name: &str, list: bool, settings: BuildSettings<'_>) -> Result<(), i32> {
    let home = cwd;
    let graph = read_graph(home).ok_or(1)?;
    if list {
        graph.print_build_preset_list(None);
        return Err(0);
    }
    let Some(pair) = graph.build_presets.get(preset_name) else {
        error(format!("No such build preset in {home}: \"{preset_name}\""));
        graph.print_build_preset_list(None);
        return Err(1);
    };
    if pair.unexpanded.base.hidden {
        error(format!("Cannot use hidden build preset in {home}: \"{preset_name}\""));
        graph.print_build_preset_list(None);
        return Err(1);
    }
    let Some(expanded) = &pair.expanded else {
        error(format!(
            "Could not evaluate build preset \"{preset_name}\": Invalid macro expansion"
        ));
        graph.print_build_preset_list(None);
        return Err(1);
    };
    if !expanded.base.condition_result {
        error(format!("Cannot use disabled build preset in {home}: \"{preset_name}\""));
        graph.print_build_preset_list(None);
        return Err(1);
    }
    let Some(configure) = graph.configure_presets.get(&expanded.configure_preset) else {
        error(format!(
            "No such configure preset in {home}: \"{}\"",
            expanded.configure_preset
        ));
        print_preset_list(&graph);
        return Err(1);
    };
    if configure.unexpanded.base.hidden {
        error(format!(
            "Cannot use hidden configure preset in {home}: \"{}\"",
            expanded.configure_preset
        ));
        print_preset_list(&graph);
        return Err(1);
    }
    let Some(expanded_configure) = &configure.expanded else {
        error(format!(
            "Could not evaluate configure preset \"{}\": Invalid macro expansion",
            expanded.configure_preset
        ));
        return Err(1);
    };
    if !expanded_configure.binary_dir.is_empty() {
        *settings.dir = expanded_configure.binary_dir.clone();
    }
    for (name, value) in &expanded.base.environment {
        if let Some(value) = value {
            // SAFETY: single-threaded.
            unsafe { std::env::set_var(name, value) };
        }
    }
    if matches!(*settings.jobs, None | Some(0))
        && let Some(jobs) = expanded.jobs
    {
        *settings.jobs = Some(jobs.max(0) as u32);
    }
    if settings.targets.is_empty() {
        settings.targets.extend(expanded.targets.iter().cloned());
    }
    if settings.config.is_empty() {
        *settings.config = expanded.configuration.clone();
    }
    if !*settings.clean_first
        && let Some(clean) = expanded.clean_first
    {
        *settings.clean_first = clean;
    }
    if !*settings.verbose
        && let Some(verbose) = expanded.verbose
    {
        *settings.verbose = verbose;
    }
    if settings.native.is_empty() {
        settings.native.extend(expanded.native_tool_options.iter().cloned());
    }
    Ok(())
}

/// `do_workflow` (cmakemain.cxx) + `cmake::Workflow`.
pub fn workflow(args: &[String], cwd: &str) -> i32 {
    let mut preset_name = String::new();
    let mut list = false;
    let mut fresh = false;
    let mut input: Vec<String> = args.to_vec();
    let mut i = 0;
    while i < input.len() {
        let arg = input[i].clone();
        let mut matched = true;
        let mut parsed = true;
        if arg == "--preset" || arg.starts_with("--preset=") || i == 0 && arg != "--list-presets" && arg != "--fresh" {
            if !(arg == "--preset" || arg.starts_with("--preset=")) {
                input.insert(0, "--preset".to_owned());
            }
            // cmCommandLineArgument Values::One
            let current = input[i].clone();
            if let Some(value) = current.strip_prefix("--preset=") {
                preset_name = value.to_owned();
                parsed = !value.is_empty();
            } else if input.get(i + 1).is_some_and(|next| !crate::cli::is_flag(next)) {
                preset_name = input[i + 1].clone();
                i += 1;
            } else {
                parsed = false;
            }
        } else if arg == "--list-presets" {
            list = true;
        } else if arg == "--fresh" {
            fresh = true;
        } else {
            matched = false;
        }
        if !(matched && parsed) {
            if !matched {
                preset_name.clear();
                list = false;
                eprintln!("Unknown argument {arg}");
            }
            break;
        }
        i += 1;
    }
    if preset_name.is_empty() && !list {
        eprint!(
            "Usage: cmake --workflow <options>\nOptions:\n  --preset <preset> = Workflow preset to execute.\n  --list-presets    = List available workflow presets.\n  --fresh           = Configure a fresh build tree, removing any existing cache file.\n"
        );
        return 1;
    }

    let home = cwd;
    let Some(graph) = read_graph(home) else {
        return 1;
    };
    if list {
        graph.print_workflow_preset_list(None);
        return 0;
    }
    let Some(pair) = graph.workflow_presets.get(&preset_name) else {
        error(format!("No such workflow preset in {home}: \"{preset_name}\""));
        graph.print_workflow_preset_list(None);
        return 1;
    };
    if pair.unexpanded.base.hidden {
        error(format!("Cannot use hidden workflow preset in {home}: \"{preset_name}\""));
        graph.print_workflow_preset_list(None);
        return 1;
    }
    let Some(expanded) = &pair.expanded else {
        error(format!(
            "Could not evaluate workflow preset \"{preset_name}\": Invalid macro expansion"
        ));
        graph.print_workflow_preset_list(None);
        return 1;
    };
    if !expanded.base.condition_result {
        error(format!("Cannot use disabled workflow preset in {home}: \"{preset_name}\""));
        graph.print_workflow_preset_list(None);
        return 1;
    }

    let bin_dir = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|p| p.to_string_lossy().into_owned()))
        .unwrap_or_default();
    let cmake = std::env::current_exe()
        .map(|exe| exe.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "cmake".to_owned());
    let ctest = format!("{bin_dir}/ctest");
    let cpack = format!("{bin_dir}/cpack");

    // cmake::FindPresetForWorkflow
    fn find<T: super::PresetKind>(
        home: &str,
        kind: &str,
        presets: &std::collections::BTreeMap<String, super::PresetPair<T>>,
        name: &str,
    ) -> bool {
        let Some(pair) = presets.get(name) else {
            error(format!("No such {kind} preset in {home}: \"{name}\""));
            return false;
        };
        if pair.unexpanded.base().hidden {
            error(format!("Cannot use hidden {kind} preset in {home}: \"{name}\""));
            return false;
        }
        let Some(expanded) = &pair.expanded else {
            error(format!(
                "Could not evaluate {kind} preset \"{name}\": Invalid macro expansion"
            ));
            return false;
        };
        if !expanded.base().condition_result {
            error(format!("Cannot use disabled {kind} preset in {home}: \"{name}\""));
            return false;
        }
        true
    }

    let mut steps: Vec<(&str, String, Vec<String>)> = Vec::new();
    for step in &expanded.steps {
        let name = step.preset_name.clone();
        let (kind, ok, command) = match step.preset_type {
            StepType::Configure => {
                let mut command = vec![cmake.clone(), "--preset".to_owned(), name.clone()];
                if fresh {
                    command.push("--fresh".to_owned());
                }
                ("configure", find(home, "configure", &graph.configure_presets, &name), command)
            }
            StepType::Build => (
                "build",
                find(home, "build", &graph.build_presets, &name),
                vec![cmake.clone(), "--build".to_owned(), "--preset".to_owned(), name.clone()],
            ),
            StepType::Test => (
                "test",
                find(home, "test", &graph.test_presets, &name),
                vec![ctest.clone(), "--preset".to_owned(), name.clone()],
            ),
            StepType::Package => (
                "package",
                find(home, "package", &graph.package_presets, &name),
                vec![cpack.clone(), "--preset".to_owned(), name.clone()],
            ),
        };
        if !ok {
            return 1;
        }
        steps.push((kind, name, command));
    }

    let total = steps.len();
    for (index, (kind, name, command)) in steps.iter().enumerate() {
        if index > 0 {
            println!();
        }
        print!(
            "Executing workflow step {} of {total}: {kind} preset \"{name}\"\n\n",
            index + 1
        );
        let _ = std::io::stdout().flush();
        let status = std::process::Command::new(&command[0])
            .args(&command[1..])
            .status();
        let code = match status {
            Ok(status) => status.code().unwrap_or(1),
            Err(_) => 1,
        };
        if code != 0 {
            return code;
        }
    }
    0
}
