//! Builtin command registry.

use crate::interp::Interp;

pub mod configure_file;
pub mod core;
pub mod file;
pub mod legacy_fs;
pub mod list;
pub mod math;
pub mod parse_arguments;
pub mod path;
pub mod project;
pub mod separate_arguments;
pub mod string;
pub mod try_compile;

pub fn register_all(interp: &mut Interp) {
    for name in [
        "if", "elseif", "else", "endif", "foreach", "endforeach", "while", "endwhile", "function",
        "endfunction", "macro", "endmacro", "block", "endblock", "break", "continue", "return",
    ] {
        interp.register_flow(name);
    }
    core::register(interp);
    configure_file::register(interp);
    file::register(interp);
    legacy_fs::register(interp);
    path::register(interp);
    project::register(interp);
    string::register(interp);
    list::register(interp);
    math::register(interp);
    separate_arguments::register(interp);
    parse_arguments::register(interp);
    try_compile::register(interp);
}
