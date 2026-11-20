//! Builtin command registry.

use crate::interp::Interp;

pub mod core;
pub mod sysinfo;
pub mod property;
pub mod process;
pub mod message;
pub mod language;

pub fn register_all(interp: &mut Interp) {
    for name in [
        "if", "elseif", "else", "endif", "foreach", "endforeach", "while", "endwhile", "function",
        "endfunction", "macro", "endmacro", "block", "endblock", "break", "continue", "return",
    ] {
        interp.register_flow(name);
    }
    core::register(interp);
    sysinfo::register(interp);
    property::register(interp);
    process::register(interp);
    message::register(interp);
    language::register(interp);
}
