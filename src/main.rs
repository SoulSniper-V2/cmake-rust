//! cmake-rust: an independent implementation of CMake in Rust.

fn main() {
    cmake_rust::stdio::init();
    let code = cmake_rust::cli::main();
    // The cmake instance's destructor closes the profiling output.
    cmake_rust::profiling::finish();
    std::process::exit(code);
}
