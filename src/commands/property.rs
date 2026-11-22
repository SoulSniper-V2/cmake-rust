//! Builtins for defining, querying, and changing CMake properties.

use std::collections::BTreeSet;

use crate::cache::CacheType;
use crate::interp::{CmdErr, CmdResult, Interp, err};
use crate::messages::MessageType;
use crate::policy::PolicyStatus;
use crate::properties::{self as prop, Definition, Object, Scope};
use crate::state::find_target_to_use;
use crate::util::{collapse_full_path, is_off, is_on};

pub fn register(interp: &mut Interp) {
    for (name, function) in [
        ("define_property", define_property as crate::interp::Builtin),
        ("get_property", get_property),
        ("set_property", set_property),
        ("get_cmake_property", get_cmake_property),
        ("get_directory_property", get_directory_property),
        ("set_directory_properties", set_directory_properties),
        ("get_source_file_property", get_source_file_property),
        ("set_source_files_properties", set_source_files_properties),
        ("get_target_property", get_target_property),
        ("set_target_properties", set_target_properties),
        ("get_test_property", get_test_property),
        ("set_tests_properties", set_tests_properties),
    ] {
        interp.register(name, function);
    }
}

const WRONG_COUNT: &str = "called with incorrect number of arguments";
const NO_PROPERTY: &str = "not given a PROPERTY <name> argument.";
const NO_DIRECTORY: &str = "DIRECTORY scope provided but requested directory was not found. This could be because the directory argument was invalid or, it is valid but has not been processed yet.";
const NO_PROPERTIES: &str = "called with illegal arguments, maybe missing a PROPERTIES specifier?";

fn invalid_arg(arg: &str) -> CmdErr {
    format!("given invalid argument \"{arg}\".").into()
}

fn scope(arg: &str, valid: &str) -> Result<Scope, CmdErr> {
    Scope::parse(arg)
        .filter(|_| valid.split(", ").any(|name| name == arg))
        .ok_or_else(|| format!("given invalid scope {arg}.  Valid scopes are {valid}.").into())
}

fn define_property(interp: &mut Interp, args: &[String]) -> CmdResult {
    if interp.mode == crate::interp::Mode::Script {
        return err("command is not scriptable");
    }
    let Some((kind, args)) = args.split_first() else {
        return err(WRONG_COUNT);
    };
    let scope = scope(
        kind,
        "GLOBAL, DIRECTORY, TARGET, SOURCE, TEST, VARIABLE, CACHED_VARIABLE",
    )?;
    let mut name = "";
    let mut definition = Definition::default();
    let mut field = "";
    for arg in args {
        match arg.as_str() {
            "PROPERTY" | "BRIEF_DOCS" | "FULL_DOCS" | "INITIALIZE_FROM_VARIABLE" => field = arg,
            "INHERITED" => {
                definition.inherited = true;
                field = "";
            }
            _ => match field {
                "PROPERTY" => {
                    name = arg;
                    field = "";
                }
                "BRIEF_DOCS" => definition.brief.push_str(arg),
                "FULL_DOCS" => definition.full.push_str(arg),
                "INITIALIZE_FROM_VARIABLE" => {
                    definition.initialize_from_variable.clone_from(arg);
                    field = "";
                }
                _ => return Err(invalid_arg(arg)),
            },
        }
    }
    if name.is_empty() {
        return err(NO_PROPERTY);
    }
    let variable = &definition.initialize_from_variable;
    if !variable.is_empty() {
        if scope != Scope::Target {
            return err("Scope must be TARGET if INITIALIZE_FROM_VARIABLE is specified");
        }
        if !variable.ends_with(name) {
            return err(format!(
                "Variable name \"{variable}\" does not end with property name \"{name}\""
            ));
        }
        if !name.contains('_') {
            return err(format!(
                "Property name \"{name}\" defined with INITIALIZE_FROM_VARIABLE does not contain underscore"
            ));
        }
        if variable.starts_with("CMAKE_") || variable.starts_with("_CMAKE_") {
            return err(format!("variable name \"{variable}\" is reserved"));
        }
    }
    interp
        .ext
        .property_definitions
        .define(scope, name, definition);
    Ok(())
}

#[derive(Default)]
struct SourceDirectories<'a> {
    directories: Vec<&'a str>,
    targets: Vec<&'a str>,
    directory_option: bool,
    target_option: bool,
}

fn missing_scope(option: &str) -> CmdErr {
    format!("{WRONG_COUNT} no value provided to the {option} option").into()
}

fn directory_option(interp: &Interp, dir: &str) -> Result<usize, CmdErr> {
    prop::find_directory(interp, dir)
        .ok_or_else(|| format!("given non-existent DIRECTORY {dir}").into())
}

impl SourceDirectories<'_> {
    fn resolve(&self, interp: &Interp) -> Result<Vec<usize>, CmdErr> {
        if self.directory_option && self.directories.is_empty() {
            return Err(missing_scope("DIRECTORY"));
        }
        if self.target_option && self.targets.is_empty() {
            return Err(missing_scope("TARGET_DIRECTORY"));
        }
        let mut dirs = Vec::new();
        for path in &self.directories {
            let dir = directory_option(interp, path)?;
            if !dirs.contains(&dir) {
                dirs.push(dir);
            }
        }
        for name in &self.targets {
            let dir = find_target_to_use(interp, name)
                .ok_or_else(|| format!("given non-existent target for TARGET_DIRECTORY {name}"))?
                .makefile;
            if !dirs.contains(&dir) {
                dirs.push(dir);
            }
        }
        if dirs.is_empty() {
            dirs.push(interp.mf_index());
        }
        Ok(dirs)
    }

    fn source_path(&self, interp: &Interp, path: &str) -> String {
        if self.directory_option || self.target_option {
            collapse_full_path(path, &interp.current_source_dir())
        } else {
            path.into()
        }
    }
}

fn test_directory(interp: &Interp, directory: Option<&str>) -> Result<usize, CmdErr> {
    match directory {
        Some("") => Err(missing_scope("DIRECTORY")),
        Some(dir) => directory_option(interp, dir),
        None => Ok(interp.mf_index()),
    }
}

#[derive(PartialEq, Eq)]
enum Field {
    Name,
    Property,
    Value,
    Directory,
    TargetDirectory,
    TestDirectory,
    None,
}

#[derive(PartialEq, Eq)]
enum Query {
    Value,
    Set,
    Defined,
    Brief,
    Full,
}

fn get_property(interp: &mut Interp, args: &[String]) -> CmdResult {
    if args.len() < 3 {
        return err(WRONG_COUNT);
    }
    let (output, kind) = (&args[0], &args[1]);
    let scope = scope(
        kind,
        "GLOBAL, DIRECTORY, TARGET, SOURCE, TEST, VARIABLE, CACHE, INSTALL",
    )?;
    let mut name = "";
    let mut property = "";
    let mut query = Query::Value;
    let mut field = Field::Name;
    let mut source = SourceDirectories::default();
    let mut test_dir = None;

    for arg in &args[2..] {
        let mode = match arg.as_str() {
            "SET" => Some(Query::Set),
            "DEFINED" => Some(Query::Defined),
            "BRIEF_DOCS" => Some(Query::Brief),
            "FULL_DOCS" => Some(Query::Full),
            _ => None,
        };
        if let Some(mode) = mode {
            query = mode;
            field = Field::None;
        } else if arg == "PROPERTY" {
            field = Field::Property;
        } else {
            field = match field {
                Field::Name => {
                    name = arg;
                    Field::None
                }
                Field::None if scope == Scope::Source && arg == "DIRECTORY" => {
                    source.directory_option = true;
                    Field::Directory
                }
                Field::None if scope == Scope::Source && arg == "TARGET_DIRECTORY" => {
                    source.target_option = true;
                    Field::TargetDirectory
                }
                Field::None if scope == Scope::Test && arg == "DIRECTORY" => {
                    test_dir = Some("");
                    Field::TestDirectory
                }
                Field::Directory => {
                    source.directories.push(arg);
                    Field::None
                }
                Field::TargetDirectory => {
                    source.targets.push(arg);
                    Field::None
                }
                Field::TestDirectory => {
                    test_dir = Some(arg);
                    Field::None
                }
                Field::Property => {
                    property = arg;
                    Field::None
                }
                _ => return Err(invalid_arg(arg)),
            };
        }
    }
    if property.is_empty() {
        return err(NO_PROPERTY);
    }
    let dirs = source.resolve(interp)?;
    let test_dir = test_directory(interp, test_dir)?;
    if matches!(query, Query::Defined | Query::Brief | Query::Full) {
        let def = interp.ext.property_definitions.get(scope, property);
        let value = match query {
            Query::Defined => {
                if def.is_some() {
                    "1"
                } else {
                    "0"
                }
            }
            Query::Brief => def
                .map(|def| def.brief.as_str())
                .filter(|value| !value.is_empty())
                .unwrap_or("NOTFOUND"),
            _ => def
                .map(|def| def.full.as_str())
                .filter(|value| !value.is_empty())
                .unwrap_or("NOTFOUND"),
        }
        .to_owned();
        interp.set_def(output, value);
        return Ok(());
    }

    if matches!(scope, Scope::Global | Scope::Variable) && !name.is_empty() {
        return err(format!("given name for {kind} scope."));
    }
    if !matches!(scope, Scope::Global | Scope::Directory | Scope::Variable) && name.is_empty() {
        return err(format!("not given name for {kind} scope."));
    }
    let path = source.source_path(interp, name);
    let object = match scope {
        Scope::Global => Object::Global,
        Scope::Directory => Object::Directory(if name.is_empty() {
            interp.mf_index()
        } else {
            prop::find_directory(interp, name).ok_or(NO_DIRECTORY)?
        }),
        Scope::Target => {
            if find_target_to_use(interp, name).is_none() {
                return err(format!(
                    "could not find TARGET {name}.  Perhaps it has not yet been created."
                ));
            }
            Object::Target(name)
        }
        Scope::Source => {
            // cmGetPropertyCommand::HandleSourceMode: GENERATED under
            // CMP0163 NEW consults only the global generated registry
            // (caller's binary then source directory) and never creates a
            // source file entry.
            let cmp0163 = interp.policy_status_at(dirs[0], "CMP0163");
            if property == "GENERATED"
                && cmp0163 != PolicyStatus::Old
                && cmp0163 != PolicyStatus::Warn
            {
                let generated = [interp.current_binary_dir(), interp.current_source_dir()]
                    .iter()
                    .any(|dir| {
                        interp
                            .ext
                            .generated_sources
                            .contains(&collapse_full_path(name, dir))
                    });
                store(
                    interp,
                    output,
                    query,
                    Some(if generated { "1".into() } else { "0".into() }),
                );
                return Ok(());
            }
            prop::ensure_source(interp, dirs[0], &path);
            Object::Source(dirs[0], &path)
        }
        Scope::Test => {
            if !interp
                .ext
                .tests
                .iter()
                .any(|test| test.name == name && test.makefile == test_dir)
            {
                return err(format!("given TEST name that does not exist: {name}"));
            }
            Object::Test(test_dir, name)
        }
        Scope::Cache => Object::Cache(name),
        Scope::Install => Object::Install(name),
        Scope::Variable => {
            let value = interp.read_definition(property);
            store(interp, output, query, value);
            return Ok(());
        }
        Scope::CachedVariable => unreachable!(),
    };
    let value = match object {
        Object::Target(name) => target_value(interp, name, property),
        _ => prop::get(interp, object, property),
    };
    store(interp, output, query, value);
    Ok(())
}

/// `cmTarget::GetProperty` (with its computed properties and diagnostics),
/// falling back to INHERITED property definitions.
fn target_value(interp: &mut Interp, name: &str, property: &str) -> Option<String> {
    crate::commands::target::read_target_property(interp, name, property)
        .or_else(|| prop::get(interp, Object::Target(name), property))
}

fn store(interp: &mut Interp, output: &str, query: Query, value: Option<String>) {
    if query == Query::Set {
        interp.set_def(output, if value.is_some() { "1" } else { "0" });
    } else if let Some(value) = value {
        interp.set_def(output, value);
    } else {
        interp.unset_def(output);
    }
}

#[derive(Clone, Copy)]
enum Change<'a> {
    Set(Option<&'a str>),
    Append(&'a str, bool),
}

fn change(interp: &mut Interp, object: Object<'_>, name: &str, change: Change<'_>) {
    if let Object::Target(target) = object
        && !validate_target(interp, target, name, change)
    {
        return;
    }
    if let Object::Source(index, path) = object
        && name == "GENERATED"
        && handle_generated(interp, index, path, change)
    {
        return;
    }
    match change {
        Change::Set(value) => prop::set(interp, object, name, value),
        Change::Append(value, as_string) => prop::append(interp, object, name, value, as_string),
    }
}

fn handle_generated(interp: &mut Interp, index: usize, path: &str, change: Change<'_>) -> bool {
    let (value, append) = match change {
        Change::Set(value) => (value.unwrap_or_default(), false),
        Change::Append(value, _) => (value, true),
    };
    let cmp0163 = interp.policy_status_at(index, "CMP0163");
    let cmp0118 = interp.policy_status_at(index, "CMP0118");
    let policy = if cmp0163 == PolicyStatus::New {
        Some("CMP0163")
    } else if cmp0118 == PolicyStatus::New {
        Some("CMP0118")
    } else {
        None
    };
    if let Some(policy) = policy {
        let error = if !is_on(value) && !is_off(value) {
            Some(format!(
                "Policy {policy} is set to NEW and the following non-boolean value given for property 'GENERATED' is therefore not allowed:\n{value}\nReplace it with a boolean value!\n"
            ))
        } else if is_off(value) {
            Some(format!(
                "Unsetting the 'GENERATED' property is not allowed under {policy}!\n"
            ))
        } else if append {
            Some(format!(
                "Policy {policy} is set to NEW and appending to the 'GENERATED' property is therefore not allowed. Only setting it to \"1\" is allowed!\n"
            ))
        } else {
            None
        };
        // mf.IssueMessage uses the source directory's own backtrace (the
        // add_subdirectory/include frame for other directories).
        let backtrace = interp.directory_backtrace(index);
        if let Some(error) = error {
            // cmMessenger::ConvertMessageType demotes AUTHOR_ERROR to a
            // developer warning unless dev warnings are errors.
            interp.issue_at(MessageType::AuthorError, &error, &backtrace);
        } else {
            // cmSourceFile::MarkAsGenerated on the ambiguously-located
            // source created by ensure_source: the binary directory wins.
            crate::commands::target::mark_generated_ambiguous(interp, index, path);
        }
        return true;
    }
    if cmp0118 == PolicyStatus::Warn {
        let backtrace = interp.directory_backtrace(index);
        let warning = crate::policy::index_of("CMP0118")
            .map(crate::policy::policy_warning)
            .unwrap_or_default();
        if !is_on(value) && !is_off(value) {
            interp.issue_at(MessageType::AuthorWarning, &format!("{warning}\nAttempt to set property 'GENERATED' with the following non-boolean value (which will be interpreted as \"0\"):\n{value}\nThat exact value will not be retrievable. A value of \"0\" will be returned instead.\nThis will be an error under policy CMP0118.\n"), &backtrace);
        }
        if is_off(value) {
            interp.issue_at(
                MessageType::AuthorWarning,
                &format!("{warning}\nUnsetting property 'GENERATED' will not be allowed under policy CMP0118!\n"),
                &backtrace,
            );
        }
        if append {
            interp.issue_at(
                MessageType::AuthorWarning,
                &format!("{warning}\nAppending to property 'GENERATED' will not be allowed under policy CMP0118!\n"),
                &backtrace,
            );
        }
    }
    false
}

fn validate_target(interp: &mut Interp, name: &str, property: &str, change: Change<'_>) -> bool {
    let Some(key) = crate::state::find_target_key(interp, name) else {
        return false;
    };
    let target = &interp.ext.targets[&key];
    let policy = interp.policy_status("CMP0160");
    let read_only = match property {
        "NAME"
        | "TYPE"
        | "MANUALLY_ADDED_DEPENDENCIES"
        | "HEADER_SETS"
        | "INTERFACE_HEADER_SETS" => Some("is read-only for target"),
        "EXPORT_NAME" | "SOURCES" if target.imported => Some("can't be set on imported targets"),
        "IMPORTED_GLOBAL" if !target.imported => Some("can't be set on non-imported targets"),
        "ALIAS_GLOBAL"
        | "BINARY_DIR"
        | "CXX_MODULE_SETS"
        | "IMPORTED"
        | "INTERFACE_CXX_MODULE_SETS"
        | "LOCATION"
        | "LOCATION_CONFIG"
        | "SOURCE_DIR"
            if policy != PolicyStatus::Old =>
        {
            let message = format!("{property} property is read-only for target(\"{name}\")\n");
            if policy == PolicyStatus::Warn {
                let warning = crate::policy::policy_warning(
                    crate::policy::index_of("CMP0160").expect("registered policy"),
                );
                interp.issue(MessageType::AuthorWarning, &format!("{warning}\n{message}"));
                return true;
            }
            Some("is read-only for target")
        }
        _ => None,
    };
    if let Some(reason) = read_only {
        interp.issue(
            MessageType::FatalError,
            &format!("{property} property {reason}(\"{name}\")\n"),
        );
        return false;
    }
    if matches!(change, Change::Append(..)) {
        let error = match property {
            "C_STANDARD" | "CXX_STANDARD" | "CUDA_STANDARD" | "HIP_STANDARD" | "OBJC_STANDARD"
            | "OBJCXX_STANDARD" => Some(format!("{property} property may not be appended.")),
            property if property.starts_with("IMPORTED_LIBNAME") => {
                Some(format!("{property} property may not be APPENDed."))
            }
            _ => None,
        };
        if let Some(error) = error {
            interp.issue(MessageType::FatalError, &error);
            return false;
        }
    }
    // cmTarget::SetProperty's PRECOMPILE_HEADERS_REUSE_FROM handling (and
    // the matching AppendProperty check) live in write_target_property.
    let reuse_set = property == "PRECOMPILE_HEADERS_REUSE_FROM" && matches!(change, Change::Set(_));
    let pch_append = property == "PRECOMPILE_HEADERS"
        && matches!(change, Change::Append(..))
        && target.props.contains("PRECOMPILE_HEADERS_REUSE_FROM");
    if reuse_set || pch_append {
        let (value, how) = match change {
            Change::Set(value) => (value, crate::commands::target::PropertyWrite::Set),
            Change::Append(value, true) => (
                Some(value),
                crate::commands::target::PropertyWrite::AppendString,
            ),
            Change::Append(value, false) => {
                (Some(value), crate::commands::target::PropertyWrite::Append)
            }
        };
        if let Err(crate::interp::CmdErr::Error(message)) =
            crate::commands::target::write_target_property(interp, name, property, value, how)
        {
            interp.issue(MessageType::FatalError, &message);
        }
        return false;
    }
    if property == "IMPORTED_GLOBAL" {
        let error = if matches!(change, Change::Append(_, _)) {
            format!(
                "IMPORTED_GLOBAL property can't be appended, only set on imported targets (\"{name}\")\n"
            )
        } else if !matches!(change, Change::Set(Some(value)) if is_on(value)) {
            format!("IMPORTED_GLOBAL property can't be set to FALSE on targets (\"{name}\")\n")
        } else {
            let current = interp.mf_index();
            let target = interp.ext.targets.get_mut(&key).expect("resolved target");
            target.imported_global = true;
            if target.makefile == current {
                return false;
            }
            format!(
                "Attempt to promote imported target \"{name}\" to global scope (by setting IMPORTED_GLOBAL) which is not built in this directory.\n"
            )
        };
        interp.issue(MessageType::FatalError, &error);
        return false;
    }
    true
}

fn set_property(interp: &mut Interp, args: &[String]) -> CmdResult {
    if args.len() < 2 {
        return err(WRONG_COUNT);
    }
    let scope = scope(
        &args[0],
        "GLOBAL, DIRECTORY, TARGET, SOURCE, TEST, CACHE, INSTALL",
    )?;
    let mut names = BTreeSet::new();
    let mut property = "";
    let mut values = Vec::new();
    let mut append = None;
    let mut field = Field::Name;
    let mut source = SourceDirectories::default();
    let mut test_dir = None;
    for arg in &args[1..] {
        match arg.as_str() {
            "PROPERTY" => field = Field::Property,
            "APPEND" | "APPEND_STRING" => {
                field = Field::None;
                append = Some(arg == "APPEND_STRING");
            }
            "DIRECTORY"
                if !matches!(field, Field::Property | Field::Value) && scope == Scope::Source =>
            {
                source.directory_option = true;
                field = Field::Directory;
            }
            "TARGET_DIRECTORY"
                if !matches!(field, Field::Property | Field::Value) && scope == Scope::Source =>
            {
                source.target_option = true;
                field = Field::TargetDirectory;
            }
            "DIRECTORY"
                if !matches!(field, Field::Property | Field::Value) && scope == Scope::Test =>
            {
                test_dir = Some("");
                field = Field::TestDirectory;
            }
            _ => match field {
                Field::Name => {
                    names.insert(arg.as_str());
                }
                Field::Directory => source.directories.push(arg),
                Field::TargetDirectory => source.targets.push(arg),
                Field::TestDirectory => {
                    test_dir = Some(arg);
                    field = Field::None;
                }
                Field::Property => {
                    property = arg;
                    field = Field::Value;
                }
                Field::Value => values.push(arg.as_str()),
                Field::None => return Err(invalid_arg(arg)),
            },
        }
    }
    if property.is_empty() {
        return err(NO_PROPERTY);
    }
    let dirs = source.resolve(interp)?;
    let test_dir = test_directory(interp, test_dir)?;
    let value = values.join(";");
    let operation = append.map_or(
        Change::Set((!values.is_empty()).then_some(value.as_str())),
        |as_string| Change::Append(&value, as_string),
    );
    match scope {
        Scope::Global => {
            if !names.is_empty() {
                return err("given names for GLOBAL scope.");
            }
            change(interp, Object::Global, property, operation);
        }
        Scope::Directory => {
            if names.len() > 1 {
                return err("allows at most one name for DIRECTORY scope.");
            }
            let index = match names.first() {
                Some(path) => prop::find_directory(interp, path).ok_or(NO_DIRECTORY)?,
                None => interp.mf_index(),
            };
            change(interp, Object::Directory(index), property, operation);
        }
        Scope::Target => {
            for name in names {
                if interp.ext.aliases.contains_key(name) {
                    return err("can not be used on an ALIAS target.");
                }
                if find_target_to_use(interp, name).is_none() {
                    return err(format!(
                        "could not find TARGET {name}.  Perhaps it has not yet been created."
                    ));
                }
                change(interp, Object::Target(name), property, operation);
            }
        }
        Scope::Source => {
            for path in names {
                let path = source.source_path(interp, path);
                for &index in &dirs {
                    prop::ensure_source(interp, index, &path);
                    change(interp, Object::Source(index, &path), property, operation);
                }
            }
        }
        Scope::Test => {
            let mut missing = String::new();
            for name in names {
                if interp
                    .ext
                    .tests
                    .iter()
                    .any(|test| test.name == name && test.makefile == test_dir)
                {
                    change(interp, Object::Test(test_dir, name), property, operation);
                } else {
                    missing.push_str(&format!("  {name}\n"));
                }
            }
            if !missing.is_empty() {
                return err(format!("given TEST names that do not exist:\n{missing}"));
            }
        }
        Scope::Cache => {
            match property {
                "ADVANCED"
                    if !matches!(operation, Change::Set(None))
                        && !is_on(&value)
                        && !is_off(&value) =>
                {
                    return err(format!(
                        "given non-boolean value \"{value}\" for CACHE property \"ADVANCED\".  "
                    ));
                }
                "TYPE" if CacheType::parse_strict(&value).is_none() => {
                    return err(format!("given invalid CACHE entry TYPE \"{value}\""));
                }
                "ADVANCED" | "TYPE" | "HELPSTRING" | "STRINGS" | "VALUE" => {}
                _ => {
                    return err(format!(
                        "given invalid CACHE property {property}.  Settable CACHE properties are: ADVANCED, HELPSTRING, STRINGS, TYPE, and VALUE."
                    ));
                }
            }
            for name in names {
                if interp.cache.value(name).is_none() {
                    return err(format!(
                        "could not find CACHE variable {name}.  Perhaps it has not yet been created."
                    ));
                }
                // Cache removal first erases the property, then assigns its empty value.
                let operation = if matches!(operation, Change::Set(None)) {
                    Change::Set(Some(""))
                } else {
                    operation
                };
                change(interp, Object::Cache(name), property, operation);
            }
        }
        Scope::Install => {
            for name in names {
                change(interp, Object::Install(name), property, operation);
            }
        }
        _ => unreachable!(),
    }
    Ok(())
}

fn get_cmake_property(interp: &mut Interp, args: &[String]) -> CmdResult {
    if args.len() < 2 {
        return err(WRONG_COUNT);
    }
    let value = match args[1].as_str() {
        "VARIABLES" => interp.definition_names().join(";"),
        "MACROS" => prop::macros(interp),
        "COMPONENTS" => interp
            .ext
            .misc
            .get("install-components")
            .cloned()
            .unwrap_or_default(),
        name => prop::get(interp, Object::Global, name).unwrap_or_else(|| "NOTFOUND".into()),
    };
    interp.set_def(&args[0], value);
    Ok(())
}

fn get_directory_property(interp: &mut Interp, args: &[String]) -> CmdResult {
    if args.len() < 2 {
        return err(WRONG_COUNT);
    }
    let mut index = interp.mf_index();
    let mut rest = &args[1..];
    if rest[0] == "DIRECTORY" {
        let dir = rest
            .get(1)
            .ok_or("DIRECTORY argument provided without subsequent arguments")?;
        index = prop::find_directory(interp, dir).ok_or("DIRECTORY argument provided but requested directory not found. This could be because the directory argument was invalid or, it is valid but has not been processed yet.")?;
        rest = &rest[2..];
    }
    let property = rest.first().ok_or(WRONG_COUNT)?;
    let value = if property == "DEFINITION" {
        let name = rest
            .get(1)
            .ok_or("A request for a variable definition was made without providing the name of the variable to get.")?;
        if index == interp.mf_index() {
            interp.read_definition(name).unwrap_or_default()
        } else {
            prop::directory_definition(interp, index, name).unwrap_or_default()
        }
    } else {
        if property.is_empty() {
            return err("given empty string for the property name to get");
        }
        prop::get(interp, Object::Directory(index), property).unwrap_or_default()
    };
    interp.set_def(&args[0], value);
    Ok(())
}

fn set_directory_properties(interp: &mut Interp, args: &[String]) -> CmdResult {
    if args.is_empty() {
        return err(WRONG_COUNT);
    }
    if args.len() % 2 != 1 {
        return err("Wrong number of arguments");
    }
    let directory = Object::Directory(interp.mf_index());
    for pair in args[1..].chunks_exact(2) {
        match pair[0].as_str() {
            "VARIABLES" => {
                return err("Variables and cache variables should be set using SET command");
            }
            "MACROS" => return err("Commands and macros cannot be set using SET_CMAKE_PROPERTIES"),
            name => prop::set(interp, directory, name, Some(&pair[1])),
        }
    }
    Ok(())
}

fn get_source_file_property(interp: &mut Interp, args: &[String]) -> CmdResult {
    if interp.mode == crate::interp::Mode::Script {
        return err("command is not scriptable");
    }
    if !matches!(args.len(), 3 | 5) {
        return err(WRONG_COUNT);
    }
    let mut source = SourceDirectories::default();
    let mut property = &args[2];
    if args.len() == 5 {
        match property.as_str() {
            "DIRECTORY" => {
                source.directory_option = true;
                source.directories.push(&args[3]);
                property = &args[4];
            }
            "TARGET_DIRECTORY" => {
                source.target_option = true;
                source.targets.push(&args[3]);
                property = &args[4];
            }
            _ => {}
        }
    }
    let index = source.resolve(interp)?[0];
    let path = source.source_path(interp, &args[1]);
    // cmGetSourceFilePropertyCommand: GENERATED under CMP0163 NEW
    // (policy of the source's directory) consults only the global
    // generated registry -- the caller's binary then source directory --
    // and never looks up or creates a source file.
    let cmp0163 = interp.policy_status_at(index, "CMP0163");
    if property == "GENERATED" && cmp0163 != PolicyStatus::Old && cmp0163 != PolicyStatus::Warn {
        let generated = [interp.current_binary_dir(), interp.current_source_dir()]
            .iter()
            .any(|dir| {
                interp
                    .ext
                    .generated_sources
                    .contains(&collapse_full_path(&args[1], dir))
            });
        interp.set_def(&args[0], if generated { "1" } else { "0" });
        return Ok(());
    }
    let value = if !property.is_empty()
        && (prop::has_source(interp, index, &path) || property == "LOCATION")
    {
        prop::get(interp, Object::Source(index, &path), property)
    } else {
        None
    };
    interp.set_def(&args[0], value.unwrap_or_else(|| "NOTFOUND".into()));
    Ok(())
}

fn set_source_files_properties(interp: &mut Interp, args: &[String]) -> CmdResult {
    if interp.mode == crate::interp::Mode::Script {
        return err("command is not scriptable");
    }
    if args.len() < 2 {
        return err(WRONG_COUNT);
    }
    let option = |name: &str| {
        matches!(
            name,
            "ABSTRACT"
                | "GENERATED"
                | "WRAP_EXCLUDE"
                | "COMPILE_FLAGS"
                | "OBJECT_DEPENDS"
                | "PROPERTIES"
                | "DIRECTORY"
                | "TARGET_DIRECTORY"
        )
    };
    let split = args
        .iter()
        .position(|arg| option(arg))
        .unwrap_or(args.len());
    let mut source = SourceDirectories::default();
    let mut tail = &args[split..];
    let mut field = Field::None;
    while let Some((arg, rest)) = tail.split_first() {
        match arg.as_str() {
            "DIRECTORY" => {
                source.directory_option = true;
                field = Field::Directory;
            }
            "TARGET_DIRECTORY" => {
                source.target_option = true;
                field = Field::TargetDirectory;
            }
            name if option(name) => break,
            name if field == Field::Directory => source.directories.push(name),
            name if field == Field::TargetDirectory => source.targets.push(name),
            name => return Err(invalid_arg(name)),
        }
        tail = rest;
    }
    let dirs = source.resolve(interp)?;
    let mut pairs = Vec::new();
    while let Some((arg, rest)) = tail.split_first() {
        tail = rest;
        let name = arg.as_str();
        match name {
            "ABSTRACT" | "GENERATED" | "WRAP_EXCLUDE" => pairs.push((name, "1")),
            "COMPILE_FLAGS" | "OBJECT_DEPENDS" => {
                let extra = if name == "COMPILE_FLAGS" {
                    "flags"
                } else {
                    "dependencies"
                };
                let (value, rest) = tail
                    .split_first()
                    .ok_or_else(|| format!("{WRONG_COUNT} {name} with no {extra}"))?;
                pairs.push((name, value));
                tail = rest;
            }
            "PROPERTIES" => {
                if !tail.len().is_multiple_of(2) {
                    return err(format!("{WRONG_COUNT}."));
                }
                pairs.extend(
                    tail.chunks_exact(2)
                        .map(|pair| (pair[0].as_str(), pair[1].as_str())),
                );
                break;
            }
            _ => return err(NO_PROPERTIES),
        }
    }
    for path in &args[..split] {
        let path = source.source_path(interp, path);
        for &index in &dirs {
            prop::ensure_source(interp, index, &path);
            for &(name, value) in &pairs {
                change(
                    interp,
                    Object::Source(index, &path),
                    name,
                    Change::Set(Some(value)),
                );
            }
        }
    }
    Ok(())
}

fn get_target_property(interp: &mut Interp, args: &[String]) -> CmdResult {
    if interp.mode == crate::interp::Mode::Script {
        return err("command is not scriptable");
    }
    let [output, target, property] = args else {
        return err(WRONG_COUNT);
    };
    if find_target_to_use(interp, target).is_none() {
        interp.issue(
            MessageType::FatalError,
            &format!("get_target_property() called with non-existent target \"{target}\"."),
        );
        return Err(CmdErr::Reported);
    }
    let value =
        target_value(interp, target, property).unwrap_or_else(|| format!("{output}-NOTFOUND"));
    interp.set_def(output, value);
    Ok(())
}

fn get_test_property(interp: &mut Interp, args: &[String]) -> CmdResult {
    if interp.mode == crate::interp::Mode::Script {
        return err("command is not scriptable");
    }
    if !matches!(args.len(), 3 | 5) {
        return err(WRONG_COUNT);
    }
    let scoped = args.len() == 5 && args[2] == "DIRECTORY";
    let directory = test_directory(interp, scoped.then(|| args[3].as_str()))?;
    let value = prop::get(interp, Object::Test(directory, &args[0]), &args[1])
        .unwrap_or_else(|| "NOTFOUND".into());
    interp.set_def(&args[if scoped { 4 } else { 2 }], value);
    Ok(())
}

fn property_pairs(args: &[String]) -> Result<(&[String], &[String]), CmdErr> {
    let split = args
        .iter()
        .position(|arg| arg == "PROPERTIES")
        .ok_or(NO_PROPERTIES)?;
    let properties = &args[split + 1..];
    if properties.is_empty() {
        return err(NO_PROPERTIES);
    }
    if !properties.len().is_multiple_of(2) {
        return err(format!("{WRONG_COUNT}."));
    }
    Ok((&args[..split], properties))
}

fn set_target_properties(interp: &mut Interp, args: &[String]) -> CmdResult {
    if interp.mode == crate::interp::Mode::Script {
        return err("command is not scriptable");
    }
    if args.len() < 2 {
        return err(WRONG_COUNT);
    }
    let (targets, pairs) = property_pairs(args)?;
    for name in targets {
        if interp.ext.aliases.contains_key(name) {
            return err("can not be used on an ALIAS target.");
        }
        if find_target_to_use(interp, name).is_none() {
            return err(format!("Can not find target to add properties to: {name}"));
        }
        for pair in pairs.chunks_exact(2) {
            change(
                interp,
                Object::Target(name),
                &pair[0],
                Change::Set(Some(&pair[1])),
            );
        }
    }
    Ok(())
}

fn set_tests_properties(interp: &mut Interp, args: &[String]) -> CmdResult {
    if interp.mode == crate::interp::Mode::Script {
        return err("command is not scriptable");
    }
    if args.is_empty() {
        return err(WRONG_COUNT);
    }
    let (args, pairs) = property_pairs(args)?;
    let mut names = Vec::new();
    let mut directory = None;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if arg == "DIRECTORY" {
            let Some(dir) = args.next().filter(|arg| *arg != "DIRECTORY") else {
                interp.issue(
                    MessageType::FatalError,
                    "Error after keyword \"DIRECTORY\":\n  missing required value\n",
                );
                return Err(CmdErr::Reported);
            };
            directory = (!dir.is_empty()).then_some(dir.as_str());
        } else {
            names.push(arg);
        }
    }
    let directory = test_directory(interp, directory)?;
    for name in names {
        if !interp
            .ext
            .tests
            .iter()
            .any(|test| test.makefile == directory && &test.name == name)
        {
            return err(format!("Can not find test to add properties to: {name}"));
        }
        for pair in pairs.chunks_exact(2) {
            if !pair[0].is_empty() {
                prop::set(
                    interp,
                    Object::Test(directory, name),
                    &pair[0],
                    Some(&pair[1]),
                );
            }
        }
    }
    Ok(())
}
