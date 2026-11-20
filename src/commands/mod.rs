//! Builtin command registry.

use crate::interp::Interp;

pub mod core;
pub mod export;
pub mod find;
pub mod install;
pub mod project;
pub mod string;
pub mod list;
pub mod math;
pub mod parse_arguments;
pub mod separate_arguments;
pub mod try_compile;

pub fn register_all(interp: &mut Interp) {
    for name in [
        "if", "elseif", "else", "endif", "foreach", "endforeach", "while", "endwhile", "function",
        "endfunction", "macro", "endmacro", "block", "endblock", "break", "continue", "return",
    ] {
        interp.register_flow(name);
    }
    core::register(interp);
    export::register(interp);
    find::register(interp);
    install::register(interp);
    project::register(interp);
    string::register(interp);
    list::register(interp);
    math::register(interp);
    separate_arguments::register(interp);
    parse_arguments::register(interp);
    try_compile::register(interp);
}
