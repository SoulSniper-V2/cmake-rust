//! cmake-rust: an independent implementation of CMake in Rust.

mod cache;
mod carchive;
mod cli;
mod cmcmd;
mod cmelf;
mod cmregex;
mod commands;
mod condition;
mod configure;
mod configure_log;
mod fileapi;
mod flow;
mod fsutil;
mod generator;
#[allow(dead_code)]
mod genex;
mod help;
mod install_model;
mod interp;
mod languages;
mod lexer;
mod messages;
mod outconv;
mod package_info;
mod pathresolver;
mod pkgconfig;
mod policy;
mod policy_table;
mod presets;
mod profiling;
pub mod properties;
mod props;
mod state;
mod util;

fn main() {
    let code = cli::main();
    // The cmake instance's destructor closes the profiling output.
    profiling::finish();
    std::process::exit(code);
}
