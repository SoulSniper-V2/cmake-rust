//! Builtin command registry.

use crate::interp::Interp;

pub mod core;
pub mod configure_file;
pub mod file;
pub mod legacy_fs;
pub mod path;

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
}
