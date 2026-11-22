//! cmake-rust: an independent implementation of CMake in Rust.

mod cache;
mod cli;
mod cmcmd;
mod cmelf;
mod cmregex;
mod commands;
mod condition;
mod configure;
mod configure_log;
mod flow;
mod fileapi;
mod fsutil;
mod generator;
mod install_model;
#[allow(dead_code)] mod genex;
mod interp;
mod languages;
mod lexer;
mod messages;
mod outconv;
mod policy;
mod policy_table;
pub mod properties;
mod presets;
mod props;
mod state;
mod util;

fn main() {
    std::process::exit(cli::main());
}
