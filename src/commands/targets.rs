//! Configure-time target, custom-command, and test commands.

use std::collections::BTreeSet;

use crate::cache::{CacheType, convert_to_unix_slashes};
use crate::interp::{Builtin, CmdResult, Interp, Mode, err};
use crate::messages::MessageType;
use crate::policy::PolicyStatus;
use crate::state::{
    CustomCommand, CustomCommandKind, FileSet, GeneratorAction, Scope, TargetKind, Test,
    find_alias, find_global_target_key, find_target_key, find_target_to_use,
};
use crate::util::{collapse_full_path, expand_list, is_off_opt, is_on, strtol};

use super::directory::input_path;
use super::target::{
    PropertyWrite, add_sources, check_target_name, create_alias, create_target,
    enforce_unique_name, format_signature_traces, full_path, genex_start, has_genex,
    mark_generated, policy_warning, report, write_target_property,
};

pub fn register(interp: &mut Interp) {
    let commands: &[(&str, Builtin)] = &[
        ("add_executable", add_executable),
        ("add_library", add_library),
        ("add_custom_target", add_custom_target),
        ("add_custom_command", add_custom_command),
        ("add_dependencies", add_dependencies),
        ("target_link_libraries", target_link_libraries),
        ("target_include_directories", target_include_directories),
        ("target_compile_definitions", target_compile_definitions),
        ("target_compile_options", target_compile_options),
        ("target_compile_features", target_compile_features),
        ("target_link_options", target_link_options),
        ("target_link_directories", target_link_directories),
        ("target_sources", target_sources),
        ("target_precompile_headers", target_precompile_headers),
        ("enable_testing", enable_testing),
        ("add_test", add_test),
    ];
    if interp.mode == Mode::Project {
        for &(name, command) in commands {
            interp.register(name, command);
        }
    }
}

/// `GetProjectCommandsInScriptMode` (cmCommands.cxx): in script mode every
/// project command is replaced by one that fails, before its arguments are
/// expanded, with "command is not scriptable".  Called after all families
/// have registered so it overrides them.
pub fn register_script_mode_unexpected(interp: &mut Interp) {
    if interp.mode == Mode::Project {
        return;
    }
    for name in [
        "add_compile_options",
        "add_custom_command",
        "add_custom_target",
        "add_definitions",
        "add_dependencies",
        "add_executable",
        "add_library",
        "add_subdirectory",
        "add_test",
        "aux_source_directory",
        "build_command",
        "cmake_file_api",
        "create_test_sourcelist",
        "define_property",
        "enable_language",
        "enable_testing",
        "export",
        "fltk_wrap_ui",
        "get_source_file_property",
        "get_target_property",
        "get_test_property",
        "include_directories",
        "include_external_msproject",
        "include_regular_expression",
        "install",
        "link_directories",
        "link_libraries",
        "project",
        "qt_wrap_cpp",
        "qt_wrap_ui",
        "remove_definitions",
        "set_source_files_properties",
        "set_target_properties",
        "set_tests_properties",
        "source_group",
        "target_compile_definitions",
        "target_compile_features",
        "target_compile_options",
        "target_include_directories",
        "target_link_libraries",
        "target_sources",
        "try_compile",
        "try_run",
        // deprecated commands
        "export_library_dependencies",
        "load_command",
        "output_required_files",
        "subdir_depends",
        "utility_source",
        "variable_requires",
    ] {
        interp.register_unexpected(name, "command is not scriptable");
    }
}

fn bad_arity() -> CmdResult {
    err("called with incorrect number of arguments")
}

#[derive(Default)]
struct Modifiers {
    exclude: bool,
    imported: bool,
    global: bool,
    alias: bool,
}

impl Modifiers {
    fn read(&mut self, arg: &str) -> bool {
        match arg {
            "EXCLUDE_FROM_ALL" => self.exclude = true,
            "IMPORTED" => self.imported = true,
            "GLOBAL" if self.imported => self.global = true,
            "ALIAS" => self.alias = true,
            _ => return false,
        }
        true
    }
}

fn add_executable(interp: &mut Interp, args: &[String]) -> CmdResult {
    let Some((name, mut remaining)) = args.split_first() else {
        return bad_arity();
    };
    let mut modifiers = Modifiers::default();
    let (mut win32, mut mac_bundle) = (false, false);
    while let Some((arg, rest)) = remaining.split_first() {
        match arg.as_str() {
            "WIN32" => win32 = true,
            "MACOSX_BUNDLE" => mac_bundle = true,
            arg if modifiers.read(arg) => {}
            _ => break,
        }
        remaining = rest;
    }
    check_target_name(interp, name, modifiers.imported || modifiers.alias)?;
    if modifiers.imported {
        for (set, name) in [
            (win32, "WIN32"),
            (mac_bundle, "MACOSX_BUNDLE"),
            (modifiers.exclude, "EXCLUDE_FROM_ALL"),
        ] {
            if set {
                return err(format!("may not be given {name} for an IMPORTED target."));
            }
        }
    }
    if modifiers.alias {
        return add_alias(interp, name, &modifiers, args, true);
    }
    let key = add_build_target(interp, name, TargetKind::Executable, &modifiers, remaining)?;
    let target = interp.ext.targets.get_mut(&key).unwrap();
    for (set, prop) in [(win32, "WIN32_EXECUTABLE"), (mac_bundle, "MACOSX_BUNDLE")] {
        if set {
            target.props.set(prop, "ON".to_owned());
        }
    }
    Ok(())
}

fn add_library(interp: &mut Interp, args: &[String]) -> CmdResult {
    use TargetKind::*;
    let Some((name, mut remaining)) = args.split_first() else {
        return bad_arity();
    };
    let mut kind = if is_off_opt(interp.get_def("BUILD_SHARED_LIBS")) {
        StaticLibrary
    } else {
        SharedLibrary
    };
    let mut specified_type = false;
    let mut modifiers = Modifiers::default();
    while let Some((arg, rest)) = remaining.split_first() {
        let new_type = match arg.as_str() {
            "STATIC" => Some(StaticLibrary),
            "SHARED" => Some(SharedLibrary),
            "MODULE" => Some(ModuleLibrary),
            "OBJECT" => Some(ObjectLibrary),
            "UNKNOWN" => Some(UnknownLibrary),
            "INTERFACE" => Some(InterfaceLibrary),
            _ => None,
        };
        if new_type == Some(InterfaceLibrary) {
            if specified_type {
                return err("INTERFACE library specified with conflicting/multiple types.");
            }
            if modifiers.alias {
                return err("INTERFACE library specified with conflicting ALIAS type.");
            }
        } else if kind == InterfaceLibrary {
            if new_type.is_some() || arg == "ALIAS" {
                return err(format!(
                    "INTERFACE library specified with conflicting {arg} type."
                ));
            }
            if arg == "GLOBAL" && !modifiers.imported {
                return err("GLOBAL option may only be used with IMPORTED libraries.");
            }
        }
        if let Some(new_type) = new_type {
            kind = new_type;
            specified_type = true;
        } else if !modifiers.read(arg) {
            break;
        }
        remaining = rest;
    }
    check_target_name(interp, name, modifiers.imported || modifiers.alias)?;
    if modifiers.alias {
        return add_alias(interp, name, &modifiers, args, false);
    }
    if modifiers.imported && modifiers.exclude {
        return err("excludeFromAll with IMPORTED target makes no sense.");
    }
    if matches!(kind, SharedLibrary | ModuleLibrary)
        && interp
            .global_props
            .get("TARGET_SUPPORTS_SHARED_LIBS")
            .is_some_and(|value| !is_on(value))
    {
        let option = if kind == SharedLibrary {
            "SHARED"
        } else {
            "MODULE"
        };
        let message = format!(
            "ADD_LIBRARY called with {option} option but the target platform does not support dynamic linking."
        );
        match interp.policy_status("CMP0164") {
            PolicyStatus::New => {
                interp.set_fatal();
                return report(interp, &message);
            }
            PolicyStatus::Warn => interp.issue(
                MessageType::AuthorWarning,
                &format!("{message} Building a STATIC library instead. This may lead to problems."),
            ),
            PolicyStatus::Old => {}
        }
        kind = StaticLibrary;
    }
    if modifiers.imported && !specified_type {
        return err("called with IMPORTED argument but no library type.");
    }
    if !modifiers.imported && kind == UnknownLibrary {
        return report(
            interp,
            "The UNKNOWN library type may be used only for IMPORTED libraries.",
        );
    }
    add_build_target(interp, name, kind, &modifiers, remaining)?;
    Ok(())
}

fn add_alias(
    interp: &mut Interp,
    name: &str,
    modifiers: &Modifiers,
    args: &[String],
    executable: bool,
) -> CmdResult {
    if modifiers.exclude {
        return err("EXCLUDE_FROM_ALL with ALIAS makes no sense.");
    }
    if modifiers.imported {
        return err("IMPORTED with ALIAS is not allowed.");
    }
    if args.len() != 3 {
        return err("ALIAS requires exactly one target argument.");
    }
    if !executable
        && interp.policy_status("CMP0107") == PolicyStatus::New
        && find_target_key(interp, name).is_some()
    {
        return err(format!(
            "cannot create ALIAS target \"{name}\" because another target with the same name already exists."
        ));
    }
    create_alias(interp, name, &args[2], executable)
}

fn add_build_target(
    interp: &mut Interp,
    name: &str,
    kind: TargetKind,
    modifiers: &Modifiers,
    sources: &[String],
) -> Result<String, crate::interp::CmdErr> {
    if modifiers.imported {
        if find_target_key(interp, name).is_some() {
            return err(format!(
                "cannot create imported target \"{name}\" because another target with the same name already exists."
            ));
        }
        let global = modifiers.global
            || interp
                .ext
                .find_package
                .get(&interp.mf_index())
                .is_some_and(|state| state.global);
        return Ok(create_target(interp, name, kind, true, global));
    }
    enforce_unique_name(interp, name, false)?;
    let key = create_target(interp, name, kind, false, false);
    if kind != TargetKind::Executable {
        interp.cache.remove(&format!("{name}_LIB_DEPENDS"));
    }
    if modifiers.exclude {
        interp
            .ext
            .targets
            .get_mut(&key)
            .unwrap()
            .props
            .set("EXCLUDE_FROM_ALL", "TRUE".to_owned());
    }
    add_sources(interp, &key, sources);
    if kind != TargetKind::InterfaceLibrary {
        let libs = expand_list(
            interp.mf().props.get("LINK_LIBRARIES").unwrap_or_default(),
            false,
        );
        let mut libs = libs.iter();
        while let Some(library) = libs.next() {
            let (link_type, lib) = match library.as_str() {
                "debug" | "optimized" => (library.as_str(), libs.next().map_or("", String::as_str)),
                _ => ("general", library.as_str()),
            };
            let (direct, interface) = link_expressions(interp, lib, link_type);
            record_link_dependency(interp, &key, lib, link_type);
            let target = interp.ext.targets.get_mut(&key).unwrap();
            target.insert_entry("LINK_LIBRARIES", &direct, &interp.backtrace, false);
            target.insert_entry(
                "INTERFACE_LINK_LIBRARIES",
                &interface,
                &interp.backtrace,
                false,
            );
        }
    }
    Ok(key)
}

fn add_dependencies(interp: &mut Interp, args: &[String]) -> CmdResult {
    let Some((name, dependencies)) = args.split_first() else {
        return bad_arity();
    };
    if find_alias(interp, name).is_some() {
        interp.issue(
            MessageType::FatalError,
            &format!("Cannot add target-level dependencies to alias target \"{name}\".\n"),
        );
    }
    let Some(key) = find_target_key(interp, name) else {
        return report(
            interp,
            &format!(
                "Cannot add target-level dependencies to non-existent target \"{name}\".\nThe add_dependencies works for top-level logical targets created by the add_executable, add_library, or add_custom_target commands.  If you want to add file-level dependencies see the DEPENDS option of the add_custom_target and add_custom_command commands."
            ),
        );
    };
    let target = interp.ext.targets.get_mut(&key).unwrap();
    for dependency in dependencies {
        target.add_dependency(dependency, &interp.backtrace);
    }
    Ok(())
}

fn enable_testing(interp: &mut Interp, _: &[String]) -> CmdResult {
    interp.set_def("CMAKE_TESTING_ENABLED", "1");
    interp.ext.testing_enabled.insert(interp.mf_index());
    Ok(())
}

fn add_test(interp: &mut Interp, args: &[String]) -> CmdResult {
    #[derive(Clone, Copy)]
    enum Parsing {
        Name,
        Command,
        Configs,
        Directory,
        Policy,
        Idle,
    }
    let dir = interp.mf_index();
    let mut policy = interp.policy_status("CMP0178");
    let parse_policy = |value: &str| match value {
        "NEW" => PolicyStatus::New,
        "OLD" => PolicyStatus::Old,
        _ => PolicyStatus::Warn,
    };
    let mut configurations = Vec::new();
    let mut working_directory = "";
    let mut command_expand_lists = false;
    let old_style = args.first().is_none_or(|arg| arg != "NAME");
    let (name, command) = if old_style {
        if args.len() < 2 {
            return bad_arity();
        }
        let end = args[2..]
            .iter()
            .position(|arg| arg == "__CMP0178")
            .map_or(args.len(), |index| index + 2);
        if end != args.len() {
            let Some(value) = args.get(end + 1) else {
                return err("__CMP0178 keyword missing value");
            };
            policy = parse_policy(value);
        }
        (&args[0][..], args[1..end].to_vec())
    } else {
        let mut name = "";
        let mut command = Vec::new();
        let mut doing = Parsing::Name;
        for arg in &args[1..] {
            doing = match arg.as_str() {
                "COMMAND" if command.is_empty() => Parsing::Command,
                "COMMAND" => return err(" may be given at most one COMMAND."),
                "CONFIGURATIONS" if configurations.is_empty() => Parsing::Configs,
                "CONFIGURATIONS" => return err(" may be given at most one set of CONFIGURATIONS."),
                "WORKING_DIRECTORY" if working_directory.is_empty() => Parsing::Directory,
                "WORKING_DIRECTORY" => return err(" may be given at most one WORKING_DIRECTORY."),
                "__CMP0178" => Parsing::Policy,
                "COMMAND_EXPAND_LISTS" if !command_expand_lists => {
                    command_expand_lists = true;
                    Parsing::Idle
                }
                "COMMAND_EXPAND_LISTS" => {
                    return err(" may be given at most one COMMAND_EXPAND_LISTS.");
                }
                _ => match doing {
                    Parsing::Name => {
                        name = arg;
                        Parsing::Idle
                    }
                    Parsing::Command => {
                        command.push(arg.clone());
                        doing
                    }
                    Parsing::Configs => {
                        configurations.push(arg.clone());
                        doing
                    }
                    Parsing::Directory => {
                        working_directory = arg;
                        Parsing::Idle
                    }
                    Parsing::Policy => {
                        policy = parse_policy(arg);
                        Parsing::Idle
                    }
                    Parsing::Idle => return err(format!(" given unknown argument:\n  {arg}\n")),
                },
            };
        }
        if name.is_empty() {
            return err(" must be given non-empty NAME.");
        }
        if command.is_empty() {
            return err(" must be given non-empty COMMAND.");
        }
        (name, command)
    };
    if let Some(test) = interp
        .ext
        .tests
        .iter_mut()
        .find(|test| test.makefile == dir && test.name == name)
    {
        if old_style && test.old_style {
            test.command = command;
            return Ok(());
        }
        let kind = if old_style { "name" } else { "NAME" };
        return err(format!(
            " given test {kind} \"{name}\" which already exists in this directory."
        ));
    }
    let mut test = Test {
        name: name.to_owned(),
        makefile: dir,
        command,
        props: Default::default(),
        old_style,
        configurations,
        command_expand_lists,
        empty_argument_policy: policy,
        emulator_policy: interp.policy_status("CMP0158"),
        backtrace: interp.backtrace.clone(),
    };
    if !working_directory.is_empty() {
        test.props
            .set("WORKING_DIRECTORY", working_directory.to_owned());
    }
    interp.ext.tests.push(test);
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LinkScope {
    Plain,
    LegacyInterface,
    LegacyPublic,
    LegacyPrivate,
    Public,
    Private,
    Interface,
}

impl LinkScope {
    fn keyword(self) -> bool {
        !matches!(self, Self::Plain | Self::LegacyInterface)
    }

    fn direct(self) -> bool {
        !matches!(self, Self::LegacyInterface | Self::Interface)
    }
}

fn link_expressions(interp: &Interp, name: &str, link_type: &str) -> (String, String) {
    if link_type == "general" {
        return (name.to_owned(), name.to_owned());
    }
    let mut configs = expand_list(
        interp
            .global_props
            .get("DEBUG_CONFIGURATIONS")
            .unwrap_or_default(),
        false,
    )
    .into_iter()
    .map(|config| format!("$<CONFIG:{}>", config.to_ascii_uppercase()))
    .collect::<Vec<_>>();
    let mut condition = match configs.len() {
        0 => "$<CONFIG:DEBUG>".to_owned(),
        1 => configs.pop().unwrap(),
        _ => format!("$<OR:{}>", configs.join(",")),
    };
    if link_type == "optimized" {
        condition = format!("$<NOT:{condition}>");
    }
    let interface = format!("$<{condition}:{name}>");
    let direct = if find_target_to_use(interp, name).is_some_and(|target| !target.imported) {
        format!("$<{condition}:$<TARGET_NAME:{name}>>")
    } else {
        interface.clone()
    };
    (direct, interface)
}

fn record_link_dependency(interp: &mut Interp, key: &str, library: &str, link_type: &str) {
    let target = &interp.ext.targets[key];
    if !matches!(
        target.kind,
        TargetKind::StaticLibrary | TargetKind::SharedLibrary | TargetKind::ModuleLibrary
    ) || target
        .policies
        .get(crate::policy::index_of("CMP0073").unwrap())
        == Some(PolicyStatus::New)
        || target.name == library
        || has_genex(library)
        || find_target_to_use(interp, library).is_some_and(|target| {
            matches!(
                target.kind,
                TargetKind::InterfaceLibrary | TargetKind::ObjectLibrary
            )
        })
    {
        return;
    }
    let entry = format!("{}_LIB_DEPENDS", target.name);
    let dependencies = format!(
        "{}{link_type};{library};",
        interp.get_def(&entry).unwrap_or_default()
    );
    interp.add_cache_def(
        &entry,
        Some(&dependencies),
        Some("Dependencies for the target"),
        CacheType::Static,
        false,
    );
}

/// Directory marker understood by the generator when looking up a link item.
#[allow(dead_code, reason = "used when resolving generated link lines")]
pub fn link_lookup_directory(entry: &str) -> Option<usize> {
    usize::from_str_radix(entry.strip_prefix("::@(0x")?.strip_suffix(')')?, 16).ok()
}

struct LinkCall {
    key: String,
    remote: bool,
    wrapped: BTreeSet<&'static str>,
}

impl LinkCall {
    fn append(&mut self, interp: &mut Interp, prop: &'static str, value: &str) {
        let dir = interp.mf_index();
        if self.remote
            && interp.policy_status_at(dir, "CMP0079") == PolicyStatus::New
            && self.wrapped.insert(prop)
        {
            interp.ext.targets.get_mut(&self.key).unwrap().insert_entry(
                prop,
                &format!("::@(0x{dir:x})"),
                &interp.backtrace,
                false,
            );
        }
        if !value.is_empty() {
            interp.ext.targets.get_mut(&self.key).unwrap().insert_entry(
                prop,
                value,
                &interp.backtrace,
                false,
            );
        }
    }

    fn library(
        &mut self,
        interp: &mut Interp,
        scope: LinkScope,
        lib: &str,
        link_type: &str,
    ) -> CmdResult {
        let target = &interp.ext.targets[&self.key];
        let name = target.name.clone();
        if scope != LinkScope::Interface {
            if target.kind == TargetKind::InterfaceLibrary {
                return report(
                    interp,
                    "INTERFACE library can only be used with the INTERFACE keyword of target_link_libraries",
                );
            }
            if target.imported {
                return report(
                    interp,
                    "IMPORTED library can only be used with the INTERFACE keyword of target_link_libraries",
                );
            }
        }
        let keyword = scope.keyword();
        // cmTarget::PushTLLCommandTrace: record this call first (even when
        // it conflicts), then compare with the previous signature.
        let conflict = target
            .link_signatures
            .last()
            .is_some_and(|(previous, _)| *previous != keyword);
        let kind = target.kind;
        if let Some(frame) = interp.backtrace.last() {
            let target = interp.ext.targets.get_mut(&self.key).unwrap();
            if target.link_signatures.last().is_none_or(|(_, previous)| {
                previous.file != frame.file || previous.line != frame.line
            }) {
                target.link_signatures.push((keyword, frame.clone()));
            }
        }
        if conflict {
            let target = &interp.ext.targets[&self.key];
            let existing = if keyword { "plain" } else { "keyword" };
            let traces = format_signature_traces(interp, target, !keyword);
            return report(
                interp,
                &format!(
                    "The {existing} signature for target_link_libraries has already been used with the target \"{name}\".  All uses of target_link_libraries with a target must be either all-keyword or all-plain.\n{traces}"
                ),
            );
        }
        let (direct, mut interface) = link_expressions(interp, lib, link_type);
        let policy = interp.policy_status("CMP0079");
        if scope.direct() {
            if self.remote && policy != PolicyStatus::New {
                return report(
                    interp,
                    &format!(
                        "Attempt to add link library \"{lib}\" to target \"{name}\" which is not built in this directory.\nThis is allowed only when policy CMP0079 is set to NEW."
                    ),
                );
            }
            let right = interp
                .ext
                .aliases
                .get(lib)
                .cloned()
                .or_else(|| find_global_target_key(interp, lib));
            if let Some(right) = right.map(|key| &interp.ext.targets[&key]) {
                use TargetKind::*;
                let linkable = matches!(
                    right.kind,
                    StaticLibrary
                        | SharedLibrary
                        | UnknownLibrary
                        | ObjectLibrary
                        | InterfaceLibrary
                ) || right.kind == Executable
                    && right.props.get("ENABLE_EXPORTS").is_some_and(is_on);
                if !linkable {
                    interp.issue(MessageType::FatalError, &format!(
                        "Target \"{lib}\" of type {} may not be linked into another target. One may link only to INTERFACE, OBJECT, STATIC or SHARED libraries, or to executables with the ENABLE_EXPORTS property set.",
                        right.kind.type_name()
                    ));
                }
            }
            self.append(interp, "LINK_LIBRARIES", &direct);
            record_link_dependency(interp, &self.key, lib, link_type);
        }
        if self.remote && policy == PolicyStatus::Warn {
            interp.issue(MessageType::AuthorWarning, &format!(
                "{}\nTarget\n  {name}\nis not created in this directory.  For compatibility with older versions of CMake, link library\n  {lib}\nwill be looked up in the directory in which the target was created rather than in this calling directory.",
                policy_warning("CMP0079")
            ));
        }
        if matches!(scope, LinkScope::Private | LinkScope::LegacyPrivate) {
            if !matches!(kind, TargetKind::StaticLibrary | TargetKind::ObjectLibrary) {
                return Ok(());
            }
            if super::target::valid_target_name(lib) || has_genex(lib) {
                interface = format!("$<LINK_ONLY:{interface}>");
            }
        }
        self.append(interp, "INTERFACE_LINK_LIBRARIES", &interface);
        Ok(())
    }

    fn parse(&mut self, interp: &mut Interp, args: &[String]) -> CmdResult {
        let mut scope = LinkScope::Plain;
        let mut specifier = None;
        let mut partial = Vec::new();
        let mut nesting = 0usize;
        for (index, arg) in args.iter().enumerate() {
            let new_scope = match arg.as_str() {
                "LINK_INTERFACE_LIBRARIES" => Some(LinkScope::LegacyInterface),
                "LINK_PUBLIC" => Some(LinkScope::LegacyPublic),
                "LINK_PRIVATE" => Some(LinkScope::LegacyPrivate),
                "PUBLIC" => Some(LinkScope::Public),
                "PRIVATE" => Some(LinkScope::Private),
                "INTERFACE" => Some(LinkScope::Interface),
                _ => None,
            };
            let new_specifier = matches!(arg.as_str(), "debug" | "optimized" | "general");
            if (new_scope.is_some() || new_specifier) && !partial.is_empty() {
                self.library(interp, scope, &partial.join(";"), "general")?;
                partial.clear();
                nesting = 0;
            }
            if let Some(new_scope) = new_scope {
                let (allowed, description) = match new_scope {
                    LinkScope::LegacyInterface => (false, "LINK_INTERFACE_LIBRARIES"),
                    LinkScope::LegacyPrivate | LinkScope::LegacyPublic => (
                        matches!(scope, LinkScope::LegacyPrivate | LinkScope::LegacyPublic),
                        "LINK_PUBLIC or LINK_PRIVATE",
                    ),
                    _ => (
                        matches!(
                            scope,
                            LinkScope::Interface | LinkScope::Private | LinkScope::Public
                        ),
                        "INTERFACE, PUBLIC or PRIVATE",
                    ),
                };
                if index != 0 && !allowed {
                    return report(
                        interp,
                        &format!(
                            "The {description} option must appear as the second argument, just after the target name."
                        ),
                    );
                }
                scope = new_scope;
            } else if new_specifier {
                if let Some(previous) = specifier.replace(arg.as_str()) {
                    interp.issue(MessageType::AuthorWarning, &format!(
                        "Link library type specifier \"{previous}\" is followed by specifier \"{arg}\" instead of a library name.  The first specifier will be ignored."
                    ));
                }
            } else if let Some(specifier) = specifier.take() {
                self.library(interp, scope, arg, specifier)?;
            } else {
                let mut bytes = arg.as_bytes().iter().peekable();
                while let Some(byte) = bytes.next() {
                    if *byte == b'$' && bytes.next_if_eq(&&b'<').is_some() {
                        nesting += 1;
                    } else if *byte == b'>' {
                        nesting = nesting.saturating_sub(1);
                    }
                }
                partial.push(arg.as_str());
                if nesting == 0 {
                    self.library(interp, scope, &partial.join(";"), "general")?;
                    partial.clear();
                }
            }
        }
        if !partial.is_empty() {
            self.library(interp, scope, &partial.join(";"), "general")?;
        }
        if let Some(specifier) = specifier {
            interp.set_fatal();
            return report(
                interp,
                &format!("The \"{specifier}\" argument must be followed by a library."),
            );
        }
        Ok(())
    }
}

fn target_link_libraries(interp: &mut Interp, args: &[String]) -> CmdResult {
    let Some((name, libraries)) = args.split_first() else {
        return bad_arity();
    };
    if find_alias(interp, name).is_some() {
        return err("can not be used on an ALIAS target.");
    }
    let Some(key) = find_global_target_key(interp, name).or_else(|| {
        interp
            .ext
            .scoped_targets
            .get(&(interp.mf_index(), name.clone()))
            .filter(|key| interp.ext.targets[*key].makefile == interp.mf_index())
            .cloned()
    }) else {
        interp.set_fatal();
        return report(
            interp,
            &format!(
                "Cannot specify link libraries for target \"{name}\" which is not built by this project."
            ),
        );
    };
    let target = &interp.ext.targets[&key];
    if target.kind == TargetKind::Utility {
        return report(
            interp,
            &format!(
                "Utility target \"{name}\" must not be used as the target of a target_link_libraries call."
            ),
        );
    }
    let mut call = LinkCall {
        key,
        remote: target.makefile != interp.mf_index(),
        wrapped: BTreeSet::new(),
    };
    let result = call.parse(interp, libraries);
    let target = interp.ext.targets.get_mut(&call.key).unwrap();
    for prop in call.wrapped {
        target.insert_entry(prop, "::@", &interp.backtrace, false);
    }
    result
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Usage {
    Includes,
    Definitions,
    CompileOptions,
    CompileFeatures,
    LinkOptions,
    LinkDirectories,
    Sources,
    PrecompileHeaders,
}

impl Usage {
    fn property(self) -> &'static str {
        match self {
            Self::Includes => "INCLUDE_DIRECTORIES",
            Self::Definitions => "COMPILE_DEFINITIONS",
            Self::CompileOptions => "COMPILE_OPTIONS",
            Self::CompileFeatures => "COMPILE_FEATURES",
            Self::LinkOptions => "LINK_OPTIONS",
            Self::LinkDirectories => "LINK_DIRECTORIES",
            Self::Sources => "SOURCES",
            Self::PrecompileHeaders => "PRECOMPILE_HEADERS",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::Includes => "include directories",
            Self::Definitions => "compile definitions",
            Self::CompileOptions => "compile options",
            Self::CompileFeatures => "compile features",
            Self::LinkOptions => "link options",
            Self::LinkDirectories => "link directories",
            Self::Sources => "sources",
            Self::PrecompileHeaders => "precompile headers",
        }
    }

    fn accepts_before(self) -> bool {
        matches!(
            self,
            Self::Includes | Self::CompileOptions | Self::LinkOptions | Self::LinkDirectories
        )
    }
}

macro_rules! target_usage_commands {
    ($($name:ident => $kind:ident),+ $(,)?) => {
        $(fn $name(interp: &mut Interp, args: &[String]) -> CmdResult {
            target_usage(interp, args, Usage::$kind)
        })+
    };
}

target_usage_commands! {
    target_include_directories => Includes,
    target_compile_definitions => Definitions,
    target_compile_options => CompileOptions,
    target_compile_features => CompileFeatures,
    target_link_options => LinkOptions,
    target_link_directories => LinkDirectories,
    target_sources => Sources,
    target_precompile_headers => PrecompileHeaders,
}

fn target_usage(interp: &mut Interp, args: &[String], usage: Usage) -> CmdResult {
    if args.len() < 2 {
        return bad_arity();
    }
    let name = &args[0];
    if find_alias(interp, name).is_some() {
        return err("can not be used on an ALIAS target.");
    }
    let Some(key) = find_global_target_key(interp, name).or_else(|| find_target_key(interp, name))
    else {
        let description = usage.description();
        return report(
            interp,
            &format!(
                "Cannot specify {description} for target \"{name}\" which is not built by this project."
            ),
        );
    };
    let target = &interp.ext.targets[&key];
    let (kind, imported, owner) = (target.kind, target.imported, target.makefile);
    if kind == TargetKind::GlobalTarget || (kind == TargetKind::Utility && usage != Usage::Sources)
    {
        return err("called with non-compilable target type");
    }
    let mut args = &args[1..];
    let system = usage == Usage::Includes && args[0] == "SYSTEM";
    if system {
        if args.len() < 2 {
            return bad_arity();
        }
        args = &args[1..];
    }
    let before = usage.accepts_before() && args[0] == "BEFORE";
    if before || (usage == Usage::Includes && args[0] == "AFTER") {
        if args.len() == 1 && !system {
            return bad_arity();
        }
        args = &args[1..];
    }
    if usage == Usage::PrecompileHeaders && args.first().is_some_and(|arg| arg == "REUSE_FROM") {
        if args.len() != 2 {
            return bad_arity();
        }
        return write_target_property(
            interp,
            name,
            "PRECOMPILE_HEADERS_REUSE_FROM",
            Some(&args[1]),
            PropertyWrite::Set,
        );
    }
    let scope_of = |arg: &str| match arg {
        "PRIVATE" => Some(Scope::Private),
        "PUBLIC" => Some(Scope::Public),
        "INTERFACE" => Some(Scope::Interface),
        _ => None,
    };
    let base = interp.current_source_dir();
    while let Some((scope_arg, remaining)) = args.split_first() {
        let Some(scope) = scope_of(scope_arg) else {
            return err("called with invalid arguments");
        };
        let end = remaining
            .iter()
            .position(|arg| scope_of(arg).is_some())
            .unwrap_or(remaining.len());
        let content = &remaining[..end];
        args = &remaining[end..];
        if content.is_empty() {
            continue;
        }
        if kind == TargetKind::InterfaceLibrary && scope.direct() && usage != Usage::Sources {
            return err("may only set INTERFACE properties on INTERFACE targets");
        }
        if imported && scope.direct() {
            return err("may only set INTERFACE properties on IMPORTED targets");
        }
        if kind == TargetKind::Utility && scope.interface() {
            return err("may only set PRIVATE properties on custom targets");
        }
        if usage == Usage::Sources && content[0] == "FILE_SET" {
            for set in content[1..].split(|arg| arg == "FILE_SET") {
                add_file_set(interp, &key, scope, set)?;
            }
            continue;
        }
        for interface in [false, true] {
            if (interface && !scope.interface()) || (!interface && !scope.direct()) {
                continue;
            }
            if usage == Usage::CompileFeatures && !interface {
                for feature in content {
                    add_feature(interp, &key, feature)?;
                }
                continue;
            }
            let relativize_sources = usage == Usage::Sources
                && (interface || interp.makefiles[owner].source_dir != base);
            let convert_sources =
                relativize_sources && interp.policy_status("CMP0076") == PolicyStatus::New;
            if relativize_sources
                && interp.policy_status("CMP0076") == PolicyStatus::Warn
                && content
                    .iter()
                    .any(|arg| !full_path(arg) && !genex_start(arg))
            {
                let context = if interface {
                    "An interface source of target"
                } else {
                    "A private source from a directory other than that of target"
                };
                interp.issue(
                    MessageType::AuthorWarning,
                    &format!(
                        "{}\n{context} \"{name}\" has a relative path.",
                        policy_warning("CMP0076")
                    ),
                );
            }
            let values: Vec<_> = content
                .iter()
                .map(|arg| match usage {
                    Usage::Definitions => arg.strip_prefix("-D").unwrap_or(arg).to_owned(),
                    Usage::Includes => input_path(&base, arg),
                    Usage::LinkDirectories => input_path(&base, &convert_to_unix_slashes(arg)),
                    Usage::PrecompileHeaders if !arg.starts_with(['<', '"']) => {
                        input_path(&base, arg)
                    }
                    Usage::Sources if convert_sources => input_path(&base, arg),
                    _ => arg.clone(),
                })
                .collect();
            let prop = if interface {
                format!("INTERFACE_{}", usage.property())
            } else {
                usage.property().to_owned()
            };
            let prepend = before
                && (interface
                    || usage != Usage::CompileOptions
                    || interp.policy_status("CMP0101") == PolicyStatus::New);
            let target = interp.ext.targets.get_mut(&key).unwrap();
            if usage == Usage::PrecompileHeaders
                && !interface
                && target.props.contains("PRECOMPILE_HEADERS_REUSE_FROM")
            {
                interp.issue(MessageType::FatalError, &format!("PRECOMPILE_HEADERS_REUSE_FROM property is already set on target (\"{name}\")\n"));
                continue;
            }
            let value = values.join(";");
            let inserts_empty = !interface
                && matches!(
                    usage,
                    Usage::Includes
                        | Usage::CompileOptions
                        | Usage::LinkOptions
                        | Usage::LinkDirectories
                        | Usage::Sources
                );
            if !value.is_empty() || prepend || inserts_empty {
                target.insert_entry(&prop, &value, &interp.backtrace, prepend);
            }
            if system {
                if interface {
                    target.insert_entry(
                        "INTERFACE_SYSTEM_INCLUDE_DIRECTORIES",
                        &values.join(";"),
                        &interp.backtrace,
                        false,
                    );
                } else {
                    target.system_include_directories.extend(values);
                }
            }
        }
    }
    Ok(())
}

fn add_file_set(interp: &mut Interp, key: &str, scope: Scope, args: &[String]) -> CmdResult {
    let mut name = "";
    let mut kind = "";
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    let mut mode = "FILE_SET";
    for arg in args {
        match arg.as_str() {
            "TYPE" | "BASE_DIRS" | "FILES" => mode = arg,
            _ => match mode {
                "FILE_SET" => {
                    name = arg;
                    mode = "";
                }
                "TYPE" => {
                    kind = arg;
                    mode = "";
                }
                "BASE_DIRS" => dirs.push(arg),
                "FILES" => files.push(arg),
                _ => return err(format!("Unrecognized keyword: \"{arg}\"")),
            },
        }
    }
    if name.is_empty() {
        return err("FILE_SET must not be empty");
    }
    let target = &interp.ext.targets[key];
    if target.kind == TargetKind::Utility {
        return err("FILE_SETs may not be added to custom targets");
    }
    if interp.is_on("APPLE") && target.props.get("FRAMEWORK").is_some_and(is_on) {
        return err("FILE_SETs may not be added to FRAMEWORK targets");
    }
    let imported = target.imported;
    let target_kind = target.kind;
    let new_set = !target.file_sets.contains_key(name);
    let is_default = kind == name || (kind.is_empty() && name.as_bytes()[0].is_ascii_uppercase());
    let kind = if let Some(original) = target.file_sets.get(name) {
        if !kind.is_empty() && original.kind != kind {
            return err(format!(
                "Type \"{kind}\" for file set \"{name}\" does not match original type \"{}\"",
                original.kind
            ));
        }
        if original.scope != scope {
            return err(format!(
                "Scope {} for file set \"{name}\" does not match original scope {}",
                scope.name(),
                original.scope.name()
            ));
        }
        original.kind.clone()
    } else {
        let kind = if is_default { name } else { kind };
        let target = interp.ext.targets.get_mut(key).unwrap();
        // cmTarget::GetOrCreateFileSet registers the set before the command
        // validates its type, name and visibility.
        target.file_sets.insert(
            name.to_owned(),
            FileSet {
                kind: kind.to_owned(),
                scope,
                base_dirs: Vec::new(),
                files: Vec::new(),
            },
        );
        let property = match kind {
            "HEADERS" => Some("HEADER_SETS"),
            "CXX_MODULES" => Some("CXX_MODULE_SETS"),
            _ => None,
        };
        if let Some(property) = property {
            if scope.direct() {
                target.props.append(property, name, false);
            }
            if scope.interface() {
                target
                    .props
                    .append(&format!("INTERFACE_{property}"), name, false);
            }
        }
        let valid_name =
            name.as_bytes()[0].is_ascii_lowercase() || name.as_bytes()[0].is_ascii_digit();
        if !is_default
            && (!valid_name
                || !name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_'))
        {
            return err(
                "Non-default file set name must contain only letters, numbers, and underscores, and must not start with a capital letter or underscore",
            );
        }
        if kind.is_empty() {
            return err("Must specify a TYPE when creating file set");
        }
        if !matches!(kind, "HEADERS" | "CXX_MODULES") {
            return err("File set TYPE may only be \"HEADERS\" or \"CXX_MODULES\"");
        }
        if kind == "CXX_MODULES" && !imported {
            if scope.direct() && target_kind == TargetKind::InterfaceLibrary {
                return err(
                    "File set TYPE \"CXX_MODULES\" may not have \"PUBLIC\" or \"PRIVATE\" visibility on INTERFACE libraries.",
                );
            }
            if scope == Scope::Interface {
                return err("File set TYPE \"CXX_MODULES\" may not have \"INTERFACE\" visibility");
            }
        }
        kind.to_owned()
    };
    let base = interp.current_source_dir();
    let dirs: Vec<_> = if new_set && dirs.is_empty() {
        vec![base.clone()]
    } else {
        dirs.iter().map(|dir| input_path(&base, dir)).collect()
    };
    let files: Vec<_> = files.iter().map(|file| input_path(&base, file)).collect();
    let (dirs_prop, files_prop) = match kind.as_str() {
        "HEADERS" => ("HEADER_DIRS", "HEADER_SET"),
        "CXX_MODULES" => ("CXX_MODULE_DIRS", "CXX_MODULE_SET"),
        _ => return Ok(()),
    };
    let files_prop = if name == kind {
        files_prop.to_owned()
    } else {
        format!("{files_prop}_{name}")
    };
    let dirs_prop = if name == kind {
        dirs_prop.to_owned()
    } else {
        format!("{dirs_prop}_{name}")
    };
    let target = interp.ext.targets.get_mut(key).unwrap();
    let set = target.file_sets.get_mut(name).unwrap();
    let files = files.join(";");
    if !files.is_empty() {
        set.files.push((files.clone(), interp.backtrace.clone()));
        target.props.append(&files_prop, &files, false);
    } else {
        // Existing file sets expose empty file and directory lists as set properties.
        if !target.props.contains(&files_prop) {
            target.props.set(&files_prop, String::new());
        }
    }
    let directories = dirs.join(";");
    if !directories.is_empty() {
        set.base_dirs
            .push((directories.clone(), interp.backtrace.clone()));
        target.props.append(&dirs_prop, &directories, false);
    }
    if kind == "HEADERS" {
        for dir in expand_list(&directories, false) {
            let entry = format!("$<BUILD_INTERFACE:{dir}>");
            if scope.direct() {
                target.insert_entry("INCLUDE_DIRECTORIES", &entry, &interp.backtrace, false);
            }
            if scope.interface() {
                target.insert_entry(
                    "INTERFACE_INCLUDE_DIRECTORIES",
                    &entry,
                    &interp.backtrace,
                    false,
                );
            }
        }
    }
    Ok(())
}

const C_FEATURES: &[&str] = &[
    "c_function_prototypes",
    "c_restrict",
    "c_variadic_macros",
    "c_static_assert",
    "c_std_90",
    "c_std_99",
    "c_std_11",
    "c_std_17",
    "c_std_23",
];

const CXX_FEATURES: &[&str] = &[
    "cxx_template_template_parameters",
    "cxx_alias_templates",
    "cxx_alignas",
    "cxx_alignof",
    "cxx_attributes",
    "cxx_auto_type",
    "cxx_constexpr",
    "cxx_decltype",
    "cxx_decltype_incomplete_return_types",
    "cxx_default_function_template_args",
    "cxx_defaulted_functions",
    "cxx_defaulted_move_initializers",
    "cxx_delegating_constructors",
    "cxx_deleted_functions",
    "cxx_enum_forward_declarations",
    "cxx_explicit_conversions",
    "cxx_extended_friend_declarations",
    "cxx_extern_templates",
    "cxx_final",
    "cxx_func_identifier",
    "cxx_generalized_initializers",
    "cxx_inheriting_constructors",
    "cxx_inline_namespaces",
    "cxx_lambdas",
    "cxx_local_type_template_args",
    "cxx_long_long_type",
    "cxx_noexcept",
    "cxx_nonstatic_member_init",
    "cxx_nullptr",
    "cxx_override",
    "cxx_range_for",
    "cxx_raw_string_literals",
    "cxx_reference_qualified_functions",
    "cxx_right_angle_brackets",
    "cxx_rvalue_references",
    "cxx_sizeof_member",
    "cxx_static_assert",
    "cxx_strong_enums",
    "cxx_thread_local",
    "cxx_trailing_return_types",
    "cxx_unicode_literals",
    "cxx_uniform_initialization",
    "cxx_unrestricted_unions",
    "cxx_user_literals",
    "cxx_variadic_macros",
    "cxx_variadic_templates",
    "cxx_aggregate_default_initializers",
    "cxx_attribute_deprecated",
    "cxx_binary_literals",
    "cxx_contextual_conversions",
    "cxx_decltype_auto",
    "cxx_digit_separators",
    "cxx_generic_lambdas",
    "cxx_lambda_init_captures",
    "cxx_relaxed_constexpr",
    "cxx_return_type_deduction",
    "cxx_variable_templates",
    "cxx_std_98",
    "cxx_std_11",
    "cxx_std_14",
    "cxx_std_17",
    "cxx_std_20",
    "cxx_std_23",
    "cxx_std_26",
];

const CUDA_FEATURES: &[&str] = &[
    "cuda_std_03",
    "cuda_std_11",
    "cuda_std_14",
    "cuda_std_17",
    "cuda_std_20",
    "cuda_std_23",
    "cuda_std_26",
];

const HIP_FEATURES: &[&str] = &[
    "hip_std_98",
    "hip_std_11",
    "hip_std_14",
    "hip_std_17",
    "hip_std_20",
    "hip_std_23",
    "hip_std_26",
];

pub(crate) fn compile_feature_language(
    feature: &str,
) -> Option<(&'static str, &'static [&'static str])> {
    if C_FEATURES.contains(&feature) {
        Some(("C", &["90", "99", "11", "17", "23"]))
    } else if CXX_FEATURES.contains(&feature) {
        Some(("CXX", &["98", "11", "14", "17", "20", "23", "26"]))
    } else if CUDA_FEATURES.contains(&feature) {
        Some(("CUDA", &["03", "11", "14", "17", "20", "23", "26"]))
    } else if HIP_FEATURES.contains(&feature) {
        Some(("HIP", &["98", "11", "14", "17", "20", "23", "26"]))
    } else {
        None
    }
}

fn add_feature(interp: &mut Interp, key: &str, feature: &str) -> CmdResult {
    if has_genex(feature) {
        interp.ext.targets.get_mut(key).unwrap().insert_entry(
            "COMPILE_FEATURES",
            feature,
            &interp.backtrace,
            false,
        );
        return Ok(());
    }
    let target = &interp.ext.targets[key];
    let name = target.name.clone();
    let Some((lang, standards)) = compile_feature_language(feature) else {
        return err(format!(
            "specified unknown feature \"{feature}\" for target \"{name}\"."
        ));
    };
    if interp.langs.enabled.iter().any(|language| language == lang) {
        let features = interp
            .get_def(&format!("CMAKE_{lang}_COMPILE_FEATURES"))
            .unwrap_or_default();
        let id = interp
            .get_def(&format!("CMAKE_{lang}_COMPILER_ID"))
            .unwrap_or_default();
        let version = interp
            .get_def(&format!("CMAKE_{lang}_COMPILER_VERSION"))
            .unwrap_or_default();
        if features.is_empty() {
            return err(format!(
                "no known features for {lang} compiler\n\"{id}\"\nversion {version}."
            ));
        }
        if !expand_list(features, false)
            .iter()
            .any(|value| value == feature)
        {
            return err(format!(
                "The compiler feature \"{feature}\" is not known to {lang} compiler\n\"{id}\"\nversion {version}."
            ));
        }
    }
    interp.ext.targets.get_mut(key).unwrap().insert_entry(
        "COMPILE_FEATURES",
        feature,
        &interp.backtrace,
        false,
    );
    let prop = format!("{lang}_STANDARD");
    let current = interp.ext.targets[key].props.get(&prop).or_else(|| {
        interp
            .get_def(&format!("CMAKE_{lang}_STANDARD_DEFAULT"))
            .filter(|value| !value.is_empty())
    });
    let current = if let Some(value) = current {
        let (standard, consumed) = strtol(value, 10);
        let Some(level) = standards
            .iter()
            .position(|candidate| candidate.parse::<i64>().ok() == Some(standard))
            .filter(|_| consumed)
        else {
            return err(format!(
                "The {prop} property on target \"{name}\" contained an invalid value: \"{value}\"."
            ));
        };
        Some(level)
    } else {
        None
    };
    let needed = standards.iter().rposition(|standard| {
        let value = interp
            .get_def(&format!("CMAKE_{lang}{standard}_COMPILE_FEATURES"))
            .unwrap_or_default();
        expand_list(value, false)
            .iter()
            .any(|value| value == feature)
    });
    if let Some(level) = needed.filter(|needed| current.is_none_or(|current| current < *needed)) {
        interp.ext.targets.get_mut(key).unwrap().set_entry(
            &prop,
            Some(standards[level]),
            &interp.backtrace,
        );
    }
    Ok(())
}

fn custom_output(path: &str, binary: &str) -> String {
    let path = if full_path(path) || genex_start(path) {
        path.to_owned()
    } else {
        format!("{binary}/{path}")
    };
    let path = convert_to_unix_slashes(&path);
    if full_path(&path) {
        collapse_full_path(&path, binary)
    } else {
        path
    }
}

pub(super) fn validate_command_lines(interp: &mut Interp, command: &CustomCommand) -> CmdResult {
    if let Some(name) = command
        .command_lines
        .iter()
        .filter_map(|line| line.first())
        .find(|name| name.starts_with('"'))
    {
        return report(
            interp,
            &format!("COMMAND may not contain literal quotes:\n  {name}\n"),
        );
    }
    Ok(())
}

fn add_custom_target(interp: &mut Interp, args: &[String]) -> CmdResult {
    let Some((name, rest)) = args.split_first() else {
        return bad_arity();
    };
    if name.contains(['/', '\\']) {
        return err(format!(
            "called with invalid target name \"{name}\".  Target names may not contain a slash.  Use ADD_CUSTOM_COMMAND to generate files."
        ));
    }
    let all = rest.first().is_some_and(|arg| arg == "ALL");
    let mut command = CustomCommand {
        kind: CustomCommandKind::Utility,
        makefile: interp.mf_index(),
        backtrace: interp.backtrace.clone(),
        policies: interp.effective_policies(),
        ..Default::default()
    };
    let mut line = Vec::new();
    let mut mode = "COMMAND";
    let mut sources = Vec::new();
    let binary = interp.current_binary_dir();
    for arg in &rest[usize::from(all)..] {
        match arg.as_str() {
            "COMMAND" => {
                if !line.is_empty() {
                    command.command_lines.push(std::mem::take(&mut line));
                }
                mode = arg;
            }
            "DEPENDS" | "BYPRODUCTS" | "WORKING_DIRECTORY" | "COMMENT" | "JOB_POOL"
            | "JOB_SERVER_AWARE" | "SOURCES" => mode = arg,
            "VERBATIM" | "USES_TERMINAL" | "COMMAND_EXPAND_LISTS" => {
                match arg.as_str() {
                    "VERBATIM" => command.verbatim = true,
                    "USES_TERMINAL" => command.uses_terminal = true,
                    _ => command.command_expand_lists = true,
                }
                mode = "";
            }
            _ => match mode {
                "COMMAND" => line.push(arg.clone()),
                "DEPENDS" => command.depends.push(convert_to_unix_slashes(arg)),
                "BYPRODUCTS" => command.byproducts.push(custom_output(arg, &binary)),
                "WORKING_DIRECTORY" => command.working_directory = arg.clone(),
                "COMMENT" => command.comment = Some(arg.clone()),
                "JOB_POOL" => command.job_pool = arg.clone(),
                "JOB_SERVER_AWARE" => command.job_server_aware = is_on(arg),
                "SOURCES" => sources.push(arg.clone()),
                _ => return err("Wrong syntax. Unknown type of argument."),
            },
        }
    }
    if !line.is_empty() {
        command.command_lines.push(line);
    }
    check_target_name(interp, name, false)?;
    enforce_unique_name(interp, name, true)?;
    if command.command_lines.is_empty() {
        for (specified, option) in [
            (!command.byproducts.is_empty(), "BYPRODUCTS"),
            (command.uses_terminal, "USES_TERMINAL"),
            (command.command_expand_lists, "COMMAND_EXPAND_LISTS"),
        ] {
            if specified {
                return report(
                    interp,
                    &format!("{option} may not be specified without any COMMAND"),
                );
            }
        }
    }
    if command.uses_terminal && !command.job_pool.is_empty() {
        return err("JOB_POOL is shadowed by USES_TERMINAL.");
    }
    let key = create_target(interp, name, TargetKind::Utility, false, false);
    if !all {
        interp
            .ext
            .targets
            .get_mut(&key)
            .unwrap()
            .props
            .set("EXCLUDE_FROM_ALL", "TRUE".to_owned());
    }
    // cmMakefile::AddUtilityCommand: nothing to run without commands or
    // depends; otherwise validate and defer to generate time.
    let valid = if command.command_lines.is_empty() && command.depends.is_empty() {
        Ok(())
    } else {
        validate_command_lines(interp, &command)
    };
    if valid.is_ok() && !(command.command_lines.is_empty() && command.depends.is_empty()) {
        for byproduct in &command.byproducts {
            mark_generated(interp, command.makefile, byproduct);
        }
        interp.ext.generator_actions.push(GeneratorAction::Utility {
            target: key.clone(),
            command,
        });
    }
    add_sources(interp, &key, &sources);
    valid
}

const CUSTOM_TARGET_KEYWORDS: &[&str] = &[
    "ARGS",
    "BYPRODUCTS",
    "COMMAND",
    "COMMAND_EXPAND_LISTS",
    "COMMENT",
    "POST_BUILD",
    "PRE_BUILD",
    "PRE_LINK",
    "TARGET",
    "USES_TERMINAL",
    "VERBATIM",
    "WORKING_DIRECTORY",
];
const CUSTOM_OUTPUT_KEYWORDS: &[&str] = &[
    "APPEND",
    "ARGS",
    "BYPRODUCTS",
    "CODEGEN",
    "COMMAND",
    "COMMAND_EXPAND_LISTS",
    "COMMENT",
    "DEPENDS",
    "DEPENDS_EXPLICIT_ONLY",
    "DEPFILE",
    "IMPLICIT_DEPENDS",
    "JOB_POOL",
    "JOB_SERVER_AWARE",
    "MAIN_DEPENDENCY",
    "OUTPUT",
    "USES_TERMINAL",
    "VERBATIM",
    "WORKING_DIRECTORY",
];
const CUSTOM_APPEND_KEYWORDS: &[&str] = &[
    "APPEND",
    "ARGS",
    "COMMAND",
    "COMMENT",
    "DEPENDS",
    "IMPLICIT_DEPENDS",
    "MAIN_DEPENDENCY",
    "OUTPUT",
    "WORKING_DIRECTORY",
];

fn cmp0175(interp: &mut Interp, message: &str, warning_suffix: &str) -> CmdResult {
    match interp.policy_status("CMP0175") {
        PolicyStatus::New => report(interp, message),
        PolicyStatus::Warn => {
            interp.issue(
                MessageType::AuthorWarning,
                &format!("{message}{warning_suffix}\n{}", policy_warning("CMP0175")),
            );
            Ok(())
        }
        PolicyStatus::Old => Ok(()),
    }
}

fn validate_custom_keywords(
    interp: &mut Interp,
    seen: &BTreeSet<&str>,
    allowed: &[&str],
    context: &str,
) -> CmdResult {
    let unsupported: Vec<_> = seen
        .iter()
        .filter(|word| !allowed.contains(word))
        .copied()
        .collect();
    if !unsupported.is_empty() {
        cmp0175(
            interp,
            &format!(
                "The following keywords are not supported when using {context}: {}",
                unsupported.join(", ")
            ),
            ".",
        )?;
    }
    Ok(())
}

fn add_custom_command(interp: &mut Interp, args: &[String]) -> CmdResult {
    if args.len() < 4 {
        return err("called with wrong number of arguments.");
    }
    let mut command = CustomCommand {
        makefile: interp.mf_index(),
        depends_explicit_only: interp.is_on("CMAKE_ADD_CUSTOM_COMMAND_DEPENDS_EXPLICIT_ONLY"),
        backtrace: interp.backtrace.clone(),
        policies: interp.effective_policies(),
        ..Default::default()
    };
    let mut seen = BTreeSet::new();
    let (mut target, mut source, mut mode, mut language) = ("", "", "", "");
    let mut line = Vec::new();
    let mut expected = None;
    let mut stage = CustomCommandKind::PostBuild;
    let binary = interp.current_binary_dir();
    for arg in args.iter().map(String::as_str) {
        if CUSTOM_OUTPUT_KEYWORDS.contains(&arg)
            || CUSTOM_TARGET_KEYWORDS.contains(&arg)
            || matches!(arg, "SOURCE" | "OUTPUTS")
        {
            if let Some(keyword) = expected.take() {
                cmp0175(
                    interp,
                    &format!("Keyword {keyword} requires a value, but none was given."),
                    "",
                )?;
            }
            seen.insert(arg);
            match arg {
                "COMMAND" => {
                    if !line.is_empty() {
                        command.command_lines.push(std::mem::take(&mut line));
                    }
                    mode = arg;
                }
                "SOURCE" | "TARGET" | "OUTPUT" | "WORKING_DIRECTORY" | "MAIN_DEPENDENCY"
                | "COMMENT" | "DEPFILE" | "JOB_POOL" | "JOB_SERVER_AWARE" => {
                    if arg == "DEPFILE" {
                        let generator = interp.get_def("CMAKE_GENERATOR").unwrap_or("Ninja");
                        let supported = generator.starts_with("Ninja")
                            || generator == "Xcode"
                            || generator.contains("Makefiles")
                            || generator == "Watcom WMake"
                            || generator.starts_with("Visual Studio");
                        if !supported {
                            return err(format!("Option DEPFILE not supported by {generator}"));
                        }
                    }
                    mode = arg;
                    expected = Some(arg);
                }
                "DEPENDS" | "OUTPUTS" | "BYPRODUCTS" | "IMPLICIT_DEPENDS" => mode = arg,
                "PRE_BUILD" => stage = CustomCommandKind::PreBuild,
                "PRE_LINK" => stage = CustomCommandKind::PreLink,
                "POST_BUILD" => stage = CustomCommandKind::PostBuild,
                "VERBATIM" => command.verbatim = true,
                "USES_TERMINAL" => command.uses_terminal = true,
                "COMMAND_EXPAND_LISTS" => command.command_expand_lists = true,
                "DEPENDS_EXPLICIT_ONLY" => command.depends_explicit_only = true,
                "CODEGEN" => command.codegen = true,
                _ => {}
            }
            continue;
        }
        expected = None;
        match mode {
            "COMMAND" => line.push(arg.to_owned()),
            "SOURCE" => source = arg,
            "TARGET" => target = arg,
            "OUTPUT" => command.outputs.push(custom_output(arg, &binary)),
            "OUTPUTS" => {}
            "BYPRODUCTS" => command.byproducts.push(custom_output(arg, &binary)),
            "WORKING_DIRECTORY" => command.working_directory = arg.to_owned(),
            "MAIN_DEPENDENCY" => command.main_dependency = arg.to_owned(),
            "DEPFILE" => command.depfile = arg.to_owned(),
            "JOB_POOL" => command.job_pool = arg.to_owned(),
            "JOB_SERVER_AWARE" => command.job_server_aware = is_on(arg),
            "DEPENDS" => command.depends.push(arg.to_owned()),
            "IMPLICIT_DEPENDS" => {
                language = arg;
                mode = "implicit-file";
            }
            "implicit-file" => {
                let path = convert_to_unix_slashes(arg);
                command.depends.push(path.clone());
                command.implicit_depends.push((language.to_owned(), path));
                mode = "IMPLICIT_DEPENDS";
            }
            "COMMENT" => {
                if command
                    .comment
                    .as_ref()
                    .is_some_and(|comment| !comment.is_empty())
                {
                    cmp0175(
                        interp,
                        "COMMENT requires exactly one argument, but multiple values or COMMENT keywords have been given.",
                        "",
                    )?;
                }
                command.comment = Some(arg.to_owned());
            }
            _ => return err("Wrong syntax. Unknown type of argument."),
        }
    }
    if !line.is_empty() {
        command.command_lines.push(line);
    }
    if command.outputs.is_empty() && target.is_empty() {
        return err("Wrong syntax. A TARGET or OUTPUT must be specified.");
    }
    if source.is_empty() && !target.is_empty() && !command.outputs.is_empty() {
        return err("Wrong syntax. A TARGET and OUTPUT can not both be specified.");
    }
    let append = seen.contains("APPEND");
    if append && command.outputs.is_empty() {
        return err("given APPEND option with no OUTPUT.");
    }
    if !command.implicit_depends.is_empty()
        && !command.depfile.is_empty()
        && interp
            .get_def("CMAKE_GENERATOR")
            .is_some_and(|value| value != "Ninja")
    {
        return err("IMPLICIT_DEPENDS and DEPFILE can not both be specified.");
    }
    if command.codegen {
        if command.outputs.is_empty() {
            return err("CODEGEN requires at least 1 OUTPUT.");
        }
        if append {
            return err("CODEGEN may not be used with APPEND.");
        }
        if !command.implicit_depends.is_empty() {
            return err("CODEGEN is not compatible with IMPLICIT_DEPENDS.");
        }
        if interp.policy_status("CMP0171") != PolicyStatus::New {
            return err("CODEGEN option requires policy CMP0171 be set to NEW!");
        }
    }
    if append {
        validate_custom_keywords(
            interp,
            &seen,
            CUSTOM_APPEND_KEYWORDS,
            "APPEND with add_custom_command(OUTPUT)",
        )?;
        validate_command_lines(interp, &command)?;
        command.outputs.truncate(1);
        return append_custom_command(interp, command);
    }
    if command.uses_terminal && !command.job_pool.is_empty() {
        return err("JOB_POOL is shadowed by USES_TERMINAL.");
    }
    if source.is_empty() && command.outputs.is_empty() {
        if command.command_lines.is_empty() {
            cmp0175(interp, "At least one COMMAND must be given.", "")?;
        }
        validate_custom_keywords(
            interp,
            &seen,
            CUSTOM_TARGET_KEYWORDS,
            "add_custom_command(TARGET)",
        )?;
        if ["PRE_BUILD", "PRE_LINK", "POST_BUILD"]
            .iter()
            .filter(|option| seen.contains(**option))
            .count()
            != 1
        {
            let stage = match stage {
                CustomCommandKind::PreBuild => "PRE_BUILD",
                CustomCommandKind::PreLink => "PRE_LINK",
                _ => "POST_BUILD",
            };
            cmp0175(
                interp,
                "Exactly one of PRE_BUILD, PRE_LINK, or POST_BUILD must be given.",
                &format!(" Assuming {stage} to preserve backward compatibility."),
            )?;
        }
        let key = custom_command_target(interp, target)?;
        validate_command_lines(interp, &command)?;
        command.kind = stage;
        // These fields are used by the OUTPUT signature only.
        command.depends.clear();
        command.implicit_depends.clear();
        command.main_dependency.clear();
        for byproduct in &command.byproducts {
            mark_generated(interp, command.makefile, byproduct);
        }
        interp.ext.generator_actions.push(GeneratorAction::Target {
            target: key,
            command,
        });
    } else if target.is_empty() {
        validate_custom_keywords(
            interp,
            &seen,
            CUSTOM_OUTPUT_KEYWORDS,
            "add_custom_command(OUTPUT)",
        )?;
        validate_command_lines(interp, &command)?;
        if !command.main_dependency.is_empty() {
            command.depends.insert(0, command.main_dependency.clone());
        }
        for output in command.outputs.iter().chain(&command.byproducts) {
            mark_generated(interp, command.makefile, output);
        }
        interp.ext.generator_actions.push(GeneratorAction::Output {
            command,
            replace: false,
        });
    } else {
        return report(
            interp,
            "The SOURCE signatures of add_custom_command are no longer supported.",
        );
    }
    Ok(())
}

fn custom_command_target(interp: &mut Interp, name: &str) -> Result<String, crate::interp::CmdErr> {
    let key = find_target_key(interp, name);
    let target = key.as_ref().map(|key| &interp.ext.targets[key]);
    let message = match target {
        None => format!("No TARGET '{name}' has been created in this directory."),
        Some(target) if target.imported => {
            format!("TARGET '{name}' is IMPORTED and does not build here.")
        }
        Some(target) if target.makefile != interp.mf_index() => {
            format!("TARGET '{name}' was not created in this directory.")
        }
        Some(target)
            if matches!(
                target.kind,
                TargetKind::ObjectLibrary | TargetKind::InterfaceLibrary
            ) =>
        {
            let kind = if target.kind == TargetKind::ObjectLibrary {
                "an OBJECT"
            } else {
                "an INTERFACE"
            };
            format!(
                "Target \"{name}\" is {kind} library that may not have PRE_BUILD, PRE_LINK, or POST_BUILD commands."
            )
        }
        Some(_) => return Ok(key.unwrap()),
    };
    report(interp, &message)
}

/// `cmMakefile::AppendCustomCommandToOutput`: resolved at generate time.
fn append_custom_command(interp: &mut Interp, command: CustomCommand) -> CmdResult {
    interp
        .ext
        .generator_actions
        .push(GeneratorAction::Append(command));
    Ok(())
}

#[cfg(test)]
mod project_cases;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::target::{directory_entries, find_source, resolve_source};
    use crate::state::{find_target_key_at, has_test, read_target_property, target_property};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn interpreter() -> Interp {
        Interp::new(Mode::Project, "/src", "/build", "")
    }

    fn configure(interp: &mut Interp, code: &str) {
        let file = format!("{}/CMakeLists.txt", interp.current_source_dir());
        assert!(interp.run_string(code, &file));
        assert!(!interp.error);
    }

    struct Tree(PathBuf);

    impl Tree {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "cmake-rust-targets-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn write(&self, relative: &str, contents: &str) {
            let path = self.0.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, contents).unwrap();
        }

        fn interp(&self) -> Interp {
            let source = self.0.to_str().unwrap();
            Interp::new(Mode::Project, source, &format!("{source}/build"), "")
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn construction_snapshots_variables_and_preserves_directory_provenance() {
        let mut interp = interpreter();
        configure(
            &mut interp,
            r#"
cmake_minimum_required(VERSION 4.1)
set(CMAKE_BUILD_TYPE Debug)
set(CMAKE_POSITION_INDEPENDENT_CODE OFF)
set(CMAKE_CXX_STANDARD 23)
set(CMAKE_OSX_ARCHITECTURES arm64 x86_64)
set(CMAKE_MAP_IMPORTED_CONFIG_DEBUG Release)
set(CMAKE_ARCHIVE_OUTPUT_DIRECTORY_DEBUG archive-debug)
set(CMAKE_DEBUG_POSTFIX _dbg)
set(CMAKE_EXECUTABLE_ENABLE_EXPORTS ON)
include_directories(first)
add_compile_options(-snapshot)
add_compile_definitions(-DEARLY)
add_library(native STATIC file.cc)
add_library(shared SHARED file.cc)
add_library(imp SHARED IMPORTED GLOBAL)
add_executable(app WIN32 MACOSX_BUNDLE EXCLUDE_FROM_ALL file.cc)
add_compile_options(-later)
add_compile_definitions(-DLATE)
include_directories(BEFORE SYSTEM second third)
set(CMAKE_CXX_STANDARD 26)
"#,
        );
        let native = &interp.ext.targets["native"];
        let shared = &interp.ext.targets["shared"];
        let app = &interp.ext.targets["app"];
        let imported = &interp.ext.targets["imp"];
        assert_eq!(native.props.get("CXX_STANDARD"), Some("23"));
        assert_eq!(
            native.props.get("ARCHIVE_OUTPUT_DIRECTORY_DEBUG"),
            Some("archive-debug")
        );
        assert_eq!(native.props.get("POSITION_INDEPENDENT_CODE"), Some("OFF"));
        assert_eq!(shared.props.get("POSITION_INDEPENDENT_CODE"), Some("True"));
        assert_eq!(
            imported.props.get("POSITION_INDEPENDENT_CODE"),
            Some("True")
        );
        assert_eq!(imported.props.get("CXX_STANDARD"), None);
        assert_eq!(
            imported.props.get("MAP_IMPORTED_CONFIG_DEBUG"),
            Some("Release")
        );
        assert_eq!(imported.props.get("SYSTEM"), Some("ON"));
        assert_eq!(native.props.get("DEBUG_POSTFIX"), Some("_dbg"));
        assert_eq!(app.props.get("DEBUG_POSTFIX"), None);
        assert_eq!(app.props.get("WIN32_EXECUTABLE"), Some("ON"));
        assert_eq!(app.props.get("MACOSX_BUNDLE"), Some("ON"));
        assert_eq!(app.props.get("EXCLUDE_FROM_ALL"), Some("TRUE"));
        assert_eq!(app.props.get("ENABLE_EXPORTS"), Some("ON"));
        assert_eq!(native.props.get("COMPILE_OPTIONS"), Some("-snapshot"));
        assert_eq!(native.props.get("COMPILE_DEFINITIONS"), None);
        assert_eq!(
            native.props.get("INCLUDE_DIRECTORIES"),
            Some("/src/third;/src/second;/src/first")
        );
        let provenance = &native.provenance["INCLUDE_DIRECTORIES"];
        assert_eq!(
            provenance[0].1.last().unwrap().name.as_deref(),
            Some("include_directories")
        );
        assert!(provenance[0].1.last().unwrap().line > provenance[1].1.last().unwrap().line);
        assert_eq!(native.system_include_directories.len(), 2);
        let definitions = directory_entries(&interp, 0, "COMPILE_DEFINITIONS");
        assert_eq!(
            definitions
                .iter()
                .map(|entry| entry.0.as_str())
                .collect::<Vec<_>>(),
            ["-DEARLY", "-DLATE"]
        );
    }

    #[test]
    fn link_and_file_set_properties_keep_their_entry_order() {
        let mut interp = interpreter();
        configure(
            &mut interp,
            r#"
cmake_minimum_required(VERSION 4.1)
add_library(leaf INTERFACE)
add_library(lib STATIC source.c)
target_link_libraries(lib PRIVATE debug leaf PUBLIC general another INTERFACE thing)
target_include_directories(lib SYSTEM BEFORE PUBLIC component)
target_compile_definitions(lib PRIVATE -DA=1 PUBLIC B)
target_sources(lib PUBLIC FILE_SET HEADERS FILES header.h
                          FILE_SET extra TYPE HEADERS BASE_DIRS extra FILES extra/more.h)
add_dependencies(lib leaf zoo leaf alpha)
"#,
        );
        let target = &interp.ext.targets["lib"];
        assert_eq!(
            target.props.get("LINK_LIBRARIES"),
            Some("$<$<CONFIG:DEBUG>:$<TARGET_NAME:leaf>>;another")
        );
        assert_eq!(
            target.props.get("INTERFACE_LINK_LIBRARIES"),
            Some("$<LINK_ONLY:$<$<CONFIG:DEBUG>:leaf>>;another;thing")
        );
        assert_eq!(
            target.props.get("INCLUDE_DIRECTORIES"),
            Some("/src/component;$<BUILD_INTERFACE:/src>;$<BUILD_INTERFACE:/src/extra>")
        );
        assert_eq!(
            target.props.get("INTERFACE_SYSTEM_INCLUDE_DIRECTORIES"),
            Some("/src/component")
        );
        assert_eq!(target.props.get("COMPILE_DEFINITIONS"), Some("A=1;B"));
        assert_eq!(target.props.get("HEADER_SETS"), Some("HEADERS;extra"));
        assert_eq!(
            target.props.get("INTERFACE_HEADER_SETS"),
            Some("HEADERS;extra")
        );
        assert_eq!(target.file_sets["extra"].base_dirs[0].0, "/src/extra");
        assert_eq!(target.file_sets["extra"].files[0].0, "/src/extra/more.h");
        assert_eq!(
            target.props.get("MANUALLY_ADDED_DEPENDENCIES"),
            Some("alpha;leaf;zoo")
        );
        write_target_property(
            &mut interp,
            "lib",
            "HEADER_SET",
            Some("a.h;b.h"),
            PropertyWrite::Set,
        )
        .unwrap();
        assert_eq!(
            interp.ext.targets["lib"].file_sets["HEADERS"].files[0].0,
            "a.h;b.h"
        );
    }

    #[test]
    fn imported_targets_and_aliases_are_scoped_and_remote_links_encode_the_caller() {
        let tree = Tree::new();
        tree.write(
            "CMakeLists.txt",
            r#"
cmake_minimum_required(VERSION 4.1)
add_library(api INTERFACE)
add_library(root_import UNKNOWN IMPORTED)
add_subdirectory(one first SYSTEM EXCLUDE_FROM_ALL)
add_subdirectory(two)
subdirs(late)
set(late_input value)
add_library(root_late UNKNOWN IMPORTED)
"#,
        );
        tree.write(
            "one/CMakeLists.txt",
            r#"
add_library(local UNKNOWN IMPORTED)
add_library(alias ALIAS local)
add_library(visible ALIAS root_import)
target_link_libraries(api INTERFACE alias)
add_library(system_lib INTERFACE)
"#,
        );
        tree.write(
            "two/CMakeLists.txt",
            "add_library(local INTERFACE IMPORTED)\n",
        );
        tree.write(
            "late/CMakeLists.txt",
            "add_custom_target(late_target COMMAND echo ${late_input})\n",
        );
        let mut interp = tree.interp();
        let root = tree.0.join("CMakeLists.txt");
        assert!(interp.read_list_file_directory(root.to_str().unwrap()));
        assert!(!interp.error);
        assert!(find_target_to_use(&interp, "local").is_none());
        assert!(find_target_key_at(&interp, 1, "root_late").is_none());
        assert!(find_target_key_at(&interp, 3, "root_late").is_some());
        let link = interp.ext.targets["api"]
            .props
            .get("INTERFACE_LINK_LIBRARIES")
            .unwrap();
        let (marker, rest) = link.split_once(';').unwrap();
        let caller = link_lookup_directory(marker).unwrap();
        assert_eq!(rest, "alias;::@");
        assert_eq!(caller, 1);
        let key = find_target_key_at(&interp, caller, "alias").unwrap();
        assert_eq!(interp.ext.targets[&key].kind, TargetKind::UnknownLibrary);
        let sibling = find_target_key_at(&interp, 2, "local").unwrap();
        assert_ne!(key, sibling);
        assert_eq!(
            interp.ext.targets["system_lib"].props.get("SYSTEM"),
            Some("ON")
        );
        crate::commands::custom_actions::run_generator_actions(
            &mut interp,
            &mut crate::commands::custom_actions::unsupported_output_genex,
        );
        assert_eq!(
            crate::commands::custom_actions::utility_command(&interp, "late_target")
                .unwrap()
                .command_lines[0],
            ["echo", "value"]
        );
        interp.mf_stack.push(caller);
        assert_eq!(
            target_property(&interp, "alias", "ALIAS_GLOBAL").as_deref(),
            Some("FALSE")
        );
        write_target_property(
            &mut interp,
            "local",
            "IMPORTED_GLOBAL",
            Some("YES"),
            PropertyWrite::Set,
        )
        .unwrap();
        assert_eq!(
            target_property(&interp, "alias", "IMPORTED_GLOBAL").as_deref(),
            Some("TRUE")
        );
        assert_eq!(
            target_property(&interp, "alias", "ALIAS_GLOBAL").as_deref(),
            Some("FALSE")
        );
        interp.mf_stack.pop();
        assert!(find_target_to_use(&interp, "local").is_some());
        assert!(find_target_to_use(&interp, "alias").is_none());
    }

    #[test]
    fn imported_location_mapping_and_compiler_feature_validation() {
        let mut interp = interpreter();
        interp.langs.enabled.push("CXX".to_owned());
        configure(
            &mut interp,
            r#"
cmake_minimum_required(VERSION 4.1)
set(CMAKE_CXX_COMPILE_FEATURES cxx_std_17 cxx_std_20)
set(CMAKE_CXX20_COMPILE_FEATURES cxx_std_20)
set(CMAKE_CXX_STANDARD_DEFAULT 17)
set(CMAKE_CXX_COMPILER_ID GNU)
set(CMAKE_CXX_COMPILER_VERSION 12.1)
add_library(sdk UNKNOWN IMPORTED)
add_library(app OBJECT source.cxx)
target_compile_features(app PRIVATE cxx_std_20)
"#,
        );
        assert_eq!(
            interp.ext.targets["app"].props.get("CXX_STANDARD"),
            Some("20")
        );
        assert!(matches!(
            add_feature(&mut interp, "app", "cxx_std_26"),
            Err(crate::interp::CmdErr::Error(message)) if message.contains("not known to CXX compiler")
        ));
        assert!(matches!(
            add_feature(&mut interp, "app", "cxx_fake_feature"),
            Err(crate::interp::CmdErr::Error(message)) if message.starts_with("specified unknown feature")
        ));
        for (prop, value) in [
            ("IMPORTED_LOCATION", "/plain"),
            ("IMPORTED_LOCATION_RELEASE", "/release"),
            ("IMPORTED_LOCATION_DEBUG", "/debug"),
            ("MAP_IMPORTED_CONFIG_RELWITHDEBINFO", "Debug;Release"),
        ] {
            write_target_property(&mut interp, "sdk", prop, Some(value), PropertyWrite::Set)
                .unwrap();
        }
        assert_eq!(
            read_target_property(&mut interp, "sdk", "LOCATION").as_deref(),
            Some("/plain")
        );
        assert_eq!(
            read_target_property(&mut interp, "sdk", "LOCATION_RELEASE").as_deref(),
            Some("/release")
        );
        assert_eq!(
            read_target_property(&mut interp, "sdk", "RELWITHDEBINFO_LOCATION").as_deref(),
            Some("/debug")
        );
    }

    #[test]
    fn custom_rules_and_tests_retain_configure_time_metadata() {
        let mut interp = interpreter();
        configure(
            &mut interp,
            r#"
cmake_minimum_required(VERSION 4.1)
set(CMAKE_GENERATOR Ninja)
add_custom_command(OUTPUT main.out second.out COMMAND tool make
  MAIN_DEPENDENCY main.in DEPENDS dependency IMPLICIT_DEPENDS C more.c
  BYPRODUCTS side.out DEPFILE rule.d JOB_POOL jobs JOB_SERVER_AWARE ON
  WORKING_DIRECTORY where COMMENT "Make files"
  VERBATIM COMMAND_EXPAND_LISTS DEPENDS_EXPLICIT_ONLY)
add_custom_command(OUTPUT second.out APPEND COMMAND tool append DEPENDS last)
add_custom_target(work ALL DEPENDS main.out COMMAND tool run SOURCES info.txt)
add_custom_command(TARGET work POST_BUILD COMMAND tool cleanup BYPRODUCTS clean.out)
enable_testing()
add_test(old echo before)
add_test(old echo after)
add_test(NAME "new test" COMMAND echo "" CONFIGURATIONS Debug Release
  WORKING_DIRECTORY /somewhere COMMAND_EXPAND_LISTS)
"#,
        );
        // Nothing is attached until generation starts.
        assert!(find_source(&interp, 0, "main.in").is_none());
        assert!(interp.ext.targets["work"].custom_commands.is_empty());
        crate::commands::custom_actions::run_generator_actions(
            &mut interp,
            &mut crate::commands::custom_actions::unsupported_output_genex,
        );
        assert!(!interp.error);
        // MAIN_DEPENDENCY carries the command (detail::AddCustomCommand).
        let source = find_source(&interp, 0, "main.in").unwrap();
        let cc = source.custom_command.as_ref().unwrap();
        assert!(cc.escape_allow_make_vars);
        let post = &interp.ext.targets["work"].custom_commands;
        assert_eq!(post.len(), 1);
        assert_eq!(post[0].target.as_deref(), Some("work"));
        let utility = crate::commands::custom_actions::utility_command(&interp, "work").unwrap();
        assert_eq!(utility.command_lines, [["tool", "run"]]);
        assert_eq!(utility.outputs, ["/build/CMakeFiles/work"]);
        assert_eq!(
            interp.ext.targets["work"].props.get("SOURCES"),
            Some("info.txt;/build/CMakeFiles/work")
        );
        let entry = crate::commands::custom_actions::source_with_output(
            &interp,
            0,
            "/build/second.out",
            false,
        )
        .unwrap();
        assert_eq!(
            interp.ext.sources[&0][entry.source.unwrap()]
                .location
                .path(),
            "main.in"
        );
        assert_eq!(cc.command_lines[1], ["tool", "append"]);
        assert_eq!(cc.implicit_depends, [("C".to_owned(), "more.c".to_owned())]);
        assert_eq!(cc.depends, ["main.in", "dependency", "more.c", "last"]);
        assert_eq!(cc.working_directory, "where");
        assert_eq!(cc.comment.as_deref(), Some("Make files"));
        assert!(
            cc.verbatim
                && cc.job_server_aware
                && cc.depends_explicit_only
                && cc.command_expand_lists
        );
        assert!(interp.ext.generator_actions.is_empty());
        for generated in ["main.out", "second.out", "side.out", "clean.out"] {
            let file = find_source(&interp, 0, generated).unwrap();
            assert!(file.generated);
            assert_eq!(
                resolve_source(&interp, 0, generated).unwrap().path,
                format!("/build/{generated}")
            );
        }
        assert_eq!(utility.kind, CustomCommandKind::Utility);
        assert_eq!(post[0].kind, CustomCommandKind::PostBuild);
        assert!(interp.ext.testing_enabled.contains(&0));
        assert!(has_test(&interp, "new test"));
        assert_eq!(interp.ext.tests[0].command, ["echo", "after"]);
        assert_eq!(interp.ext.tests[1].command, ["echo", ""]);
        assert_eq!(interp.ext.tests[1].configurations, ["Debug", "Release"]);
        assert_eq!(
            interp.ext.tests[1].props.get("WORKING_DIRECTORY"),
            Some("/somewhere")
        );
        assert!(interp.ext.tests[1].command_expand_lists);
    }

    #[test]
    fn source_resolution_uses_0115_and_0187_without_probing_generated_files() {
        let tree = Tree::new();
        tree.write("unit.c", "");
        let mut interp = tree.interp();
        interp.langs.enabled.push("C".to_owned());
        interp.set_def("CMAKE_C_SOURCE_FILE_EXTENSIONS", "c");
        let resolved = resolve_source(&interp, 0, "unit").unwrap();
        assert_eq!(resolved.path, tree.0.join("unit.c").to_str().unwrap());
        assert_eq!(resolved.language.as_deref(), Some("C"));
        assert!(resolved.extension_warning.unwrap().contains("CMP0115"));
        configure(&mut interp, "cmake_minimum_required(VERSION 4.1)");
        assert!(resolve_source(&interp, 0, "unit").is_err());
        crate::commands::target::get_or_create_source(&mut interp, 0, "unit")
            .props
            .set("LANGUAGE", "CXX".to_owned());
        assert!(find_source(&interp, 0, "unit.c").is_none());
        let output = format!("{}/make.o", interp.current_binary_dir());
        mark_generated(&mut interp, 0, &output);
        let generated = resolve_source(&interp, 0, "make.o").unwrap();
        assert_eq!(generated.path, output);
        assert!(generated.generated && generated.external_object);
    }
}
