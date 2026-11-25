//! `CMakeCache.txt` model, reader and writer (port of `cmCacheManager`).

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CacheType {
    Bool,
    Path,
    Filepath,
    String,
    Internal,
    Static,
    Uninitialized,
}

impl CacheType {
    pub fn as_str(self) -> &'static str {
        match self {
            CacheType::Bool => "BOOL",
            CacheType::Path => "PATH",
            CacheType::Filepath => "FILEPATH",
            CacheType::String => "STRING",
            CacheType::Internal => "INTERNAL",
            CacheType::Static => "STATIC",
            CacheType::Uninitialized => "UNINITIALIZED",
        }
    }

    /// `cmState::StringToCacheEntryType` (unknown strings become STRING).
    pub fn parse(text: &str) -> CacheType {
        CacheType::parse_strict(text).unwrap_or(CacheType::String)
    }

    pub fn parse_strict(text: &str) -> Option<CacheType> {
        Some(match text {
            "BOOL" => CacheType::Bool,
            "PATH" => CacheType::Path,
            "FILEPATH" => CacheType::Filepath,
            "STRING" => CacheType::String,
            "INTERNAL" => CacheType::Internal,
            "STATIC" => CacheType::Static,
            "UNINITIALIZED" => CacheType::Uninitialized,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug)]
pub struct CacheEntry {
    pub value: String,
    pub kind: CacheType,
    pub initialized: bool,
    /// HELPSTRING, ADVANCED, STRINGS, MODIFIED, and others.
    pub properties: BTreeMap<String, String>,
}

impl CacheEntry {
    pub fn new(value: impl Into<String>, kind: CacheType, help: &str) -> Self {
        let mut properties = BTreeMap::new();
        properties.insert("HELPSTRING".to_owned(), help.to_owned());
        CacheEntry {
            value: value.into(),
            kind,
            initialized: true,
            properties,
        }
    }
}

const PERSISTENT: [&str; 3] = ["ADVANCED", "MODIFIED", "STRINGS"];

#[derive(Clone, Debug, Default)]
pub struct Cache {
    pub entries: BTreeMap<String, CacheEntry>,
    /// Keys added since the last `clear_touched` (each `cmake::AddCacheEntry`
    /// stops watching the key for unused-variable warnings).
    pub touched: std::collections::BTreeSet<String>,
    /// Parse errors of the last `load`, which `cmCacheManager::LoadCache`
    /// reports with `cmSystemTools::Error` while it keeps loading.
    pub load_errors: Vec<String>,
}

/// Parses one `KEY:TYPE=VALUE` line (or `KEY=VALUE`), as
/// `cmState::ParseCacheEntry`.
pub fn parse_entry(line: &str) -> Option<(String, String, CacheType)> {
    fn trim_value(value: &str) -> String {
        let trimmed = value.trim_end_matches(['\r', '\t', ' ']);
        let value = if trimmed.is_empty() { "" } else { trimmed };
        if value.len() >= 2 && value.starts_with('\'') && value.ends_with('\'') {
            value[1..value.len() - 1].to_owned()
        } else {
            value.to_owned()
        }
    }
    // "key":type=value
    if let Some(rest) = line.strip_prefix('"')
        && let Some(close) = rest.find('"')
    {
        let key = &rest[..close];
        let after = &rest[close + 1..];
        if let Some(after) = after.strip_prefix(':')
            && let Some(eq) = after.find('=')
        {
            return Some((
                key.to_owned(),
                trim_value(&after[eq + 1..]),
                CacheType::parse(&after[..eq]),
            ));
        }
    }
    // key:type=value
    if let Some(colon) = line.find([':', '='])
        && line.as_bytes()[colon] == b':'
    {
        let key = &line[..colon];
        let after = &line[colon + 1..];
        if let Some(eq) = after.find('=') {
            return Some((
                key.to_owned(),
                trim_value(&after[eq + 1..]),
                CacheType::parse(&after[..eq]),
            ));
        }
    }
    // "key"=value / key=value
    if let Some(rest) = line.strip_prefix('"')
        && let Some(close) = rest.find('"')
        && rest[close + 1..].starts_with('=')
    {
        return Some((
            rest[..close].to_owned(),
            trim_value(&rest[close + 2..]),
            CacheType::Uninitialized,
        ));
    }
    let eq = line.find('=')?;
    Some((
        line[..eq].to_owned(),
        trim_value(&line[eq + 1..]),
        CacheType::Uninitialized,
    ))
}

impl Cache {
    pub fn get(&self, key: &str) -> Option<&CacheEntry> {
        self.entries.get(key)
    }

    pub fn value(&self, key: &str) -> Option<&str> {
        self.entries
            .get(key)
            .filter(|entry| entry.initialized)
            .map(|entry| entry.value.as_str())
    }

    /// `cmCacheManager::AddCacheEntry`.
    pub fn add(&mut self, key: &str, value: Option<&str>, help: Option<&str>, kind: CacheType) {
        self.touched.insert(key.to_owned());
        let entry = self
            .entries
            .entry(key.to_owned())
            .or_insert_with(|| CacheEntry {
                value: String::new(),
                kind,
                initialized: false,
                properties: BTreeMap::new(),
            });
        entry.kind = kind;
        if let Some(value) = value {
            entry.value = value.to_owned();
            entry.initialized = true;
        }
        // cmState::AddCacheEntry takes the help as `std::string const&`, so
        // a null cmValue arrives as "" and cmCacheManager's "(This variable
        // does not exist and should not be used)" fallback is never reached.
        entry
            .properties
            .insert("HELPSTRING".into(), help.unwrap_or("").to_owned());
        // Normalize paths for PATH/FILEPATH entries (`cmSystemTools::ConvertToUnixSlashes`
        // of each list element).
        if matches!(kind, CacheType::Path | CacheType::Filepath) {
            if entry.value.contains(';') {
                let items: Vec<String> = crate::util::expand_list(&entry.value, false)
                    .into_iter()
                    .map(|item| convert_to_unix_slashes(&item))
                    .collect();
                entry.value = crate::util::join_list(&items);
            } else {
                entry.value = convert_to_unix_slashes(&entry.value);
            }
        }
    }

    pub fn remove(&mut self, key: &str) {
        self.entries.remove(key);
    }

    /// Loads `<dir>/CMakeCache.txt`. Returns Ok(false) when absent.
    pub fn load(&mut self, dir: &Path) -> Result<bool, String> {
        let path = dir.join("CMakeCache.txt");
        let Ok(bytes) = std::fs::read(&path) else {
            return Ok(false);
        };
        let text = crate::bytes::decode(&bytes);
        self.entries.clear();
        self.load_errors.clear();
        let lines: Vec<&str> = text.split('\n').collect();
        let mut index = 0usize;
        while index < lines.len() {
            let mut line = lines[index].trim_start_matches([' ', '\t', '\r', '\n']);
            index += 1;
            if line.starts_with('#') || line.is_empty() {
                continue;
            }
            let mut help = String::new();
            while let Some(rest) = line.strip_prefix("//") {
                let rest = rest.trim_end_matches('\r');
                if let Some(rest) = rest.strip_prefix("\\n") {
                    help.push('\n');
                    help.push_str(rest);
                } else {
                    help.push_str(rest);
                }
                if index >= lines.len() {
                    line = "";
                    break;
                }
                line = lines[index];
                index += 1;
            }
            if let Err(error) = self.load_entry(line, help, index, &path) {
                self.load_errors.push(error);
            }
        }
        Ok(true)
    }

    fn load_entry(
        &mut self,
        line: &str,
        help: String,
        lineno: usize,
        path: &Path,
    ) -> Result<(), String> {
        let line = line.trim_end_matches('\r');
        let Some((key, value, kind)) = parse_entry(line) else {
            return Err(format!(
                "Parse error in cache file {} on line {lineno}. Offending entry: {line}",
                path.display()
            ));
        };
        // Property entries: KEY-ADVANCED etc. (INTERNAL only).
        if kind == CacheType::Internal {
            for property in PERSISTENT {
                if let Some(base) = key
                    .strip_suffix(property)
                    .and_then(|rest| rest.strip_suffix('-'))
                    && !base.is_empty()
                {
                    let entry = self
                        .entries
                        .entry(base.to_owned())
                        .or_insert_with(|| CacheEntry {
                            value: String::new(),
                            kind: CacheType::Uninitialized,
                            initialized: false,
                            properties: BTreeMap::new(),
                        });
                    entry.properties.insert(property.to_owned(), value);
                    return Ok(());
                }
            }
        }
        let mut entry = CacheEntry::new(value, kind, &help);
        if let Some(existing) = self.entries.get(&key) {
            for (name, value) in &existing.properties {
                if name != "HELPSTRING" {
                    entry.properties.insert(name.clone(), value.clone());
                }
            }
        }
        self.entries.insert(key, entry);
        Ok(())
    }

    /// Writes `<dir>/CMakeCache.txt` (only when content changes) and
    /// `CMakeFiles/cmake.check_cache`.
    pub fn save(
        &mut self,
        dir: &Path,
        cmake_command: &str,
        version: (u32, u32, u32),
    ) -> std::io::Result<()> {
        let help_major = "Major version of cmake used to create the current loaded cache";
        let help_minor = "Minor version of cmake used to create the current loaded cache";
        let help_patch = "Patch version of cmake used to create the current loaded cache";
        self.add(
            "CMAKE_CACHE_MAJOR_VERSION",
            Some(&version.0.to_string()),
            Some(help_major),
            CacheType::Internal,
        );
        self.add(
            "CMAKE_CACHE_MINOR_VERSION",
            Some(&version.1.to_string()),
            Some(help_minor),
            CacheType::Internal,
        );
        self.add(
            "CMAKE_CACHE_PATCH_VERSION",
            Some(&version.2.to_string()),
            Some(help_patch),
            CacheType::Internal,
        );
        let cwd = convert_to_unix_slashes(&dir.to_string_lossy());
        self.add(
            "CMAKE_CACHEFILE_DIR",
            Some(&cwd),
            Some("This is the directory where this CMakeCache.txt was created"),
            CacheType::Internal,
        );
        let mut out = String::new();
        let _ = write!(
            out,
            "# This is the CMakeCache file.\n# For build in directory: {cwd}\n# It was generated by CMake: {cmake_command}\n# You can edit this file to change values found and used by cmake.\n# If you do not want to change any of the values, simply exit the editor.\n# If you do want to change a value, simply edit, save, and exit the editor.\n# The syntax for the file is as follows:\n# KEY:TYPE=VALUE\n# KEY is the name of a variable in the cache.\n# TYPE is a hint to GUIs for the type of VALUE, DO NOT EDIT TYPE!.\n# VALUE is the current value for the KEY.\n\n########################\n# EXTERNAL cache entries\n########################\n\n"
        );
        for (key, entry) in &self.entries {
            if !entry.initialized || entry.kind == CacheType::Internal {
                continue;
            }
            match entry.properties.get("HELPSTRING") {
                Some(help) => output_help(&mut out, help),
                None => output_help(&mut out, "Missing description"),
            }
            output_key(&mut out, key);
            let _ = write!(out, ":{}=", entry.kind.as_str());
            output_value(&mut out, &entry.value);
            out.push('\n');
            output_newline_warning(&mut out, key, &entry.value);
            out.push('\n');
        }
        out.push_str(
            "\n########################\n# INTERNAL cache entries\n########################\n\n",
        );
        for (key, entry) in &self.entries {
            if !entry.initialized {
                continue;
            }
            for property in PERSISTENT {
                if let Some(value) = entry.properties.get(property) {
                    output_help(
                        &mut out,
                        &format!("{property} property for variable: {key}"),
                    );
                    output_key(&mut out, &format!("{key}-{property}"));
                    out.push_str(":INTERNAL=");
                    output_value(&mut out, value);
                    out.push('\n');
                }
            }
            if entry.kind == CacheType::Internal {
                if let Some(help) = entry.properties.get("HELPSTRING") {
                    output_help(&mut out, help);
                }
                output_key(&mut out, key);
                out.push_str(":INTERNAL=");
                output_value(&mut out, &entry.value);
                out.push('\n');
            }
        }
        out.push('\n');
        crate::fsutil::write_if_different(&dir.join("CMakeCache.txt"), &out)?;
        std::fs::create_dir_all(dir.join("CMakeFiles"))?;
        std::fs::write(
            dir.join("CMakeFiles/cmake.check_cache"),
            "# This file is generated by cmake for dependency checking of the CMakeCache.txt file\n",
        )
    }
}

fn output_key(out: &mut String, key: &str) {
    if key.contains(':') || key.starts_with("//") {
        let _ = write!(out, "\"{key}\"");
    } else {
        out.push_str(key);
    }
}

fn output_value(out: &mut String, value: &str) {
    let value = value.split('\n').next().unwrap_or("");
    if value.ends_with(' ') || value.ends_with('\t') {
        let _ = write!(out, "'{value}'");
    } else {
        out.push_str(value);
    }
}

/// `cmCacheManager::OutputWarningComment`.
fn output_warning_comment(out: &mut String, message: &str, wrap_spaces: bool) {
    let bytes = message.as_bytes();
    let mut pos = 0usize;
    for i in 0..=bytes.len() {
        if (i == bytes.len())
            || (bytes[i] == b'\n')
            || (i - pos >= 60 && bytes[i] == b' ' && wrap_spaces)
        {
            out.push_str("# ");
            let mut start = pos;
            if bytes[start] == b'\n' {
                start += 1;
                out.push_str("\\n");
            }
            out.push_str(&message[start..i]);
            out.push('\n');
            pos = i;
        }
    }
}

/// `cmCacheManager::OutputNewlineTruncationWarning` (comment part).
fn output_newline_warning(out: &mut String, key: &str, value: &str) {
    if value.contains('\n') {
        let comment = format!(
            "WARNING: Value of {key} contained a newline and was truncated. Original value:"
        );
        output_warning_comment(out, &comment, true);
        output_warning_comment(out, value, false);
    }
}

fn output_help(out: &mut String, help: &str) {
    let bytes = help.as_bytes();
    let end = bytes.len();
    if end == 0 {
        return;
    }
    let mut pos = 0usize;
    for i in 1..=end {
        if i == end || bytes[i] == b'\n' || (i - pos >= 60 && bytes[i] == b' ') {
            out.push_str("//");
            if bytes[pos] == b'\n' {
                pos += 1;
                out.push_str("\\n");
            }
            out.push_str(&crate::bytes::decode(&bytes[pos..i]));
            out.push('\n');
            pos = i;
        }
    }
}

/// kwsys `SystemTools::ConvertToUnixSlashes`: backslashes to slashes,
/// repeated slashes collapsed (keeping exactly two leading ones), `~` and
/// `~user` expanded, and a trailing slash removed.
pub fn convert_to_unix_slashes(path: &str) -> String {
    if path.is_empty() {
        return String::new();
    }
    let replaced = path.replace('\\', "/");
    let bytes = replaced.as_bytes();
    let double =
        bytes.len() >= 2 && bytes[0] == b'/' && bytes[1] == b'/' && bytes.get(2) != Some(&b'/');
    let mut out = String::with_capacity(replaced.len());
    let mut previous_slash = false;
    for (index, ch) in replaced.char_indices() {
        if ch == '/' && previous_slash && !(double && index == 1) {
            continue;
        }
        previous_slash = ch == '/';
        out.push(ch);
    }
    if out.starts_with('~') {
        let rest = &out[1..];
        if rest.is_empty() || rest.starts_with('/') {
            if let Ok(home) = std::env::var("HOME") {
                out = format!("{home}{rest}");
            }
        } else {
            let idx = out.find('/').unwrap_or(out.len());
            if let Some(dir) = home_of_user(&out[1..idx]) {
                out = format!("{dir}{}", &out[idx..]);
            }
        }
    }
    // Keep the root slash and the slash after a drive letter ("c:/").
    let size = out.len();
    if size > 1
        && out.ends_with('/')
        && !(size == 3 && out.as_bytes()[1] == b':')
        && out.as_bytes()[size - 2] != b'/'
    {
        out.pop();
    }
    out
}

/// `getpwnam(user)->pw_dir`.
fn home_of_user(user: &str) -> Option<String> {
    let user = std::ffi::CString::new(user).ok()?;
    // SAFETY: getpwnam with a valid C string; fields read immediately.
    let pw = unsafe { libc::getpwnam(user.as_ptr()) };
    if pw.is_null() {
        return None;
    }
    // SAFETY: pw is non-null.
    let dir = unsafe { (*pw).pw_dir };
    if dir.is_null() {
        return None;
    }
    // SAFETY: non-null C string.
    Some(
        unsafe { std::ffi::CStr::from_ptr(dir) }
            .to_string_lossy()
            .into_owned(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries() {
        assert_eq!(
            parse_entry("A:STRING=x y  "),
            Some(("A".into(), "x y".into(), CacheType::String))
        );
        assert_eq!(
            parse_entry("\"a:b\":PATH='v '"),
            Some(("a:b".into(), "v ".into(), CacheType::Path))
        );
        assert_eq!(
            parse_entry("K=v"),
            Some(("K".into(), "v".into(), CacheType::Uninitialized))
        );
    }

    #[test]
    fn roundtrip() {
        let dir = std::env::temp_dir().join(format!("cmrs-cache-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut cache = Cache::default();
        cache.add(
            "FOO",
            Some("bar "),
            Some("A help string\nsecond line"),
            CacheType::String,
        );
        cache.add("INT", Some("1"), Some("internal"), CacheType::Internal);
        cache
            .entries
            .get_mut("FOO")
            .unwrap()
            .properties
            .insert("ADVANCED".into(), "1".into());
        cache.save(&dir, "/x/cmake", (4, 1, 0)).unwrap();
        let mut loaded = Cache::default();
        assert!(loaded.load(&dir).unwrap());
        let foo = loaded.get("FOO").unwrap();
        assert_eq!(foo.value, "bar ");
        assert_eq!(foo.kind, CacheType::String);
        assert_eq!(foo.properties["ADVANCED"], "1");
        assert_eq!(foo.properties["HELPSTRING"], "A help string\nsecond line");
        assert_eq!(loaded.value("INT"), Some("1"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
