//! Project configuration pipeline (configure + generate). The CLI builds a
//! `ConfigureOptions` and calls `configure`.

use crate::cache::CacheType;

/// One `-D` cache definition from the command line.
#[derive(Clone, Debug)]
pub struct CacheDefinition {
    pub name: String,
    pub value: String,
    /// None when the type was not given (UNINITIALIZED).
    pub kind: Option<CacheType>,
}

#[derive(Clone, Debug, Default)]
pub struct ConfigureOptions {
    pub source_dir: String,
    pub binary_dir: String,
    pub generator: Option<String>,
    pub toolset: Option<String>,
    pub platform: Option<String>,
    pub toolchain_file: Option<String>,
    pub install_prefix: Option<String>,
    pub definitions: Vec<CacheDefinition>,
    /// `-U` glob patterns.
    pub undefine: Vec<String>,
    /// `-C <initial-cache>` scripts.
    pub preload_scripts: Vec<String>,
    pub fresh: bool,
    pub log_level: Option<String>,
    pub log_context: bool,
    pub warn_dev: Option<bool>,
    pub warn_dev_error: Option<bool>,
    pub warn_deprecated: Option<bool>,
    pub warn_deprecated_error: Option<bool>,
    pub warn_uninitialized: bool,
    pub debug_trycompile: bool,
    pub trace: bool,
    /// Original command-line arguments (for CMAKE_ARGV-like bookkeeping).
    pub argv: Vec<String>,
    /// `--explain <target> <flag>` request (cmake-rust extension).
    pub explain: Option<(String, String)>,
}

/// Runs configure and generate. Returns the process exit code.
pub fn configure(options: &ConfigureOptions) -> i32 {
    let _ = options;
    eprintln!("CMake Error: project configuration is not implemented yet in this build of cmake-rust");
    1
}
