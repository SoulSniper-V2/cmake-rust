//! Builtin command registry.

use crate::interp::Interp;

pub mod core;
pub mod find;
pub mod project;
pub mod string;
pub mod language;
pub mod list;
pub mod math;
pub mod message;
pub mod parse_arguments;
pub mod process;
pub mod property;
pub mod separate_arguments;
pub mod sysinfo;
pub mod try_compile;

pub fn register_all(interp: &mut Interp) {
    for name in [
        "if", "elseif", "else", "endif", "foreach", "endforeach", "while", "endwhile", "function",
        "endfunction", "macro", "endmacro", "block", "endblock", "break", "continue", "return",
    ] {
        interp.register_flow(name);
    }
    core::register(interp);
    find::register(interp);
    project::register(interp);
    string::register(interp);
    language::register(interp);
    list::register(interp);
    math::register(interp);
    message::register(interp);
    parse_arguments::register(interp);
    process::register(interp);
    property::register(interp);
    separate_arguments::register(interp);
    sysinfo::register(interp);
    try_compile::register(interp);
}
