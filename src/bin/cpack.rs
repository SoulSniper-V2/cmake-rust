//! CPack driver binary.

fn main() {
    cmake_rust::stdio::init();
    std::process::exit(cmake_rust::cpack::main());
}
