//! `cmCMakePresetsErrors` (cmCMakePresetsErrors.cxx).

use super::json::{ErrorGen, ObjectErrorGen, State, V, error_gen, invalid_named_object};

fn get_preset(state: &State) -> Option<V> {
    if state.parse_stack.len() < 2 {
        return None;
    }
    let first = &state.parse_stack[0].0;
    if matches!(
        first.as_str(),
        "configurePresets" | "packagePresets" | "buildPresets" | "testPresets"
    ) {
        return state.parse_stack[1].1.clone();
    }
    None
}

fn get_preset_name(state: &State) -> String {
    match get_preset(state).and_then(|preset| preset.member("name")) {
        Some(name) if name.is_convertible_to_string() => name.as_string(),
        _ => String::new(),
    }
}

fn get_variable_name(state: &State) -> String {
    let var = state.key_after("cacheVariables");
    format!("variable \"{var}\" for preset \"{}\"", get_preset_name(state))
}

pub fn file_not_found(filename: &str, state: &mut State) {
    state.add_error(format!("File not found: {filename}"));
}

pub fn invalid_root(value: Option<&V>, state: &mut State) {
    state.add_error_at_value("Invalid root object", value);
}

pub fn no_version(value: Option<&V>, state: &mut State) {
    state.add_error_at_value("No \"version\" field", value);
}

pub fn invalid_version(value: Option<&V>, state: &mut State) {
    state.add_error_at_value("Invalid \"version\" field", value);
}

pub fn invalid_presets(value: Option<&V>, state: &mut State) {
    state.add_error_at_value("Invalid \"configurePresets\" field", value);
}

pub fn invalid_preset(value: Option<&V>, state: &mut State) {
    state.add_error_at_value("Invalid preset", value);
}

pub fn invalid_preset_named(name: &str, state: &mut State) {
    state.add_error(format!("Invalid preset: \"{name}\""));
}

pub fn invalid_variable(value: Option<&V>, state: &mut State) {
    let var = get_variable_name(state);
    state.add_error_at_value(format!("Invalid CMake {var}"), value);
}

pub fn duplicate_presets(name: &str, state: &mut State) {
    state.add_error(format!("Duplicate preset: \"{name}\""));
}

pub fn cyclic_preset_inheritance(name: &str, state: &mut State) {
    state.add_error(format!("Cyclic preset inheritance for preset \"{name}\""));
}

pub fn inherited_preset_unreachable_from_file(name: &str, state: &mut State) {
    state.add_error(format!(
        "Inherited preset \"{name}\" is unreachable from preset's file"
    ));
}

pub fn configure_preset_unreachable_from_file(name: &str, state: &mut State) {
    state.add_error(format!(
        "Configure preset \"{name}\" is unreachable from preset's file"
    ));
}

pub fn invalid_macro_expansion(name: &str, state: &mut State) {
    state.add_error(format!("Invalid macro expansion in \"{name}\""));
}

pub fn build_test_presets_unsupported(state: &mut State) {
    state.add_error("File version must be 2 or higher for build and test preset support");
}

pub fn package_presets_unsupported(state: &mut State) {
    state.add_error("File version must be 6 or higher for package preset support");
}

pub fn workflow_presets_unsupported(state: &mut State) {
    state.add_error("File version must be 6 or higher for workflow preset support");
}

pub fn include_unsupported(state: &mut State) {
    state.add_error("File version must be 4 or higher for include support");
}

pub fn invalid_include(value: Option<&V>, state: &mut State) {
    state.add_error_at_value("Invalid \"include\" field", value);
}

pub fn invalid_configure_preset(name: &str, state: &mut State) {
    state.add_error(format!("Invalid \"configurePreset\": \"{name}\""));
}

pub fn install_prefix_unsupported(value: Option<&V>, state: &mut State) {
    state.add_error_at_value(
        "File version must be 3 or higher for installDir preset support",
        value,
    );
}

pub fn condition_unsupported(state: &mut State) {
    state.add_error("File version must be 3 or higher for condition support");
}

pub fn toolchain_file_unsupported(state: &mut State) {
    state.add_error("File version must be 3 or higher for toolchainFile preset support");
}

pub fn graphviz_file_unsupported(state: &mut State) {
    state.add_error("File version must be 10 or higher for graphviz preset support");
}

pub fn cyclic_include(file: &str, state: &mut State) {
    state.add_error(format!("Cyclic include among preset files: {file}"));
}

pub fn test_output_truncation_unsupported(state: &mut State) {
    state.add_error(
        "File version must be 5 or higher for testOutputTruncation preset support",
    );
}

pub fn invalid_workflow_steps(step: &str, state: &mut State) {
    state.add_error(format!("Invalid workflow step \"{step}\""));
}

pub fn no_workflow_steps(name: &str, state: &mut State) {
    state.add_error(format!("No workflow steps specified for \"{name}\""));
}

pub fn first_workflow_step_not_configure(step: &str, state: &mut State) {
    state.add_error(format!(
        "First workflow step \"{step}\" must be a configure step"
    ));
}

pub fn configure_workflow_step_not_first(step: &str, state: &mut State) {
    state.add_error(format!(
        "Configure workflow step \"{step}\" must be the first step"
    ));
}

pub fn workflow_step_unreachable_from_file(step: &str, state: &mut State) {
    state.add_error(format!(
        "Workflow step \"{step}\" is unreachable from preset's file"
    ));
}

pub fn ctest_junit_unsupported(state: &mut State) {
    state.add_error("File version must be 6 or higher for CTest JUnit output support");
}

pub fn trace_unsupported(state: &mut State) {
    state.add_error("File version must be 7 or higher for trace preset support");
}

pub fn unrecognized_version_range(min: i32, max: i32) -> ErrorGen {
    error_gen(move |value, state| {
        let text = value
            .filter(|v| v.is_convertible_to_string())
            .map(|v| v.as_string())
            .unwrap_or_default();
        state.add_error_at_value(
            format!("Unrecognized \"version\" {text}: must be >={min} and <={max}"),
            value,
        );
    })
}

pub fn unrecognized_cmake_version(version: &str, current: u32, required: u32, value: Option<&V>, state: &mut State) {
    state.add_error_at_value(
        format!(
            "\"cmakeMinimumRequired\" {version} version {required} must be less than {current}"
        ),
        value,
    );
}

pub fn invalid_preset_name(value: Option<&V>, state: &mut State) {
    let mut message = "Invalid Preset Name".to_owned();
    if let Some(v) = value
        && v.is_convertible_to_string()
        && !v.as_string().is_empty()
    {
        message += &format!(": {}", v.as_string());
    }
    state.add_error_at_value(message, value);
}

pub fn invalid_condition(value: Option<&V>, state: &mut State) {
    let name = get_preset_name(state);
    state.add_error_at_value(format!("Invalid condition for preset \"{name}\""), value);
}

pub fn invalid_condition_object() -> ObjectErrorGen {
    invalid_named_object(|_, state| format!(" condition for preset \"{}\"", get_preset_name(state)))
}

pub fn invalid_variable_object() -> ObjectErrorGen {
    invalid_named_object(|_, state| get_variable_name(state))
}

pub fn invalid_preset_object() -> ObjectErrorGen {
    invalid_named_object(|_, _| "Preset".to_owned())
}

pub fn invalid_root_object() -> ObjectErrorGen {
    invalid_named_object(|_, _| "root object".to_owned())
}

pub fn preset_missing_field(name: &str, field: &str, state: &mut State) {
    state.add_error(format!("Preset \"{name}\" missing field \"{field}\""));
}

pub fn schema_unsupported(state: &mut State) {
    state.add_error("File version must be 8 or higher for $schema support");
}

pub fn error_fn(f: fn(Option<&V>, &mut State)) -> ErrorGen {
    error_gen(f)
}
