//! `cmCMakePresetsGraph::ReadJSONFile` and the JSON helpers of
//! cmCMakePresetsGraphReadJSON*.cxx.

use std::collections::BTreeMap;
use std::rc::Rc;

use super::errors;
use super::json::{
    ErrorGen, Helper, Object, State, V, bool_helper, checked_helper, helper, int_helper,
    invalid_bool, invalid_int, invalid_named_object_key, invalid_object, invalid_string,
    map_helper, optional_helper, required_helper, string_helper, uint_helper, vector_helper,
};
use super::*;

const MIN_VERSION: i32 = 1;
const MAX_VERSION: i32 = 10;

#[derive(Default)]
struct CMakeVersion {
    major: u32,
    minor: u32,
    patch: u32,
}

#[derive(Default)]
struct RootPresets {
    cmake_minimum_required: CMakeVersion,
    configure_presets: Vec<ConfigurePreset>,
    build_presets: Vec<BuildPreset>,
    test_presets: Vec<TestPreset>,
    package_presets: Vec<PackagePreset>,
    workflow_presets: Vec<WorkflowPreset>,
    include: Vec<String>,
}

fn e(f: fn(Option<&V>, &mut State)) -> ErrorGen {
    errors::error_fn(f)
}

// ---------------------------------------------------------------------
// Conditions

#[derive(Default)]
struct ConstC {
    value: bool,
}
#[derive(Default)]
struct EqualsC {
    lhs: String,
    rhs: String,
}
#[derive(Default)]
struct InListC {
    string: String,
    list: Vec<String>,
}
#[derive(Default)]
struct MatchesC {
    string: String,
    regex: String,
}
#[derive(Default)]
struct AnyAllC {
    conditions: Vec<Option<Box<Condition>>>,
}
#[derive(Default)]
struct NotC {
    sub: Option<Box<Condition>>,
}

fn condition_string() -> Helper<String> {
    string_helper(invalid_string(), "")
}

fn condition_helper(
    out: &mut Option<Box<Condition>>,
    value: Option<&V>,
    state: &mut State,
) -> bool {
    let Some(v) = value else {
        *out = None;
        return true;
    };
    if v.is_bool() {
        *out = Some(Box::new(Condition::Const(v.as_bool())));
        return true;
    }
    if v.is_null() {
        *out = Some(Box::new(Condition::Null));
        return true;
    }
    if v.is_object() {
        let Some(type_value) = v.member("type") else {
            errors::invalid_condition(value, state);
            return false;
        };
        if !type_value.is_string() {
            errors::invalid_condition(value, state);
            return false;
        }
        let type_ = type_value.as_string();
        let error = errors::invalid_condition_object;
        match type_.as_str() {
            "const" => {
                let h = Object::<ConstC>::new(error(), false)
                    .bind_dummy::<String>("type", condition_string(), true)
                    .bind(
                        "value",
                        |c: &mut ConstC| &mut c.value,
                        bool_helper(invalid_bool(), false),
                        true,
                    )
                    .build();
                let mut c = ConstC::default();
                if !h(&mut c, value, state) {
                    return false;
                }
                *out = Some(Box::new(Condition::Const(c.value)));
                return true;
            }
            "equals" | "notEquals" => {
                let h = Object::<EqualsC>::new(error(), false)
                    .bind_dummy::<String>("type", condition_string(), true)
                    .bind(
                        "lhs",
                        |c: &mut EqualsC| &mut c.lhs,
                        condition_string(),
                        true,
                    )
                    .bind(
                        "rhs",
                        |c: &mut EqualsC| &mut c.rhs,
                        condition_string(),
                        true,
                    )
                    .build();
                let mut c = EqualsC::default();
                if !h(&mut c, value, state) {
                    return false;
                }
                let cond = Condition::Equals {
                    lhs: c.lhs,
                    rhs: c.rhs,
                };
                *out = Some(Box::new(invert_if(cond, type_ == "notEquals")));
                return true;
            }
            "inList" | "notInList" => {
                let list = vector_helper(e(errors::invalid_condition), condition_string());
                let h = Object::<InListC>::new(error(), false)
                    .bind_dummy::<String>("type", condition_string(), true)
                    .bind(
                        "string",
                        |c: &mut InListC| &mut c.string,
                        condition_string(),
                        true,
                    )
                    .bind("list", |c: &mut InListC| &mut c.list, list, true)
                    .build();
                let mut c = InListC::default();
                if !h(&mut c, value, state) {
                    return false;
                }
                let cond = Condition::InList {
                    string: c.string,
                    list: c.list,
                };
                *out = Some(Box::new(invert_if(cond, type_ == "notInList")));
                return true;
            }
            "matches" | "notMatches" => {
                let h = Object::<MatchesC>::new(error(), false)
                    .bind_dummy::<String>("type", condition_string(), true)
                    .bind(
                        "string",
                        |c: &mut MatchesC| &mut c.string,
                        condition_string(),
                        true,
                    )
                    .bind(
                        "regex",
                        |c: &mut MatchesC| &mut c.regex,
                        condition_string(),
                        true,
                    )
                    .build();
                let mut c = MatchesC::default();
                if !h(&mut c, value, state) {
                    return false;
                }
                let cond = Condition::Matches {
                    string: c.string,
                    regex: c.regex,
                };
                *out = Some(Box::new(invert_if(cond, type_ == "notMatches")));
                return true;
            }
            "anyOf" | "allOf" => {
                let list =
                    vector_helper(e(errors::invalid_condition), helper(sub_condition_helper));
                let h = Object::<AnyAllC>::new(error(), false)
                    .bind_dummy::<String>("type", condition_string(), true)
                    .bind(
                        "conditions",
                        |c: &mut AnyAllC| &mut c.conditions,
                        list,
                        true,
                    )
                    .build();
                let mut c = AnyAllC::default();
                if !h(&mut c, value, state) {
                    return false;
                }
                *out = Some(Box::new(Condition::AnyAllOf {
                    conditions: c
                        .conditions
                        .into_iter()
                        .map(|c| c.map_or(Condition::Null, |c| *c))
                        .collect(),
                    stop_value: type_ == "anyOf",
                }));
                return true;
            }
            "not" => {
                let h = Object::<NotC>::new(error(), false)
                    .bind_dummy::<String>("type", condition_string(), true)
                    .bind(
                        "condition",
                        |c: &mut NotC| &mut c.sub,
                        helper(sub_condition_helper),
                        true,
                    )
                    .build();
                let mut c = NotC::default();
                if !h(&mut c, value, state) {
                    return false;
                }
                *out = Some(Box::new(Condition::Not(
                    c.sub.unwrap_or_else(|| Box::new(Condition::Null)),
                )));
                return true;
            }
            _ => {}
        }
    }
    errors::invalid_condition(value, state);
    false
}

fn invert_if(condition: Condition, invert: bool) -> Condition {
    if invert {
        Condition::Not(Box::new(condition))
    } else {
        condition
    }
}

fn sub_condition_helper(
    out: &mut Option<Box<Condition>>,
    value: Option<&V>,
    state: &mut State,
) -> bool {
    let mut ptr = None;
    let result = condition_helper(&mut ptr, value, state);
    if ptr.as_ref().is_some_and(|c| c.is_null()) {
        errors::invalid_condition(value, state);
        return false;
    }
    *out = ptr;
    result
}

fn preset_condition_helper() -> Helper<Option<Rc<Condition>>> {
    helper(|out: &mut Option<Rc<Condition>>, value, state| {
        let mut ptr = None;
        let result = condition_helper(&mut ptr, value, state);
        *out = ptr.map(|c| Rc::new(*c));
        result
    })
}

// ---------------------------------------------------------------------
// Common preset helpers (cmCMakePresetsGraphInternal)

fn preset_string() -> Helper<String> {
    string_helper(invalid_string(), "")
}

fn preset_name() -> Helper<String> {
    helper(|out: &mut String, value, state| match value {
        Some(v) if v.is_string() && !v.as_string().is_empty() => {
            *out = v.as_string();
            true
        }
        _ => {
            errors::invalid_preset_name(value, state);
            false
        }
    })
}

fn preset_vector_string() -> Helper<Vec<String>> {
    vector_helper(e(errors::invalid_preset), preset_string())
}

fn preset_bool() -> Helper<bool> {
    bool_helper(invalid_bool(), false)
}

fn preset_optional_bool() -> Helper<Option<bool>> {
    optional_helper(preset_bool())
}

fn preset_int() -> Helper<i32> {
    int_helper(invalid_int(), 0)
}

fn preset_optional_int() -> Helper<Option<i32>> {
    optional_helper(preset_int())
}

fn preset_vector_int() -> Helper<Vec<i32>> {
    vector_helper(e(errors::invalid_preset), preset_int())
}

fn vendor_helper(error: ErrorGen) -> Helper<()> {
    helper(move |_: &mut (), value, state| {
        let Some(v) = value else {
            return true;
        };
        if !v.is_object() {
            error(value, state);
            return false;
        }
        true
    })
}

fn preset_vector_one_or_more_string() -> Helper<Vec<String>> {
    let vector = preset_vector_string();
    helper(move |out: &mut Vec<String>, value, state| {
        out.clear();
        let Some(v) = value else {
            return true;
        };
        if v.is_string() {
            out.push(v.as_string());
            return true;
        }
        vector(out, value, state)
    })
}

fn environment_helper(out: &mut Option<String>, value: Option<&V>, state: &mut State) -> bool {
    match value {
        None => {
            *out = None;
            true
        }
        Some(v) if v.is_null() => {
            *out = None;
            true
        }
        Some(v) if v.is_string() => {
            *out = Some(v.as_string());
            true
        }
        _ => {
            errors::invalid_preset(value, state);
            false
        }
    }
}

fn environment_map() -> Helper<BTreeMap<String, Option<String>>> {
    map_helper(e(errors::invalid_preset), helper(environment_helper))
}

/// Binds the fields every preset kind shares, in the per-kind order.
macro_rules! base_field {
    ($obj:expr, $t:ty, name) => {
        $obj.bind("name", |p: &mut $t| &mut p.base.name, preset_name(), true)
    };
    ($obj:expr, $t:ty, inherits) => {
        $obj.bind(
            "inherits",
            |p: &mut $t| &mut p.base.inherits,
            preset_vector_one_or_more_string(),
            false,
        )
    };
    ($obj:expr, $t:ty, hidden) => {
        $obj.bind(
            "hidden",
            |p: &mut $t| &mut p.base.hidden,
            preset_bool(),
            false,
        )
    };
    ($obj:expr, $t:ty, vendor) => {
        $obj.bind_dummy::<()>("vendor", vendor_helper(e(errors::invalid_preset)), false)
    };
    ($obj:expr, $t:ty, displayName) => {
        $obj.bind(
            "displayName",
            |p: &mut $t| &mut p.base.display_name,
            preset_string(),
            false,
        )
    };
    ($obj:expr, $t:ty, description) => {
        $obj.bind(
            "description",
            |p: &mut $t| &mut p.base.description,
            preset_string(),
            false,
        )
    };
    ($obj:expr, $t:ty, environment) => {
        $obj.bind(
            "environment",
            |p: &mut $t| &mut p.base.environment,
            environment_map(),
            false,
        )
    };
    ($obj:expr, $t:ty, condition) => {
        $obj.bind(
            "condition",
            |p: &mut $t| &mut p.base.condition,
            preset_condition_helper(),
            false,
        )
    };
}

// ---------------------------------------------------------------------
// Configure presets

fn arch_toolset_strategy(
    out: &mut Option<ArchToolsetStrategy>,
    value: Option<&V>,
    state: &mut State,
) -> bool {
    let Some(v) = value else {
        *out = None;
        return true;
    };
    if !v.is_string() {
        errors::invalid_preset(value, state);
        return false;
    }
    match v.as_string().as_str() {
        "set" => {
            *out = Some(ArchToolsetStrategy::Set);
            true
        }
        "external" => {
            *out = Some(ArchToolsetStrategy::External);
            true
        }
        _ => {
            errors::invalid_preset(value, state);
            false
        }
    }
}

fn arch_toolset_helper(
    value_field: fn(&mut ConfigurePreset) -> &mut String,
    strategy_field: fn(&mut ConfigurePreset) -> &mut Option<ArchToolsetStrategy>,
) -> Helper<ConfigurePreset> {
    let object = Object::<ConfigurePreset>::new(invalid_object(), false)
        .bind("value", value_field, preset_string(), false)
        .bind(
            "strategy",
            strategy_field,
            helper(arch_toolset_strategy),
            false,
        )
        .build();
    helper(move |out: &mut ConfigurePreset, value, state| {
        let Some(v) = value else {
            value_field(out).clear();
            *strategy_field(out) = None;
            return true;
        };
        if v.is_string() {
            *value_field(out) = v.as_string();
            *strategy_field(out) = None;
            return true;
        }
        if v.is_object() {
            return object(out, value, state);
        }
        errors::invalid_preset(value, state);
        false
    })
}

fn trace_enable_mode(
    out: &mut Option<TraceEnableMode>,
    value: Option<&V>,
    state: &mut State,
) -> bool {
    let Some(v) = value else {
        *out = None;
        return true;
    };
    if !v.is_string() {
        errors::invalid_preset(value, state);
        return false;
    }
    *out = Some(match v.as_string().as_str() {
        "on" => TraceEnableMode::Default,
        "off" => TraceEnableMode::Disable,
        "expand" => TraceEnableMode::Expand,
        _ => {
            errors::invalid_preset(value, state);
            return false;
        }
    });
    true
}

fn trace_output_format(
    out: &mut Option<TraceOutputFormat>,
    value: Option<&V>,
    state: &mut State,
) -> bool {
    let Some(v) = value else {
        *out = None;
        return true;
    };
    if !v.is_string() {
        errors::invalid_preset(value, state);
        return false;
    }
    *out = Some(match v.as_string().as_str() {
        "human" => TraceOutputFormat::Human,
        "json-v1" => TraceOutputFormat::JsonV1,
        _ => {
            errors::invalid_preset(value, state);
            return false;
        }
    });
    true
}

fn variable_value(out: &mut String, value: Option<&V>, state: &mut State) -> bool {
    let Some(v) = value else {
        out.clear();
        return true;
    };
    if v.is_bool() {
        *out = if v.as_bool() { "TRUE" } else { "FALSE" }.to_owned();
        return true;
    }
    preset_string()(out, value, state)
}

fn variable_helper() -> Helper<Option<CacheVariable>> {
    let object = Object::<CacheVariable>::new(errors::invalid_variable_object(), false)
        .bind(
            "type",
            |c: &mut CacheVariable| &mut c.type_,
            preset_string(),
            false,
        )
        .bind(
            "value",
            |c: &mut CacheVariable| &mut c.value,
            helper(variable_value),
            true,
        )
        .build();
    helper(move |out: &mut Option<CacheVariable>, value, state| {
        let Some(v) = value else {
            return true;
        };
        if v.is_bool() {
            *out = Some(CacheVariable {
                type_: "BOOL".to_owned(),
                value: if v.as_bool() { "TRUE" } else { "FALSE" }.to_owned(),
            });
            return true;
        }
        if v.is_string() {
            *out = Some(CacheVariable {
                type_: String::new(),
                value: v.as_string(),
            });
            return true;
        }
        if v.is_object() {
            let inner = out.insert(CacheVariable::default());
            return object(inner, value, state);
        }
        if v.is_null() {
            *out = None;
            return true;
        }
        errors::invalid_variable(value, state);
        false
    })
}

fn configure_preset_helper() -> Helper<ConfigurePreset> {
    type C = ConfigurePreset;
    let warnings = Object::<C>::new(invalid_named_object_key(), false)
        .bind(
            "dev",
            |p: &mut C| &mut p.warn_dev,
            preset_optional_bool(),
            false,
        )
        .bind(
            "deprecated",
            |p: &mut C| &mut p.warn_deprecated,
            preset_optional_bool(),
            false,
        )
        .bind(
            "uninitialized",
            |p: &mut C| &mut p.warn_uninitialized,
            preset_optional_bool(),
            false,
        )
        .bind(
            "unusedCli",
            |p: &mut C| &mut p.warn_unused_cli,
            preset_optional_bool(),
            false,
        )
        .bind(
            "systemVars",
            |p: &mut C| &mut p.warn_system_vars,
            preset_optional_bool(),
            false,
        )
        .build();
    let errors_ = Object::<C>::new(invalid_named_object_key(), false)
        .bind(
            "dev",
            |p: &mut C| &mut p.error_dev,
            preset_optional_bool(),
            false,
        )
        .bind(
            "deprecated",
            |p: &mut C| &mut p.error_deprecated,
            preset_optional_bool(),
            false,
        )
        .build();
    let debug = Object::<C>::new(invalid_named_object_key(), false)
        .bind(
            "output",
            |p: &mut C| &mut p.debug_output,
            preset_optional_bool(),
            false,
        )
        .bind(
            "tryCompile",
            |p: &mut C| &mut p.debug_try_compile,
            preset_optional_bool(),
            false,
        )
        .bind(
            "find",
            |p: &mut C| &mut p.debug_find,
            preset_optional_bool(),
            false,
        )
        .build();
    let trace = Object::<C>::new(errors::invalid_preset_object(), false)
        .bind(
            "mode",
            |p: &mut C| &mut p.trace_mode,
            helper(trace_enable_mode),
            false,
        )
        .bind(
            "format",
            |p: &mut C| &mut p.trace_format,
            helper(trace_output_format),
            false,
        )
        .bind(
            "source",
            |p: &mut C| &mut p.trace_source,
            preset_vector_one_or_more_string(),
            false,
        )
        .bind(
            "redirect",
            |p: &mut C| &mut p.trace_redirect,
            preset_string(),
            false,
        )
        .build();
    let obj = Object::<C>::new(errors::invalid_preset_object(), false);
    let obj = base_field!(obj, C, name);
    let obj = base_field!(obj, C, inherits);
    let obj = base_field!(obj, C, hidden);
    let obj = base_field!(obj, C, vendor);
    let obj = base_field!(obj, C, displayName);
    let obj = base_field!(obj, C, description);
    let obj = obj
        .bind(
            "generator",
            |p: &mut C| &mut p.generator,
            preset_string(),
            false,
        )
        .bind_fn(
            "architecture",
            arch_toolset_helper(|p| &mut p.architecture, |p| &mut p.architecture_strategy),
            false,
        )
        .bind_fn(
            "toolset",
            arch_toolset_helper(|p| &mut p.toolset, |p| &mut p.toolset_strategy),
            false,
        )
        .bind(
            "toolchainFile",
            |p: &mut C| &mut p.toolchain_file,
            preset_string(),
            false,
        )
        .bind(
            "graphviz",
            |p: &mut C| &mut p.graphviz_file,
            preset_string(),
            false,
        )
        .bind(
            "binaryDir",
            |p: &mut C| &mut p.binary_dir,
            preset_string(),
            false,
        )
        .bind(
            "installDir",
            |p: &mut C| &mut p.install_dir,
            preset_string(),
            false,
        )
        .bind_dummy::<String>("cmakeExecutable", preset_string(), false)
        .bind(
            "cacheVariables",
            |p: &mut C| &mut p.cache_variables,
            map_helper(e(errors::invalid_preset), variable_helper()),
            false,
        );
    let obj = base_field!(obj, C, environment);
    let obj = obj
        .bind_fn("warnings", warnings, false)
        .bind_fn("errors", errors_, false)
        .bind_fn("debug", debug, false)
        .bind_fn("trace", trace, false);
    let obj = base_field!(obj, C, condition);
    obj.build()
}

// ---------------------------------------------------------------------
// Build presets

fn package_resolve_mode(
    out: &mut Option<PackageResolveMode>,
    value: Option<&V>,
    state: &mut State,
) -> bool {
    let Some(v) = value else {
        *out = None;
        return true;
    };
    if !v.is_string() {
        errors::invalid_preset(value, state);
        return false;
    }
    *out = Some(match v.as_string().as_str() {
        "on" => PackageResolveMode::Force,
        "off" => PackageResolveMode::Disable,
        "only" => PackageResolveMode::OnlyResolve,
        _ => {
            errors::invalid_preset(value, state);
            return false;
        }
    });
    true
}

fn build_preset_helper() -> Helper<BuildPreset> {
    type B = BuildPreset;
    let obj = Object::<B>::new(errors::invalid_preset_object(), false);
    let obj = base_field!(obj, B, name);
    let obj = base_field!(obj, B, inherits);
    let obj = base_field!(obj, B, hidden);
    let obj = base_field!(obj, B, vendor);
    let obj = base_field!(obj, B, displayName);
    let obj = base_field!(obj, B, description);
    let obj = base_field!(obj, B, environment);
    let obj = obj
        .bind(
            "configurePreset",
            |p: &mut B| &mut p.configure_preset,
            preset_string(),
            false,
        )
        .bind(
            "inheritConfigureEnvironment",
            |p: &mut B| &mut p.inherit_configure_environment,
            preset_optional_bool(),
            false,
        )
        .bind(
            "jobs",
            |p: &mut B| &mut p.jobs,
            preset_optional_int(),
            false,
        )
        .bind(
            "targets",
            |p: &mut B| &mut p.targets,
            preset_vector_one_or_more_string(),
            false,
        )
        .bind(
            "configuration",
            |p: &mut B| &mut p.configuration,
            preset_string(),
            false,
        )
        .bind(
            "cleanFirst",
            |p: &mut B| &mut p.clean_first,
            preset_optional_bool(),
            false,
        )
        .bind(
            "verbose",
            |p: &mut B| &mut p.verbose,
            preset_optional_bool(),
            false,
        )
        .bind(
            "nativeToolOptions",
            |p: &mut B| &mut p.native_tool_options,
            preset_vector_string(),
            false,
        );
    let obj = base_field!(obj, B, condition);
    obj.bind(
        "resolvePackageReferences",
        |p: &mut B| &mut p.resolve_package_references,
        helper(package_resolve_mode),
        false,
    )
    .build()
}

// ---------------------------------------------------------------------
// Test presets

fn verbosity(out: &mut Verbosity, value: Option<&V>, state: &mut State) -> bool {
    let Some(v) = value else {
        *out = Verbosity::Default;
        return true;
    };
    if !v.is_string() {
        errors::invalid_preset(value, state);
        return false;
    }
    *out = match v.as_string().as_str() {
        "default" => Verbosity::Default,
        "verbose" => Verbosity::Verbose,
        "extra" => Verbosity::Extra,
        _ => {
            errors::invalid_preset(value, state);
            return false;
        }
    };
    true
}

fn truncation(out: &mut Option<TruncationMode>, value: Option<&V>, state: &mut State) -> bool {
    let Some(v) = value else {
        *out = None;
        return true;
    };
    if !v.is_string() {
        errors::invalid_preset(value, state);
        return false;
    }
    *out = Some(match v.as_string().as_str() {
        "tail" => TruncationMode::Tail,
        "middle" => TruncationMode::Middle,
        "head" => TruncationMode::Head,
        _ => {
            errors::invalid_preset(value, state);
            return false;
        }
    });
    true
}

fn show_only(out: &mut ShowOnly, value: Option<&V>, state: &mut State) -> bool {
    let Some(v) = value.filter(|v| v.is_string()) else {
        errors::invalid_preset(value, state);
        return false;
    };
    *out = match v.as_string().as_str() {
        "human" => ShowOnly::Human,
        "json-v1" => ShowOnly::JsonV1,
        _ => {
            errors::invalid_preset(value, state);
            return false;
        }
    };
    true
}

fn repeat_mode(out: &mut RepeatMode, value: Option<&V>, state: &mut State) -> bool {
    let Some(v) = value else {
        return true;
    };
    if !v.is_string() {
        errors::invalid_preset(value, state);
        return false;
    }
    *out = match v.as_string().as_str() {
        "until-fail" => RepeatMode::UntilFail,
        "until-pass" => RepeatMode::UntilPass,
        "after-timeout" => RepeatMode::AfterTimeout,
        _ => {
            errors::invalid_preset(value, state);
            return false;
        }
    };
    true
}

fn no_tests_action(out: &mut NoTestsAction, value: Option<&V>, state: &mut State) -> bool {
    let Some(v) = value else {
        *out = NoTestsAction::Default;
        return true;
    };
    if !v.is_string() {
        errors::invalid_preset(value, state);
        return false;
    }
    *out = match v.as_string().as_str() {
        "default" => NoTestsAction::Default,
        "error" => NoTestsAction::Error,
        "ignore" => NoTestsAction::Ignore,
        _ => {
            errors::invalid_preset(value, state);
            return false;
        }
    };
    true
}

fn test_preset_helper() -> Helper<TestPreset> {
    type T = TestPreset;
    type O = OutputOptions;
    let output = optional_helper(
        Object::<O>::new(invalid_object(), false)
            .bind(
                "shortProgress",
                |o: &mut O| &mut o.short_progress,
                preset_optional_bool(),
                false,
            )
            .bind(
                "verbosity",
                |o: &mut O| &mut o.verbosity,
                optional_helper(helper(verbosity)),
                false,
            )
            .bind(
                "debug",
                |o: &mut O| &mut o.debug,
                preset_optional_bool(),
                false,
            )
            .bind(
                "outputOnFailure",
                |o: &mut O| &mut o.output_on_failure,
                preset_optional_bool(),
                false,
            )
            .bind(
                "quiet",
                |o: &mut O| &mut o.quiet,
                preset_optional_bool(),
                false,
            )
            .bind(
                "outputLogFile",
                |o: &mut O| &mut o.output_log_file,
                preset_string(),
                false,
            )
            .bind(
                "outputJUnitFile",
                |o: &mut O| &mut o.output_junit_file,
                preset_string(),
                false,
            )
            .bind(
                "labelSummary",
                |o: &mut O| &mut o.label_summary,
                preset_optional_bool(),
                false,
            )
            .bind(
                "subprojectSummary",
                |o: &mut O| &mut o.subproject_summary,
                preset_optional_bool(),
                false,
            )
            .bind(
                "maxPassedTestOutputSize",
                |o: &mut O| &mut o.max_passed_test_output_size,
                preset_optional_int(),
                false,
            )
            .bind(
                "maxFailedTestOutputSize",
                |o: &mut O| &mut o.max_failed_test_output_size,
                preset_optional_int(),
                false,
            )
            .bind(
                "testOutputTruncation",
                |o: &mut O| &mut o.test_output_truncation,
                helper(truncation),
                false,
            )
            .bind(
                "maxTestNameWidth",
                |o: &mut O| &mut o.max_test_name_width,
                preset_optional_int(),
                false,
            )
            .build(),
    );
    let index_object = optional_helper(
        Object::<IndexOptions>::new(invalid_object(), true)
            .bind(
                "start",
                |o: &mut IndexOptions| &mut o.start,
                preset_optional_int(),
                false,
            )
            .bind(
                "end",
                |o: &mut IndexOptions| &mut o.end,
                preset_optional_int(),
                false,
            )
            .bind(
                "stride",
                |o: &mut IndexOptions| &mut o.stride,
                preset_optional_int(),
                false,
            )
            .bind(
                "specificTests",
                |o: &mut IndexOptions| &mut o.specific_tests,
                preset_vector_int(),
                false,
            )
            .build(),
    );
    let index = helper(
        move |out: &mut Option<IndexOptions>, value: Option<&V>, state: &mut State| {
            let Some(v) = value else {
                *out = None;
                return true;
            };
            if v.is_string() {
                *out = Some(IndexOptions {
                    index_file: v.as_string(),
                    ..IndexOptions::default()
                });
                return true;
            }
            if v.is_object() {
                return index_object(out, value, state);
            }
            false
        },
    );
    let include = optional_helper(
        Object::<IncludeOptions>::new(invalid_object(), true)
            .bind(
                "name",
                |o: &mut IncludeOptions| &mut o.name,
                preset_string(),
                false,
            )
            .bind(
                "label",
                |o: &mut IncludeOptions| &mut o.label,
                preset_string(),
                false,
            )
            .bind("index", |o: &mut IncludeOptions| &mut o.index, index, false)
            .bind(
                "useUnion",
                |o: &mut IncludeOptions| &mut o.use_union,
                preset_optional_bool(),
                false,
            )
            .build(),
    );
    let fixtures = optional_helper(
        Object::<FixturesOptions>::new(invalid_object(), true)
            .bind(
                "any",
                |o: &mut FixturesOptions| &mut o.any,
                preset_string(),
                false,
            )
            .bind(
                "setup",
                |o: &mut FixturesOptions| &mut o.setup,
                preset_string(),
                false,
            )
            .bind(
                "cleanup",
                |o: &mut FixturesOptions| &mut o.cleanup,
                preset_string(),
                false,
            )
            .build(),
    );
    let exclude = optional_helper(
        Object::<ExcludeOptions>::new(invalid_object(), true)
            .bind(
                "name",
                |o: &mut ExcludeOptions| &mut o.name,
                preset_string(),
                false,
            )
            .bind(
                "label",
                |o: &mut ExcludeOptions| &mut o.label,
                preset_string(),
                false,
            )
            .bind(
                "fixtures",
                |o: &mut ExcludeOptions| &mut o.fixtures,
                fixtures,
                false,
            )
            .build(),
    );
    let filter = optional_helper(
        Object::<FilterOptions>::new(invalid_object(), true)
            .bind(
                "include",
                |o: &mut FilterOptions| &mut o.include,
                include,
                false,
            )
            .bind(
                "exclude",
                |o: &mut FilterOptions| &mut o.exclude,
                exclude,
                false,
            )
            .build(),
    );
    let repeat = optional_helper(
        Object::<RepeatOptions>::new(invalid_object(), true)
            .bind(
                "mode",
                |o: &mut RepeatOptions| &mut o.mode,
                helper(repeat_mode),
                true,
            )
            .bind(
                "count",
                |o: &mut RepeatOptions| &mut o.count,
                preset_int(),
                true,
            )
            .build(),
    );
    type X = ExecutionOptions;
    let execution = optional_helper(
        Object::<X>::new(invalid_object(), true)
            .bind(
                "stopOnFailure",
                |o: &mut X| &mut o.stop_on_failure,
                preset_optional_bool(),
                false,
            )
            .bind(
                "enableFailover",
                |o: &mut X| &mut o.enable_failover,
                preset_optional_bool(),
                false,
            )
            .bind(
                "jobs",
                |o: &mut X| &mut o.jobs,
                preset_optional_int(),
                false,
            )
            .bind(
                "resourceSpecFile",
                |o: &mut X| &mut o.resource_spec_file,
                preset_string(),
                false,
            )
            .bind(
                "testLoad",
                |o: &mut X| &mut o.test_load,
                preset_optional_int(),
                false,
            )
            .bind(
                "showOnly",
                |o: &mut X| &mut o.show_only,
                optional_helper(helper(show_only)),
                false,
            )
            .bind("repeat", |o: &mut X| &mut o.repeat, repeat, false)
            .bind(
                "interactiveDebugging",
                |o: &mut X| &mut o.interactive_debugging,
                preset_optional_bool(),
                false,
            )
            .bind(
                "scheduleRandom",
                |o: &mut X| &mut o.schedule_random,
                preset_optional_bool(),
                false,
            )
            .bind(
                "timeout",
                |o: &mut X| &mut o.timeout,
                preset_optional_int(),
                false,
            )
            .bind(
                "noTestsAction",
                |o: &mut X| &mut o.no_tests_action,
                optional_helper(helper(no_tests_action)),
                false,
            )
            .build(),
    );
    let obj = Object::<T>::new(errors::invalid_preset_object(), false);
    let obj = base_field!(obj, T, name);
    let obj = base_field!(obj, T, inherits);
    let obj = base_field!(obj, T, hidden);
    let obj = base_field!(obj, T, vendor);
    let obj = base_field!(obj, T, displayName);
    let obj = base_field!(obj, T, description);
    let obj = base_field!(obj, T, environment);
    let obj = obj
        .bind(
            "configurePreset",
            |p: &mut T| &mut p.configure_preset,
            preset_string(),
            false,
        )
        .bind(
            "inheritConfigureEnvironment",
            |p: &mut T| &mut p.inherit_configure_environment,
            preset_optional_bool(),
            false,
        )
        .bind(
            "configuration",
            |p: &mut T| &mut p.configuration,
            preset_string(),
            false,
        )
        .bind(
            "overwriteConfigurationFile",
            |p: &mut T| &mut p.overwrite_configuration_file,
            preset_vector_string(),
            false,
        )
        .bind("output", |p: &mut T| &mut p.output, output, false)
        .bind("filter", |p: &mut T| &mut p.filter, filter, false)
        .bind("execution", |p: &mut T| &mut p.execution, execution, false);
    let obj = base_field!(obj, T, condition);
    obj.build()
}

// ---------------------------------------------------------------------
// Package presets

fn package_preset_helper() -> Helper<PackagePreset> {
    type P = PackagePreset;
    let output = Object::<P>::new(invalid_named_object_key(), false)
        .bind(
            "debug",
            |p: &mut P| &mut p.debug_output,
            preset_optional_bool(),
            false,
        )
        .bind(
            "verbose",
            |p: &mut P| &mut p.verbose_output,
            preset_optional_bool(),
            false,
        )
        .build();
    let variables = map_helper(
        e(errors::invalid_variable),
        string_helper(e(errors::invalid_variable), ""),
    );
    let obj = Object::<P>::new(errors::invalid_preset_object(), false);
    let obj = base_field!(obj, P, name);
    let obj = base_field!(obj, P, inherits);
    let obj = base_field!(obj, P, hidden);
    let obj = base_field!(obj, P, vendor);
    let obj = base_field!(obj, P, displayName);
    let obj = base_field!(obj, P, description);
    let obj = base_field!(obj, P, environment);
    let obj = obj
        .bind(
            "configurePreset",
            |p: &mut P| &mut p.configure_preset,
            preset_string(),
            false,
        )
        .bind(
            "inheritConfigureEnvironment",
            |p: &mut P| &mut p.inherit_configure_environment,
            preset_optional_bool(),
            false,
        )
        .bind(
            "generators",
            |p: &mut P| &mut p.generators,
            preset_vector_string(),
            false,
        )
        .bind(
            "configurations",
            |p: &mut P| &mut p.configurations,
            preset_vector_string(),
            false,
        )
        .bind("variables", |p: &mut P| &mut p.variables, variables, false)
        .bind(
            "configFile",
            |p: &mut P| &mut p.config_file,
            preset_string(),
            false,
        )
        .bind_fn("output", output, false)
        .bind(
            "packageName",
            |p: &mut P| &mut p.package_name,
            preset_string(),
            false,
        )
        .bind(
            "packageVersion",
            |p: &mut P| &mut p.package_version,
            preset_string(),
            false,
        )
        .bind(
            "packageDirectory",
            |p: &mut P| &mut p.package_directory,
            preset_string(),
            false,
        )
        .bind(
            "vendorName",
            |p: &mut P| &mut p.vendor_name,
            preset_string(),
            false,
        );
    let obj = base_field!(obj, P, condition);
    obj.build()
}

// ---------------------------------------------------------------------
// Workflow presets

fn step_type(out: &mut StepType, value: Option<&V>, state: &mut State) -> bool {
    let Some(v) = value else {
        errors::invalid_preset(value, state);
        return false;
    };
    if !v.is_string() {
        return false;
    }
    *out = match v.as_string().as_str() {
        "configure" => StepType::Configure,
        "build" => StepType::Build,
        "test" => StepType::Test,
        "package" => StepType::Package,
        _ => {
            errors::invalid_preset(value, state);
            return false;
        }
    };
    true
}

fn workflow_preset_helper() -> Helper<WorkflowPreset> {
    type W = WorkflowPreset;
    let step = Object::<WorkflowStep>::new(invalid_object(), false)
        .bind(
            "type",
            |s: &mut WorkflowStep| &mut s.preset_type,
            helper(step_type),
            true,
        )
        .bind(
            "name",
            |s: &mut WorkflowStep| &mut s.preset_name,
            preset_string(),
            true,
        )
        .build();
    let steps = vector_helper(e(errors::invalid_preset), step);
    let obj = Object::<W>::new(errors::invalid_preset_object(), false);
    let obj = base_field!(obj, W, name);
    let obj = base_field!(obj, W, vendor);
    let obj = base_field!(obj, W, displayName);
    let obj = base_field!(obj, W, description);
    obj.bind("steps", |p: &mut W| &mut p.steps, steps, true)
        .build()
}

// ---------------------------------------------------------------------
// Root

fn version_helper() -> Helper<i32> {
    required_helper(
        e(errors::no_version),
        int_helper(e(errors::invalid_version), 0),
    )
}

fn root_presets_helper() -> Helper<RootPresets> {
    type R = RootPresets;
    let cmake_version = Object::<CMakeVersion>::new(invalid_named_object_key(), false)
        .bind(
            "major",
            |v: &mut CMakeVersion| &mut v.major,
            uint_helper(e(errors::invalid_version), 0),
            false,
        )
        .bind(
            "minor",
            |v: &mut CMakeVersion| &mut v.minor,
            uint_helper(e(errors::invalid_version), 0),
            false,
        )
        .bind(
            "patch",
            |v: &mut CMakeVersion| &mut v.patch,
            uint_helper(e(errors::invalid_version), 0),
            false,
        )
        .build();
    let include = vector_helper(
        e(errors::invalid_include),
        string_helper(e(errors::invalid_include), ""),
    );
    Object::<R>::new(errors::invalid_root_object(), false)
        .bind_dummy::<i32>("version", version_helper(), true)
        .bind(
            "configurePresets",
            |r: &mut R| &mut r.configure_presets,
            vector_helper(e(errors::invalid_presets), configure_preset_helper()),
            false,
        )
        .bind(
            "buildPresets",
            |r: &mut R| &mut r.build_presets,
            vector_helper(e(errors::invalid_presets), build_preset_helper()),
            false,
        )
        .bind(
            "testPresets",
            |r: &mut R| &mut r.test_presets,
            vector_helper(e(errors::invalid_presets), test_preset_helper()),
            false,
        )
        .bind(
            "packagePresets",
            |r: &mut R| &mut r.package_presets,
            vector_helper(e(errors::invalid_presets), package_preset_helper()),
            false,
        )
        .bind(
            "workflowPresets",
            |r: &mut R| &mut r.workflow_presets,
            vector_helper(e(errors::invalid_presets), workflow_preset_helper()),
            false,
        )
        .bind(
            "cmakeMinimumRequired",
            |r: &mut R| &mut r.cmake_minimum_required,
            cmake_version,
            false,
        )
        .bind("include", |r: &mut R| &mut r.include, include, false)
        .bind_dummy::<()>("vendor", vendor_helper(e(errors::invalid_root)), false)
        .bind_dummy::<()>("$schema", helper(|_: &mut (), _, _| true), false)
        .build()
}

/// `cmSystemTools::SameFile`.
fn same_file(a: &str, b: &str) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    }
}

impl Graph {
    pub(super) fn read_json_file(
        &mut self,
        filename: &str,
        root_type: RootType,
        reason: ReadReason,
        in_progress: &mut Vec<usize>,
        file_out: &mut usize,
    ) -> bool {
        for (index, f) in self.files.iter().enumerate() {
            if same_file(filename, &f.filename) {
                *file_out = index;
                if in_progress.contains(&index) {
                    errors::cyclic_include(filename, &mut self.parse_state);
                    return false;
                }
                return true;
            }
        }

        let (state, root) = State::read(filename);
        self.parse_state = state;
        if self.parse_state.has_errors() {
            return false;
        }
        let root = root.expect("parsed root");

        let mut v = 0;
        let root_version = Object::<i32>::new(errors::invalid_root_object(), true)
            .bind(
                "version",
                |v: &mut i32| v,
                checked_helper(
                    errors::unrecognized_version_range(MIN_VERSION, MAX_VERSION),
                    version_helper(),
                    |v: &i32| (MIN_VERSION..=MAX_VERSION).contains(v),
                ),
                false,
            )
            .build();
        if !root_version(&mut v, Some(&root), &mut self.parse_state) {
            return false;
        }

        if v < 2 {
            if root.is_member("buildPresets") || root.is_member("testPresets") {
                errors::build_test_presets_unsupported(&mut self.parse_state);
                return false;
            }
        }
        if v < 6 && root.is_member("packagePresets") {
            errors::package_presets_unsupported(&mut self.parse_state);
            return false;
        }
        if v < 6 && root.is_member("workflowPresets") {
            errors::workflow_presets_unsupported(&mut self.parse_state);
            return false;
        }
        if v < 4 && root.is_member("include") {
            errors::include_unsupported(&mut self.parse_state);
            return false;
        }
        if v < 8 && root.is_member("$schema") {
            errors::schema_unsupported(&mut self.parse_state);
            return false;
        }
        self.parse_state.allow_comments = v >= 10;

        let mut presets = RootPresets::default();
        if !root_presets_helper()(&mut presets, Some(&root), &mut self.parse_state) {
            return false;
        }

        let (major, minor, patch) = crate::interp::VERSION;
        let required = &presets.cmake_minimum_required;
        let cmr = root.member("cmakeMinimumRequired");
        let field = |name: &str| cmr.as_ref().and_then(|c| c.member(name));
        if required.major > major {
            errors::unrecognized_cmake_version(
                "major",
                major,
                required.major,
                field("major").as_ref(),
                &mut self.parse_state,
            );
            return false;
        }
        if required.major == major {
            if required.minor > minor {
                errors::unrecognized_cmake_version(
                    "minor",
                    minor,
                    required.minor,
                    field("minor").as_ref(),
                    &mut self.parse_state,
                );
                return false;
            }
            if required.minor == minor && required.patch > patch {
                errors::unrecognized_cmake_version(
                    "patch",
                    patch,
                    required.patch,
                    field("patch").as_ref(),
                    &mut self.parse_state,
                );
                return false;
            }
        }

        let file = self.files.len();
        self.files.push(File {
            filename: filename.to_owned(),
            version: v,
            reachable: [file].into_iter().collect(),
        });
        in_progress.push(file);
        *file_out = file;

        for mut preset in presets.configure_presets {
            preset.base.origin_file = file;
            if preset.base.name.is_empty() {
                return false;
            }
            let name = preset.base.name.clone();
            if self.configure_presets.contains_key(&name) {
                errors::duplicate_presets(&name, &mut self.parse_state);
                return false;
            }
            let (install_dir, condition, toolchain, trace, graphviz) = (
                !preset.install_dir.is_empty(),
                preset.base.condition.is_some(),
                !preset.toolchain_file.is_empty(),
                preset.trace_mode.is_some()
                    || preset.trace_format.is_some()
                    || !preset.trace_redirect.is_empty()
                    || !preset.trace_source.is_empty(),
                !preset.graphviz_file.is_empty(),
            );
            self.configure_presets.insert(
                name.clone(),
                PresetPair {
                    unexpanded: preset,
                    expanded: None,
                },
            );
            if v < 3 && install_dir {
                errors::install_prefix_unsupported(
                    root.member("installDir").as_ref(),
                    &mut self.parse_state,
                );
                return false;
            }
            if v < 3 && condition {
                errors::condition_unsupported(&mut self.parse_state);
                return false;
            }
            if v < 3 && toolchain {
                errors::toolchain_file_unsupported(&mut self.parse_state);
                return false;
            }
            if v < 7 && trace {
                errors::trace_unsupported(&mut self.parse_state);
                return false;
            }
            if v < 10 && graphviz {
                errors::graphviz_file_unsupported(&mut self.parse_state);
                return false;
            }
            self.configure_order.push(name);
        }

        for mut preset in presets.build_presets {
            preset.base.origin_file = file;
            if preset.base.name.is_empty() {
                return false;
            }
            let name = preset.base.name.clone();
            if self.build_presets.contains_key(&name) {
                errors::duplicate_presets(&name, &mut self.parse_state);
                return false;
            }
            let condition = preset.base.condition.is_some();
            self.build_presets.insert(
                name.clone(),
                PresetPair {
                    unexpanded: preset,
                    expanded: None,
                },
            );
            if v < 3 && condition {
                errors::condition_unsupported(&mut self.parse_state);
                return false;
            }
            self.build_order.push(name);
        }

        for mut preset in presets.test_presets {
            preset.base.origin_file = file;
            if preset.base.name.is_empty() {
                return false;
            }
            let name = preset.base.name.clone();
            if self.test_presets.contains_key(&name) {
                errors::duplicate_presets(&name, &mut self.parse_state);
                return false;
            }
            let condition = preset.base.condition.is_some();
            let truncation = preset
                .output
                .as_ref()
                .is_some_and(|o| o.test_output_truncation.is_some());
            let junit = preset
                .output
                .as_ref()
                .is_some_and(|o| !o.output_junit_file.is_empty());
            self.test_presets.insert(
                name.clone(),
                PresetPair {
                    unexpanded: preset,
                    expanded: None,
                },
            );
            if v < 3 && condition {
                errors::condition_unsupported(&mut self.parse_state);
                return false;
            }
            if v < 5 && truncation {
                errors::test_output_truncation_unsupported(&mut self.parse_state);
                return false;
            }
            if v < 6 && junit {
                errors::ctest_junit_unsupported(&mut self.parse_state);
                return false;
            }
            self.test_order.push(name);
        }

        for mut preset in presets.package_presets {
            preset.base.origin_file = file;
            if preset.base.name.is_empty() {
                return false;
            }
            let name = preset.base.name.clone();
            if self.package_presets.contains_key(&name) {
                errors::duplicate_presets(&name, &mut self.parse_state);
                return false;
            }
            self.package_presets.insert(
                name.clone(),
                PresetPair {
                    unexpanded: preset,
                    expanded: None,
                },
            );
            self.package_order.push(name);
        }

        for mut preset in presets.workflow_presets {
            preset.base.origin_file = file;
            if preset.base.name.is_empty() {
                return false;
            }
            let name = preset.base.name.clone();
            if self.workflow_presets.contains_key(&name) {
                errors::duplicate_presets(&name, &mut self.parse_state);
                return false;
            }
            self.workflow_presets.insert(
                name.clone(),
                PresetPair {
                    unexpanded: preset,
                    expanded: None,
                },
            );
            self.workflow_order.push(name);
        }

        let include_items: Vec<V> = root
            .member("include")
            .map(|i| i.items().to_vec())
            .unwrap_or_default();
        for (i, include) in presets.include.iter().enumerate() {
            let mut include = include.clone();
            if v >= 7 {
                let mut expanders = Vec::new();
                if v >= 9 {
                    expanders.push(Expander::Base {
                        source_dir: &self.source_dir,
                        file: Some(filename.to_owned()),
                    });
                }
                expanders.push(Expander::ProcessEnvironment);
                if expand_macros(&mut include, &expanders, v) != ExpandResult::Ok {
                    let item = include_items.get(i).cloned();
                    errors::invalid_include(item.as_ref(), &mut self.parse_state);
                    return false;
                }
            }
            if !crate::util::is_full_path(&include) {
                let directory = crate::util::filename_path(filename);
                include = format!("{directory}/{include}");
            }
            if !self.include_file(&include, root_type, ReadReason::Included, in_progress, file) {
                return false;
            }
        }

        if root_type == RootType::User && reason == ReadReason::Root {
            let project = Graph::filename(&self.source_dir);
            if std::path::Path::new(&project).exists()
                && !self.include_file(
                    &project,
                    RootType::Project,
                    ReadReason::Root,
                    in_progress,
                    file,
                )
            {
                return false;
            }
        }

        in_progress.pop();
        true
    }

    fn include_file(
        &mut self,
        include: &str,
        root_type: RootType,
        reason: ReadReason,
        in_progress: &mut Vec<usize>,
        file: usize,
    ) -> bool {
        let mut included = 0;
        if !self.read_json_file(include, root_type, reason, in_progress, &mut included) {
            return false;
        }
        let reachable = self.files[included].reachable.clone();
        self.files[file].reachable.extend(reachable);
        true
    }
}
