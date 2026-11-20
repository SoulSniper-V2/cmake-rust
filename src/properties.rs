//! Shared property storage and computed lookups for commands and generation.

use std::collections::BTreeMap;

use crate::cache::CacheType;
use crate::interp::{Interp, Mode};
use crate::props::Props;
use crate::state::{Target, find_target_to_use};
use crate::util::{collapse_full_path, is_on};

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Scope {
    Global,
    Directory,
    Target,
    Source,
    Test,
    Cache,
    Install,
    Variable,
    CachedVariable,
}

impl Scope {
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "GLOBAL" => Self::Global,
            "DIRECTORY" => Self::Directory,
            "TARGET" => Self::Target,
            "SOURCE" => Self::Source,
            "TEST" => Self::Test,
            "CACHE" => Self::Cache,
            "INSTALL" => Self::Install,
            "VARIABLE" => Self::Variable,
            "CACHED_VARIABLE" => Self::CachedVariable,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Object<'a> {
    Global,
    Directory(usize),
    Target(&'a str),
    Source(usize, &'a str),
    Test(usize, &'a str),
    Cache(&'a str),
    Install(&'a str),
}

#[derive(Debug, Default)]
pub struct Definition {
    pub brief: String,
    pub full: String,
    pub inherited: bool,
    pub initialize_from_variable: String,
}

#[derive(Debug)]
pub struct Definitions(BTreeMap<Scope, BTreeMap<String, Definition>>);

impl Default for Definitions {
    fn default() -> Self {
        let mut definitions = Self(BTreeMap::new());
        for scope in [Scope::Target, Scope::Directory] {
            for name in [
                "RULE_LAUNCH_COMPILE",
                "RULE_LAUNCH_LINK",
                "RULE_LAUNCH_CUSTOM",
            ] {
                definitions.define(
                    scope,
                    name,
                    Definition {
                        inherited: true,
                        ..Definition::default()
                    },
                );
            }
        }
        definitions
    }
}

impl Definitions {
    pub fn define(&mut self, scope: Scope, name: &str, definition: Definition) {
        self.0
            .entry(scope)
            .or_default()
            .entry(name.to_owned())
            .or_insert(definition);
    }

    pub fn get(&self, scope: Scope, name: &str) -> Option<&Definition> {
        self.0.get(&scope)?.get(name)
    }

    fn inherited(&self, scope: Scope, name: &str) -> bool {
        self.get(scope, name)
            .is_some_and(|definition| definition.inherited)
    }
}

/// Apply INITIALIZE_FROM_VARIABLE during target creation.
pub fn initialize_target(interp: &Interp, target: &mut Target) {
    if let Some(definitions) = interp.ext.property_definitions.0.get(&Scope::Target) {
        for (name, definition) in definitions {
            if !definition.initialize_from_variable.is_empty()
                && let Some(value) = interp.get_def(&definition.initialize_from_variable)
            {
                target.props.set(name, value.to_owned());
            }
        }
    }
}

pub fn find_directory(interp: &Interp, dir: &str) -> Option<usize> {
    let path = collapse_full_path(dir, &interp.current_source_dir());
    if interp.mode == Mode::Script {
        return None;
    }
    interp
        .makefiles
        .iter()
        .position(|mf| mf.source_dir == path || mf.binary_dir == path)
}

pub fn directory_definition(interp: &Interp, index: usize, name: &str) -> Option<String> {
    if index == interp.mf_index() {
        return interp.get_def(name).map(str::to_owned);
    }
    interp
        .ext
        .directory_definitions
        .get(&index)
        .and_then(|defs| defs.get(name))
        .cloned()
        .or_else(|| interp.cache.value(name).map(str::to_owned))
}

pub fn listfile_stack(interp: &Interp) -> String {
    let root = (interp.mode == Mode::Script).then_some("");
    root.into_iter()
        .chain(interp.ext.execution_files.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join(";")
}

pub fn macros(interp: &Interp) -> String {
    interp.mf().props.get("MACROS").unwrap_or_default().into()
}

pub fn get(interp: &Interp, object: Object<'_>, name: &str) -> Option<String> {
    let stored = match object {
        Object::Global => {
            match name {
                "CACHE_VARIABLES" => {
                    return Some(
                        interp
                            .cache
                            .entries
                            .keys()
                            .cloned()
                            .collect::<Vec<_>>()
                            .join(";"),
                    );
                }
                "COMMANDS" => {
                    let mut commands: Vec<_> = interp.commands.keys().map(String::as_str).collect();
                    commands.sort_unstable();
                    return Some(commands.join(";"));
                }
                "CMAKE_ROLE" => {
                    return Some(
                        if interp.mode == Mode::Script {
                            "SCRIPT"
                        } else {
                            "PROJECT"
                        }
                        .into(),
                    );
                }
                "ENABLED_LANGUAGES" => {
                    let mut languages = interp.ext.languages.clone();
                    languages.sort_unstable();
                    return Some(languages.join(";"));
                }
                "IN_TRY_COMPILE" => {
                    return Some(if interp.in_try_compile { "1" } else { "0" }.into());
                }
                "GENERATOR_IS_MULTI_CONFIG" => {
                    return Some(
                        if interp.get_def("CMAKE_GENERATOR") == Some("Ninja Multi-Config") {
                            "1"
                        } else {
                            "0"
                        }
                        .into(),
                    );
                }
                _ => {}
            }
            known_features(name).or_else(|| interp.global_props.get(name))
        }
        Object::Directory(index) => {
            return directory(
                interp,
                index,
                name,
                interp
                    .ext
                    .property_definitions
                    .inherited(Scope::Directory, name),
            );
        }
        Object::Target(target_name) => {
            let target = find_target_to_use(interp, target_name)?;
            let mf = &interp.makefiles[target.makefile];
            match name {
                "ALIASED_TARGET" => return interp.ext.aliases.get(target_name).cloned(),
                "ALIAS_GLOBAL" => {
                    return interp
                        .ext
                        .aliases
                        .contains_key(target_name)
                        .then(|| bool_text(!target.imported || target.imported_global).into());
                }
                "NAME" => return Some(target.name.clone()),
                "TYPE" => return Some(target.kind.type_name().into()),
                "SOURCE_DIR" => return Some(mf.source_dir.clone()),
                "BINARY_DIR" => return Some(mf.binary_dir.clone()),
                "IMPORTED" => return Some(bool_text(target.imported).into()),
                "IMPORTED_GLOBAL" => return Some(bool_text(target.imported_global).into()),
                _ => {}
            }
            if let Some(value) = target.props.get(name) {
                return Some(value.into());
            }
            return inherited(interp, Scope::Target, target.makefile, name);
        }
        Object::Source(index, path) => {
            let mf = &interp.makefiles[index];
            let location = SourceLocation::new(interp, index, path);
            let source = find_source(interp, index, &location);
            let location = source.map_or(&location, |source| &source.location);
            let value = source.and_then(|source| source.props.get(name));
            match name {
                "GENERATED" => {
                    let generated = [mf.binary_dir.as_str(), mf.source_dir.as_str()]
                        .into_iter()
                        .map(|directory| collapse_full_path(path, directory))
                        .any(|path| interp.ext.generated_sources.contains(&path));
                    return Some(
                        if generated || value.is_some_and(is_on) {
                            "1"
                        } else {
                            "0"
                        }
                        .into(),
                    );
                }
                "LANGUAGE" => {
                    let computed = language(&location.name);
                    return Some(
                        value
                            .unwrap_or_else(|| {
                                if interp.ext.languages.iter().any(|name| name == computed) {
                                    computed
                                } else {
                                    ""
                                }
                            })
                            .into(),
                    );
                }
                "LOCATION" => return source_full_path(interp, index, location),
                _ => {}
            }
            return value
                .map(str::to_owned)
                .or_else(|| inherited(interp, Scope::Source, index, name));
        }
        Object::Test(index, test_name) => {
            let test = interp
                .ext
                .tests
                .iter()
                .find(|test| test.makefile == index && test.name == test_name)?;
            return test
                .props
                .get(name)
                .map(str::to_owned)
                .or_else(|| inherited(interp, Scope::Test, index, name));
        }
        Object::Install(file) => interp.ext.install_properties.get(file)?.get(name),
        Object::Cache(key) => {
            let entry = interp.cache.get(key)?;
            match name {
                "VALUE" => Some(entry.value.as_str()),
                "TYPE" => Some(entry.kind.as_str()),
                _ => entry.properties.get(name).map(String::as_str),
            }
        }
    };
    stored.map(str::to_owned)
}

fn bool_text(value: bool) -> &'static str {
    if value { "TRUE" } else { "FALSE" }
}

fn inherited(interp: &Interp, scope: Scope, index: usize, name: &str) -> Option<String> {
    if interp.ext.property_definitions.inherited(scope, name) {
        return directory(interp, index, name, true);
    }
    None
}

fn directory(interp: &Interp, index: usize, name: &str, chain: bool) -> Option<String> {
    let mf = &interp.makefiles[index];
    let target_names = |imported: bool| {
        let order = if mf.targets.is_empty() {
            &interp.ext.target_order
        } else {
            &mf.targets
        };
        order
            .iter()
            .filter(|name| {
                interp.ext.targets.get(*name).is_some_and(|target| {
                    target.makefile == index
                        && target.imported == imported
                        && target.alias_of.is_none()
                })
            })
            .cloned()
            .collect::<Vec<_>>()
            .join(";")
    };
    let value = match name {
        "PARENT_DIRECTORY" => mf
            .parent
            .map(|parent| interp.makefiles[parent].source_dir.clone())
            .unwrap_or_default(),
        "SOURCE_DIR" => mf.source_dir.clone(),
        "BINARY_DIR" => mf.binary_dir.clone(),
        "SUBDIRECTORIES" => mf
            .children
            .iter()
            .map(|&child| interp.makefiles[child].source_dir.as_str())
            .collect::<Vec<_>>()
            .join(";"),
        "BUILDSYSTEM_TARGETS" => target_names(false),
        "IMPORTED_TARGETS" => target_names(true),
        "TESTS" => {
            let mut names: Vec<_> = interp
                .ext
                .tests
                .iter()
                .filter(|test| test.makefile == index)
                .map(|test| test.name.as_str())
                .collect();
            names.sort_unstable();
            names.join(";")
        }
        "LISTFILE_STACK" if index == interp.mf_index() => listfile_stack(interp),
        "LISTFILE_STACK" => format!("{}/CMakeLists.txt", mf.source_dir),
        "CACHE_VARIABLES" => return get(interp, Object::Global, name),
        "INCLUDE_REGULAR_EXPRESSION" if interp.mode == Mode::Project => {
            mf.props.get(name).unwrap_or("^.*$").into()
        }
        "VARIABLES" if index == interp.mf_index() => interp.definition_names().join(";"),
        "VARIABLES" => {
            let mut names: Vec<_> = interp
                .ext
                .directory_definitions
                .get(&index)
                .into_iter()
                .flat_map(|defs| defs.keys())
                .chain(interp.cache.entries.keys())
                .cloned()
                .collect();
            names.sort_unstable();
            names.join(";")
        }
        "INCLUDE_DIRECTORIES"
        | "COMPILE_OPTIONS"
        | "COMPILE_DEFINITIONS"
        | "LINK_OPTIONS"
        | "LINK_DIRECTORIES" => mf.props.get(name).unwrap_or_default().into(),
        _ => {
            if let Some(value) = mf.props.get(name) {
                return Some(value.into());
            }
            if chain {
                return match mf.parent {
                    Some(parent) => directory(interp, parent, name, true),
                    None => get(interp, Object::Global, name),
                };
            }
            return None;
        }
    };
    Some(value)
}

const SOURCE_EXTENSIONS: &[&str] = &[
    "c", "C", "c++", "cc", "cpp", "cxx", "cu", "mpp", "m", "M", "mm", "ixx", "cppm", "ccm", "cxxm",
    "c++m", "h", "hh", "h++", "hm", "hpp", "hxx", "in", "txx", "f", "F", "for", "f77", "f90",
    "f95", "f03", "hip", "ispc",
];

#[derive(Debug)]
struct SourceLocation {
    directory: String,
    name: String,
    ambiguous_directory: bool,
    ambiguous_extension: bool,
}

impl SourceLocation {
    fn new(interp: &Interp, index: usize, path: &str) -> Self {
        let mf = &interp.makefiles[index];
        let (dir, name) = path.rsplit_once('/').unwrap_or(("", path));
        let mut ambiguous_directory = !path.starts_with('/');
        let strict_extensions = interp.policy_status("CMP0115") == crate::policy::PolicyStatus::New
            && interp.policy_status("CMP0187") == crate::policy::PolicyStatus::New;
        let mut ambiguous_extension = !strict_extensions;
        let mut directory = if ambiguous_directory {
            dir.to_owned()
        } else {
            collapse_full_path(if dir.is_empty() { "/" } else { dir }, &mf.source_dir)
        };
        let extension = name.rsplit_once('.').map_or("", |(_, ext)| ext);
        if SOURCE_EXTENSIONS.contains(&extension)
            || interp
                .ext
                .languages
                .iter()
                .any(|enabled| enabled == language(name))
        {
            ambiguous_extension = false;
        } else if std::path::Path::new(&collapse_full_path(path, &mf.source_dir)).is_file() {
            ambiguous_extension = false;
            if ambiguous_directory {
                directory = collapse_full_path(dir, &mf.source_dir);
                ambiguous_directory = false;
            }
        }
        Self {
            directory,
            name: name.into(),
            ambiguous_directory,
            ambiguous_extension,
        }
    }

    fn matches(&self, other: &Self, source: &str, binary: &str) -> bool {
        let matching_name = |known: &str, ambiguous: &str| {
            known == ambiguous
                || known
                    .strip_prefix(ambiguous)
                    .and_then(|suffix| suffix.strip_prefix('.'))
                    .is_some_and(|ext| SOURCE_EXTENSIONS.contains(&ext))
        };
        let name_matches = match (self.ambiguous_extension, other.ambiguous_extension) {
            (true, false) => matching_name(&other.name, &self.name),
            (false, true) => matching_name(&self.name, &other.name),
            _ => self.name == other.name,
        };
        let directory_matches = |absolute: &str, relative: &str| {
            collapse_full_path(relative, source) == absolute
                || collapse_full_path(relative, binary) == absolute
        };
        name_matches
            && match (self.ambiguous_directory, other.ambiguous_directory) {
                (true, false) => directory_matches(&other.directory, &self.directory),
                (false, true) => directory_matches(&self.directory, &other.directory),
                _ => self.directory == other.directory,
            }
    }

    fn update(&mut self, other: Self) {
        if self.ambiguous_directory && !other.ambiguous_directory {
            self.directory = other.directory;
            self.ambiguous_directory = false;
        }
        if self.ambiguous_extension && !other.ambiguous_extension {
            self.name = other.name;
            self.ambiguous_extension = false;
        }
    }

    fn path(&self, base: &str) -> String {
        let directory = collapse_full_path(&self.directory, base);
        collapse_full_path(&self.name, &directory)
    }
}

#[derive(Debug)]
pub struct SourceFile {
    location: SourceLocation,
    props: Props,
}

fn find_source<'a>(
    interp: &'a Interp,
    index: usize,
    location: &SourceLocation,
) -> Option<&'a SourceFile> {
    let mf = &interp.makefiles[index];
    interp
        .ext
        .source_properties
        .get(&index)?
        .iter()
        .find(|source| {
            source
                .location
                .matches(location, &mf.source_dir, &mf.binary_dir)
        })
}

fn source_full_path(interp: &Interp, index: usize, location: &SourceLocation) -> Option<String> {
    let mf = &interp.makefiles[index];
    for dir in [&mf.source_dir, &mf.binary_dir] {
        let path = location.path(dir);
        let exists = |path: &str| {
            std::path::Path::new(path).is_file() || interp.ext.generated_sources.contains(path)
        };
        if exists(&path) {
            return Some(path);
        }
        if interp.policy_status("CMP0115") != crate::policy::PolicyStatus::New {
            for ext in SOURCE_EXTENSIONS {
                let candidate = format!("{path}.{ext}");
                if exists(&candidate) {
                    return Some(candidate);
                }
            }
        }
    }
    None
}

fn language(path: &str) -> &'static str {
    match path.rsplit_once('.').map(|(_, ext)| ext) {
        Some("c") => "C",
        Some("C" | "cc" | "cp" | "cxx" | "cpp" | "CPP" | "c++" | "CXX" | "ixx" | "cppm") => "CXX",
        Some("m") => "OBJC",
        Some("mm" | "M") => "OBJCXX",
        Some("f" | "F" | "f90" | "F90" | "f95" | "F95" | "f03" | "f08" | "for" | "f77") => {
            "Fortran"
        }
        Some("cu") => "CUDA",
        Some("hip") => "HIP",
        Some("swift") => "Swift",
        Some("ispc") => "ISPC",
        _ => "",
    }
}

fn source_mut<'a>(interp: &'a mut Interp, index: usize, path: &str) -> &'a mut SourceFile {
    let location = SourceLocation::new(interp, index, path);
    let mf = &interp.makefiles[index];
    let sources = interp.ext.source_properties.entry(index).or_default();
    if let Some(position) = sources.iter().position(|source| {
        source
            .location
            .matches(&location, &mf.source_dir, &mf.binary_dir)
    }) {
        let source = &mut sources[position];
        source.location.update(location);
        source
    } else {
        sources.push(SourceFile {
            location,
            props: Props::default(),
        });
        sources.last_mut().expect("created source")
    }
}

pub fn ensure_source(interp: &mut Interp, index: usize, path: &str) {
    source_mut(interp, index, path);
}

pub fn has_source(interp: &Interp, index: usize, path: &str) -> bool {
    find_source(interp, index, &SourceLocation::new(interp, index, path)).is_some()
}

pub fn mark_generated(interp: &mut Interp, index: usize, path: &str) {
    let binary = interp.makefiles[index].binary_dir.clone();
    let location = &mut source_mut(interp, index, path).location;
    let full_path = location.path(&binary);
    location.directory = crate::util::filename_path(&full_path);
    location.ambiguous_directory = false;
    interp.ext.generated_sources.insert(full_path);
}

fn storage<'a>(interp: &'a mut Interp, object: Object<'_>) -> Option<&'a mut Props> {
    Some(match object {
        Object::Global => &mut interp.global_props,
        Object::Directory(index) => &mut interp.makefiles[index].props,
        Object::Target(name) => &mut interp.ext.targets.get_mut(name)?.props,
        Object::Source(index, path) => &mut source_mut(interp, index, path).props,
        Object::Test(index, name) => {
            &mut interp
                .ext
                .tests
                .iter_mut()
                .find(|test| test.makefile == index && test.name == name)?
                .props
        }
        Object::Install(path) => interp
            .ext
            .install_properties
            .entry(path.to_owned())
            .or_default(),
        Object::Cache(_) => return None,
    })
}

/// The target properties represented as entries with origin backtraces.
fn target_entries(name: &str) -> bool {
    matches!(
        name,
        "INCLUDE_DIRECTORIES"
            | "COMPILE_OPTIONS"
            | "COMPILE_FEATURES"
            | "COMPILE_DEFINITIONS"
            | "PRECOMPILE_HEADERS"
            | "SOURCES"
            | "LINK_OPTIONS"
            | "LINK_DIRECTORIES"
            | "LINK_LIBRARIES"
            | "INTERFACE_LINK_LIBRARIES"
            | "INTERFACE_LINK_LIBRARIES_DIRECT"
            | "INTERFACE_LINK_LIBRARIES_DIRECT_EXCLUDE"
            | "IMPORTED_CXX_MODULES_INCLUDE_DIRECTORIES"
            | "IMPORTED_CXX_MODULES_COMPILE_DEFINITIONS"
            | "IMPORTED_CXX_MODULES_COMPILE_FEATURES"
            | "IMPORTED_CXX_MODULES_COMPILE_OPTIONS"
            | "IMPORTED_CXX_MODULES_LINK_LIBRARIES"
    )
}

pub fn set(interp: &mut Interp, object: Object<'_>, name: &str, value: Option<&str>) {
    if let Object::Target(target) = object
        && target_entries(name)
        && let Some(target) = interp.ext.targets.get_mut(target)
    {
        if let Some(value) = value {
            target.provenance.insert(
                name.to_owned(),
                vec![(value.to_owned(), interp.backtrace.clone())],
            );
        } else {
            target.provenance.remove(name);
        }
    }
    if let Object::Cache(key) = object {
        if let Some(entry) = interp.cache.entries.get_mut(key) {
            match (name, value) {
                ("VALUE", value) => entry.value = value.unwrap_or_default().into(),
                ("TYPE", Some(value)) => entry.kind = CacheType::parse(value),
                ("TYPE", None) => entry.kind = CacheType::String,
                (_, Some(value)) => {
                    entry.properties.insert(name.into(), value.into());
                }
                (_, None) => {
                    entry.properties.remove(name);
                }
            }
        }
    } else if let Some(props) = storage(interp, object) {
        match value {
            Some(value) => props.set(name, value.to_owned()),
            None => props.remove(name),
        }
    }
}

pub fn append(interp: &mut Interp, object: Object<'_>, name: &str, value: &str, as_string: bool) {
    if let Object::Target(target) = object
        && target_entries(name)
    {
        if (name == "SOURCES" || !value.is_empty())
            && let Some(target) = interp.ext.targets.get_mut(target)
        {
            let combined = target
                .props
                .get(name)
                .map_or_else(|| value.to_owned(), |old| format!("{old};{value}"));
            target.props.set(name, combined);
            target
                .provenance
                .entry(name.to_owned())
                .or_default()
                .push((value.to_owned(), interp.backtrace.clone()));
        }
    } else if matches!(object, Object::Install(_)) {
        if let Some(props) = storage(interp, object) {
            let combined = props
                .get(name)
                .map_or_else(|| value.to_owned(), |old| format!("{old};{value}"));
            props.set(name, combined);
        }
    } else if matches!(object, Object::Cache(_)) {
        if name == "TYPE" {
            set(
                interp,
                object,
                name,
                Some(if value.is_empty() { "STRING" } else { value }),
            );
        } else if !value.is_empty() {
            let mut result = get(interp, object, name).unwrap_or_default();
            if !as_string && !result.is_empty() {
                result.push(';');
            }
            result.push_str(value);
            set(interp, object, name, Some(&result));
        }
    } else if let Some(props) = storage(interp, object) {
        props.append(name, value, as_string);
    }
}

fn known_features(name: &str) -> Option<&'static str> {
    Some(match name {
        "CMAKE_C_KNOWN_FEATURES" => concat!(
            "c_std_90;",
            "c_std_99;",
            "c_std_11;",
            "c_std_17;",
            "c_std_23;",
            "c_function_prototypes;",
            "c_restrict;",
            "c_variadic_macros;",
            "c_static_assert",
        ),
        "CMAKE_C90_KNOWN_FEATURES" => "c_function_prototypes",
        "CMAKE_C99_KNOWN_FEATURES" => concat!("c_restrict;", "c_variadic_macros",),
        "CMAKE_C11_KNOWN_FEATURES" => "c_static_assert",
        "CMAKE_CXX_KNOWN_FEATURES" => concat!(
            "cxx_std_98;",
            "cxx_std_11;",
            "cxx_std_14;",
            "cxx_std_17;",
            "cxx_std_20;",
            "cxx_std_23;",
            "cxx_std_26;",
            "cxx_template_template_parameters;",
            "cxx_alias_templates;",
            "cxx_alignas;",
            "cxx_alignof;",
            "cxx_attributes;",
            "cxx_auto_type;",
            "cxx_constexpr;",
            "cxx_decltype;",
            "cxx_decltype_incomplete_return_types;",
            "cxx_default_function_template_args;",
            "cxx_defaulted_functions;",
            "cxx_defaulted_move_initializers;",
            "cxx_delegating_constructors;",
            "cxx_deleted_functions;",
            "cxx_enum_forward_declarations;",
            "cxx_explicit_conversions;",
            "cxx_extended_friend_declarations;",
            "cxx_extern_templates;",
            "cxx_final;",
            "cxx_func_identifier;",
            "cxx_generalized_initializers;",
            "cxx_inheriting_constructors;",
            "cxx_inline_namespaces;",
            "cxx_lambdas;",
            "cxx_local_type_template_args;",
            "cxx_long_long_type;",
            "cxx_noexcept;",
            "cxx_nonstatic_member_init;",
            "cxx_nullptr;",
            "cxx_override;",
            "cxx_range_for;",
            "cxx_raw_string_literals;",
            "cxx_reference_qualified_functions;",
            "cxx_right_angle_brackets;",
            "cxx_rvalue_references;",
            "cxx_sizeof_member;",
            "cxx_static_assert;",
            "cxx_strong_enums;",
            "cxx_thread_local;",
            "cxx_trailing_return_types;",
            "cxx_unicode_literals;",
            "cxx_uniform_initialization;",
            "cxx_unrestricted_unions;",
            "cxx_user_literals;",
            "cxx_variadic_macros;",
            "cxx_variadic_templates;",
            "cxx_aggregate_default_initializers;",
            "cxx_attribute_deprecated;",
            "cxx_binary_literals;",
            "cxx_contextual_conversions;",
            "cxx_decltype_auto;",
            "cxx_digit_separators;",
            "cxx_generic_lambdas;",
            "cxx_lambda_init_captures;",
            "cxx_relaxed_constexpr;",
            "cxx_return_type_deduction;",
            "cxx_variable_templates",
        ),
        "CMAKE_CXX98_KNOWN_FEATURES" => "cxx_template_template_parameters",
        "CMAKE_CXX11_KNOWN_FEATURES" => concat!(
            "cxx_alias_templates;",
            "cxx_alignas;",
            "cxx_alignof;",
            "cxx_attributes;",
            "cxx_auto_type;",
            "cxx_constexpr;",
            "cxx_decltype;",
            "cxx_decltype_incomplete_return_types;",
            "cxx_default_function_template_args;",
            "cxx_defaulted_functions;",
            "cxx_defaulted_move_initializers;",
            "cxx_delegating_constructors;",
            "cxx_deleted_functions;",
            "cxx_enum_forward_declarations;",
            "cxx_explicit_conversions;",
            "cxx_extended_friend_declarations;",
            "cxx_extern_templates;",
            "cxx_final;",
            "cxx_func_identifier;",
            "cxx_generalized_initializers;",
            "cxx_inheriting_constructors;",
            "cxx_inline_namespaces;",
            "cxx_lambdas;",
            "cxx_local_type_template_args;",
            "cxx_long_long_type;",
            "cxx_noexcept;",
            "cxx_nonstatic_member_init;",
            "cxx_nullptr;",
            "cxx_override;",
            "cxx_range_for;",
            "cxx_raw_string_literals;",
            "cxx_reference_qualified_functions;",
            "cxx_right_angle_brackets;",
            "cxx_rvalue_references;",
            "cxx_sizeof_member;",
            "cxx_static_assert;",
            "cxx_strong_enums;",
            "cxx_thread_local;",
            "cxx_trailing_return_types;",
            "cxx_unicode_literals;",
            "cxx_uniform_initialization;",
            "cxx_unrestricted_unions;",
            "cxx_user_literals;",
            "cxx_variadic_macros;",
            "cxx_variadic_templates",
        ),
        "CMAKE_CXX14_KNOWN_FEATURES" => concat!(
            "cxx_aggregate_default_initializers;",
            "cxx_attribute_deprecated;",
            "cxx_binary_literals;",
            "cxx_contextual_conversions;",
            "cxx_decltype_auto;",
            "cxx_digit_separators;",
            "cxx_generic_lambdas;",
            "cxx_lambda_init_captures;",
            "cxx_relaxed_constexpr;",
            "cxx_return_type_deduction;",
            "cxx_variable_templates",
        ),
        "CMAKE_CUDA_KNOWN_FEATURES" => concat!(
            "cuda_std_03;",
            "cuda_std_11;",
            "cuda_std_14;",
            "cuda_std_17;",
            "cuda_std_20;",
            "cuda_std_23;",
            "cuda_std_26",
        ),
        "CMAKE_HIP_KNOWN_FEATURES" => concat!(
            "hip_std_98;",
            "hip_std_11;",
            "hip_std_14;",
            "hip_std_17;",
            "hip_std_20;",
            "hip_std_23;",
            "hip_std_26",
        ),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::TargetKind;

    #[test]
    fn inherited_target_properties_initialize_and_track_local_entries() {
        let mut interp = Interp::new(Mode::Project, "/src", "/build", "");
        assert!(interp.run_string(
            r#"
define_property(TARGET PROPERTY example_flag INHERITED INITIALIZE_FROM_VARIABLE default_example_flag)
set(default_example_flag initialized)
set_property(DIRECTORY PROPERTY example_flag inherited)
"#,
            "/src/probe.cmake",
        ));

        let mut target = Target {
            name: "example".into(),
            kind: TargetKind::StaticLibrary,
            makefile: 0,
            imported: false,
            imported_global: false,
            alias_of: None,
            props: Props::default(),
            provenance: BTreeMap::new(),
            backtrace: Vec::new(),
        };
        initialize_target(&interp, &mut target);
        interp.ext.targets.insert(target.name.clone(), target);

        let target = Object::Target("example");
        assert_eq!(
            get(&interp, target, "example_flag").as_deref(),
            Some("initialized")
        );
        set(&mut interp, target, "example_flag", None);
        assert_eq!(
            get(&interp, target, "example_flag").as_deref(),
            Some("inherited")
        );

        set(&mut interp, target, "COMPILE_OPTIONS", Some(""));
        append(&mut interp, target, "COMPILE_OPTIONS", "-Wall", true);
        assert_eq!(
            get(&interp, target, "COMPILE_OPTIONS").as_deref(),
            Some(";-Wall")
        );
        assert_eq!(
            interp.ext.targets["example"].provenance["COMPILE_OPTIONS"].len(),
            2
        );
    }
}
