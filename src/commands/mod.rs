//! Builtin command registry.

use crate::interp::Interp;

pub mod core;
pub mod directory;
pub mod targets;
#[path = "../target.rs"]
pub mod target;

pub fn register_all(interp: &mut Interp) {
    for name in [
        "if", "elseif", "else", "endif", "foreach", "endforeach", "while", "endwhile", "function",
        "endfunction", "macro", "endmacro", "block", "endblock", "break", "continue", "return",
    ] {
        interp.register_flow(name);
    }
    core::register(interp);
    directory::register(interp);
    targets::register(interp);
}
