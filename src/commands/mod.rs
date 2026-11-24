//! Builtin command registry.

use crate::interp::Interp;

pub mod configure_file;
pub mod core;
pub mod custom_actions;
pub mod directory;
pub mod export;
pub mod file;
pub mod find;
pub mod install;
pub mod language;
pub mod legacy_fs;
pub mod list;
pub mod math;
pub mod message;
pub mod parse_arguments;
pub mod path;
pub mod pkg_config;
pub mod process;
pub mod project;
pub mod property;
pub mod separate_arguments;
pub mod string;
pub mod sysinfo;
#[path = "../target.rs"]
pub mod target;
pub mod targets;
pub mod try_compile;
mod wrap;

pub fn register_all(interp: &mut Interp) {
    for name in [
        "if",
        "elseif",
        "else",
        "endif",
        "foreach",
        "endforeach",
        "while",
        "endwhile",
        "function",
        "endfunction",
        "macro",
        "endmacro",
        "block",
        "endblock",
        "break",
        "continue",
        "return",
    ] {
        interp.register_flow(name);
    }
    core::register(interp);
    export::register(interp);
    find::register(interp);
    directory::register(interp);
    targets::register(interp);
    configure_file::register(interp);
    file::register(interp);
    legacy_fs::register(interp);
    path::register(interp);
    install::register(interp);
    project::register(interp);
    string::register(interp);
    language::register(interp);
    list::register(interp);
    math::register(interp);
    message::register(interp);
    parse_arguments::register(interp);
    pkg_config::register(interp);
    process::register(interp);
    property::register(interp);
    separate_arguments::register(interp);
    sysinfo::register(interp);
    // cmake::RoleProject: project and --find-package modes.
    if matches!(
        interp.mode,
        crate::interp::Mode::Project | crate::interp::Mode::FindPackage
    ) {
        crate::instrumentation::register(interp);
    }
    try_compile::register(interp);
    wrap::register(interp);
    // `cmState::AddRemovedCommand`: commands removed by policy that report a
    // FATAL_ERROR but still count as successfully invoked.
    for name in [
        "build_name",
        "use_mangled_mesa",
        "export_library_dependencies",
        "load_command",
        "output_required_files",
        "subdir_depends",
        "utility_source",
        "variable_requires",
    ] {
        interp.register_raw(name, removed_command);
    }
    // Must stay last: overrides project commands in script mode.
    targets::register_script_mode_unexpected(interp);
}

/// `cmState::AddRemovedCommand` handler: issues the removal diagnostic and
/// returns success without expanding arguments.
fn removed_command(
    interp: &mut Interp,
    _file: &std::rc::Rc<crate::interp::ParsedFile>,
    command: &crate::lexer::Command,
    _subst: Option<&crate::interp::MacroSubst>,
) -> (bool, crate::interp::Ctl) {
    let message = match command.lname.as_str() {
        "build_name" => "The build_name command has been removed; see CMP0036.",
        "use_mangled_mesa" => "The use_mangled_mesa command has been removed; see CMP0030.",
        "export_library_dependencies" => {
            "The export_library_dependencies command has been removed; see CMP0033."
        }
        "load_command" => "The load_command command has been removed; see CMP0031.",
        "output_required_files" => {
            "The output_required_files command has been removed; see CMP0032."
        }
        "subdir_depends" => "The subdir_depends command has been removed; see CMP0029.",
        "utility_source" => "The utility_source command has been removed; see CMP0034.",
        "variable_requires" => "The variable_requires command has been removed; see CMP0035.",
        _ => unreachable!("removed command {}", command.lname),
    };
    interp.issue(crate::messages::MessageType::FatalError, message);
    (true, crate::interp::Ctl::Normal)
}
