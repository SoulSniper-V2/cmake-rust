//! cmake-rust: an independent implementation of CMake in Rust.

mod cache;
mod cli;
mod cmcmd;
mod cmregex;
mod commands;
mod condition;
mod configure;
mod flow;
mod fsutil;
mod interp;
mod lexer;
mod messages;
mod policy;
mod policy_table;
mod props;
mod state;
mod util;

fn main() {
    std::process::exit(cli::main());
}
