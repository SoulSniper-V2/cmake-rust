//! Ports of the pkg-config support machinery used by `cmake_pkg_config`:
//! `llpkgc` (the generated .pc state-machine parser), `cmPkgConfigParser`
//! and `cmPkgConfigResolver`.

pub mod parser;
pub mod resolver;
