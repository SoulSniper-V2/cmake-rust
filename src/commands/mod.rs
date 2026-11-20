//! Builtin command registry.

use crate::interp::Interp;

pub mod core;
pub mod export;
pub mod install;
pub mod project;

pub fn register_all(interp: &mut Interp) {
    for name in [
        "if", "elseif", "else", "endif", "foreach", "endforeach", "while", "endwhile", "function",
        "endfunction", "macro", "endmacro", "block", "endblock", "break", "continue", "return",
    ] {
        interp.register_flow(name);
    }
    core::register(interp);
    export::register(interp);
    install::register(interp);
    project::register(interp);
}
