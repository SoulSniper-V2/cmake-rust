//! Port of `Source/cmPkgConfigResolver.cxx` (strict/permissive/best-effort
//! .pc resolution, variable expansion, flag tokenizing/mangling and the
//! RPM-style version comparison).

use std::collections::HashMap;

use super::parser::Entry;

/// C `isspace` on bytes (locale "C": space, \t, \n, \v, \f, \r).
fn c_isspace(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// `cmPkgConfigCflagsResult`.
#[derive(Default)]
pub struct CflagsResult {
    pub flagline: Vec<u8>,
    pub includes: Vec<String>,
    pub compile_options: Vec<String>,
}

/// `cmPkgConfigLibsResult`.
#[derive(Default)]
pub struct LibsResult {
    pub flagline: Vec<u8>,
    pub lib_dirs: Vec<String>,
    pub lib_names: Vec<String>,
    pub link_options: Vec<String>,
}

/// `cmPkgConfigVersionReq`.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum VersionOp {
    #[default]
    Any,
    Lt,
    LtEq,
    Eq,
    Neq,
    GtEq,
    Gt,
}

#[derive(Clone, Default)]
pub struct VersionReq {
    pub operation: VersionOp,
    pub version: String,
}

impl VersionReq {
    /// `cmPkgConfigVersionReq::string()`.
    pub fn string(&self) -> String {
        match self.operation {
            VersionOp::Any => String::new(),
            VersionOp::Lt => format!("<{}", self.version),
            VersionOp::LtEq => format!("<={}", self.version),
            VersionOp::Eq => format!("={}", self.version),
            VersionOp::Neq => format!("!={}", self.version),
            VersionOp::GtEq => format!(">={}", self.version),
            VersionOp::Gt => format!(">{}", self.version),
        }
    }
}

/// `cmPkgConfigDependency`.
#[derive(Default)]
pub struct Dependency {
    pub name: String,
    pub ver_req: VersionReq,
}

/// `cmPkgConfigEnv`.
pub struct Env {
    pub path: Option<Vec<String>>,
    pub lib_dirs: Option<Vec<String>>,
    pub sys_cflags: Option<Vec<String>>,
    pub sys_libs: Option<Vec<String>>,
    pub sysroot_dir: Option<String>,
    pub top_build_dir: Option<String>,
    pub search: Vec<String>,
    pub disable_uninstalled: Option<bool>,
    pub allow_sys_cflags: bool,
    pub allow_sys_libs: bool,
}

impl Default for Env {
    fn default() -> Self {
        Env {
            path: None,
            lib_dirs: None,
            sys_cflags: None,
            sys_libs: None,
            sysroot_dir: None,
            top_build_dir: None,
            search: Vec::new(),
            disable_uninstalled: None,
            allow_sys_cflags: true,
            allow_sys_libs: true,
        }
    }
}

/// `cmPkgConfigResult`.  `env` in the C++ is only consulted by `Cflags`/`Libs`,
/// so those take it as an argument here.
#[derive(Default)]
pub struct PkgConfig {
    pub keywords: HashMap<String, String>,
    pub variables: HashMap<String, String>,
}

impl PkgConfig {
    fn str_or_default(&self, key: &str) -> String {
        self.keywords.get(key).cloned().unwrap_or_default()
    }

    pub fn name(&self) -> String {
        self.str_or_default("Name")
    }

    pub fn description(&self) -> String {
        self.str_or_default("Description")
    }

    pub fn version(&self) -> String {
        self.str_or_default("Version")
    }

    pub fn conflicts(&self) -> Vec<Dependency> {
        match self.keywords.get("Conflicts") {
            Some(deps) => parse_dependencies(deps),
            None => Vec::new(),
        }
    }

    pub fn provides(&self) -> Vec<Dependency> {
        match self.keywords.get("Provides") {
            Some(deps) => parse_dependencies(deps),
            None => Vec::new(),
        }
    }

    pub fn requires(&self, priv_: bool) -> Vec<Dependency> {
        let key = if priv_ {
            "Requires.private"
        } else {
            "Requires"
        };
        match self.keywords.get(key) {
            Some(deps) => parse_dependencies(deps),
            None => Vec::new(),
        }
    }

    /// `cmPkgConfigResult::Cflags`.
    pub fn cflags(&self, env: &Env, priv_: bool) -> CflagsResult {
        let mut cflags = String::new();
        let key = if priv_ { "Cflags.private" } else { "Cflags" };
        if let Some(value) = self.keywords.get(key) {
            cflags += value;
        }
        let key = if priv_ { "CFlags.private" } else { "CFlags" };
        if let Some(value) = self.keywords.get(key) {
            if !cflags.is_empty() {
                cflags += " ";
            }
            cflags += value;
        }

        let tokens = tokenize_flags(cflags.as_bytes());

        if env.allow_sys_cflags {
            if let Some(sysroot) = &env.sysroot_dir {
                return mangle_cflags_sysroot(&tokens, sysroot);
            }
            return mangle_cflags(&tokens);
        }

        if let Some(syspaths) = &env.sys_cflags {
            if let Some(sysroot) = &env.sysroot_dir {
                return mangle_cflags_sysroot_syspaths(&tokens, sysroot, syspaths);
            }
            return mangle_cflags_syspaths(&tokens, syspaths);
        }

        if let Some(sysroot) = &env.sysroot_dir {
            return mangle_cflags_sysroot_syspaths(&tokens, sysroot, &["/usr/include".to_owned()]);
        }

        mangle_cflags_syspaths(&tokens, &["/usr/include".to_owned()])
    }

    /// `cmPkgConfigResult::Libs`.
    pub fn libs(&self, env: &Env, priv_: bool) -> LibsResult {
        let key = if priv_ { "Libs.private" } else { "Libs" };
        let Some(libs) = self.keywords.get(key) else {
            return LibsResult::default();
        };

        let tokens = tokenize_flags(libs.as_bytes());

        if env.allow_sys_libs {
            if let Some(sysroot) = &env.sysroot_dir {
                return mangle_libs_sysroot(&tokens, sysroot);
            }
            return mangle_libs(&tokens);
        }

        if let Some(syspaths) = &env.sys_libs {
            if let Some(sysroot) = &env.sysroot_dir {
                return mangle_libs_sysroot_syspaths(&tokens, sysroot, syspaths);
            }
            return mangle_libs_syspaths(&tokens, syspaths);
        }

        if let Some(sysroot) = &env.sysroot_dir {
            return mangle_libs_sysroot_syspaths(&tokens, sysroot, &["/usr/lib".to_owned()]);
        }

        mangle_libs_syspaths(&tokens, &["/usr/lib".to_owned()])
    }
}

// ---------------------------------------------------------------------------
// anonymous-namespace helpers from cmPkgConfigResolver.cxx

/// `TrimBack`: drop trailing C-locale whitespace.
fn trim_back(str_: &mut Vec<u8>) {
    while str_.last().is_some_and(|b| c_isspace(*b)) {
        str_.pop();
    }
}

/// `AppendAndTrim`: append `sv` to `str_` and return the newly appended token
/// with trailing whitespace removed.
fn append_and_trim(str_: &mut Vec<u8>, sv: &[u8]) -> Vec<u8> {
    let size = str_.len();
    str_.extend_from_slice(sv);
    if str_.is_empty() {
        return Vec::new();
    }
    let begin = size;
    let mut cur = str_.len() - 1;
    while cur != begin && c_isspace(str_[cur]) {
        cur -= 1;
    }
    if c_isspace(str_[cur]) {
        return Vec::new();
    }
    str_[begin..=cur].to_vec()
}

// ---------------------------------------------------------------------------
// cmPkgConfigResolver

/// `cmPkgConfigResolver::ReplaceSep` (POSIX: ':' -> ';').
pub fn replace_sep(list: &mut String) {
    if cfg!(windows) {
        return;
    }
    *list = list.replace(':', ";");
}

/// `cmPkgConfigResolver::ResolveStrict`.
pub fn resolve_strict(entries: &[Entry], env: &Env) -> Option<PkgConfig> {
    let mut config = PkgConfig::default();

    if let Some(sysroot) = &env.sysroot_dir {
        config
            .variables
            .insert("pc_sysrootdir".to_owned(), sysroot.clone());
    } else {
        config
            .variables
            .insert("pc_sysrootdir".to_owned(), "/".to_owned());
    }

    if let Some(top) = &env.top_build_dir {
        config
            .variables
            .insert("pc_top_builddir".to_owned(), top.clone());
    }

    for entry in entries {
        let key = &entry.key;
        if entry.is_variable {
            if config.variables.contains_key(key) {
                return None;
            }
            let var = handle_variable_strict(entry, &config.variables)?;
            config.variables.insert(key.clone(), var);
        } else {
            let keys = &mut config.keywords;
            if key == "Cflags" && keys.contains_key("CFlags") {
                return None;
            }
            if key == "CFlags" && keys.contains_key("Cflags") {
                return None;
            }
            if key == "Cflags.private" && keys.contains_key("CFlags.private") {
                return None;
            }
            if key == "CFlags.private" && keys.contains_key("Cflags.private") {
                return None;
            }
            if keys.contains_key(key) {
                return None;
            }
            keys.insert(key.clone(), handle_keyword(entry, &config.variables));
        }
    }

    if !config.keywords.contains_key("Name")
        || !config.keywords.contains_key("Description")
        || !config.keywords.contains_key("Version")
    {
        return None;
    }

    Some(config)
}

/// `cmPkgConfigResolver::ResolvePermissive`.
pub fn resolve_permissive(entries: &[Entry], env: &Env) -> Option<PkgConfig> {
    let config = resolve_best_effort(entries, env);

    if !config.keywords.contains_key("Name")
        || !config.keywords.contains_key("Description")
        || !config.keywords.contains_key("Version")
    {
        return None;
    }

    Some(config)
}

/// `cmPkgConfigResolver::ResolveBestEffort`.
pub fn resolve_best_effort(entries: &[Entry], env: &Env) -> PkgConfig {
    let mut result = PkgConfig::default();

    if let Some(sysroot) = &env.sysroot_dir {
        result
            .variables
            .insert("pc_sysrootdir".to_owned(), sysroot.clone());
    } else {
        result
            .variables
            .insert("pc_sysrootdir".to_owned(), "/".to_owned());
    }

    if let Some(top) = &env.top_build_dir {
        result
            .variables
            .insert("pc_top_builddir".to_owned(), top.clone());
    }

    for entry in entries {
        let key = &entry.key;
        if entry.is_variable {
            let value = handle_variable_permissive(entry, &result.variables);
            result.variables.insert(key.clone(), value);
        } else {
            let value = handle_keyword(entry, &result.variables);
            *result.keywords.entry(key.clone()).or_default() += &value;
        }
    }
    result
}

/// `cmPkgConfigResolver::HandleVariablePermissive`.
fn handle_variable_permissive(entry: &Entry, variables: &HashMap<String, String>) -> String {
    let mut result = Vec::new();
    for segment in &entry.val {
        if !segment.is_variable {
            result.extend_from_slice(segment.data.as_bytes());
        } else if entry.key != segment.data {
            if let Some(value) = variables.get(&segment.data) {
                result.extend_from_slice(value.as_bytes());
            }
        }
    }

    trim_back(&mut result);
    String::from_utf8_lossy(&result).into_owned()
}

/// `cmPkgConfigResolver::HandleVariableStrict`.
fn handle_variable_strict(entry: &Entry, variables: &HashMap<String, String>) -> Option<String> {
    let mut value = Vec::new();
    for segment in &entry.val {
        if !segment.is_variable {
            value.extend_from_slice(segment.data.as_bytes());
        } else if entry.key == segment.data {
            return None;
        } else {
            match variables.get(&segment.data) {
                Some(v) => value.extend_from_slice(v.as_bytes()),
                None => return None,
            }
        }
    }

    trim_back(&mut value);
    Some(String::from_utf8_lossy(&value).into_owned())
}

/// `cmPkgConfigResolver::HandleKeyword`.
fn handle_keyword(entry: &Entry, variables: &HashMap<String, String>) -> String {
    let mut result = Vec::new();
    for segment in &entry.val {
        if !segment.is_variable {
            result.extend_from_slice(segment.data.as_bytes());
        } else {
            if let Some(value) = variables.get(&segment.data) {
                result.extend_from_slice(value.as_bytes());
            }
        }
    }

    trim_back(&mut result);
    String::from_utf8_lossy(&result).into_owned()
}

/// `cmPkgConfigResolver::TokenizeFlags`: tokens keep trailing whitespace.
fn tokenize_flags(flagline: &[u8]) -> Vec<&[u8]> {
    let mut result = Vec::new();

    let mut it = 0usize;
    while it < flagline.len() && c_isspace(flagline[it]) {
        it += 1;
    }

    while it < flagline.len() {
        let start = it;
        while it < flagline.len() && !c_isspace(flagline[it]) {
            it += 1;
        }
        while it < flagline.len() && c_isspace(flagline[it]) {
            it += 1;
        }
        result.push(&flagline[start..it]);
    }

    result
}

// ---------------------------------------------------------------------------
// MangleCflags / MangleLibs

fn mangle_cflags(flags: &[&[u8]]) -> CflagsResult {
    let mut result = CflagsResult::default();
    for flag in flags {
        if flag.starts_with(b"-I") {
            let token = append_and_trim(&mut result.flagline, flag);
            result
                .includes
                .push(String::from_utf8_lossy(&token).into_owned());
        } else {
            let token = append_and_trim(&mut result.flagline, flag);
            result
                .compile_options
                .push(String::from_utf8_lossy(&token).into_owned());
        }
    }
    result
}

fn mangle_cflags_sysroot(flags: &[&[u8]], sysroot: &str) -> CflagsResult {
    let mut result = CflagsResult::default();
    for flag in flags {
        if flag.starts_with(b"-I") {
            let reroot = reroot(flag, b"-I", sysroot);
            let token = append_and_trim(&mut result.flagline, &reroot);
            result
                .includes
                .push(String::from_utf8_lossy(&token).into_owned());
        } else {
            let token = append_and_trim(&mut result.flagline, flag);
            result
                .compile_options
                .push(String::from_utf8_lossy(&token).into_owned());
        }
    }
    result
}

fn mangle_cflags_syspaths(flags: &[&[u8]], syspaths: &[String]) -> CflagsResult {
    let mut result = CflagsResult::default();
    for flag in flags {
        if flag.starts_with(b"-I") {
            let noprefix = &flag[2..];
            if syspaths
                .iter()
                .all(|path| !noprefix.starts_with(path.as_bytes()))
            {
                let token = append_and_trim(&mut result.flagline, flag);
                result
                    .includes
                    .push(String::from_utf8_lossy(&token).into_owned());
            }
        } else {
            let token = append_and_trim(&mut result.flagline, flag);
            result
                .compile_options
                .push(String::from_utf8_lossy(&token).into_owned());
        }
    }
    result
}

fn mangle_cflags_sysroot_syspaths(
    flags: &[&[u8]],
    sysroot: &str,
    syspaths: &[String],
) -> CflagsResult {
    let mut result = CflagsResult::default();
    for flag in flags {
        if flag.starts_with(b"-I") {
            let reroot = reroot(flag, b"-I", sysroot);
            let noprefix = &reroot[2..];
            if syspaths
                .iter()
                .all(|path| !noprefix.starts_with(path.as_bytes()))
            {
                let token = append_and_trim(&mut result.flagline, &reroot);
                result
                    .includes
                    .push(String::from_utf8_lossy(&token).into_owned());
            }
        } else {
            let token = append_and_trim(&mut result.flagline, flag);
            result
                .compile_options
                .push(String::from_utf8_lossy(&token).into_owned());
        }
    }
    result
}

fn mangle_libs(flags: &[&[u8]]) -> LibsResult {
    let mut result = LibsResult::default();
    for flag in flags {
        if flag.starts_with(b"-L") {
            let token = append_and_trim(&mut result.flagline, flag);
            result
                .lib_dirs
                .push(String::from_utf8_lossy(&token).into_owned());
        } else if flag.starts_with(b"-l") {
            let token = append_and_trim(&mut result.flagline, flag);
            result
                .lib_names
                .push(String::from_utf8_lossy(&token).into_owned());
        } else {
            let token = append_and_trim(&mut result.flagline, flag);
            result
                .link_options
                .push(String::from_utf8_lossy(&token).into_owned());
        }
    }
    result
}

fn mangle_libs_sysroot(flags: &[&[u8]], sysroot: &str) -> LibsResult {
    let mut result = LibsResult::default();
    for flag in flags {
        if flag.starts_with(b"-L") {
            let reroot = reroot(flag, b"-L", sysroot);
            let token = append_and_trim(&mut result.flagline, &reroot);
            result
                .lib_dirs
                .push(String::from_utf8_lossy(&token).into_owned());
        } else if flag.starts_with(b"-l") {
            let token = append_and_trim(&mut result.flagline, flag);
            result
                .lib_names
                .push(String::from_utf8_lossy(&token).into_owned());
        } else {
            let token = append_and_trim(&mut result.flagline, flag);
            result
                .link_options
                .push(String::from_utf8_lossy(&token).into_owned());
        }
    }
    result
}

fn mangle_libs_syspaths(flags: &[&[u8]], syspaths: &[String]) -> LibsResult {
    let mut result = LibsResult::default();
    for flag in flags {
        if flag.starts_with(b"-L") {
            let noprefix = &flag[2..];
            if syspaths
                .iter()
                .all(|path| !noprefix.starts_with(path.as_bytes()))
            {
                let token = append_and_trim(&mut result.flagline, flag);
                result
                    .lib_dirs
                    .push(String::from_utf8_lossy(&token).into_owned());
            }
        } else if flag.starts_with(b"-l") {
            let token = append_and_trim(&mut result.flagline, flag);
            result
                .lib_names
                .push(String::from_utf8_lossy(&token).into_owned());
        } else {
            let token = append_and_trim(&mut result.flagline, flag);
            result
                .link_options
                .push(String::from_utf8_lossy(&token).into_owned());
        }
    }
    result
}

fn mangle_libs_sysroot_syspaths(flags: &[&[u8]], sysroot: &str, syspaths: &[String]) -> LibsResult {
    let mut result = LibsResult::default();
    for flag in flags {
        if flag.starts_with(b"-L") {
            let reroot = reroot(flag, b"-L", sysroot);
            let noprefix = &reroot[2..];
            if syspaths
                .iter()
                .all(|path| !noprefix.starts_with(path.as_bytes()))
            {
                let token = append_and_trim(&mut result.flagline, &reroot);
                result
                    .lib_dirs
                    .push(String::from_utf8_lossy(&token).into_owned());
            }
        } else if flag.starts_with(b"-l") {
            let token = append_and_trim(&mut result.flagline, flag);
            result
                .lib_names
                .push(String::from_utf8_lossy(&token).into_owned());
        } else {
            let token = append_and_trim(&mut result.flagline, flag);
            result
                .link_options
                .push(String::from_utf8_lossy(&token).into_owned());
        }
    }
    result
}

/// `cmPkgConfigResolver::Reroot`.
fn reroot(flag: &[u8], prefix: &[u8], sysroot: &str) -> Vec<u8> {
    let mut result = prefix.to_vec();
    result.extend_from_slice(sysroot.as_bytes());
    result.extend_from_slice(&flag[prefix.len()..]);
    result
}

// ---------------------------------------------------------------------------
// Version parsing and comparison

/// `cmPkgConfigResolver::ParseVersion(cur, end)`.
fn parse_version_it(bytes: &[u8], cur: &mut usize) -> VersionReq {
    let end = bytes.len();
    let mut result = VersionReq::default();

    if bytes[*cur] == b'=' {
        result.operation = VersionOp::Eq;
        *cur += 1;
    } else if bytes[*cur] == b'>' {
        *cur += 1;
        if *cur == end {
            result.operation = VersionOp::Gt;
            return result;
        }
        if bytes[*cur] == b'=' {
            result.operation = VersionOp::GtEq;
            *cur += 1;
        } else {
            result.operation = VersionOp::Gt;
        }
    } else if bytes[*cur] == b'<' {
        *cur += 1;
        if *cur == end {
            result.operation = VersionOp::Lt;
            return result;
        }
        if bytes[*cur] == b'=' {
            result.operation = VersionOp::LtEq;
            *cur += 1;
        } else {
            result.operation = VersionOp::Lt;
        }
    } else if bytes[*cur] == b'!' {
        *cur += 1;
        if *cur == end {
            result.operation = VersionOp::Any;
            return result;
        }
        if bytes[*cur] == b'=' {
            result.operation = VersionOp::Neq;
            *cur += 1;
        } else {
            result.operation = VersionOp::Any;
        }
    }

    loop {
        if *cur == end {
            return result;
        }
        if !c_isspace(bytes[*cur]) {
            break;
        }
        *cur += 1;
    }

    while *cur != end && !c_isspace(bytes[*cur]) && bytes[*cur] != b',' {
        result
            .version
            .push_str(&String::from_utf8_lossy(&bytes[*cur..*cur + 1]));
        *cur += 1;
    }

    result
}

/// `cmPkgConfigResolver::ParseVersion(string)`.
pub fn parse_version(version: &str) -> VersionReq {
    let bytes = version.as_bytes();
    if bytes.is_empty() {
        return VersionReq {
            operation: VersionOp::Eq,
            version: String::new(),
        };
    }

    let mut cur = 0usize;
    let mut result = parse_version_it(bytes, &mut cur);

    if !matches!(bytes[0], b'=' | b'!' | b'<' | b'>') {
        result.operation = VersionOp::Eq;
    }

    result
}

/// `cmPkgConfigResolver::ParseDependencies`.
fn parse_dependencies(deps: &str) -> Vec<Dependency> {
    let bytes = deps.as_bytes();
    let end = bytes.len();
    let mut cur = 0usize;
    let mut result: Vec<Dependency> = Vec::new();

    while cur != end {
        loop {
            if !c_isspace(bytes[cur]) && bytes[cur] != b',' {
                break;
            }
            cur += 1;
            if cur == end {
                return result;
            }
        }

        result.push(Dependency::default());
        let dep_index = result.len() - 1;

        loop {
            if c_isspace(bytes[cur]) || bytes[cur] == b',' {
                break;
            }
            result[dep_index]
                .name
                .push_str(&String::from_utf8_lossy(&bytes[cur..cur + 1]));
            cur += 1;
            if cur == end {
                return result;
            }
        }

        // in_operator
        let mut in_operator = false;
        loop {
            if cur == end {
                break;
            }
            if matches!(bytes[cur], b'>' | b'=' | b'<' | b'!') {
                in_operator = true;
                break;
            }
            if !c_isspace(bytes[cur]) {
                break;
            }
            cur += 1;
        }

        if !in_operator {
            continue;
        }

        result[dep_index].ver_req = parse_version_it(bytes, &mut cur);
    }

    result
}

/// `cmPkgConfigResolver::CheckVersion` (RPM-style compare).
pub fn check_version(desired: &VersionReq, provided: &str) -> bool {
    if desired.operation == VersionOp::Any {
        return true;
    }

    // check_with_op: `comp` is sign(desired - provided).
    let check_with_op = |comp: i32| -> bool {
        match desired.operation {
            VersionOp::Eq => comp == 0,
            VersionOp::Neq => comp != 0,
            VersionOp::Gt => comp < 0,
            VersionOp::GtEq => comp <= 0,
            VersionOp::Lt => comp > 0,
            VersionOp::LtEq => comp >= 0,
            _ => true,
        }
    };

    let a = desired.version.as_bytes();
    let b = provided.as_bytes();

    if desired.version == provided {
        return check_with_op(0);
    }

    let mut a_cur = 0usize;
    let a_end = a.len();
    let mut b_cur = 0usize;
    let b_end = b.len();

    while a_cur != a_end && b_cur != b_end {
        while a_cur != a_end && !a[a_cur].is_ascii_alphanumeric() && a[a_cur] != b'~' {
            a_cur += 1;
        }
        while b_cur != b_end && !b[b_cur].is_ascii_alphanumeric() && b[b_cur] != b'~' {
            b_cur += 1;
        }

        if a_cur == a_end || b_cur == b_end {
            break;
        }

        if a[a_cur] == b'~' || b[b_cur] == b'~' {
            if a[a_cur] != b'~' {
                return check_with_op(1);
            }
            if b[b_cur] != b'~' {
                return check_with_op(-1);
            }
            a_cur += 1;
            b_cur += 1;
            continue;
        }

        let mut a_seg = a_cur;
        let mut b_seg = b_cur;
        let is_num;

        if a[a_cur].is_ascii_digit() {
            is_num = true;
            while a_cur != a_end && a[a_cur].is_ascii_digit() {
                a_cur += 1;
            }
            while b_cur != b_end && b[b_cur].is_ascii_digit() {
                b_cur += 1;
            }
        } else {
            is_num = false;
            while a_cur != a_end && a[a_cur].is_ascii_alphabetic() {
                a_cur += 1;
            }
            while b_cur != b_end && b[b_cur].is_ascii_alphabetic() {
                b_cur += 1;
            }
        }

        let mut a_len = a_cur - a_seg;
        let mut b_len = b_cur - b_seg;

        if b_len == 0 {
            return check_with_op(if is_num { 1 } else { -1 });
        }

        if is_num {
            while a_seg != a_cur && a[a_seg] == b'0' {
                a_seg += 1;
            }
            while b_seg != b_cur && b[b_seg] == b'0' {
                b_seg += 1;
            }

            a_len = a_cur - a_seg;
            b_len = b_cur - b_seg;

            if a_len != b_len {
                return check_with_op(if a_len > b_len { 1 } else { -1 });
            }

            let cmp = a[a_seg..a_seg + a_len].cmp(&b[b_seg..b_seg + a_len]);
            if cmp != std::cmp::Ordering::Equal {
                return check_with_op(match cmp {
                    std::cmp::Ordering::Less => -1,
                    std::cmp::Ordering::Greater => 1,
                    std::cmp::Ordering::Equal => 0,
                });
            }
        } else {
            let min = a_len.min(b_len);
            let cmp = a[a_seg..a_seg + min].cmp(&b[b_seg..b_seg + min]);
            if cmp != std::cmp::Ordering::Equal {
                return check_with_op(match cmp {
                    std::cmp::Ordering::Less => -1,
                    std::cmp::Ordering::Greater => 1,
                    std::cmp::Ordering::Equal => 0,
                });
            }

            if a_len != b_len {
                return check_with_op(if a_len > b_len { 1 } else { -1 });
            }
        }
    }

    if a_cur == a_end {
        if b_cur == b_end {
            return check_with_op(0);
        }
        return check_with_op(-1);
    }

    check_with_op(1)
}
